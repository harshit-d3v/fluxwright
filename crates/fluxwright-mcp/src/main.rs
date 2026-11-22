//! Fluxwright MCP server (stdio). Claude Desktop, Codex, and Cursor spawn this
//! process and call tools to drive a Chromium fleet.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use base64::Engine as _;
use fluxwright::{
    netguard, with_timeout, BrowserEngine, ContextLease, HoverOptions, MarkdownOptions, NetEvent,
    PageLease, ScreenshotOptions, SelectEntry, SnapshotOptions, WaitState, WaitUntil,
};
use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig},
    schemars, tool, tool_handler, tool_router,
    transport::stdio,
    ErrorData as McpError, ServerHandler, ServiceExt,
};
use tokio::sync::{MappedMutexGuard, Mutex, MutexGuard};
use tracing_subscriber::EnvFilter;

const MAX_NET_RECORDS: usize = 500;

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
}

impl Drop for Tab {
    fn drop(&mut self) {
        self.net_task.abort();
    }
}

struct State {
    tabs: Vec<Tab>,
    context: Option<ContextLease>,
    engine: Option<BrowserEngine>,
    active: usize,
    next_tab_id: usize,
}

#[derive(Clone)]
struct FluxwrightMcp {
    state: Arc<Mutex<State>>,
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
}

impl FluxwrightMcp {
    fn new(headless: bool) -> Self {
        Self {
            state: Arc::new(Mutex::new(State {
                tabs: Vec::new(),
                context: None,
                engine: None,
                active: 0,
                next_tab_id: 1,
            })),
            headless,
            tool_router: Self::tool_router(),
        }
    }

    async fn ensure_context(st: &mut State, headless: bool) -> Result<(), McpError> {
        if st.engine.is_none() {
            let engine = BrowserEngine::builder()
                .max_browsers(2)
                .max_contexts_per_browser(4)
                .headless(headless)
                .acquire_timeout(Duration::from_secs(60))
                .build()
                .await
                .map_err(internal)?;
            st.engine = Some(engine);
        }
        let ctx_closed = st.context.as_ref().is_some_and(ContextLease::is_closed);
        if st.context.is_none() || ctx_closed {
            let ctx = st
                .engine
                .as_ref()
                .expect("made above")
                .acquire_context()
                .await
                .map_err(internal)?;
            st.context = Some(ctx);
        }
        Ok(())
    }

    async fn attach_tab(page: PageLease, id: usize) -> Result<Tab, McpError> {
        if !any_url() {
            netguard::guard_requests(&page).await.map_err(internal)?;
        }
        let mut rx = page.subscribe_network();
        let net_buf = Arc::new(std::sync::Mutex::new(VecDeque::with_capacity(64)));
        let buf = net_buf.clone();
        let net_task = tokio::spawn(async move {
            while let Some(ev) = rx.recv().await {
                record_net_event(&buf, ev);
            }
        });
        Ok(Tab {
            id,
            page,
            net_buf,
            net_task,
        })
    }

