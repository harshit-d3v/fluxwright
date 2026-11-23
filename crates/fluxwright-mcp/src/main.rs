//! Fluxwright MCP server (stdio by default, or Streamable HTTP via `--http <ADDR>`).
//! Claude Desktop, Codex, Cursor, and remote MCP clients can call tools to drive a Chromium fleet.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::{Arc, Weak};
use std::time::Duration;

use anyhow::Result;
use axum::extract::{Request, State as AxumState};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use base64::Engine as _;
use clap::Parser;
use fluxwright::{
    netguard, with_timeout, BrowserEngine, ContextLease, HoverOptions, MarkdownOptions, NetEvent,
    PageLease, ScreenshotOptions, SelectEntry, SnapshotOptions, WaitState, WaitUntil,
};
use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig},
    schemars, tool, tool_handler, tool_router,
    transport::{
        stdio,
        streamable_http_server::{
            session::local::LocalSessionManager, SessionManager, StreamableHttpServerConfig,
            StreamableHttpService,
        },
    },
    ErrorData as McpError, ServerHandler, ServiceExt,
};
use tokio::sync::{MappedMutexGuard, Mutex, MutexGuard, OnceCell};
use tracing_subscriber::EnvFilter;

const MAX_NET_RECORDS: usize = 500;
const MIN_REMOTE_TOKEN_LEN: usize = 32;

#[derive(Debug, Parser)]
#[command(
    name = "fluxwright-mcp",
    version,
    about = "MCP server for Fluxwright (stdio by default, or Streamable HTTP with --http)"
)]
struct Cli {
    /// Serve MCP over Streamable HTTP on this address (e.g. 127.0.0.1:8088) instead of stdio
    #[arg(long, value_name = "ADDR")]
    http: Option<SocketAddr>,

