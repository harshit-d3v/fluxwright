//! A bounded, authenticated HTTP job API over one local Fluxwright fleet.
//!
//! Exposes job submission, lifecycle management, Prometheus metrics, OpenTelemetry
//! tracing spans per job, and a live fleet dashboard. See `docs/SERVER.md`.

mod cdp_gateway;
mod cluster;
pub mod dashboard;
mod job;
mod journal;
mod live;
pub mod prometheus;
pub mod telemetry;

#[cfg(test)]
mod tests;

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::extract::rejection::JsonRejection;
use axum::extract::{DefaultBodyLimit, Path, Request, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use base64::Engine;
use fluxwright::BrowserEngine;
use serde_json::{json, Value};
use tokio::task::AbortHandle;
use tracing::Instrument;
use uuid::Uuid;

pub use cluster::{spawn_worker, WorkerHandle, WorkerInfo, WorkerStatus, WorkersSummary};
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
    /// A file that keeps accepted jobs across restarts: jobs that had not ended when the server
    /// stopped are queued again, under their old ids, when it starts. A job that was running
    /// runs again from its first step.
    pub journal: Option<std::path::PathBuf>,
    /// Allow `?token=<token>` authentication on `/cdp` and `/json/version` for CDP clients that
    /// cannot send custom WebSocket headers. Disabled by default.
    pub allow_query_token: bool,
}

const DEFAULT_LEASE_TIMEOUT_MS: u64 = 30_000;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("the server token must be 32-256 printable ASCII characters without whitespace")]
    InvalidToken,
    #[error("max_jobs must be between 1 and 4096")]
    InvalidCapacity,
    #[error("the job journal could not be opened: {0}")]
    Journal(std::io::Error),
    #[error("invalid coordinator configuration: {0}")]
    InvalidCoordinator(String),
}

impl ServerConfig {
    pub fn new(token: String) -> Result<Self, ConfigError> {
        if !(32..=256).contains(&token.len()) || !token.bytes().all(|c| c.is_ascii_graphic()) {
            return Err(ConfigError::InvalidToken);
        }
        Ok(Self {
            token,
            max_jobs: 128,
            journal: None,
            allow_query_token: false,
        })
    }
}

#[derive(Clone)]
pub struct HttpServer {
    inner: Arc<Inner>,
}

pub(crate) struct Inner {
    engine: Option<BrowserEngine>,
    token: Vec<u8>,
    max_jobs: usize,
    allow_query_token: bool,
    lease_ms: AtomicU64,
    lease_notify: tokio::sync::Notify,
    shutdown_tx: tokio::sync::watch::Sender<bool>,
    active_cdp_sessions: AtomicUsize,
    total_cdp_sessions: AtomicU64,
    live: live::LiveRegistry,
    registry: Mutex<Registry>,
    journal: Option<journal::Journal>,
}

struct Registry {
    accepting: bool,
    jobs: VecDeque<Record>,
    workers: HashMap<String, cluster::WorkerRecord>,
    succeeded_jobs: u64,
    failed_jobs: u64,
    cancelled_jobs: u64,
    expired_leases: u64,
}

struct Record {
    job: Job,
    request: JobRequest,
    abort: Option<AbortHandle>,
    worker: Option<String>,
    lease_until: Option<u64>,
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
    pub(crate) fn lease_ms(&self) -> u64 {
        self.lease_ms.load(Ordering::Relaxed)
    }

    /// Expires overdue worker leases under the registry lock, runs `f`, and flushes newly
    /// terminal job IDs to the journal once the lock is dropped.
    pub(crate) fn with_registry<T>(&self, f: impl FnOnce(&mut Registry, u64) -> T) -> T {
        let now = now_ms();
        let (out, expired) = {
            let mut registry = self.registry.lock().unwrap();
            let expired = cluster::expire_leases(&mut registry, now);
            (f(&mut registry, now), expired)
        };
        for id in expired {
            self.journal_done(id);
        }
        out
    }

