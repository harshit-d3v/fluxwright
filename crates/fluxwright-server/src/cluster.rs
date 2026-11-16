//! Coordinator HTTP endpoints and pulling worker loop for running Fluxwright across multiple machines.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use fluxwright::BrowserEngine;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::task::AbortHandle;
use uuid::Uuid;

use crate::{
    bounded_error, execute_job, json_len, not_found, now_ms, parse_id, ApiError, ConfigError,
    ExecutionProgress, FailureEvidence, Inner, Job, JobPriority, JobRequest, JobStatus,
    JobTraceContext, Registry, MAX_RESULT_BYTES,
};

const MAX_LEASE_BATCH: usize = 64;
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(200);
const DEFAULT_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);

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

/// Requeues or fails leased jobs whose deadline passed without a heartbeat, returning job IDs
/// that reached a terminal state and must be marked done in the journal once unlocked.
pub(crate) fn expire_leases(registry: &mut Registry, now: u64) -> Vec<Uuid> {
    let mut done = Vec::new();
    for record in &mut registry.jobs {
        if record.job.status == JobStatus::Running
            && record.lease_until.is_some_and(|expiry| expiry <= now)
        {
            record.worker = None;
            record.lease_until = None;
            record.job.step_index = None;
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

    let now = now_ms();
    let lease_ms = inner.lease_ms();
    let (leased, expired) = {
        let mut registry = inner.registry.lock().unwrap();
        let expired = expire_leases(&mut registry, now);
        if !registry.accepting {
            drop(registry);
            for id in expired {
                inner.journal_done(id);
            }
            return Err(ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "shutting_down",
                "the coordinator is shutting down",
            ));
        }

        let limit = req.max_jobs.min(MAX_LEASE_BATCH);
        let mut picked = Vec::with_capacity(limit.min(registry.jobs.len()));
        if limit > 0 {
            for prio in [JobPriority::High, JobPriority::Normal, JobPriority::Low] {
                for (idx, record) in registry.jobs.iter().enumerate() {
                    if picked.len() >= limit {
                        break;
                    }
                    if record.job.status == JobStatus::Queued && record.request.priority == prio {
                        picked.push(idx);
                    }
                }
                if picked.len() >= limit {
                    break;
                }
            }
        }

        let mut leased = Vec::with_capacity(picked.len());
        for idx in picked {
            let record = &mut registry.jobs[idx];
            record.job.status = JobStatus::Running;
            record.job.started_at.get_or_insert(now);
            record.job.attempts += 1;
            record.job.step_index = None;
            record.job.failure_evidence = None;
            record.worker = Some(worker.clone());
            record.lease_until = Some(now.saturating_add(lease_ms));
            leased.push(LeasedJob {
                id: record.job.id,
                request: record.request.clone(),
                attempts: record.job.attempts,
                trace_id: record.job.trace_id.clone(),
            });
        }
        (leased, expired)
    };

    for id in expired {
        inner.journal_done(id);
    }
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

    let now = now_ms();
    let lease_ms = inner.lease_ms();
    let (cancelled, expired) = {
        let mut registry = inner.registry.lock().unwrap();
        let expired = expire_leases(&mut registry, now);
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
        (cancelled, expired)
    };

    for id in expired {
        inner.journal_done(id);
    }
    Ok(Json(HeartbeatResponse { cancelled }))
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
        let value = Value::Array(results.clone());
        if json_len(&value) > MAX_RESULT_BYTES {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "job results exceed 1 MiB",
            ));
        }
    }

    let now = now_ms();
    let mut done_ids = Vec::new();
    let job = {
        let mut registry = inner.registry.lock().unwrap();
        done_ids.extend(expire_leases(&mut registry, now));

        let Some(record) = registry.find_mut(id) else {
            drop(registry);
            for expired_id in done_ids {
                inner.journal_done(expired_id);
            }
            return Err(not_found());
        };

        if record.job.status == JobStatus::Cancelled {
            let snapshot = record.job.clone();
            drop(registry);
            for expired_id in done_ids {
                inner.journal_done(expired_id);
            }
            return Ok(Json(snapshot));
        }

        if record.job.status != JobStatus::Running
            || record.worker.as_deref() != Some(req.worker.as_str())
        {
            drop(registry);
            for expired_id in done_ids {
                inner.journal_done(expired_id);
            }
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
            JobStatus::Succeeded => registry.succeeded_jobs += 1,
            JobStatus::Failed => registry.failed_jobs += 1,
            _ => unreachable!(),
        }
        done_ids.push(id);
        snapshot
    };

    for finished_id in done_ids {
        inner.journal_done(finished_id);
    }
    tracing::info!(job_id = %id, status = ?job.status, worker = %req.worker, "cluster job finished");
    Ok(Json(job))
}

#[derive(Clone)]
pub(crate) struct CoordinatorEndpoint {
    host: String,
    port: u16,
    host_header: String,
    base_path: String,
}

