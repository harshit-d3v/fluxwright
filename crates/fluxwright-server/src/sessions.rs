//! Managed persistent cloud browser sessions (`POST /v1/sessions`, `/v1/sessions/{id}`,
//! `/v1/sessions/{id}/extend`, `/v1/sessions/{id}/cdp`, `/v1/sessions/{id}/replay`,
//! `/v1/sessions/{id}/network`).
//!
//! Mints isolated, long-lived Chrome contexts backed by the pooled engine (or opt-in external
//! BYOB CDP endpoints), paired with a live screencast viewer URL, reconnectable WebSocket CDP
//! endpoint, and searchable session replay & network/HAR inspector.

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv6Addr};
use std::path::{Path as StdPath, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, UNIX_EPOCH};

use axum::body::to_bytes;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{FromRequestParts, Path, RawQuery, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use base64::Engine;
use fluxwright::{
    netguard, validate_profile_name, BrowserEngine, CdpEvent, ContextLease, JobOptions, Priority,
    StorageState,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::{mpsc, watch, Mutex, Notify};
use tokio::time::Instant;
use uuid::Uuid;

use crate::cdp_gateway::{
    compute_enforce_server_proxy, create_initial_page, detach_client_sessions,
    ensure_preset_blocker, ensure_session_observer, host_base, percent_decode_str,
    run_gateway_loop, spawn_fail_blocked_request, teardown_gateway_state, GatewayPolicy,
    GatewayState,
};
use crate::job::{JobPriority, ServerResourcePreset};
use crate::live::{EmbedMode, LiveGuard};
use crate::replay::{RetainedReplay, RetainedReplayStore, SessionRecorder};
use crate::tenants::TenantIdentity;
use crate::{now_ms, ApiError, Inner};

pub(crate) const MAX_MANAGED_SESSIONS: usize = 64;
pub(crate) const MAX_SESSION_FILE_BYTES: usize = 4 * 1024 * 1024;
pub(crate) const MAX_SESSION_TOTAL_BYTES: u64 = 16 * 1024 * 1024;
pub(crate) const MAX_SESSION_FILES: usize = 32;
pub(crate) const MAX_SESSION_FILE_BODY_BYTES: usize = 6 * 1024 * 1024;
const MAX_SESSION_EXTENSIONS: usize = 8;
const DEFAULT_SESSION_TTL_SECS: u64 = 300;
const MIN_SESSION_TTL_SECS: u64 = 10;
const MAX_SESSION_TTL_SECS: u64 = 3600;
const PROFILE_SAVE_TIMEOUT: Duration = Duration::from_secs(3);
const DELETE_WAIT_TIMEOUT: Duration = Duration::from_secs(5);
const BYOB_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);
const MIN_FRAME_CAPTURE_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SessionViewport {
    pub(crate) width: u32,
    pub(crate) height: u32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CreateSessionRequest {
    #[serde(default = "default_ttl_secs")]
    pub(crate) ttl_secs: u64,
    #[serde(default = "default_true")]
    pub(crate) keep_alive: bool,
    #[serde(default)]
    pub(crate) priority: JobPriority,
    #[serde(default)]
    pub(crate) resource_preset: Option<ServerResourcePreset>,
    #[serde(default)]
    pub(crate) block_cookie_banners: bool,
    #[serde(default)]
    pub(crate) profile: Option<String>,
    #[serde(default = "default_true")]
    pub(crate) persist_profile: bool,
    #[serde(default)]
    pub(crate) proxy_session: Option<String>,
    #[serde(default)]
    pub(crate) viewport: Option<SessionViewport>,
    #[serde(default)]
    pub(crate) live_view_mode: EmbedMode,
    #[serde(default)]
    pub(crate) record_frames: bool,
    #[serde(default)]
    pub(crate) cdp_endpoint: Option<String>,
    #[serde(default)]
    pub(crate) extensions: Vec<String>,
}

impl Default for CreateSessionRequest {
    fn default() -> Self {
        Self {
            ttl_secs: DEFAULT_SESSION_TTL_SECS,
            keep_alive: true,
            priority: JobPriority::Normal,
            resource_preset: None,
            block_cookie_banners: false,
            profile: None,
            persist_profile: true,
            proxy_session: None,
            viewport: None,
            live_view_mode: EmbedMode::ReadOnly,
            record_frames: false,
            cdp_endpoint: None,
            extensions: Vec::new(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExtendSessionRequest {
    #[serde(default = "default_ttl_secs")]
    ttl_secs: u64,
}

impl Default for ExtendSessionRequest {
    fn default() -> Self {
        Self {
            ttl_secs: DEFAULT_SESSION_TTL_SECS,
        }
    }
}

fn default_ttl_secs() -> u64 {
    DEFAULT_SESSION_TTL_SECS
}

fn default_true() -> bool {
    true
}

impl CreateSessionRequest {
    fn validate(&self) -> Result<(), String> {
        validate_ttl(self.ttl_secs)?;
        if let Some(ref profile) = self.profile {
            validate_profile_name(profile).map_err(|e| e.to_string())?;
        }
        if let Some(ref session) = self.proxy_session {
            if session.trim().is_empty()
                || session.len() > 128
                || !session.bytes().all(|b| (0x20..=0x7e).contains(&b))
            {
                return Err("proxy_session must be 1-128 printable ASCII characters".into());
            }
        }
        if let Some(vp) = self.viewport {
            if !(1..=7680).contains(&vp.width) || !(1..=4320).contains(&vp.height) {
                return Err("viewport dimensions must be within 1..=7680 x 1..=4320".into());
            }
        }
        if self.extensions.len() > MAX_SESSION_EXTENSIONS {
            return Err(format!(
                "extensions may contain at most {MAX_SESSION_EXTENSIONS} entries"
            ));
        }
        if !self.extensions.is_empty() && self.cdp_endpoint.is_some() {
            return Err("extensions cannot be combined with cdp_endpoint".into());
        }
        let mut seen_ext = HashSet::with_capacity(self.extensions.len());
        for ext in &self.extensions {
            validate_safe_entry_name(ext)
                .map_err(|msg| format!("invalid extension name '{ext}': {msg}"))?;
            if !seen_ext.insert(ext.as_str()) {
                return Err(format!("duplicate extension name '{ext}'"));
            }
        }
        Ok(())
    }

    fn job_options(&self) -> JobOptions {
        let priority = match self.priority {
            JobPriority::Low => Priority::Low,
            JobPriority::Normal => Priority::Normal,
            JobPriority::High => Priority::High,
        };
        let mut opts = JobOptions::default()
            .priority(priority)
            .block_cookie_banners(self.block_cookie_banners)
            .persist_profile(self.persist_profile);
        if let Some(ref profile) = self.profile {
            opts = opts.profile(profile.clone());
        }
        if let Some(ref session) = self.proxy_session {
            opts = opts.proxy_session(session.clone());
        }
        opts
    }
}

fn validate_ttl(ttl_secs: u64) -> Result<(), String> {
    if !(MIN_SESSION_TTL_SECS..=MAX_SESSION_TTL_SECS).contains(&ttl_secs) {
        return Err(format!(
            "ttl_secs must be between {MIN_SESSION_TTL_SECS} and {MAX_SESSION_TTL_SECS}"
        ));
    }
    Ok(())
}

fn validate_session_id(id: &str) -> Result<(), ApiError> {
    let valid = !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'));
    if !valid {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_session_id",
            "invalid managed session id",
        ));
    }
    Ok(())
}

pub(crate) fn is_session_cdp_path(path: &str) -> bool {
    path.strip_prefix("/v1/sessions/")
        .and_then(|rest| rest.strip_suffix("/cdp"))
        .is_some_and(|id| !id.is_empty() && !id.contains('/'))
}

fn validate_safe_entry_name(name: &str) -> Result<(), &'static str> {
    if name.is_empty() || name.len() > 128 {
        return Err("must be 1-128 ASCII characters");
    }
    if name.starts_with('.') || name.contains("..") || name.contains('/') || name.contains('\\') {
        return Err("must not start with '.' or contain '..', '/', or '\\'");
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    {
        return Err("must contain only [A-Za-z0-9._-]");
    }
    Ok(())
}

pub(crate) fn validate_session_filename(name: &str) -> Result<(), ApiError> {
    validate_safe_entry_name(name).map_err(|msg| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_filename",
            format!("invalid session filename: {msg}"),
        )
    })
}

pub(crate) fn resolve_session_extensions(
    extensions_dir: Option<&StdPath>,
    names: &[String],
) -> Result<Vec<PathBuf>, ApiError> {
    if names.is_empty() {
        return Ok(Vec::new());
    }
    if names.len() > MAX_SESSION_EXTENSIONS {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            format!("extensions may contain at most {MAX_SESSION_EXTENSIONS} entries"),
        ));
    }
    let Some(base_dir) = extensions_dir else {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "extensions_not_configured",
            "extensions_dir is not configured on this server",
        ));
    };
    let base_canon = std::fs::canonicalize(base_dir).map_err(|_| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "extensions_not_configured",
            "extensions_dir does not exist or is unreadable",
        )
    })?;
    if !base_canon.is_dir() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "extensions_not_configured",
            "extensions_dir is not a directory",
        ));
    }
    let mut resolved = Vec::with_capacity(names.len());
    for name in names {
        validate_safe_entry_name(name).map_err(|msg| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                "invalid_extension",
                format!("invalid extension name '{name}': {msg}"),
            )
        })?;
        let candidate = base_canon.join(name);
        let canon = std::fs::canonicalize(&candidate).map_err(|_| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                "extension_not_found",
                format!("extension '{name}' not found"),
            )
        })?;
        if !canon.starts_with(&base_canon) || canon == base_canon {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "invalid_extension",
                format!("extension '{name}' escapes extensions_dir"),
            ));
        }
        if !canon.is_dir() {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "invalid_extension",
                format!("extension '{name}' is not a directory"),
            ));
        }
        let manifest_path = canon.join("manifest.json");
        let manifest_canon = std::fs::canonicalize(&manifest_path).map_err(|_| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                "invalid_extension",
                format!("extension '{name}' is missing manifest.json"),
            )
        })?;
        if !manifest_canon.starts_with(&canon) || !manifest_canon.is_file() {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "invalid_extension",
                format!("extension '{name}' has an invalid manifest.json"),
            ));
        }
        resolved.push(canon);
    }
    Ok(resolved)
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct SessionFileInfo {
    pub(crate) name: String,
    pub(crate) size: u64,
    pub(crate) modified_at: u64,
}