    pub async fn prometheus_text(&self) -> String {
        let fleet = match &self.engine {
            Some(engine) => engine.metrics().await,
            None => Default::default(),
        };
        let lease_ms = self.lease_ms();
        let metrics = self.with_registry(|reg, now| {
            let mut queued = 0;
            let mut running = 0;
            for r in &reg.jobs {
                match r.job.status {
                    JobStatus::Queued => queued += 1,
                    JobStatus::Running => running += 1,
                    _ => {}
                }
            }
            let (_, workers_summary) = cluster::workers_snapshot(reg, now, lease_ms);
            ServerMetrics {
                fleet,
                queued_jobs: queued,
                running_jobs: running,
                succeeded_jobs: reg.succeeded_jobs,
                failed_jobs: reg.failed_jobs,
                cancelled_jobs: reg.cancelled_jobs,
                max_jobs: self.max_jobs,
                online_workers: workers_summary.online,
                draining_workers: workers_summary.draining,
                stale_workers: workers_summary.stale,
                expired_leases: workers_summary.expired_leases,
                active_cdp_sessions: self.active_cdp_sessions.load(Ordering::Relaxed),
                total_cdp_sessions: self.total_cdp_sessions.load(Ordering::Relaxed),
            }
        });
        metrics.to_prometheus_text()
    }

    pub async fn dashboard_snapshot(&self) -> Value {
        let fleet = match &self.engine {
            Some(engine) => engine.metrics().await,
            None => Default::default(),
        };
        let lease_ms = self.lease_ms();
        let mode = if self.engine.is_some() {
            "standalone"
        } else {
            "coordinator"
        };
        self.with_registry(|reg, now| {
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
                let id_str = r.job.id.to_string();
                let (is_live, is_paused) = self.live.job_flags(&id_str);
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
                    "worker": r.worker,
                    "has_evidence": r.job.failure_evidence.is_some(),
                    "live": is_live,
                    "paused": is_paused,
                }));
            }
            let (workers, workers_summary) = cluster::workers_snapshot(reg, now, lease_ms);
            json!({
                "mode": mode,
                "fleet": fleet,
                "server": {
                    "queued_jobs": queued,
                    "running_jobs": running,
                    "succeeded_jobs": reg.succeeded_jobs,
                    "failed_jobs": reg.failed_jobs,
                    "cancelled_jobs": reg.cancelled_jobs,
                    "max_jobs": self.max_jobs,
                    "active_cdp_sessions": self.active_cdp_sessions.load(Ordering::Relaxed),
                    "total_cdp_sessions": self.total_cdp_sessions.load(Ordering::Relaxed),
                },
                "workers": workers,
                "workers_summary": workers_summary,
                "live_sessions": self.live.snapshot(),
                "jobs": jobs,
            })
        })
    }
}

impl Inner {
    fn journal_done(&self, id: Uuid) {
        if let Some(journal) = &self.journal {
            journal.done(id);
        }
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
        Self::build(Some(engine), config)
    }

    /// Creates a coordinator HTTP server that accepts and distributes jobs to pulling workers
    /// without running any local browsers.
    pub fn coordinator(config: ServerConfig) -> Result<Self, ConfigError> {
        Self::build(None, config)
    }

    /// Overrides the cluster worker lease expiration window (default 30 seconds).
    pub fn set_lease_timeout(&self, timeout: Duration) {
        let ms = timeout.as_millis().clamp(1, u64::MAX as u128) as u64;
        self.inner.lease_ms.store(ms, Ordering::Relaxed);
        self.inner.lease_notify.notify_one();
    }

