//! Tier 2 external lightweight CDP fallback (`--fast-cdp-endpoint`) for `/v1/scrape` and `/v1/map`.
//!
//! Connects lazily via [`CdpBrowser::connect`] when Tier 1 HTTP fast path escalates for eligible
//! non-visual requests (`mode: "auto"` without profile, resource/URL blocking, or proxy session).
//! Any connection, capability, or execution failure falls back cleanly to local Chromium.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use fluxwright::{
    netguard, MarkdownOptions, ResourcePreset, ResourceType, WaitUntil, COOKIE_BANNER_URL_PATTERNS,
};
use fluxwright_cdp::{CdpBrowser, CdpPage, ContextId, Error as CdpError, InterceptedRequest};
use serde_json::Value;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::task::JoinHandle;
use url::{Host, Url};

use crate::http_post::ip_allowed;
use crate::job::{NavigationWait, ServerResourcePreset};
use crate::map::EXTRACT_LINKS_JS;
use crate::scrape::{ScrapeFormat, EXTRACT_TEXT_JS};
use crate::Inner;

const MAX_CONNECT_BUDGET: Duration = Duration::from_secs(5);
const MAX_EXEC_BUDGET: Duration = Duration::from_secs(15);
const CONTEXT_DISPOSE_TIMEOUT: Duration = Duration::from_secs(2);

pub(crate) struct FastCdpScrapeResult {
    pub(crate) url: String,
    pub(crate) title: String,
    pub(crate) markdown: Option<String>,
    pub(crate) html: Option<String>,
    pub(crate) text: Option<String>,
}

pub(crate) struct FastCdpMapResult {
    pub(crate) url: String,
    pub(crate) links: Vec<String>,
}

pub(crate) enum FastCdpOutcome<T> {
    Ok(T),
    Blocked(String),
    Fallback(String),
}

struct FastCdpContextGuard {
    browser: Arc<CdpBrowser>,
    ctx: Option<ContextId>,
}

impl FastCdpContextGuard {
    async fn dispose(&mut self) {
        if let Some(ctx) = self.ctx.take() {
            let _ = tokio::time::timeout(CONTEXT_DISPOSE_TIMEOUT, self.browser.dispose_context(&ctx))
                .await;
        }
    }
}

impl Drop for FastCdpContextGuard {
    fn drop(&mut self) {
        if let Some(ctx) = self.ctx.take() {
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                let browser = self.browser.clone();
                handle.spawn(async move {
                    let _ = tokio::time::timeout(
                        CONTEXT_DISPOSE_TIMEOUT,
                        browser.dispose_context(&ctx),
                    )
                    .await;
                });
            }
        }
    }
}

struct AbortOnDrop(JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn is_method_not_found(err: &CdpError) -> bool {
    matches!(
        err,
        CdpError::Command { message, .. }
            if message.contains("-32601")
                || message.contains("Method not found")
                || message.contains("wasn't found")
    )
}

fn is_connection_broken(err: &CdpError) -> bool {
    matches!(
        err,
        CdpError::BrowserDead { .. } | CdpError::WebSocket(_) | CdpError::SessionGone(_)
    )
}

fn preset_resource_types(preset: ResourcePreset) -> &'static [ResourceType] {
    match preset {
        ResourcePreset::Scraping => &[ResourceType::Image, ResourceType::Font, ResourceType::Media],
        ResourcePreset::Minimal => &[
            ResourceType::Image,
            ResourceType::Font,
            ResourceType::Media,
            ResourceType::Stylesheet,
        ],
        ResourcePreset::NoMedia => &[ResourceType::Image, ResourceType::Media],
    }
}

async fn clear_cached_if_same(inner: &Inner, failed: &Arc<CdpBrowser>) {
    let mut guard = inner.fast_cdp_browser.lock().await;
    if guard.as_ref().is_some_and(|b| Arc::ptr_eq(b, failed)) {
        *guard = None;
    }
}

