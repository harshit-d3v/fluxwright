//! A bounded, authenticated HTTP job API over one local Fluxwright fleet.
//!
//! Exposes job submission, lifecycle management, Prometheus metrics, OpenTelemetry
//! tracing spans per job, and a live fleet dashboard. See `docs/SERVER.md`.

pub mod dashboard;
mod job;
pub mod prometheus;
pub mod telemetry;

#[cfg(test)]
mod tests;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::extract::rejection::JsonRejection;
use axum::extract::{DefaultBodyLimit, Path, Request, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine;
use fluxwright::BrowserEngine;
use serde_json::{json, Value};
use tokio::task::AbortHandle;
use tracing::Instrument;
use uuid::Uuid;

pub use dashboard::dashboard_html;
pub use job::{
    ConsoleEntry, FailureEvidence, Job, JobPriority, JobRequest, JobStatus, NavigationWait,
    NetworkEntry, Step, MAX_CONSOLE_ENTRIES, MAX_CONSOLE_TEXT_BYTES, MAX_NETWORK_ENTRIES,
    MAX_NETWORK_URL_BYTES, MAX_RESULT_BYTES, MAX_SCREENSHOT_BYTES,
};
pub use prometheus::ServerMetrics;
pub use telemetry::JobTraceContext;

pub const MAX_REQUEST_BYTES: usize = 64 * 1024;

/// Rows in each dashboard snapshot, newest first. All 4096 retained jobs were up to 9 MB a
/// second per open tab, built under the lock every job needs to report its progress.
const DASHBOARD_JOBS: usize = 100;

/// No Debug implementation: the bearer token must not appear in logs.
pub struct ServerConfig {
    token: String,
    /// Bounds running, queued and retained completed jobs together. Completed jobs are evicted
    /// oldest-first to admit new work; active jobs are never evicted.
    pub max_jobs: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("the server token must be 32-256 printable ASCII characters without whitespace")]
    InvalidToken,
    #[error("max_jobs must be between 1 and 4096")]
    InvalidCapacity,
}

impl ServerConfig {
    pub fn new(token: String) -> Result<Self, ConfigError> {
        if !(32..=256).contains(&token.len()) || !token.bytes().all(|c| c.is_ascii_graphic()) {
            return Err(ConfigError::InvalidToken);
        }
        Ok(Self {
            token,
            max_jobs: 128,
        })
    }
}

#[derive(Clone)]
pub struct HttpServer {
    inner: Arc<Inner>,
}

pub(crate) struct Inner {
    engine: BrowserEngine,
    token: Vec<u8>,
    max_jobs: usize,
    registry: Mutex<Registry>,
}

struct Registry {
    accepting: bool,
    jobs: VecDeque<Record>,
    succeeded_jobs: u64,
    failed_jobs: u64,
    cancelled_jobs: u64,
}

struct Record {
    job: Job,
    abort: Option<AbortHandle>,
}

impl Registry {
    fn find_mut(&mut self, id: Uuid) -> Option<&mut Record> {
        self.jobs.iter_mut().find(|r| r.job.id == id)
    }

    fn make_room(&mut self, max: usize) -> bool {
        while self.jobs.len() >= max {
            let Some(index) = self
                .jobs
                .iter()
                .position(|r| r.job.status.terminal() && r.abort.is_none())
            else {
                return false;
            };
            self.jobs.remove(index);
        }
        true
    }
}

impl Inner {
    pub async fn prometheus_text(&self) -> String {
        let fleet = self.engine.metrics().await;
        let metrics = {
            let reg = self.registry.lock().unwrap();
            let mut queued = 0;
            let mut running = 0;
            for r in &reg.jobs {
                match r.job.status {
                    JobStatus::Queued => queued += 1,
                    JobStatus::Running => running += 1,
                    _ => {}
                }
            }
            ServerMetrics {
                fleet,
                queued_jobs: queued,
                running_jobs: running,
                succeeded_jobs: reg.succeeded_jobs,
                failed_jobs: reg.failed_jobs,
                cancelled_jobs: reg.cancelled_jobs,
                max_jobs: self.max_jobs,
            }
        };
        metrics.to_prometheus_text()
    }