pub(crate) fn list_files_in_dir(dir: &StdPath) -> Result<Vec<SessionFileInfo>, ApiError> {
    if !matches!(dir.try_exists(), Ok(true)) {
        return Ok(Vec::new());
    }
    let dir_meta = std::fs::symlink_metadata(dir).map_err(|e| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "file_error",
            format!("failed to inspect session files directory: {e}"),
        )
    })?;
    if !dir_meta.file_type().is_dir() {
        return Err(ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "file_error",
            "session files path is not a directory",
        ));
    }
    let mut files = Vec::new();
    let entries = std::fs::read_dir(dir).map_err(|e| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "file_error",
            format!("failed to list session files: {e}"),
        )
    })?;
    for entry in entries.flatten() {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if validate_safe_entry_name(&name).is_err() {
            continue;
        }
        let Ok(meta) = std::fs::symlink_metadata(entry.path()) else {
            continue;
        };
        if !meta.file_type().is_file() {
            continue;
        }
        let modified_at = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        files.push(SessionFileInfo {
            name,
            size: meta.len(),
            modified_at,
        });
    }
    files.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(files)
}

pub(crate) fn write_file_in_dir(
    dir: &StdPath,
    name: &str,
    bytes: &[u8],
) -> Result<(u64, String), ApiError> {
    validate_session_filename(name)?;
    if bytes.len() > MAX_SESSION_FILE_BYTES {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "file_too_large",
            format!("session file exceeds {MAX_SESSION_FILE_BYTES} bytes"),
        ));
    }
    std::fs::create_dir_all(dir).map_err(|e| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "file_error",
            format!("failed to create session files directory: {e}"),
        )
    })?;
    let dir_meta = std::fs::symlink_metadata(dir).map_err(|e| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "file_error",
            format!("failed to inspect session files directory: {e}"),
        )
    })?;
    if !dir_meta.file_type().is_dir() {
        return Err(ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "file_error",
            "session files path is not a directory",
        ));
    }
    let target = dir.join(name);
    let mut existed = false;
    if let Ok(meta) = std::fs::symlink_metadata(&target) {
        if !meta.file_type().is_file() {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "invalid_file",
                "refusing to write to a symlink or non-regular file",
            ));
        }
        existed = true;
    }

    let mut other_count = 0usize;
    let mut other_bytes = 0u64;
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let Ok(meta) = std::fs::symlink_metadata(entry.path()) else {
                continue;
            };
            if !meta.file_type().is_file() {
                continue;
            }
            if entry.file_name().to_str() == Some(name) {
                continue;
            }
            other_count += 1;
            other_bytes = other_bytes.saturating_add(meta.len());
        }
    }
    if !existed && other_count >= MAX_SESSION_FILES {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "file_limit_exceeded",
            format!("session may store at most {MAX_SESSION_FILES} files"),
        ));
    }
    if other_bytes.saturating_add(bytes.len() as u64) > MAX_SESSION_TOTAL_BYTES {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "session_storage_exceeded",
            format!("session file storage exceeds {MAX_SESSION_TOTAL_BYTES} bytes"),
        ));
    }

    std::fs::write(&target, bytes).map_err(|e| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "file_error",
            format!("failed to write session file: {e}"),
        )
    })?;
    let dir_canon = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    let file_canon = std::fs::canonicalize(&target).unwrap_or_else(|_| target.clone());
    if !file_canon.starts_with(&dir_canon) {
        let _ = std::fs::remove_file(&target);
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_file",
            "session file path escaped session directory",
        ));
    }
    Ok((bytes.len() as u64, file_canon.to_string_lossy().into_owned()))
}

pub(crate) fn read_file_in_dir(dir: &StdPath, name: &str) -> Result<Vec<u8>, ApiError> {
    validate_session_filename(name)?;
    let target = dir.join(name);
    let meta = std::fs::symlink_metadata(&target).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            ApiError::new(StatusCode::NOT_FOUND, "file_not_found", "session file not found")
        } else {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "file_error",
                format!("failed to inspect session file: {e}"),
            )
        }
    })?;
    if !meta.file_type().is_file() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_file",
            "refusing to read symlink or non-regular file",
        ));
    }
    if meta.len() > MAX_SESSION_FILE_BYTES as u64 {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "file_too_large",
            format!("session file exceeds {MAX_SESSION_FILE_BYTES} bytes"),
        ));
    }
    let dir_canon = std::fs::canonicalize(dir).map_err(|_| {
        ApiError::new(StatusCode::NOT_FOUND, "file_not_found", "session file not found")
    })?;
    let file_canon = std::fs::canonicalize(&target).map_err(|_| {
        ApiError::new(StatusCode::NOT_FOUND, "file_not_found", "session file not found")
    })?;
    if !file_canon.starts_with(&dir_canon) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_file",
            "session file path escaped session directory",
        ));
    }
    std::fs::read(&file_canon).map_err(|e| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "file_error",
            format!("failed to read session file: {e}"),
        )
    })
}

pub(crate) fn delete_file_in_dir(dir: &StdPath, name: &str) -> Result<(), ApiError> {
    validate_session_filename(name)?;
    let target = dir.join(name);
    let meta = std::fs::symlink_metadata(&target).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            ApiError::new(StatusCode::NOT_FOUND, "file_not_found", "session file not found")
        } else {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "file_error",
                format!("failed to inspect session file: {e}"),
            )
        }
    })?;
    if !meta.file_type().is_file() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_file",
            "refusing to delete symlink or non-regular file",
        ));
    }
    std::fs::remove_file(&target).map_err(|e| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "file_error",
            format!("failed to delete session file: {e}"),
        )
    })
}

#[derive(Default)]
struct SessionMap {
    sessions: HashMap<String, Arc<ManagedSession>>,
    reserved: usize,
    retained: RetainedReplayStore,
    retained_tenants: HashMap<String, String>,
}

struct SlotReservation {
    manager: Arc<StdMutex<SessionMap>>,
    committed: bool,
}

impl Drop for SlotReservation {
    fn drop(&mut self) {
        if !self.committed {
            let mut g = self.manager.lock().unwrap();
            g.reserved = g.reserved.saturating_sub(1);
        }
    }
}