    /// Allow binding a non-loopback --http address without FLUXWRIGHT_MCP_TOKEN
    #[arg(long, requires = "http")]
    allow_unauthenticated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct NetRecord {
    id: String,
    method: String,
    url: String,
    resource_type: String,
    status: Option<u16>,
    failed: Option<String>,
}

struct Tab {
    id: usize,
    page: PageLease,
    net_buf: Arc<std::sync::Mutex<VecDeque<NetRecord>>>,
    net_task: tokio::task::JoinHandle<()>,
    popup_task: tokio::task::JoinHandle<()>,
}

impl Drop for Tab {
    fn drop(&mut self) {
        self.net_task.abort();
        self.popup_task.abort();
    }
}

struct State {
    tabs: Vec<Tab>,
    context: Option<ContextLease>,
    active: usize,
    next_tab_id: usize,
    context_gen: u64,
}

impl Drop for State {
    fn drop(&mut self) {
        self.context_gen = self.context_gen.wrapping_add(1);
        self.tabs.clear();
        self.context = None;
    }
}

#[derive(Clone)]
struct FluxwrightMcp {
    state: Arc<Mutex<State>>,
    engine: Arc<OnceCell<BrowserEngine>>,
    headless: bool,
    tool_router: ToolRouter<Self>,
}

#[derive(Debug, Default, serde::Deserialize, schemars::JsonSchema)]
#[serde(default)]
struct OpenArgs {
    #[schemars(description = "Optional URL to open immediately")]
    url: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct UrlArgs {
    #[schemars(description = "Absolute URL, e.g. https://example.com")]
    url: String,
}

#[derive(Debug, Default, serde::Deserialize, schemars::JsonSchema)]
#[serde(default)]
struct SnapshotArgs {
    #[schemars(description = "Only include interactive controls in a flat list (default false)")]
    interactive_only: Option<bool>,
    #[schemars(description = "Omit unnamed generic/layout wrapper roles (default true)")]
    compact: Option<bool>,
    #[schemars(description = "Maximum tree depth between 1 and 64")]
    max_depth: Option<usize>,
    #[schemars(description = "Maximum nodes to emit between 1 and 5000 (default 1500)")]
    max_nodes: Option<usize>,
    #[schemars(description = "Return a compact line-level diff against the previous snapshot on this tab (default false)")]
    diff: Option<bool>,
}

#[derive(Debug, Default, serde::Deserialize, schemars::JsonSchema)]
#[serde(default)]
struct MarkdownArgs {
    #[schemars(description = "Optional CSS/locator/@eN selector subtree instead of whole page")]
    selector: Option<String>,
    #[schemars(description = "Prefer <main>, <article>, or [role=\"main\"] when selector is omitted (default false)")]
    main_content_only: Option<bool>,
    #[schemars(description = "Include [label](url) links (default true)")]
    include_links: Option<bool>,
    #[schemars(description = "Include ![alt](src) images (default false)")]
    include_images: Option<bool>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct SelectorArgs {
    #[schemars(description = "Target element selector: @eN ref, ref=eN, CSS selector, text=Visible text, or role=button[name=\"Save\"]")]
    selector: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct FillArgs {
    #[schemars(description = "Target input selector (@eN ref, CSS, text=, or role=)")]
    selector: String,
    #[schemars(description = "Text value to enter")]
    value: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct PressArgs {
    #[schemars(description = "Key name, e.g. Enter, Escape, Tab, ArrowDown, Control+a")]
    key: String,
    #[schemars(description = "Optional element selector to focus before pressing the key")]
    selector: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct SelectArgs {
    #[schemars(description = "Target <select> element selector (@eN ref, CSS, or role=)")]
    selector: String,
    #[schemars(description = "Option value attribute to select")]
    value: Option<String>,
    #[schemars(description = "Option visible text label to select")]
    label: Option<String>,
    #[schemars(description = "0-based option index to select")]
    index: Option<usize>,
}

#[derive(Debug, Default, serde::Deserialize, schemars::JsonSchema)]
#[serde(default)]
struct ScrollArgs {
    #[schemars(description = "Optional selector to scroll into view instead of wheel scrolling")]
    selector: Option<String>,
    #[schemars(description = "Horizontal mouse wheel delta in CSS pixels (default 0)")]
    delta_x: Option<f64>,
    #[schemars(description = "Vertical mouse wheel delta in CSS pixels (default 500)")]
    delta_y: Option<f64>,
}

#[derive(Debug, Default, serde::Deserialize, schemars::JsonSchema)]
#[serde(default)]
struct WaitForArgs {
    #[schemars(description = "Selector (@eN ref, CSS, text=, or role=) to wait for")]
    selector: Option<String>,
    #[schemars(description = "Selector state: visible (default), attached, detached, or hidden")]
    state: Option<String>,
    #[schemars(description = "URL glob pattern to wait for instead of a selector")]
    url: Option<String>,
    #[schemars(description = "Optional timeout in milliseconds (clamped 1..120000)")]
    timeout_ms: Option<u64>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct TabsArgs {
    #[schemars(description = "Tab action: list, new, switch, or close")]
    action: String,
    #[schemars(description = "0-based tab index for switch or close")]
    index: Option<usize>,
    #[schemars(description = "Optional URL to open when action=new")]
    url: Option<String>,
}

#[derive(Debug, Default, serde::Deserialize, schemars::JsonSchema)]
#[serde(default)]
struct ConsoleArgs {
    #[schemars(description = "Maximum recent entries to return (default 50, max 200)")]
    limit: Option<usize>,
}

#[derive(Debug, Default, serde::Deserialize, schemars::JsonSchema)]
#[serde(default)]
struct NetworkArgs {
    #[schemars(description = "Maximum recent requests to return (default 50, max 200)")]
    limit: Option<usize>,
    #[schemars(description = "Optional case-insensitive substring filter on URL, method, or resource type")]
    filter: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct EvalArgs {
    #[schemars(description = "JavaScript expression, e.g. document.title")]
    expression: String,
}

#[derive(Debug, Default, serde::Deserialize, schemars::JsonSchema)]
#[serde(default)]
struct ScreenshotArgs {
    #[schemars(description = "Capture the full scrollable page instead of just the viewport (default false)")]
    full_page: Option<bool>,
    #[schemars(description = "Overlay Set-of-Marks @eN labels on interactive elements before capture (default false)")]
    annotate: Option<bool>,
}

impl FluxwrightMcp {
    fn new(headless: bool) -> Self {
        Self::with_engine(Arc::new(OnceCell::new()), headless)
    }

    fn with_engine(engine: Arc<OnceCell<BrowserEngine>>, headless: bool) -> Self {
        Self {
            state: Arc::new(Mutex::new(State {
                tabs: Vec::new(),
                context: None,
                active: 0,
                next_tab_id: 1,
                context_gen: 0,
            })),
            engine,
            headless,
            tool_router: Self::tool_router(),
        }
    }

    async fn ensure_context(
        st: &mut State,
        engine_cell: &OnceCell<BrowserEngine>,
        headless: bool,
    ) -> fluxwright::Result<()> {
        let ctx_closed = st.context.as_ref().is_some_and(ContextLease::is_closed);
        if st.context.is_none() || ctx_closed {
            let engine = engine_cell
                .get_or_try_init(|| async move {
                    BrowserEngine::builder()
                        .max_browsers(2)
                        .max_contexts_per_browser(4)
                        .headless(headless)
                        .acquire_timeout(Duration::from_secs(60))
                        .build()
                        .await
                })
                .await?;
            let ctx = engine.acquire_context().await?;
            st.context = Some(ctx);
            st.context_gen += 1;
        }
        Ok(())
    }

    fn wrap_tab(
        page: PageLease,
        id: usize,
        state: Weak<Mutex<State>>,
        context_gen: u64,
    ) -> Tab {
        let mut rx = page.subscribe_network();
        let net_buf = Arc::new(std::sync::Mutex::new(VecDeque::with_capacity(64)));
        let buf = net_buf.clone();
        let net_task = tokio::spawn(async move {
            while let Some(ev) = rx.recv().await {
                record_net_event(&buf, ev);
            }
        });
        let mut popup_rx = page.subscribe_popups();
        let popup_task = tokio::spawn(async move {
            while let Some(popup_page) = popup_rx.recv().await {
                if !any_url() {
                    if let Err(err) = netguard::guard_requests(&popup_page).await {
                        tracing::warn!(%err, "failed to guard popup requests");
                        continue;
                    }
                }
                let Some(state_arc) = state.upgrade() else {
                    break;
                };
                let mut st = state_arc.lock().await;
                if st.context_gen != context_gen || popup_page.is_closed() {
                    continue;
                }
                let popup_id = st.next_tab_id;
                st.next_tab_id += 1;
                let popup_tab =
                    Self::wrap_tab(popup_page, popup_id, Arc::downgrade(&state_arc), context_gen);
                st.tabs.push(popup_tab);
            }
        });
        Tab {
            id,
            page,
            net_buf,
            net_task,
            popup_task,
        }
    }

    async fn attach_tab(
        page: PageLease,
        id: usize,
        state: &Arc<Mutex<State>>,
        context_gen: u64,
    ) -> fluxwright::Result<Tab> {
        if !any_url() {
            netguard::guard_requests(&page).await?;
        }
        Ok(Self::wrap_tab(page, id, Arc::downgrade(state), context_gen))
    }

    async fn create_tab(
        st: &mut State,
        state: &Arc<Mutex<State>>,
        engine: &OnceCell<BrowserEngine>,
        headless: bool,
    ) -> fluxwright::Result<Tab> {
        Self::ensure_context(st, engine, headless).await?;
        let page = st
            .context
            .as_ref()
            .expect("made above")
            .new_page()
            .await?;
        let id = st.next_tab_id;
        st.next_tab_id += 1;
        let ctx_gen = st.context_gen;
        Self::attach_tab(page, id, state, ctx_gen).await
    }

    /// Replaces the current context with a fresh single-tab context and returns a guard to that tab.
    async fn open_fresh(&self) -> fluxwright::Result<MappedMutexGuard<'_, Tab>> {
        let mut st = self.state.lock().await;
        st.context_gen += 1;
        st.tabs.clear();
        st.context = None;
        st.active = 0;
        let tab = Self::create_tab(&mut st, &self.state, &self.engine, self.headless).await?;
        st.tabs.push(tab);
        st.active = 0;
        Ok(MutexGuard::map(st, |s| &mut s.tabs[0]))
    }

    /// Returns a lock-held guard to the active tab, creating an engine/context/tab if empty.
    async fn active_tab(&self) -> fluxwright::Result<MappedMutexGuard<'_, Tab>> {
        let mut st = self.state.lock().await;
        st.tabs.retain(|t| !t.page.is_closed());
        if st.tabs.is_empty() {
            let tab = Self::create_tab(&mut st, &self.state, &self.engine, self.headless).await?;
            st.tabs.push(tab);
            st.active = 0;
        } else if st.active >= st.tabs.len() {
            st.active = st.tabs.len() - 1;
        }
        let active = st.active;
        Ok(MutexGuard::map(st, |s| &mut s.tabs[active]))
    }
}

fn record_net_event(buf: &Arc<std::sync::Mutex<VecDeque<NetRecord>>>, ev: NetEvent) {
    let mut guard = buf.lock().unwrap();
    match ev {
        NetEvent::Request(req) => {
            while guard.len() >= MAX_NET_RECORDS {
                guard.pop_front();
            }
            guard.push_back(NetRecord {
                id: req.id,
                method: req.method,
                url: req.url,
                resource_type: req.resource_type,
                status: None,
                failed: None,
            });
        }
        NetEvent::Response(resp) => {
            if let Some(rec) = guard.iter_mut().rev().find(|r| r.id == resp.request.id) {
                rec.status = Some(resp.status);
            } else {
                while guard.len() >= MAX_NET_RECORDS {
                    guard.pop_front();
                }
                guard.push_back(NetRecord {
                    id: resp.request.id,
                    method: resp.request.method,
                    url: resp.url,
                    resource_type: resp.request.resource_type,
                    status: Some(resp.status),
                    failed: None,
                });
            }
        }
        NetEvent::Failed(req, reason) => {
            if let Some(rec) = guard.iter_mut().rev().find(|r| r.id == req.id) {
                rec.failed = Some(reason);
            } else {
                while guard.len() >= MAX_NET_RECORDS {
                    guard.pop_front();
                }
                guard.push_back(NetRecord {
                    id: req.id,
                    method: req.method,
                    url: req.url,
                    resource_type: req.resource_type,
                    status: None,
                    failed: Some(reason),
                });
            }
        }
        NetEvent::Finished(_) => {}
    }
}

fn tool_ok(text: impl Into<String>) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(text)])
}

fn tool_err(e: impl std::fmt::Display) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(e.to_string())])
}

/// FLUXWRIGHT_MCP_ANY_URL lifts the URL rules below.
fn any_url() -> bool {
    std::env::var_os("FLUXWRIGHT_MCP_ANY_URL").is_some()
}

fn check_url(url: &str) -> Result<(), String> {
    if any_url() {
        return Ok(());
    }
    netguard::url_allowed(url)
        .map_err(|why| format!("{why}: {url} (FLUXWRIGHT_MCP_ANY_URL=1 allows any URL)"))
}

/// `text` cut to at most `max` bytes on a character boundary (a byte cut panicked inside a
/// multi-byte character).
fn truncate(text: String, max: usize) -> String {
    if text.len() <= max {
        return text;
    }
    let cut = text.floor_char_boundary(max);
    format!(
        "{}…\n[truncated: {} bytes in all]",
        &text[..cut],
        text.len()
    )
}

fn parse_wait_state(state: Option<&str>) -> Result<WaitState, McpError> {
    match state {
        None | Some("visible") => Ok(WaitState::Visible),
        Some("attached") => Ok(WaitState::Attached),
        Some("detached") => Ok(WaitState::Detached),
        Some("hidden") => Ok(WaitState::Hidden),
        Some(other) => Err(McpError::invalid_params(
            format!("wait_for state must be attached, detached, visible, or hidden, got {other:?}"),
            None,
        )),
    }
}

