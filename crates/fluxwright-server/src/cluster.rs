//! Coordinator HTTP endpoints and pulling worker loop (experimental in 0.7.0) for running
//! Fluxwright across multiple machines.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use fluxwright::BrowserEngine;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, watch, Notify};
use tokio::task::AbortHandle;
use uuid::Uuid;

use crate::{
    bounded_error, execute_job, json_len, not_found, parse_id, ApiError, ConfigError,
    ExecutionProgress, FailureEvidence, Inner, Job, JobPriority, JobRequest, JobStatus,
    JobTraceContext, MAX_RESULT_BYTES,
};

const MAX_LEASE_BATCH: usize = 64;
/// Coordinator stops adding queued jobs to one lease response past 2 MiB so batches stay fast and
/// never hit the worker's 4 MiB read cap even with JSON escaping overhead.
const MAX_LEASE_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
const MAX_HTTP_RESPONSE_BYTES: usize = 4 * 1024 * 1024 + 65_536;
const HTTP_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// How long shutdown waits for aborted job tasks to stop before deciding which jobs to release.
const TASK_STOP_WAIT: Duration = Duration::from_secs(1);
/// Time a draining worker may spend after its grace deadline: one in-flight result call, the
/// release call, and one pending-result call, plus task stop time and slack.
const SHUTDOWN_REPORT_BUDGET: Duration =
    Duration::from_secs(3 * HTTP_REQUEST_TIMEOUT.as_secs() + TASK_STOP_WAIT.as_secs() + 2);
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(200);
const DEFAULT_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);
const MAX_WORKERS: usize = 1024;

