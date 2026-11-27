//! Asynchronous recursive site crawler (`POST /v1/crawl`, `GET /v1/crawl/{id}`, `DELETE /v1/crawl/{id}`).

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio::task::{AbortHandle, JoinSet};
use url::Url;
use uuid::Uuid;

use crate::fast_fetch::{self, FastFetchError, FetchMode};
use crate::fast_html::{self, ExtractField};
use crate::job::{JobPriority, JobRequest, NavigationWait, ServerResourcePreset, Step};
use crate::map::{UrlOrigin, EXTRACT_LINKS_JS};
use crate::scrape::{ScrapeFormat, EXTRACT_TEXT_JS};
use crate::{execute_job, now_ms, ApiError, ExecutionProgress, Inner, JobTraceContext};

const MAX_CRAWLS: usize = 64;
const DEFAULT_MAX_DEPTH: usize = 2;
const MAX_CRAWL_DEPTH: usize = 5;
const DEFAULT_MAX_PAGES: usize = 25;
const MAX_CRAWL_PAGES: usize = 250;
const DEFAULT_CONCURRENCY: usize = 4;
const MAX_CONCURRENCY: usize = 16;
const DEFAULT_TIMEOUT_MS: u64 = 30_000;
const MAX_TIMEOUT_MS: u64 = 120_000;
const MAX_PATH_PATTERNS: usize = 32;
const MAX_PATH_PATTERN_BYTES: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum CrawlStatus {
    Queued,
    Running,
    Completed,
    Failed,
    Cancelled,
}

impl CrawlStatus {
    pub(crate) fn terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CrawlRequest {
    url: String,
    #[serde(default = "default_max_depth")]
    max_depth: usize,
    #[serde(default = "default_max_pages")]
    max_pages: usize,
    #[serde(default = "default_concurrency")]
    concurrency: usize,
    #[serde(default = "default_true")]
    same_origin: bool,
    #[serde(default)]
    include_subdomains: bool,
    #[serde(default)]
    include_paths: Vec<String>,
    #[serde(default)]
    exclude_paths: Vec<String>,
    #[serde(default = "default_crawl_formats")]
    formats: Vec<ScrapeFormat>,
    #[serde(default)]
    mode: FetchMode,
    #[serde(default)]
    extract: Option<BTreeMap<String, ExtractField>>,
    #[serde(default = "default_timeout_ms")]
    timeout_ms: u64,
    #[serde(default)]
    wait_until: NavigationWait,
    #[serde(default)]
    resource_preset: Option<ServerResourcePreset>,
    #[serde(default)]
    block_cookie_banners: bool,
    #[serde(default)]
    profile: Option<String>,
    #[serde(default)]
    proxy_session: Option<String>,
}

fn default_max_depth() -> usize {
    DEFAULT_MAX_DEPTH
}

fn default_max_pages() -> usize {
    DEFAULT_MAX_PAGES
}

fn default_concurrency() -> usize {
    DEFAULT_CONCURRENCY
}

fn default_true() -> bool {
    true
}

fn default_crawl_formats() -> Vec<ScrapeFormat> {
    vec![ScrapeFormat::Markdown]
}

fn default_timeout_ms() -> u64 {
    DEFAULT_TIMEOUT_MS
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct CrawlPageResult {
    pub(crate) url: String,
    pub(crate) depth: usize,
    pub(crate) status: String,
    pub(crate) engine: String,
    pub(crate) title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) markdown: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) html: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) extracted: Option<Map<String, Value>>,
    pub(crate) duration_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct CrawlSnapshot {
    pub(crate) id: String,
    pub(crate) status: CrawlStatus,
    pub(crate) seed_url: String,
    pub(crate) created_at: u64,
    pub(crate) started_at: Option<u64>,
    pub(crate) finished_at: Option<u64>,
    pub(crate) discovered: usize,
    pub(crate) completed: usize,
    pub(crate) failed: usize,
    pub(crate) pages: Vec<CrawlPageResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) error: Option<String>,
}

struct CrawlRecord {
    snapshot: CrawlSnapshot,
    abort: Option<AbortHandle>,
}

#[derive(Default)]
pub(crate) struct CrawlRegistry {
    records: Mutex<VecDeque<CrawlRecord>>,
}