async fn get_or_connect(
    inner: &Inner,
    endpoint: &str,
    remaining: Duration,
) -> Result<Arc<CdpBrowser>, String> {
    let connect_budget =
        (remaining / 2).clamp(Duration::from_millis(250).min(remaining), MAX_CONNECT_BUDGET);
    let mut guard = inner.fast_cdp_browser.lock().await;
    if let Some(ref cached) = *guard {
        if !cached.connection_dead() {
            return Ok(cached.clone());
        }
        cached.start_kill();
    }
    *guard = None;

    match tokio::time::timeout(connect_budget, CdpBrowser::connect(endpoint)).await {
        Ok(Ok(browser)) => {
            *guard = Some(browser.clone());
            Ok(browser)
        }
        Ok(Err(err)) => Err(format!("fast-CDP connect failed: {err}")),
        Err(_) => Err(format!(
            "fast-CDP connect timed out after {}ms",
            connect_budget.as_millis()
        )),
    }
}

async fn url_resolves_to_blocked(
    raw_url: &str,
    verdicts: &Arc<StdMutex<HashMap<String, bool>>>,
) -> bool {
    let Ok(parsed) = Url::parse(raw_url) else {
        return false;
    };
    if !matches!(parsed.scheme(), "http" | "https" | "ws" | "wss") {
        return false;
    }
    let Some(Host::Domain(domain)) = parsed.host() else {
        return false;
    };
    let host = domain.trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty() {
        return false;
    }
    if let Some(&blocked) = verdicts.lock().unwrap().get(&host) {
        return blocked;
    }
    if let Some(v4) = netguard::ipv4_host(&host) {
        let blocked = ip_allowed(IpAddr::V4(v4)).is_err();
        verdicts.lock().unwrap().insert(host, blocked);
        return blocked;
    }
    let port = parsed.port_or_known_default().unwrap_or(80);
    let blocked = match tokio::net::lookup_host((host.as_str(), port)).await {
        Ok(mut addrs) => addrs.any(|addr| ip_allowed(addr.ip()).is_err()),
        Err(_) => false,
    };
    verdicts.lock().unwrap().insert(host, blocked);
    blocked
}

async fn serve_fast_cdp_guarded(mut requests: UnboundedReceiver<InterceptedRequest>) {
    let verdicts: Arc<StdMutex<HashMap<String, bool>>> = Default::default();
    while let Some(req) = requests.recv().await {
        let verdicts = verdicts.clone();
        tokio::spawn(async move {
            if netguard::request_blocked(&req.url)
                || url_resolves_to_blocked(&req.url, &verdicts).await
            {
                tracing::warn!(url = %req.url, "blocked fast-CDP request to a link-local host");
                let _ = req.abort("blockedbyclient").await;
            }
        });
    }
}

async fn validate_final_url(raw_url: &str) -> Result<(), String> {
    netguard::url_allowed(raw_url).map_err(str::to_string)?;
    let parsed = Url::parse(raw_url).map_err(|e| format!("invalid final URL: {e}"))?;
    match parsed.host() {
        Some(Host::Ipv4(v4)) => ip_allowed(IpAddr::V4(v4)).map_err(str::to_string)?,
        Some(Host::Ipv6(v6)) => ip_allowed(IpAddr::V6(v6)).map_err(str::to_string)?,
        Some(Host::Domain(domain)) => {
            if let Some(v4) = netguard::ipv4_host(domain) {
                ip_allowed(IpAddr::V4(v4)).map_err(str::to_string)?;
            } else {
                let port = parsed.port_or_known_default().unwrap_or(80);
                if let Ok(addrs) = tokio::net::lookup_host((domain, port)).await {
                    for addr in addrs {
                        ip_allowed(addr.ip()).map_err(str::to_string)?;
                    }
                }
            }
        }
        None => return Err("final URL is missing a host".into()),
    }
    Ok(())
}

struct PreparedPage {
    ctx_guard: FastCdpContextGuard,
    _guard_task: Option<AbortOnDrop>,
    page: CdpPage,
    final_url: String,
    step_budget: Duration,
}

enum PrepareAttempt {
    Ready(PreparedPage),
    Blocked(String),
}