    pub async fn dashboard_snapshot(&self) -> Value {
        let fleet = self.engine.metrics().await;
        let reg = self.registry.lock().unwrap();
        let mut queued = 0;
        let mut running = 0;
        let mut jobs = Vec::with_capacity(reg.jobs.len().min(DASHBOARD_JOBS));
        for r in reg.jobs.iter().rev() {
            match r.job.status {
                JobStatus::Queued => queued += 1,
                JobStatus::Running => running += 1,
                _ => {}
            }
            if jobs.len() == DASHBOARD_JOBS {
                continue;
            }
            jobs.push(json!({
                "id": r.job.id,
                "status": r.job.status,
                "submitted_at": r.job.submitted_at,
                "started_at": r.job.started_at,
                "finished_at": r.job.finished_at,
                "attempts": r.job.attempts,
                "step_index": r.job.step_index,
                "error": r.job.error,
                "trace_id": r.job.trace_id,
                "has_evidence": r.job.failure_evidence.is_some(),
            }));
        }
        json!({
            "fleet": fleet,
            "server": {
                "queued_jobs": queued,
                "running_jobs": running,
                "succeeded_jobs": reg.succeeded_jobs,
                "failed_jobs": reg.failed_jobs,
                "cancelled_jobs": reg.cancelled_jobs,
                "max_jobs": self.max_jobs,
            },
            "jobs": jobs,
        })
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        // Workers and completion monitors hold Weak<Inner>, so dropping the last server/router
        // aborts outstanding jobs rather than keeping the fleet alive forever.
        let registry = self.registry.get_mut().unwrap_or_else(|e| e.into_inner());
        for record in &registry.jobs {
            if let Some(abort) = &record.abort {
                abort.abort();
            }
        }
    }
}

impl HttpServer {
    pub fn new(engine: BrowserEngine, config: ServerConfig) -> Result<Self, ConfigError> {
        if !(1..=4096).contains(&config.max_jobs) {
            return Err(ConfigError::InvalidCapacity);
        }
        Ok(Self {
            inner: Arc::new(Inner {
                engine,
                token: config.token.into_bytes(),
                max_jobs: config.max_jobs,
                registry: Mutex::new(Registry {
                    accepting: true,
                    jobs: VecDeque::new(),
                    succeeded_jobs: 0,
                    failed_jobs: 0,
                    cancelled_jobs: 0,
                }),
            }),
        })
    }

    pub fn router(&self) -> Router {
        let protected = Router::new()
            .route("/v1/jobs", post(submit))
            .route("/v1/jobs/{id}", get(job_status).delete(cancel))
            .route("/v1/stats", get(stats))
            .route("/v1/events", get(dashboard::dashboard_events))
            .route("/metrics", get(metrics_endpoint))
            .route_layer(middleware::from_fn_with_state(
                self.inner.clone(),
                authorize,
            ));

        Router::new()
            // Liveness only: does not expose fleet state or launch Chrome.
            .route(
                "/healthz",
                get(|| async { Json(json!({ "status": "ok" })) }),
            )
            .route("/dashboard", get(dashboard::dashboard_html))
            .route("/", get(dashboard::dashboard_html))
            .merge(protected)
            .fallback(|| async {
                ApiError::new(StatusCode::NOT_FOUND, "not_found", "endpoint not found")
            })
            .layer(DefaultBodyLimit::max(MAX_REQUEST_BYTES))
            .layer(middleware::from_fn(response_headers))
            .with_state(self.inner.clone())
    }

    /// Stops admission, cancels active jobs, then gives lease cleanup time to finish. Call this
    /// from the HTTP listener's shutdown signal, before waiting for connections to drain.
    pub async fn shutdown(&self, grace: Duration) -> fluxwright::Result<()> {
        {
            let mut registry = self.inner.registry.lock().unwrap();
            registry.accepting = false;
            let mut newly_cancelled = 0;
            for record in &mut registry.jobs {
                if !record.job.status.terminal() {
                    record.job.status = JobStatus::Cancelled;
                    record.job.finished_at = Some(now_ms());
                    record.job.error = Some("server shutting down".into());
                    newly_cancelled += 1;
                    if let Some(abort) = record.abort.take() {
                        abort.abort();
                    }
                }
            }
            registry.cancelled_jobs += newly_cancelled;
        }
        self.inner.engine.shutdown(grace).await
    }
}

