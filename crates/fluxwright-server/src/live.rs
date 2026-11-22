//! Live browser viewport screencast streaming (`Page.startScreencast`), single-use
//! WebSocket ticket auth, and Human-in-the-Loop (HITL) mouse/keyboard forwarding.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, RawQuery, Request, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use fluxwright::{netguard, Connection};
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use serde_json::{json, Value};
use tokio::sync::watch;
use uuid::Uuid;

use crate::cdp_gateway::percent_decode_str;
use crate::{parse_id, same_token, ApiError, Inner};

const MAX_LIVE_VIEWERS: usize = 16;
const MAX_TICKETS: usize = 256;
const TICKET_TTL: Duration = Duration::from_secs(30);
const MAX_WS_PAYLOAD_BYTES: usize = 16 * 1024;
const CDP_CALL_TIMEOUT: Duration = Duration::from_secs(5);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(2);
const STATIC_FALLBACK_DELAY: Duration = Duration::from_millis(300);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LiveKind {
    Job,
    Cdp,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct LiveSessionInfo {
    pub(crate) id: String,
    pub(crate) kind: LiveKind,
    pub(crate) url: String,
    pub(crate) started_at: u64,
    pub(crate) paused: bool,
    pub(crate) pause_reason: Option<String>,
    pub(crate) can_pause: bool,
}

#[derive(Clone)]
pub(crate) struct LiveEntry {
    pub(crate) id: String,
    pub(crate) kind: LiveKind,
    pub(crate) conn: Arc<Connection>,
    pub(crate) target_id: Arc<StdMutex<Option<String>>>,
    pub(crate) url: Arc<StdMutex<String>>,
    pub(crate) started_at: u64,
    pub(crate) pause_tx: Option<watch::Sender<bool>>,
    pub(crate) pause_reason: Arc<StdMutex<Option<String>>>,
}

impl LiveEntry {
    fn info(&self) -> LiveSessionInfo {
        let paused = self.pause_tx.as_ref().is_some_and(|tx| *tx.borrow());
        LiveSessionInfo {
            id: self.id.clone(),
            kind: self.kind,
            url: self.url.lock().unwrap().clone(),
            started_at: self.started_at,
            paused,
            pause_reason: paused.then(|| self.pause_reason.lock().unwrap().clone()).flatten(),
            can_pause: self.pause_tx.is_some(),
        }
    }

    fn set_url_if_nonempty(&self, next_url: &str) {
        if !next_url.is_empty() {
            *self.url.lock().unwrap() = next_url.to_string();
        }
    }
}

struct LiveTicket {
    session_id: String,
    expires_at: Instant,
}

#[derive(Default)]
pub(crate) struct LiveRegistry {
    targets: Arc<StdMutex<HashMap<String, LiveEntry>>>,
    tickets: StdMutex<HashMap<String, LiveTicket>>,
    active_viewers: Arc<AtomicUsize>,
}

pub(crate) struct LiveGuard {
    id: String,
    url: Arc<StdMutex<String>>,
    pause_reason: Arc<StdMutex<Option<String>>>,
    targets: Arc<StdMutex<HashMap<String, LiveEntry>>>,
}

impl LiveGuard {
    pub(crate) fn set_url(&self, next_url: String) {
        *self.url.lock().unwrap() = next_url;
    }

    pub(crate) fn set_pause_reason(&self, reason: Option<String>) {
        *self.pause_reason.lock().unwrap() = reason;
    }
}

impl Drop for LiveGuard {
    fn drop(&mut self) {
        self.targets.lock().unwrap().remove(&self.id);
    }
}

struct ViewerGuard {
    active_viewers: Arc<AtomicUsize>,
}

impl Drop for ViewerGuard {
    fn drop(&mut self) {
        self.active_viewers.fetch_sub(1, Ordering::Relaxed);
    }
}

impl LiveRegistry {
    pub(crate) fn register_job(
        &self,
        id: String,
        conn: Arc<Connection>,
        target_id: String,
        initial_url: String,
        started_at: u64,
        pause_tx: watch::Sender<bool>,
        pause_reason: Arc<StdMutex<Option<String>>>,
    ) -> LiveGuard {
        let url = Arc::new(StdMutex::new(initial_url));
        let entry = LiveEntry {
            id: id.clone(),
            kind: LiveKind::Job,
            conn,
            target_id: Arc::new(StdMutex::new(Some(target_id))),
            url: url.clone(),
            started_at,
            pause_tx: Some(pause_tx),
            pause_reason: pause_reason.clone(),
        };
        self.targets.lock().unwrap().insert(id.clone(), entry);
        LiveGuard {
            id,
            url,
            pause_reason,
            targets: self.targets.clone(),
        }
    }

    pub(crate) fn register_cdp(
        &self,
        id: String,
        conn: Arc<Connection>,
        target_id: Arc<StdMutex<Option<String>>>,
        url: Arc<StdMutex<String>>,
        started_at: u64,
    ) -> LiveGuard {
        let pause_reason = Arc::new(StdMutex::new(None));
        let entry = LiveEntry {
            id: id.clone(),
            kind: LiveKind::Cdp,
            conn,
            target_id,
            url: url.clone(),
            started_at,
            pause_tx: None,
            pause_reason: pause_reason.clone(),
        };
        self.targets.lock().unwrap().insert(id.clone(), entry);
        LiveGuard {
            id,
            url,
            pause_reason,
            targets: self.targets.clone(),
        }
    }

    pub(crate) fn get(&self, id: &str) -> Option<LiveEntry> {
        self.targets.lock().unwrap().get(id).cloned()
    }

    pub(crate) fn job_flags(&self, id: &str) -> (bool, bool) {
        let guard = self.targets.lock().unwrap();
        match guard.get(id) {
            Some(entry) => (
                true,
                entry.pause_tx.as_ref().is_some_and(|tx| *tx.borrow()),
            ),
            None => (false, false),
        }
    }

    pub(crate) fn snapshot(&self) -> Vec<LiveSessionInfo> {
        let mut list: Vec<LiveSessionInfo> = self
            .targets
            .lock()
            .unwrap()
            .values()
            .map(LiveEntry::info)
            .collect();
        list.sort_by(|a, b| b.started_at.cmp(&a.started_at).then_with(|| a.id.cmp(&b.id)));
        list
    }

    pub(crate) fn set_paused(&self, id: &str, paused: bool) -> Result<bool, ApiError> {
        let entry = self.get(id).ok_or_else(|| {
            ApiError::new(StatusCode::NOT_FOUND, "not_found", "live session not found")
        })?;
        let Some(tx) = &entry.pause_tx else {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "not_pausable",
                "only HTTP jobs can be paused or resumed",
            ));
        };
        apply_pause_state(tx, &entry.pause_reason, paused, None);
        Ok(paused)
    }

    pub(crate) fn issue_ticket(&self, session_id: &str) -> Result<String, ApiError> {
        self.issue_ticket_at(session_id, Instant::now())
    }

    fn issue_ticket_at(&self, session_id: &str, now: Instant) -> Result<String, ApiError> {
        if self.get(session_id).is_none() {
            return Err(ApiError::new(
                StatusCode::NOT_FOUND,
                "not_found",
                "live session not found",
            ));
        }
        let mut tickets = self.tickets.lock().unwrap();
        tickets.retain(|_, t| t.expires_at > now);
        if tickets.len() >= MAX_TICKETS {
            return Err(ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                "capacity_exceeded",
                "too many pending live tickets",
            ));
        }
        let secret = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        tickets.insert(
            secret.clone(),
            LiveTicket {
                session_id: session_id.to_string(),
                expires_at: now + TICKET_TTL,
            },
        );
        Ok(secret)
    }

    pub(crate) fn consume_ticket(&self, secret: &str, session_id: &str) -> bool {
        self.consume_ticket_at(secret, session_id, Instant::now())
    }

    fn consume_ticket_at(&self, secret: &str, session_id: &str, now: Instant) -> bool {
        let mut tickets = self.tickets.lock().unwrap();
        tickets.retain(|_, t| t.expires_at > now);
        let Some(ticket) = tickets.remove(secret) else {
            return false;
        };
        ticket.session_id == session_id && now <= ticket.expires_at
    }

    fn acquire_viewer(&self) -> Result<ViewerGuard, ApiError> {
        let res = self.active_viewers.try_update(
            Ordering::SeqCst,
            Ordering::SeqCst,
            |cur| (cur < MAX_LIVE_VIEWERS).then_some(cur + 1),
        );
        if res.is_err() {
            return Err(ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                "capacity_exceeded",
                "too many active live viewers",
            ));
        }
        Ok(ViewerGuard {
            active_viewers: self.active_viewers.clone(),
        })
    }
}