pub(crate) struct WorkerRecord {
    pub(crate) max_jobs: usize,
    pub(crate) registered_at: u64,
    pub(crate) last_seen_at: u64,
    pub(crate) draining: bool,
    pub(crate) succeeded_jobs: u64,
    pub(crate) failed_jobs: u64,
    pub(crate) expired_leases: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerStatus {
    Online,
    Draining,
    Stale,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkerInfo {
    pub worker: String,
    pub status: WorkerStatus,
    pub max_jobs: usize,
    pub active_jobs: usize,
    pub succeeded_jobs: u64,
    pub failed_jobs: u64,
    pub expired_leases: u64,
    pub registered_at: u64,
    pub last_seen_at: u64,
    pub draining: bool,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct WorkersSummary {
    pub total: usize,
    pub online: usize,
    pub draining: usize,
    pub stale: usize,
    pub capacity: usize,
    pub active_jobs: usize,
    pub expired_leases: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LeaseRequest {
    #[serde(default = "default_lease_max")]
    pub(crate) max_jobs: usize,
}

fn default_lease_max() -> usize {
    1
}

#[derive(Serialize, Deserialize)]
pub(crate) struct LeaseResponse {
    pub(crate) jobs: Vec<LeasedJob>,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct LeasedJob {
    pub(crate) id: Uuid,
    pub(crate) request: JobRequest,
    pub(crate) attempts: u32,
    pub(crate) trace_id: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HeartbeatRequest {
    pub(crate) jobs: Vec<HeartbeatItem>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HeartbeatItem {
    pub(crate) id: Uuid,
    pub(crate) step_index: Option<usize>,
    pub(crate) attempts: u32,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct HeartbeatResponse {
    pub(crate) cancelled: Vec<Uuid>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReleaseRequest {
    pub(crate) jobs: Vec<Uuid>,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct ReleaseResponse {
    pub(crate) released: Vec<Uuid>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ResultRequest {
    pub(crate) worker: String,
    pub(crate) status: JobStatus,
    pub(crate) attempts: u32,
    pub(crate) step_index: Option<usize>,
    pub(crate) results: Option<Vec<Value>>,
    pub(crate) error: Option<String>,
    pub(crate) failure_evidence: Option<FailureEvidence>,
}

fn validate_worker(worker: &str) -> Result<(), ApiError> {
    if worker.is_empty()
        || worker.len() > 128
        || worker.chars().any(|c| c.is_control() || c.is_whitespace())
    {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_worker",
            "worker name must be 1-128 non-whitespace characters",
        ));
    }
    Ok(())
}

fn touch_worker<'a>(
    registry: &'a mut crate::Registry,
    worker: &str,
    now: u64,
    max_jobs: Option<usize>,
) -> &'a mut WorkerRecord {
    if !registry.workers.contains_key(worker) && registry.workers.len() >= MAX_WORKERS {
        let evict = registry
            .workers
            .iter()
            .filter(|(id, _)| {
                !registry.jobs.iter().any(|r| {
                    r.job.status == JobStatus::Running && r.worker.as_deref() == Some(id.as_str())
                })
            })
            .min_by_key(|(_, rec)| rec.last_seen_at)
            .map(|(id, _)| id.clone());
        if let Some(id) = evict {
            registry.workers.remove(&id);
        }
    }
    let rec = registry
        .workers
        .entry(worker.to_string())
        .or_insert_with(|| WorkerRecord {
            max_jobs: max_jobs.unwrap_or(1),
            registered_at: now,
            last_seen_at: now,
            draining: false,
            succeeded_jobs: 0,
            failed_jobs: 0,
            expired_leases: 0,
        });
    rec.last_seen_at = now;
    if let Some(m) = max_jobs {
        rec.max_jobs = m;
    }
    rec
}

fn worker_info_for(
    worker: &str,
    rec: &WorkerRecord,
    active_jobs: usize,
    now: u64,
    stale_after: u64,
) -> WorkerInfo {
    let status = if active_jobs == 0 && now.saturating_sub(rec.last_seen_at) > stale_after {
        WorkerStatus::Stale
    } else if rec.draining {
        WorkerStatus::Draining
    } else {
        WorkerStatus::Online
    };
    WorkerInfo {
        worker: worker.to_string(),
        status,
        max_jobs: rec.max_jobs,
        active_jobs,
        succeeded_jobs: rec.succeeded_jobs,
        failed_jobs: rec.failed_jobs,
        expired_leases: rec.expired_leases,
        registered_at: rec.registered_at,
        last_seen_at: rec.last_seen_at,
        draining: rec.draining,
    }
}

pub(crate) fn workers_snapshot(
    registry: &crate::Registry,
    now: u64,
    lease_ms: u64,
) -> (Vec<WorkerInfo>, WorkersSummary) {
    let mut active_counts: HashMap<&str, usize> = HashMap::new();
    for r in &registry.jobs {
        if r.job.status == JobStatus::Running {
            if let Some(w) = r.worker.as_deref() {
                *active_counts.entry(w).or_insert(0) += 1;
            }
        }
    }
    let stale_after = lease_ms.saturating_mul(2).max(1_000);
    let mut workers: Vec<WorkerInfo> = registry
        .workers
        .iter()
        .map(|(id, rec)| {
            let active = active_counts.get(id.as_str()).copied().unwrap_or(0);
            worker_info_for(id, rec, active, now, stale_after)
        })
        .collect();
    workers.sort_by(|a, b| a.worker.cmp(&b.worker));

    let mut summary = WorkersSummary {
        total: workers.len(),
        expired_leases: registry.expired_leases,
        ..Default::default()
    };
    for w in &workers {
        match w.status {
            WorkerStatus::Online => {
                summary.online += 1;
                summary.capacity += w.max_jobs;
            }
            WorkerStatus::Draining => summary.draining += 1,
            WorkerStatus::Stale => summary.stale += 1,
        }
        summary.active_jobs += w.active_jobs;
    }
    (workers, summary)
}

/// Requeues or fails leased jobs whose deadline passed without a heartbeat, returning job IDs
/// that reached a terminal state and must be marked done in the journal once unlocked.
pub(crate) fn expire_leases(registry: &mut crate::Registry, now: u64) -> Vec<Uuid> {
    let mut done = Vec::new();
    let mut expired_workers = Vec::new();
    for record in &mut registry.jobs {
        if record.job.status == JobStatus::Running
            && record.lease_until.is_some_and(|expiry| expiry <= now)
        {
            if let Some(prev) = record.worker.take() {
                expired_workers.push(prev);
            }
            record.lease_until = None;
            record.job.step_index = None;
            registry.expired_leases += 1;
            if record.job.attempts <= record.request.retries {
                record.job.status = JobStatus::Queued;
            } else {
                record.job.status = JobStatus::Failed;
                record.job.finished_at = Some(now);
                record.job.error = Some("worker lost".into());
                registry.failed_jobs += 1;
                done.push(record.job.id);
            }
        }
    }
    for w in expired_workers {
        if let Some(rec) = registry.workers.get_mut(&w) {
            rec.expired_leases += 1;
        }
    }
    done
}

pub(crate) async fn lease(
    State(inner): State<Arc<Inner>>,
    Path(worker): Path<String>,
    payload: Result<Json<LeaseRequest>, JsonRejection>,
) -> Result<Json<LeaseResponse>, ApiError> {
    validate_worker(&worker)?;
    let Json(req) =
        payload.map_err(|e| ApiError::new(e.status(), "invalid_request", e.body_text()))?;

    let lease_ms = inner.lease_ms();
    let leased = inner.with_registry(|registry, now| -> Result<Vec<LeasedJob>, ApiError> {
        if !registry.accepting {
            return Err(ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "shutting_down",
                "the coordinator is shutting down",
            ));
        }

        let limit = req.max_jobs.min(MAX_LEASE_BATCH);
        let draining = touch_worker(registry, &worker, now, Some(limit)).draining;
        if draining {
            return Ok(Vec::new());
        }
        let mut candidates: Vec<(usize, LeasedJob)> =
            Vec::with_capacity(limit.min(registry.jobs.len()));
        let mut total_bytes: usize = 16; // {"jobs":[]}
        if limit > 0 {
            'pick: for prio in [JobPriority::High, JobPriority::Normal, JobPriority::Low] {
                for (idx, record) in registry.jobs.iter().enumerate() {
                    if candidates.len() >= limit {
                        break 'pick;
                    }
                    if record.job.status == JobStatus::Queued && record.request.priority == prio {
                        let candidate = LeasedJob {
                            id: record.job.id,
                            request: record.request.clone(),
                            attempts: record.job.attempts + 1,
                            trace_id: record.job.trace_id.clone(),
                        };
                        let entry_bytes =
                            json_len(&candidate) + usize::from(!candidates.is_empty());
                        if !candidates.is_empty()
                            && total_bytes + entry_bytes > MAX_LEASE_RESPONSE_BYTES
                        {
                            break 'pick;
                        }
                        total_bytes += entry_bytes;
                        candidates.push((idx, candidate));
                    }
                }
            }
        }

        let mut leased = Vec::with_capacity(candidates.len());
        for (idx, item) in candidates {
            let record = &mut registry.jobs[idx];
            record.job.status = JobStatus::Running;
            record.job.started_at.get_or_insert(now);
            record.job.attempts = item.attempts;
            record.job.step_index = None;
            record.job.failure_evidence = None;
            record.worker = Some(worker.clone());
            record.lease_until = Some(now.saturating_add(lease_ms));
            leased.push(item);
        }
        Ok(leased)
    })?;

    Ok(Json(LeaseResponse { jobs: leased }))
}

pub(crate) async fn heartbeat(
    State(inner): State<Arc<Inner>>,
    Path(worker): Path<String>,
    payload: Result<Json<HeartbeatRequest>, JsonRejection>,
) -> Result<Json<HeartbeatResponse>, ApiError> {
    validate_worker(&worker)?;
    let Json(req) =
        payload.map_err(|e| ApiError::new(e.status(), "invalid_request", e.body_text()))?;

    let lease_ms = inner.lease_ms();
    let cancelled = inner.with_registry(|registry, now| {
        touch_worker(registry, &worker, now, None);
        let mut cancelled = Vec::new();
        for item in req.jobs {
            if let Some(record) = registry.find_mut(item.id) {
                if record.job.status == JobStatus::Running
                    && record.worker.as_deref() == Some(worker.as_str())
                {
                    record.lease_until = Some(now.saturating_add(lease_ms));
                    if item.step_index.is_some() {
                        record.job.step_index = item.step_index;
                    }
                    record.job.attempts = record.job.attempts.max(item.attempts);
                    continue;
                }
            }
            cancelled.push(item.id);
        }
        cancelled
    });

    Ok(Json(HeartbeatResponse { cancelled }))
}

pub(crate) async fn release(
    State(inner): State<Arc<Inner>>,
    Path(worker): Path<String>,
    payload: Result<Json<ReleaseRequest>, JsonRejection>,
) -> Result<Json<ReleaseResponse>, ApiError> {
    validate_worker(&worker)?;
    let Json(req) =
        payload.map_err(|e| ApiError::new(e.status(), "invalid_request", e.body_text()))?;

    let released = inner.with_registry(|registry, now| {
        touch_worker(registry, &worker, now, None);
        let mut released = Vec::new();
        for id in req.jobs {
            if let Some(record) = registry.find_mut(id) {
                if record.job.status == JobStatus::Running
                    && record.worker.as_deref() == Some(worker.as_str())
                {
                    record.job.status = JobStatus::Queued;
                    record.job.attempts = record.job.attempts.saturating_sub(1);
                    if record.job.attempts == 0 {
                        record.job.started_at = None;
                    }
                    record.job.step_index = None;
                    record.job.failure_evidence = None;
                    record.worker = None;
                    record.lease_until = None;
                    released.push(id);
                }
            }
        }
        released
    });

    Ok(Json(ReleaseResponse { released }))
}

pub(crate) async fn result(
    State(inner): State<Arc<Inner>>,
    Path(id): Path<String>,
    payload: Result<Json<ResultRequest>, JsonRejection>,
) -> Result<Json<Job>, ApiError> {
    let id = parse_id(&id)?;
    let Json(mut req) =
        payload.map_err(|e| ApiError::new(e.status(), "invalid_request", e.body_text()))?;
    validate_worker(&req.worker)?;
    if !matches!(req.status, JobStatus::Succeeded | JobStatus::Failed) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "result status must be succeeded or failed",
        ));
    }
    if let Some(results) = &req.results {
        if json_len(results) > MAX_RESULT_BYTES {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "job results exceed 1 MiB",
            ));
        }
    }