impl CrawlRegistry {
    pub(crate) fn shutdown(&self) {
        let mut reg = self.records.lock().unwrap_or_else(|e| e.into_inner());
        let ended_at = now_ms();
        for rec in reg.iter_mut() {
            if !rec.snapshot.status.terminal() {
                rec.snapshot.status = CrawlStatus::Cancelled;
                rec.snapshot.finished_at = Some(ended_at);
                rec.snapshot.error = Some("server shutting down".into());
                if let Some(abort) = rec.abort.take() {
                    abort.abort();
                }
            }
        }
    }

    fn admit(&self, snapshot: CrawlSnapshot) -> Result<(), ApiError> {
        let mut reg = self.records.lock().unwrap_or_else(|e| e.into_inner());
        while reg.len() >= MAX_CRAWLS {
            if let Some(idx) = reg.iter().position(|r| r.snapshot.status.terminal()) {
                reg.remove(idx);
            } else {
                return Err(ApiError::new(
                    StatusCode::TOO_MANY_REQUESTS,
                    "capacity_exceeded",
                    format!("crawl queue is full ({MAX_CRAWLS} active crawls)"),
                ));
            }
        }
        reg.push_back(CrawlRecord {
            snapshot,
            abort: None,
        });
        Ok(())
    }

    fn attach_abort(&self, id: &str, abort: AbortHandle) {
        let mut reg = self.records.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(rec) = reg.iter_mut().find(|r| r.snapshot.id == id) {
            if rec.snapshot.status.terminal() {
                abort.abort();
            } else {
                rec.abort = Some(abort);
            }
        }
    }

    fn mark_running(&self, id: &str, started_at: u64, discovered: usize) {
        let mut reg = self.records.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(rec) = reg.iter_mut().find(|r| r.snapshot.id == id) {
            if !rec.snapshot.status.terminal() {
                rec.snapshot.status = CrawlStatus::Running;
                rec.snapshot.started_at = Some(started_at);
                rec.snapshot.discovered = discovered;
            }
        }
    }

    fn push_page(&self, id: &str, page: CrawlPageResult, discovered: usize) {
        let mut reg = self.records.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(rec) = reg.iter_mut().find(|r| r.snapshot.id == id) {
            if rec.snapshot.status.terminal() {
                return;
            }
            if page.status == "ok" {
                rec.snapshot.completed += 1;
            } else {
                rec.snapshot.failed += 1;
            }
            rec.snapshot.discovered = discovered;
            rec.snapshot.pages.push(page);
        }
    }

    fn finish(&self, id: &str, status: CrawlStatus, error: Option<String>, discovered: usize) {
        let mut reg = self.records.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(rec) = reg.iter_mut().find(|r| r.snapshot.id == id) {
            if !rec.snapshot.status.terminal() {
                rec.snapshot.status = status;
                rec.snapshot.finished_at = Some(now_ms());
                rec.snapshot.discovered = discovered;
                rec.snapshot.error = error;
                rec.abort = None;
            }
        }
    }

    fn get(&self, id: &str) -> Option<CrawlSnapshot> {
        let reg = self.records.lock().unwrap_or_else(|e| e.into_inner());
        reg.iter()
            .find(|r| r.snapshot.id == id)
            .map(|r| r.snapshot.clone())
    }

    fn cancel(&self, id: &str) -> Option<CrawlSnapshot> {
        let mut reg = self.records.lock().unwrap_or_else(|e| e.into_inner());
        let rec = reg.iter_mut().find(|r| r.snapshot.id == id)?;
        if !rec.snapshot.status.terminal() {
            rec.snapshot.status = CrawlStatus::Cancelled;
            rec.snapshot.finished_at = Some(now_ms());
            if let Some(abort) = rec.abort.take() {
                abort.abort();
            }
        }
        Some(rec.snapshot.clone())
    }
}

/// Normalizes HTTP(S) crawl candidate URLs by stripping fragments and non-HTTP schemes.
pub(crate) fn normalize_crawl_url(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return None;
    }
    let mut url = Url::parse(trimmed).ok()?;
    if !matches!(url.scheme(), "http" | "https") || url.host().is_none() {
        return None;
    }
    url.set_fragment(None);
    Some(url.to_string())
}