fn build_select_entry(
    value: Option<String>,
    label: Option<String>,
    index: Option<usize>,
) -> Result<SelectEntry, McpError> {
    match (value, label, index) {
        (Some(v), None, None) => Ok(SelectEntry::Value(v)),
        (None, Some(l), None) => Ok(SelectEntry::Label(l)),
        (None, None, Some(i)) => Ok(SelectEntry::Index(i)),
        (None, None, None) => Err(McpError::invalid_params(
            "select requires one of value, label, or index",
            None,
        )),
        _ => Err(McpError::invalid_params(
            "select takes only one of value, label, or index per call",
            None,
        )),
    }
}

fn resolve_switch_index(len: usize, index: Option<usize>) -> Result<usize, McpError> {
    if len == 0 {
        return Err(McpError::invalid_params("no open tabs", None));
    }
    let idx = index.ok_or_else(|| McpError::invalid_params("tabs switch requires index", None))?;
    if idx >= len {
        return Err(McpError::invalid_params(
            format!("tab index {idx} out of range (0..{len})"),
            None,
        ));
    }
    Ok(idx)
}

fn resolve_close_indices(len: usize, active: usize, index: Option<usize>) -> Result<(usize, usize), McpError> {
    if len == 0 {
        return Err(McpError::invalid_params("no open tabs", None));
    }
    let remove_idx = index.unwrap_or(active);
    if remove_idx >= len {
        return Err(McpError::invalid_params(
            format!("tab index {remove_idx} out of range (0..{len})"),
            None,
        ));
    }
    let remaining = len - 1;
    let new_active = if remaining == 0 {
        0
    } else if remove_idx < active {
        active - 1
    } else {
        active.min(remaining - 1)
    };
    Ok((remove_idx, new_active))
}

fn format_console(
    messages: &[fluxwright::ConsoleMessage],
    errors: &[fluxwright::PageError],
    limit: usize,
) -> String {
    let cap = limit.min(200);
    if cap == 0 || (messages.is_empty() && errors.is_empty()) {
        return "no console output or page errors".into();
    }
    let mut lines = Vec::with_capacity(messages.len() + errors.len());
    for msg in messages {
        lines.push(format!("[{}] {}", msg.kind, msg.text));
    }
    for err in errors {
        lines.push(format!("[pageerror] {}: {}", err.name, err.message));
    }
    let start = lines.len().saturating_sub(cap);
    lines[start..].join("\n")
}

fn format_net_records(records: &VecDeque<NetRecord>, limit: usize, filter: Option<&str>) -> String {
    let cap = limit.min(200);
    let needle = filter.map(str::trim).filter(|s| !s.is_empty()).map(str::to_lowercase);
    let matched: Vec<String> = records
        .iter()
        .filter(|rec| {
            needle.as_ref().is_none_or(|n| {
                rec.url.to_lowercase().contains(n)
                    || rec.method.to_lowercase().contains(n)
                    || rec.resource_type.to_lowercase().contains(n)
            })
        })
        .map(|rec| {
            let outcome = rec
                .status
                .map(|s| s.to_string())
                .or_else(|| rec.failed.as_ref().map(|f| format!("FAILED: {f}")))
                .unwrap_or_else(|| "pending".into());
            format!("{} [{}] {} -> {outcome}", rec.method, rec.resource_type, rec.url)
        })
        .collect();
    if cap == 0 || matched.is_empty() {
        return "no matching network requests".into();
    }
    let start = matched.len().saturating_sub(cap);
    matched[start..].join("\n")
}

#[tool_router]
impl FluxwrightMcp {
    #[tool(description = "Open a fresh browser context and tab. Pass url to navigate immediately.")]
    async fn open(
        &self,
        Parameters(OpenArgs { url }): Parameters<OpenArgs>,
    ) -> Result<CallToolResult, McpError> {
        if let Some(url) = &url {
            if let Err(msg) = check_url(url) {
                return Ok(tool_err(msg));
            }
        }
        let tab = match self.open_fresh().await {
            Ok(t) => t,
            Err(e) => return Ok(tool_err(e)),
        };
        if let Some(url) = url {
            if let Err(e) = tab.page.goto(&url).await {
                return Ok(tool_err(e));
            }
            let title = tab.page.title().await.unwrap_or_default();
            return Ok(tool_ok(format!("opened {url}\ntitle: {title}")));
        }
        Ok(tool_ok("Chromium page ready (about:blank)"))
    }

    #[tool(description = "Navigate the active tab to a URL.")]
    async fn goto(
        &self,
        Parameters(UrlArgs { url }): Parameters<UrlArgs>,
    ) -> Result<CallToolResult, McpError> {
        if let Err(msg) = check_url(&url) {
            return Ok(tool_err(msg));
        }
        let tab = match self.active_tab().await {
            Ok(t) => t,
            Err(e) => return Ok(tool_err(e)),
        };
        if let Err(e) = tab.page.goto(&url).await {
            return Ok(tool_err(e));
        }
        let title = tab.page.title().await.unwrap_or_default();
        Ok(tool_ok(format!("navigated to {url}\ntitle: {title}")))
    }

    #[tool(description = "Return the active tab's title.")]
    async fn title(&self) -> Result<CallToolResult, McpError> {
        let tab = match self.active_tab().await {
            Ok(t) => t,
            Err(e) => return Ok(tool_err(e)),
        };
        match tab.page.title().await {
            Ok(t) => Ok(tool_ok(t)),
            Err(e) => Ok(tool_err(e)),
        }
    }

    #[tool(description = "Return the active tab's raw HTML (truncated). Prefer snapshot() or markdown() first.")]
    async fn content(&self) -> Result<CallToolResult, McpError> {
        let tab = match self.active_tab().await {
            Ok(t) => t,
            Err(e) => return Ok(tool_err(e)),
        };
        match tab.page.content().await {
            Ok(html) => Ok(tool_ok(truncate(html, 40_000))),
            Err(e) => Ok(tool_err(e)),
        }
    }

    #[tool(description = "Capture a compact ARIA accessibility tree with short @eN element refs for click/fill/hover/select (set diff=true for incremental changes).")]
    async fn snapshot(
        &self,
        Parameters(args): Parameters<SnapshotArgs>,
    ) -> Result<CallToolResult, McpError> {
        if args.max_depth.is_some_and(|d| d == 0 || d > 64) {
            return Err(McpError::invalid_params("max_depth must be between 1 and 64", None));
        }
        if args.max_nodes.is_some_and(|n| n == 0 || n > 5000) {
            return Err(McpError::invalid_params("max_nodes must be between 1 and 5000", None));
        }
        let mut opts = SnapshotOptions {
            interactive_only: args.interactive_only.unwrap_or(false),
            compact: args.compact.unwrap_or(true),
            max_depth: args.max_depth,
            diff: args.diff.unwrap_or(false),
            ..Default::default()
        };
        if let Some(limit) = args.max_nodes {
            opts.max_nodes = limit;
        }
        let tab = match self.active_tab().await {
            Ok(t) => t,
            Err(e) => return Ok(tool_err(e)),
        };
        let snap = match tab.page.aria_snapshot_with(&opts).await {
            Ok(s) => s,
            Err(e) => return Ok(tool_err(e)),
        };
        let url = tab.page.url();
        let title = tab.page.title().await.unwrap_or_default();
        let mut out = format!(
            "url: {url}\ntitle: {title}\nrefs: {}\ndiff: {}\nchanged: {}\n\n{}",
            snap.refs.len(),
            snap.is_diff,
            snap.changed,
            snap.tree
        );
        if snap.truncated {
            out.push_str("\n\n[snapshot truncated: narrow with max_depth or interactive_only=true]");
        }
        Ok(tool_ok(truncate(out, 40_000)))
    }

    #[tool(description = "Extract visible page content (or a matching selector/main landmark) as clean Markdown.")]
    async fn markdown(
        &self,
        Parameters(args): Parameters<MarkdownArgs>,
    ) -> Result<CallToolResult, McpError> {
        let opts = MarkdownOptions {
            selector: args.selector,
            main_content_only: args.main_content_only.unwrap_or(false),
            include_links: args.include_links.unwrap_or(true),
            include_images: args.include_images.unwrap_or(false),
        };
        let tab = match self.active_tab().await {
            Ok(t) => t,
            Err(e) => return Ok(tool_err(e)),
        };
        match tab.page.markdown_with(&opts).await {
            Ok(md) => Ok(tool_ok(truncate(md, 40_000))),
            Err(e) => Ok(tool_err(e)),
        }
    }

