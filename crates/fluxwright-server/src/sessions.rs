//! Managed persistent cloud browser sessions (`POST /v1/sessions`, `/v1/sessions/{id}`,
//! `/v1/sessions/{id}/extend`, `/v1/sessions/{id}/cdp`).
//!
//! Mints isolated, long-lived Chrome contexts backed by the pooled engine, paired with a live
//! screencast viewer URL and a reconnectable WebSocket CDP endpoint.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use axum::body::to_bytes;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use fluxwright::{validate_profile_name, CdpEvent, ContextLease, JobOptions, Priority, StorageState};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::{mpsc, watch, Mutex, Notify};
use tokio::time::Instant;
use uuid::Uuid;

use crate::cdp_gateway::{
    compute_enforce_server_proxy, create_initial_page, detach_client_sessions, host_base,
    run_gateway_loop, spawn_fail_blocked_request, teardown_gateway_state, GatewayPolicy,
    GatewayState,
};
use crate::job::{JobPriority, ServerResourcePreset};
use crate::live::{EmbedMode, LiveGuard};
use crate::{now_ms, ApiError, Inner};

pub(crate) const MAX_MANAGED_SESSIONS: usize = 64;
const DEFAULT_SESSION_TTL_SECS: u64 = 300;
const MIN_SESSION_TTL_SECS: u64 = 10;
const MAX_SESSION_TTL_SECS: u64 = 3600;
const PROFILE_SAVE_TIMEOUT: Duration = Duration::from_secs(3);
const DELETE_WAIT_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SessionViewport {
    pub(crate) width: u32,
    pub(crate) height: u32,
}

#[derive(Debug, Deserialize)]
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
    pub(crate) profile: Option<String>,
    #[serde(default = "default_true")]
    pub(crate) persist_profile: bool,
    #[serde(default)]
    pub(crate) proxy_session: Option<String>,
    #[serde(default)]
    pub(crate) viewport: Option<SessionViewport>,
    #[serde(default)]
    pub(crate) live_view_mode: EmbedMode,
}

impl Default for CreateSessionRequest {
    fn default() -> Self {
        Self {
            ttl_secs: DEFAULT_SESSION_TTL_SECS,
            keep_alive: true,
            priority: JobPriority::Normal,
            resource_preset: None,
            profile: None,
            persist_profile: true,
            proxy_session: None,
            viewport: None,
            live_view_mode: EmbedMode::ReadOnly,
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

#[derive(Default)]
struct SessionMap {
    sessions: HashMap<String, Arc<ManagedSession>>,
    reserved: usize,
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
    keep_alive: bool,
    profile: Option<String>,
    persist_profile: bool,
    proxy_session: Option<String>,
    created_at: u64,
    lease: Arc<ContextLease>,
    state: Arc<Mutex<GatewayState>>,
    live_url: Arc<StdMutex<String>>,
    policy: GatewayPolicy,
    connected: AtomicBool,
    expires_tx: watch::Sender<(Instant, u64)>,
    close_tx: watch::Sender<bool>,
    closed_notify: Arc<Notify>,
}

impl ManagedSession {
    fn summary(&self) -> Value {
        let (_, expires_at) = *self.expires_tx.borrow();
        let url = {
            let u = self.live_url.lock().unwrap();
            if u.is_empty() {
                String::from("about:blank")
            } else {
                u.clone()
            }
        };
        json!({
            "id": self.id,
            "status": if *self.close_tx.borrow() { "closing" } else { "active" },
            "cdp_path": format!("/v1/sessions/{}/cdp", self.id),
            "keep_alive": self.keep_alive,
            "connected": self.connected.load(Ordering::Acquire),
            "url": url,
            "profile": self.profile,
            "proxy_session": self.proxy_session,
            "created_at": self.created_at,
            "expires_at": expires_at,
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

    fn remove(&self, id: &str) {
        self.inner.lock().unwrap().sessions.remove(id);
    }

    fn list(&self) -> Vec<Value> {
        let mut items: Vec<_> = self
            .inner
            .lock()
            .unwrap()
            .sessions
            .values()
            .map(|s| s.summary())
            .collect();
        items.sort_by_key(|v| v["created_at"].as_u64().unwrap_or(0));
        items
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

pub(crate) async fn create_session(
    State(inner): State<Arc<Inner>>,
    headers: HeaderMap,
    request: Request,
) -> Result<Response, ApiError> {
    let Some(engine) = inner.engine.clone() else {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            "managed sessions are unavailable in coordinator mode",
        ));
    };
    if !inner.with_registry(|r, _| r.accepting) {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "shutting_down",
            "the server is shutting down",
        ));
    }

    let bytes = to_bytes(request.into_body(), crate::MAX_REQUEST_BYTES)
        .await
        .map_err(|_| {
            ApiError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "payload_too_large",
                "session request body exceeds 64 KiB",
            )
        })?;
    let req: CreateSessionRequest = if bytes.iter().all(u8::is_ascii_whitespace) {
        CreateSessionRequest::default()
    } else {
        let Json(parsed) = Json::<CreateSessionRequest>::from_bytes(&bytes)
            .map_err(|e| ApiError::new(e.status(), "invalid_request", e.body_text()))?;
        parsed
    };
    req.validate()
        .map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, "invalid_request", e))?;

    let reservation = inner.sessions.try_reserve().ok_or_else(|| {
        ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "capacity_exceeded",
            "maximum active managed browser sessions reached",
        )
    })?;