    fn build(engine: Option<BrowserEngine>, config: ServerConfig) -> Result<Self, ConfigError> {
        if !(1..=4096).contains(&config.max_jobs) {
            return Err(ConfigError::InvalidCapacity);
        }
        let (journal, unfinished) = match &config.journal {
            Some(path) => {
                let (journal, jobs) = journal::Journal::open(path).map_err(ConfigError::Journal)?;
                (Some(journal), jobs)
            }
            None => (None, Vec::new()),
        };
        let (shutdown_tx, _) = tokio::sync::watch::channel(false);
        let inner = Arc::new(Inner {
            engine,
            token: config.token.into_bytes(),
            max_jobs: config.max_jobs,
            allow_query_token: config.allow_query_token,
            lease_ms: AtomicU64::new(DEFAULT_LEASE_TIMEOUT_MS),
            lease_notify: tokio::sync::Notify::new(),
            shutdown_tx,
            active_cdp_sessions: AtomicUsize::new(0),
            total_cdp_sessions: AtomicU64::new(0),
            live: live::LiveRegistry::default(),
            registry: Mutex::new(Registry {
                accepting: true,
                jobs: VecDeque::new(),
                workers: HashMap::new(),
                succeeded_jobs: 0,
                failed_jobs: 0,
                cancelled_jobs: 0,
                expired_leases: 0,
            }),
            journal,
        });
        // All of them, even past max_jobs: they were accepted once, and new work waits for room.
        let mut registry = inner.registry.lock().unwrap();
        for (id, request) in unfinished {
            match serde_json::from_value::<JobRequest>(request) {
                Ok(request) if request.validate().is_ok() => {
                    tracing::info!(job_id = %id, "queued again from the journal");
                    enqueue(
                        &inner,
                        &mut registry,
                        id,
                        request,
                        JobTraceContext::from_headers(None),
                    );
                }
                // Written by a version with other rules: it cannot run, so it is over.
                _ => inner.journal_done(id),
            }
        }
        drop(registry);
        if inner.engine.is_none() {
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                let weak = Arc::downgrade(&inner);
                handle.spawn(async move {
                    loop {
                        let Some(inner) = weak.upgrade() else {
                            break;
                        };
                        let accepting = inner.with_registry(|reg, _| reg.accepting);
                        if !accepting {
                            break;
                        }
                        let interval =
                            Duration::from_millis((inner.lease_ms() / 2).clamp(25, 5_000));
                        tokio::select! {
                            _ = tokio::time::sleep(interval) => {}
                            _ = inner.lease_notify.notified() => {}
                        }
                    }
                });
            }
        }
        Ok(Self { inner })
    }

    pub fn router(&self) -> Router {
        let mut small = Router::new()
            .route("/v1/jobs", post(submit))
            .route("/v1/jobs/{id}", get(job_status).delete(cancel))
            .route("/v1/stats", get(stats))
            .route("/v1/events", get(dashboard::dashboard_events))
            .route("/metrics", get(metrics_endpoint));

        let mut protected = Router::new();
        // Worker cluster endpoints belong strictly to coordinators (`engine == None`); exposing
        // them on a standalone `serve` instance would lease out jobs that it also runs locally.
        if self.inner.engine.is_none() {
            small = small
                .route("/v1/workers", get(cluster::list_workers))
                .route("/v1/workers/{worker}", delete(cluster::remove_worker))
                .route("/v1/workers/{worker}/drain", post(cluster::drain_worker))
                .route("/v1/workers/{worker}/lease", post(cluster::lease))
                .route("/v1/workers/{worker}/heartbeat", post(cluster::heartbeat))
                .route("/v1/workers/{worker}/release", post(cluster::release));
            protected = protected
                .route("/v1/jobs/{id}/result", post(cluster::result))
                .layer(DefaultBodyLimit::max(MAX_RESULT_BYTES * 2 + MAX_REQUEST_BYTES));
        }

        let mut protected = small
            .layer(DefaultBodyLimit::max(MAX_REQUEST_BYTES))
            .merge(protected);
        let mut live_ws = Router::new();
        if self.inner.engine.is_some() {
            protected = protected
                .route("/cdp", get(cdp_gateway::ws_handler))
                .route("/json/version", get(cdp_gateway::json_version))
                .route("/v1/live", get(live::list_sessions))
                .route("/v1/live/{id}/ticket", post(live::create_ticket))
                .route("/v1/jobs/{id}/pause", post(live::pause_job))
                .route("/v1/jobs/{id}/resume", post(live::resume_job));
            live_ws = Router::new()
                .route("/v1/live/{id}/ws", get(live::ws_handler))
                .route_layer(middleware::from_fn_with_state(
                    self.inner.clone(),
                    live::authorize_ws,
                ));
        }
        let protected = protected.route_layer(middleware::from_fn_with_state(
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
            .merge(live_ws)
            .fallback(|| async {
                ApiError::new(StatusCode::NOT_FOUND, "not_found", "endpoint not found")
            })
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
                    record.worker = None;
                    record.lease_until = None;
                    newly_cancelled += 1;
                    if let Some(abort) = record.abort.take() {
                        abort.abort();
                    }
                }
            }
            registry.cancelled_jobs += newly_cancelled;
        }
        self.inner.lease_notify.notify_one();
        let _ = self.inner.shutdown_tx.send(true);
        match &self.inner.engine {
            Some(engine) => engine.shutdown(grace).await,
            None => Ok(()),
        }
    }
}

