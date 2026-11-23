//! OpenTelemetry OTLP/HTTP JSON trace exporter (`POST <base>/v1/traces`).
//!
//! Spans are buffered in a non-blocking 512-capacity bounded channel and flushed in batches of up
//! to 32 spans or every 2 seconds in the background.

use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

use crate::http_post::{HttpEndpoint, SMALL_HTTP_RESPONSE_BYTES, post_bytes};
use crate::{JobStatus, JobTraceContext};

const OTLP_CHANNEL_CAPACITY: usize = 512;
const OTLP_FLUSH_BATCH_SIZE: usize = 32;
const OTLP_FLUSH_INTERVAL: Duration = Duration::from_secs(2);
const OTLP_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone)]
pub(crate) struct OtlpSpanData {
    pub(crate) trace_ctx: JobTraceContext,
    pub(crate) job_id: Uuid,
    pub(crate) status: JobStatus,
    pub(crate) steps: usize,
    pub(crate) attempts: u32,
    pub(crate) start_time_unix_nano: u128,
    pub(crate) end_time_unix_nano: u128,
    pub(crate) error: Option<String>,
}

pub(crate) fn parse_otlp_endpoint(raw: &str) -> Result<(HttpEndpoint, String), String> {
    let ep = HttpEndpoint::parse(raw, true)?;
    let (path, query) = match ep.path_and_query.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (ep.path_and_query.as_str(), None),
    };
    let trimmed = path.trim_end_matches('/');
    let mut traces_path = if trimmed.ends_with("/v1/traces") {
        trimmed.to_string()
    } else {
        format!("{trimmed}/v1/traces")
    };
    if let Some(q) = query {
        traces_path.push('?');
        traces_path.push_str(q);
    }
    Ok((ep, traces_path))
}

pub(crate) fn build_export_payload(service_name: &str, spans: &[OtlpSpanData]) -> Value {
    let mut span_values = Vec::with_capacity(spans.len());
    for span in spans {
        let status_str = match span.status {
            JobStatus::Queued => "queued",
            JobStatus::Running => "running",
            JobStatus::Succeeded => "succeeded",
            JobStatus::Failed => "failed",
            JobStatus::Cancelled => "cancelled",
        };
        let status_obj = match span.status {
            JobStatus::Succeeded => json!({ "code": 1 }),
            JobStatus::Failed => json!({
                "code": 2,
                "message": span.error.as_deref().unwrap_or("job failed"),
            }),
            JobStatus::Cancelled => json!({
                "code": 2,
                "message": span.error.as_deref().unwrap_or("job cancelled"),
            }),
            JobStatus::Queued | JobStatus::Running => json!({ "code": 0 }),
        };
        let end_ns = span.end_time_unix_nano.max(span.start_time_unix_nano);
        let mut obj = json!({
            "traceId": span.trace_ctx.trace_id,
            "spanId": span.trace_ctx.root_span_id,
            "name": "fluxwright.job",
            "kind": 2,
            "startTimeUnixNano": span.start_time_unix_nano.to_string(),
            "endTimeUnixNano": end_ns.to_string(),
            "attributes": [
                { "key": "job.id", "value": { "stringValue": span.job_id.to_string() } },
                { "key": "job.status", "value": { "stringValue": status_str } },
                { "key": "job.steps", "value": { "intValue": span.steps.to_string() } },
                { "key": "job.attempts", "value": { "intValue": span.attempts.to_string() } },
            ],
            "status": status_obj,
        });
        if let Some(ref parent) = span.trace_ctx.parent_span_id {
            obj["parentSpanId"] = json!(parent);
        }
        span_values.push(obj);
    }

    json!({
        "resourceSpans": [{
            "resource": {
                "attributes": [{
                    "key": "service.name",
                    "value": { "stringValue": service_name },
                }],
            },
            "scopeSpans": [{
                "scope": {
                    "name": "fluxwright-server",
                    "version": env!("CARGO_PKG_VERSION"),
                },
                "spans": span_values,
            }],
        }],
    })
}

#[derive(Clone)]
pub(crate) struct OtlpExporter {
    tx: mpsc::Sender<OtlpSpanData>,
    flush_tx: mpsc::UnboundedSender<oneshot::Sender<()>>,
}