    #[tool(description = "Click an element by @eN snapshot ref, CSS, text=, or role= selector.")]
    async fn click(
        &self,
        Parameters(SelectorArgs { selector }): Parameters<SelectorArgs>,
    ) -> Result<CallToolResult, McpError> {
        let tab = match self.active_tab().await {
            Ok(t) => t,
            Err(e) => return Ok(tool_err(e)),
        };
        match tab.page.click(&selector).await {
            Ok(()) => Ok(tool_ok(format!("clicked {selector}"))),
            Err(e) => Ok(tool_err(e)),
        }
    }

    #[tool(description = "Hover over an element by @eN snapshot ref, CSS, text=, or role= selector.")]
    async fn hover(
        &self,
        Parameters(SelectorArgs { selector }): Parameters<SelectorArgs>,
    ) -> Result<CallToolResult, McpError> {
        let tab = match self.active_tab().await {
            Ok(t) => t,
            Err(e) => return Ok(tool_err(e)),
        };
        match tab.page.hover(&selector, &HoverOptions::default()).await {
            Ok(()) => Ok(tool_ok(format!("hovered {selector}"))),
            Err(e) => Ok(tool_err(e)),
        }
    }

    #[tool(description = "Fill an input/textarea by @eN snapshot ref, CSS, text=, or role= selector.")]
    async fn fill(
        &self,
        Parameters(FillArgs { selector, value }): Parameters<FillArgs>,
    ) -> Result<CallToolResult, McpError> {
        let tab = match self.active_tab().await {
            Ok(t) => t,
            Err(e) => return Ok(tool_err(e)),
        };
        match tab.page.fill(&selector, &value).await {
            Ok(()) => Ok(tool_ok(format!("filled {selector}"))),
            Err(e) => Ok(tool_err(e)),
        }
    }

    #[tool(description = "Press a key (e.g. Enter, Tab, Escape), optionally after focusing selector.")]
    async fn press(
        &self,
        Parameters(PressArgs { key, selector }): Parameters<PressArgs>,
    ) -> Result<CallToolResult, McpError> {
        let tab = match self.active_tab().await {
            Ok(t) => t,
            Err(e) => return Ok(tool_err(e)),
        };
        if let Some(sel) = &selector {
            match tab.page.press(sel, &key, None).await {
                Ok(()) => Ok(tool_ok(format!("pressed {key} on {sel}"))),
                Err(e) => Ok(tool_err(e)),
            }
        } else {
            match tab.page.keyboard_press(&key, None).await {
                Ok(()) => Ok(tool_ok(format!("pressed {key}"))),
                Err(e) => Ok(tool_err(e)),
            }
        }
    }

    #[tool(description = "Select an option in a <select> element by value, label, or 0-based index.")]
    async fn select(
        &self,
        Parameters(SelectArgs {
            selector,
            value,
            label,
            index,
        }): Parameters<SelectArgs>,
    ) -> Result<CallToolResult, McpError> {
        let entry = build_select_entry(value, label, index)?;
        let tab = match self.active_tab().await {
            Ok(t) => t,
            Err(e) => return Ok(tool_err(e)),
        };
        match tab.page.select_option(&selector, &[entry], false).await {
            Ok(selected) => Ok(tool_ok(format!("selected {selected:?} on {selector}"))),
            Err(e) => Ok(tool_err(e)),
        }
    }

    #[tool(description = "Scroll an element into view (with selector) or scroll the viewport by wheel deltas.")]
    async fn scroll(
        &self,
        Parameters(ScrollArgs {
            selector,
            delta_x,
            delta_y,
        }): Parameters<ScrollArgs>,
    ) -> Result<CallToolResult, McpError> {
        let tab = match self.active_tab().await {
            Ok(t) => t,
            Err(e) => return Ok(tool_err(e)),
        };
        if let Some(sel) = &selector {
            match tab.page.scroll_into_view_if_needed(sel).await {
                Ok(()) => Ok(tool_ok(format!("scrolled {sel} into view"))),
                Err(e) => Ok(tool_err(e)),
            }
        } else {
            let dx = delta_x.unwrap_or(0.0);
            let dy = delta_y.unwrap_or(500.0);
            match tab.page.mouse_wheel(dx, dy).await {
                Ok(()) => Ok(tool_ok(format!("scrolled by ({dx}, {dy})"))),
                Err(e) => Ok(tool_err(e)),
            }
        }
    }

    #[tool(description = "Wait for a selector state (visible, attached, detached, hidden) or URL glob match.")]
    async fn wait_for(
        &self,
        Parameters(WaitForArgs {
            selector,
            state,
            url,
            timeout_ms,
        }): Parameters<WaitForArgs>,
    ) -> Result<CallToolResult, McpError> {
        if selector.is_none() && url.is_none() {
            return Err(McpError::invalid_params(
                "wait_for requires selector or url",
                None,
            ));
        }
        let wait_state = parse_wait_state(state.as_deref())?;
        let timeout = timeout_ms.map(|ms| Duration::from_millis(ms.clamp(1, 120_000)));
        let tab = match self.active_tab().await {
            Ok(t) => t,
            Err(e) => return Ok(tool_err(e)),
        };
        let res = with_timeout(timeout, async {
            if let Some(sel) = &selector {
                tab.page.wait_for_selector_state(sel, wait_state).await?;
            }
            if let Some(pattern) = &url {
                tab.page.wait_for_url(pattern, WaitUntil::Load).await?;
            }
            Ok::<_, fluxwright::Error>(())
        })
        .await;
        match res {
            Ok(()) => Ok(tool_ok("wait condition satisfied")),
            Err(e) => Ok(tool_err(e)),
        }
    }

    #[tool(description = "Navigate back in session history on the active tab.")]
    async fn back(&self) -> Result<CallToolResult, McpError> {
        let tab = match self.active_tab().await {
            Ok(t) => t,
            Err(e) => return Ok(tool_err(e)),
        };
        match tab.page.go_back().await {
            Ok(true) => Ok(tool_ok(format!("went back to {}", tab.page.url()))),
            Ok(false) => Ok(tool_ok("no previous history entry")),
            Err(e) => Ok(tool_err(e)),
        }
    }

    #[tool(description = "Navigate forward in session history on the active tab.")]
    async fn forward(&self) -> Result<CallToolResult, McpError> {
        let tab = match self.active_tab().await {
            Ok(t) => t,
            Err(e) => return Ok(tool_err(e)),
        };
        match tab.page.go_forward().await {
            Ok(true) => Ok(tool_ok(format!("went forward to {}", tab.page.url()))),
            Ok(false) => Ok(tool_ok("no forward history entry")),
            Err(e) => Ok(tool_err(e)),
        }
    }

