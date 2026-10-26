//! Fluxwright MCP server (stdio). Claude Desktop, Codex, and Cursor spawn this
//! process and call tools to drive a Chromium fleet.

use std::net::{Ipv4Addr, Ipv6Addr};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use base64::Engine as _;
use fluxwright::{BrowserEngine, PageLease};
use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig},
    schemars, tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler, ServiceExt,
    transport::stdio,
};
use tokio::sync::{MappedMutexGuard, Mutex, MutexGuard};
use tracing_subscriber::EnvFilter;

struct State {
    engine: Option<BrowserEngine>,
    page: Option<PageLease>,
}

#[derive(Clone)]
struct FluxwrightMcp {
    state: Arc<Mutex<State>>,
    headless: bool,
    tool_router: ToolRouter<Self>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct UrlArgs {
    #[schemars(description = "Absolute URL, e.g. https://www.amazon.in")]
    url: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct OpenArgs {
    #[schemars(description = "Optional URL to open immediately")]
    url: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct SelectorArgs {
    #[schemars(description = "CSS selector, text=Visible text, or role=button[name=\"Save\"]")]
    selector: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct FillArgs {
    #[schemars(description = "CSS selector, text=Visible text, or role=textbox[name=\"Email\"]")]
    selector: String,
    #[schemars(description = "Text to type")]
    value: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct EvalArgs {
    #[schemars(description = "JavaScript expression, e.g. document.title")]
    expression: String,
}

impl FluxwrightMcp {
    fn new(headless: bool) -> Self {
        Self {
            state: Arc::new(Mutex::new(State {
                engine: None,
                page: None,
            })),
            headless,
            tool_router: Self::tool_router(),
        }
    }

    /// The current page, made (with the engine) if there is none; `fresh` replaces it. The lock
    /// stays held while the caller uses the page, so a parallel `close` cannot pull it away
    /// mid-call (the tools used to unlock, lock again and unwrap).
    async fn page(&self, fresh: bool) -> Result<MappedMutexGuard<'_, PageLease>, McpError> {
        let mut st = self.state.lock().await;
        if fresh {
            st.page = None;
        }
        if st.page.is_none() {
            if st.engine.is_none() {
                let engine = BrowserEngine::builder()
                    .max_browsers(2)
                    .max_contexts_per_browser(4)
                    .headless(self.headless)
                    .acquire_timeout(Duration::from_secs(60))
                    .build()
                    .await
                    .map_err(internal)?;
                st.engine = Some(engine);
            }
            let page = st.engine.as_ref().expect("made above").acquire().await.map_err(internal)?;
            if !any_url() {
                guard_requests(&page).await.map_err(internal)?;
            }
            st.page = Some(page);
        }
        Ok(MutexGuard::map(st, |s| s.page.as_mut().expect("made above")))
    }
}

fn internal(e: impl std::fmt::Display) -> McpError {
    McpError::internal_error(e.to_string(), None)
}

/// FLUXWRIGHT_MCP_ANY_URL lifts the URL rules below.
fn any_url() -> bool {
    std::env::var_os("FLUXWRIGHT_MCP_ANY_URL").is_some()
}

/// Every request the page makes (subresources, redirects, navigations a script or click starts)
/// is checked against the host rules, on the URL as Chrome parsed it: nothing the agent types
/// can slip past through a parser difference, a redirect or `evaluate`. `check_url` on
/// `open`/`goto` adds the scheme rule (Chrome itself refuses pages' own navigations to `file:`).
/// shortcut: a hostname that resolves to a link-local address is not caught; the README says
/// never to expose this server without authentication.
async fn guard_requests(page: &PageLease) -> fluxwright::Result<()> {
    let mut requests = page.intercept().await?;
    tokio::spawn(async move {
        while let Some(req) = requests.recv().await {
            if request_blocked(&req.url) {
                tracing::warn!(url = %req.url, "blocked a request to a link-local host");
                let _ = req.abort("blockedbyclient").await;
            }
            // Any other request is dropped here, which lets it through.
        }
    });
    Ok(())
}

/// Web pages only. A page the agent reads can talk it into anything: `file://` would hand it
/// local files, and a link-local host the cloud metadata service (instance credentials).
fn check_url(url: &str) -> Result<(), McpError> {
    if any_url() {
        return Ok(());
    }
    url_allowed(url).map_err(|why| {
        McpError::invalid_params(format!("{why}: {url} (FLUXWRIGHT_MCP_ANY_URL=1 allows any URL)"), None)
    })
}

fn url_allowed(url: &str) -> Result<(), &'static str> {
    // Browsers drop tabs and newlines anywhere in a URL.
    let url: String = url.trim().chars().filter(|c| !matches!(c, '\t' | '\n' | '\r')).collect();
    if url.eq_ignore_ascii_case("about:blank") {
        return Ok(());
    }
    let (scheme, rest) = url.split_once("://").ok_or("only http and https URLs are allowed")?;
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        return Err("only http and https URLs are allowed");
    }
    if host_blocked(rest) {
        return Err("link-local hosts (cloud metadata) are blocked");
    }
    Ok(())
}

/// For a request URL as Chrome wrote it: web requests to a blocked host, and `file:`.
fn request_blocked(url: &str) -> bool {
    match url.split_once("://") {
        Some((scheme, rest)) if matches!(scheme, "http" | "https" | "ws" | "wss") => host_blocked(rest),
        _ => url.starts_with("file:"),
    }
}

/// Whether the host of `after_scheme` (the URL after `://`) is link-local or a cloud metadata
/// name.
fn host_blocked(after_scheme: &str) -> bool {
    // Browsers read `\` as `/` in web URLs.
    let authority = after_scheme.split(['/', '?', '#', '\\']).next().unwrap_or("");
    let host = authority.rsplit('@').next().unwrap_or("").to_ascii_lowercase();
    let host = match host.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or(""),
        None => host.split(':').next().unwrap_or(""),
    }
    .trim_end_matches('.');
    if host == "metadata.google.internal" || host == "metadata" {
        return true;
    }
    match host.parse::<Ipv6Addr>() {
        Ok(v6) => {
            v6.segments()[0] & 0xffc0 == 0xfe80
                || v6 == Ipv6Addr::new(0xfd00, 0xec2, 0, 0, 0, 0, 0, 0x254) // AWS IMDS over IPv6
                || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_link_local())
        }
        Err(_) => ipv4_host(host).is_some_and(|v4| v4.is_link_local()),
    }
}