/// The `Authorization` header only: a token in the URL lands in proxy logs, browser history and
/// Referer headers. The dashboard reads its event stream with `fetch`, which can send the header.
async fn authorize(State(inner): State<Arc<Inner>>, request: Request, next: Next) -> Response {
    let valid = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.as_bytes().strip_prefix(b"Bearer "))
        .is_some_and(|supplied| same_token(supplied, &inner.token));

    if !valid {
        let mut response = ApiError::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "a bearer token is required",
        )
        .into_response();
        response
            .headers_mut()
            .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        return response;
    }
    next.run(request).await
}

/// Compare all equal-length bytes rather than returning on the first mismatch. The length is
/// not secret; operators should use a random token with at least 256 bits of entropy.
fn same_token(supplied: &[u8], expected: &[u8]) -> bool {
    if supplied.len() != expected.len() {
        return false;
    }
    supplied
        .iter()
        .zip(expected)
        .fold(0u8, |diff, (a, b)| diff | (a ^ b))
        == 0
}

async fn response_headers(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response
}

async fn submit(
    State(inner): State<Arc<Inner>>,
    headers: axum::http::HeaderMap,
    payload: Result<Json<JobRequest>, JsonRejection>,
) -> Result<Response, ApiError> {
    let Json(request) =
        payload.map_err(|e| ApiError::new(e.status(), "invalid_request", e.body_text()))?;
    request
        .validate()
        .map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, "invalid_request", e))?;

    let traceparent = headers.get("traceparent").and_then(|v| v.to_str().ok());
    let trace_ctx = JobTraceContext::from_headers(traceparent);
    let trace_id = trace_ctx.trace_id.clone();
    let outbound_tp = trace_ctx.to_traceparent();

    let mut registry = inner.registry.lock().unwrap();
    if !registry.accepting {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "shutting_down",
            "the server is shutting down",
        ));
    }
    if !registry.make_room(inner.max_jobs) {
        let mut error = ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "capacity_exceeded",
            "all job slots are busy",
        )
        .into_response();
        error
            .headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
        return Ok(error);
    }

    let id = Uuid::new_v4();
    let job = Job {
        id,
        status: JobStatus::Queued,
        submitted_at: now_ms(),
        started_at: None,
        finished_at: None,
        attempts: 0,
        step_index: None,
        results: None,
        error: None,
        trace_id: Some(trace_id),
        failure_evidence: None,
    };
    let weak = Arc::downgrade(&inner);
    let engine = inner.engine.clone();
    let worker_state = weak.clone();
    let worker_ctx = trace_ctx.clone();
    let task =
        tokio::spawn(async move { execute(engine, request, worker_state, id, worker_ctx).await });
    registry.jobs.push_back(Record {
        job: job.clone(),
        abort: Some(task.abort_handle()),
    });
    drop(registry);

    // A separate monitor records panics as failures, updates lifetime counters, and waits for a
    // cancelled task to actually drop its lease. Its Weak reference does not keep a discarded server alive.
    tokio::spawn(async move {
        let outcome = task.await;
        let Some(inner) = weak.upgrade() else { return };
        let mut registry = inner.registry.lock().unwrap();
        let Some(record) = registry.find_mut(id) else {
            return;
        };
        record.abort = None;
        if record.job.status.terminal() {
            return; // Cancellation wins over a result arriving at the same instant.
        }
        record.job.finished_at = Some(now_ms());
        let final_status = match outcome {
            Ok(Ok(results)) => {
                record.job.status = JobStatus::Succeeded;
                record.job.results = Some(results);
                JobStatus::Succeeded
            }
            Ok(Err(error)) => {
                record.job.status = JobStatus::Failed;
                record.job.error = Some(bounded_error(&error.to_string()));
                JobStatus::Failed
            }
            Err(error) => {
                record.job.status = JobStatus::Failed;
                record.job.error = Some(
                    if error.is_panic() {
                        "job worker panicked"
                    } else {
                        "job worker stopped"
                    }
                    .into(),
                );
                JobStatus::Failed
            }
        };
        // Record is no longer borrowed, so counters can be updated safely
        match final_status {
            JobStatus::Succeeded => registry.succeeded_jobs += 1,
            JobStatus::Failed => registry.failed_jobs += 1,
            _ => {}
        }
        tracing::info!(job_id = %id, status = ?final_status, "HTTP job finished");
    });

    let mut response = (StatusCode::ACCEPTED, Json(job)).into_response();
    response.headers_mut().insert(
        header::LOCATION,
        HeaderValue::from_str(&format!("/v1/jobs/{id}")).expect("UUID is a valid header"),
    );
    if let Ok(tp_val) = HeaderValue::from_str(&outbound_tp) {
        response.headers_mut().insert(
            header::HeaderName::from_static("traceparent"),
            tp_val,
        );
    }
    Ok(response)
}