    let (job, finished) = inner.with_registry(|registry, now| -> Result<(Job, bool), ApiError> {
        touch_worker(registry, &req.worker, now, None);
        let record = registry.find_mut(id).ok_or_else(not_found)?;
        if record.job.status == JobStatus::Cancelled {
            return Ok((record.job.clone(), false));
        }
        if record.job.status != JobStatus::Running
            || record.worker.as_deref() != Some(req.worker.as_str())
        {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "lease_lost",
                "the job lease expired or belongs to another worker",
            ));
        }

        record.job.finished_at = Some(now);
        record.job.attempts = record.job.attempts.max(req.attempts);
        if req.step_index.is_some() {
            record.job.step_index = req.step_index;
        }
        record.worker = None;
        record.lease_until = None;

        match req.status {
            JobStatus::Succeeded => {
                record.job.status = JobStatus::Succeeded;
                record.job.results = Some(req.results.take().unwrap_or_default());
                record.job.error = None;
                record.job.failure_evidence = None;
            }
            JobStatus::Failed => {
                record.job.status = JobStatus::Failed;
                record.job.results = None;
                record.job.error = Some(bounded_error(
                    req.error.as_deref().unwrap_or("job failed"),
                ));
                if let Some(mut evidence) = req.failure_evidence.take() {
                    evidence.enforce_bounds();
                    if !evidence.is_empty() {
                        record.job.failure_evidence = Some(evidence);
                    }
                }
            }
            _ => unreachable!(),
        }
        let snapshot = record.job.clone();
        match req.status {
            JobStatus::Succeeded => {
                registry.succeeded_jobs += 1;
                if let Some(w) = registry.workers.get_mut(&req.worker) {
                    w.succeeded_jobs += 1;
                }
            }
            JobStatus::Failed => {
                registry.failed_jobs += 1;
                if let Some(w) = registry.workers.get_mut(&req.worker) {
                    w.failed_jobs += 1;
                }
            }
            _ => unreachable!(),
        }
        Ok((snapshot, true))
    })?;

    if finished {
        inner.journal_done(id);
        tracing::info!(job_id = %id, status = ?job.status, worker = %req.worker, "cluster job finished");
    }
    Ok(Json(job))
}