pub(crate) struct ManagedSession {
    id: String,
    tenant: String,
    keep_alive: bool,
    profile: Option<String>,
    persist_profile: bool,
    proxy_session: Option<String>,
    record_frames: bool,
    is_byob: bool,
    extensions: Vec<String>,
    byob_engine: Option<BrowserEngine>,
    created_at: u64,
    lease: Arc<ContextLease>,
    state: Arc<Mutex<GatewayState>>,
    live_url: Arc<StdMutex<String>>,
    recorder: Arc<StdMutex<SessionRecorder>>,
    policy: GatewayPolicy,
    connected: AtomicBool,
    expires_tx: watch::Sender<(Instant, u64)>,
    close_tx: watch::Sender<bool>,
    closed_notify: Arc<Notify>,
}

impl ManagedSession {
    fn current_url(&self) -> String {
        let u = self.live_url.lock().unwrap();
        if u.is_empty() {
            String::from("about:blank")
        } else {
            u.clone()
        }
    }

    fn summary(&self) -> Value {
        let (_, expires_at) = *self.expires_tx.borrow();
        json!({
            "id": self.id,
            "status": if *self.close_tx.borrow() { "closing" } else { "active" },
            "cdp_path": format!("/v1/sessions/{}/cdp", self.id),
            "keep_alive": self.keep_alive,
            "connected": self.connected.load(Ordering::Acquire),
            "url": self.current_url(),
            "profile": self.profile,
            "proxy_session": self.proxy_session,
            "record_frames": self.record_frames,
            "byob": self.is_byob,
            "extensions": self.extensions,
            "created_at": self.created_at,
            "expires_at": expires_at,
        })
    }

    fn dashboard_summary(&self) -> Value {
        let (_, expires_at) = *self.expires_tx.borrow();
        let url = self.current_url();
        let rec = self.recorder.lock().unwrap();
        json!({
            "id": self.id,
            "status": if *self.close_tx.borrow() { "closing" } else { "active" },
            "cdp_path": format!("/v1/sessions/{}/cdp", self.id),
            "keep_alive": self.keep_alive,
            "connected": self.connected.load(Ordering::Acquire),
            "url": url,
            "profile": self.profile,
            "proxy_session": self.proxy_session,
            "byob": self.is_byob,
            "extensions": self.extensions,
            "record_frames": self.record_frames,
            "created_at": self.created_at,
            "expires_at": expires_at,
            "closed_at": Value::Null,
            "event_count": rec.timeline.len(),
            "network_count": rec.network.len(),
            "frame_count": rec.frame_count,
        })
    }
}

#[derive(Default)]
pub(crate) struct SessionStoreState {
    inner: Arc<StdMutex<SessionMap>>,
}

impl SessionStoreState {
    fn try_reserve(&self) -> Option<SlotReservation> {
        let mut g = self.inner.lock().unwrap();
        if g.sessions.len() + g.reserved >= MAX_MANAGED_SESSIONS {
            return None;
        }
        g.reserved += 1;
        Some(SlotReservation {
            manager: self.inner.clone(),
            committed: false,
        })
    }

    fn insert(&self, mut reservation: SlotReservation, session: Arc<ManagedSession>) {
        let mut g = self.inner.lock().unwrap();
        g.reserved = g.reserved.saturating_sub(1);
        g.sessions.insert(session.id.clone(), session);
        reservation.committed = true;
    }

    fn get(&self, id: &str) -> Option<Arc<ManagedSession>> {
        self.inner.lock().unwrap().sessions.get(id).cloned()
    }

    fn close_and_retain(
        &self,
        session: &ManagedSession,
        url: String,
        recorder: SessionRecorder,
    ) {
        let closed_at = now_ms();
        let retained = RetainedReplay {
            id: session.id.clone(),
            created_at: session.created_at,
            closed_at,
            url,
            keep_alive: session.keep_alive,
            profile: session.profile.clone(),
            proxy_session: session.proxy_session.clone(),
            byob: session.is_byob,
            record_frames: session.record_frames,
            recorder,
        };
        let mut g = self.inner.lock().unwrap();
        g.sessions.remove(&session.id);
        g.retained.push(retained, closed_at);
        if g.retained_tenants.len() >= 256 {
            g.retained_tenants.clear();
        }
        g.retained_tenants
            .insert(session.id.clone(), session.tenant.clone());
    }

    fn list(&self, tenant: &TenantIdentity) -> Vec<Value> {
        let mut items: Vec<_> = self
            .inner
            .lock()
            .unwrap()
            .sessions
            .values()
            .filter(|s| tenant.can_access(&s.tenant))
            .map(|s| s.summary())
            .collect();
        items.sort_by_key(|v| v["created_at"].as_u64().unwrap_or(0));
        items
    }

    pub(crate) fn list_with_retained(&self) -> Vec<Value> {
        let now = now_ms();
        let mut g = self.inner.lock().unwrap();
        let mut items: Vec<Value> = g
            .sessions
            .values()
            .map(|s| s.dashboard_summary())
            .collect();
        items.extend(g.retained.summaries(now));
        items.sort_by(|a, b| {
            let ca = a["created_at"].as_u64().unwrap_or(0);
            let cb = b["created_at"].as_u64().unwrap_or(0);
            cb.cmp(&ca)
                .then_with(|| a["id"].as_str().cmp(&b["id"].as_str()))
        });
        items
    }

    fn with_replay<R>(
        &self,
        id: &str,
        tenant: &TenantIdentity,
        f: impl FnOnce(&str, u64, Option<u64>, &str, &SessionRecorder) -> R,
    ) -> Option<R> {
        let active = {
            let mut g = self.inner.lock().unwrap();
            if let Some(sess) = g.sessions.get(id).cloned() {
                if !tenant.can_access(&sess.tenant) {
                    return None;
                }
                Some(sess)
            } else {
                let owner = g
                    .retained_tenants
                    .get(id)
                    .map(String::as_str)
                    .unwrap_or(crate::tenants::DEFAULT_TENANT);
                if !tenant.can_access(owner) {
                    return None;
                }
                let now = now_ms();
                return g.retained.with_entry(id, now, |r| {
                    let url = if r.url.is_empty() {
                        "about:blank"
                    } else {
                        r.url.as_str()
                    };
                    f("closed", r.created_at, Some(r.closed_at), url, &r.recorder)
                });
            }
        };
        if let Some(sess) = active {
            let status = if *sess.close_tx.borrow() {
                "closing"
            } else {
                "active"
            };
            let url = sess.current_url();
            let rec = sess.recorder.lock().unwrap();
            return Some(f(status, sess.created_at, None, &url, &rec));
        }
        None
    }

    pub(crate) async fn drain(&self, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        loop {
            let next_notify = {
                let g = self.inner.lock().unwrap();
                g.sessions
                    .values()
                    .next()
                    .map(|s| (s.id.clone(), s.close_tx.clone(), s.closed_notify.clone()))
            };
            let Some((id, close_tx, notify)) = next_notify else {
                break;
            };
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            let fut = notify.notified();
            tokio::pin!(fut);
            fut.as_mut().enable();
            let _ = close_tx.send(true);
            if self.get(&id).is_some() {
                let _ = tokio::time::timeout(remaining.min(Duration::from_millis(100)), fut).await;
            }
        }
    }
}

fn byob_ip_allowed(ip: IpAddr) -> Result<(), String> {
    match ip {
        IpAddr::V4(v4) => {
            if v4.is_loopback() {
                return Ok(());
            }
            let oct = v4.octets();
            let is_cgnat = oct[0] == 100 && (oct[1] & 0xc0) == 64;
            if v4.is_link_local()
                || v4.is_private()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_multicast()
                || is_cgnat
            {
                return Err(format!(
                    "cdp_endpoint IP {v4} is blocked (link-local and private addresses are not permitted)"
                ));
            }
            Ok(())
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return byob_ip_allowed(IpAddr::V4(v4));
            }
            if v6.is_loopback() {
                return Ok(());
            }
            let seg0 = v6.segments()[0];
            let is_link_local = (seg0 & 0xffc0) == 0xfe80;
            let is_ula = (seg0 & 0xfe00) == 0xfc00;
            if v6.is_unspecified() || v6.is_multicast() || is_link_local || is_ula {
                return Err(format!(
                    "cdp_endpoint IP {v6} is blocked (link-local and private addresses are not permitted)"
                ));
            }
            Ok(())
        }
    }
}