impl CoordinatorEndpoint {
    pub(crate) fn parse(url: &str) -> Result<Self, ConfigError> {
        let trimmed = url.trim();
        if trimmed.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(ConfigError::InvalidCoordinator(
                "URL must not contain whitespace".into(),
            ));
        }
        if trimmed.starts_with("https://") {
            return Err(ConfigError::InvalidCoordinator(
                "HTTPS coordinator URLs are not supported natively; terminate TLS in a reverse proxy and point --join at http://"
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

/// Starts a background pull loop that leases jobs from `coordinator_url` and runs them on `engine`.
/// Aborting the returned handle stops polling and aborts all active leased job tasks.
pub fn spawn_worker(
    engine: BrowserEngine,
    coordinator_url: String,
    token: String,
    worker_id: Option<String>,
    max_jobs: usize,
) -> Result<AbortHandle, ConfigError> {
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
) -> Result<AbortHandle, ConfigError> {
    let endpoint = CoordinatorEndpoint::parse(&coordinator_url)?;
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

    let handle = tokio::spawn(worker_loop(
        engine,
        endpoint,
        token,
        worker_id,
        max_jobs,
        poll_interval,
        heartbeat_interval,
    ));
    Ok(handle.abort_handle())
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

struct TaskGuard(HashMap<Uuid, RunningTask>);

impl Drop for TaskGuard {
    fn drop(&mut self) {
        for (_, task) in self.0.drain() {
            task.abort.abort();
        }
    }
}

async fn worker_loop(
    engine: BrowserEngine,
    endpoint: CoordinatorEndpoint,
    token: String,
    worker_id: String,
    max_jobs: usize,
    poll_interval: Duration,
    heartbeat_interval: Duration,
) {
    let mut tasks = TaskGuard(HashMap::new());
    let mut pending_results: VecDeque<(Uuid, ResultRequest)> = VecDeque::new();
    let (done_tx, mut done_rx) = mpsc::unbounded_channel::<(Uuid, ResultRequest)>();

    let mut hb = tokio::time::interval(heartbeat_interval);
    hb.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    hb.reset();

    let poll_timer = tokio::time::sleep(Duration::ZERO);
    tokio::pin!(poll_timer);
    let mut can_lease = true;
    let mut backoff = poll_interval.max(Duration::from_millis(50));

    loop {
        while let Some((id, req)) = pending_results.front() {
            match post_result(&endpoint, &token, *id, req).await {
                ResultPost::Delivered | ResultPost::Dropped => {
                    pending_results.pop_front();
                }
                ResultPost::Retry => break,
            }
        }

        if can_lease && tasks.0.len() < max_jobs && pending_results.len() < max_jobs * 2 {
            let want = (max_jobs - tasks.0.len()).min(MAX_LEASE_BATCH);
            match post_lease(&endpoint, &token, &worker_id, want).await {
                Ok(leased) if !leased.is_empty() => {
                    backoff = poll_interval.max(Duration::from_millis(50));
                    let got = leased.len();
                    for item in leased {
                        spawn_leased_job(&engine, &worker_id, item, &done_tx, &mut tasks.0);
                    }
                    if tasks.0.len() >= max_jobs {
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

        tokio::select! {
            _ = &mut poll_timer, if !can_lease && tasks.0.len() < max_jobs => {
                can_lease = true;
            }
            _ = hb.tick() => {
                if !tasks.0.is_empty() {
                    let items: Vec<HeartbeatItem> = tasks
                        .0
                        .iter()
                        .map(|(&id, t)| {
                            let p = t.progress.lock().unwrap();
                            HeartbeatItem {
                                id,
                                step_index: p.step_index,
                                attempts: p.attempts,
                            }
                        })
                        .collect();
                    match post_heartbeat(&endpoint, &token, &worker_id, &items).await {
                        Ok(resp) => {
                            for cid in resp.cancelled {
                                if let Some(t) = tasks.0.remove(&cid) {
                                    t.abort.abort();
                                }
                            }
                        }
                        Err(error) => {
                            tracing::warn!(%error, worker = %worker_id, "coordinator heartbeat failed");
                        }
                    }
                }
            }
            Some((id, req)) = done_rx.recv() => {
                if tasks.0.remove(&id).is_some() {
                    match post_result(&endpoint, &token, id, &req).await {
                        ResultPost::Delivered | ResultPost::Dropped => {}
                        ResultPost::Retry => pending_results.push_back((id, req)),
                    }
                    if tasks.0.len() < max_jobs {
                        can_lease = true;
                    }
                }
            }
        }
    }
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
        let limit = (MAX_RESULT_BYTES * 2 + 65_536) as u64;
        stream
            .take(limit)
            .read_to_end(&mut raw)
            .await
            .map_err(|e| e.to_string())?;
        Ok::<Vec<u8>, String>(raw)
    };
    let raw = tokio::time::timeout(Duration::from_secs(15), io)
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