async fn execute(
    engine: BrowserEngine,
    request: JobRequest,
    inner: Weak<Inner>,
    id: Uuid,
    trace_ctx: JobTraceContext,
) -> fluxwright::Result<Vec<Value>> {
    let timeout = Duration::from_millis(request.timeout_ms);
    let priority_label = match request.priority {
        JobPriority::Low => "low",
        JobPriority::Normal => "normal",
        JobPriority::High => "high",
    };
    let options = request.options();
    let steps = Arc::new(request.steps);

    let job_span = tracing::info_span!(
        "fluxwright.job",
        otel.name = "fluxwright.job",
        trace_id = %trace_ctx.trace_id,
        span_id = %trace_ctx.root_span_id,
        parent_span_id = trace_ctx.parent_span_id.as_deref().unwrap_or(""),
        job.id = %id,
        job.priority = priority_label,
        job.timeout_ms = request.timeout_ms,
        job.retries = request.retries,
    );

    let execution = async {
        let start_instant = tokio::time::Instant::now();
        tokio::time::timeout(
            timeout,
            engine.run(options, move |page| {
                let steps = steps.clone();
                let inner = inner.clone();
                let trace_id = trace_ctx.trace_id.clone();
                async move {
                    update(&inner, id, |job| {
                        job.status = JobStatus::Running;
                        job.started_at.get_or_insert_with(now_ms);
                        job.attempts += 1;
                        job.step_index = None;
                        job.failure_evidence = None;
                    });

                    let mut net_rx = page.subscribe_network();
                    let mut log_rx = page.subscribe_logs();
                    let mut accumulator = EvidenceAccumulator::new();

                    let mut results = Vec::with_capacity(steps.len());
                    let mut bytes = 2;
                    let mut failure_screenshot: Option<String> = None;
                    let mut run_error: Option<fluxwright::Error> = None;

                    for (index, step) in steps.iter().enumerate() {
                        drain_evidence(&mut net_rx, &mut log_rx, &mut accumulator);
                        update(&inner, id, |job| job.step_index = Some(index));
                        let step_span = tracing::info_span!(
                            "fluxwright.step",
                            otel.name = "fluxwright.step",
                            trace_id = %trace_id,
                            job.id = %id,
                            step.index = index,
                            step.action = step.action_name(),
                        );

                        let elapsed = start_instant.elapsed();
                        let remaining = timeout.saturating_sub(elapsed);
                        if remaining.is_zero() {
                            run_error = Some(fluxwright::Error::JobTimeout);
                            break;
                        }

                        // 500 ms of the deadline stay for failure evidence, unless that is all left.
                        let step_budget = match remaining.saturating_sub(Duration::from_millis(500)) {
                            d if d.is_zero() => remaining,
                            d => d,
                        };
                        let step_exec =
                            tokio::time::timeout(step_budget, step.execute(&page).instrument(step_span));
                        tokio::pin!(step_exec);
                        let mut log_closed = false;
                        // Evidence is read while the step runs. Read only between steps, a page
                        // logging in a loop kept everything it logged in memory until the step ended.
                        let outcome = loop {
                            tokio::select! {
                                done = &mut step_exec => {
                                    break done.map_err(|_| fluxwright::Error::JobTimeout).and_then(|r| r);
                                }
                                Some(event) = net_rx.recv() => accumulator.push_net(event),
                                Some(entry) = log_rx.recv(), if !log_closed => {
                                    accumulator.push_log(entry);
                                    if accumulator.console_full() {
                                        // Nothing more is kept: stop the page queueing it.
                                        log_rx.close();
                                        log_closed = true;
                                    }
                                }
                            }
                        };

                        drain_evidence(&mut net_rx, &mut log_rx, &mut accumulator);

                        match outcome {
                            Ok(result) => {
                                bytes += serde_json::to_vec(&result).expect("Value is JSON").len()
                                    + usize::from(index > 0);
                                if bytes > MAX_RESULT_BYTES {
                                    run_error = Some(fluxwright::Error::Other(
                                        "job results exceed 1 MiB".into(),
                                    ));
                                    break;
                                }
                                results.push(result);
                            }
                            Err(err) => {
                                run_error = Some(err);
                                break;
                            }
                        }
                    }

                    if run_error.is_some() {
                        // Finish before the job deadline: the outer timeout would cancel this
                        // block and drop the logs below along with the screenshot. Everything
                        // after the screenshot is synchronous, so a small margin is enough.
                        let screenshot_budget = timeout
                            .saturating_sub(start_instant.elapsed())
                            .saturating_sub(Duration::from_millis(50))
                            .min(Duration::from_secs(5));
                        if let Ok(Ok(screenshot_bytes)) =
                            tokio::time::timeout(screenshot_budget, page.screenshot()).await
                        {
                            if screenshot_bytes.len() <= MAX_SCREENSHOT_BYTES {
                                failure_screenshot = Some(
                                    base64::engine::general_purpose::STANDARD
                                        .encode(screenshot_bytes),
                                );
                            }
                        }

                        drain_evidence(&mut net_rx, &mut log_rx, &mut accumulator);
                        accumulator.sync_page_logs(&page);

                        let evidence = accumulator.finalize(failure_screenshot);

                        update(&inner, id, |job| {
                            if !evidence.is_empty() {
                                job.failure_evidence = Some(evidence);
                            }
                        });
                    }

                    // Drop schedules context disposal even if the enclosing deadline/cancellation fires.
                    // Do not await close() here: cancellation halfway through an async close would depend
                    // on that close's cancellation safety.
                    drop(page);

                    if let Some(err) = run_error {
                        Err(err)
                    } else {
                        Ok(results)
                    }
                }
            }),
        )
        .await
        .map_err(|_| fluxwright::Error::JobTimeout)?
    };

    execution.instrument(job_span).await
}