/// The `Authorization` header by default; `/cdp` and `/json/version` also accept `?token=` when
/// `ServerConfig::allow_query_token` is explicitly enabled for CDP clients that cannot set headers.
async fn authorize(State(inner): State<Arc<Inner>>, request: Request, next: Next) -> Response {
    let header_valid = request
        .headers()
        .get(header::AUTHORIZATION)
        // RFC 9110: the auth scheme is case-insensitive.
        .and_then(|value| value.as_bytes().split_at_checked(7))
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case(b"Bearer "))
        .is_some_and(|(_, supplied)| same_token(supplied, &inner.token));

    let path = request.uri().path();
    let query_valid = !header_valid
        && inner.allow_query_token
        && (path == "/cdp" || path == "/json/version")
        && request
            .uri()
            .query()
            .and_then(cdp_gateway::extract_query_token)
            .is_some_and(|supplied| same_token(&supplied, &inner.token));

    if !header_valid && !query_valid {
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
    payload: Result<Json<Value>, JsonRejection>,
) -> Result<Response, ApiError> {
    // Kept as sent as well, for the journal: the request type only reads, it does not write.
    let Json(raw) =
        payload.map_err(|e| ApiError::new(e.status(), "invalid_request", e.body_text()))?;
    let request: JobRequest = serde_json::from_value(raw.clone()).map_err(|e| {
        ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "invalid_request", e.to_string())
    })?;
    request
        .validate()
        .map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, "invalid_request", e))?;

    let traceparent = headers.get("traceparent").and_then(|v| v.to_str().ok());
    let trace_ctx = JobTraceContext::from_headers(traceparent);
    let outbound_tp = trace_ctx.to_traceparent();

    let job = match inner.with_registry(|registry, _| -> Result<Job, Response> {
        if !registry.accepting {
            return Err(ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "shutting_down",
                "the server is shutting down",
            )
            .into_response());
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
            return Err(error);
        }

        let id = Uuid::new_v4();
        // Before the worker starts, so a job is never running without being on file.
        if let Some(journal) = &inner.journal {
            journal.submitted(id, &raw);
        }
        Ok(enqueue(&inner, registry, id, request, trace_ctx))
    }) {
        Ok(job) => job,
        Err(error_response) => return Ok(error_response),
    };
    let id = job.id;

    let mut response = (StatusCode::ACCEPTED, Json(job)).into_response();
    response.headers_mut().insert(
        header::LOCATION,
        HeaderValue::from_str(&format!("/v1/jobs/{id}")).expect("UUID is a valid header"),
    );
    if let Ok(tp_val) = HeaderValue::from_str(&outbound_tp) {
        response
            .headers_mut()
            .insert(header::HeaderName::from_static("traceparent"), tp_val);
    }
    Ok(response)
}