pub(crate) fn validate_cdp_endpoint_syntax(
    raw: &str,
    allow_any_url: bool,
) -> Result<(String, String, u16, bool), String> {
    if raw.is_empty()
        || raw.len() > 2048
        || raw.chars().any(|c| c.is_whitespace() || c.is_control())
        || raw.contains('\\')
    {
        return Err(
            "cdp_endpoint must be 1-2048 characters without whitespace, control characters, or backslashes"
                .into(),
        );
    }
    let (scheme_raw, rest) = raw
        .split_once("://")
        .ok_or_else(|| "cdp_endpoint must use ws://, wss://, http://, or https://".to_string())?;
    let scheme_lower = scheme_raw.to_ascii_lowercase();
    let default_port = match scheme_lower.as_str() {
        "ws" | "http" => 80u16,
        "wss" | "https" => 443u16,
        _ => {
            return Err("cdp_endpoint must use ws://, wss://, http://, or https://".into());
        }
    };

    let rest_no_frag = rest.split('#').next().unwrap_or(rest);
    let auth_end = rest_no_frag.find(['/', '?']).unwrap_or(rest_no_frag.len());
    let authority = &rest_no_frag[..auth_end];
    if authority.is_empty() || authority.contains('@') {
        return Err("cdp_endpoint authority is invalid or contains userinfo".into());
    }

    let (host, port) = if let Some(after_bracket) = authority.strip_prefix('[') {
        let Some((ip_str, tail)) = after_bracket.split_once(']') else {
            return Err("cdp_endpoint has an unclosed IPv6 host".into());
        };
        if ip_str.parse::<Ipv6Addr>().is_err() {
            return Err("cdp_endpoint has an invalid IPv6 host".into());
        }
        let port = match tail {
            "" => default_port,
            _ => tail
                .strip_prefix(':')
                .and_then(|p| p.parse::<u16>().ok())
                .filter(|&p| p != 0)
                .ok_or_else(|| "cdp_endpoint has an invalid port".to_string())?,
        };
        (ip_str.to_ascii_lowercase(), port)
    } else if let Some((h, p)) = authority.rsplit_once(':') {
        if h.is_empty() || h.contains(':') {
            return Err("cdp_endpoint has an invalid host".into());
        }
        let port = p
            .parse::<u16>()
            .ok()
            .filter(|&n| n != 0)
            .ok_or_else(|| "cdp_endpoint has an invalid port".to_string())?;
        (h.to_ascii_lowercase(), port)
    } else {
        (authority.to_ascii_lowercase(), default_port)
    };

    let host_norm = host.trim_end_matches('.').to_ascii_lowercase();
    if host_norm.is_empty() || !host_norm.bytes().all(|b| b.is_ascii_graphic()) {
        return Err("cdp_endpoint has an empty or invalid host".into());
    }

    let mut needs_dns = false;
    if !allow_any_url {
        if host_norm == "metadata"
            || host_norm == "metadata.google.internal"
            || host_norm.ends_with(".metadata.google.internal")
        {
            return Err("cdp_endpoint host is blocked by SSRF policy".into());
        }
        if host_norm == "localhost" || host_norm.ends_with(".localhost") {
            // Local stealth browsers (Patchright/Camoufox/Chrome) on loopback are permitted.
        } else if let Ok(v6) = host_norm.parse::<Ipv6Addr>() {
            byob_ip_allowed(IpAddr::V6(v6))?;
        } else if let Some(v4) = netguard::ipv4_host(&host_norm) {
            byob_ip_allowed(IpAddr::V4(v4))?;
        } else {
            needs_dns = true;
        }
    }

    Ok((format!("{scheme_lower}://{rest}"), host_norm, port, needs_dns))
}

async fn validate_cdp_endpoint(raw: &str, allow_any_url: bool) -> Result<String, String> {
    let (normalized, host, port, needs_dns) = validate_cdp_endpoint_syntax(raw, allow_any_url)?;
    if needs_dns && !allow_any_url {
        if let Ok(addrs) = tokio::net::lookup_host((host.as_str(), port)).await {
            for addr in addrs {
                byob_ip_allowed(addr.ip())?;
            }
        }
    }
    Ok(normalized)
}

fn map_acquire_error(e: fluxwright::Error) -> ApiError {
    match e {
        fluxwright::Error::ShuttingDown => {
            ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "shutting_down", e.to_string())
        }
        fluxwright::Error::QueueFull => {
            ApiError::new(StatusCode::TOO_MANY_REQUESTS, "capacity_exceeded", e.to_string())
        }
        fluxwright::Error::AcquireTimeout => ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "acquire_timeout",
            e.to_string(),
        ),
        other => {
            let msg = other.to_string();
            if msg.contains("profiles_dir") {
                ApiError::new(StatusCode::BAD_REQUEST, "invalid_request", msg)
            } else {
                ApiError::new(StatusCode::BAD_GATEWAY, "browser_error", msg)
            }
        }
    }
}

