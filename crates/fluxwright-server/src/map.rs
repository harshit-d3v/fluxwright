//! Synchronous site link discovery endpoint (`POST /v1/map`).

use std::collections::{HashMap, HashSet};
use std::net::{Ipv4Addr, Ipv6Addr};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::rejection::JsonRejection;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use fluxwright::ProxyPool;
use serde::Deserialize;
use serde_json::{json, Value};
use url::Url;
use uuid::Uuid;

use crate::fast_cdp::{self, FastCdpOutcome};
use crate::fast_fetch::{self, FastFetchError, FetchMode};
use crate::fast_html;
use crate::job::{JobPriority, JobRequest, NavigationWait, ServerResourcePreset, Step};
use crate::{execute_job, map_sync_job_error, ApiError, ExecutionProgress, Inner, JobTraceContext};

const DEFAULT_MAP_LIMIT: usize = 100;
const MAX_MAP_LIMIT: usize = 1000;
const DEFAULT_MAP_TIMEOUT_MS: u64 = 30_000;
const MAX_MAP_TIMEOUT_MS: u64 = 120_000;
const MAX_SEARCH_BYTES: usize = 512;
const MAX_SUB_SITEMAPS: usize = 5;

pub(crate) const EXTRACT_LINKS_JS: &str = r#"(() => {
  const seen = new Set();
  const out = [];
  for (const a of document.querySelectorAll('a[href]')) {
    try {
      const u = new URL(a.href, location.href);
      if (u.protocol !== 'http:' && u.protocol !== 'https:') continue;
      const s = u.toString();
      if (!seen.has(s)) {
        seen.add(s);
        out.push(s);
        if (out.length >= 5000) break;
      }
    } catch (_) {}
  }
  return out;
})()"#;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MapRequest {
    url: String,
    #[serde(default = "default_true")]
    same_origin: bool,
    #[serde(default)]
    include_subdomains: bool,
    #[serde(default)]
    pub(crate) include_sitemap: bool,
    #[serde(default)]
    pub(crate) mode: FetchMode,
    #[serde(default)]
    search: Option<String>,
    #[serde(default = "default_limit")]
    limit: usize,
    #[serde(default)]
    wait_until: NavigationWait,
    #[serde(default = "default_timeout_ms")]
    timeout_ms: u64,
    #[serde(default)]
    resource_preset: Option<ServerResourcePreset>,
    #[serde(default)]
    block_cookie_banners: bool,
    #[serde(default)]
    profile: Option<String>,
    #[serde(default)]
    proxy_session: Option<String>,
}

fn default_true() -> bool {
    true
}

fn default_limit() -> usize {
    DEFAULT_MAP_LIMIT
}