/// Registers a job as queued and starts its local worker if running with an engine.
/// In coordinator mode, the job waits in `Queued` state for a remote worker lease.
fn enqueue(
    inner: &Arc<Inner>,
    registry: &mut Registry,
    id: Uuid,
    request: JobRequest,
    trace_ctx: JobTraceContext,
) -> Job {
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
        trace_id: Some(trace_ctx.trace_id.clone()),
        failure_evidence: None,
    };
    if let Some(engine) = inner.engine.clone() {
        let weak = Arc::downgrade(inner);
        let worker_state = weak.clone();
        let req_clone = request.clone();
        let task = tokio::spawn(async move {
            let progress = Arc::new(Mutex::new(ExecutionProgress::default()));
            execute_job(engine, req_clone, Some(worker_state), id, trace_ctx, 0, progress).await
        });
        registry.jobs.push_back(Record {
            job: job.clone(),
            request,
            abort: Some(task.abort_handle()),
            worker: None,
            lease_until: None,
        });

        // A separate monitor records panics as failures, updates lifetime counters, and waits for a
        // cancelled task to actually drop its lease. Its Weak reference does not keep a discarded server alive.
        tokio::spawn(async move {
            let outcome = task.await;
            if let Some(inner) = weak.upgrade() {
                finish_job(&inner, id, outcome);
            }
        });
    } else {
        registry.jobs.push_back(Record {
            job: job.clone(),
            request,
            abort: None,
            worker: None,
            lease_until: None,
        });
    }
    job
}

/// Records how a job's worker ended, and counts it, unless the job already ended (a cancel).
fn finish_job(
    inner: &Inner,
    id: Uuid,
    outcome: Result<fluxwright::Result<Vec<Value>>, tokio::task::JoinError>,
) {
    let finished_status = inner.with_registry(|registry, now| {
        let record = registry.find_mut(id)?;
        record.abort = None;
        if record.job.status.terminal() {
            return None; // Cancellation wins over a result arriving at the same instant.
        }
        record.job.finished_at = Some(now);
        match outcome {
            Ok(Ok(results)) => {
                record.job.status = JobStatus::Succeeded;
                record.job.results = Some(results);
            }
            Ok(Err(error)) => {
                record.job.status = JobStatus::Failed;
                record.job.error = Some(bounded_error(&error.to_string()));
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
            }
        }
        let status = record.job.status;
        match status {
            JobStatus::Succeeded => registry.succeeded_jobs += 1,
            _ => registry.failed_jobs += 1,
        }
        Some(status)
    });
    if let Some(status) = finished_status {
        inner.journal_done(id);
        tracing::info!(job_id = %id, ?status, "HTTP job finished");
    }
}

#[derive(Clone, Default)]
pub(crate) struct ExecutionProgress {
    pub(crate) attempts: u32,
    pub(crate) step_index: Option<usize>,
    pub(crate) failure_evidence: Option<FailureEvidence>,
}