fn apply_pause_state(
    tx: &watch::Sender<bool>,
    reason_slot: &Arc<StdMutex<Option<String>>>,
    paused: bool,
    reason_override: Option<String>,
) {
    {
        let mut guard = reason_slot.lock().unwrap();
        if paused {
            if let Some(r) = reason_override {
                *guard = Some(r);
            } else if guard.is_none() {
                *guard = Some("Paused by operator".into());
            }
        } else {
            *guard = None;
        }
    }
    tx.send_if_modified(|cur| {
        if *cur == paused {
            false
        } else {
            *cur = paused;
            true
        }
    });
}

fn extract_single_query_param(query: &str, key: &str) -> Option<String> {
    let mut found: Option<String> = None;
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = pair.split_once('=')?;
        if k == key {
            if found.is_some() {
                return None;
            }
            let decoded = percent_decode_str(v)?;
            if decoded.is_empty() {
                return None;
            }
            found = Some(decoded);
        }
    }
    found
}

fn extract_live_ws_id(path: &str) -> Option<&str> {
    let id = path.strip_prefix("/v1/live/")?.strip_suffix("/ws")?;
    (!id.is_empty() && !id.contains('/')).then_some(id)
}

/// Auth middleware exclusively for `GET /v1/live/{id}/ws`.
/// Accepts `Authorization: Bearer <token>` OR a single-use `?ticket=<val>` bound to `{id}`.
/// Never accepts `?token=`.
pub(crate) async fn authorize_ws(
    State(inner): State<Arc<Inner>>,
    request: Request,
    next: Next,
) -> Response {
    let header_valid = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.as_bytes().split_at_checked(7))
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case(b"Bearer "))
        .is_some_and(|(_, supplied)| same_token(supplied, &inner.token));

    let ticket_valid = if header_valid {
        true
    } else if let Some(id) = extract_live_ws_id(request.uri().path()) {
        request
            .uri()
            .query()
            .and_then(|q| extract_single_query_param(q, "ticket"))
            .is_some_and(|ticket| inner.live.consume_ticket(&ticket, id))
    } else {
        false
    };

    if !ticket_valid {
        let mut response = ApiError::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "a bearer token or single-use live ticket is required",
        )
        .into_response();
        response
            .headers_mut()
            .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        return response;
    }

    next.run(request).await
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LiveStreamQuery {
    quality: u8,
    max_width: u32,
    max_height: u32,
}