pub(crate) async fn list_workers(State(inner): State<Arc<Inner>>) -> Json<Value> {
    let lease_ms = inner.lease_ms();
    inner.with_registry(|registry, now| {
        let (workers, summary) = workers_snapshot(registry, now, lease_ms);
        Json(json!({ "workers": workers, "summary": summary }))
    })
}

pub(crate) async fn drain_worker(
    State(inner): State<Arc<Inner>>,
    Path(worker): Path<String>,
) -> Result<Json<WorkerInfo>, ApiError> {
    validate_worker(&worker)?;
    let lease_ms = inner.lease_ms();
    inner.with_registry(|registry, now| {
        if !registry.workers.contains_key(&worker) {
            return Err(ApiError::new(
                StatusCode::NOT_FOUND,
                "not_found",
                "worker not found",
            ));
        }
        let active = registry
            .jobs
            .iter()
            .filter(|r| {
                r.job.status == JobStatus::Running && r.worker.as_deref() == Some(worker.as_str())
            })
            .count();
        let stale_after = lease_ms.saturating_mul(2).max(1_000);
        let rec = registry.workers.get_mut(&worker).unwrap();
        rec.draining = true;
        Ok(Json(worker_info_for(&worker, rec, active, now, stale_after)))
    })
}

pub(crate) async fn remove_worker(
    State(inner): State<Arc<Inner>>,
    Path(worker): Path<String>,
) -> Result<Json<Value>, ApiError> {
    validate_worker(&worker)?;
    inner.with_registry(|registry, _| {
        if !registry.workers.contains_key(&worker) {
            return Err(ApiError::new(
                StatusCode::NOT_FOUND,
                "not_found",
                "worker not found",
            ));
        }
        let busy = registry.jobs.iter().any(|r| {
            r.job.status == JobStatus::Running && r.worker.as_deref() == Some(worker.as_str())
        });
        if busy {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "worker_busy",
                "cannot remove worker while it holds active job leases",
            ));
        }
        registry.workers.remove(&worker);
        Ok(Json(json!({ "removed": worker })))
    })
}

#[derive(Clone)]
pub(crate) struct CoordinatorEndpoint {
    pub(crate) host: String,
    port: u16,
    host_header: String,
    base_path: String,
}

impl CoordinatorEndpoint {
    pub(crate) fn is_loopback(&self) -> bool {
        self.host.eq_ignore_ascii_case("localhost")
            || self
                .host
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    }

    pub(crate) fn parse(url: &str) -> Result<Self, ConfigError> {
        let trimmed = url.trim();
        if trimmed.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(ConfigError::InvalidCoordinator(
                "URL must not contain whitespace".into(),
            ));
        }
        if trimmed.starts_with("https://") {
            return Err(ConfigError::InvalidCoordinator(
                "HTTPS coordinator URLs are not supported natively; terminate TLS in a local reverse proxy/tunnel or use http:// on a trusted private network"
                    .into(),
            ));
        }
        let rest = trimmed.strip_prefix("http://").ok_or_else(|| {
            ConfigError::InvalidCoordinator("coordinator URL must start with http://".into())
        })?;
        let (authority, path_raw) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, ""),
        };
        if authority.contains('@') || authority.contains(['?', '#']) || path_raw.contains(['?', '#'])
        {
            return Err(ConfigError::InvalidCoordinator(
                "userinfo, query, and fragment are not allowed".into(),
            ));
        }
        let (host, port) = if let Some(after_bracket) = authority.strip_prefix('[') {
            let Some(end) = after_bracket.find(']') else {
                return Err(ConfigError::InvalidCoordinator("unclosed IPv6 host".into()));
            };
            let ip = &after_bracket[..end];
            let tail = &after_bracket[end + 1..];
            let port = match tail {
                "" => 80,
                _ => tail
                    .strip_prefix(':')
                    .and_then(|p| p.parse::<u16>().ok())
                    .filter(|&p| p != 0)
                    .ok_or_else(|| ConfigError::InvalidCoordinator("invalid port".into()))?,
            };
            (ip.to_string(), port)
        } else if let Some((h, p)) = authority.rsplit_once(':') {
            let port = p
                .parse::<u16>()
                .ok()
                .filter(|&n| n != 0)
                .ok_or_else(|| ConfigError::InvalidCoordinator("invalid port".into()))?;
            (h.to_string(), port)
        } else {
            (authority.to_string(), 80)
        };
        if host.is_empty() {
            return Err(ConfigError::InvalidCoordinator(
                "empty coordinator host".into(),
            ));
        }
        Ok(Self {
            host,
            port,
            host_header: authority.to_string(),
            base_path: path_raw.trim_end_matches('/').to_string(),
        })
    }
}

/// Control handle returned by [`spawn_worker`].
#[derive(Clone)]
pub struct WorkerHandle {
    drain_tx: watch::Sender<Option<Duration>>,
    finished_rx: watch::Receiver<bool>,
    abort: AbortHandle,
    loopback: bool,
}

impl WorkerHandle {
    /// Returns true when the configured coordinator host is a loopback address.
    pub fn coordinator_is_loopback(&self) -> bool {
        self.loopback
    }

    /// Stops polling immediately and aborts active leased job tasks.
    pub fn abort(&self) {
        self.drain_tx.send_if_modified(|v| {
            let changed = v.is_none();
            *v = Some(Duration::ZERO);
            changed
        });
        self.abort.abort();
    }