    #[tool(description = "Manage tabs in the current browser context: action is list, new, switch, or close.")]
    async fn tabs(
        &self,
        Parameters(TabsArgs { action, index, url }): Parameters<TabsArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut st = self.state.lock().await;
        st.tabs.retain(|t| !t.page.is_closed());
        if st.active >= st.tabs.len() && !st.tabs.is_empty() {
            st.active = st.tabs.len() - 1;
        }
        match action.as_str() {
            "list" => {
                if st.tabs.is_empty() {
                    return Ok(tool_ok("no open tabs"));
                }
                let mut lines = Vec::with_capacity(st.tabs.len());
                for (i, tab) in st.tabs.iter().enumerate() {
                    let marker = if i == st.active { "*" } else { " " };
                    let title = tab.page.title().await.unwrap_or_default();
                    lines.push(format!("[{i}]{marker} #{} {} — {title}", tab.id, tab.page.url()));
                }
                Ok(tool_ok(lines.join("\n")))
            }
            "new" => {
                if let Some(u) = &url {
                    if let Err(msg) = check_url(u) {
                        return Ok(tool_err(msg));
                    }
                }
                let tab = match Self::create_tab(&mut st, &self.state, &self.engine, self.headless)
                    .await
                {
                    Ok(t) => t,
                    Err(e) => return Ok(tool_err(e)),
                };
                if let Some(u) = &url {
                    if let Err(e) = tab.page.goto(u).await {
                        return Ok(tool_err(e));
                    }
                }
                st.tabs.push(tab);
                st.active = st.tabs.len() - 1;
                let _ = st.tabs[st.active].page.bring_to_front().await;
                let cur = &st.tabs[st.active];
                Ok(tool_ok(format!(
                    "opened tab {} #{}: {}",
                    st.active,
                    cur.id,
                    cur.page.url()
                )))
            }
            "switch" => {
                let idx = resolve_switch_index(st.tabs.len(), index)?;
                st.active = idx;
                if let Err(e) = st.tabs[idx].page.bring_to_front().await {
                    return Ok(tool_err(e));
                }
                Ok(tool_ok(format!(
                    "switched to tab {idx} #{}: {}",
                    st.tabs[idx].id,
                    st.tabs[idx].page.url()
                )))
            }
            "close" => {
                let (remove_idx, new_active) = resolve_close_indices(st.tabs.len(), st.active, index)?;
                let closed = st.tabs.remove(remove_idx);
                if st.tabs.is_empty() {
                    st.context_gen += 1;
                    st.context = None;
                    st.active = 0;
                    Ok(tool_ok(format!(
                        "closed tab {remove_idx} #{} (context now empty)",
                        closed.id
                    )))
                } else {
                    st.active = new_active;
                    Ok(tool_ok(format!(
                        "closed tab {remove_idx} #{}; active tab is {new_active}",
                        closed.id
                    )))
                }
            }
            other => Err(McpError::invalid_params(
                format!("tabs action must be list, new, switch, or close, got {other:?}"),
                None,
            )),
        }
    }

    #[tool(description = "Read recent console messages and uncaught page errors from the active tab.")]
    async fn console(
        &self,
        Parameters(ConsoleArgs { limit }): Parameters<ConsoleArgs>,
    ) -> Result<CallToolResult, McpError> {
        let tab = match self.active_tab().await {
            Ok(t) => t,
            Err(e) => return Ok(tool_err(e)),
        };
        let out = format_console(
            &tab.page.console_messages(),
            &tab.page.page_errors(),
            limit.unwrap_or(50),
        );
        Ok(tool_ok(truncate(out, 20_000)))
    }

    #[tool(description = "Read recent network requests and response statuses/failures captured on the active tab.")]
    async fn network(
        &self,
        Parameters(NetworkArgs { limit, filter }): Parameters<NetworkArgs>,
    ) -> Result<CallToolResult, McpError> {
        let tab = match self.active_tab().await {
            Ok(t) => t,
            Err(e) => return Ok(tool_err(e)),
        };
        let out = {
            let guard = tab.net_buf.lock().unwrap();
            format_net_records(&guard, limit.unwrap_or(50), filter.as_deref())
        };
        Ok(tool_ok(truncate(out, 20_000)))
    }

    #[tool(description = "Evaluate a JavaScript expression in the active tab and return JSON.")]
    async fn evaluate(
        &self,
        Parameters(EvalArgs { expression }): Parameters<EvalArgs>,
    ) -> Result<CallToolResult, McpError> {
        let tab = match self.active_tab().await {
            Ok(t) => t,
            Err(e) => return Ok(tool_err(e)),
        };
        match tab.page.evaluate(&expression).await {
            Ok(v) => Ok(tool_ok(v.to_string())),
            Err(e) => Ok(tool_err(e)),
        }
    }

    #[tool(description = "Take a PNG screenshot of the active tab (viewport by default, or full_page=true; annotate=true overlays @eN labels).")]
    async fn screenshot(
        &self,
        Parameters(ScreenshotArgs { full_page, annotate }): Parameters<ScreenshotArgs>,
    ) -> Result<CallToolResult, McpError> {
        let opts = ScreenshotOptions {
            full_page: full_page.unwrap_or(false),
            annotate: annotate.unwrap_or(false),
            ..Default::default()
        };
        let tab = match self.active_tab().await {
            Ok(t) => t,
            Err(e) => return Ok(tool_err(e)),
        };
        let png = match tab.page.screenshot_with(&opts).await {
            Ok(bytes) => bytes,
            Err(e) => return Ok(tool_err(e)),
        };
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
        Ok(CallToolResult::success(vec![ContentBlock::image(
            b64,
            "image/png",
        )]))
    }

    #[tool(description = "Close all tabs and destroy the current browser context.")]
    async fn close(&self) -> Result<CallToolResult, McpError> {
        let mut st = self.state.lock().await;
        st.context_gen += 1;
        st.tabs.clear();
        st.context = None;
        st.active = 0;
        Ok(tool_ok("page closed"))
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for FluxwrightMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("fluxwright-mcp", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Fluxwright Chromium fleet. Tools: open, goto, title, content, snapshot, markdown, \
                 click, hover, fill, press, select, scroll, wait_for, back, forward, tabs, \
                 console, network, evaluate, screenshot, close. \
                 Each open() starts a fresh browser context. Use snapshot() for @eN element refs \
                 (pass @eN as selector in click/hover/fill/press/select/scroll/wait_for) and \
                 markdown() for clean text extraction before falling back to content() or screenshot().",
            )
    }
}

#[derive(Clone)]
struct ExpectedToken(Arc<Vec<u8>>);

impl ExpectedToken {
    fn new(token: String) -> Self {
        Self(Arc::new(token.into_bytes()))
    }
}

/// Compare supplied bytes against the expected secret without early exit on byte mismatches.
fn constant_time_eq(supplied: &[u8], expected: &[u8]) -> bool {
    let mut diff = supplied.len() ^ expected.len();
    for (i, &b) in expected.iter().enumerate() {
        let s = supplied.get(i).copied().unwrap_or(0);
        diff |= usize::from(s ^ b);
    }
    diff == 0
}

fn extract_bearer_token(headers: &HeaderMap) -> Option<&[u8]> {
    let mut values = headers.get_all(header::AUTHORIZATION).iter();
    let value = values.next()?;
    if values.next().is_some() {
        return None;
    }
    let (scheme, token) = value.as_bytes().split_at_checked(7)?;
    if !scheme.eq_ignore_ascii_case(b"Bearer ") {
        return None;
    }
    Some(token)
}

fn unauthorized_response() -> Response {
    let mut resp = (
        StatusCode::UNAUTHORIZED,
        Json(serde_json::json!({
            "error": "unauthorized",
            "message": "valid Authorization: Bearer token required",
        })),
    )
        .into_response();
    resp.headers_mut()
        .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
    resp
}

async fn require_bearer_auth(
    AxumState(expected): AxumState<ExpectedToken>,
    request: Request,
    next: Next,
) -> Response {
    let authorized = extract_bearer_token(request.headers())
        .is_some_and(|supplied| constant_time_eq(supplied, expected.0.as_slice()));
    if !authorized {
        return unauthorized_response();
    }
    next.run(request).await
}

fn validate_http_security(
    addr: SocketAddr,
    token: Option<&str>,
    allow_unauthenticated: bool,
) -> Result<Option<ExpectedToken>> {
    let token = token.filter(|v| !v.is_empty());
    if token.is_some_and(|tok| !tok.bytes().all(|b| b.is_ascii_graphic())) {
        anyhow::bail!(
            "FLUXWRIGHT_MCP_TOKEN must contain only printable ASCII characters without whitespace"
        );
    }
    if !addr.ip().is_loopback() && !allow_unauthenticated {
        match token {
            None => anyhow::bail!(
                "refusing to bind non-loopback address {addr} without authentication: \
                 set FLUXWRIGHT_MCP_TOKEN (at least {MIN_REMOTE_TOKEN_LEN} characters) \
                 or pass --allow-unauthenticated"
            ),
            Some(tok) if tok.len() < MIN_REMOTE_TOKEN_LEN => anyhow::bail!(
                "refusing to bind non-loopback address {addr}: FLUXWRIGHT_MCP_TOKEN must be \
                 at least {MIN_REMOTE_TOKEN_LEN} characters (got {}), or pass --allow-unauthenticated",
                tok.len()
            ),
            Some(_) => {}
        }
    }
    Ok(token.map(|t| ExpectedToken::new(t.to_owned())))
}