/// A host read as IPv4 the way browsers do: 1 to 4 dot-separated numbers in decimal, hex (0x)
/// or octal (leading 0), the last one filling the remaining bytes. So `2852039166` and
/// `0xa9.0xfe.0xa9.0xfe` are both 169.254.169.254. None for a name.
fn ipv4_host(host: &str) -> Option<Ipv4Addr> {
    let parts: Vec<&str> = host.split('.').collect();
    if parts.len() > 4 {
        return None;
    }
    let mut nums = Vec::with_capacity(parts.len());
    for p in parts {
        let n = if let Some(hex) = p.strip_prefix("0x") {
            if hex.is_empty() { 0 } else { u64::from_str_radix(hex, 16).ok()? }
        } else if p.len() > 1 && p.starts_with('0') {
            u64::from_str_radix(&p[1..], 8).ok()?
        } else {
            p.parse::<u64>().ok()?
        };
        nums.push(n);
    }
    let (&last, head) = nums.split_last()?;
    if head.iter().any(|&n| n > 255) || last >= 256u64.pow(5 - nums.len() as u32) {
        return None;
    }
    let value = head.iter().enumerate().fold(last, |v, (i, &n)| v + (n << (8 * (3 - i))));
    Some(Ipv4Addr::from(value as u32))
}

/// `text` cut to at most `max` bytes on a character boundary (a byte cut panicked inside a
/// multi-byte character).
fn truncate(text: String, max: usize) -> String {
    if text.len() <= max {
        return text;
    }
    let mut cut = max;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…\n[truncated: {} bytes in all]", &text[..cut], text.len())
}

#[tool_router]
impl FluxwrightMcp {
    #[tool(description = "Open a Chromium page (fresh context). Pass url to navigate immediately.")]
    async fn open(
        &self,
        Parameters(OpenArgs { url }): Parameters<OpenArgs>,
    ) -> Result<String, McpError> {
        if let Some(url) = &url {
            check_url(url)?;
        }
        let page = self.page(true).await?;
        if let Some(url) = url {
            page.goto(&url).await.map_err(internal)?;
            let title = page.title().await.unwrap_or_default();
            return Ok(format!("opened {url}\ntitle: {title}"));
        }
        Ok("Chromium page ready (about:blank)".into())
    }