pub(crate) async fn create_session(
    State(inner): State<Arc<Inner>>,
    headers: HeaderMap,
    request: Request,
) -> Result<Response, ApiError> {
    let tenant = crate::tenants::tenant_from_ext(request.extensions());
    let bytes = to_bytes(request.into_body(), crate::MAX_REQUEST_BYTES)
        .await
        .map_err(|_| {
            ApiError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "payload_too_large",
                "session request body exceeds 64 KiB",
            )
        })?;
    let Some(engine) = inner.engine.clone() else {
        return crate::cluster::create_sticky_session(&inner, tenant, &bytes);
    };
    if !inner.with_registry(|r, _| r.accepting) {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "shutting_down",
            "the server is shutting down",
        ));
    }

    let req: CreateSessionRequest = if bytes.iter().all(u8::is_ascii_whitespace) {
        CreateSessionRequest::default()
    } else {
        let Json(parsed) = Json::<CreateSessionRequest>::from_bytes(&bytes)
            .map_err(|e| ApiError::new(e.status(), "invalid_request", e.body_text()))?;
        parsed
    };
    req.validate()
        .map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, "invalid_request", e))?;

    let validated_cdp_endpoint = match req.cdp_endpoint.as_deref() {
        Some(raw) => {
            if !inner.allow_session_cdp_endpoint {
                return Err(ApiError::new(
                    StatusCode::FORBIDDEN,
                    "byob_disabled",
                    "per-session cdp_endpoint is disabled on this server",
                ));
            }
            let normalized = validate_cdp_endpoint(raw, inner.allow_any_url())
                .await
                .map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, "invalid_request", e))?;
            Some(normalized)
        }
        None => None,
    };

    let resolved_extensions = if req.extensions.is_empty() {
        Vec::new()
    } else {
        let ext_dir = inner.extensions_dir.clone();
        let ext_names = req.extensions.clone();
        tokio::task::spawn_blocking(move || {
            resolve_session_extensions(ext_dir.as_deref(), &ext_names)
        })
        .await
        .map_err(|e| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "extension_error",
                e.to_string(),
            )
        })??
    };

    let reservation = inner.sessions.try_reserve().ok_or_else(|| {
        ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "capacity_exceeded",
            "maximum active managed browser sessions reached",
        )
    })?;

    let opts = req.job_options();
    let is_byob = validated_cdp_endpoint.is_some();
    let (byob_engine, active_engine) = if let Some(endpoint) = validated_cdp_endpoint {
        let mut builder = BrowserEngine::builder()
            .connect_over_cdp(endpoint)
            .max_browsers(1)
            .max_contexts_per_browser(1)
            .acquire_timeout(Duration::from_secs(15));
        if let Some(ref profiles_dir) = engine.config().profiles_dir {
            builder = builder.profiles_dir(profiles_dir.clone());
        }
        let byob = builder.build().await.map_err(|e| {
            ApiError::new(StatusCode::BAD_GATEWAY, "browser_error", e.to_string())
        })?;
        (Some(byob.clone()), byob)
    } else if !resolved_extensions.is_empty() {
        let joined = resolved_extensions
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(",");
        let launch = &engine.config().launch;
        let mut builder = BrowserEngine::builder()
            .max_browsers(1)
            .max_contexts_per_browser(1)
            .acquire_timeout(Duration::from_secs(15))
            .no_sandbox(launch.no_sandbox)
            .headless(launch.headless)
            .low_memory(launch.low_memory)
            .ignore_default_args_list(["--disable-extensions"]);
        if let Some(ref exe) = launch.executable {
            builder = builder.chrome(exe.clone());
        }
        if let Some(ref ch) = launch.channel {
            builder = builder.channel(ch.clone());
        }
        if let Some(ref profiles_dir) = engine.config().profiles_dir {
            builder = builder.profiles_dir(profiles_dir.clone());
        }
        if !launch.extra_args.is_empty() {
            builder = builder.args(launch.extra_args.clone());
        }
        builder = builder.args([
            format!("--disable-extensions-except={joined}"),
            format!("--load-extension={joined}"),
        ]);
        let ext_engine = builder.build().await.map_err(|e| {
            ApiError::new(StatusCode::BAD_GATEWAY, "browser_error", e.to_string())
        })?;
        (Some(ext_engine.clone()), ext_engine)
    } else {
        (None, engine)
    };

    let lease = match active_engine.acquire_context_with(&opts).await {
        Ok(l) => Arc::new(l),
        Err(err) => {
            if let Some(ref byob) = byob_engine {
                let _ = byob.shutdown(BYOB_SHUTDOWN_TIMEOUT).await;
            }
            return Err(map_acquire_error(err));
        }
    };

    lease.discard_warm_page().await;
    lease.mark_external_context(lease.context_id());

    let id = format!("sess-{}", Uuid::new_v4().simple());
    let conn = lease.cdp_connection();
    let primary_context = lease.context_id().to_string();
    let live_target = Arc::new(StdMutex::new(None));
    let live_url = Arc::new(StdMutex::new(String::from("about:blank")));
    let recorder = Arc::new(StdMutex::new(SessionRecorder::default()));
    let created_at = now_ms();

    let live_guard = inner.live.register_cdp(
        id.clone(),
        conn.clone(),
        live_target.clone(),
        live_url.clone(),
        created_at,
    );

    let policy = GatewayPolicy {
        allow_any_url: inner.allow_any_url(),
        enforce_server_proxy: compute_enforce_server_proxy(&active_engine),
        resource_preset: req.resource_preset.map(|p| match p {
            ServerResourcePreset::Scraping => fluxwright::ResourcePreset::Scraping,
            ServerResourcePreset::Minimal => fluxwright::ResourcePreset::Minimal,
            ServerResourcePreset::NoMedia => fluxwright::ResourcePreset::NoMedia,
        }),
        block_cookie_banners: req.block_cookie_banners,
        download_dir: std::env::temp_dir().join(format!("fluxwright-sess-dl-{}", Uuid::new_v4().simple())),
    };

    let state = Arc::new(Mutex::new(GatewayState::new(
        primary_context,
        live_target,
        live_url.clone(),
    )));
    // Subscribe before installing the page/blocker so no disconnect or Fetch.requestPaused is missed.
    let cdp_events = conn.subscribe_unbounded();
    let viewport = req.viewport.map(|v| (v.width, v.height));
    let Some(initial_tid) = create_initial_page(&conn, &state, &lease, &policy, viewport).await
    else {
        teardown_gateway_state(&conn, &state, &lease, &policy).await;
        if let Some(ref byob) = byob_engine {
            let _ = byob.shutdown(BYOB_SHUTDOWN_TIMEOUT).await;
        }
        drop(live_guard);
        return Err(ApiError::new(
            StatusCode::BAD_GATEWAY,
            "browser_error",
            "failed to create initial page for managed session",
        ));
    };

    let initial_observer = ensure_session_observer(&conn, &state, &lease, &initial_tid).await;

    let (embed_token, effective_ttl, expires_at) = match inner
        .live
        .issue_embed(&id, req.live_view_mode, req.ttl_secs)
    {
        Ok(res) => res,
        Err(err) => {
            teardown_gateway_state(&conn, &state, &lease, &policy).await;
            if let Some(ref byob) = byob_engine {
                let _ = byob.shutdown(BYOB_SHUTDOWN_TIMEOUT).await;
            }
            drop(live_guard);
            return Err(err);
        }
    };

    let (expires_tx, expires_rx) =
        watch::channel((Instant::now() + Duration::from_secs(effective_ttl), expires_at));
    let (close_tx, close_rx) = watch::channel(false);
    let closed_notify = Arc::new(Notify::new());

    let session = Arc::new(ManagedSession {
        id: id.clone(),
        tenant: tenant.id,
        keep_alive: req.keep_alive,
        profile: req.profile,
        persist_profile: req.persist_profile,
        proxy_session: req.proxy_session,
        record_frames: req.record_frames,
        is_byob,
        extensions: req.extensions,
        byob_engine,
        created_at,
        lease,
        state,
        live_url,
        recorder,
        policy,
        connected: AtomicBool::new(false),
        expires_tx,
        close_tx,
        closed_notify,
    });

    inner.sessions.insert(reservation, session.clone());
    spawn_session_reaper(
        inner.clone(),
        active_engine,
        session.clone(),
        expires_rx,
        close_rx,
        cdp_events,
        live_guard,
        initial_tid,
        initial_observer,
    );

    let (ws_scheme, host) = host_base(&headers);
    let cdp_path = format!("/v1/sessions/{id}/cdp");
    let cdp_ws_url = format!("{ws_scheme}://{host}{cdp_path}");

    let body = json!({
        "id": id,
        "status": "active",
        "cdp_ws_url": cdp_ws_url,
        "cdp_path": cdp_path,
        "live_view_url": format!("/embed/live/{embed_token}"),
        "live_view_mode": req.live_view_mode,
        "keep_alive": session.keep_alive,
        "connected": false,
        "url": "about:blank",
        "profile": session.profile,
        "proxy_session": session.proxy_session,
        "record_frames": session.record_frames,
        "byob": session.is_byob,
        "extensions": session.extensions,
        "created_at": session.created_at,
        "expires_at": expires_at,
        "ttl_secs": effective_ttl,
    });
    Ok((StatusCode::CREATED, Json(body)).into_response())
}

pub(crate) async fn list_sessions(
    State(inner): State<Arc<Inner>>,
    tenant: TenantIdentity,
) -> Json<Value> {
    if inner.engine.is_none() {
        return crate::cluster::list_sticky_sessions(&inner, &tenant);
    }
    Json(json!({ "sessions": inner.sessions.list(&tenant) }))
}

pub(crate) async fn get_session(
    State(inner): State<Arc<Inner>>,
    tenant: TenantIdentity,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    validate_session_id(&id)?;
    if inner.engine.is_none() {
        return crate::cluster::get_sticky_session(&inner, &id, &tenant);
    }
    let sess = inner
        .sessions
        .get(&id)
        .filter(|s| tenant.can_access(&s.tenant))
        .ok_or_else(session_not_found)?;
    Ok(Json(sess.summary()))
}

pub(crate) async fn extend_session(
    State(inner): State<Arc<Inner>>,
    Path(id): Path<String>,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    validate_session_id(&id)?;
    let tenant = crate::tenants::tenant_from_ext(request.extensions());
    let bytes = to_bytes(request.into_body(), crate::MAX_REQUEST_BYTES)
        .await
        .map_err(|_| {
            ApiError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "payload_too_large",
                "extend request body exceeds 64 KiB",
            )
        })?;
    let req: ExtendSessionRequest = if bytes.iter().all(u8::is_ascii_whitespace) {
        ExtendSessionRequest::default()
    } else {
        let Json(parsed) = Json::<ExtendSessionRequest>::from_bytes(&bytes)
            .map_err(|e| ApiError::new(e.status(), "invalid_request", e.body_text()))?;
        parsed
    };
    validate_ttl(req.ttl_secs)
        .map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, "invalid_request", e))?;

    if inner.engine.is_none() {
        return crate::cluster::extend_sticky_session(&inner, &id, req.ttl_secs, &tenant);
    }
    let sess = inner
        .sessions
        .get(&id)
        .filter(|s| tenant.can_access(&s.tenant))
        .ok_or_else(session_not_found)?;
    if *sess.close_tx.borrow() || sess.lease.is_closed() {
        let _ = sess.close_tx.send(true);
        return Err(session_not_found());
    }
    let next_std = std::time::Instant::now() + Duration::from_secs(req.ttl_secs);
    let next_instant = Instant::from_std(next_std);
    let next_ms = now_ms().saturating_add(req.ttl_secs.saturating_mul(1000));
    inner.live.extend_embeds(&id, next_std);
    let _ = sess.expires_tx.send((next_instant, next_ms));
    Ok(Json(sess.summary()))
}