fn build_http_server_config(addr: SocketAddr) -> StreamableHttpServerConfig {
    let mut config = StreamableHttpServerConfig::default();
    if addr.ip().is_loopback() {
        let ip_str = addr.ip().to_string();
        if !config.allowed_hosts.iter().any(|h| h == &ip_str) {
            config.allowed_hosts.push(ip_str);
        }
    } else {
        config = config.disable_allowed_hosts();
    }
    config
}

async fn healthz() -> Response {
    let mut resp = Json(serde_json::json!({
        "status": "ok",
        "transport": "streamable-http",
    }))
    .into_response();
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    resp
}

fn build_http_router(
    engine_cell: Arc<OnceCell<BrowserEngine>>,
    headless: bool,
    session_manager: Arc<LocalSessionManager>,
    http_config: StreamableHttpServerConfig,
    expected_token: Option<ExpectedToken>,
) -> Router {
    let mcp_service = StreamableHttpService::new(
        move || Ok::<_, std::io::Error>(FluxwrightMcp::with_engine(Arc::clone(&engine_cell), headless)),
        session_manager,
        http_config,
    );

    let mcp_router = Router::new().nest_service("/mcp", mcp_service);
    let mcp_router = if let Some(token) = expected_token {
        mcp_router.route_layer(middleware::from_fn_with_state(token, require_bearer_auth))
    } else {
        mcp_router
    };

    Router::new()
        .route("/healthz", get(healthz))
        .merge(mcp_router)
}

async fn close_all_sessions(session_manager: &LocalSessionManager) {
    let ids: Vec<_> = session_manager.sessions.read().await.keys().cloned().collect();
    for id in ids {
        let _ = session_manager.close_session(&id).await;
    }
}

async fn shutdown_signal() -> std::io::Result<()> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result?,
            _ = terminate.recv() => {},
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await
    }
}

async fn run_stdio(headless: bool) -> Result<()> {
    tracing::info!(headless, "fluxwright MCP on stdio");
    let mcp = FluxwrightMcp::new(headless);
    let state = Arc::clone(&mcp.state);
    let engine_cell = Arc::clone(&mcp.engine);
    let service = mcp
        .serve(stdio())
        .await
        .inspect_err(|e| tracing::error!("serve: {e}"))?;
    let wait_res = service.waiting().await;
    {
        let mut st = state.lock().await;
        st.context_gen = st.context_gen.wrapping_add(1);
        st.tabs.clear();
        st.context = None;
    }
    if let Some(engine) = engine_cell.get() {
        if let Err(err) = engine.shutdown(Duration::from_secs(5)).await {
            tracing::warn!(%err, "error shutting down browser engine");
        }
    }
    wait_res?;
    Ok(())
}