pub(crate) async fn execute_job(
    engine: BrowserEngine,
    request: JobRequest,
    inner: Option<Weak<Inner>>,
    id: Uuid,
    trace_ctx: JobTraceContext,
    initial_attempts: u32,
    progress: Arc<Mutex<ExecutionProgress>>,
) -> fluxwright::Result<Vec<Value>> {
    let timeout = Duration::from_millis(request.timeout_ms);
    let priority_label = match request.priority {
        JobPriority::Low => "low",
        JobPriority::Normal => "normal",
        JobPriority::High => "high",
    };
    let options = request.options();
    let steps = Arc::new(request.steps);
    let attempt_count = Arc::new(std::sync::atomic::AtomicU32::new(0));

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
                let progress = progress.clone();
                let attempt_count = attempt_count.clone();
                let trace_id = trace_ctx.trace_id.clone();
                async move {
                    let n = attempt_count.fetch_add(1, Ordering::Relaxed);
                    let total_attempts = if n == 0 && initial_attempts > 0 {
                        initial_attempts
                    } else {
                        initial_attempts.saturating_add(n).max(n + 1)
                    };
                    {
                        let mut p = progress.lock().unwrap();
                        p.attempts = total_attempts;
                        p.step_index = None;
                        p.failure_evidence = None;
                    }
                    if let Some(weak) = &inner {
                        update(weak, id, |job| {
                            job.status = JobStatus::Running;
                            job.started_at.get_or_insert_with(now_ms);
                            job.attempts = total_attempts;
                            job.step_index = None;
                            job.failure_evidence = None;
                        });
                    }

                    let mut net_rx = page.subscribe_network();
                    let mut log_rx = page.subscribe_logs();
                    let mut accumulator = EvidenceAccumulator::new();

                    // SSRF guard: a job must not reach link-local hosts (cloud metadata)
                    // through a goto, redirect or subresource. FLUXWRIGHT_SERVER_ANY_URL=1
                    // turns it off for trusted workloads.
                    if std::env::var_os("FLUXWRIGHT_SERVER_ANY_URL").is_none() {
                        fluxwright::netguard::guard_requests(&page).await?;
                    }

                    let (pause_tx, mut pause_rx) = tokio::sync::watch::channel(false);
                    let pause_reason = Arc::new(Mutex::new(None));
                    let mut url_rx = page.subscribe_url();
                    let live_guard = inner.as_ref().and_then(|w| w.upgrade()).map(|inn| {
                        inn.live.register_job(
                            id.to_string(),
                            page.cdp_connection(),
                            page.target_id().to_string(),
                            url_rx.borrow().clone(),
                            now_ms(),
                            pause_tx.clone(),
                            pause_reason.clone(),
                        )
                    });

                    let mut results = Vec::with_capacity(steps.len());
                    let mut bytes = 2;
                    let mut failure_screenshot: Option<String> = None;
                    let mut run_error: Option<fluxwright::Error> = None;
                    let mut log_closed = false;

                    for (index, step) in steps.iter().enumerate() {
                        drain_evidence(&mut net_rx, &mut log_rx, &mut accumulator);
                        if let Some(lg) = &live_guard {
                            lg.set_url(url_rx.borrow_and_update().clone());
                        }
                        progress.lock().unwrap().step_index = Some(index);
                        if let Some(weak) = &inner {
                            update(weak, id, |job| job.step_index = Some(index));
                        }
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
                        if *pause_rx.borrow() {
                            if let Err(err) = wait_while_paused(
                                &mut pause_rx,
                                &mut net_rx,
                                &mut log_rx,
                                &mut url_rx,
                                live_guard.as_ref(),
                                &mut accumulator,
                                &mut log_closed,
                                step_budget,
                            )
                            .await
                            {
                                run_error = Some(err);
                                break;
                            }
                        }

                        let outcome = if let Step::PauseForHuman { message, timeout_ms } = step {
                            if let Some(lg) = &live_guard {
                                lg.set_pause_reason(Some(
                                    message
                                        .clone()
                                        .unwrap_or_else(|| "Paused for human action".into()),
                                ));
                            }
                            let _ = pause_tx.send(true);
                            let pause_budget = timeout_ms
                                .map(Duration::from_millis)
                                .map(|d| d.min(step_budget))
                                .unwrap_or(step_budget);
                            let wait_res = wait_while_paused(
                                &mut pause_rx,
                                &mut net_rx,
                                &mut log_rx,
                                &mut url_rx,
                                live_guard.as_ref(),
                                &mut accumulator,
                                &mut log_closed,
                                pause_budget,
                            )
                            .await;
                            let _ = pause_tx.send(false);
                            if let Some(lg) = &live_guard {
                                lg.set_pause_reason(None);
                            }
                            wait_res.map(|()| json!({ "resumed": true }))
                        } else {
                            let step_exec = tokio::time::timeout(
                                step_budget,
                                step.execute(&page).instrument(step_span),
                            );
                            tokio::pin!(step_exec);
                            // Evidence is read while the step runs. Read only between steps, a page
                            // logging in a loop kept everything it logged in memory until the step ended.
                            loop {
                                tokio::select! {
                                    done = &mut step_exec => {
                                        break done.map_err(|_| fluxwright::Error::JobTimeout).and_then(|r| r);
                                    }
                                    Ok(()) = url_rx.changed() => {
                                        if let Some(lg) = &live_guard {
                                            lg.set_url(url_rx.borrow_and_update().clone());
                                        }
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
                            }
                        };

                        drain_evidence(&mut net_rx, &mut log_rx, &mut accumulator);
                        if let Some(lg) = &live_guard {
                            lg.set_url(url_rx.borrow_and_update().clone());
                        }

                        match outcome {
                            Ok(result) => {
                                bytes += json_len(&result)
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
                        if !evidence.is_empty() {
                            progress.lock().unwrap().failure_evidence = Some(evidence.clone());
                            if let Some(weak) = &inner {
                                update(weak, id, |job| {
                                    job.failure_evidence = Some(evidence);
                                });
                            }
                        }
                    }

                    // Drop schedules context disposal even if the enclosing deadline/cancellation fires.
                    // Do not await close() here: cancellation halfway through an async close would depend
                    // on that close's cancellation safety.
                    drop(live_guard);
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

#[allow(clippy::too_many_arguments)]
async fn wait_while_paused(
    pause_rx: &mut tokio::sync::watch::Receiver<bool>,
    net_rx: &mut tokio::sync::mpsc::UnboundedReceiver<fluxwright::NetEvent>,
    log_rx: &mut tokio::sync::mpsc::UnboundedReceiver<fluxwright::LogEntry>,
    url_rx: &mut tokio::sync::watch::Receiver<String>,
    live_guard: Option<&live::LiveGuard>,
    accumulator: &mut EvidenceAccumulator,
    log_closed: &mut bool,
    budget: Duration,
) -> fluxwright::Result<()> {
    let deadline = tokio::time::sleep(budget);
    tokio::pin!(deadline);
    while *pause_rx.borrow() {
        tokio::select! {
            _ = &mut deadline => return Err(fluxwright::Error::JobTimeout),
            res = pause_rx.changed() => {
                if res.is_err() {
                    break;
                }
            }
            Ok(()) = url_rx.changed() => {
                if let Some(lg) = live_guard {
                    lg.set_url(url_rx.borrow_and_update().clone());
                }
            }
            Some(event) = net_rx.recv() => accumulator.push_net(event),
            Some(entry) = log_rx.recv(), if !*log_closed => {
                accumulator.push_log(entry);
                if accumulator.console_full() {
                    log_rx.close();
                    *log_closed = true;
                }
            }
        }
    }
    Ok(())
}

/// Serialized byte length of a JSON-serializable value without materializing it.
pub(crate) fn json_len<T: serde::Serialize + ?Sized>(value: &T) -> usize {
    struct Tally(usize);
    impl std::io::Write for Tally {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0 += buf.len();
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut tally = Tally(0);
    let _ = serde_json::to_writer(&mut tally, value);
    tally.0
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

#[derive(Default)]
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
            .chain(
                page.page_errors()
                    .into_iter()
                    .map(|err| ("error".to_string(), format_page_error(&err))),
            );
        for (kind, text) in logged {
            if self.console_full() {
                break;
            }
            let text = job::truncate_bytes(&text, MAX_CONSOLE_TEXT_BYTES);
            if !self
                .console
                .iter()
                .any(|c| c.kind == kind && c.text == text)
            {
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
    inner
        .with_registry(|registry, _| {
            registry
                .jobs
                .iter()
                .find(|r| r.job.id == id)
                .map(|r| r.job.clone())
        })
        .map(Json)
        .ok_or_else(not_found)
}

async fn cancel(
    State(inner): State<Arc<Inner>>,
    Path(id): Path<String>,
) -> Result<Json<Job>, ApiError> {
    let id = parse_id(&id)?;
    let (job_clone, abort, finished) = inner.with_registry(|registry, now| -> Result<_, ApiError> {
        let record = registry.find_mut(id).ok_or_else(not_found)?;
        if record.job.status == JobStatus::Cancelled {
            return Ok((record.job.clone(), None, false));
        }
        if record.job.status.terminal() {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "already_finished",
                "the job already finished",
            ));
        }
        record.job.status = JobStatus::Cancelled;
        record.job.finished_at = Some(now);
        record.worker = None;
        record.lease_until = None;
        let job = record.job.clone();
        let abort = record.abort.clone();
        registry.cancelled_jobs += 1;
        Ok((job, abort, true))
    })?;
    if let Some(abort) = abort {
        abort.abort();
    }
    if finished {
        // A job the client cancelled is over. One the shutdown cancels is not: it runs again.
        inner.journal_done(id);
    }
    Ok(Json(job_clone))
}

async fn stats(State(inner): State<Arc<Inner>>) -> Json<fluxwright::MetricsSnapshot> {
    match &inner.engine {
        Some(engine) => Json(engine.metrics().await),
        None => Json(Default::default()),
    }
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

#[derive(Debug)]
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