/// Matches a URL path against a simple glob (`*` wildcards) or prefix pattern.
pub(crate) fn path_matches_pattern(path: &str, pattern: &str) -> bool {
    let pat = pattern.trim();
    if pat.is_empty() {
        return false;
    }
    if !pat.contains('*') {
        return path == pat
            || path.starts_with(pat)
            || path
                .strip_prefix('/')
                .is_some_and(|stripped| stripped.starts_with(pat));
    }
    let parts: Vec<&str> = pat.split('*').collect();
    let mut remainder = path;
    for (idx, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        if idx == 0 {
            let Some(rest) = remainder.strip_prefix(part) else {
                return false;
            };
            remainder = rest;
        } else if idx + 1 == parts.len() {
            return remainder.ends_with(part);
        } else {
            let Some(pos) = remainder.find(part) else {
                return false;
            };
            remainder = &remainder[pos + part.len()..];
        }
    }
    true
}

fn url_path_allowed(raw_url: &str, include_paths: &[String], exclude_paths: &[String]) -> bool {
    let Ok(parsed) = Url::parse(raw_url) else {
        return false;
    };
    let path = parsed.path();
    if !include_paths.is_empty()
        && !include_paths
            .iter()
            .any(|p| path_matches_pattern(path, p))
    {
        return false;
    }
    if exclude_paths.iter().any(|p| path_matches_pattern(path, p)) {
        return false;
    }
    true
}

fn validate_crawl_formats(raw: &[ScrapeFormat]) -> Result<Vec<ScrapeFormat>, String> {
    if raw.is_empty() || raw.len() > 3 {
        return Err("crawl formats must contain 1 to 3 entries (markdown, html, text)".into());
    }
    let mut out = Vec::with_capacity(raw.len());
    for &fmt in raw {
        if !matches!(
            fmt,
            ScrapeFormat::Markdown | ScrapeFormat::Html | ScrapeFormat::Text
        ) {
            return Err("crawl only supports markdown, html, and text formats".into());
        }
        if !out.contains(&fmt) {
            out.push(fmt);
        }
    }
    Ok(out)
}

fn validate_path_patterns(name: &str, patterns: &[String]) -> Result<(), String> {
    if patterns.len() > MAX_PATH_PATTERNS {
        return Err(format!("{name} can contain at most {MAX_PATH_PATTERNS} patterns"));
    }
    for pat in patterns {
        let t = pat.trim();
        if t.is_empty() || t.len() > MAX_PATH_PATTERN_BYTES || t.chars().any(char::is_control) {
            return Err(format!(
                "each entry in {name} must be 1 to {MAX_PATH_PATTERN_BYTES} non-control bytes"
            ));
        }
    }
    Ok(())
}

fn validate_target_url(
    url: &str,
    allow_any_url: bool,
    wait_until: NavigationWait,
    timeout_ms: u64,
    resource_preset: Option<ServerResourcePreset>,
    block_cookie_banners: bool,
    profile: Option<String>,
    proxy_session: Option<String>,
) -> Result<(), String> {
    let req = JobRequest {
        steps: vec![Step::Goto {
            url: url.to_string(),
            wait_until,
        }],
        timeout_ms,
        retries: 0,
        priority: JobPriority::Normal,
        resource_preset,
        block_resources: Vec::new(),
        block_url_patterns: Vec::new(),
        block_cookie_banners,
        profile,
        persist_profile: false,
        proxy_session,
        webhook_url: None,
        webhook_headers: HashMap::new(),
    };
    req.validate_with(allow_any_url)
}

