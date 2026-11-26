//! Synchronous one-shot scrape and render endpoint (`POST /v1/scrape`).

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::rejection::JsonRejection;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::fast_fetch::{self, FastFetchError, FetchMode};
use crate::fast_html;
use crate::job::{
    JobPriority, JobRequest, NavigationWait, ServerImageFormat, ServerResourcePreset,
    ServerResourceType, Step,
};
use crate::{execute_job, ApiError, ExecutionProgress, Inner, JobTraceContext};

const DEFAULT_SCRAPE_TIMEOUT_MS: u64 = 30_000;
const MAX_SCRAPE_TIMEOUT_MS: u64 = 120_000;
const MAX_SCRAPE_FORMATS: usize = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ScrapeFormat {
    Markdown,
    Html,
    Text,
    Snapshot,
    Screenshot,
    Pdf,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ScrapeRequest {
    url: String,
    #[serde(default = "default_formats")]
    formats: Vec<ScrapeFormat>,
    #[serde(default)]
    mode: FetchMode,
    #[serde(default)]
    wait_until: NavigationWait,
    #[serde(default)]
    wait_for_selector: Option<String>,
    #[serde(default = "default_timeout_ms")]
    timeout_ms: u64,
    #[serde(default)]
    priority: JobPriority,
    #[serde(default)]
    resource_preset: Option<ServerResourcePreset>,
    #[serde(default)]
    block_resources: Vec<ServerResourceType>,
    #[serde(default)]
    block_url_patterns: Vec<String>,
    #[serde(default)]
    block_cookie_banners: bool,
    #[serde(default)]
    profile: Option<String>,
    #[serde(default)]
    persist_profile: bool,
    #[serde(default)]
    proxy_session: Option<String>,
    #[serde(default)]
    screenshot_full_page: bool,
    #[serde(default)]
    screenshot_annotate: bool,
    #[serde(default)]
    snapshot_interactive_only: bool,
}

fn default_formats() -> Vec<ScrapeFormat> {
    vec![ScrapeFormat::Markdown]
}

fn default_timeout_ms() -> u64 {
    DEFAULT_SCRAPE_TIMEOUT_MS
}

fn dedup_formats(raw: &[ScrapeFormat]) -> Result<Vec<ScrapeFormat>, String> {
    if raw.is_empty() || raw.len() > MAX_SCRAPE_FORMATS {
        return Err(format!(
            "formats must contain between 1 and {MAX_SCRAPE_FORMATS} entries"
        ));
    }
    let mut seen = Vec::with_capacity(raw.len());
    for &f in raw {
        if !seen.contains(&f) {
            seen.push(f);
        }
    }
    Ok(seen)
}

pub(crate) async fn scrape_handler(
    State(inner): State<Arc<Inner>>,
    headers: HeaderMap,
    payload: Result<Json<ScrapeRequest>, JsonRejection>,
) -> Result<Response, ApiError> {
    let Some(engine) = inner.engine.clone() else {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            "/v1/scrape is unavailable in coordinator mode",
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

    if !(1..=MAX_SCRAPE_TIMEOUT_MS).contains(&req.timeout_ms) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            format!("timeout_ms must be between 1 and {MAX_SCRAPE_TIMEOUT_MS}"),
        ));
    }
    let formats = dedup_formats(&req.formats)
        .map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, "invalid_request", e))?;

    let has_visual = formats
        .iter()
        .any(|f| matches!(f, ScrapeFormat::Screenshot | ScrapeFormat::Pdf));
    let has_snapshot = formats.iter().any(|f| matches!(f, ScrapeFormat::Snapshot));
    let has_browser_only_opts = has_visual
        || has_snapshot
        || req.wait_for_selector.is_some()
        || req.profile.is_some()
        || req.persist_profile
        || !req.block_resources.is_empty()
        || !req.block_url_patterns.is_empty();
    let is_about_blank = req.url.trim().eq_ignore_ascii_case("about:blank");

    let req_url = req.url.clone();
    let proxy_session = req.proxy_session.clone();
    let mode = req.mode;
    let total_timeout_ms = req.timeout_ms;

    let mut steps = vec![Step::Goto {
        url: req.url,
        wait_until: req.wait_until,
    }];
    if let Some(selector) = req.wait_for_selector {
        steps.push(Step::WaitForSelector { selector });
    }
    steps.push(Step::Url {});
    steps.push(Step::Title {});

    for &fmt in &formats {
        steps.push(match fmt {
            ScrapeFormat::Markdown => Step::Markdown {
                selector: None,
                main_content_only: false,
                include_links: true,
                include_images: false,
            },
            ScrapeFormat::Html => Step::Content {},
            ScrapeFormat::Text => Step::Evaluate {
                expression: "document.body ? document.body.innerText : (document.documentElement ? document.documentElement.textContent : '')".into(),
            },
            ScrapeFormat::Snapshot => Step::Snapshot {
                interactive_only: req.snapshot_interactive_only,
                compact: true,
                max_depth: None,
                max_nodes: None,
                diff: false,
            },
            ScrapeFormat::Screenshot => Step::Screenshot {
                full_page: req.screenshot_full_page,
                kind: ServerImageFormat::Png,
                quality: None,
                clip: None,
                omit_background: false,
                annotate: req.screenshot_annotate,
                mask: Vec::new(),
                mask_color: None,
                animations: None,
                caret: None,
            },
            ScrapeFormat::Pdf => Step::Pdf {
                format: Some("A4".into()),
                width: None,
                height: None,
                margin: None,
                landscape: false,
                print_background: true,
                scale: None,
                page_ranges: None,
                prefer_css_page_size: false,
                display_header_footer: false,
                header_template: None,
                footer_template: None,
                outline: false,
                tagged: false,
            },
        });
    }

    let browser_resource_preset = req
        .resource_preset
        .or_else(|| (!has_visual).then_some(ServerResourcePreset::Scraping));

    let mut job_req = JobRequest {
        steps,
        timeout_ms: total_timeout_ms,
        retries: 0,
        priority: req.priority,
        resource_preset: browser_resource_preset,
        block_resources: req.block_resources,
        block_url_patterns: req.block_url_patterns,
        block_cookie_banners: req.block_cookie_banners,
        profile: req.profile,
        persist_profile: req.persist_profile,
        proxy_session: req.proxy_session,
        webhook_url: None,
        webhook_headers: HashMap::new(),
    };
    job_req
        .validate_with(inner.allow_any_url())
        .map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, "invalid_request", e))?;

    if mode == FetchMode::Http && (has_browser_only_opts || is_about_blank) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "mode 'http' cannot be used with about:blank, screenshot, pdf, snapshot, wait_for_selector, profile, persist_profile, block_resources, or block_url_patterns",
        ));
    }

    let started = Instant::now();
    let mut fallback_reason: Option<String> = None;

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
                    return Err(ApiError::new(StatusCode::BAD_GATEWAY, "scrape_failed", msg));
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
                        return Err(ApiError::new(
                            StatusCode::BAD_GATEWAY,
                            "scrape_failed",
                            reason,
                        ));
                    }
                    inner.fastpath_fallbacks.fetch_add(1, Ordering::Relaxed);
                    fallback_reason = Some(reason);
                } else {
                    inner.fastpath_hits.fetch_add(1, Ordering::Relaxed);
                    let want_markdown = formats.contains(&ScrapeFormat::Markdown);
                    let want_text = formats.contains(&ScrapeFormat::Text);
                    let extracted = fast_html::extract_scrape_fields(
                        &html_str,
                        &resp.final_url,
                        want_markdown,
                        want_text,
                    );
                    let duration_ms = started.elapsed().as_millis() as u64;

                    let mut obj = serde_json::Map::new();
                    obj.insert("url".into(), Value::String(resp.final_url));
                    obj.insert("title".into(), Value::String(extracted.title));
                    obj.insert("duration_ms".into(), json!(duration_ms));
                    obj.insert("engine".into(), Value::String("http".into()));
                    for &fmt in &formats {
                        match fmt {
                            ScrapeFormat::Markdown => {
                                obj.insert(
                                    "markdown".into(),
                                    Value::String(extracted.markdown.clone().unwrap_or_default()),
                                );
                            }
                            ScrapeFormat::Html => {
                                obj.insert("html".into(), Value::String(html_str.clone()));
                            }
                            ScrapeFormat::Text => {
                                obj.insert(
                                    "text".into(),
                                    Value::String(extracted.text.clone().unwrap_or_default()),
                                );
                            }
                            ScrapeFormat::Snapshot
                            | ScrapeFormat::Screenshot
                            | ScrapeFormat::Pdf => {}
                        }
                    }
                    return Ok((StatusCode::OK, Json(Value::Object(obj))).into_response());
                }
            }
        }

        let elapsed_ms = started.elapsed().as_millis() as u64;
        if elapsed_ms >= total_timeout_ms {
            return Err(ApiError::new(
                StatusCode::GATEWAY_TIMEOUT,
                "timeout",
                "scrape request timed out",
            ));
        }
        job_req.timeout_ms = total_timeout_ms.saturating_sub(elapsed_ms).max(1);
    }

    let traceparent = headers.get("traceparent").and_then(|v| v.to_str().ok());
    let trace_ctx = JobTraceContext::from_headers(traceparent);
    let progress = Arc::new(Mutex::new(ExecutionProgress::default()));

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
    .map_err(map_scrape_error)?;
    let duration_ms = started.elapsed().as_millis() as u64;

    // Last (2 + formats.len()) elements hold [url, title, ...format results].
    let base = results.len().saturating_sub(2 + formats.len());
    let final_url = results
        .get(base)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let title = results
        .get(base + 1)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    let mut obj = serde_json::Map::new();
    obj.insert("url".into(), Value::String(final_url));
    obj.insert("title".into(), Value::String(title));
    obj.insert("duration_ms".into(), json!(duration_ms));
    obj.insert("engine".into(), Value::String("browser".into()));
    if let Some(reason) = fallback_reason {
        obj.insert("fallback_reason".into(), Value::String(reason));
    }

    for (idx, fmt) in formats.iter().enumerate() {
        let Some(val) = results.get(base + 2 + idx) else {
            continue;
        };
        match fmt {
            ScrapeFormat::Markdown => {
                obj.insert("markdown".into(), val.clone());
            }
            ScrapeFormat::Html => {
                obj.insert("html".into(), val.clone());
            }
            ScrapeFormat::Text => {
                obj.insert("text".into(), val.clone());
            }
            ScrapeFormat::Snapshot => {
                obj.insert("snapshot".into(), val.clone());
            }
            ScrapeFormat::Screenshot => {
                if let Some(b64) = val.get("base64").and_then(Value::as_str) {
                    obj.insert("screenshot_base64".into(), Value::String(b64.to_string()));
                }
            }
            ScrapeFormat::Pdf => {
                if let Some(b64) = val.get("base64").and_then(Value::as_str) {
                    obj.insert("pdf_base64".into(), Value::String(b64.to_string()));
                }
            }
        }
    }

    Ok((StatusCode::OK, Json(Value::Object(obj))).into_response())
}

fn map_scrape_error(err: fluxwright::Error) -> ApiError {
    match err {
        fluxwright::Error::QueueFull => {
            ApiError::new(StatusCode::TOO_MANY_REQUESTS, "capacity_exceeded", err.to_string())
        }
        fluxwright::Error::ShuttingDown | fluxwright::Error::AcquireTimeout => {
            ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "unavailable", err.to_string())
        }
        fluxwright::Error::JobTimeout => {
            ApiError::new(StatusCode::GATEWAY_TIMEOUT, "timeout", err.to_string())
        }
        other => {
            let msg = other.to_string();
            if msg.contains("profiles_dir") {
                ApiError::new(StatusCode::BAD_REQUEST, "invalid_request", msg)
            } else {
                ApiError::new(StatusCode::BAD_GATEWAY, "scrape_failed", msg)
            }
        }
    }
}