fn default_timeout_ms() -> u64 {
    DEFAULT_MAP_TIMEOUT_MS
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UrlOrigin {
    scheme: String,
    host: String,
    port: u16,
    is_ip: bool,
}

impl UrlOrigin {
    pub(crate) fn parse(raw: &str) -> Option<Self> {
        let trimmed = raw.trim();
        let (scheme_raw, rest) = trimmed.split_once("://")?;
        let (scheme, default_port) = if scheme_raw.eq_ignore_ascii_case("http") {
            ("http".to_string(), 80u16)
        } else if scheme_raw.eq_ignore_ascii_case("https") {
            ("https".to_string(), 443u16)
        } else {
            return None;
        };

        let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
        let host_port = authority.rsplit('@').next().unwrap_or("");
        if host_port.is_empty() {
            return None;
        }

        let (host, port, is_ip) = if let Some(after_bracket) = host_port.strip_prefix('[') {
            let (ip_str, tail) = after_bracket.split_once(']')?;
            ip_str.parse::<Ipv6Addr>().ok()?;
            let port = if tail.is_empty() {
                default_port
            } else {
                tail.strip_prefix(':')?
                    .parse::<u16>()
                    .ok()
                    .filter(|&p| p != 0)?
            };
            (ip_str.to_ascii_lowercase(), port, true)
        } else if let Some((h, p)) = host_port.rsplit_once(':') {
            if h.is_empty() || h.contains(':') {
                return None;
            }
            let port = p.parse::<u16>().ok().filter(|&n| n != 0)?;
            let host = h.trim_end_matches('.').to_ascii_lowercase();
            let is_ip = host.parse::<Ipv4Addr>().is_ok();
            (host, port, is_ip)
        } else {
            let host = host_port.trim_end_matches('.').to_ascii_lowercase();
            let is_ip = host.parse::<Ipv4Addr>().is_ok();
            (host, default_port, is_ip)
        };

        if host.is_empty() {
            return None;
        }
        Some(Self {
            scheme,
            host,
            port,
            is_ip,
        })
    }

    pub(crate) fn matches(&self, base: &Self, include_subdomains: bool) -> bool {
        if self.scheme != base.scheme || self.port != base.port {
            return false;
        }
        if self.host == base.host {
            return true;
        }
        include_subdomains
            && !base.is_ip
            && self
                .host
                .strip_suffix(&base.host)
                .is_some_and(|prefix| prefix.ends_with('.'))
    }
}

pub(crate) fn filter_links(
    raw_links: &[Value],
    base_url: &str,
    same_origin: bool,
    include_subdomains: bool,
    search: Option<&str>,
    limit: usize,
) -> Vec<String> {
    filter_link_strs(
        raw_links.iter().filter_map(Value::as_str),
        base_url,
        same_origin,
        include_subdomains,
        search,
        limit,
    )
}

fn filter_link_strs<'a, I>(
    raw_links: I,
    base_url: &str,
    same_origin: bool,
    include_subdomains: bool,
    search: Option<&str>,
    limit: usize,
) -> Vec<String>
where
    I: IntoIterator<Item = &'a str>,
{
    let base_origin = UrlOrigin::parse(base_url);
    let search_lower = search
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_ascii_lowercase);

    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for link in raw_links {
        let Some(origin) = UrlOrigin::parse(link) else {
            continue;
        };
        if same_origin {
            let Some(ref base) = base_origin else {
                continue;
            };
            if !origin.matches(base, include_subdomains) {
                continue;
            }
        }
        if let Some(ref needle) = search_lower {
            if !link.to_ascii_lowercase().contains(needle) {
                continue;
            }
        }
        if seen.insert(link.to_string()) {
            out.push(link.to_string());
            if out.len() >= limit {
                break;
            }
        }
    }
    out
}