impl Default for LiveStreamQuery {
    fn default() -> Self {
        Self {
            quality: 65,
            max_width: 1280,
            max_height: 800,
        }
    }
}

impl LiveStreamQuery {
    fn parse(raw: Option<&str>) -> Result<Self, ApiError> {
        let mut out = Self::default();
        let Some(raw) = raw.filter(|s| !s.is_empty()) else {
            return Ok(out);
        };
        let mut seen = HashSet::new();
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
            let val = percent_decode_str(v).ok_or_else(|| {
                ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "invalid_query",
                    format!("invalid percent-encoding in parameter: {k}"),
                )
            })?;
            match k {
                "ticket" => {}
                "quality" => {
                    let q = val.parse::<u8>().ok().filter(|q| (20..=95).contains(q));
                    out.quality = q.ok_or_else(|| {
                        ApiError::new(
                            StatusCode::BAD_REQUEST,
                            "invalid_query",
                            "quality must be an integer between 20 and 95",
                        )
                    })?;
                }
                "max_width" => {
                    let w = val.parse::<u32>().ok().filter(|w| (320..=1920).contains(w));
                    out.max_width = w.ok_or_else(|| {
                        ApiError::new(
                            StatusCode::BAD_REQUEST,
                            "invalid_query",
                            "max_width must be an integer between 320 and 1920",
                        )
                    })?;
                }
                "max_height" => {
                    let h = val.parse::<u32>().ok().filter(|h| (240..=1200).contains(h));
                    out.max_height = h.ok_or_else(|| {
                        ApiError::new(
                            StatusCode::BAD_REQUEST,
                            "invalid_query",
                            "max_height must be an integer between 240 and 1200",
                        )
                    })?;
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

pub(crate) async fn list_sessions(State(inner): State<Arc<Inner>>) -> Json<Value> {
    Json(json!({ "sessions": inner.live.snapshot() }))
}

pub(crate) async fn create_ticket(
    State(inner): State<Arc<Inner>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let ticket = inner.live.issue_ticket(&id)?;
    Ok(Json(json!({
        "ticket": ticket,
        "ws_url": format!("/v1/live/{id}/ws?ticket={ticket}"),
        "expires_in_secs": TICKET_TTL.as_secs(),
    })))
}

pub(crate) async fn pause_job(
    State(inner): State<Arc<Inner>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let uuid = parse_id(&id)?;
    let paused = inner.live.set_paused(&uuid.to_string(), true)?;
    Ok(Json(json!({ "id": uuid, "paused": paused })))
}

pub(crate) async fn resume_job(
    State(inner): State<Arc<Inner>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let uuid = parse_id(&id)?;
    let paused = inner.live.set_paused(&uuid.to_string(), false)?;
    Ok(Json(json!({ "id": uuid, "paused": paused })))
}

pub(crate) async fn ws_handler(
    State(inner): State<Arc<Inner>>,
    Path(id): Path<String>,
    RawQuery(raw_query): RawQuery,
    ws: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    let query = LiveStreamQuery::parse(raw_query.as_deref())?;
    let entry = inner.live.get(&id).ok_or_else(|| {
        ApiError::new(StatusCode::NOT_FOUND, "not_found", "live session not found")
    })?;
    let viewer_guard = inner.live.acquire_viewer()?;
    Ok(ws
        .max_frame_size(MAX_WS_PAYLOAD_BYTES)
        .max_message_size(MAX_WS_PAYLOAD_BYTES)
        .on_upgrade(move |socket| run_live_stream(socket, inner, entry, query, viewer_guard)))
}

#[derive(Debug, PartialEq)]
enum ClientCommand {
    Mouse(Value),
    Key(Value),
    Text(String),
    Navigate(String),
    Pause,
    Resume,
}

fn parse_client_command(raw: &str, any_url: bool) -> Result<ClientCommand, String> {
    let val: Value = serde_json::from_str(raw).map_err(|_| "invalid JSON command".to_string())?;
    let kind = val["type"]
        .as_str()
        .ok_or_else(|| "command missing string field 'type'".to_string())?;
    match kind {
        "mouse" => {
            let event = val["event"].as_str().unwrap_or("");
            if !matches!(
                event,
                "mousePressed" | "mouseReleased" | "mouseMoved" | "mouseWheel"
            ) {
                return Err(format!("invalid mouse event {event:?}"));
            }
            let x = val["x"]
                .as_f64()
                .filter(|v| v.is_finite())
                .ok_or_else(|| "mouse x must be a finite number".to_string())?;
            let y = val["y"]
                .as_f64()
                .filter(|v| v.is_finite())
                .ok_or_else(|| "mouse y must be a finite number".to_string())?;
            let button = val["button"].as_str().unwrap_or("none");
            if !matches!(button, "none" | "left" | "middle" | "right") {
                return Err(format!("invalid mouse button {button:?}"));
            }
            let click_count = val["clickCount"].as_u64().unwrap_or(1).min(8);
            let modifiers = val["modifiers"].as_u64().unwrap_or(0) & 15;
            let mut params = json!({
                "type": event,
                "x": x,
                "y": y,
                "button": button,
                "modifiers": modifiers,
            });
            if matches!(event, "mousePressed" | "mouseReleased") {
                params["clickCount"] = json!(click_count);
            }
            if event == "mouseWheel" {
                let dx = val["deltaX"].as_f64().unwrap_or(0.0);
                let dy = val["deltaY"].as_f64().unwrap_or(0.0);
                if !dx.is_finite() || !dy.is_finite() {
                    return Err("mouseWheel deltas must be finite".into());
                }
                params["deltaX"] = json!(dx);
                params["deltaY"] = json!(dy);
            }
            Ok(ClientCommand::Mouse(params))
        }
        "key" => {
            let event = val["event"].as_str().unwrap_or("");
            if !matches!(event, "keyDown" | "keyUp" | "rawKeyDown" | "char") {
                return Err(format!("invalid key event {event:?}"));
            }
            let modifiers = val["modifiers"].as_u64().unwrap_or(0) & 15;
            let mut params = json!({
                "type": event,
                "modifiers": modifiers,
            });
            if let Some(key) = val["key"].as_str() {
                params["key"] = json!(key);
            }
            if let Some(code) = val["code"].as_str() {
                params["code"] = json!(code);
            }
            if let Some(text) = val["text"].as_str().filter(|t| !t.is_empty()) {
                params["text"] = json!(text);
            }
            if let Some(vk) = val["windowsVirtualKeyCode"].as_i64() {
                params["windowsVirtualKeyCode"] = json!(vk);
                params["nativeVirtualKeyCode"] = json!(vk);
            }
            Ok(ClientCommand::Key(params))
        }
        "text" => {
            let text = val["text"]
                .as_str()
                .ok_or_else(|| "text command requires string field 'text'".to_string())?;
            if text.len() > 4096 {
                return Err("text command exceeds 4096 bytes".into());
            }
            Ok(ClientCommand::Text(text.to_string()))
        }
        "navigate" => {
            let url = val["url"]
                .as_str()
                .ok_or_else(|| "navigate command requires string field 'url'".to_string())?;
            if !any_url {
                netguard::url_allowed(url)?;
            }
            Ok(ClientCommand::Navigate(url.to_string()))
        }
        "pause" => Ok(ClientCommand::Pause),
        "resume" => Ok(ClientCommand::Resume),
        _ => Err(format!("unknown command type {kind:?}")),
    }
}

async fn send_json(ws_tx: &mut futures_util::stream::SplitSink<WebSocket, Message>, val: &Value) -> bool {
    ws_tx
        .send(Message::Text(val.to_string().into()))
        .await
        .is_ok()
}

fn status_payload(entry: &LiveEntry) -> Value {
    let info = entry.info();
    json!({
        "type": "status",
        "id": info.id,
        "kind": info.kind,
        "url": info.url,
        "paused": info.paused,
        "pause_reason": info.pause_reason,
        "can_pause": info.can_pause,
    })
}

async fn resolve_target_id(entry: &LiveEntry) -> Option<String> {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(tid) = entry.target_id.lock().unwrap().clone() {
            return Some(tid);
        }
        if Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn run_live_stream(
    socket: WebSocket,
    inner: Arc<Inner>,
    entry: LiveEntry,
    query: LiveStreamQuery,
    _viewer_guard: ViewerGuard,
) {
    let (mut ws_tx, mut ws_rx) = socket.split();

    let Some(tid) = resolve_target_id(&entry).await else {
        let _ = send_json(
            &mut ws_tx,
            &json!({ "type": "error", "message": "no active page target" }),
        )
        .await;
        let _ = ws_tx.send(Message::Close(None)).await;
        return;
    };

    if let Ok(info) = entry
        .conn
        .call(
            "Target.getTargetInfo",
            json!({ "targetId": tid }),
            None,
            CDP_CALL_TIMEOUT,
        )
        .await
    {
        if let Some(u) = info["targetInfo"]["url"].as_str() {
            entry.set_url_if_nonempty(u);
        }
    }

    // Subscribe to Chrome events BEFORE attaching so no initial frame or detach slips past.
    let mut cdp_events = entry.conn.subscribe_unbounded();

    let obs_sid = match entry
        .conn
        .call(
            "Target.attachToTarget",
            json!({ "targetId": tid, "flatten": true }),
            None,
            CDP_CALL_TIMEOUT,
        )
        .await
    {
        Ok(res) => match res["sessionId"].as_str() {
            Some(sid) => sid.to_string(),
            None => {
                let _ = send_json(
                    &mut ws_tx,
                    &json!({ "type": "error", "message": "attachToTarget did not return sessionId" }),
                )
                .await;
                let _ = ws_tx.send(Message::Close(None)).await;
                return;
            }
        },
        Err(err) => {
            let _ = send_json(
                &mut ws_tx,
                &json!({ "type": "error", "message": err.to_string() }),
            )
            .await;
            let _ = ws_tx.send(Message::Close(None)).await;
            return;
        }
    };

    let _ = entry
        .conn
        .call("Page.enable", json!({}), Some(&obs_sid), CDP_CALL_TIMEOUT)
        .await;

    if let Err(err) = entry
        .conn
        .call(
            "Page.startScreencast",
            json!({
                "format": "jpeg",
                "quality": query.quality,
                "maxWidth": query.max_width,
                "maxHeight": query.max_height,
                "everyNthFrame": 1,
            }),
            Some(&obs_sid),
            CDP_CALL_TIMEOUT,
        )
        .await
    {
        let _ = send_json(
            &mut ws_tx,
            &json!({ "type": "error", "message": format!("startScreencast: {err}") }),
        )
        .await;
        let _ = entry
            .conn
            .call(
                "Target.detachFromTarget",
                json!({ "sessionId": obs_sid }),
                None,
                CLEANUP_TIMEOUT,
            )
            .await;
        let _ = ws_tx.send(Message::Close(None)).await;
        return;
    }

    if !send_json(&mut ws_tx, &status_payload(&entry)).await {
        let _ = entry
            .conn
            .call("Page.stopScreencast", json!({}), Some(&obs_sid), CLEANUP_TIMEOUT)
            .await;
        let _ = entry
            .conn
            .call(
                "Target.detachFromTarget",
                json!({ "sessionId": obs_sid }),
                None,
                CLEANUP_TIMEOUT,
            )
            .await;
        return;
    }

    let any_url = std::env::var_os("FLUXWRIGHT_SERVER_ANY_URL").is_some();
    let mut shutdown_rx = inner.shutdown_tx.subscribe();
    let mut pause_rx = entry.pause_tx.as_ref().map(|tx| tx.subscribe());
    let mut got_frame = false;
    let fallback_sleep = tokio::time::sleep(STATIC_FALLBACK_DELAY);
    tokio::pin!(fallback_sleep);
    let mut target_detached = false;

    loop {
        if *shutdown_rx.borrow() {
            let _ = send_json(
                &mut ws_tx,
                &json!({ "type": "ended", "reason": "server shutting down" }),
            )
            .await;
            break;
        }

        tokio::select! {
            res = shutdown_rx.changed() => {
                if res.is_err() || *shutdown_rx.borrow() {
                    let _ = send_json(
                        &mut ws_tx,
                        &json!({ "type": "ended", "reason": "server shutting down" }),
                    ).await;
                    break;
                }
            }
            Ok(()) = async {
                match pause_rx.as_mut() {
                    Some(rx) => rx.changed().await,
                    None => std::future::pending().await,
                }
            } => {
                if !send_json(&mut ws_tx, &status_payload(&entry)).await {
                    break;
                }
            }
            () = &mut fallback_sleep, if !got_frame => {
                got_frame = true;
                if let Some(frame_msg) = capture_fallback_frame(&entry.conn, &obs_sid, query.quality).await {
                    if !send_json(&mut ws_tx, &frame_msg).await {
                        break;
                    }
                }
            }
            ev = cdp_events.recv() => {
                let Some(ev) = ev else {
                    let _ = send_json(
                        &mut ws_tx,
                        &json!({ "type": "ended", "reason": "browser disconnected" }),
                    ).await;
                    target_detached = true;
                    break;
                };
                if ev.method == "Fluxwright.disconnected" {
                    let _ = send_json(
                        &mut ws_tx,
                        &json!({ "type": "ended", "reason": "browser disconnected" }),
                    ).await;
                    target_detached = true;
                    break;
                }
                if ev.method == "Target.detachedFromTarget"
                    && ev.params["sessionId"].as_str() == Some(obs_sid.as_str())
                {
                    let _ = send_json(
                        &mut ws_tx,
                        &json!({ "type": "ended", "reason": "page closed" }),
                    ).await;
                    target_detached = true;
                    break;
                }
                if ev.session_id.as_deref() != Some(obs_sid.as_str()) {
                    continue;
                }
                match ev.method.as_str() {
                    "Page.screencastFrame" => {
                        got_frame = true;
                        let meta = &ev.params["metadata"];
                        let msg = json!({
                            "type": "frame",
                            "data": ev.params["data"],
                            "width": meta["deviceWidth"].as_f64().unwrap_or(query.max_width as f64),
                            "height": meta["deviceHeight"].as_f64().unwrap_or(query.max_height as f64),
                            "timestamp": meta["timestamp"].as_f64().unwrap_or(0.0),
                        });
                        if !send_json(&mut ws_tx, &msg).await {
                            break;
                        }
                        if let Some(ack_id) = ev.params.get("sessionId").cloned() {
                            let _ = entry.conn.call(
                                "Page.screencastFrameAck",
                                json!({ "sessionId": ack_id }),
                                Some(&obs_sid),
                                CLEANUP_TIMEOUT,
                            ).await;
                        }
                    }
                    "Page.frameNavigated" if !ev.params["frame"]["parentId"].is_string() => {
                        if let Some(u) = ev.params["frame"]["url"].as_str() {
                            entry.set_url_if_nonempty(u);
                            if !send_json(&mut ws_tx, &status_payload(&entry)).await {
                                break;
                            }
                        }
                    }
                    _ => {}
                }
            }
            msg = ws_rx.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        match parse_client_command(text.as_str(), any_url) {
                            Ok(ClientCommand::Mouse(params)) => {
                                let _ = entry.conn.call("Input.dispatchMouseEvent", params, Some(&obs_sid), CDP_CALL_TIMEOUT).await;
                            }
                            Ok(ClientCommand::Key(params)) => {
                                let _ = entry.conn.call("Input.dispatchKeyEvent", params, Some(&obs_sid), CDP_CALL_TIMEOUT).await;
                            }
                            Ok(ClientCommand::Text(t)) => {
                                let _ = entry.conn.call("Input.insertText", json!({ "text": t }), Some(&obs_sid), CDP_CALL_TIMEOUT).await;
                            }
                            Ok(ClientCommand::Navigate(url)) => {
                                if let Err(err) = entry.conn.call("Page.navigate", json!({ "url": url }), Some(&obs_sid), CDP_CALL_TIMEOUT).await {
                                    let _ = send_json(&mut ws_tx, &json!({ "type": "error", "message": err.to_string() })).await;
                                }
                            }
                            Ok(ClientCommand::Pause) => {
                                if let Some(tx) = &entry.pause_tx {
                                    apply_pause_state(tx, &entry.pause_reason, true, None);
                                    if !send_json(&mut ws_tx, &status_payload(&entry)).await {
                                        break;
                                    }
                                } else {
                                    let _ = send_json(&mut ws_tx, &json!({ "type": "error", "message": "CDP gateway sessions cannot be paused" })).await;
                                }
                            }
                            Ok(ClientCommand::Resume) => {
                                if let Some(tx) = &entry.pause_tx {
                                    apply_pause_state(tx, &entry.pause_reason, false, None);
                                    if !send_json(&mut ws_tx, &status_payload(&entry)).await {
                                        break;
                                    }
                                } else {
                                    let _ = send_json(&mut ws_tx, &json!({ "type": "error", "message": "CDP gateway sessions cannot be resumed" })).await;
                                }
                            }
                            Err(err) => {
                                if !send_json(&mut ws_tx, &json!({ "type": "error", "message": err })).await {
                                    break;
                                }
                            }
                        }
                    }
                    Some(Ok(Message::Ping(payload))) => {
                        if ws_tx.send(Message::Pong(payload)).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(Message::Pong(_))) => {}
                    Some(Ok(Message::Binary(_))) => {
                        let _ = send_json(&mut ws_tx, &json!({ "type": "error", "message": "binary WebSocket messages are not supported" })).await;
                    }
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                }
            }
        }
    }

    if !target_detached {
        let _ = entry
            .conn
            .call("Page.stopScreencast", json!({}), Some(&obs_sid), CLEANUP_TIMEOUT)
            .await;
        let _ = entry
            .conn
            .call(
                "Target.detachFromTarget",
                json!({ "sessionId": obs_sid }),
                None,
                CLEANUP_TIMEOUT,
            )
            .await;
    }
    let _ = ws_tx.send(Message::Close(None)).await;
}

async fn capture_fallback_frame(conn: &Connection, sid: &str, quality: u8) -> Option<Value> {
    let metrics = conn
        .call("Page.getLayoutMetrics", json!({}), Some(sid), CDP_CALL_TIMEOUT)
        .await
        .ok();
    let vp = metrics.as_ref().map(|m| {
        if m["cssLayoutViewport"].is_object() {
            &m["cssLayoutViewport"]
        } else {
            &m["layoutViewport"]
        }
    });
    let width = vp
        .and_then(|v| v["clientWidth"].as_f64())
        .filter(|w| *w > 0.0)
        .unwrap_or(1280.0);
    let height = vp
        .and_then(|v| v["clientHeight"].as_f64())
        .filter(|h| *h > 0.0)
        .unwrap_or(720.0);
    let shot = conn
        .call(
            "Page.captureScreenshot",
            json!({ "format": "jpeg", "quality": quality }),
            Some(sid),
            CDP_CALL_TIMEOUT,
        )
        .await
        .ok()?;
    let data = shot["data"].as_str()?;
    Some(json!({
        "type": "frame",
        "data": data,
        "width": width,
        "height": height,
        "timestamp": 0.0,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_query_parses_and_rejects_invalid_bounds() {
        assert_eq!(LiveStreamQuery::parse(None).unwrap(), LiveStreamQuery::default());
        assert_eq!(
            LiveStreamQuery::parse(Some("ticket=abc&quality=80&max_width=640&max_height=480")).unwrap(),
            LiveStreamQuery {
                quality: 80,
                max_width: 640,
                max_height: 480,
            }
        );
        for bad in [
            "quality=10",
            "quality=96",
            "max_width=100",
            "max_width=4000",
            "max_height=100",
            "max_height=2000",
            "quality=70&quality=80",
            "bogus=1",
            "malformed",
        ] {
            assert!(LiveStreamQuery::parse(Some(bad)).is_err(), "{bad}");
        }
    }

    #[test]
    fn extract_live_ws_id_and_single_param() {
        assert_eq!(extract_live_ws_id("/v1/live/job-123/ws"), Some("job-123"));
        assert_eq!(extract_live_ws_id("/v1/live//ws"), None);
        assert_eq!(extract_live_ws_id("/v1/live/a/b/ws"), None);
        assert_eq!(
            extract_single_query_param("ticket=tok_1&quality=70", "ticket"),
            Some("tok_1".into())
        );
        assert_eq!(
            extract_single_query_param("ticket=a&ticket=b", "ticket"),
            None
        );
        assert_eq!(extract_single_query_param("ticket=", "ticket"), None);
        assert_eq!(extract_single_query_param("token=secret", "ticket"), None);
    }

    #[test]
    fn client_command_validation_and_ssrf_guard() {
        assert!(matches!(
            parse_client_command(r#"{"type":"mouse","event":"mousePressed","x":12,"y":34,"button":"left"}"#, false),
            Ok(ClientCommand::Mouse(_))
        ));
        assert!(matches!(
            parse_client_command(r#"{"type":"key","event":"keyDown","key":"Enter","code":"Enter","windowsVirtualKeyCode":13}"#, false),
            Ok(ClientCommand::Key(_))
        ));
        assert!(matches!(
            parse_client_command(r#"{"type":"text","text":"hello"}"#, false),
            Ok(ClientCommand::Text(ref t)) if t == "hello"
        ));
        assert!(matches!(
            parse_client_command(r#"{"type":"navigate","url":"https://example.com"}"#, false),
            Ok(ClientCommand::Navigate(_))
        ));
        assert!(matches!(parse_client_command(r#"{"type":"pause"}"#, false), Ok(ClientCommand::Pause)));
        assert!(matches!(parse_client_command(r#"{"type":"resume"}"#, false), Ok(ClientCommand::Resume)));

        // SSRF checks on navigate
        assert!(parse_client_command(r#"{"type":"navigate","url":"file:///etc/passwd"}"#, false).is_err());
        assert!(parse_client_command(r#"{"type":"navigate","url":"http://169.254.169.254/latest/meta-data/"}"#, false).is_err());
        assert!(parse_client_command(r#"{"type":"navigate","url":"file:///etc/passwd"}"#, true).is_ok());

        // Bounds & invalid variants
        assert!(parse_client_command(r#"{"type":"mouse","event":"bogus","x":1,"y":2}"#, false).is_err());
        assert!(parse_client_command(&format!(r#"{{"type":"text","text":"{}"}}"#, "a".repeat(4097)), false).is_err());
        assert!(parse_client_command(r#"{"type":"unknown"}"#, false).is_err());
    }

    #[test]
    fn ticket_storage_enforces_single_use_session_binding_and_expiry() {
        let reg = LiveRegistry::default();
        let now = Instant::now();
        assert!(reg.issue_ticket_at("missing", now).is_err());

        // Directly insert a ticket to verify expiry, session mismatch, and single-use behavior.
        {
            let mut tickets = reg.tickets.lock().unwrap();
            tickets.insert(
                "sec-1".into(),
                LiveTicket {
                    session_id: "job-1".into(),
                    expires_at: now + Duration::from_secs(30),
                },
            );
            tickets.insert(
                "sec-expired".into(),
                LiveTicket {
                    session_id: "job-1".into(),
                    expires_at: now - Duration::from_secs(1),
                },
            );
        }

        // Wrong session consumes/fails? Look: wrong session removes OR must NOT match: wait!
        // Note `consume_ticket_at` removes on lookup so brute-forcing wrong session burns it too.
        assert!(!reg.consume_ticket_at("sec-expired", "job-1", now));
        assert!(!reg.consume_ticket_at("sec-1", "job-2", now));
        assert!(!reg.consume_ticket_at("sec-1", "job-1", now));

        {
            reg.tickets.lock().unwrap().insert(
                "sec-2".into(),
                LiveTicket {
                    session_id: "job-1".into(),
                    expires_at: now + Duration::from_secs(30),
                },
            );
        }
        assert!(reg.consume_ticket_at("sec-2", "job-1", now));
        assert!(!reg.consume_ticket_at("sec-2", "job-1", now));
    }
}