    /// Replaces the current context with a fresh single-tab context and returns a guard to that tab.
    async fn open_fresh(&self) -> Result<MappedMutexGuard<'_, Tab>, McpError> {
        let mut st = self.state.lock().await;
        st.tabs.clear();
        st.context = None;
        st.active = 0;
        Self::ensure_context(&mut st, self.headless).await?;
        let page = st
            .context
            .as_ref()
            .expect("made above")
            .new_page()
            .await
            .map_err(internal)?;
        let id = st.next_tab_id;
        st.next_tab_id += 1;
        let tab = Self::attach_tab(page, id).await?;
        st.tabs.push(tab);
        st.active = 0;
        Ok(MutexGuard::map(st, |s| &mut s.tabs[0]))
    }

    /// Returns a lock-held guard to the active tab, creating an engine/context/tab if empty.
    async fn active_tab(&self) -> Result<MappedMutexGuard<'_, Tab>, McpError> {
        let mut st = self.state.lock().await;
        st.tabs.retain(|t| !t.page.is_closed());
        if st.tabs.is_empty() {
            Self::ensure_context(&mut st, self.headless).await?;
            let page = st
                .context
                .as_ref()
                .expect("made above")
                .new_page()
                .await
                .map_err(internal)?;
            let id = st.next_tab_id;
            st.next_tab_id += 1;
            let tab = Self::attach_tab(page, id).await?;
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

fn internal(e: impl std::fmt::Display) -> McpError {
    McpError::internal_error(e.to_string(), None)
}

/// FLUXWRIGHT_MCP_ANY_URL lifts the URL rules below.
fn any_url() -> bool {
    std::env::var_os("FLUXWRIGHT_MCP_ANY_URL").is_some()
}

fn check_url(url: &str) -> Result<(), McpError> {
    if any_url() {
        return Ok(());
    }
    netguard::url_allowed(url).map_err(|why| {
        McpError::invalid_params(
            format!("{why}: {url} (FLUXWRIGHT_MCP_ANY_URL=1 allows any URL)"),
            None,
        )
    })
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
    ) -> Result<String, McpError> {
        if let Some(url) = &url {
            check_url(url)?;
        }
        let tab = self.open_fresh().await?;
        if let Some(url) = url {
            tab.page.goto(&url).await.map_err(internal)?;
            let title = tab.page.title().await.unwrap_or_default();
            return Ok(format!("opened {url}\ntitle: {title}"));
        }
        Ok("Chromium page ready (about:blank)".into())
    }

    #[tool(description = "Navigate the active tab to a URL.")]
    async fn goto(
        &self,
        Parameters(UrlArgs { url }): Parameters<UrlArgs>,
    ) -> Result<String, McpError> {
        check_url(&url)?;
        let tab = self.active_tab().await?;
        tab.page.goto(&url).await.map_err(internal)?;
        let title = tab.page.title().await.unwrap_or_default();
        Ok(format!("navigated to {url}\ntitle: {title}"))
    }

    #[tool(description = "Return the active tab's title.")]
    async fn title(&self) -> Result<String, McpError> {
        self.active_tab().await?.page.title().await.map_err(internal)
    }

    #[tool(description = "Return the active tab's raw HTML (truncated). Prefer snapshot() or markdown() first.")]
    async fn content(&self) -> Result<String, McpError> {
        let html = self
            .active_tab()
            .await?
            .page
            .content()
            .await
            .map_err(internal)?;
        Ok(truncate(html, 40_000))
    }

    #[tool(description = "Capture a compact ARIA accessibility tree with short @eN element refs for click/fill/hover/select.")]
    async fn snapshot(
        &self,
        Parameters(args): Parameters<SnapshotArgs>,
    ) -> Result<String, McpError> {
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
            ..Default::default()
        };
        if let Some(limit) = args.max_nodes {
            opts.max_nodes = limit;
        }
        let tab = self.active_tab().await?;
        let snap = tab.page.aria_snapshot_with(&opts).await.map_err(internal)?;
        let url = tab.page.url();
        let title = tab.page.title().await.unwrap_or_default();
        let mut out = format!(
            "url: {url}\ntitle: {title}\nrefs: {}\n\n{}",
            snap.refs.len(),
            snap.tree
        );
        if snap.truncated {
            out.push_str("\n\n[snapshot truncated: narrow with max_depth or interactive_only=true]");
        }
        Ok(truncate(out, 40_000))
    }

    #[tool(description = "Extract visible page content (or a matching selector/main landmark) as clean Markdown.")]
    async fn markdown(
        &self,
        Parameters(args): Parameters<MarkdownArgs>,
    ) -> Result<String, McpError> {
        let opts = MarkdownOptions {
            selector: args.selector,
            main_content_only: args.main_content_only.unwrap_or(false),
            include_links: args.include_links.unwrap_or(true),
            include_images: args.include_images.unwrap_or(false),
        };
        let md = self
            .active_tab()
            .await?
            .page
            .markdown_with(&opts)
            .await
            .map_err(internal)?;
        Ok(truncate(md, 40_000))
    }

    #[tool(description = "Click an element by @eN snapshot ref, CSS, text=, or role= selector.")]
    async fn click(
        &self,
        Parameters(SelectorArgs { selector }): Parameters<SelectorArgs>,
    ) -> Result<String, McpError> {
        self.active_tab()
            .await?
            .page
            .click(&selector)
            .await
            .map_err(internal)?;
        Ok(format!("clicked {selector}"))
    }

    #[tool(description = "Hover over an element by @eN snapshot ref, CSS, text=, or role= selector.")]
    async fn hover(
        &self,
        Parameters(SelectorArgs { selector }): Parameters<SelectorArgs>,
    ) -> Result<String, McpError> {
        self.active_tab()
            .await?
            .page
            .hover(&selector, &HoverOptions::default())
            .await
            .map_err(internal)?;
        Ok(format!("hovered {selector}"))
    }

    #[tool(description = "Fill an input/textarea by @eN snapshot ref, CSS, text=, or role= selector.")]
    async fn fill(
        &self,
        Parameters(FillArgs { selector, value }): Parameters<FillArgs>,
    ) -> Result<String, McpError> {
        self.active_tab()
            .await?
            .page
            .fill(&selector, &value)
            .await
            .map_err(internal)?;
        Ok(format!("filled {selector}"))
    }

    #[tool(description = "Press a key (e.g. Enter, Tab, Escape), optionally after focusing selector.")]
    async fn press(
        &self,
        Parameters(PressArgs { key, selector }): Parameters<PressArgs>,
    ) -> Result<String, McpError> {
        let tab = self.active_tab().await?;
        if let Some(sel) = &selector {
            tab.page.press(sel, &key, None).await.map_err(internal)?;
            Ok(format!("pressed {key} on {sel}"))
        } else {
            tab.page
                .keyboard_press(&key, None)
                .await
                .map_err(internal)?;
            Ok(format!("pressed {key}"))
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
    ) -> Result<String, McpError> {
        let entry = build_select_entry(value, label, index)?;
        let selected = self
            .active_tab()
            .await?
            .page
            .select_option(&selector, &[entry], false)
            .await
            .map_err(internal)?;
        Ok(format!("selected {selected:?} on {selector}"))
    }

    #[tool(description = "Scroll an element into view (with selector) or scroll the viewport by wheel deltas.")]
    async fn scroll(
        &self,
        Parameters(ScrollArgs {
            selector,
            delta_x,
            delta_y,
        }): Parameters<ScrollArgs>,
    ) -> Result<String, McpError> {
        let tab = self.active_tab().await?;
        if let Some(sel) = &selector {
            tab.page
                .scroll_into_view_if_needed(sel)
                .await
                .map_err(internal)?;
            Ok(format!("scrolled {sel} into view"))
        } else {
            let dx = delta_x.unwrap_or(0.0);
            let dy = delta_y.unwrap_or(500.0);
            tab.page.mouse_wheel(dx, dy).await.map_err(internal)?;
            Ok(format!("scrolled by ({dx}, {dy})"))
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
    ) -> Result<String, McpError> {
        if selector.is_none() && url.is_none() {
            return Err(McpError::invalid_params(
                "wait_for requires selector or url",
                None,
            ));
        }
        let wait_state = parse_wait_state(state.as_deref())?;
        let timeout = timeout_ms.map(|ms| Duration::from_millis(ms.clamp(1, 120_000)));
        let tab = self.active_tab().await?;
        with_timeout(timeout, async {
            if let Some(sel) = &selector {
                tab.page.wait_for_selector_state(sel, wait_state).await?;
            }
            if let Some(pattern) = &url {
                tab.page.wait_for_url(pattern, WaitUntil::Load).await?;
            }
            Ok::<_, fluxwright::Error>(())
        })
        .await
        .map_err(internal)?;
        Ok("wait condition satisfied".into())
    }

    #[tool(description = "Navigate back in session history on the active tab.")]
    async fn back(&self) -> Result<String, McpError> {
        let tab = self.active_tab().await?;
        let moved = tab.page.go_back().await.map_err(internal)?;
        if moved {
            Ok(format!("went back to {}", tab.page.url()))
        } else {
            Ok("no previous history entry".into())
        }
    }

    #[tool(description = "Navigate forward in session history on the active tab.")]
    async fn forward(&self) -> Result<String, McpError> {
        let tab = self.active_tab().await?;
        let moved = tab.page.go_forward().await.map_err(internal)?;
        if moved {
            Ok(format!("went forward to {}", tab.page.url()))
        } else {
            Ok("no forward history entry".into())
        }
    }

    #[tool(description = "Manage tabs in the current browser context: action is list, new, switch, or close.")]
    async fn tabs(
        &self,
        Parameters(TabsArgs { action, index, url }): Parameters<TabsArgs>,
    ) -> Result<String, McpError> {
        let mut st = self.state.lock().await;
        st.tabs.retain(|t| !t.page.is_closed());
        if st.active >= st.tabs.len() && !st.tabs.is_empty() {
            st.active = st.tabs.len() - 1;
        }
        match action.as_str() {
            "list" => {
                if st.tabs.is_empty() {
                    return Ok("no open tabs".into());
                }
                let mut lines = Vec::with_capacity(st.tabs.len());
                for (i, tab) in st.tabs.iter().enumerate() {
                    let marker = if i == st.active { "*" } else { " " };
                    let title = tab.page.title().await.unwrap_or_default();
                    lines.push(format!("[{i}]{marker} #{} {} — {title}", tab.id, tab.page.url()));
                }
                Ok(lines.join("\n"))
            }
            "new" => {
                if let Some(u) = &url {
                    check_url(u)?;
                }
                Self::ensure_context(&mut st, self.headless).await?;
                let page = st
                    .context
                    .as_ref()
                    .expect("made above")
                    .new_page()
                    .await
                    .map_err(internal)?;
                let id = st.next_tab_id;
                st.next_tab_id += 1;
                let tab = Self::attach_tab(page, id).await?;
                if let Some(u) = &url {
                    tab.page.goto(u).await.map_err(internal)?;
                }
                st.tabs.push(tab);
                st.active = st.tabs.len() - 1;
                let _ = st.tabs[st.active].page.bring_to_front().await;
                let cur = &st.tabs[st.active];
                Ok(format!("opened tab {} #{}: {}", st.active, cur.id, cur.page.url()))
            }
            "switch" => {
                let idx = resolve_switch_index(st.tabs.len(), index)?;
                st.active = idx;
                st.tabs[idx].page.bring_to_front().await.map_err(internal)?;
                Ok(format!(
                    "switched to tab {idx} #{}: {}",
                    st.tabs[idx].id,
                    st.tabs[idx].page.url()
                ))
            }
            "close" => {
                let (remove_idx, new_active) = resolve_close_indices(st.tabs.len(), st.active, index)?;
                let closed = st.tabs.remove(remove_idx);
                if st.tabs.is_empty() {
                    st.context = None;
                    st.active = 0;
                    Ok(format!("closed tab {remove_idx} #{} (context now empty)", closed.id))
                } else {
                    st.active = new_active;
                    Ok(format!(
                        "closed tab {remove_idx} #{}; active tab is {new_active}",
                        closed.id
                    ))
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
    ) -> Result<String, McpError> {
        let tab = self.active_tab().await?;
        let out = format_console(
            &tab.page.console_messages(),
            &tab.page.page_errors(),
            limit.unwrap_or(50),
        );
        Ok(truncate(out, 20_000))
    }

    #[tool(description = "Read recent network requests and response statuses/failures captured on the active tab.")]
    async fn network(
        &self,
        Parameters(NetworkArgs { limit, filter }): Parameters<NetworkArgs>,
    ) -> Result<String, McpError> {
        let tab = self.active_tab().await?;
        let out = {
            let guard = tab.net_buf.lock().unwrap();
            format_net_records(&guard, limit.unwrap_or(50), filter.as_deref())
        };
        Ok(truncate(out, 20_000))
    }

    #[tool(description = "Evaluate a JavaScript expression in the active tab and return JSON.")]
    async fn evaluate(
        &self,
        Parameters(EvalArgs { expression }): Parameters<EvalArgs>,
    ) -> Result<String, McpError> {
        let v = self
            .active_tab()
            .await?
            .page
            .evaluate(&expression)
            .await
            .map_err(internal)?;
        Ok(v.to_string())
    }

    #[tool(description = "Take a PNG screenshot of the active tab (viewport by default, or full_page=true).")]
    async fn screenshot(
        &self,
        Parameters(ScreenshotArgs { full_page }): Parameters<ScreenshotArgs>,
    ) -> Result<CallToolResult, McpError> {
        let opts = ScreenshotOptions {
            full_page: full_page.unwrap_or(false),
            ..Default::default()
        };
        let png = self
            .active_tab()
            .await?
            .page
            .screenshot_with(&opts)
            .await
            .map_err(internal)?;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
        Ok(CallToolResult::success(vec![ContentBlock::image(
            b64,
            "image/png",
        )]))
    }

    #[tool(description = "Close all tabs and destroy the current browser context.")]
    async fn close(&self) -> Result<String, McpError> {
        let mut st = self.state.lock().await;
        st.tabs.clear();
        st.context = None;
        st.active = 0;
        Ok("page closed".into())
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

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("fluxwright=info".parse()?))
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let headless = std::env::var("FLUXWRIGHT_HEADLESS")
        .ok()
        .filter(|v| v != "0" && !v.eq_ignore_ascii_case("false"))
        .is_some();

    tracing::info!(headless, "fluxwright MCP on stdio");
    let service = FluxwrightMcp::new(headless)
        .serve(stdio())
        .await
        .inspect_err(|e| tracing::error!("serve: {e}"))?;
    service.waiting().await?;
    Ok(())
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
    }
}