pub(crate) async fn map_handler(
    State(inner): State<Arc<Inner>>,
    headers: HeaderMap,
    payload: Result<Json<MapRequest>, JsonRejection>,
) -> Result<Response, ApiError> {
    let Some(engine) = inner.engine.clone() else {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            "/v1/map is unavailable in coordinator mode",
        ));
    };
    if !inner.with_registry(|r, _| r.accepting) {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "shutting_down",
            "the server is shutting down",
        ));
    }

    let Json(req) =
        payload.map_err(|e| ApiError::new(e.status(), "invalid_request", e.body_text()))?;

    if !(1..=MAX_MAP_LIMIT).contains(&req.limit) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            format!("limit must be between 1 and {MAX_MAP_LIMIT}"),
        ));
    }
    if !(1..=MAX_MAP_TIMEOUT_MS).contains(&req.timeout_ms) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            format!("timeout_ms must be between 1 and {MAX_MAP_TIMEOUT_MS}"),
        ));
    }
    if let Some(ref s) = req.search {
        if s.len() > MAX_SEARCH_BYTES || s.chars().any(char::is_control) {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                format!("search must be at most {MAX_SEARCH_BYTES} bytes without control characters"),
            ));
        }
    }

    let req_url = req.url.clone();
    let mode = req.mode;
    let wait_until = req.wait_until;
    let block_cookie_banners = req.block_cookie_banners;
    let include_sitemap = req.include_sitemap;
    let total_timeout_ms = req.timeout_ms;
    let proxy_session = req.proxy_session.clone();
    let browser_resource_preset = req.resource_preset.or(Some(ServerResourcePreset::Scraping));
    let has_browser_only_opts = req.profile.is_some();
    let is_about_blank = req_url.trim().eq_ignore_ascii_case("about:blank");

    let mut job_req = JobRequest {
        steps: vec![
            Step::Goto {
                url: req.url,
                wait_until,
            },
            Step::Url {},
            Step::Evaluate {
                expression: EXTRACT_LINKS_JS.to_string(),
            },
        ],
        timeout_ms: total_timeout_ms,
        retries: 0,
        priority: JobPriority::Normal,
        resource_preset: browser_resource_preset,
        block_resources: Vec::new(),
        block_url_patterns: Vec::new(),
        block_cookie_banners: req.block_cookie_banners,
        profile: req.profile,
        persist_profile: false,
        proxy_session: req.proxy_session,
        webhook_url: None,
        webhook_headers: HashMap::new(),
        affinity_key: None,
    };
    job_req
        .validate_with(inner.allow_any_url())
        .map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, "invalid_request", e))?;

    if mode == FetchMode::Http && (has_browser_only_opts || is_about_blank) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "mode 'http' cannot be used with about:blank or profile",
        ));
    }

    let started = Instant::now();
    let mut engine_label = "browser";
    let mut fallback_reason: Option<String> = None;
    let mut final_url = req_url.clone();
    let mut discovered_links: Option<Vec<String>> = None;

    let try_fastpath = mode != FetchMode::Browser && !has_browser_only_opts && !is_about_blank;
    if try_fastpath {
        let fetch_res = fast_fetch::fetch_url(
            &req_url,
            Duration::from_millis(total_timeout_ms),
            inner.allow_any_url(),
            engine.proxy_pool(),
            proxy_session.as_deref(),
        )
        .await;

        match fetch_res {
            Err(FastFetchError::InvalidOrBlocked(msg)) => {
                return Err(ApiError::new(StatusCode::BAD_REQUEST, "invalid_request", msg));
            }
            Err(FastFetchError::Transport(msg)) => {
                if mode == FetchMode::Http {
                    return Err(ApiError::new(StatusCode::BAD_GATEWAY, "map_failed", msg));
                }
                inner.fastpath_fallbacks.fetch_add(1, Ordering::Relaxed);
                fallback_reason = Some(format!("fetch error: {msg}"));
            }
            Ok(resp) => {
                let html_str =
                    fast_html::decode_body_text(&resp.body, resp.content_type.as_deref());
                if let Some(reason) = fast_html::detect_escalation(
                    resp.status,
                    &resp.headers,
                    resp.content_type.as_deref(),
                    &html_str,
                ) {
                    if mode == FetchMode::Http {
                        return Err(ApiError::new(StatusCode::BAD_GATEWAY, "map_failed", reason));
                    }
                    inner.fastpath_fallbacks.fetch_add(1, Ordering::Relaxed);
                    fallback_reason = Some(reason);
                } else {
                    inner.fastpath_hits.fetch_add(1, Ordering::Relaxed);
                    engine_label = "http";
                    final_url = resp.final_url;
                    discovered_links = Some(fast_html::extract_links(&html_str, &final_url));
                }
            }
        }
    }

    if discovered_links.is_none()
        && mode == FetchMode::Auto
        && fallback_reason.is_some()
        && inner.fast_cdp_endpoint.is_some()
        && !has_browser_only_opts
        && !is_about_blank
        && proxy_session.is_none()
        && engine.proxy_pool().is_none()
    {
        let elapsed_ms = started.elapsed().as_millis() as u64;
        if elapsed_ms >= total_timeout_ms {
            return Err(ApiError::new(
                StatusCode::GATEWAY_TIMEOUT,
                "timeout",
                "map request timed out",
            ));
        }
        match fast_cdp::map(
            &inner,
            &req_url,
            wait_until,
            browser_resource_preset,
            block_cookie_banners,
            started,
            total_timeout_ms,
        )
        .await
        {
            FastCdpOutcome::Ok(fast) => {
                final_url = fast.url;
                discovered_links = Some(fast.links);
            }
            FastCdpOutcome::Blocked(msg) => {
                return Err(ApiError::new(StatusCode::BAD_REQUEST, "invalid_request", msg));
            }
            FastCdpOutcome::Fallback(reason) => {
                tracing::warn!(
                    url = %req_url,
                    %reason,
                    "fast-CDP map unavailable; falling back to local Chromium"
                );
            }
        }
    }

    let mut raw_links = match discovered_links {
        Some(links) => links,
        None => {
            let elapsed_ms = started.elapsed().as_millis() as u64;
            if elapsed_ms >= total_timeout_ms {
                return Err(ApiError::new(
                    StatusCode::GATEWAY_TIMEOUT,
                    "timeout",
                    "map request timed out",
                ));
            }
            job_req.timeout_ms = total_timeout_ms.saturating_sub(elapsed_ms).max(1);

            let traceparent = headers.get("traceparent").and_then(|v| v.to_str().ok());
            let trace_ctx = JobTraceContext::from_headers(traceparent);
            let progress = Arc::new(Mutex::new(ExecutionProgress::default()));

            let results = execute_job(
                engine.clone(),
                job_req,
                Some(Arc::downgrade(&inner)),
                Uuid::new_v4(),
                trace_ctx,
                0,
                progress,
            )
            .await
            .map_err(|err| map_sync_job_error(err, "map_failed"))?;

            final_url = results
                .get(1)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .unwrap_or(&req_url)
                .to_string();
            results
                .get(2)
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default()
        }
    };

    if include_sitemap {
        let elapsed_ms = started.elapsed().as_millis() as u64;
        if let Some(remaining_ms) = total_timeout_ms.checked_sub(elapsed_ms).filter(|&ms| ms > 0) {
            let sitemap_links = fetch_sitemap_urls(
                &final_url,
                Duration::from_millis(remaining_ms),
                inner.allow_any_url(),
                engine.proxy_pool(),
                proxy_session.as_deref(),
            )
            .await;
            raw_links.extend(sitemap_links);
        }
    }

    let links = filter_link_strs(
        raw_links.iter().map(String::as_str),
        &final_url,
        req.same_origin,
        req.include_subdomains,
        req.search.as_deref(),
        req.limit,
    );
    let duration_ms = started.elapsed().as_millis() as u64;

    let mut obj = serde_json::Map::new();
    obj.insert("url".into(), Value::String(final_url));
    obj.insert("count".into(), json!(links.len()));
    obj.insert("links".into(), json!(links));
    obj.insert("duration_ms".into(), json!(duration_ms));
    obj.insert("engine".into(), Value::String(engine_label.into()));
    if let Some(reason) = fallback_reason {
        obj.insert("fallback_reason".into(), Value::String(reason));
    }

    Ok((StatusCode::OK, Json(Value::Object(obj))).into_response())
}