pub(crate) async fn start_crawl(
    State(inner): State<Arc<Inner>>,
    payload: Result<Json<CrawlRequest>, JsonRejection>,
) -> Result<Response, ApiError> {
    if inner.engine.is_none() {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            "/v1/crawl is unavailable in coordinator mode",
        ));
    }
    if !inner.with_registry(|r, _| r.accepting) {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "shutting_down",
            "the server is shutting down",
        ));
    }

    let Json(req) =
        payload.map_err(|e| ApiError::new(e.status(), "invalid_request", e.body_text()))?;

    if req.max_depth > MAX_CRAWL_DEPTH {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            format!("max_depth must be between 0 and {MAX_CRAWL_DEPTH}"),
        ));
    }
    if !(1..=MAX_CRAWL_PAGES).contains(&req.max_pages) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            format!("max_pages must be between 1 and {MAX_CRAWL_PAGES}"),
        ));
    }
    if !(1..=MAX_CONCURRENCY).contains(&req.concurrency) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            format!("concurrency must be between 1 and {MAX_CONCURRENCY}"),
        ));
    }
    if !(1..=MAX_TIMEOUT_MS).contains(&req.timeout_ms) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            format!("timeout_ms must be between 1 and {MAX_TIMEOUT_MS}"),
        ));
    }
    validate_path_patterns("include_paths", &req.include_paths)
        .map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, "invalid_request", e))?;
    validate_path_patterns("exclude_paths", &req.exclude_paths)
        .map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, "invalid_request", e))?;

    let formats = validate_crawl_formats(&req.formats)
        .map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, "invalid_request", e))?;
    if let Some(ref rules) = req.extract {
        fast_html::validate_extract_rules(rules)
            .map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, "invalid_request", e))?;
    }

    let seed_url = normalize_crawl_url(&req.url).ok_or_else(|| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "url must be a valid http or https URL",
        )
    })?;
    let seed_origin = UrlOrigin::parse(&seed_url).ok_or_else(|| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "url must have a valid origin host",
        )
    })?;

    validate_target_url(
        &seed_url,
        inner.allow_any_url(),
        req.wait_until,
        req.timeout_ms,
        req.resource_preset,
        req.block_cookie_banners,
        req.profile.clone(),
        req.proxy_session.clone(),
    )
    .map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, "invalid_request", e))?;

    let id = Uuid::new_v4().to_string();
    let created_at = now_ms();
    let snapshot = CrawlSnapshot {
        id: id.clone(),
        status: CrawlStatus::Queued,
        seed_url: seed_url.clone(),
        created_at,
        started_at: None,
        finished_at: None,
        discovered: 1,
        completed: 0,
        failed: 0,
        pages: Vec::new(),
        error: None,
    };

    inner.crawls.admit(snapshot.clone())?;

    let cfg = CrawlRunConfig {
        seed_url,
        seed_origin,
        max_depth: req.max_depth,
        max_pages: req.max_pages,
        concurrency: req.concurrency,
        same_origin: req.same_origin,
        include_subdomains: req.include_subdomains,
        include_paths: req.include_paths,
        exclude_paths: req.exclude_paths,
        formats,
        mode: req.mode,
        extract: req.extract,
        timeout_ms: req.timeout_ms,
        wait_until: req.wait_until,
        resource_preset: req.resource_preset.or(Some(ServerResourcePreset::Scraping)),
        block_cookie_banners: req.block_cookie_banners,
        profile: req.profile,
        proxy_session: req.proxy_session,
    };

    let inner_bg = Arc::clone(&inner);
    let id_bg = id.clone();
    let handle = tokio::spawn(async move {
        run_crawl(inner_bg, id_bg, cfg).await;
    });
    inner.crawls.attach_abort(&id, handle.abort_handle());

    Ok((StatusCode::ACCEPTED, Json(snapshot)).into_response())
}

pub(crate) async fn get_crawl(
    State(inner): State<Arc<Inner>>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    if inner.engine.is_none() {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            "/v1/crawl is unavailable in coordinator mode",
        ));
    }
    match inner.crawls.get(&id) {
        Some(snap) => Ok((StatusCode::OK, Json(snap)).into_response()),
        None => Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            "crawl job not found",
        )),
    }
}

pub(crate) async fn cancel_crawl(
    State(inner): State<Arc<Inner>>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    if inner.engine.is_none() {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            "/v1/crawl is unavailable in coordinator mode",
        ));
    }
    match inner.crawls.cancel(&id) {
        Some(snap) => Ok((StatusCode::OK, Json(snap)).into_response()),
        None => Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            "crawl job not found",
        )),
    }
}