fn drain_evidence(
    net_rx: &mut tokio::sync::mpsc::UnboundedReceiver<fluxwright::NetEvent>,
    log_rx: &mut tokio::sync::mpsc::UnboundedReceiver<fluxwright::LogEntry>,
    accumulator: &mut EvidenceAccumulator,
) {
    while let Ok(event) = net_rx.try_recv() {
        accumulator.push_net(event);
    }
    while let Ok(entry) = log_rx.try_recv() {
        accumulator.push_log(entry);
    }
}

#[derive(Clone, Default)]
struct EvidenceAccumulator {
    console: Vec<ConsoleEntry>,
    network: Vec<NetworkEntry>,
}

impl EvidenceAccumulator {
    fn new() -> Self {
        Self::default()
    }

    fn push_net(&mut self, event: fluxwright::NetEvent) {
        match event {
            fluxwright::NetEvent::Request(req) => {
                if self.network.len() < MAX_NETWORK_ENTRIES {
                    self.network.push(NetworkEntry {
                        url: job::truncate_bytes(&req.url, MAX_NETWORK_URL_BYTES),
                        method: req.method,
                        status: None,
                        resource_type: Some(req.resource_type),
                        failure: None,
                    });
                }
            }
            fluxwright::NetEvent::Response(resp) => {
                let url = job::truncate_bytes(&resp.url, MAX_NETWORK_URL_BYTES);
                if let Some(entry) = self
                    .network
                    .iter_mut()
                    .rev()
                    .find(|e| e.url == url && e.status.is_none())
                {
                    entry.status = Some(resp.status);
                } else if self.network.len() < MAX_NETWORK_ENTRIES {
                    self.network.push(NetworkEntry {
                        url,
                        method: resp.request.method,
                        status: Some(resp.status),
                        resource_type: Some(resp.request.resource_type),
                        failure: None,
                    });
                }
            }
            fluxwright::NetEvent::Finished(_) => {}
            fluxwright::NetEvent::Failed(req, reason) => {
                let url = job::truncate_bytes(&req.url, MAX_NETWORK_URL_BYTES);
                let failure = Some(job::truncate_bytes(&reason, 512));
                if let Some(entry) = self
                    .network
                    .iter_mut()
                    .rev()
                    .find(|e| e.url == url && e.failure.is_none())
                {
                    entry.failure = failure;
                } else if self.network.len() < MAX_NETWORK_ENTRIES {
                    self.network.push(NetworkEntry {
                        url,
                        method: req.method,
                        status: None,
                        resource_type: Some(req.resource_type),
                        failure,
                    });
                }
            }
        }
    }

    fn console_full(&self) -> bool {
        self.console.len() >= MAX_CONSOLE_ENTRIES
    }