pub(crate) async fn delete_session(
    State(inner): State<Arc<Inner>>,
    tenant: TenantIdentity,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    validate_session_id(&id)?;
    if inner.engine.is_none() {
        return crate::cluster::delete_sticky_session(&inner, &id, &tenant);
    }
    let sess = inner
        .sessions
        .get(&id)
        .filter(|s| tenant.can_access(&s.tenant))
        .ok_or_else(session_not_found)?;
    let notified = sess.closed_notify.notified();
    tokio::pin!(notified);
    notified.as_mut().enable();
    let _ = sess.close_tx.send(true);
    if inner.sessions.get(&id).is_some() {
        let _ = tokio::time::timeout(DELETE_WAIT_TIMEOUT, notified).await;
    }
    Ok(Json(json!({ "id": id, "status": "closed" })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UploadSessionFileRequest {
    name: String,
    #[serde(default)]
    content_base64: Option<String>,
    #[serde(default)]
    content: Option<String>,
}

pub(crate) async fn list_session_files(
    State(inner): State<Arc<Inner>>,
    tenant: TenantIdentity,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    if inner.engine.is_none() {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            "session files are unavailable in coordinator mode",
        ));
    }
    validate_session_id(&id)?;
    let sess = inner
        .sessions
        .get(&id)
        .filter(|s| tenant.can_access(&s.tenant))
        .ok_or_else(session_not_found)?;
    let dir = sess.policy.download_dir.clone();
    let files = tokio::task::spawn_blocking(move || list_files_in_dir(&dir))
        .await
        .map_err(|e| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "file_error",
                e.to_string(),
            )
        })??;
    Ok(Json(json!({ "id": id, "files": files })))
}

pub(crate) async fn upload_session_file(
    State(inner): State<Arc<Inner>>,
    Path(id): Path<String>,
    request: Request,
) -> Result<Response, ApiError> {
    if inner.engine.is_none() {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            "session files are unavailable in coordinator mode",
        ));
    }
    validate_session_id(&id)?;
    let tenant = crate::tenants::tenant_from_ext(request.extensions());
    let sess = inner
        .sessions
        .get(&id)
        .filter(|s| tenant.can_access(&s.tenant))
        .ok_or_else(session_not_found)?;
    if *sess.close_tx.borrow() || sess.lease.is_closed() {
        let _ = sess.close_tx.send(true);
        return Err(session_not_found());
    }
    let body = to_bytes(request.into_body(), MAX_SESSION_FILE_BODY_BYTES)
        .await
        .map_err(|_| {
            ApiError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "payload_too_large",
                "session file upload body exceeds 6 MiB",
            )
        })?;
    let Json(req) = Json::<UploadSessionFileRequest>::from_bytes(&body)
        .map_err(|e| ApiError::new(e.status(), "invalid_request", e.body_text()))?;
    validate_session_filename(&req.name)?;
    let data = match (req.content_base64, req.content) {
        (Some(b64), None) => base64::engine::general_purpose::STANDARD
            .decode(b64.as_bytes())
            .map_err(|e| {
                ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "invalid_request",
                    format!("invalid content_base64: {e}"),
                )
            })?,
        (None, Some(text)) => text.into_bytes(),
        _ => {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "specify exactly one of content_base64 or content",
            ));
        }
    };
    let dir = sess.policy.download_dir.clone();
    let name = req.name;
    let name_for_write = name.clone();
    let (size, path) = tokio::task::spawn_blocking(move || {
        write_file_in_dir(&dir, &name_for_write, &data)
    })
    .await
    .map_err(|e| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "file_error",
            e.to_string(),
        )
    })??;
    Ok((
        StatusCode::CREATED,
        Json(json!({ "name": name, "size": size, "path": path })),
    )
        .into_response())
}

pub(crate) async fn download_session_file(
    State(inner): State<Arc<Inner>>,
    tenant: TenantIdentity,
    Path((id, name)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    if inner.engine.is_none() {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            "session files are unavailable in coordinator mode",
        ));
    }
    validate_session_id(&id)?;
    validate_session_filename(&name)?;
    let sess = inner
        .sessions
        .get(&id)
        .filter(|s| tenant.can_access(&s.tenant))
        .ok_or_else(session_not_found)?;
    let dir = sess.policy.download_dir.clone();
    let name_for_read = name.clone();
    let bytes = tokio::task::spawn_blocking(move || read_file_in_dir(&dir, &name_for_read))
        .await
        .map_err(|e| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "file_error",
                e.to_string(),
            )
        })??;
    let mut response = (StatusCode::OK, bytes).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    if let Ok(cd) = HeaderValue::from_str(&format!("attachment; filename=\"{name}\"")) {
        response.headers_mut().insert(header::CONTENT_DISPOSITION, cd);
    }
    Ok(response)
}

pub(crate) async fn delete_session_file(
    State(inner): State<Arc<Inner>>,
    tenant: TenantIdentity,
    Path((id, name)): Path<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    if inner.engine.is_none() {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            "session files are unavailable in coordinator mode",
        ));
    }
    validate_session_id(&id)?;
    validate_session_filename(&name)?;
    let sess = inner
        .sessions
        .get(&id)
        .filter(|s| tenant.can_access(&s.tenant))
        .ok_or_else(session_not_found)?;
    let dir = sess.policy.download_dir.clone();
    let name_for_delete = name.clone();
    tokio::task::spawn_blocking(move || delete_file_in_dir(&dir, &name_for_delete))
        .await
        .map_err(|e| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "file_error",
                e.to_string(),
            )
        })??;
    Ok(Json(json!({ "deleted": name })))
}

struct ConnectionGuard {
    session: Arc<ManagedSession>,
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.session.connected.store(false, Ordering::Release);
    }
}

pub(crate) async fn cdp_ws_handler(
    State(inner): State<Arc<Inner>>,
    Path(id): Path<String>,
    request: Request,
) -> Result<Response, ApiError> {
    validate_session_id(&id)?;
    let tenant = crate::tenants::tenant_from_ext(request.extensions());
    let Some(_) = inner.engine else {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            "managed sessions are unavailable in coordinator mode",
        ));
    };
    let sess = inner
        .sessions
        .get(&id)
        .filter(|s| tenant.can_access(&s.tenant))
        .ok_or_else(session_not_found)?;
    if *sess.close_tx.borrow() || sess.lease.is_closed() {
        let _ = sess.close_tx.send(true);
        return Err(session_not_found());
    }
    let (mut parts, _) = request.into_parts();
    let ws: WebSocketUpgrade = WebSocketUpgrade::from_request_parts(&mut parts, &inner)
        .await
        .map_err(|e| ApiError::new(e.status(), "invalid_websocket", e.body_text()))?;
    if sess
        .connected
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "session_in_use",
            "a CDP client is already connected to this session",
        ));
    }
    let guard = ConnectionGuard {
        session: sess.clone(),
    };
    Ok(ws
        .max_frame_size(128 << 20)
        .max_message_size(128 << 20)
        .on_upgrade(move |socket| async move {
            let _conn_guard = guard;
            let close_rx = sess.close_tx.subscribe();
            let conn = sess.lease.cdp_connection();
            run_gateway_loop(
                socket,
                inner,
                sess.lease.clone(),
                sess.state.clone(),
                sess.policy.clone(),
                close_rx,
                false,
            )
            .await;
            detach_client_sessions(&conn, &sess.state, &sess.lease).await;
            drop(_conn_guard);
            if !sess.keep_alive || sess.lease.is_closed() {
                let _ = sess.close_tx.send(true);
            }
        }))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReplayQuery {
    q: Option<String>,
    kind: Option<String>,
    include_frames: bool,
}

impl Default for ReplayQuery {
    fn default() -> Self {
        Self {
            q: None,
            kind: None,
            include_frames: true,
        }
    }
}