#[derive(Clone)]
struct CrawlRunConfig {
    seed_url: String,
    seed_origin: UrlOrigin,
    max_depth: usize,
    max_pages: usize,
    concurrency: usize,
    same_origin: bool,
    include_subdomains: bool,
    include_paths: Vec<String>,
    exclude_paths: Vec<String>,
    formats: Vec<ScrapeFormat>,
    mode: FetchMode,
    extract: Option<BTreeMap<String, ExtractField>>,
    timeout_ms: u64,
    wait_until: NavigationWait,
    resource_preset: Option<ServerResourcePreset>,
    block_cookie_banners: bool,
    profile: Option<String>,
    proxy_session: Option<String>,
}

struct CrawlPageOutcome {
    result: CrawlPageResult,
    links: Vec<String>,
}

async fn run_crawl(inner: Arc<Inner>, crawl_id: String, cfg: CrawlRunConfig) {
    let Some(engine) = inner.engine.clone() else {
        inner
            .crawls
            .finish(&crawl_id, CrawlStatus::Failed, Some("engine unavailable".into()), 0);
        return;
    };

    inner.crawls.mark_running(&crawl_id, now_ms(), 1);
    let mut visited = HashSet::new();
    let mut queue = VecDeque::new();
    visited.insert(cfg.seed_url.clone());
    queue.push_back((cfg.seed_url.clone(), 0usize));

    let mut scheduled = 0usize;
    let queue_cap = cfg.max_pages.saturating_mul(4);

    while !queue.is_empty() && scheduled < cfg.max_pages {
        if !inner.with_registry(|r, _| r.accepting) {
            inner.crawls.finish(
                &crawl_id,
                CrawlStatus::Cancelled,
                Some("server shutting down".into()),
                visited.len(),
            );
            return;
        }

        let batch_limit = cfg.concurrency.min(cfg.max_pages - scheduled);
        let mut join_set = JoinSet::new();
        for _ in 0..batch_limit {
            let Some((url, depth)) = queue.pop_front() else {
                break;
            };
            scheduled += 1;
            let inner_c = Arc::clone(&inner);
            let engine_c = engine.clone();
            let cfg_c = cfg.clone();
            join_set.spawn(async move { crawl_one_page(inner_c, engine_c, cfg_c, url, depth).await });
        }

        while let Some(joined) = join_set.join_next().await {
            let Ok(outcome) = joined else {
                continue;
            };
            if outcome.result.depth < cfg.max_depth {
                for raw_link in outcome.links {
                    if visited.len() >= queue_cap {
                        break;
                    }
                    let Some(norm) = normalize_crawl_url(&raw_link) else {
                        continue;
                    };
                    if cfg.same_origin {
                        let Some(origin) = UrlOrigin::parse(&norm) else {
                            continue;
                        };
                        if !origin.matches(&cfg.seed_origin, cfg.include_subdomains) {
                            continue;
                        }
                    }
                    if !url_path_allowed(&norm, &cfg.include_paths, &cfg.exclude_paths) {
                        continue;
                    }
                    if visited.insert(norm.clone()) {
                        queue.push_back((norm, outcome.result.depth + 1));
                    }
                }
            }
            inner.crawls.push_page(&crawl_id, outcome.result, visited.len());
        }
    }

    inner
        .crawls
        .finish(&crawl_id, CrawlStatus::Completed, None, visited.len());
}