async fn prepare_and_navigate(
    browser: &Arc<CdpBrowser>,
    url: &str,
    wait_until: WaitUntil,
    resource_preset: Option<ServerResourcePreset>,
    block_cookie_banners: bool,
    allow_any_url: bool,
    exec_budget: Duration,
) -> Result<PrepareAttempt, CdpError> {
    let step_budget = exec_budget;
    let ctx = browser.create_context(None).await?;
    let mut ctx_guard = FastCdpContextGuard {
        browser: browser.clone(),
        ctx: Some(ctx.clone()),
    };

    if let Some(preset) = resource_preset {
        let _ = browser
            .context_block_resource_types(
                &ctx,
                preset_resource_types(preset.into_core()),
                step_budget,
            )
            .await;
    }
    if block_cookie_banners {
        let patterns: Vec<String> = COOKIE_BANNER_URL_PATTERNS
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        let _ = browser
            .context_set_blocked_urls(&ctx, &patterns, step_budget)
            .await;
    }

    let page = browser.create_page(&ctx, step_budget).await?;

    let guard_task = if !allow_any_url {
        match page.intercept(step_budget).await {
            Ok(rx) => Some(AbortOnDrop(tokio::spawn(serve_fast_cdp_guarded(rx)))),
            Err(err) if is_method_not_found(&err) => {
                tracing::debug!(
                    error = %err,
                    "fast-CDP endpoint does not implement Fetch.enable; relying on URL SSRF checks"
                );
                None
            }
            Err(err) => {
                ctx_guard.dispose().await;
                return Err(err);
            }
        }
    } else {
        None
    };

    if let Err(err) = page.goto(url, wait_until, step_budget).await {
        drop(guard_task);
        ctx_guard.dispose().await;
        return Err(err);
    }

    let final_url = match page.evaluate("location.href", step_budget).await {
        Ok(Value::String(u)) if !u.is_empty() => u,
        _ => {
            let fallback = page.url();
            if fallback.is_empty() {
                url.to_string()
            } else {
                fallback
            }
        }
    };

    if !allow_any_url {
        if let Err(msg) = validate_final_url(&final_url).await {
            drop(guard_task);
            ctx_guard.dispose().await;
            return Ok(PrepareAttempt::Blocked(msg));
        }
    }

    Ok(PrepareAttempt::Ready(PreparedPage {
        ctx_guard,
        _guard_task: guard_task,
        page,
        final_url,
        step_budget,
    }))
}