impl ReplayQuery {
    fn parse(raw: Option<&str>) -> Result<Self, ApiError> {
        let mut out = Self::default();
        for (k, val) in parse_query_pairs(raw)? {
            match k.as_str() {
                "q" => {
                    let trimmed = val.trim();
                    if !trimmed.is_empty() {
                        out.q = Some(trimmed.to_string());
                    }
                }
                "kind" => {
                    let norm = val.trim().to_ascii_lowercase();
                    if !norm.is_empty() {
                        if !matches!(norm.as_str(), "nav" | "console" | "network" | "frame") {
                            return Err(ApiError::new(
                                StatusCode::BAD_REQUEST,
                                "invalid_query",
                                "kind must be nav, console, network, or frame",
                            ));
                        }
                        out.kind = Some(norm);
                    }
                }
                "include_frames" => {
                    out.include_frames = parse_query_bool("include_frames", &val)?;
                }
                _ => {
                    return Err(ApiError::new(
                        StatusCode::BAD_REQUEST,
                        "invalid_query",
                        format!("unknown query parameter: {k}"),
                    ));
                }
            }
        }
        Ok(out)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum NetworkFormat {
    #[default]
    Json,
    Har,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct NetworkQuery {
    filter: Option<String>,
    api_only: bool,
    format: NetworkFormat,
    include_auth: bool,
}

impl NetworkQuery {
    fn parse(raw: Option<&str>) -> Result<Self, ApiError> {
        let mut out = Self::default();
        for (k, val) in parse_query_pairs(raw)? {
            match k.as_str() {
                "filter" => {
                    let trimmed = val.trim();
                    if !trimmed.is_empty() {
                        out.filter = Some(trimmed.to_string());
                    }
                }
                "api_only" => {
                    out.api_only = parse_query_bool("api_only", &val)?;
                }
                "include_auth" => {
                    out.include_auth = parse_query_bool("include_auth", &val)?;
                }
                "format" => {
                    out.format = match val.trim().to_ascii_lowercase().as_str() {
                        "" | "json" => NetworkFormat::Json,
                        "har" => NetworkFormat::Har,
                        _ => {
                            return Err(ApiError::new(
                                StatusCode::BAD_REQUEST,
                                "invalid_query",
                                "format must be json or har",
                            ));
                        }
                    };
                }
                _ => {
                    return Err(ApiError::new(
                        StatusCode::BAD_REQUEST,
                        "invalid_query",
                        format!("unknown query parameter: {k}"),
                    ));
                }
            }
        }
        Ok(out)
    }
}

fn parse_query_pairs(raw: Option<&str>) -> Result<Vec<(String, String)>, ApiError> {
    let Some(raw) = raw.filter(|s| !s.is_empty()) else {
        return Ok(Vec::new());
    };
    let mut seen = HashSet::new();
    let mut pairs = Vec::new();
    for pair in raw.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = pair.split_once('=').ok_or_else(|| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                "invalid_query",
                format!("malformed query parameter: {pair}"),
            )
        })?;
        if !seen.insert(k) {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "invalid_query",
                format!("duplicate query parameter: {k}"),
            ));
        }
        let val = percent_decode_str(&v.replace('+', " ")).ok_or_else(|| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                "invalid_query",
                format!("invalid percent-encoding in parameter: {k}"),
            )
        })?;
        pairs.push((k.to_string(), val));
    }
    Ok(pairs)
}

fn parse_query_bool(name: &str, val: &str) -> Result<bool, ApiError> {
    match val {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_query",
            format!("{name} must be true or false"),
        )),
    }
}

pub(crate) async fn get_session_replay(
    State(inner): State<Arc<Inner>>,
    tenant: TenantIdentity,
    Path(id): Path<String>,
    RawQuery(raw_query): RawQuery,
) -> Result<Json<Value>, ApiError> {
    if inner.engine.is_none() {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            "managed sessions are unavailable in coordinator mode",
        ));
    }
    validate_session_id(&id)?;
    let query = ReplayQuery::parse(raw_query.as_deref())?;
    let payload = inner
        .sessions
        .with_replay(&id, &tenant, |status, created_at, closed_at, url, rec| {
            let (total_events, events) =
                rec.query_replay(query.q.as_deref(), query.kind.as_deref(), query.include_frames);
            json!({
                "id": id,
                "status": status,
                "created_at": created_at,
                "closed_at": closed_at,
                "url": url,
                "total_events": total_events,
                "events": events,
            })
        })
        .ok_or_else(session_not_found)?;
    Ok(Json(payload))
}

pub(crate) async fn get_session_network(
    State(inner): State<Arc<Inner>>,
    tenant: TenantIdentity,
    Path(id): Path<String>,
    RawQuery(raw_query): RawQuery,
) -> Result<Response, ApiError> {
    if inner.engine.is_none() {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            "managed sessions are unavailable in coordinator mode",
        ));
    }
    validate_session_id(&id)?;
    let query = NetworkQuery::parse(raw_query.as_deref())?;
    let payload = inner
        .sessions
        .with_replay(&id, &tenant, |status, created_at, closed_at, url, rec| {
            match query.format {
                NetworkFormat::Har => {
                    let har = rec.export_har(
                        query.filter.as_deref(),
                        query.api_only,
                        query.include_auth,
                    );
                    serde_json::to_value(har).unwrap_or_else(|_| json!({}))
                }
                NetworkFormat::Json => {
                    let (total_entries, entries) = rec.export_network_entries(
                        query.filter.as_deref(),
                        query.api_only,
                        query.include_auth,
                    );
                    json!({
                        "id": id,
                        "status": status,
                        "created_at": created_at,
                        "closed_at": closed_at,
                        "url": url,
                        "total_entries": total_entries,
                        "entries": entries,
                    })
                }
            }
        })
        .ok_or_else(session_not_found)?;
    Ok(Json(payload).into_response())
}

fn frame_url(frame: &Value) -> String {
    let base = frame["url"].as_str().unwrap_or("");
    let frag = frame["urlFragment"].as_str().unwrap_or("");
    if !frag.is_empty() && !base.contains('#') {
        format!("{base}{frag}")
    } else {
        base.to_string()
    }
}

fn spawn_session_frame_capture(
    conn: Arc<fluxwright::Connection>,
    sid: String,
    nav_url: String,
    recorder: Arc<StdMutex<SessionRecorder>>,
    inflight: Arc<AtomicBool>,
    captured_urls: Arc<StdMutex<HashMap<String, String>>>,
    last_frame_at: Arc<StdMutex<Option<std::time::Instant>>>,
    close_rx: watch::Receiver<bool>,
) {
    tokio::spawn(async move {
        struct Reset(Arc<AtomicBool>);
        impl Drop for Reset {
            fn drop(&mut self) {
                self.0.store(false, Ordering::Release);
            }
        }
        let _reset = Reset(inflight);
        if *close_rx.borrow() {
            return;
        }
        if let Ok(Ok(res)) = conn
            .call_raw(
                "Page.captureScreenshot",
                json!({ "format": "jpeg", "quality": 45 }),
                Some(&sid),
                Duration::from_secs(10),
            )
            .await
        {
            if let Some(data) = res["data"].as_str().filter(|s| !s.is_empty()) {
                *last_frame_at.lock().unwrap() = Some(std::time::Instant::now());
                captured_urls
                    .lock()
                    .unwrap()
                    .insert(sid.clone(), nav_url.clone());
                recorder
                    .lock()
                    .unwrap()
                    .push_frame(now_ms(), &nav_url, data.to_string());
            }
        }
    });
}

