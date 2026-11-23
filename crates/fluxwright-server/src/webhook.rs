//! Signed asynchronous job-completion webhook delivery (`job.completed`).
//!
//! Dispatches `POST` requests with `Content-Type: application/json`,
//! `X-Fluxwright-Event`, `X-Fluxwright-Job-Id`, `X-Fluxwright-Timestamp`, and optional
//! `X-Fluxwright-Signature: sha256=<hex>` (HMAC-SHA256 via `ring::hmac` over `"{ts}.{body}"`).
//! Bounded by a 32-permit semaphore and up to 3 attempts (500ms, 1500ms backoff, 10s per attempt).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fluxwright::netguard;
use ring::hmac;
use serde_json::json;
use tokio::sync::Semaphore;

use crate::Job;
use crate::http_post::{
    HttpEndpoint, SMALL_HTTP_RESPONSE_BYTES, is_reserved_header, post_bytes, validate_header_name,
    validate_header_value,
};

pub(crate) const MAX_WEBHOOK_HEADERS: usize = 16;
pub(crate) const MAX_WEBHOOK_URL_BYTES: usize = 2048;
const MAX_INFLIGHT_WEBHOOKS: usize = 32;
const MAX_QUEUED_WEBHOOKS: usize = 512;
const WEBHOOK_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(10);
const WEBHOOK_BACKOFFS: [Duration; 2] = [Duration::from_millis(500), Duration::from_millis(1500)];

#[derive(Clone, Debug)]
pub(crate) struct WebhookTarget {
    pub(crate) url: String,
    pub(crate) headers: HashMap<String, String>,
}

impl WebhookTarget {
    pub(crate) fn from_request(req: &crate::JobRequest) -> Option<Self> {
        req.webhook_url.as_ref().map(|url| Self {
            url: url.clone(),
            headers: req.webhook_headers.clone(),
        })
    }
}

pub(crate) fn validate_webhook_config(
    url: Option<&str>,
    headers: &HashMap<String, String>,
    allow_any_url: bool,
) -> Result<(), String> {
    if let Some(raw) = url {
        if raw.len() > MAX_WEBHOOK_URL_BYTES {
            return Err(format!(
                "webhook_url must be at most {MAX_WEBHOOK_URL_BYTES} bytes"
            ));
        }
        HttpEndpoint::parse(raw, true).map_err(|e| format!("invalid webhook_url: {e}"))?;
        if !allow_any_url {
            netguard::url_allowed(raw).map_err(|e| format!("webhook_url is not allowed: {e}"))?;
        }
    }

    if headers.len() > MAX_WEBHOOK_HEADERS {
        return Err(format!(
            "webhook_headers may contain at most {MAX_WEBHOOK_HEADERS} entries"
        ));
    }

    let mut seen_lower = HashSet::with_capacity(headers.len());
    for (name, value) in headers {
        validate_header_name(name)?;
        validate_header_value(value)?;
        if is_reserved_header(name) {
            return Err(format!("webhook header '{name}' is reserved"));
        }
        let lower = name.to_ascii_lowercase();
        if !seen_lower.insert(lower) {
            return Err(format!("duplicate webhook header '{name}'"));
        }
    }

    Ok(())
}

