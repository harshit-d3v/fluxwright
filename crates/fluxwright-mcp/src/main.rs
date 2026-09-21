//! Fluxwright MCP server (stdio). Claude Desktop, Codex, and Cursor spawn this
//! process and call tools to drive a Chromium fleet.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use base64::Engine as _;
use fluxwright::{BrowserEngine, PageLease};
use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, Content, ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler, ServiceExt,
    transport::stdio,
};
use tokio::sync::Mutex;
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
    #[schemars(description = "CSS selector")]
    selector: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct FillArgs {
    #[schemars(description = "CSS selector")]
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

    async fn ensure_page(&self) -> Result<(), McpError> {
        let mut st = self.state.lock().await;
        if st.page.is_some() {
            return Ok(());
        }
        if st.engine.is_none() {
            let engine = BrowserEngine::builder()
                .max_browsers(2)
                .max_contexts_per_browser(4)
                .headless(self.headless)
                .acquire_timeout(Duration::from_secs(60))
                .build()
                .await
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            st.engine = Some(engine);
        }
        let engine = st.engine.as_ref().unwrap();
        let page = engine
            .acquire()
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        st.page = Some(page);
        Ok(())
    }
}

#[tool_router]
impl FluxwrightMcp {
    #[tool(description = "Open a Chromium page (fresh context). Pass url to navigate immediately.")]
    async fn open(
        &self,
        Parameters(OpenArgs { url }): Parameters<OpenArgs>,
    ) -> Result<String, McpError> {
        {
            let mut st = self.state.lock().await;
            st.page = None;
        }
        self.ensure_page().await?;
        if let Some(url) = url {
            let st = self.state.lock().await;
            st.page
                .as_ref()
                .unwrap()
                .goto(&url)
                .await
                .map_err(|e| McpError::internal_error(e.to_string(), None))?;
            let title = st.page.as_ref().unwrap().title().await.unwrap_or_default();
            return Ok(format!("opened {url}\ntitle: {title}"));
        }
        Ok("Chromium page ready (about:blank)".into())
    }

    #[tool(description = "Navigate the current page to a URL.")]
    async fn goto(&self, Parameters(UrlArgs { url }): Parameters<UrlArgs>) -> Result<String, McpError> {
        self.ensure_page().await?;
        let st = self.state.lock().await;
        st.page
            .as_ref()
            .unwrap()
            .goto(&url)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let title = st.page.as_ref().unwrap().title().await.unwrap_or_default();
        Ok(format!("navigated to {url}\ntitle: {title}"))
    }

    #[tool(description = "Return the current page title.")]
    async fn title(&self) -> Result<String, McpError> {
        self.ensure_page().await?;
        let st = self.state.lock().await;
        st.page
            .as_ref()
            .unwrap()
            .title()
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))
    }

    #[tool(description = "Return the current page HTML (truncated).")]
    async fn content(&self) -> Result<String, McpError> {
        self.ensure_page().await?;
        let st = self.state.lock().await;
        let html = st
            .page
            .as_ref()
            .unwrap()
            .content()
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        const MAX: usize = 40_000;
        if html.len() > MAX {
            Ok(format!("{}…\n[truncated {} chars]", &html[..MAX], html.len()))
        } else {
            Ok(html)
        }
    }

    #[tool(description = "Click an element by CSS selector.")]
    async fn click(
        &self,
        Parameters(SelectorArgs { selector }): Parameters<SelectorArgs>,
    ) -> Result<String, McpError> {
        self.ensure_page().await?;
        let st = self.state.lock().await;
        st.page
            .as_ref()
            .unwrap()
            .click(&selector)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(format!("clicked {selector}"))
    }

    #[tool(description = "Fill an input/textarea by CSS selector.")]
    async fn fill(
        &self,
        Parameters(FillArgs { selector, value }): Parameters<FillArgs>,
    ) -> Result<String, McpError> {
        self.ensure_page().await?;
        let st = self.state.lock().await;
        st.page
            .as_ref()
            .unwrap()
            .fill(&selector, &value)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(format!("filled {selector}"))
    }

    #[tool(description = "Evaluate a JavaScript expression in the page and return JSON.")]
    async fn evaluate(
        &self,
        Parameters(EvalArgs { expression }): Parameters<EvalArgs>,
    ) -> Result<String, McpError> {
        self.ensure_page().await?;
        let st = self.state.lock().await;
        let v = st
            .page
            .as_ref()
            .unwrap()
            .evaluate(&expression)
            .await
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        Ok(v.to_string())
    }

    #[tool(description = "Take a PNG screenshot of the current page.")]
    async fn screenshot(&self) -> Result<CallToolResult, McpError> {
        self.ensure_page().await?;
        let png = {
            let st = self.state.lock().await;
            st.page
                .as_ref()
                .unwrap()
                .screenshot()
                .await
                .map_err(|e| McpError::internal_error(e.to_string(), None))?
        };
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
        Ok(CallToolResult::success(vec![Content::image(b64, "image/png")]))
    }

    #[tool(description = "Close the current page (destroys the browser context).")]
    async fn close(&self) -> Result<String, McpError> {
        let mut st = self.state.lock().await;
        st.page = None;
        Ok("page closed".into())
    }
}

#[tool_handler]
impl ServerHandler for FluxwrightMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some(
                "Fluxwright Chromium fleet. Tools: open, goto, title, content, click, fill, evaluate, screenshot, close. \
                 Each open() is a fresh browser context. Use CSS selectors. Prefer evaluate() to extract prices/text."
                    .into(),
            ),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            ..Default::default()
        }
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