async fn crawl_one_page(
    inner: Arc<Inner>,
    engine: fluxwright::BrowserEngine,
    cfg: CrawlRunConfig,
    url: String,
    depth: usize,
) -> CrawlPageOutcome {
    let started = Instant::now();
    let allow_any_url = inner.allow_any_url();
    if let Err(reason) = validate_target_url(
        &url,
        allow_any_url,
        cfg.wait_until,
        cfg.timeout_ms,
        cfg.resource_preset,
        cfg.block_cookie_banners,
        cfg.profile.clone(),
        cfg.proxy_session.clone(),
    ) {
        return CrawlPageOutcome {
            result: CrawlPageResult {
                url,
                depth,
                status: "failed".into(),
                engine: "http".into(),
                title: String::new(),
                markdown: None,
                html: None,
                text: None,
                extracted: None,
                duration_ms: started.elapsed().as_millis() as u64,
                error: Some(reason),
            },
            links: Vec::new(),
        };
    }

    let need_links = depth < cfg.max_depth;
    let try_http = cfg.mode != FetchMode::Browser && cfg.profile.is_none();
    if try_http {
        match fast_fetch::fetch_url(
            &url,
            Duration::from_millis(cfg.timeout_ms),
            allow_any_url,
            engine.proxy_pool(),
            cfg.proxy_session.as_deref(),
        )
        .await
        {
            Err(FastFetchError::InvalidOrBlocked(msg)) => {
                return CrawlPageOutcome {
                    result: CrawlPageResult {
                        url,
                        depth,
                        status: "failed".into(),
                        engine: "http".into(),
                        title: String::new(),
                        markdown: None,
                        html: None,
                        text: None,
                        extracted: None,
                        duration_ms: started.elapsed().as_millis() as u64,
                        error: Some(msg),
                    },
                    links: Vec::new(),
                };
            }
            Err(FastFetchError::Transport(msg)) => {
                if cfg.mode == FetchMode::Http {
                    return CrawlPageOutcome {
                        result: CrawlPageResult {
                            url,
                            depth,
                            status: "failed".into(),
                            engine: "http".into(),
                            title: String::new(),
                            markdown: None,
                            html: None,
                            text: None,
                            extracted: None,
                            duration_ms: started.elapsed().as_millis() as u64,
                            error: Some(msg),
                        },
                        links: Vec::new(),
                    };
                }
                inner.fastpath_fallbacks.fetch_add(1, Ordering::Relaxed);
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
                    if cfg.mode == FetchMode::Http {
                        return CrawlPageOutcome {
                            result: CrawlPageResult {
                                url: resp.final_url,
                                depth,
                                status: "failed".into(),
                                engine: "http".into(),
                                title: String::new(),
                                markdown: None,
                                html: None,
                                text: None,
                                extracted: None,
                                duration_ms: started.elapsed().as_millis() as u64,
                                error: Some(reason),
                            },
                            links: Vec::new(),
                        };
                    }
                    inner.fastpath_fallbacks.fetch_add(1, Ordering::Relaxed);
                } else {
                    inner.fastpath_hits.fetch_add(1, Ordering::Relaxed);
                    let want_md = cfg.formats.contains(&ScrapeFormat::Markdown);
                    let want_html = cfg.formats.contains(&ScrapeFormat::Html);
                    let want_txt = cfg.formats.contains(&ScrapeFormat::Text);
                    let fields = fast_html::extract_scrape_fields(
                        &html_str,
                        &resp.final_url,
                        want_md,
                        want_txt,
                    );
                    let extracted = match cfg.extract {
                        Some(ref rules) => fast_html::extract_css_fields(&html_str, rules).ok(),
                        None => None,
                    };
                    let links = if need_links {
                        fast_html::extract_links(&html_str, &resp.final_url)
                    } else {
                        Vec::new()
                    };
                    return CrawlPageOutcome {
                        result: CrawlPageResult {
                            url: resp.final_url,
                            depth,
                            status: "ok".into(),
                            engine: "http".into(),
                            title: fields.title,
                            markdown: want_md.then(|| fields.markdown.unwrap_or_default()),
                            html: want_html.then_some(html_str),
                            text: want_txt.then(|| fields.text.unwrap_or_default()),
                            extracted,
                            duration_ms: started.elapsed().as_millis() as u64,
                            error: None,
                        },
                        links,
                    };
                }
            }
        }
    }

    let elapsed_ms = started.elapsed().as_millis() as u64;
    let remaining_ms = cfg.timeout_ms.saturating_sub(elapsed_ms).max(1);
    let extra_html = cfg.extract.is_some() && !cfg.formats.contains(&ScrapeFormat::Html);

    let mut steps = vec![
        Step::Goto {
            url: url.clone(),
            wait_until: cfg.wait_until,
        },
        Step::Url {},
        Step::Title {},
    ];
    for &fmt in &cfg.formats {
        steps.push(match fmt {
            ScrapeFormat::Markdown => Step::Markdown {
                selector: None,
                main_content_only: false,
                include_links: true,
                include_images: false,
            },
            ScrapeFormat::Html => Step::Content {},
            ScrapeFormat::Text => Step::Evaluate {
                expression: EXTRACT_TEXT_JS.into(),
            },
            _ => unreachable!("validated crawl format"),
        });
    }
    if extra_html {
        steps.push(Step::Content {});
    }
    if need_links {
        steps.push(Step::Evaluate {
            expression: EXTRACT_LINKS_JS.to_string(),
        });
    }

    let job_req = JobRequest {
        steps,
        timeout_ms: remaining_ms,
        retries: 0,
        priority: JobPriority::Normal,
        resource_preset: cfg.resource_preset,
        block_resources: Vec::new(),
        block_url_patterns: Vec::new(),
        block_cookie_banners: cfg.block_cookie_banners,
        profile: cfg.profile,
        persist_profile: false,
        proxy_session: cfg.proxy_session,
        webhook_url: None,
        webhook_headers: HashMap::new(),
    };
    let progress = Arc::new(Mutex::new(ExecutionProgress::default()));
    let tail_len = 2 + cfg.formats.len() + usize::from(extra_html) + usize::from(need_links);

    match execute_job(
        engine,
        job_req,
        Some(Arc::downgrade(&inner)),
        Uuid::new_v4(),
        JobTraceContext::from_headers(None),
        0,
        progress,
    )
    .await
    {
        Ok(results) => {
            let base = results.len().saturating_sub(tail_len);
            let final_url = results
                .get(base)
                .and_then(Value::as_str)
                .unwrap_or(&url)
                .to_string();
            let title = results
                .get(base + 1)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let mut markdown = None;
            let mut html = None;
            let mut text = None;
            for (idx, fmt) in cfg.formats.iter().enumerate() {
                let val = results.get(base + 2 + idx).and_then(Value::as_str);
                match fmt {
                    ScrapeFormat::Markdown => markdown = val.map(str::to_string),
                    ScrapeFormat::Html => html = val.map(str::to_string),
                    ScrapeFormat::Text => text = val.map(str::to_string),
                    _ => {}
                }
            }
            let raw_html = if extra_html {
                results
                    .get(base + 2 + cfg.formats.len())
                    .and_then(Value::as_str)
            } else {
                html.as_deref()
            };
            let extracted = match (cfg.extract, raw_html) {
                (Some(ref rules), Some(doc_html)) => {
                    fast_html::extract_css_fields(doc_html, rules).ok()
                }
                _ => None,
            };
            let links = if need_links {
                let link_idx = base + 2 + cfg.formats.len() + usize::from(extra_html);
                results
                    .get(link_idx)
                    .and_then(Value::as_array)
                    .map(|arr| {
                        arr.iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default()
            } else {
                Vec::new()
            };

            CrawlPageOutcome {
                result: CrawlPageResult {
                    url: final_url,
                    depth,
                    status: "ok".into(),
                    engine: "browser".into(),
                    title,
                    markdown,
                    html,
                    text,
                    extracted,
                    duration_ms: started.elapsed().as_millis() as u64,
                    error: None,
                },
                links,
            }
        }
        Err(err) => CrawlPageOutcome {
            result: CrawlPageResult {
                url,
                depth,
                status: "failed".into(),
                engine: "browser".into(),
                title: String::new(),
                markdown: None,
                html: None,
                text: None,
                extracted: None,
                duration_ms: started.elapsed().as_millis() as u64,
                error: Some(err.to_string()),
            },
            links: Vec::new(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_crawl_url_strips_fragment_and_rejects_non_http() {
        assert_eq!(
            normalize_crawl_url("https://example.com/docs#section").as_deref(),
            Some("https://example.com/docs")
        );
        assert!(normalize_crawl_url("javascript:alert(1)").is_none());
        assert!(normalize_crawl_url("mailto:ops@example.com").is_none());
        assert!(normalize_crawl_url("not a url").is_none());
    }

    #[test]
    fn path_matches_pattern_handles_wildcards() {
        assert!(path_matches_pattern("/admin/users", "/admin*"));
        assert!(path_matches_pattern("/docs/v1/intro", "/docs/*/intro"));
        assert!(!path_matches_pattern("/blog/post", "/docs/*"));
        assert!(path_matches_pattern("/blog/post", "/blog"));
    }
}