async fn run_http(addr: SocketAddr, allow_unauthenticated: bool, headless: bool) -> Result<()> {
    let raw_token = std::env::var("FLUXWRIGHT_MCP_TOKEN").ok();
    let expected_token = validate_http_security(addr, raw_token.as_deref(), allow_unauthenticated)?;

    let listener = tokio::net::TcpListener::bind(addr).await?;
    let bound = listener.local_addr()?;

    let engine_cell = Arc::new(OnceCell::new());
    let session_manager = Arc::new(LocalSessionManager::default());
    let http_config = build_http_server_config(bound);
    let ct = http_config.cancellation_token.clone();
    let auth_enabled = expected_token.is_some();

    let app = build_http_router(
        Arc::clone(&engine_cell),
        headless,
        Arc::clone(&session_manager),
        http_config,
        expected_token,
    );

    if auth_enabled {
        tracing::info!(%bound, headless, "fluxwright MCP on Streamable HTTP (bearer auth enabled)");
    } else if !bound.ip().is_loopback() {
        tracing::warn!(
            %bound,
            headless,
            "fluxwright MCP on non-loopback Streamable HTTP WITHOUT authentication"
        );
    } else {
        tracing::info!(
            %bound,
            headless,
            "fluxwright MCP on loopback Streamable HTTP (no bearer token set)"
        );
    }

    let shutdown_ct = ct.clone();
    let shutdown_sessions = Arc::clone(&session_manager);
    let serve_res = axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            if let Err(err) = shutdown_signal().await {
                tracing::warn!(%err, "failed waiting for shutdown signal");
            }
            tracing::info!("shutting down fluxwright-mcp HTTP server");
            shutdown_ct.cancel();
            close_all_sessions(&shutdown_sessions).await;
        })
        .await;

    ct.cancel();
    close_all_sessions(&session_manager).await;
    if let Some(engine) = engine_cell.get() {
        if let Err(err) = engine.shutdown(Duration::from_secs(5)).await {
            tracing::warn!(%err, "error shutting down browser engine");
        }
    }
    serve_res?;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("fluxwright=info".parse()?))
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let headless = std::env::var("FLUXWRIGHT_HEADLESS")
        .ok()
        .filter(|v| v != "0" && !v.eq_ignore_ascii_case("false"))
        .is_some();

    match cli.http {
        Some(addr) => run_http(addr, cli.allow_unauthenticated, headless).await,
        None => run_stdio(headless).await,
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use super::*;
    use netguard::guard_requests;

    #[test]
    fn the_agent_opens_web_pages_only() {
        for ok in [
            "https://example.com/a?b",
            "HTTPS://Example.com",
            "http://localhost:3456/",
            "http://127.0.0.1:8080",
            "http://[::1]:3000/",
            "http://192.168.1.10/",
            "about:blank",
        ] {
            assert!(netguard::url_allowed(ok).is_ok(), "{ok}");
        }
        for bad in [
            "file:///etc/passwd",
            "FILE:///C:/Users/me/.ssh/id_ed25519",
            "chrome://settings",
            "javascript:alert(1)",
            "http://169.254.169.254/latest/meta-data/",
            "http://169.254.169.254./",
            "http://user@169.254.169.254:80/",
            "http://2852039166/",
            "http://0xa9.0xfe.0xa9.0xfe/",
            "http://0251.0376.0251.0376/",
            "http://169.254.43518/",
            "http://[fe80::1]/",
            "http://[::ffff:169.254.169.254]/",
            "http://[fd00:ec2::254]/",
            "http://metadata.google.internal/computeMetadata/v1/",
        ] {
            assert!(netguard::url_allowed(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn ipv4_hosts_read_as_browsers_do() {
        assert_eq!(
            netguard::ipv4_host("2852039166"),
            Some(Ipv4Addr::new(169, 254, 169, 254))
        );
        assert_eq!(
            netguard::ipv4_host("0x7f.1"),
            Some(Ipv4Addr::new(127, 0, 0, 1))
        );
        assert_eq!(
            netguard::ipv4_host("010.0.0.1"),
            Some(Ipv4Addr::new(8, 0, 0, 1))
        );
        assert_eq!(netguard::ipv4_host("example.com"), None);
        assert_eq!(netguard::ipv4_host("1.2.3.256"), None);
        assert_eq!(netguard::ipv4_host("1.2.3.4.5"), None);
    }

    #[test]
    fn content_is_cut_on_a_character_boundary() {
        let html = "é".repeat(30_000); // 2 bytes each: byte 40,001 is mid-character
        let cut = truncate(html, 40_001);
        assert!(cut.starts_with(&"é".repeat(20_000)));
        assert!(cut.ends_with("[truncated: 60000 bytes in all]"));
    }

    #[test]
    fn requests_are_checked_as_chrome_wrote_them() {
        for blocked in [
            "http://169.254.169.254/latest/meta-data/",
            "https://[fe80::1]/",
            "ws://169.254.169.254/",
            "file:///etc/passwd",
        ] {
            assert!(netguard::request_blocked(blocked), "{blocked}");
        }
        for allowed in [
            "https://example.com/a.js",
            "data:image/png;base64,iVBO",
            "blob:https://example.com/1-2",
            "http://127.0.0.1:3000/",
        ] {
            assert!(!netguard::request_blocked(allowed), "{allowed}");
        }
        // Browsers drop tabs inside a URL: so does the goto check.
        assert!(netguard::url_allowed("http://169.254.169.254\t/").is_err());
    }

    /// A redirect to the metadata address is stopped too: the guard sees every request with the
    /// URL Chrome parsed, not only what goto was given.
    #[tokio::test(flavor = "multi_thread")]
    async fn redirects_to_link_local_hosts_are_blocked() {
        use std::io::{Read, Write};
        let server = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://127.0.0.1:{}/", server.local_addr().unwrap().port());
        std::thread::spawn(move || {
            for mut stream in server.incoming().flatten() {
                let _ = stream.read(&mut [0u8; 2048]);
                let _ = stream.write_all(
                    b"HTTP/1.1 302 Found\r\nLocation: http://169.254.169.254/latest/meta-data/\r\n\
                      Content-Length: 0\r\nConnection: close\r\n\r\n",
                );
            }
        });
        let engine = BrowserEngine::builder()
            .max_browsers(1)
            .build()
            .await
            .unwrap();
        let page = engine.acquire().await.unwrap();
        guard_requests(&page).await.unwrap();
        let start = std::time::Instant::now();
        let err = page
            .goto(&url)
            .await
            .expect_err("the redirect was followed");
        assert!(err.to_string().contains("BLOCKED_BY_CLIENT"), "{err}");
        assert!(start.elapsed() < Duration::from_secs(10));
        drop(page);
        engine.shutdown(Duration::from_secs(5)).await.unwrap();
    }

    /// Only link-local hosts are blocked: a local fixture on 127.0.0.1 loads with the guard on.
    #[tokio::test(flavor = "multi_thread")]
    async fn guarded_pages_load_loopback_hosts() {
        use std::io::{Read, Write};
        let server = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://127.0.0.1:{}/", server.local_addr().unwrap().port());
        std::thread::spawn(move || {
            for mut stream in server.incoming().flatten() {
                let _ = stream.read(&mut [0u8; 2048]);
                let body = "<title>local</title>";
                let _ = stream.write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\n\
                         Connection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                );
            }
        });
        let engine = BrowserEngine::builder()
            .max_browsers(1)
            .build()
            .await
            .unwrap();
        let page = engine.acquire().await.unwrap();
        guard_requests(&page).await.unwrap();
        page.goto(&url).await.unwrap();
        assert_eq!(page.title().await.unwrap(), "local");
        drop(page);
        engine.shutdown(Duration::from_secs(5)).await.unwrap();
    }

    #[test]
    fn truncate_respects_utf8_char_boundary() {
        assert_eq!(truncate("ok".into(), 5), "ok");
        let out = truncate("a🦀b".into(), 2);
        assert!(out.starts_with("a…"), "{out:?}");
    }

    #[test]
    fn wait_state_and_select_entry_validate_inputs() {
        assert!(matches!(parse_wait_state(None), Ok(WaitState::Visible)));
        assert!(matches!(parse_wait_state(Some("attached")), Ok(WaitState::Attached)));
        assert!(matches!(parse_wait_state(Some("detached")), Ok(WaitState::Detached)));
        assert!(matches!(parse_wait_state(Some("hidden")), Ok(WaitState::Hidden)));
        assert!(parse_wait_state(Some("stale")).is_err());

        assert!(matches!(
            build_select_entry(Some("v".into()), None, None),
            Ok(SelectEntry::Value(v)) if v == "v"
        ));
        assert!(matches!(
            build_select_entry(None, Some("Lbl".into()), None),
            Ok(SelectEntry::Label(l)) if l == "Lbl"
        ));
        assert!(matches!(
            build_select_entry(None, None, Some(2)),
            Ok(SelectEntry::Index(2))
        ));
        assert!(build_select_entry(None, None, None).is_err());
        assert!(build_select_entry(Some("v".into()), Some("l".into()), None).is_err());
    }

    #[test]
    fn network_and_tab_helpers_bound_and_filter_cleanly() {
        let mut records = VecDeque::new();
        records.push_back(NetRecord {
            id: "1".into(),
            method: "GET".into(),
            url: "https://example.com/api/items".into(),
            resource_type: "fetch".into(),
            status: Some(200),
            failed: None,
        });
        records.push_back(NetRecord {
            id: "2".into(),
            method: "POST".into(),
            url: "https://example.com/auth".into(),
            resource_type: "xhr".into(),
            status: None,
            failed: Some("net::ERR_FAILED".into()),
        });
        records.push_back(NetRecord {
            id: "3".into(),
            method: "GET".into(),
            url: "https://example.com/app.js".into(),
            resource_type: "script".into(),
            status: Some(304),
            failed: None,
        });

        let filtered = format_net_records(&records, 10, Some("XHR"));
        assert!(filtered.contains("POST [xhr] https://example.com/auth -> FAILED: net::ERR_FAILED"));
        assert!(!filtered.contains("app.js"));

        let limited = format_net_records(&records, 1, None);
        assert!(limited.contains("app.js -> 304"));
        assert!(!limited.contains("items"));

        assert_eq!(resolve_switch_index(2, Some(1)).unwrap(), 1);
        assert!(resolve_switch_index(2, None).is_err());
        assert!(resolve_switch_index(2, Some(2)).is_err());

        assert_eq!(resolve_close_indices(3, 2, Some(0)).unwrap(), (0, 1));
        assert_eq!(resolve_close_indices(2, 1, None).unwrap(), (1, 0));
        assert_eq!(resolve_close_indices(1, 0, None).unwrap(), (0, 0));
        assert!(resolve_close_indices(1, 0, Some(1)).is_err());
    }

    #[test]
    fn optional_parameter_structs_accept_empty_json() {
        assert!(serde_json::from_str::<OpenArgs>("{}").is_ok());
        assert!(serde_json::from_str::<SnapshotArgs>("{}").is_ok());
        assert!(serde_json::from_str::<MarkdownArgs>("{}").is_ok());
        assert!(serde_json::from_str::<ScrollArgs>("{}").is_ok());
        assert!(serde_json::from_str::<ConsoleArgs>("{}").is_ok());
        assert!(serde_json::from_str::<NetworkArgs>("{}").is_ok());
        assert!(serde_json::from_str::<ScreenshotArgs>("{}").is_ok());

        let snap: SnapshotArgs = serde_json::from_str(r#"{"diff":true,"interactive_only":true}"#).unwrap();
        assert_eq!(snap.diff, Some(true));
        assert_eq!(snap.interactive_only, Some(true));

        let shot: ScreenshotArgs = serde_json::from_str(r#"{"full_page":true,"annotate":true}"#).unwrap();
        assert_eq!(shot.full_page, Some(true));
        assert_eq!(shot.annotate, Some(true));
    }

    #[test]
    fn tool_results_set_is_error_appropriately() {
        let ok = tool_ok("ready");
        assert_eq!(ok.is_error, Some(false));
        assert_eq!(ok.content.len(), 1);

        let err = tool_err("selector timed out");
        assert_eq!(err.is_error, Some(true));
        assert_eq!(err.content.len(), 1);
    }

    #[test]
    fn constant_time_token_comparison() {
        assert!(constant_time_eq(b"secret-token-123", b"secret-token-123"));
        assert!(!constant_time_eq(b"", b"secret-token-123"));
        assert!(!constant_time_eq(b"secret-token-12", b"secret-token-123"));
        assert!(!constant_time_eq(b"secret-token-1234", b"secret-token-123"));
        assert!(!constant_time_eq(b"aecret-token-123", b"secret-token-123"));
        assert!(!constant_time_eq(b"secret-xoken-123", b"secret-token-123"));
        assert!(!constant_time_eq(b"secret-token-124", b"secret-token-123"));
    }

    #[test]
    fn http_security_enforces_non_loopback_fail_closed() {
        let non_loopback: SocketAddr = "0.0.0.0:8088".parse().unwrap();
        let lan_ip: SocketAddr = "10.0.0.5:8088".parse().unwrap();
        let v6_any: SocketAddr = "[::]:8088".parse().unwrap();
        let loopback_v4: SocketAddr = "127.0.0.1:8088".parse().unwrap();
        let loopback_v6: SocketAddr = "[::1]:8088".parse().unwrap();

        assert!(validate_http_security(non_loopback, None, false).is_err());
        assert!(validate_http_security(lan_ip, Some("short"), false).is_err());
        assert!(validate_http_security(v6_any, Some(&"a".repeat(31)), false).is_err());

        assert!(validate_http_security(non_loopback, Some(&"a".repeat(32)), false)
            .unwrap()
            .is_some());
        assert!(validate_http_security(non_loopback, None, true)
            .unwrap()
            .is_none());

        assert!(validate_http_security(loopback_v4, None, false)
            .unwrap()
            .is_none());
        assert!(validate_http_security(loopback_v6, None, false)
            .unwrap()
            .is_none());
        assert!(validate_http_security(loopback_v4, Some("local-test-token"), false)
            .unwrap()
            .is_some());
        assert!(validate_http_security(loopback_v4, Some("bad token with space"), false).is_err());
    }

    fn parse_first_sse_json(raw: &[u8]) -> serde_json::Value {
        let text = std::str::from_utf8(raw).expect("valid utf-8 sse response");
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("data:") {
                let trimmed = rest.trim();
                if trimmed.is_empty() {
                    continue;
                }
                return serde_json::from_str(trimmed).expect("valid json-rpc sse payload");
            }
        }
        panic!("expected non-empty SSE data payload, got:\n{text}");
    }

    fn test_http_app(
        token: Option<&str>,
    ) -> (
        Router,
        Arc<LocalSessionManager>,
        StreamableHttpServerConfig,
    ) {
        let addr: SocketAddr = "127.0.0.1:8088".parse().unwrap();
        let cfg = build_http_server_config(addr);
        let ct = cfg.cancellation_token.clone();
        let sessions = Arc::new(LocalSessionManager::default());
        let app = build_http_router(
            Arc::new(OnceCell::new()),
            true,
            Arc::clone(&sessions),
            cfg,
            token.map(|t| ExpectedToken::new(t.to_owned())),
        );
        let mut shutdown_cfg = StreamableHttpServerConfig::default();
        shutdown_cfg.cancellation_token = ct;
        (app, sessions, shutdown_cfg)
    }

    fn init_body() -> axum::body::Body {
        axum::body::Body::from(
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-03-26",
                    "capabilities": {},
                    "clientInfo": { "name": "mcp-http-test", "version": "0.1.0" }
                }
            })
            .to_string(),
        )
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn http_auth_middleware_enforces_bearer_and_allows_healthz() {
        use axum::http::Request;
        use tower::ServiceExt as _;

        let token = "0123456789abcdef0123456789abcdef";
        let (app, sessions, shutdown_cfg) = test_http_app(Some(token));

        let health_resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/healthz")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(health_resp.status(), StatusCode::OK);
        let health_bytes = axum::body::to_bytes(health_resp.into_body(), 64 * 1024)
            .await
            .unwrap();
        let health_json: serde_json::Value = serde_json::from_slice(&health_bytes).unwrap();
        assert_eq!(
            health_json,
            serde_json::json!({"status": "ok", "transport": "streamable-http"})
        );

        let no_auth = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/mcp")
                    .header(header::HOST, "localhost")
                    .header(header::ACCEPT, "application/json, text/event-stream")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(init_body())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(no_auth.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            no_auth.headers().get(header::WWW_AUTHENTICATE).unwrap(),
            "Bearer"
        );

        let wrong_auth = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/mcp")
                    .header(header::HOST, "localhost")
                    .header(header::ACCEPT, "application/json, text/event-stream")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::AUTHORIZATION, "Bearer wrong-token-0123456789abcdef")
                    .body(init_body())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(wrong_auth.status(), StatusCode::UNAUTHORIZED);

        let basic_auth = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/mcp")
                    .header(header::HOST, "localhost")
                    .header(header::ACCEPT, "application/json, text/event-stream")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::AUTHORIZATION, format!("Basic {token}"))
                    .body(init_body())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(basic_auth.status(), StatusCode::UNAUTHORIZED);

        let dup_auth = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/mcp")
                    .header(header::HOST, "localhost")
                    .header(header::ACCEPT, "application/json, text/event-stream")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(init_body())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(dup_auth.status(), StatusCode::UNAUTHORIZED);

        let get_no_auth = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/mcp")
                    .header(header::HOST, "localhost")
                    .header(header::ACCEPT, "text/event-stream")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(get_no_auth.status(), StatusCode::UNAUTHORIZED);

        let ok_resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/mcp")
                    .header(header::HOST, "localhost")
                    .header(header::ACCEPT, "application/json, text/event-stream")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::AUTHORIZATION, format!("bearer {token}"))
                    .body(init_body())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(ok_resp.status(), StatusCode::OK);
        assert!(ok_resp.headers().contains_key("mcp-session-id"));

        shutdown_cfg.cancellation_token.cancel();
        close_all_sessions(&sessions).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn http_mcp_initialize_tools_list_and_session_teardown() {
        use axum::http::Request;
        use tower::ServiceExt as _;

        let (app, sessions, shutdown_cfg) = test_http_app(None);

        let init_resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/mcp")
                    .header(header::HOST, "localhost")
                    .header(header::ACCEPT, "application/json, text/event-stream")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(init_body())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(init_resp.status(), StatusCode::OK);
        let session_id = init_resp
            .headers()
            .get("mcp-session-id")
            .expect("mcp-session-id header")
            .to_str()
            .unwrap()
            .to_owned();
        let init_bytes = axum::body::to_bytes(init_resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let init_json = parse_first_sse_json(&init_bytes);
        assert_eq!(init_json["result"]["serverInfo"]["name"], "fluxwright-mcp");

        let notif_resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/mcp")
                    .header(header::HOST, "localhost")
                    .header(header::ACCEPT, "application/json, text/event-stream")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header("mcp-session-id", &session_id)
                    .body(axum::body::Body::from(
                        serde_json::json!({
                            "jsonrpc": "2.0",
                            "method": "notifications/initialized"
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(notif_resp.status(), StatusCode::ACCEPTED);

        let list_resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/mcp")
                    .header(header::HOST, "localhost")
                    .header(header::ACCEPT, "application/json, text/event-stream")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header("mcp-session-id", &session_id)
                    .body(axum::body::Body::from(
                        serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": 2,
                            "method": "tools/list"
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(list_resp.status(), StatusCode::OK);
        let list_bytes = axum::body::to_bytes(list_resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let list_json = parse_first_sse_json(&list_bytes);
        let tools = list_json["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 21);

        let sid: Arc<str> = Arc::from(session_id.as_str());
        assert!(sessions.has_session(&sid).await.unwrap());

        let del_resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/mcp")
                    .header(header::HOST, "localhost")
                    .header("mcp-session-id", &session_id)
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(del_resp.status(), StatusCode::ACCEPTED);
        assert!(!sessions.has_session(&sid).await.unwrap());

        let stale_resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/mcp")
                    .header(header::HOST, "localhost")
                    .header(header::ACCEPT, "application/json, text/event-stream")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header("mcp-session-id", &session_id)
                    .body(axum::body::Body::from(
                        serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": 3,
                            "method": "tools/list"
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(stale_resp.status(), StatusCode::NOT_FOUND);

        shutdown_cfg.cancellation_token.cancel();
    }

    #[test]
    fn http_sessions_isolate_state_and_share_engine_cell() {
        let engine = Arc::new(OnceCell::new());
        let a = FluxwrightMcp::with_engine(Arc::clone(&engine), true);
        let b = FluxwrightMcp::with_engine(Arc::clone(&engine), true);
        assert!(!Arc::ptr_eq(&a.state, &b.state));
        assert!(Arc::ptr_eq(&a.engine, &b.engine));

        let weak_a = Arc::downgrade(&a.state);
        drop(a);
        assert!(weak_a.upgrade().is_none());
    }
}
