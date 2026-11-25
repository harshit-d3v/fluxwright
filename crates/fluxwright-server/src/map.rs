//! Synchronous site link discovery endpoint (`POST /v1/map`).

use std::collections::{HashMap, HashSet};
use std::net::{Ipv4Addr, Ipv6Addr};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use axum::extract::rejection::JsonRejection;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::job::{JobPriority, JobRequest, NavigationWait, ServerResourcePreset, Step};
use crate::{execute_job, map_sync_job_error, ApiError, ExecutionProgress, Inner, JobTraceContext};

const DEFAULT_MAP_LIMIT: usize = 100;
const MAX_MAP_LIMIT: usize = 1000;
const DEFAULT_MAP_TIMEOUT_MS: u64 = 30_000;
const MAX_MAP_TIMEOUT_MS: u64 = 120_000;
const MAX_SEARCH_BYTES: usize = 512;

const EXTRACT_LINKS_JS: &str = r#"(() => {
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
struct UrlOrigin {
    scheme: String,
    host: String,
    port: u16,
    is_ip: bool,
}

impl UrlOrigin {
    fn parse(raw: &str) -> Option<Self> {
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

    fn matches(&self, base: &Self, include_subdomains: bool) -> bool {
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
    let base_origin = UrlOrigin::parse(base_url);
    let search_lower = search
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_ascii_lowercase);

    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for val in raw_links {
        let Some(link) = val.as_str() else {
            continue;
        };
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
    let job_req = JobRequest {
        steps: vec![
            Step::Goto {
                url: req.url,
                wait_until: req.wait_until,
            },
            Step::Url {},
            Step::Evaluate {
                expression: EXTRACT_LINKS_JS.to_string(),
            },
        ],
        timeout_ms: req.timeout_ms,
        retries: 0,
        priority: JobPriority::Normal,
        resource_preset: req.resource_preset,
        block_resources: Vec::new(),
        block_url_patterns: Vec::new(),
        profile: req.profile,
        persist_profile: false,
        proxy_session: req.proxy_session,
        webhook_url: None,
        webhook_headers: HashMap::new(),
    };
    job_req
        .validate_with(inner.allow_any_url())
        .map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, "invalid_request", e))?;

    let traceparent = headers.get("traceparent").and_then(|v| v.to_str().ok());
    let trace_ctx = JobTraceContext::from_headers(traceparent);
    let progress = Arc::new(Mutex::new(ExecutionProgress::default()));

    let started = Instant::now();
    let results = execute_job(
        engine,
        job_req,
        Some(Arc::downgrade(&inner)),
        Uuid::new_v4(),
        trace_ctx,
        0,
        progress,
    )
    .await
    .map_err(|err| map_sync_job_error(err, "map_failed"))?;
    let duration_ms = started.elapsed().as_millis() as u64;

    let final_url = results
        .get(1)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or(&req_url)
        .to_string();
    let raw_links = results
        .get(2)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);

    let links = filter_links(
        raw_links,
        &final_url,
        req.same_origin,
        req.include_subdomains,
        req.search.as_deref(),
        req.limit,
    );

    Ok((
        StatusCode::OK,
        Json(json!({
            "url": final_url,
            "count": links.len(),
            "links": links,
            "duration_ms": duration_ms,
        })),
    )
        .into_response())
}