impl OtlpExporter {
    pub(crate) fn new(endpoint: &str, service_name: String) -> Result<Self, String> {
        let (ep, traces_path) = parse_otlp_endpoint(endpoint)?;
        let svc = if service_name.trim().is_empty() {
            "fluxwright-server".to_string()
        } else {
            service_name
        };

        let (tx, rx) = mpsc::channel(OTLP_CHANNEL_CAPACITY);
        let (flush_tx, flush_rx) = mpsc::unbounded_channel();

        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(run_exporter(ep, traces_path, svc, rx, flush_rx));
        }

        Ok(Self { tx, flush_tx })
    }

    pub(crate) fn try_record(&self, span: OtlpSpanData) {
        if let Err(err) = self.tx.try_send(span) {
            tracing::debug!(error = %err, "OTLP trace channel full or closed; dropping span");
        }
    }

    pub(crate) async fn flush(&self, timeout: Duration) {
        if timeout.is_zero() {
            return;
        }
        let (ack_tx, ack_rx) = oneshot::channel();
        if self.flush_tx.send(ack_tx).is_ok() {
            let _ = tokio::time::timeout(timeout, ack_rx).await;
        }
    }
}

async fn run_exporter(
    endpoint: HttpEndpoint,
    traces_path: String,
    service_name: String,
    mut rx: mpsc::Receiver<OtlpSpanData>,
    mut flush_rx: mpsc::UnboundedReceiver<oneshot::Sender<()>>,
) {
    let mut batch: Vec<OtlpSpanData> = Vec::with_capacity(OTLP_FLUSH_BATCH_SIZE);
    let timer = tokio::time::sleep(OTLP_FLUSH_INTERVAL);
    tokio::pin!(timer);

    loop {
        tokio::select! {
            maybe_span = rx.recv() => {
                match maybe_span {
                    Some(span) => {
                        if batch.is_empty() {
                            timer.as_mut().reset(tokio::time::Instant::now() + OTLP_FLUSH_INTERVAL);
                        }
                        batch.push(span);
                        while batch.len() < OTLP_FLUSH_BATCH_SIZE {
                            match rx.try_recv() {
                                Ok(next) => batch.push(next),
                                Err(_) => break,
                            }
                        }
                        if batch.len() >= OTLP_FLUSH_BATCH_SIZE {
                            flush_batch(&endpoint, &traces_path, &service_name, &mut batch).await;
                        }
                    }
                    None => {
                        flush_batch(&endpoint, &traces_path, &service_name, &mut batch).await;
                        break;
                    }
                }
            }
            _ = &mut timer, if !batch.is_empty() => {
                flush_batch(&endpoint, &traces_path, &service_name, &mut batch).await;
            }
            Some(ack) = flush_rx.recv() => {
                loop {
                    while batch.len() < OTLP_FLUSH_BATCH_SIZE {
                        match rx.try_recv() {
                            Ok(next) => batch.push(next),
                            Err(_) => break,
                        }
                    }
                    if batch.is_empty() {
                        break;
                    }
                    flush_batch(&endpoint, &traces_path, &service_name, &mut batch).await;
                }
                let _ = ack.send(());
            }
        }
    }
}

async fn flush_batch(
    endpoint: &HttpEndpoint,
    traces_path: &str,
    service_name: &str,
    batch: &mut Vec<OtlpSpanData>,
) {
    if batch.is_empty() {
        return;
    }
    let spans = std::mem::take(batch);
    let payload = build_export_payload(service_name, &spans);
    let Ok(body) = serde_json::to_vec(&payload) else {
        tracing::warn!("failed to serialize OTLP trace batch");
        return;
    };
    let headers = [("Content-Type", "application/json")];
    match post_bytes(
        endpoint,
        Some(traces_path),
        &headers,
        &body,
        OTLP_REQUEST_TIMEOUT,
        SMALL_HTTP_RESPONSE_BYTES,
        false,
    )
    .await
    {
        Ok((status, _)) if (200..300).contains(&status) => {
            tracing::debug!(spans = spans.len(), status, "exported OTLP trace batch");
        }
        Ok((status, _)) => {
            tracing::warn!(
                spans = spans.len(),
                host = %endpoint.host,
                status,
                "OTLP collector returned non-2xx status"
            );
        }
        Err(err) => {
            tracing::warn!(
                spans = spans.len(),
                host = %endpoint.host,
                error = %err,
                "OTLP trace export failed"
            );
        }
    }
}