fn origin_sitemap_url(page_url: &str) -> Option<String> {
    let parsed = Url::parse(page_url).ok()?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return None;
    }
    let origin = parsed.origin().ascii_serialization();
    if origin == "null" {
        return None;
    }
    Some(format!("{origin}/sitemap.xml"))
}

async fn fetch_sitemap_urls(
    page_url: &str,
    timeout: Duration,
    allow_any_url: bool,
    proxy_pool: Option<&ProxyPool>,
    proxy_session: Option<&str>,
) -> Vec<String> {
    let Some(sitemap_url) = origin_sitemap_url(page_url) else {
        return Vec::new();
    };
    let started = Instant::now();
    let Ok(resp) = fast_fetch::fetch_url(
        &sitemap_url,
        timeout,
        allow_any_url,
        proxy_pool,
        proxy_session,
    )
    .await
    else {
        return Vec::new();
    };
    if !(200..300).contains(&resp.status) {
        return Vec::new();
    }
    let xml = fast_html::decode_body_text(&resp.body, resp.content_type.as_deref());
    let locs = fast_html::parse_sitemap_locs(&xml);
    if !xml.to_ascii_lowercase().contains("<sitemapindex") {
        return locs;
    }

    let mut nested = Vec::new();
    for child_url in locs.iter().take(MAX_SUB_SITEMAPS) {
        let Some(remaining) = timeout.checked_sub(started.elapsed()) else {
            break;
        };
        if remaining.is_zero() {
            break;
        }
        if let Ok(child_resp) = fast_fetch::fetch_url(
            child_url,
            remaining,
            allow_any_url,
            proxy_pool,
            proxy_session,
        )
        .await
        {
            if (200..300).contains(&child_resp.status) {
                let child_xml = fast_html::decode_body_text(
                    &child_resp.body,
                    child_resp.content_type.as_deref(),
                );
                nested.extend(fast_html::parse_sitemap_locs(&child_xml));
            }
        }
    }
    if nested.is_empty() {
        locs
    } else {
        nested
    }
}