fn compute_exec_budget(total_timeout_ms: u64, started: Instant) -> Option<Duration> {
    let remaining = Duration::from_millis(total_timeout_ms).saturating_sub(started.elapsed());
    if remaining.is_zero() {
        return None;
    }
    Some(
        (remaining * 2 / 3)
            .clamp(Duration::from_millis(500).min(remaining), MAX_EXEC_BUDGET)
            .min(remaining),
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn scrape(
    inner: &Inner,
    url: &str,
    wait_until: NavigationWait,
    formats: &[ScrapeFormat],
    resource_preset: Option<ServerResourcePreset>,
    block_cookie_banners: bool,
    started: Instant,
    total_timeout_ms: u64,
) -> FastCdpOutcome<FastCdpScrapeResult> {
    let Some(endpoint) = inner.fast_cdp_endpoint.as_deref() else {
        return FastCdpOutcome::Fallback("fast-CDP endpoint is not configured".into());
    };
    if formats.iter().any(|f| {
        matches!(
            f,
            ScrapeFormat::Snapshot | ScrapeFormat::Screenshot | ScrapeFormat::Pdf
        )
    }) {
        return FastCdpOutcome::Fallback("requested format requires local Chromium".into());
    }
    let remaining = Duration::from_millis(total_timeout_ms).saturating_sub(started.elapsed());
    if remaining.is_zero() {
        return FastCdpOutcome::Fallback("scrape request budget exhausted".into());
    }

    let browser = match get_or_connect(inner, endpoint, remaining).await {
        Ok(b) => b,
        Err(reason) => return FastCdpOutcome::Fallback(reason),
    };
    let Some(exec_budget) = compute_exec_budget(total_timeout_ms, started) else {
        return FastCdpOutcome::Fallback("scrape request budget exhausted".into());
    };

    let allow_any_url = inner.allow_any_url();
    let wait = wait_until.into_core();
    let browser_for_run = browser.clone();
    let work = async {
        let prepared = match prepare_and_navigate(
            &browser_for_run,
            url,
            wait,
            resource_preset,
            block_cookie_banners,
            allow_any_url,
            exec_budget,
        )
        .await?
        {
            PrepareAttempt::Blocked(msg) => return Ok(FastCdpOutcome::Blocked(msg)),
            PrepareAttempt::Ready(p) => p,
        };

        let PreparedPage {
            mut ctx_guard,
            _guard_task,
            page,
            final_url,
            step_budget,
        } = prepared;

        let res = async {
            let title = page.title(step_budget).await?;
            let mut markdown = None;
            let mut html = None;
            let mut text = None;
            for &fmt in formats {
                match fmt {
                    ScrapeFormat::Markdown => {
                        let md = page
                            .markdown(
                                &MarkdownOptions {
                                    selector: None,
                                    main_content_only: false,
                                    include_links: true,
                                    include_images: false,
                                },
                                step_budget,
                            )
                            .await?;
                        markdown = Some(md);
                    }
                    ScrapeFormat::Html => {
                        html = Some(page.content(step_budget).await?);
                    }
                    ScrapeFormat::Text => {
                        let val = page.evaluate(EXTRACT_TEXT_JS, step_budget).await?;
                        text = Some(val.as_str().unwrap_or_default().to_string());
                    }
                    ScrapeFormat::Snapshot | ScrapeFormat::Screenshot | ScrapeFormat::Pdf => {}
                }
            }
            Ok::<_, CdpError>(FastCdpScrapeResult {
                url: final_url,
                title,
                markdown,
                html,
                text,
            })
        }
        .await;

        drop(_guard_task);
        ctx_guard.dispose().await;
        res.map(FastCdpOutcome::Ok)
    };

    match tokio::time::timeout(exec_budget, work).await {
        Ok(Ok(outcome)) => outcome,
        Ok(Err(err)) => {
            if browser.connection_dead() || is_connection_broken(&err) {
                browser.start_kill();
                clear_cached_if_same(inner, &browser).await;
            }
            FastCdpOutcome::Fallback(format!("fast-CDP scrape failed: {err}"))
        }
        Err(_) => FastCdpOutcome::Fallback(format!(
            "fast-CDP scrape timed out after {}ms",
            exec_budget.as_millis()
        )),
    }
}

pub(crate) async fn map(
    inner: &Inner,
    url: &str,
    wait_until: NavigationWait,
    resource_preset: Option<ServerResourcePreset>,
    block_cookie_banners: bool,
    started: Instant,
    total_timeout_ms: u64,
) -> FastCdpOutcome<FastCdpMapResult> {
    let Some(endpoint) = inner.fast_cdp_endpoint.as_deref() else {
        return FastCdpOutcome::Fallback("fast-CDP endpoint is not configured".into());
    };
    let remaining = Duration::from_millis(total_timeout_ms).saturating_sub(started.elapsed());
    if remaining.is_zero() {
        return FastCdpOutcome::Fallback("map request budget exhausted".into());
    }

    let browser = match get_or_connect(inner, endpoint, remaining).await {
        Ok(b) => b,
        Err(reason) => return FastCdpOutcome::Fallback(reason),
    };
    let Some(exec_budget) = compute_exec_budget(total_timeout_ms, started) else {
        return FastCdpOutcome::Fallback("map request budget exhausted".into());
    };

    let allow_any_url = inner.allow_any_url();
    let wait = wait_until.into_core();
    let browser_for_run = browser.clone();
    let work = async {
        let prepared = match prepare_and_navigate(
            &browser_for_run,
            url,
            wait,
            resource_preset,
            block_cookie_banners,
            allow_any_url,
            exec_budget,
        )
        .await?
        {
            PrepareAttempt::Blocked(msg) => return Ok(FastCdpOutcome::Blocked(msg)),
            PrepareAttempt::Ready(p) => p,
        };

        let PreparedPage {
            mut ctx_guard,
            _guard_task,
            page,
            final_url,
            step_budget,
        } = prepared;

        let res = async {
            let val = page.evaluate(EXTRACT_LINKS_JS, step_budget).await?;
            let arr = val.as_array().ok_or_else(|| {
                CdpError::Other("fast-CDP link extraction returned non-array".into())
            })?;
            let links = arr
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect();
            Ok::<_, CdpError>(FastCdpMapResult {
                url: final_url,
                links,
            })
        }
        .await;

        drop(_guard_task);
        ctx_guard.dispose().await;
        res.map(FastCdpOutcome::Ok)
    };

    match tokio::time::timeout(exec_budget, work).await {
        Ok(Ok(outcome)) => outcome,
        Ok(Err(err)) => {
            if browser.connection_dead() || is_connection_broken(&err) {
                browser.start_kill();
                clear_cached_if_same(inner, &browser).await;
            }
            FastCdpOutcome::Fallback(format!("fast-CDP map failed: {err}"))
        }
        Err(_) => FastCdpOutcome::Fallback(format!(
            "fast-CDP map timed out after {}ms",
            exec_budget.as_millis()
        )),
    }
}