    let opts = req.job_options();
    let lease = Arc::new(engine.acquire_context_with(&opts).await.map_err(|e| match e {
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
    })?);

    lease.discard_warm_page().await;
    lease.mark_external_context(lease.context_id());

    let id = format!("sess-{}", Uuid::new_v4().simple());
    let conn = lease.cdp_connection();
    let primary_context = lease.context_id().to_string();
    let live_target = Arc::new(StdMutex::new(None));
    let live_url = Arc::new(StdMutex::new(String::from("about:blank")));
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
        enforce_server_proxy: compute_enforce_server_proxy(&engine),
        resource_preset: req.resource_preset.map(|p| match p {
            ServerResourcePreset::Scraping => fluxwright::ResourcePreset::Scraping,
            ServerResourcePreset::Minimal => fluxwright::ResourcePreset::Minimal,
            ServerResourcePreset::NoMedia => fluxwright::ResourcePreset::NoMedia,
        }),
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
    if create_initial_page(&conn, &state, &lease, &policy, viewport)
        .await
        .is_none()
    {
        teardown_gateway_state(&conn, &state, &lease, &policy).await;
        drop(live_guard);
        return Err(ApiError::new(
            StatusCode::BAD_GATEWAY,
            "browser_error",
            "failed to create initial page for managed session",
        ));
    }

    let (embed_token, effective_ttl, expires_at) = match inner
        .live
        .issue_embed(&id, req.live_view_mode, req.ttl_secs)
    {
        Ok(res) => res,
        Err(err) => {
            teardown_gateway_state(&conn, &state, &lease, &policy).await;
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
        keep_alive: req.keep_alive,
        profile: req.profile,
        persist_profile: req.persist_profile,
        proxy_session: req.proxy_session,
        created_at,
        lease,
        state,
        live_url,
        policy,
        connected: AtomicBool::new(false),
        expires_tx,
        close_tx,
        closed_notify,
    });

    inner.sessions.insert(reservation, session.clone());
    spawn_session_reaper(
        inner.clone(),
        engine,
        session.clone(),
        expires_rx,
        close_rx,
        cdp_events,
        live_guard,
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
        "created_at": session.created_at,
        "expires_at": expires_at,
        "ttl_secs": effective_ttl,
    });
    Ok((StatusCode::CREATED, Json(body)).into_response())
}

pub(crate) async fn list_sessions(State(inner): State<Arc<Inner>>) -> Json<Value> {
    Json(json!({ "sessions": inner.sessions.list() }))
}

pub(crate) async fn get_session(
    State(inner): State<Arc<Inner>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    validate_session_id(&id)?;
    let sess = inner.sessions.get(&id).ok_or_else(session_not_found)?;
    Ok(Json(sess.summary()))
}

pub(crate) async fn extend_session(
    State(inner): State<Arc<Inner>>,
    Path(id): Path<String>,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    validate_session_id(&id)?;
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

    let sess = inner.sessions.get(&id).ok_or_else(session_not_found)?;
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
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    validate_session_id(&id)?;
    let sess = inner.sessions.get(&id).ok_or_else(session_not_found)?;
    let notified = sess.closed_notify.notified();
    tokio::pin!(notified);
    notified.as_mut().enable();
    let _ = sess.close_tx.send(true);
    if inner.sessions.get(&id).is_some() {
        let _ = tokio::time::timeout(DELETE_WAIT_TIMEOUT, notified).await;
    }
    Ok(Json(json!({ "id": id, "status": "closed" })))
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
    ws: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    validate_session_id(&id)?;
    let Some(_) = inner.engine else {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            "managed sessions are unavailable in coordinator mode",
        ));
    };
    let sess = inner.sessions.get(&id).ok_or_else(session_not_found)?;
    if *sess.close_tx.borrow() || sess.lease.is_closed() {
        let _ = sess.close_tx.send(true);
        return Err(session_not_found());
    }
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

fn spawn_session_reaper(
    inner: Arc<Inner>,
    engine: fluxwright::BrowserEngine,
    session: Arc<ManagedSession>,
    mut expires_rx: watch::Receiver<(Instant, u64)>,
    mut close_rx: watch::Receiver<bool>,
    mut cdp_events: mpsc::UnboundedReceiver<Arc<CdpEvent>>,
    live_guard: LiveGuard,
) {
    tokio::spawn(async move {
        let _live_guard = live_guard;
        let mut shutdown_rx = inner.shutdown_tx.subscribe();
        let conn = session.lease.cdp_connection();
        let has_preset = session.policy.resource_preset.is_some();
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
                    if has_preset && ev.method == "Fetch.requestPaused" {
                        if let Some(sid) = ev.session_id.as_deref() {
                            if session.state.lock().await.is_blocker_session(sid) {
                                if let Some(req_id) =
                                    ev.params["requestId"].as_str().map(str::to_owned)
                                {
                                    spawn_fail_blocked_request(&conn, sid, req_id);
                                }
                            }
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
        inner.sessions.remove(&session.id);
        drop(_live_guard);
        session.closed_notify.notify_waiters();
    });
}

async fn persist_session_profile(
    engine: &fluxwright::BrowserEngine,
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
