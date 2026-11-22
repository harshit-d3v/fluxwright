//! Durable job persistence (`--journal` JSONL and `--store` SQLite).
//!
//! `JobStore` abstracts both persistence backends:
//! - [`Journal`]: append-only JSONL replay of unfinished jobs (no retained finished results).
//! - [`SqliteStore`]: WAL-mode SQLite persistence of accepted jobs and full terminal `Job`
//!   snapshots (`results`, `error`, `failure_evidence`, `status`, `attempts`, `finished_at`) with
//!   TTL-based pruning (`--result-ttl-secs`) and startup recovery.

use std::path::Path;
#[cfg(unix)]
use std::path::PathBuf;
#[cfg(unix)]
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use rusqlite::{params, Connection};
use serde_json::Value;
use uuid::Uuid;

use crate::journal::Journal;
use crate::{ConfigError, Job, JobStatus, ServerConfig};

/// Default busy timeout (in milliseconds) for SQLite lock contention.
const SQLITE_BUSY_TIMEOUT_MS: u64 = 5_000;

pub(crate) struct PendingRecord {
    pub(crate) id: Uuid,
    pub(crate) request: Value,
    pub(crate) job: Option<Job>,
}

pub(crate) struct FinishedRecord {
    pub(crate) job: Job,
    pub(crate) request_json: String,
}

pub(crate) struct LoadedJobs {
    pub(crate) pending: Vec<PendingRecord>,
    pub(crate) finished: Vec<FinishedRecord>,
}

pub(crate) trait JobStore: Send + Sync {
    fn submitted(&self, job: &Job, request: &Value);
    fn finished(&self, job: &Job, now_ms: u64);
    fn invalid_pending(&self, id: Uuid);
}

/// Opens the configured durable store backend (either `--journal` or `--store`), rejecting both
/// together with [`ConfigError::ConflictingStore`].
pub(crate) fn open_job_store(
    config: &ServerConfig,
    now_ms: u64,
) -> Result<Option<(Box<dyn JobStore>, LoadedJobs)>, ConfigError> {
    if config.journal.is_some() && config.store.is_some() {
        return Err(ConfigError::ConflictingStore);
    }
    if let Some(path) = &config.journal {
        let (journal, pending) = Journal::open(path).map_err(ConfigError::Journal)?;
        let loaded = LoadedJobs {
            pending: pending
                .into_iter()
                .map(|(id, request)| PendingRecord {
                    id,
                    request,
                    job: None,
                })
                .collect(),
            finished: Vec::new(),
        };
        return Ok(Some((Box::new(journal), loaded)));
    }
    if let Some(path) = &config.store {
        let (sqlite, loaded) =
            SqliteStore::open_at(path, config.result_ttl, config.max_jobs, now_ms)?;
        return Ok(Some((Box::new(sqlite), loaded)));
    }
    Ok(None)
}

pub(crate) struct SqliteStore {
    conn: Mutex<Connection>,
    result_ttl: Duration,
    #[cfg(unix)]
    path: PathBuf,
    #[cfg(unix)]
    secured_wal: AtomicBool,
}

impl SqliteStore {
    pub(crate) fn open_at(
        path: &Path,
        result_ttl: Duration,
        max_finished: usize,
        now_ms: u64,
    ) -> Result<(Self, LoadedJobs), ConfigError> {
        #[cfg(unix)]
        if !is_memory_path(path) {
            precreate_owner_only(path);
        }

        let conn = Connection::open(path).map_err(|e| ConfigError::Store(e.to_string()))?;
        conn.busy_timeout(Duration::from_millis(SQLITE_BUSY_TIMEOUT_MS))
            .map_err(|e| ConfigError::Store(e.to_string()))?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             CREATE TABLE IF NOT EXISTS jobs (
                 id TEXT PRIMARY KEY,
                 status TEXT NOT NULL,
                 request_json TEXT NOT NULL,
                 job_json TEXT NOT NULL,
                 created_at INTEGER NOT NULL,
                 finished_at INTEGER
             );
             CREATE INDEX IF NOT EXISTS idx_jobs_status_finished
                 ON jobs (status, finished_at);",
        )
        .map_err(|e| ConfigError::Store(e.to_string()))?;

        #[cfg(unix)]
        if !is_memory_path(path) {
            secure_sqlite_files(path);
        }

        purge_expired_conn(&conn, result_ttl, now_ms)
            .map_err(|e| ConfigError::Store(e.to_string()))?;

        let pending = load_pending(&conn).map_err(|e| ConfigError::Store(e.to_string()))?;
        let finished =
            load_finished(&conn, max_finished).map_err(|e| ConfigError::Store(e.to_string()))?;