    /// Stops taking new leases, waits up to `grace` for in-flight jobs and pending result reports
    /// to finish cleanly, then releases any remaining unfinished jobs back to the coordinator
    /// without counting an attempt.
    pub async fn shutdown(&self, grace: Duration) {
        self.drain_tx.send_if_modified(|v| {
            if v.is_none() {
                *v = Some(grace);
                true
            } else {
                false
            }
        });
        if !*self.finished_rx.borrow() {
            let mut rx = self.finished_rx.clone();
            let wait = grace.saturating_add(SHUTDOWN_REPORT_BUDGET);
            let _ = tokio::time::timeout(wait, async move {
                while !*rx.borrow() {
                    if rx.changed().await.is_err() {
                        break;
                    }
                }
            })
            .await;
        }
        self.abort.abort();
    }
}

/// Starts a background pull loop that leases jobs from `coordinator_url` and runs them on `engine`.
pub fn spawn_worker(
    engine: BrowserEngine,
    coordinator_url: String,
    token: String,
    worker_id: Option<String>,
    max_jobs: usize,
) -> Result<WorkerHandle, ConfigError> {
    spawn_worker_with(
        engine,
        coordinator_url,
        token,
        worker_id,
        max_jobs,
        DEFAULT_POLL_INTERVAL,
        DEFAULT_HEARTBEAT_INTERVAL,
    )
}

pub(crate) fn spawn_worker_with(
    engine: BrowserEngine,
    coordinator_url: String,
    token: String,
    worker_id: Option<String>,
    max_jobs: usize,
    poll_interval: Duration,
    heartbeat_interval: Duration,
) -> Result<WorkerHandle, ConfigError> {
    let endpoint = CoordinatorEndpoint::parse(&coordinator_url)?;
    let loopback = endpoint.is_loopback();
    let worker_id = match worker_id {
        Some(id) => {
            if validate_worker(&id).is_err() || id.contains(['/', '?', '#']) {
                return Err(ConfigError::InvalidCoordinator(
                    "worker_id must be 1-128 safe path characters".into(),
                ));
            }
            id
        }
        None => default_worker_id(),
    };
    let max_jobs = max_jobs.clamp(1, MAX_LEASE_BATCH);

    let (drain_tx, drain_rx) = watch::channel(None);
    let (finished_tx, finished_rx) = watch::channel(false);
    let handle = tokio::spawn(worker_loop(
        engine,
        endpoint,
        token,
        worker_id,
        max_jobs,
        poll_interval,
        heartbeat_interval,
        drain_rx,
        finished_tx,
    ));
    Ok(WorkerHandle {
        drain_tx,
        finished_rx,
        abort: handle.abort_handle(),
        loopback,
    })
}

fn default_worker_id() -> String {
    let host = std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| "worker".into());
    let clean: String = host
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .take(48)
        .collect();
    let clean = clean.trim_matches('-');
    let prefix = if clean.is_empty() { "worker" } else { clean };
    let short = &Uuid::new_v4().to_string()[..8];
    format!("{prefix}-{}-{short}", std::process::id())
}

struct RunningTask {
    abort: AbortHandle,
    progress: Arc<Mutex<ExecutionProgress>>,
}

struct TaskGuard(Arc<Mutex<HashMap<Uuid, RunningTask>>>);

impl Drop for TaskGuard {
    fn drop(&mut self) {
        let mut guard = self.0.lock().unwrap_or_else(|e| e.into_inner());
        for (_, task) in guard.drain() {
            task.abort.abort();
        }
    }
}

