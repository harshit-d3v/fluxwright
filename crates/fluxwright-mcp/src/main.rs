//! Fluxwright MCP server (stdio). Claude Desktop, Codex, and Cursor spawn this
//! process and call tools to drive a Chromium fleet.

use std::net::{Ipv4Addr, Ipv6Addr};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use base64::Engine as _;
use fluxwright::{netguard, BrowserEngine, PageLease};
use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig},
    schemars, tool, tool_handler, tool_router,
    transport::stdio,
    ErrorData as McpError, ServerHandler, ServiceExt,
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
            let page = st
                .engine
                .as_ref()
                .expect("made above")
                .acquire()
                .await
                .map_err(internal)?;
            if !any_url() {
                netguard::guard_requests(&page).await.map_err(internal)?;
            }
            st.page = Some(page);
        }
        Ok(MutexGuard::map(st, |s| {
            s.page.as_mut().expect("made above")
        }))
    }
}

fn internal(e: impl std::fmt::Display) -> McpError {
    McpError::internal_error(e.to_string(), None)
}

/// FLUXWRIGHT_MCP_ANY_URL lifts the URL rules below.
fn any_url() -> bool {
    std::env::var_os("FLUXWRIGHT_MCP_ANY_URL").is_some()
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
    async fn goto(
        &self,
        Parameters(UrlArgs { url }): Parameters<UrlArgs>,
    ) -> Result<String, McpError> {
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
        self.page(false)
            .await?
            .click(&selector)
            .await
            .map_err(internal)?;
        Ok(format!("clicked {selector}"))
    }

    #[tool(description = "Fill an input/textarea by CSS, text= or role= selector.")]
    async fn fill(
        &self,
        Parameters(FillArgs { selector, value }): Parameters<FillArgs>,
    ) -> Result<String, McpError> {
        self.page(false)
            .await?
            .fill(&selector, &value)
            .await
            .map_err(internal)?;
        Ok(format!("filled {selector}"))
    }

    #[tool(description = "Evaluate a JavaScript expression in the page and return JSON.")]
    async fn evaluate(
        &self,
        Parameters(EvalArgs { expression }): Parameters<EvalArgs>,
    ) -> Result<String, McpError> {
        let v = self
            .page(false)
            .await?
            .evaluate(&expression)
            .await
            .map_err(internal)?;
        Ok(v.to_string())
    }

    #[tool(description = "Take a PNG screenshot of the current page.")]
    async fn screenshot(&self) -> Result<CallToolResult, McpError> {
        let png = self
            .page(false)
            .await?
            .screenshot()
            .await
            .map_err(internal)?;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
        Ok(CallToolResult::success(vec![ContentBlock::image(
            b64,
            "image/png",
        )]))
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
}