        Ok((
            Self {
                conn: Mutex::new(conn),
                result_ttl,
                #[cfg(unix)]
                path: path.to_path_buf(),
                #[cfg(unix)]
                secured_wal: AtomicBool::new(false),
            },
            LoadedJobs { pending, finished },
        ))
    }

    #[cfg(test)]
    pub(crate) fn purge_expired(&self, now_ms: u64) -> Result<usize, String> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        purge_expired_conn(&conn, self.result_ttl, now_ms).map_err(|e| e.to_string())
    }

    fn secure_wal_once(&self) {
        #[cfg(unix)]
        if !is_memory_path(&self.path) && !self.secured_wal.swap(true, Ordering::Relaxed) {
            secure_sqlite_files(&self.path);
        }
    }
}

impl JobStore for SqliteStore {
    fn submitted(&self, job: &Job, request: &Value) {
        let Ok(request_json) = serde_json::to_string(request) else {
            tracing::warn!(job_id = %job.id, "could not serialize job request for SQLite store");
            return;
        };
        let Ok(job_json) = serde_json::to_string(job) else {
            tracing::warn!(job_id = %job.id, "could not serialize job snapshot for SQLite store");
            return;
        };
        let id = job.id.to_string();
        let status = status_str(job.status);
        let created_at = job.submitted_at.min(i64::MAX as u64) as i64;
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let res = conn.execute(
            "INSERT OR REPLACE INTO jobs (id, status, request_json, job_json, created_at, finished_at)
             VALUES (?1, ?2, ?3, ?4, ?5, NULL)",
            params![id, status, request_json, job_json, created_at],
        );
        if let Err(e) = res {
            tracing::warn!(error = %e, job_id = %job.id, "could not persist submitted job to SQLite store");
        } else {
            self.secure_wal_once();
        }
    }

    fn finished(&self, job: &Job, now_ms: u64) {
        let Ok(job_json) = serde_json::to_string(job) else {
            tracing::warn!(job_id = %job.id, "could not serialize finished job for SQLite store");
            return;
        };
        let id = job.id.to_string();
        let status = status_str(job.status);
        let finished_at = job.finished_at.unwrap_or(now_ms).min(i64::MAX as u64) as i64;
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let res = conn
            .execute(
                "UPDATE jobs SET status = ?2, job_json = ?3, finished_at = ?4 WHERE id = ?1",
                params![id, status, job_json, finished_at],
            )
            .and_then(|_| purge_expired_conn(&conn, self.result_ttl, now_ms));
        if let Err(e) = res {
            tracing::warn!(error = %e, job_id = %job.id, "could not persist finished job to SQLite store");
        } else {
            self.secure_wal_once();
        }
    }

    fn invalid_pending(&self, id: Uuid) {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        if let Err(e) = conn.execute("DELETE FROM jobs WHERE id = ?1", params![id.to_string()]) {
            tracing::warn!(error = %e, job_id = %id, "could not delete invalid pending job from SQLite store");
        }
    }
}

fn status_str(status: JobStatus) -> &'static str {
    match status {
        JobStatus::Queued => "queued",
        JobStatus::Running => "running",
        JobStatus::Succeeded => "succeeded",
        JobStatus::Failed => "failed",
        JobStatus::Cancelled => "cancelled",
    }
}

fn ttl_cutoff_ms(result_ttl: Duration, now_ms: u64) -> i64 {
    let ttl_ms = result_ttl.as_millis().min(i64::MAX as u128) as i64;
    (now_ms.min(i64::MAX as u64) as i64).saturating_sub(ttl_ms)
}

fn purge_expired_conn(
    conn: &Connection,
    result_ttl: Duration,
    now_ms: u64,
) -> rusqlite::Result<usize> {
    let cutoff = ttl_cutoff_ms(result_ttl, now_ms);
    conn.execute(
        "DELETE FROM jobs
         WHERE status IN ('succeeded', 'failed', 'cancelled')
           AND (finished_at <= ?1 OR (finished_at IS NULL AND created_at <= ?1))",
        params![cutoff],
    )
}