#[allow(clippy::too_many_arguments)]
fn spawn_session_reaper(
    inner: Arc<Inner>,
    engine: BrowserEngine,
    session: Arc<ManagedSession>,
    mut expires_rx: watch::Receiver<(Instant, u64)>,
    mut close_rx: watch::Receiver<bool>,
    mut cdp_events: mpsc::UnboundedReceiver<Arc<CdpEvent>>,
    live_guard: LiveGuard,
    initial_tid: String,
    initial_observer: Option<String>,
) {
    tokio::spawn(async move {
        let _live_guard = live_guard;
        let mut shutdown_rx = inner.shutdown_tx.subscribe();
        let conn = session.lease.cdp_connection();
        let blocker = session.policy.blocker_config();
        let has_blocker = blocker.is_enabled();
        let observer_sids: Arc<StdMutex<HashSet<String>>> =
            Arc::new(StdMutex::new(initial_observer.into_iter().collect()));
        let mut observed_targets: HashSet<String> = HashSet::from([initial_tid]);
        let mut main_frames: HashMap<String, String> = HashMap::new();
        let mut observer_urls: HashMap<String, String> = HashMap::new();
        let last_frame_at: Arc<StdMutex<Option<std::time::Instant>>> = Arc::new(StdMutex::new(None));
        let screenshot_inflight = Arc::new(AtomicBool::new(false));
        let captured_urls: Arc<StdMutex<HashMap<String, String>>> =
            Arc::new(StdMutex::new(HashMap::new()));
        let sleep = tokio::time::sleep_until(expires_rx.borrow().0);
        tokio::pin!(sleep);
        loop {
            if *close_rx.borrow() || *shutdown_rx.borrow() || session.lease.is_closed() {
                break;
            }
            tokio::select! {
                () = &mut sleep => break,
                res = expires_rx.changed() => {
                    if res.is_err() {
                        break;
                    }
                    sleep.as_mut().reset(expires_rx.borrow().0);
                }
                res = close_rx.changed() => {
                    if res.is_err() || *close_rx.borrow() {
                        break;
                    }
                }
                res = shutdown_rx.changed() => {
                    if res.is_err() || *shutdown_rx.borrow() {
                        break;
                    }
                }
                ev = cdp_events.recv() => {
                    let Some(ev) = ev else {
                        break;
                    };
                    if ev.method == "Fluxwright.disconnected" {
                        break;
                    }
                    if let Some(sid) = ev.session_id.as_deref() {
                        if has_blocker && ev.method == "Fetch.requestPaused"
                            && session.state.lock().await.is_blocker_session(sid)
                        {
                            if let Some(req_id) =
                                ev.params["requestId"].as_str().map(str::to_owned)
                            {
                                spawn_fail_blocked_request(&conn, sid, req_id);
                            }
                        }
                        if observer_sids.lock().unwrap().contains(sid) {
                            match ev.method.as_str() {
                                "Page.frameNavigated" => {
                                    let frame = &ev.params["frame"];
                                    if frame["parentId"].is_null() {
                                        let nav_url = frame_url(frame);
                                        if !nav_url.is_empty() {
                                            if let Some(fid) = frame["id"].as_str() {
                                                main_frames.insert(sid.to_string(), fid.to_string());
                                            }
                                            observer_urls.insert(sid.to_string(), nav_url.clone());
                                            *session.live_url.lock().unwrap() = nav_url.clone();
                                            session.recorder.lock().unwrap().push_nav(now_ms(), &nav_url);
                                            if nav_url != "about:blank" {
                                                captured_urls.lock().unwrap().remove(sid);
                                            }
                                            if session.record_frames && nav_url != "about:blank" {
                                                let due = last_frame_at.lock().unwrap().is_none_or(|t| {
                                                    t.elapsed() >= MIN_FRAME_CAPTURE_INTERVAL
                                                });
                                                if due
                                                    && screenshot_inflight
                                                        .compare_exchange(
                                                            false,
                                                            true,
                                                            Ordering::AcqRel,
                                                            Ordering::Acquire,
                                                        )
                                                        .is_ok()
                                                {
                                                    spawn_session_frame_capture(
                                                        conn.clone(),
                                                        sid.to_string(),
                                                        nav_url,
                                                        session.recorder.clone(),
                                                        screenshot_inflight.clone(),
                                                        captured_urls.clone(),
                                                        last_frame_at.clone(),
                                                        session.close_tx.subscribe(),
                                                    );
                                                }
                                            }
                                        }
                                    }
                                }
                                "Page.navigatedWithinDocument" => {
                                    let fid = ev.params["frameId"].as_str().unwrap_or("");
                                    if main_frames.get(sid).is_some_and(|mf| mf == fid) {
                                        if let Some(nav_url) =
                                            ev.params["url"].as_str().filter(|s| !s.is_empty())
                                        {
                                            observer_urls.insert(sid.to_string(), nav_url.to_string());
                                            *session.live_url.lock().unwrap() = nav_url.to_string();
                                            session
                                                .recorder
                                                .lock()
                                                .unwrap()
                                                .push_nav(now_ms(), nav_url);
                                        }
                                    }
                                }
                                "Page.loadEventFired" => {
                                    if session.record_frames {
                                        let nav_url = observer_urls
                                            .get(sid)
                                            .cloned()
                                            .unwrap_or_else(|| session.current_url());
                                        let already_captured = captured_urls
                                            .lock()
                                            .unwrap()
                                            .get(sid)
                                            .map(String::as_str)
                                            == Some(nav_url.as_str());
                                        let due = last_frame_at.lock().unwrap().is_none_or(|t| {
                                            t.elapsed() >= MIN_FRAME_CAPTURE_INTERVAL
                                        });
                                        if !nav_url.is_empty()
                                            && nav_url != "about:blank"
                                            && !already_captured
                                            && due
                                            && screenshot_inflight
                                                .compare_exchange(
                                                    false,
                                                    true,
                                                    Ordering::AcqRel,
                                                    Ordering::Acquire,
                                                )
                                                .is_ok()
                                        {
                                            spawn_session_frame_capture(
                                                conn.clone(),
                                                sid.to_string(),
                                                nav_url,
                                                session.recorder.clone(),
                                                screenshot_inflight.clone(),
                                                captured_urls.clone(),
                                                last_frame_at.clone(),
                                                session.close_tx.subscribe(),
                                            );
                                        }
                                    }
                                }
                                "Runtime.consoleAPICalled"
                                | "Runtime.exceptionThrown"
                                | "Network.requestWillBeSent"
                                | "Network.requestWillBeSentExtraInfo"
                                | "Network.responseReceived"
                                | "Network.responseReceivedExtraInfo"
                                | "Network.loadingFinished"
                                | "Network.loadingFailed" => {
                                    let current_url = session.current_url();
                                    session.recorder.lock().unwrap().record_cdp_event(
                                        now_ms(),
                                        &ev.method,
                                        &ev.params,
                                        &current_url,
                                    );
                                }
                                _ => {}
                            }
                        }
                    } else {
                        match ev.method.as_str() {
                            "Target.targetCreated" => {
                                let info = &ev.params["targetInfo"];
                                let ty = info["type"].as_str().unwrap_or("");
                                if ty == "page" || (ty == "iframe" && has_blocker) {
                                    if let (Some(tid), Some(ctx_id)) = (
                                        info["targetId"].as_str(),
                                        info["browserContextId"].as_str(),
                                    ) {
                                        if observed_targets.len() > 1024 {
                                            observed_targets.clear();
                                        }
                                        if observed_targets.insert(tid.to_string()) {
                                            let conn = conn.clone();
                                            let state = session.state.clone();
                                            let lease = session.lease.clone();
                                            let observer_sids = observer_sids.clone();
                                            let tid = tid.to_string();
                                            let ctx_id = ctx_id.to_string();
                                            let is_page = ty == "page";
                                            tokio::spawn(async move {
                                                let is_ours = {
                                                    let g = state.lock().await;
                                                    g.is_allowed_context(&ctx_id)
                                                };
                                                if is_ours {
                                                    if has_blocker {
                                                        ensure_preset_blocker(
                                                            &conn, &state, &lease, &tid, blocker,
                                                        )
                                                        .await;
                                                    }
                                                    if is_page {
                                                        if let Some(obs_sid) = ensure_session_observer(
                                                            &conn, &state, &lease, &tid,
                                                        )
                                                        .await
                                                        {
                                                            observer_sids.lock().unwrap().insert(obs_sid);
                                                        }
                                                    }
                                                }
                                            });
                                        }
                                    }
                                }
                            }
                            "Target.detachedFromTarget" => {
                                if let Some(detached_sid) = ev.params["sessionId"].as_str() {
                                    observer_sids.lock().unwrap().remove(detached_sid);
                                    main_frames.remove(detached_sid);
                                    observer_urls.remove(detached_sid);
                                    captured_urls.lock().unwrap().remove(detached_sid);
                                }
                            }
                            "Target.targetDestroyed" => {
                                if let Some(tid) = ev.params["targetId"].as_str() {
                                    observed_targets.remove(tid);
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
        }

        let _ = session.close_tx.send(true);
        if session.persist_profile {
            if let Some(profile_name) = session.profile.clone() {
                persist_session_profile(&engine, &session.lease, profile_name).await;
            }
        }
        teardown_gateway_state(&conn, &session.state, &session.lease, &session.policy).await;
        let final_url = session.current_url();
        let recorder_snapshot = session.recorder.lock().unwrap().clone();
        inner
            .sessions
            .close_and_retain(&session, final_url, recorder_snapshot);
        if let Some(ref byob) = session.byob_engine {
            let _ = byob.shutdown(BYOB_SHUTDOWN_TIMEOUT).await;
        }
        drop(_live_guard);
        session.closed_notify.notify_waiters();
    });
}

async fn persist_session_profile(
    engine: &BrowserEngine,
    lease: &ContextLease,
    profile_name: String,
) {
    if engine.profiles().is_none() || lease.is_closed() {
        return;
    }
    let Ok(Ok(cookies)) = tokio::time::timeout(PROFILE_SAVE_TIMEOUT, lease.cookies()).await else {
        return;
    };
    let eng = engine.clone();
    let _ = tokio::task::spawn_blocking(move || {
        let Some(store) = eng.profiles() else {
            return;
        };
        let origins = store
            .load(&profile_name)
            .ok()
            .flatten()
            .map(|s| s.origins)
            .unwrap_or_default();
        let state = StorageState { cookies, origins };
        let _ = store.save(&profile_name, &state);
    })
    .await;
}

fn session_not_found() -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "session_not_found",
        "managed session not found or expired",
    )
}