    fn push_log(&mut self, entry: fluxwright::LogEntry) {
        if self.console_full() {
            return;
        }
        match entry {
            fluxwright::LogEntry::Console(msg) => {
                self.console.push(ConsoleEntry {
                    kind: msg.kind,
                    text: job::truncate_bytes(&msg.text, MAX_CONSOLE_TEXT_BYTES),
                });
            }
            fluxwright::LogEntry::Error(err) => {
                let text = format_page_error(&err);
                self.console.push(ConsoleEntry {
                    kind: "error".into(),
                    text: job::truncate_bytes(&text, MAX_CONSOLE_TEXT_BYTES),
                });
            }
        }
    }

    /// Adds what the page logged that the live stream missed. Compared cut to the stored length:
    /// compared whole, every message or stack trace over it was added a second time.
    fn sync_page_logs(&mut self, page: &fluxwright::PageLease) {
        let logged = page
            .console_messages()
            .into_iter()
            .map(|msg| (msg.kind, msg.text))
            .chain(page.page_errors().into_iter().map(|err| ("error".to_string(), format_page_error(&err))));
        for (kind, text) in logged {
            if self.console_full() {
                break;
            }
            let text = job::truncate_bytes(&text, MAX_CONSOLE_TEXT_BYTES);
            if !self.console.iter().any(|c| c.kind == kind && c.text == text) {
                self.console.push(ConsoleEntry { kind, text });
            }
        }
    }

    fn finalize(self, screenshot: Option<String>) -> FailureEvidence {
        let mut evidence = FailureEvidence {
            console: self.console,
            network: self.network,
            screenshot,
        };
        evidence.enforce_bounds();
        evidence
    }
}

fn format_page_error(err: &fluxwright::PageError) -> String {
    if err.name.is_empty() {
        err.message.clone()
    } else if err.stack.is_empty() {
        format!("{}: {}", err.name, err.message)
    } else {
        format!("{}: {}\n{}", err.name, err.message, err.stack)
    }
}

fn update(inner: &Weak<Inner>, id: Uuid, f: impl FnOnce(&mut Job)) {
    if let Some(inner) = inner.upgrade() {
        let mut registry = inner.registry.lock().unwrap();
        if let Some(record) = registry.find_mut(id) {
            if !record.job.status.terminal() {
                f(&mut record.job);
            }
        }
    }
}

async fn job_status(
    State(inner): State<Arc<Inner>>,
    Path(id): Path<String>,
) -> Result<Json<Job>, ApiError> {
    let id = parse_id(&id)?;
    let registry = inner.registry.lock().unwrap();
    let record = registry
        .jobs
        .iter()
        .find(|r| r.job.id == id)
        .ok_or_else(not_found)?;
    Ok(Json(record.job.clone()))
}

async fn cancel(
    State(inner): State<Arc<Inner>>,
    Path(id): Path<String>,
) -> Result<Json<Job>, ApiError> {
    let id = parse_id(&id)?;
    let mut registry = inner.registry.lock().unwrap();
    let (job_clone, abort) = {
        let record = registry.find_mut(id).ok_or_else(not_found)?;
        if record.job.status == JobStatus::Cancelled {
            return Ok(Json(record.job.clone()));
        }
        if record.job.status.terminal() {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "already_finished",
                "the job already finished",
            ));
        }
        record.job.status = JobStatus::Cancelled;
        record.job.finished_at = Some(now_ms());
        (record.job.clone(), record.abort.clone())
    };
    registry.cancelled_jobs += 1;
    if let Some(abort) = abort {
        abort.abort();
    }
    Ok(Json(job_clone))
}

async fn stats(State(inner): State<Arc<Inner>>) -> Json<fluxwright::MetricsSnapshot> {
    Json(inner.engine.metrics().await)
}

async fn metrics_endpoint(State(inner): State<Arc<Inner>>) -> Response {
    let text = inner.prometheus_text().await;
    let mut response = (StatusCode::OK, text).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; version=0.0.4; charset=utf-8"),
    );
    response
}

fn parse_id(id: &str) -> Result<Uuid, ApiError> {
    Uuid::parse_str(id).map_err(|_| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_id",
            "job id must be a UUID",
        )
    })
}

fn not_found() -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "not_found",
        "job not found (it may have expired)",
    )
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn bounded_error(message: &str) -> String {
    message.chars().take(2048).collect()
}

struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl ApiError {
    fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(json!({ "error": { "code": self.code, "message": self.message } })),
        )
            .into_response()
    }
}