fn load_pending(conn: &Connection) -> rusqlite::Result<Vec<PendingRecord>> {
    let mut stmt = conn.prepare(
        "SELECT id, status, request_json, job_json, created_at
         FROM jobs
         WHERE status IN ('queued', 'running')
         ORDER BY created_at ASC, rowid ASC",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, i64>(4)?,
        ))
    })?;

    let mut out = Vec::new();
    let mut delete_ids: Vec<String> = Vec::new();
    let mut reset_rows: Vec<(String, String)> = Vec::new();

    for row in rows {
        let (id_str, status, request_json, job_json, created_at) = row?;
        let Ok(id) = Uuid::parse_str(&id_str) else {
            delete_ids.push(id_str);
            continue;
        };
        let Ok(request) = serde_json::from_str::<Value>(&request_json) else {
            tracing::warn!(job_id = %id, "skipping corrupt request_json in SQLite store");
            delete_ids.push(id_str);
            continue;
        };
        let mut job = match serde_json::from_str::<Job>(&job_json) {
            Ok(j) => j,
            Err(e) => {
                tracing::warn!(error = %e, job_id = %id, "reconstructing pending job snapshot from SQLite store");
                Job {
                    id,
                    status: JobStatus::Queued,
                    submitted_at: created_at.max(0) as u64,
                    started_at: None,
                    finished_at: None,
                    attempts: 0,
                    step_index: None,
                    results: None,
                    error: None,
                    trace_id: None,
                    failure_evidence: None,
                }
            }
        };
        let needs_reset = status == "running" || job.status != JobStatus::Queued;
        job.id = id;
        job.status = JobStatus::Queued;
        job.started_at = None;
        job.finished_at = None;
        job.attempts = 0;
        job.step_index = None;
        job.results = None;
        job.error = None;
        job.failure_evidence = None;
        if needs_reset {
            if let Ok(updated_json) = serde_json::to_string(&job) {
                reset_rows.push((id_str, updated_json));
            }
        }
        out.push(PendingRecord {
            id,
            request,
            job: Some(job),
        });
    }
    drop(stmt);

    for bad_id in delete_ids {
        let _ = conn.execute("DELETE FROM jobs WHERE id = ?1", params![bad_id]);
    }
    for (id_str, updated_json) in reset_rows {
        let _ = conn.execute(
            "UPDATE jobs SET status = 'queued', job_json = ?2 WHERE id = ?1",
            params![id_str, updated_json],
        );
    }

    Ok(out)
}

fn load_finished(conn: &Connection, max_finished: usize) -> rusqlite::Result<Vec<FinishedRecord>> {
    if max_finished == 0 {
        return Ok(Vec::new());
    }
    let limit = max_finished.min(i64::MAX as usize) as i64;
    let mut stmt = conn.prepare(
        "SELECT id, request_json, job_json, finished_at
         FROM jobs
         WHERE status IN ('succeeded', 'failed', 'cancelled')
         ORDER BY COALESCE(finished_at, created_at) DESC, rowid DESC
         LIMIT ?1",
    )?;
    let rows = stmt.query_map(params![limit], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, Option<i64>>(3)?,
        ))
    })?;

    let mut out = Vec::new();
    for row in rows {
        let (id_str, request_json, job_json, finished_at) = row?;
        let Ok(mut job) = serde_json::from_str::<Job>(&job_json) else {
            tracing::warn!(job_id = %id_str, "skipping corrupt finished job_json in SQLite store");
            continue;
        };
        if job.finished_at.is_none() {
            job.finished_at = finished_at.map(|v| v.max(0) as u64);
        }
        out.push(FinishedRecord { job, request_json });
    }
    Ok(out)
}

#[cfg(unix)]
fn is_memory_path(path: &Path) -> bool {
    path == Path::new(":memory:") || path.to_string_lossy().starts_with("file::memory:")
}

#[cfg(unix)]
fn precreate_owner_only(path: &Path) {
    use std::os::unix::fs::OpenOptionsExt;
    let _ = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path);
}