struct AbortOnDrop(AbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn heartbeat_loop(
    ep: CoordinatorEndpoint,
    token: String,
    worker_id: String,
    interval: Duration,
    active: Arc<Mutex<HashMap<Uuid, RunningTask>>>,
    wake: Arc<Notify>,
) {
    let mut tick = tokio::time::interval(interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    tick.reset();

    loop {
        tick.tick().await;
        let items: Vec<HeartbeatItem> = {
            let guard = active.lock().unwrap();
            if guard.is_empty() {
                continue;
            }
            guard
                .iter()
                .map(|(&id, t)| {
                    let p = t.progress.lock().unwrap();
                    HeartbeatItem {
                        id,
                        step_index: p.step_index,
                        attempts: p.attempts,
                    }
                })
                .collect()
        };
        match post_heartbeat(&ep, &token, &worker_id, &items).await {
            Ok(resp) => {
                if !resp.cancelled.is_empty() {
                    let mut guard = active.lock().unwrap();
                    for cid in resp.cancelled {
                        if let Some(t) = guard.remove(&cid) {
                            t.abort.abort();
                        }
                    }
                    wake.notify_one();
                }
            }
            Err(error) => {
                tracing::warn!(%error, worker = %worker_id, "coordinator heartbeat failed");
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn worker_loop(
    engine: BrowserEngine,
    endpoint: CoordinatorEndpoint,
    token: String,
    worker_id: String,
    max_jobs: usize,
    poll_interval: Duration,
    heartbeat_interval: Duration,
    mut drain_rx: watch::Receiver<Option<Duration>>,
    finished_tx: watch::Sender<bool>,
) {
    let active = Arc::new(Mutex::new(HashMap::new()));
    let _task_guard = TaskGuard(active.clone());
    let wake = Arc::new(Notify::new());
    // Heartbeats run in an independent task so slow /lease or /result calls never starve them.
    let hb_task = tokio::spawn(heartbeat_loop(
        endpoint.clone(),
        token.clone(),
        worker_id.clone(),
        heartbeat_interval,
        active.clone(),
        wake.clone(),
    ));
    let hb_abort = hb_task.abort_handle();
    let _hb_guard = AbortOnDrop(hb_abort.clone());

    let mut pending_results: VecDeque<(Uuid, ResultRequest)> = VecDeque::new();
    let (done_tx, mut done_rx) = mpsc::unbounded_channel::<(Uuid, ResultRequest)>();

    let poll_timer = tokio::time::sleep(Duration::ZERO);
    tokio::pin!(poll_timer);
    let mut can_lease = true;
    let mut grace_deadline: Option<tokio::time::Instant> = (*drain_rx.borrow_and_update())
        .map(|grace| tokio::time::Instant::now() + grace);
    let mut backoff = poll_interval.max(Duration::from_millis(50));

    loop {
        if grace_deadline.is_none() {
            if let Some(grace) = *drain_rx.borrow_and_update() {
                grace_deadline = Some(tokio::time::Instant::now() + grace);
            }
        }
        let draining = grace_deadline.is_some();
        if draining {
            can_lease = false;
        }

        if !pending_results.is_empty() {
            if flush_pending_results(&endpoint, &token, &mut pending_results).await {
                backoff = poll_interval.max(Duration::from_millis(50));
            } else {
                poll_timer
                    .as_mut()
                    .reset(tokio::time::Instant::now() + backoff);
                backoff = (backoff * 2).min(Duration::from_secs(5));
            }
        }

        let active_len = active.lock().unwrap().len();
        if draining && active_len == 0 && pending_results.is_empty() {
            let _ = finished_tx.send(true);
            return;
        }

        if let Some(deadline) = grace_deadline {
            if tokio::time::Instant::now() >= deadline {
                release_remaining(
                    &hb_abort,
                    &active,
                    &mut done_rx,
                    &mut pending_results,
                    &endpoint,
                    &token,
                    &worker_id,
                )
                .await;
                let _ = finished_tx.send(true);
                return;
            }
        }

        if !draining && can_lease && active_len < max_jobs && pending_results.len() < max_jobs * 2 {
            let want = (max_jobs - active_len).min(MAX_LEASE_BATCH);
            match post_lease(&endpoint, &token, &worker_id, want).await {
                Ok(leased) if !leased.is_empty() => {
                    backoff = poll_interval.max(Duration::from_millis(50));
                    let got = leased.len();
                    {
                        let mut guard = active.lock().unwrap();
                        for item in leased {
                            spawn_leased_job(&engine, &worker_id, item, &done_tx, &mut guard);
                        }
                    }
                    let new_len = active.lock().unwrap().len();
                    if new_len >= max_jobs {
                        can_lease = false;
                    } else if got < want {
                        can_lease = false;
                        poll_timer
                            .as_mut()
                            .reset(tokio::time::Instant::now() + poll_interval);
                    }
                    continue;
                }
                Ok(_) => {
                    backoff = poll_interval.max(Duration::from_millis(50));
                    can_lease = false;
                    poll_timer
                        .as_mut()
                        .reset(tokio::time::Instant::now() + poll_interval);
                }
                Err(error) => {
                    tracing::warn!(%error, worker = %worker_id, "coordinator lease poll failed");
                    can_lease = false;
                    poll_timer
                        .as_mut()
                        .reset(tokio::time::Instant::now() + backoff);
                    backoff = (backoff * 2).min(Duration::from_secs(5));
                }
            }
        }

        let need_poll_tick = (!draining && !can_lease && active.lock().unwrap().len() < max_jobs)
            || !pending_results.is_empty();

        tokio::select! {
            _ = drain_rx.changed(), if !draining => {
                if let Some(grace) = *drain_rx.borrow_and_update() {
                    grace_deadline = Some(tokio::time::Instant::now() + grace);
                    can_lease = false;
                }
            }
            _ = async {
                if let Some(deadline) = grace_deadline {
                    tokio::time::sleep_until(deadline).await;
                }
            }, if draining => {
                release_remaining(
                    &hb_abort,
                    &active,
                    &mut done_rx,
                    &mut pending_results,
                    &endpoint,
                    &token,
                    &worker_id,
                )
                .await;
                let _ = finished_tx.send(true);
                return;
            }
            _ = wake.notified(), if draining => {}
            _ = &mut poll_timer, if need_poll_tick => {
                can_lease = !draining;
            }
            Some((id, req)) = done_rx.recv() => {
                let removed = active.lock().unwrap().remove(&id).is_some();
                if removed {
                    match post_result(&endpoint, &token, id, &req).await {
                        ResultPost::Delivered | ResultPost::Dropped => {}
                        ResultPost::Retry => {
                            pending_results.push_back((id, req));
                            poll_timer
                                .as_mut()
                                .reset(tokio::time::Instant::now() + backoff);
                        }
                    }
                    if !draining && active.lock().unwrap().len() < max_jobs {
                        can_lease = true;
                    }
                }
            }
        }
    }
}

async fn flush_pending_results(
    endpoint: &CoordinatorEndpoint,
    token: &str,
    pending_results: &mut VecDeque<(Uuid, ResultRequest)>,
) -> bool {
    while let Some((id, req)) = pending_results.front() {
        match post_result(endpoint, token, *id, req).await {
            ResultPost::Delivered | ResultPost::Dropped => {
                pending_results.pop_front();
            }
            ResultPost::Retry => return false,
        }
    }
    true
}

async fn release_remaining(
    hb_abort: &AbortHandle,
    active: &Mutex<HashMap<Uuid, RunningTask>>,
    done_rx: &mut mpsc::UnboundedReceiver<(Uuid, ResultRequest)>,
    pending_results: &mut VecDeque<(Uuid, ResultRequest)>,
    endpoint: &CoordinatorEndpoint,
    token: &str,
    worker_id: &str,
) {
    hb_abort.abort();
    let unfinished = stop_active_tasks(active, done_rx, pending_results).await;
    // Release first: it is one call, and without it a `retries: 0` job fails as "worker lost".
    if !unfinished.is_empty() {
        if let Err(error) = post_release(endpoint, token, worker_id, &unfinished).await {
            tracing::warn!(%error, worker = %worker_id, count = unfinished.len(), "failed to release unfinished jobs on shutdown");
        }
    }
    let _ = flush_pending_results(endpoint, token, pending_results).await;
}

/// Aborts every leased task, waits for them to stop, and moves results that arrived meanwhile to
/// `pending_results`. Returns the jobs that never finished. A task can be mid-poll when aborted
/// and still send its result, so collecting results before every task has stopped would release
/// (and rerun) a job that had already completed. A task still running after `TASK_STOP_WAIT` is
/// released anyway, and a result it sends later is dropped.
async fn stop_active_tasks(
    active: &Mutex<HashMap<Uuid, RunningTask>>,
    done_rx: &mut mpsc::UnboundedReceiver<(Uuid, ResultRequest)>,
    pending_results: &mut VecDeque<(Uuid, ResultRequest)>,
) -> Vec<Uuid> {
    let handles: Vec<AbortHandle> = active
        .lock()
        .unwrap()
        .values()
        .map(|task| {
            task.abort.abort();
            task.abort.clone()
        })
        .collect();
    let _ = tokio::time::timeout(TASK_STOP_WAIT, async {
        while handles.iter().any(|h| !h.is_finished()) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    while let Ok((id, req)) = done_rx.try_recv() {
        if active.lock().unwrap().remove(&id).is_some() {
            pending_results.push_back((id, req));
        }
    }
    active.lock().unwrap().drain().map(|(id, _)| id).collect()
}

fn spawn_leased_job(
    engine: &BrowserEngine,
    worker_id: &str,
    leased: LeasedJob,
    done_tx: &mpsc::UnboundedSender<(Uuid, ResultRequest)>,
    active: &mut HashMap<Uuid, RunningTask>,
) {
    let progress = Arc::new(Mutex::new(ExecutionProgress {
        attempts: leased.attempts,
        step_index: None,
        failure_evidence: None,
    }));
    let progress_for_task = progress.clone();
    let engine = engine.clone();
    let worker_name = worker_id.to_string();
    let done_tx = done_tx.clone();
    let id = leased.id;
    let initial_attempts = leased.attempts;
    let trace_id = leased.trace_id;
    let request = leased.request;

    let task = tokio::spawn(async move {
        let mut trace_ctx = JobTraceContext::new();
        if let Some(tid) = trace_id {
            trace_ctx.trace_id = tid;
        }
        let outcome = execute_job(
            engine,
            request,
            None,
            id,
            trace_ctx,
            initial_attempts,
            progress_for_task.clone(),
        )
        .await;
        let prog = progress_for_task.lock().unwrap().clone();
        let req = match outcome {
            Ok(results) => ResultRequest {
                worker: worker_name,
                status: JobStatus::Succeeded,
                attempts: prog.attempts.max(initial_attempts),
                step_index: prog.step_index,
                results: Some(results),
                error: None,
                failure_evidence: None,
            },
            Err(err) => ResultRequest {
                worker: worker_name,
                status: JobStatus::Failed,
                attempts: prog.attempts.max(initial_attempts),
                step_index: prog.step_index,
                results: None,
                error: Some(bounded_error(&err.to_string())),
                failure_evidence: prog.failure_evidence,
            },
        };
        let _ = done_tx.send((id, req));
    });

    active.insert(
        id,
        RunningTask {
            abort: task.abort_handle(),
            progress,
        },
    );
}

async fn post_lease(
    ep: &CoordinatorEndpoint,
    token: &str,
    worker: &str,
    max_jobs: usize,
) -> Result<Vec<LeasedJob>, String> {
    let (status, body) = post_json_raw(
        ep,
        token,
        &format!("/v1/workers/{worker}/lease"),
        &serde_json::json!({ "max_jobs": max_jobs }),
    )
    .await?;
    if status != 200 {
        return Err(format!("HTTP {status}"));
    }
    let resp: LeaseResponse = serde_json::from_slice(&body).map_err(|e| e.to_string())?;
    Ok(resp.jobs)
}

async fn post_heartbeat(
    ep: &CoordinatorEndpoint,
    token: &str,
    worker: &str,
    items: &[HeartbeatItem],
) -> Result<HeartbeatResponse, String> {
    let (status, body) = post_json_raw(
        ep,
        token,
        &format!("/v1/workers/{worker}/heartbeat"),
        &serde_json::json!({ "jobs": items }),
    )
    .await?;
    if status != 200 {
        return Err(format!("HTTP {status}"));
    }
    serde_json::from_slice(&body).map_err(|e| e.to_string())
}

async fn post_release(
    ep: &CoordinatorEndpoint,
    token: &str,
    worker: &str,
    ids: &[Uuid],
) -> Result<(), String> {
    let (status, _) = post_json_raw(
        ep,
        token,
        &format!("/v1/workers/{worker}/release"),
        &ReleaseRequest { jobs: ids.to_vec() },
    )
    .await?;
    if status != 200 {
        return Err(format!("HTTP {status}"));
    }
    Ok(())
}

enum ResultPost {
    Delivered,
    Dropped,
    Retry,
}

async fn post_result(
    ep: &CoordinatorEndpoint,
    token: &str,
    id: Uuid,
    req: &ResultRequest,
) -> ResultPost {
    match post_json_raw(ep, token, &format!("/v1/jobs/{id}/result"), req).await {
        Ok((200, _)) => ResultPost::Delivered,
        Ok((status @ 400..=499, _)) => {
            tracing::warn!(job_id = %id, status, "coordinator rejected job result; dropping");
            ResultPost::Dropped
        }
        Ok((status, _)) => {
            tracing::warn!(job_id = %id, status, "coordinator returned error for result; will retry");
            ResultPost::Retry
        }
        Err(error) => {
            tracing::warn!(job_id = %id, %error, "failed to report result to coordinator; will retry");
            ResultPost::Retry
        }
    }
}

async fn post_json_raw(
    ep: &CoordinatorEndpoint,
    token: &str,
    path: &str,
    payload: &impl Serialize,
) -> Result<(u16, Vec<u8>), String> {
    let json_bytes = serde_json::to_vec(payload).map_err(|e| e.to_string())?;
    let full_path = format!("{}{}", ep.base_path, path);
    let head = format!(
        "POST {full_path} HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        ep.host_header,
        json_bytes.len()
    );
    let io = async {
        let mut stream = TcpStream::connect((ep.host.as_str(), ep.port))
            .await
            .map_err(|e| e.to_string())?;
        stream
            .write_all(head.as_bytes())
            .await
            .map_err(|e| e.to_string())?;
        stream
            .write_all(&json_bytes)
            .await
            .map_err(|e| e.to_string())?;
        stream.flush().await.map_err(|e| e.to_string())?;
        let mut raw = Vec::new();
        stream
            .take((MAX_HTTP_RESPONSE_BYTES + 1) as u64)
            .read_to_end(&mut raw)
            .await
            .map_err(|e| e.to_string())?;
        if raw.len() > MAX_HTTP_RESPONSE_BYTES {
            return Err("coordinator HTTP response exceeded maximum size".into());
        }
        Ok::<Vec<u8>, String>(raw)
    };
    let raw = tokio::time::timeout(HTTP_REQUEST_TIMEOUT, io)
        .await
        .map_err(|_| "coordinator HTTP request timed out".to_string())??;
    let sep = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| "invalid HTTP response from coordinator".to_string())?;
    let header_text = std::str::from_utf8(&raw[..sep])
        .map_err(|_| "invalid HTTP headers from coordinator".to_string())?;
    let mut lines = header_text.split("\r\n");
    let status_line = lines.next().unwrap_or("");
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| format!("invalid HTTP status line: {status_line}"))?;
    let is_chunked = lines.any(|l| {
        l.split_once(':').is_some_and(|(k, v)| {
            k.trim().eq_ignore_ascii_case("transfer-encoding")
                && v.to_ascii_lowercase().contains("chunked")
        })
    });
    let body = &raw[sep + 4..];
    let body = if is_chunked {
        decode_chunked(body)?
    } else {
        body.to_vec()
    };
    Ok((status, body))
}

fn decode_chunked(mut data: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    loop {
        let Some(line_end) = data.windows(2).position(|w| w == b"\r\n") else {
            return Err("truncated chunked response".into());
        };
        let size_hex = std::str::from_utf8(&data[..line_end])
            .map_err(|_| "invalid chunk size")?
            .split(';')
            .next()
            .unwrap_or("")
            .trim();
        let size = usize::from_str_radix(size_hex, 16).map_err(|_| "invalid chunk hex")?;
        data = &data[line_end + 2..];
        if size == 0 {
            return Ok(out);
        }
        if data.len() < size + 2 {
            return Err("truncated chunk payload".into());
        }
        out.extend_from_slice(&data[..size]);
        data = &data[size + 2..];
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn running(abort: AbortHandle) -> RunningTask {
        RunningTask {
            abort,
            progress: Arc::new(Mutex::new(ExecutionProgress {
                attempts: 1,
                step_index: None,
                failure_evidence: None,
            })),
        }
    }

    fn succeeded() -> ResultRequest {
        ResultRequest {
            worker: "w".into(),
            status: JobStatus::Succeeded,
            attempts: 1,
            step_index: None,
            results: Some(Vec::new()),
            error: None,
            failure_evidence: None,
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stopping_tasks_keeps_a_result_sent_after_abort() {
        let (done_tx, mut done_rx) = mpsc::unbounded_channel();
        let active = Mutex::new(HashMap::new());
        let finishing = Uuid::new_v4();
        let stuck = Uuid::new_v4();

        // Mid-poll when aborted: it cannot be interrupted and sends its result afterwards.
        let started = Arc::new(AtomicBool::new(false));
        let flag = started.clone();
        let tx = done_tx.clone();
        let task = tokio::spawn(async move {
            flag.store(true, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(100));
            let _ = tx.send((finishing, succeeded()));
        });
        active
            .lock()
            .unwrap()
            .insert(finishing, running(task.abort_handle()));
        let task = tokio::spawn(std::future::pending::<()>());
        active
            .lock()
            .unwrap()
            .insert(stuck, running(task.abort_handle()));
        while !started.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }

        let mut pending = VecDeque::new();
        let unfinished = stop_active_tasks(&active, &mut done_rx, &mut pending).await;

        assert_eq!(unfinished, vec![stuck]);
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].0, finishing);
        assert!(active.lock().unwrap().is_empty());
    }
}