/// Computes `sha256=<hex(HMAC-SHA256(secret, "{timestamp_secs}.{body}"))>`.
pub(crate) fn sign_payload(secret: &[u8], timestamp_secs: u64, body: &[u8]) -> String {
    let key = hmac::Key::new(hmac::HMAC_SHA256, secret);
    let mut ctx = hmac::Context::with_key(&key);
    let ts = timestamp_secs.to_string();
    ctx.update(ts.as_bytes());
    ctx.update(b".");
    ctx.update(body);
    let tag = ctx.sign();
    format!("sha256={}", hex_lower(tag.as_ref()))
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

struct QueueSlotGuard(Arc<AtomicUsize>);

impl Drop for QueueSlotGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

fn try_acquire_queue_slot(counter: &Arc<AtomicUsize>) -> Option<QueueSlotGuard> {
    let mut cur = counter.load(Ordering::Acquire);
    loop {
        if cur >= MAX_QUEUED_WEBHOOKS {
            return None;
        }
        match counter.compare_exchange_weak(cur, cur + 1, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return Some(QueueSlotGuard(counter.clone())),
            Err(next) => cur = next,
        }
    }
}

#[derive(Clone)]
pub(crate) struct WebhookDispatcher {
    secret: Option<Arc<Vec<u8>>>,
    permits: Arc<Semaphore>,
    queued: Arc<AtomicUsize>,
}

impl WebhookDispatcher {
    pub(crate) fn new(secret: Option<Vec<u8>>) -> Self {
        Self {
            secret: secret.map(Arc::new),
            permits: Arc::new(Semaphore::new(MAX_INFLIGHT_WEBHOOKS)),
            queued: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub(crate) fn dispatch(&self, target: WebhookTarget, job: Job, allow_any_url: bool) {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let Some(slot_guard) = try_acquire_queue_slot(&self.queued) else {
            tracing::warn!(job_id = %job.id, "webhook dispatch queue full; dropping delivery");
            return;
        };
        let secret = self.secret.clone();
        let permits = self.permits.clone();

        handle.spawn(async move {
            let _slot_guard = slot_guard;
            let Ok(_permit) = permits.acquire_owned().await else {
                return;
            };
            deliver_with_retries(
                target,
                job,
                secret.as_deref().map(Vec::as_slice),
                allow_any_url,
            )
            .await;
        });
    }
}

async fn deliver_with_retries(
    target: WebhookTarget,
    job: Job,
    secret: Option<&[u8]>,
    allow_any_url: bool,
) {
    let endpoint = match HttpEndpoint::parse(&target.url, true) {
        Ok(ep) => ep,
        Err(err) => {
            tracing::warn!(job_id = %job.id, error = %err, "invalid webhook URL; skipping");
            return;
        }
    };

    let Ok(body) = serde_json::to_vec(&json!({
        "event": "job.completed",
        "job": job,
    })) else {
        tracing::warn!(job_id = %job.id, "failed to serialize webhook payload");
        return;
    };

    let mut custom_headers: Vec<(String, String)> = target
        .headers
        .into_iter()
        .filter(|(k, _)| !k.eq_ignore_ascii_case("content-type"))
        .collect();
    custom_headers.sort_by(|a, b| a.0.cmp(&b.0));

    let job_id_str = job.id.to_string();

    for attempt in 0..3 {
        let ts_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let ts_str = ts_secs.to_string();
        let signature = secret.map(|s| sign_payload(s, ts_secs, &body));

        let mut headers: Vec<(&str, &str)> = Vec::with_capacity(5 + custom_headers.len());
        headers.push(("Content-Type", "application/json"));
        headers.push(("X-Fluxwright-Event", "job.completed"));
        headers.push(("X-Fluxwright-Job-Id", &job_id_str));
        headers.push(("X-Fluxwright-Timestamp", &ts_str));
        if let Some(ref sig) = signature {
            headers.push(("X-Fluxwright-Signature", sig.as_str()));
        }
        for (k, v) in &custom_headers {
            headers.push((k.as_str(), v.as_str()));
        }

        match post_bytes(
            &endpoint,
            None,
            &headers,
            &body,
            WEBHOOK_ATTEMPT_TIMEOUT,
            SMALL_HTTP_RESPONSE_BYTES,
            !allow_any_url,
        )
        .await
        {
            Ok((status, _)) if (200..300).contains(&status) => {
                tracing::debug!(job_id = %job.id, host = %endpoint.host, status, "webhook delivered");
                return;
            }
            Ok((status, _)) if (400..500).contains(&status) && status != 408 && status != 429 => {
                tracing::warn!(
                    job_id = %job.id,
                    host = %endpoint.host,
                    status,
                    "webhook receiver rejected delivery; not retrying"
                );
                return;
            }
            Ok((status, _)) => {
                if attempt < WEBHOOK_BACKOFFS.len() {
                    tracing::debug!(
                        job_id = %job.id,
                        host = %endpoint.host,
                        attempt = attempt + 1,
                        status,
                        "webhook delivery returned retryable status"
                    );
                } else {
                    tracing::warn!(
                        job_id = %job.id,
                        host = %endpoint.host,
                        status,
                        "webhook delivery failed after 3 attempts"
                    );
                    return;
                }
            }
            Err(err) => {
                if attempt < WEBHOOK_BACKOFFS.len() {
                    tracing::debug!(
                        job_id = %job.id,
                        host = %endpoint.host,
                        attempt = attempt + 1,
                        error = %err,
                        "webhook delivery attempt failed"
                    );
                } else {
                    tracing::warn!(
                        job_id = %job.id,
                        host = %endpoint.host,
                        error = %err,
                        "webhook delivery failed after 3 attempts"
                    );
                    return;
                }
            }
        }

        tokio::time::sleep(WEBHOOK_BACKOFFS[attempt]).await;
    }
}