#[cfg(unix)]
fn ensure_owner_only(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if path.exists() {
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
}

#[cfg(unix)]
fn secure_sqlite_files(path: &Path) {
    ensure_owner_only(path);
    for suffix in ["-wal", "-shm"] {
        let mut sibling = path.as_os_str().to_os_string();
        sibling.push(suffix);
        ensure_owner_only(Path::new(&sibling));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ConsoleEntry, FailureEvidence, NetworkEntry};
    use serde_json::json;

    fn temp_sqlite_path(tag: &str) -> PathBufGuard {
        let path = std::env::temp_dir().join(format!("fluxwright-{tag}-{}.sqlite", Uuid::new_v4()));
        PathBufGuard(path)
    }

    struct PathBufGuard(std::path::PathBuf);

    impl Drop for PathBufGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
            for suffix in ["-wal", "-shm"] {
                let mut sibling = self.0.as_os_str().to_os_string();
                sibling.push(suffix);
                let _ = std::fs::remove_file(Path::new(&sibling));
            }
        }
    }

    fn sample_job(id: Uuid, submitted_at: u64) -> Job {
        Job {
            id,
            status: JobStatus::Queued,
            submitted_at,
            started_at: None,
            finished_at: None,
            attempts: 0,
            step_index: None,
            results: None,
            error: None,
            trace_id: Some("0123456789abcdef0123456789abcdef".into()),
            failure_evidence: None,
        }
    }

    #[test]
    fn sqlite_store_configures_wal_busy_timeout_schema_and_permissions() {
        let db = temp_sqlite_path("schema");
        let (store, loaded) =
            SqliteStore::open_at(&db.0, Duration::from_secs(3600), 128, 1_000).unwrap();
        assert!(loaded.pending.is_empty());
        assert!(loaded.finished.is_empty());

        let conn = store.conn.lock().unwrap();
        let mode: String = conn
            .query_row("PRAGMA journal_mode;", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode.to_ascii_lowercase(), "wal");
        let timeout: i64 = conn
            .query_row("PRAGMA busy_timeout;", [], |r| r.get(0))
            .unwrap();
        assert_eq!(timeout, 5_000);
        let idx_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = 'idx_jobs_status_finished';",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(idx_count, 1);
        drop(conn);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&db.0).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "sqlite store must be owner-only 0o600");
        }
    }

    #[test]
    fn sqlite_store_persists_pending_running_and_finished_jobs_across_reopen() {
        let db = temp_sqlite_path("persist");
        let (store, _) =
            SqliteStore::open_at(&db.0, Duration::from_secs(3600), 128, 1_000).unwrap();

        let id_queued = Uuid::new_v4();
        let id_running = Uuid::new_v4();
        let id_ok = Uuid::new_v4();
        let id_fail = Uuid::new_v4();
        let id_cancel = Uuid::new_v4();

        let req = json!({ "steps": [{ "action": "title" }] });
        store.submitted(&sample_job(id_queued, 1_000), &req);
        store.submitted(&sample_job(id_running, 1_100), &req);
        store.submitted(&sample_job(id_ok, 1_200), &req);
        store.submitted(&sample_job(id_fail, 1_300), &req);
        store.submitted(&sample_job(id_cancel, 1_400), &req);

        // Simulate a row transitioning to 'running' in SQLite (e.g. mid-flight crash).
        {
            let mut running_job = sample_job(id_running, 1_100);
            running_job.status = JobStatus::Running;
            running_job.started_at = Some(1_110);
            running_job.attempts = 1;
            let conn = store.conn.lock().unwrap();
            conn.execute(
                "UPDATE jobs SET status = 'running', job_json = ?2 WHERE id = ?1",
                params![
                    id_running.to_string(),
                    serde_json::to_string(&running_job).unwrap()
                ],
            )
            .unwrap();
        }

        let mut ok_job = sample_job(id_ok, 1_200);
        ok_job.status = JobStatus::Succeeded;
        ok_job.started_at = Some(1_210);
        ok_job.finished_at = Some(1_250);
        ok_job.attempts = 1;
        ok_job.results = Some(vec![json!("Example Domain")]);
        store.finished(&ok_job, 1_250);

        let mut evidence = FailureEvidence::new();
        evidence
            .console
            .push(ConsoleEntry::new("error", "boom console"));
        evidence.network.push(NetworkEntry {
            url: "https://example.com".into(),
            method: "GET".into(),
            status: Some(500),
            resource_type: Some("document".into()),
            failure: None,
        });
        evidence.screenshot = Some("iVBORtest".into());

        let mut fail_job = sample_job(id_fail, 1_300);
        fail_job.status = JobStatus::Failed;
        fail_job.started_at = Some(1_310);
        fail_job.finished_at = Some(1_360);
        fail_job.attempts = 2;
        fail_job.error = Some("step failed".into());
        fail_job.failure_evidence = Some(evidence.clone());
        store.finished(&fail_job, 1_360);

        let mut cancel_job = sample_job(id_cancel, 1_400);
        cancel_job.status = JobStatus::Cancelled;
        cancel_job.finished_at = Some(1_420);
        store.finished(&cancel_job, 1_420);

        drop(store);

        let (reopened, loaded) =
            SqliteStore::open_at(&db.0, Duration::from_secs(3600), 128, 2_000).unwrap();
        let pending_ids: Vec<Uuid> = loaded.pending.iter().map(|p| p.id).collect();
        assert_eq!(pending_ids, vec![id_queued, id_running]);
        for p in &loaded.pending {
            let j = p.job.as_ref().unwrap();
            assert_eq!(j.status, JobStatus::Queued);
            assert_eq!(j.attempts, 0);
            assert!(j.started_at.is_none());
            assert!(j.finished_at.is_none());
        }

        // Finished rows are returned newest-first by finished_at.
        let finished_ids: Vec<Uuid> = loaded.finished.iter().map(|f| f.job.id).collect();
        assert_eq!(finished_ids, vec![id_cancel, id_fail, id_ok]);
        let restored_ok = &loaded.finished[2].job;
        assert_eq!(restored_ok.status, JobStatus::Succeeded);
        assert_eq!(restored_ok.results, Some(vec![json!("Example Domain")]));
        assert_eq!(restored_ok.finished_at, Some(1_250));

        let restored_fail = &loaded.finished[1].job;
        assert_eq!(restored_fail.status, JobStatus::Failed);
        assert_eq!(restored_fail.attempts, 2);
        assert_eq!(restored_fail.error.as_deref(), Some("step failed"));
        assert_eq!(restored_fail.failure_evidence, Some(evidence));

        let restored_cancel = &loaded.finished[0].job;
        assert_eq!(restored_cancel.status, JobStatus::Cancelled);
        drop(reopened);
    }

    #[test]
    fn sqlite_store_prunes_expired_terminal_rows_and_keeps_unfinished() {
        let db = temp_sqlite_path("ttl");
        let ttl = Duration::from_secs(10); // 10_000 ms
        let (store, _) = SqliteStore::open_at(&db.0, ttl, 128, 1_000).unwrap();

        let id_pending = Uuid::new_v4();
        let id_old_done = Uuid::new_v4();
        let id_fresh_done = Uuid::new_v4();
        let req = json!({ "steps": [{ "action": "title" }] });

        store.submitted(&sample_job(id_pending, 1_000), &req);
        store.submitted(&sample_job(id_old_done, 1_000), &req);
        store.submitted(&sample_job(id_fresh_done, 25_000), &req);

        let mut old_done = sample_job(id_old_done, 1_000);
        old_done.status = JobStatus::Succeeded;
        old_done.finished_at = Some(5_000);
        store.finished(&old_done, 5_000);

        let mut fresh_done = sample_job(id_fresh_done, 25_000);
        fresh_done.status = JobStatus::Succeeded;
        fresh_done.finished_at = Some(26_000);
        // Finishing at t=26_000 with ttl=10_000 (cutoff=16_000) purges old_done (5_000 <= 16_000).
        store.finished(&fresh_done, 26_000);
        drop(store);

        // Reopen at t=30_000 (cutoff=20_000): pending stays regardless of age; fresh_done stays.
        let (store2, loaded) = SqliteStore::open_at(&db.0, ttl, 128, 30_000).unwrap();
        assert_eq!(loaded.pending.len(), 1);
        assert_eq!(loaded.pending[0].id, id_pending);
        assert_eq!(loaded.finished.len(), 1);
        assert_eq!(loaded.finished[0].job.id, id_fresh_done);

        // At t=40_000 (cutoff=30_000), fresh_done (26_000) is purged while pending survives.
        let purged = store2.purge_expired(40_000).unwrap();
        assert_eq!(purged, 1);
        drop(store2);

        let (_, loaded_after) = SqliteStore::open_at(&db.0, ttl, 128, 40_000).unwrap();
        assert_eq!(loaded_after.pending.len(), 1);
        assert!(loaded_after.finished.is_empty());
    }

    #[test]
    fn sqlite_store_zero_ttl_does_not_retain_terminal_jobs_across_reopen() {
        let db = temp_sqlite_path("zero-ttl");
        let (store, _) = SqliteStore::open_at(&db.0, Duration::ZERO, 128, 1_000).unwrap();
        let id = Uuid::new_v4();
        let req = json!({ "steps": [{ "action": "title" }] });
        store.submitted(&sample_job(id, 1_000), &req);
        let mut done = sample_job(id, 1_000);
        done.status = JobStatus::Succeeded;
        done.finished_at = Some(1_100);
        store.finished(&done, 1_100);
        drop(store);

        let (_, loaded) = SqliteStore::open_at(&db.0, Duration::ZERO, 128, 1_200).unwrap();
        assert!(loaded.finished.is_empty());
    }

    #[test]
    fn rejects_both_journal_and_store_simultaneously() {
        let mut config =
            ServerConfig::new("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into())
                .unwrap();
        config.journal = Some(std::path::PathBuf::from("a.jsonl"));
        config.store = Some(std::path::PathBuf::from("b.sqlite"));
        assert!(matches!(
            open_job_store(&config, 1_000),
            Err(ConfigError::ConflictingStore)
        ));
    }
}