    #[tool(description = "Navigate the current page to a URL.")]
    async fn goto(&self, Parameters(UrlArgs { url }): Parameters<UrlArgs>) -> Result<String, McpError> {
        check_url(&url)?;
        let page = self.page(false).await?;
        page.goto(&url).await.map_err(internal)?;
        let title = page.title().await.unwrap_or_default();
        Ok(format!("navigated to {url}\ntitle: {title}"))
    }

    #[tool(description = "Return the current page title.")]
    async fn title(&self) -> Result<String, McpError> {
        self.page(false).await?.title().await.map_err(internal)
    }

    #[tool(description = "Return the current page HTML (truncated).")]
    async fn content(&self) -> Result<String, McpError> {
        let html = self.page(false).await?.content().await.map_err(internal)?;
        Ok(truncate(html, 40_000))
    }

    #[tool(description = "Click an element by CSS, text= or role= selector.")]
    async fn click(
        &self,
        Parameters(SelectorArgs { selector }): Parameters<SelectorArgs>,
    ) -> Result<String, McpError> {
        self.page(false).await?.click(&selector).await.map_err(internal)?;
        Ok(format!("clicked {selector}"))
    }

    #[tool(description = "Fill an input/textarea by CSS, text= or role= selector.")]
    async fn fill(
        &self,
        Parameters(FillArgs { selector, value }): Parameters<FillArgs>,
    ) -> Result<String, McpError> {
        self.page(false).await?.fill(&selector, &value).await.map_err(internal)?;
        Ok(format!("filled {selector}"))
    }

    #[tool(description = "Evaluate a JavaScript expression in the page and return JSON.")]
    async fn evaluate(
        &self,
        Parameters(EvalArgs { expression }): Parameters<EvalArgs>,
    ) -> Result<String, McpError> {
        let v = self.page(false).await?.evaluate(&expression).await.map_err(internal)?;
        Ok(v.to_string())
    }

    #[tool(description = "Take a PNG screenshot of the current page.")]
    async fn screenshot(&self) -> Result<CallToolResult, McpError> {
        let png = self.page(false).await?.screenshot().await.map_err(internal)?;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
        Ok(CallToolResult::success(vec![ContentBlock::image(b64, "image/png")]))
    }

    #[tool(description = "Close the current page (destroys the browser context).")]
    async fn close(&self) -> Result<String, McpError> {
        let mut st = self.state.lock().await;
        st.page = None;
        Ok("page closed".into())
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for FluxwrightMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("fluxwright-mcp", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Fluxwright Chromium fleet. Tools: open, goto, title, content, click, fill, evaluate, screenshot, close. \
                 Each open() is a fresh browser context. Selectors: CSS, text=Sign in, or role=button[name=\"Sign in\"]. \
                 Prefer evaluate() to extract prices/text.",
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
    use super::*;

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
            assert!(url_allowed(ok).is_ok(), "{ok}");
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
            assert!(url_allowed(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn ipv4_hosts_read_as_browsers_do() {
        assert_eq!(ipv4_host("2852039166"), Some(Ipv4Addr::new(169, 254, 169, 254)));
        assert_eq!(ipv4_host("0x7f.1"), Some(Ipv4Addr::new(127, 0, 0, 1)));
        assert_eq!(ipv4_host("010.0.0.1"), Some(Ipv4Addr::new(8, 0, 0, 1)));
        assert_eq!(ipv4_host("example.com"), None);
        assert_eq!(ipv4_host("1.2.3.256"), None);
        assert_eq!(ipv4_host("1.2.3.4.5"), None);
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
            assert!(request_blocked(blocked), "{blocked}");
        }
        for allowed in ["https://example.com/a.js", "data:image/png;base64,iVBO", "blob:https://example.com/1-2", "http://127.0.0.1:3000/"] {
            assert!(!request_blocked(allowed), "{allowed}");
        }
        // Browsers drop tabs inside a URL: so does the goto check.
        assert!(url_allowed("http://169.254.169.254\t/").is_err());
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
        let engine = BrowserEngine::builder().max_browsers(1).build().await.unwrap();
        let page = engine.acquire().await.unwrap();
        guard_requests(&page).await.unwrap();
        let start = std::time::Instant::now();
        let err = page.goto(&url).await.expect_err("the redirect was followed");
        assert!(err.to_string().contains("BLOCKED_BY_CLIENT"), "{err}");
        assert!(start.elapsed() < Duration::from_secs(10));
        drop(page);
        engine.shutdown(Duration::from_secs(5)).await.unwrap();
    }
}
