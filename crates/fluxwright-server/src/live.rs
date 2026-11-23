//! Live browser viewport screencast streaming (`Page.startScreencast`), single-use
//! WebSocket ticket auth, embeddable signed live viewer URLs, and Human-in-the-Loop (HITL)
//! mouse/keyboard forwarding.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, RawQuery, Request, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use base64::Engine;
use fluxwright::{netguard, Connection};
use futures_util::{SinkExt, StreamExt};
use ring::digest;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::{mpsc, watch, Semaphore};
use tokio::task::JoinSet;
use uuid::Uuid;

use crate::cdp_gateway::percent_decode_str;
use crate::{now_ms, parse_id, same_token, ApiError, Inner};

const MAX_LIVE_VIEWERS: usize = 16;
const MAX_TICKETS: usize = 256;
const MAX_EMBED_TOKENS: usize = 256;
const TICKET_TTL: Duration = Duration::from_secs(30);
const DEFAULT_EMBED_TTL_SECS: u64 = 300;
const MIN_EMBED_TTL_SECS: u64 = 10;
const MAX_EMBED_TTL_SECS: u64 = 3600;
const MAX_WS_PAYLOAD_BYTES: usize = 16 * 1024;
const CDP_CALL_TIMEOUT: Duration = Duration::from_secs(5);
const INPUT_CALL_TIMEOUT: Duration = Duration::from_secs(2);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(2);
const STATIC_FALLBACK_DELAY: Duration = Duration::from_millis(300);
const INPUT_QUEUE_CAPACITY: usize = 256;
const MAX_INFLIGHT_ACKS: usize = 8;
const MAX_INFLIGHT_NAVIGATES: usize = 4;

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
    pub(crate) revoked_tx: watch::Sender<bool>,
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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EmbedMode {
    #[default]
    ReadOnly,
    Interactive,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EmbedRequest {
    #[serde(default)]
    mode: EmbedMode,
    #[serde(default = "default_embed_ttl_secs")]
    ttl_secs: u64,
}

impl Default for EmbedRequest {
    fn default() -> Self {
        Self {
            mode: EmbedMode::ReadOnly,
            ttl_secs: DEFAULT_EMBED_TTL_SECS,
        }
    }
}

fn default_embed_ttl_secs() -> u64 {
    DEFAULT_EMBED_TTL_SECS
}

#[derive(Debug, Clone)]
pub(crate) struct LiveAccess {
    pub(crate) read_only: bool,
    pub(crate) expires_at: Option<Instant>,
}

struct LiveTicket {
    session_id: String,
    expires_at: Instant,
}

struct EmbedToken {
    session_id: String,
    mode: EmbedMode,
    expires_at: Instant,
}

#[derive(Debug, Clone)]
pub(crate) struct VerifiedEmbed {
    pub(crate) session_id: String,
    pub(crate) mode: EmbedMode,
    pub(crate) expires_at: Instant,
}

fn sha256_token(token: &str) -> [u8; 32] {
    let d = digest::digest(&digest::SHA256, token.as_bytes());
    let mut out = [0u8; 32];
    out.copy_from_slice(d.as_ref());
    out
}

fn is_well_formed_embed_token(token: &str) -> bool {
    (32..=128).contains(&token.len()) && token.bytes().all(|b| b.is_ascii_alphanumeric())
}

fn is_safe_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

#[derive(Default)]
pub(crate) struct LiveRegistry {
    targets: Arc<StdMutex<HashMap<String, LiveEntry>>>,
    tickets: Arc<StdMutex<HashMap<String, LiveTicket>>>,
    embeds: Arc<StdMutex<HashMap<[u8; 32], EmbedToken>>>,
    active_viewers: Arc<AtomicUsize>,
}

pub(crate) struct LiveGuard {
    id: String,
    url: Arc<StdMutex<String>>,
    pause_reason: Arc<StdMutex<Option<String>>>,
    targets: Arc<StdMutex<HashMap<String, LiveEntry>>>,
    tickets: Arc<StdMutex<HashMap<String, LiveTicket>>>,
    embeds: Arc<StdMutex<HashMap<[u8; 32], EmbedToken>>>,
    revoked_tx: watch::Sender<bool>,
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
        let _ = self.revoked_tx.send(true);
        self.targets.lock().unwrap().remove(&self.id);
        self.tickets
            .lock()
            .unwrap()
            .retain(|_, t| t.session_id != self.id);
        self.embeds
            .lock()
            .unwrap()
            .retain(|_, t| t.session_id != self.id);
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
        let (revoked_tx, _) = watch::channel(false);
        let entry = LiveEntry {
            id: id.clone(),
            kind: LiveKind::Job,
            conn,
            target_id: Arc::new(StdMutex::new(Some(target_id))),
            url: url.clone(),
            started_at,
            pause_tx: Some(pause_tx),
            pause_reason: pause_reason.clone(),
            revoked_tx: revoked_tx.clone(),
        };
        self.targets.lock().unwrap().insert(id.clone(), entry);
        LiveGuard {
            id,
            url,
            pause_reason,
            targets: self.targets.clone(),
            tickets: self.tickets.clone(),
            embeds: self.embeds.clone(),
            revoked_tx,
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
        let (revoked_tx, _) = watch::channel(false);
        let entry = LiveEntry {
            id: id.clone(),
            kind: LiveKind::Cdp,
            conn,
            target_id,
            url: url.clone(),
            started_at,
            pause_tx: None,
            pause_reason: pause_reason.clone(),
            revoked_tx: revoked_tx.clone(),
        };
        self.targets.lock().unwrap().insert(id.clone(), entry);
        LiveGuard {
            id,
            url,
            pause_reason,
            targets: self.targets.clone(),
            tickets: self.tickets.clone(),
            embeds: self.embeds.clone(),
            revoked_tx,
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

    pub(crate) fn issue_embed(
        &self,
        session_id: &str,
        mode: EmbedMode,
        ttl_secs: u64,
    ) -> Result<(String, u64, u64), ApiError> {
        self.issue_embed_at(session_id, mode, ttl_secs, Instant::now(), now_ms())
    }

    pub(crate) fn issue_embed_at(
        &self,
        session_id: &str,
        mode: EmbedMode,
        ttl_secs: u64,
        now: Instant,
        now_epoch_ms: u64,
    ) -> Result<(String, u64, u64), ApiError> {
        if self.get(session_id).is_none() {
            return Err(ApiError::new(
                StatusCode::NOT_FOUND,
                "not_found",
                "live session not found",
            ));
        }
        let clamped_ttl = ttl_secs.clamp(MIN_EMBED_TTL_SECS, MAX_EMBED_TTL_SECS);
        let mut embeds = self.embeds.lock().unwrap();
        embeds.retain(|_, tok| tok.expires_at > now);
        if embeds.len() >= MAX_EMBED_TOKENS {
            return Err(ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                "capacity_exceeded",
                "too many active embed tokens",
            ));
        }
        let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        let digest = sha256_token(&token);
        let expires_at = now + Duration::from_secs(clamped_ttl);
        let expires_at_ms = now_epoch_ms.saturating_add(clamped_ttl.saturating_mul(1000));
        embeds.insert(
            digest,
            EmbedToken {
                session_id: session_id.to_string(),
                mode,
                expires_at,
            },
        );
        Ok((token, clamped_ttl, expires_at_ms))
    }

    pub(crate) fn verify_embed(
        &self,
        token: &str,
        expected_session: Option<&str>,
    ) -> Option<VerifiedEmbed> {
        self.verify_embed_at(token, expected_session, Instant::now())
    }

    pub(crate) fn verify_embed_at(
        &self,
        token: &str,
        expected_session: Option<&str>,
        now: Instant,
    ) -> Option<VerifiedEmbed> {
        if !is_well_formed_embed_token(token) {
            return None;
        }
        let digest = sha256_token(token);
        let verified = {
            let mut embeds = self.embeds.lock().unwrap();
            embeds.retain(|_, tok| tok.expires_at > now);
            let tok = embeds.get(&digest)?;
            if tok.expires_at <= now {
                return None;
            }
            if let Some(expected) = expected_session {
                if tok.session_id != expected {
                    return None;
                }
            }
            VerifiedEmbed {
                session_id: tok.session_id.clone(),
                mode: tok.mode,
                expires_at: tok.expires_at,
            }
        };
        if self.get(&verified.session_id).is_none() {
            return None;
        }
        Some(verified)
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

fn query_contains_key(query: &str, key: &str) -> bool {
    query.split('&').any(|pair| {
        pair.split_once('=')
            .is_some_and(|(k, _)| k == key)
    })
}

fn extract_live_ws_id(path: &str) -> Option<&str> {
    let id = path.strip_prefix("/v1/live/")?.strip_suffix("/ws")?;
    (!id.is_empty() && !id.contains('/')).then_some(id)
}

/// Auth middleware exclusively for `GET /v1/live/{id}/ws`.
/// Accepts `Authorization: Bearer <token>`, a single-use `?ticket=<val>`, or a scoped
/// `?embed=<token>` bound to `{id}`. Never accepts `?token=`.
pub(crate) async fn authorize_ws(
    State(inner): State<Arc<Inner>>,
    mut request: Request,
    next: Next,
) -> Response {
    let header_valid = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.as_bytes().split_at_checked(7))
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case(b"Bearer "))
        .is_some_and(|(_, supplied)| same_token(supplied, &inner.token));

    let access = if header_valid {
        Some(LiveAccess {
            read_only: false,
            expires_at: None,
        })
    } else if let Some(id) = extract_live_ws_id(request.uri().path()) {
        if let Some(q) = request.uri().query() {
            let has_ticket = query_contains_key(q, "ticket");
            let has_embed = query_contains_key(q, "embed");
            if has_ticket && has_embed {
                None
            } else if has_ticket {
                extract_single_query_param(q, "ticket")
                    .filter(|ticket| inner.live.consume_ticket(ticket, id))
                    .map(|_| LiveAccess {
                        read_only: false,
                        expires_at: None,
                    })
            } else if has_embed {
                extract_single_query_param(q, "embed")
                    .and_then(|token| inner.live.verify_embed(&token, Some(id)))
                    .map(|verified| LiveAccess {
                        read_only: verified.mode == EmbedMode::ReadOnly,
                        expires_at: Some(verified.expires_at),
                    })
            } else {
                None
            }
        } else {
            None
        }
    } else {
        None
    };

    let Some(access) = access else {
        let mut response = ApiError::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "a bearer token, single-use live ticket, or valid embed token is required",
        )
        .into_response();
        response
            .headers_mut()
            .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        return response;
    };

    request.extensions_mut().insert(access);
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
                "ticket" | "embed" => {}
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

pub(crate) async fn create_embed(
    State(inner): State<Arc<Inner>>,
    Path(id): Path<String>,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    let req: EmbedRequest = if body.iter().all(u8::is_ascii_whitespace) {
        EmbedRequest::default()
    } else {
        serde_json::from_slice(&body).map_err(|e| {
            let status = if e.is_syntax() || e.is_eof() {
                StatusCode::BAD_REQUEST
            } else {
                StatusCode::UNPROCESSABLE_ENTITY
            };
            ApiError::new(status, "invalid_request", e.to_string())
        })?
    };
    let (token, ttl, expires_at) = inner.live.issue_embed(&id, req.mode, req.ttl_secs)?;
    Ok(Json(json!({
        "url": format!("/embed/live/{token}"),
        "ws_url": format!("/v1/live/{id}/ws?embed={token}"),
        "mode": req.mode,
        "expires_at": expires_at,
        "expires_in_secs": ttl,
    })))
}

pub(crate) async fn embed_html(
    State(inner): State<Arc<Inner>>,
    Path(token): Path<String>,
) -> Result<Response, ApiError> {
    let Some(verified) = inner.live.verify_embed(&token, None) else {
        return Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "invalid or expired embed token",
        ));
    };
    if !is_safe_session_id(&verified.session_id) {
        return Err(ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "invalid_session",
            "live session id is not embeddable",
        ));
    }

    let html = render_embed_html(&verified.session_id, verified.mode);
    let csp = embed_csp_header(&inner.embed_frame_ancestors);
    let mut response = (StatusCode::OK, axum::response::Html(html)).into_response();
    if let Ok(val) = HeaderValue::from_str(&csp) {
        response
            .headers_mut()
            .insert(header::HeaderName::from_static("content-security-policy"), val);
    }
    response.headers_mut().insert(
        header::HeaderName::from_static("referrer-policy"),
        HeaderValue::from_static("no-referrer"),
    );
    Ok(response)
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
    Extension(access): Extension<LiveAccess>,
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
        .on_upgrade(move |socket| {
            run_live_stream(socket, inner, entry, query, access, viewer_guard)
        }))
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
            if let Some(buttons) = val.get("buttons").and_then(Value::as_u64) {
                params["buttons"] = json!(buttons & 31);
            }
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

fn status_payload(entry: &LiveEntry, read_only: bool) -> Value {
    let info = entry.info();
    json!({
        "type": "status",
        "id": info.id,
        "kind": info.kind,
        "url": info.url,
        "paused": info.paused,
        "pause_reason": info.pause_reason,
        "can_pause": info.can_pause && !read_only,
        "mode": if read_only { "read_only" } else { "interactive" },
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
    access: LiveAccess,
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

    if !send_json(&mut ws_tx, &status_payload(&entry, access.read_only)).await {
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

    enum InputOp {
        Mouse(Value),
        Key(Value),
        Text(String),
        Resume(u64),
    }

    let any_url = inner.allow_any_url();
    let mut shutdown_rx = inner.shutdown_tx.subscribe();
    let mut revoked_rx = entry.revoked_tx.subscribe();
    let mut pause_rx = entry.pause_tx.as_ref().map(|tx| tx.subscribe());
    let mut got_frame = false;
    let fallback_sleep = tokio::time::sleep(STATIC_FALLBACK_DELAY);
    tokio::pin!(fallback_sleep);
    let mut target_detached = false;

    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Value>();
    let (input_tx, mut input_rx) = mpsc::channel::<InputOp>(INPUT_QUEUE_CAPACITY);
    let pause_seq = Arc::new(AtomicU64::new(0));
    let ack_permits = Arc::new(Semaphore::new(MAX_INFLIGHT_ACKS));
    let nav_permits = Arc::new(Semaphore::new(MAX_INFLIGHT_NAVIGATES));
    let mut bg_tasks = JoinSet::new();

    // Run Input.* sequentially off the select! loop so slow/hung pages never stall
    // pause/resume/pings/shutdown while preserving click -> text -> submit -> resume order.
    let input_task = {
        let conn = entry.conn.clone();
        let obs_sid = obs_sid.clone();
        let entry = entry.clone();
        let out_tx = out_tx.clone();
        let pause_seq = pause_seq.clone();
        let read_only = access.read_only;
        tokio::spawn(async move {
            while let Some(op) = input_rx.recv().await {
                match op {
                    InputOp::Mouse(params) => {
                        let _ = conn
                            .call(
                                "Input.dispatchMouseEvent",
                                params,
                                Some(&obs_sid),
                                INPUT_CALL_TIMEOUT,
                            )
                            .await;
                    }
                    InputOp::Key(params) => {
                        let _ = conn
                            .call(
                                "Input.dispatchKeyEvent",
                                params,
                                Some(&obs_sid),
                                INPUT_CALL_TIMEOUT,
                            )
                            .await;
                    }
                    InputOp::Text(t) => {
                        let _ = conn
                            .call(
                                "Input.insertText",
                                json!({ "text": t }),
                                Some(&obs_sid),
                                INPUT_CALL_TIMEOUT,
                            )
                            .await;
                    }
                    InputOp::Resume(seq) => {
                        if pause_seq.load(Ordering::SeqCst) == seq {
                            if let Some(tx) = &entry.pause_tx {
                                apply_pause_state(tx, &entry.pause_reason, false, None);
                                let _ = out_tx.send(status_payload(&entry, read_only));
                            }
                        }
                    }
                }
            }
        })
    };

    loop {
        if *shutdown_rx.borrow() {
            let _ = send_json(
                &mut ws_tx,
                &json!({ "type": "ended", "reason": "server shutting down" }),
            )
            .await;
            break;
        }
        if *revoked_rx.borrow() {
            let _ = send_json(
                &mut ws_tx,
                &json!({ "type": "ended", "reason": "session closed" }),
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
            res = revoked_rx.changed() => {
                if res.is_err() || *revoked_rx.borrow() {
                    let _ = send_json(
                        &mut ws_tx,
                        &json!({ "type": "ended", "reason": "session closed" }),
                    ).await;
                    break;
                }
            }
            _ = async {
                match access.expires_at {
                    Some(deadline) => tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await,
                    None => std::future::pending().await,
                }
            } => {
                let _ = send_json(
                    &mut ws_tx,
                    &json!({ "type": "ended", "reason": "embed token expired" }),
                ).await;
                break;
            }
            Some(_) = bg_tasks.join_next(), if !bg_tasks.is_empty() => {}
            Some(msg) = out_rx.recv() => {
                if !send_json(&mut ws_tx, &msg).await {
                    break;
                }
            }
            Ok(()) = async {
                match pause_rx.as_mut() {
                    Some(rx) => rx.changed().await,
                    None => std::future::pending().await,
                }
            } => {
                if !send_json(&mut ws_tx, &status_payload(&entry, access.read_only)).await {
                    break;
                }
            }
            () = &mut fallback_sleep, if !got_frame => {
                got_frame = true;
                let conn = entry.conn.clone();
                let obs_sid = obs_sid.clone();
                let out_tx = out_tx.clone();
                let quality = query.quality;
                bg_tasks.spawn(async move {
                    if let Some(frame_msg) = capture_fallback_frame(&conn, &obs_sid, quality).await {
                        let _ = out_tx.send(frame_msg);
                    }
                });
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
                            if let Ok(permit) = ack_permits.clone().try_acquire_owned() {
                                let conn = entry.conn.clone();
                                let obs_sid = obs_sid.clone();
                                bg_tasks.spawn(async move {
                                    let _permit = permit;
                                    let _ = conn.call(
                                        "Page.screencastFrameAck",
                                        json!({ "sessionId": ack_id }),
                                        Some(&obs_sid),
                                        CLEANUP_TIMEOUT,
                                    ).await;
                                });
                            }
                        }
                    }
                    "Page.frameNavigated" if !ev.params["frame"]["parentId"].is_string() => {
                        if let Some(u) = ev.params["frame"]["url"].as_str() {
                            entry.set_url_if_nonempty(u);
                            if !send_json(&mut ws_tx, &status_payload(&entry, access.read_only)).await {
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
                        if access.read_only {
                            if !send_json(
                                &mut ws_tx,
                                &json!({
                                    "type": "error",
                                    "code": "forbidden",
                                    "status": 403,
                                    "message": "read-only embed session does not permit control commands",
                                }),
                            )
                            .await
                            {
                                break;
                            }
                            continue;
                        }
                        match parse_client_command(text.as_str(), any_url) {
                            Ok(ClientCommand::Mouse(params)) => {
                                let _ = input_tx.try_send(InputOp::Mouse(params));
                            }
                            Ok(ClientCommand::Key(params)) => {
                                let _ = input_tx.try_send(InputOp::Key(params));
                            }
                            Ok(ClientCommand::Text(t)) => {
                                let _ = input_tx.try_send(InputOp::Text(t));
                            }
                            Ok(ClientCommand::Navigate(url)) => {
                                if let Ok(permit) = nav_permits.clone().try_acquire_owned() {
                                    let conn = entry.conn.clone();
                                    let obs_sid = obs_sid.clone();
                                    let out_tx = out_tx.clone();
                                    bg_tasks.spawn(async move {
                                        let _permit = permit;
                                        if let Err(err) = conn.call("Page.navigate", json!({ "url": url }), Some(&obs_sid), CDP_CALL_TIMEOUT).await {
                                            let _ = out_tx.send(json!({ "type": "error", "message": err.to_string() }));
                                        }
                                    });
                                } else {
                                    let _ = send_json(&mut ws_tx, &json!({ "type": "error", "message": "too many concurrent navigations" })).await;
                                }
                            }
                            Ok(ClientCommand::Pause) => {
                                if let Some(tx) = &entry.pause_tx {
                                    pause_seq.fetch_add(1, Ordering::SeqCst);
                                    apply_pause_state(tx, &entry.pause_reason, true, None);
                                    if !send_json(&mut ws_tx, &status_payload(&entry, access.read_only)).await {
                                        break;
                                    }
                                } else {
                                    let _ = send_json(&mut ws_tx, &json!({ "type": "error", "message": "CDP gateway sessions cannot be paused" })).await;
                                }
                            }
                            Ok(ClientCommand::Resume) => {
                                if let Some(tx) = &entry.pause_tx {
                                    let seq = pause_seq.fetch_add(1, Ordering::SeqCst) + 1;
                                    if input_tx.try_send(InputOp::Resume(seq)).is_err() {
                                        apply_pause_state(tx, &entry.pause_reason, false, None);
                                        if !send_json(&mut ws_tx, &status_payload(&entry, access.read_only)).await {
                                            break;
                                        }
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

    input_task.abort();
    bg_tasks.abort_all();

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

fn sha256_b64(text: &str) -> String {
    let d = digest::digest(&digest::SHA256, text.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(d.as_ref())
}

fn embed_csp_header(frame_ancestors: &str) -> String {
    static HASHES: OnceLock<(String, String)> = OnceLock::new();
    let (style_hash, script_hash) =
        HASHES.get_or_init(|| (sha256_b64(EMBED_STYLE), sha256_b64(EMBED_SCRIPT)));
    format!(
        "default-src 'none'; script-src 'self' 'sha256-{script_hash}'; style-src 'sha256-{style_hash}'; img-src data:; connect-src 'self' ws: wss:; base-uri 'none'; form-action 'none'; frame-ancestors {frame_ancestors}"
    )
}

fn render_embed_html(session_id: &str, mode: EmbedMode) -> String {
    let mode_str = match mode {
        EmbedMode::ReadOnly => "read_only",
        EmbedMode::Interactive => "interactive",
    };
    let toolbar_controls = match mode {
        EmbedMode::ReadOnly => "<span class=\"pill pill-readonly\">Read-only</span>",
        EmbedMode::Interactive => {
            "<button type=\"button\" id=\"btn-live-pause\" class=\"btn-sm\" hidden>Pause</button>\
             <button type=\"button\" id=\"btn-live-esc\" class=\"btn-sm\">Send Esc</button>"
        }
    };
    let url_bar = match mode {
        EmbedMode::ReadOnly => "",
        EmbedMode::Interactive => {
            "<form id=\"live-url-form\" class=\"live-urlbar\">\
             <label for=\"live-url-input\" class=\"visually-hidden\">Target URL</label>\
             <input id=\"live-url-input\" type=\"url\" placeholder=\"https://example.com\">\
             <button type=\"submit\" class=\"btn-sm\">Go</button>\
             </form>"
        }
    };
    let banner_resume = match mode {
        EmbedMode::ReadOnly => "",
        EmbedMode::Interactive => {
            "<button type=\"button\" id=\"btn-live-banner-resume\" class=\"btn-sm\">Resume</button>"
        }
    };
    let stage_class = match mode {
        EmbedMode::ReadOnly => "live-stage readonly",
        EmbedMode::Interactive => "live-stage interactive",
    };
    format!(
        "<!DOCTYPE html>\n\
<html lang=\"en\">\n\
<head>\n\
  <meta charset=\"utf-8\">\n\
  <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
  <title>Fluxwright Live View</title>\n\
  <style>{EMBED_STYLE}</style>\n\
</head>\n\
<body data-session=\"{session_id}\" data-mode=\"{mode_str}\">\n\
  <div class=\"embed-shell\">\n\
    <div class=\"live-toolbar\">\n\
      <div class=\"live-meta-group\">\n\
        <span class=\"title\">Live View</span>\n\
        <span class=\"pill\"><span id=\"conn-dot\" class=\"status-dot\"></span><span id=\"conn-label\">Connecting...</span></span>\n\
        <span id=\"live-meta\" class=\"mono meta-text\"></span>\n\
        <span id=\"live-fps\" class=\"mono meta-text\">0 fps</span>\n\
      </div>\n\
      <div class=\"live-actions\">{toolbar_controls}</div>\n\
    </div>\n\
    {url_bar}\n\
    <div id=\"live-pause-banner\" class=\"live-pause-banner\" hidden>\n\
      <span id=\"live-pause-text\">Paused for human action</span>\n\
      {banner_resume}\n\
    </div>\n\
    <div id=\"{stage_id}\" class=\"{stage_class}\" tabindex=\"0\" aria-label=\"Live browser viewport\">\n\
      <img id=\"live-frame\" alt=\"Live browser viewport\" draggable=\"false\" hidden>\n\
      <div id=\"live-empty\" class=\"live-empty\">Connecting live stream...</div>\n\
    </div>\n\
  </div>\n\
  <script>{EMBED_SCRIPT}</script>\n\
</body>\n\
</html>\n",
        stage_id = "live-stage",
    )
}

const EMBED_STYLE: &str = r##"
:root {
  color-scheme: light dark;
  --bg: #0f172a;
  --card-bg: #1e293b;
  --border: #334155;
  --text: #f8fafc;
  --text-muted: #94a3b8;
  --primary: #38bdf8;
  --success: #4ade80;
  --warning: #fbbf24;
  --danger: #f87171;
  --font: system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif;
  --mono: ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace;
}
*, *::before, *::after { box-sizing: border-box; margin: 0; padding: 0; }
html, body { height: 100%; font-family: var(--font); background: var(--bg); color: var(--text); }
.embed-shell { display: flex; flex-direction: column; height: 100vh; }
.live-toolbar {
  display: flex; flex-wrap: wrap; justify-content: space-between; align-items: center;
  gap: 0.5rem; padding: 0.5rem 0.75rem; background: var(--card-bg); border-bottom: 1px solid var(--border);
}
.live-meta-group { display: flex; align-items: center; gap: 0.5rem; flex-wrap: wrap; }
.live-actions { display: flex; align-items: center; gap: 0.5rem; }
.title { font-size: 0.9375rem; font-weight: 600; }
.mono { font-family: var(--mono); font-size: 0.75rem; }
.meta-text { color: var(--text-muted); }
.pill {
  display: inline-flex; align-items: center; gap: 0.375rem; font-size: 0.75rem;
  padding: 0.15rem 0.5rem; border-radius: 9999px; background: var(--bg); border: 1px solid var(--border);
}
.pill-readonly { color: var(--text-muted); }
.status-dot { width: 0.5rem; height: 0.5rem; border-radius: 50%; background: var(--warning); }
.status-dot.connected { background: var(--success); }
.status-dot.disconnected { background: var(--danger); }
button, input {
  font-family: inherit; font-size: 0.8125rem; padding: 0.35rem 0.65rem;
  border-radius: 6px; border: 1px solid var(--border); background: var(--card-bg); color: var(--text);
}
button { cursor: pointer; font-weight: 500; }
button:hover { border-color: var(--primary); }
.btn-sm { padding: 0.25rem 0.625rem; font-size: 0.75rem; }
.live-urlbar {
  display: flex; gap: 0.5rem; padding: 0.4rem 0.75rem;
  border-bottom: 1px solid var(--border); background: var(--bg);
}
.live-urlbar input { flex: 1; }
.live-pause-banner {
  display: flex; justify-content: space-between; align-items: center; gap: 0.5rem;
  padding: 0.4rem 0.75rem; background: #f59e0b22; border-bottom: 1px solid #f59e0b55;
  color: var(--warning); font-size: 0.8125rem; font-weight: 500;
}
.live-stage {
  flex: 1; overflow: auto; display: flex; justify-content: center; align-items: flex-start;
  padding: 0.5rem; background: #020617; outline: none;
}
#live-frame {
  max-width: 100%; height: auto; display: block; background: #000;
  user-select: none; -webkit-user-drag: none; border-radius: 4px;
}
.live-stage.interactive #live-frame { cursor: crosshair; }
.live-empty { color: var(--text-muted); padding: 2rem; font-size: 0.875rem; }
.visually-hidden {
  position: absolute; width: 1px; height: 1px; padding: 0; margin: -1px;
  overflow: hidden; clip: rect(0, 0, 0, 0); white-space: nowrap; border: 0;
}
"##;

const EMBED_SCRIPT: &str = r##"(function () {
  const body = document.body;
  const sessionId = (body && body.dataset && body.dataset.session) || '';
  const mode = (body && body.dataset && body.dataset.mode) === 'interactive' ? 'interactive' : 'read_only';
  const parts = location.pathname.split('/').filter(Boolean);
  const embedToken = parts.length ? parts[parts.length - 1] : '';
  const connDot = document.getElementById('conn-dot');
  const connLabel = document.getElementById('conn-label');
  const liveMeta = document.getElementById('live-meta');
  const liveFps = document.getElementById('live-fps');
  const livePauseBanner = document.getElementById('live-pause-banner');
  const livePauseText = document.getElementById('live-pause-text');
  const liveStage = document.getElementById('live-stage');
  const liveFrame = document.getElementById('live-frame');
  const liveEmpty = document.getElementById('live-empty');
  const livePauseBtn = document.getElementById('btn-live-pause');
  const liveEscBtn = document.getElementById('btn-live-esc');
  const liveBannerResumeBtn = document.getElementById('btn-live-banner-resume');
  const liveUrlForm = document.getElementById('live-url-form');
  const liveUrlInput = document.getElementById('live-url-input');

  let liveWs = null;
  let liveSessionState = null;
  let mouseDragging = false;
  let frameWidth = 1280;
  let frameHeight = 800;
  let fpsWindow = [];
  let lastMoveAt = 0;

  function setStatus(state, text) {
    if (connDot) connDot.className = 'status-dot ' + state;
    if (connLabel) connLabel.textContent = text;
  }

  function sendLive(payload) {
    if (mode !== 'interactive') return;
    if (liveWs && liveWs.readyState === WebSocket.OPEN) {
      liveWs.send(JSON.stringify(payload));
    }
  }

  function applyStatus(msg) {
    liveSessionState = msg;
    if (liveMeta) liveMeta.textContent = frameWidth + 'x' + frameHeight;
    if (liveUrlInput && msg.url && document.activeElement !== liveUrlInput) {
      liveUrlInput.value = String(msg.url);
    }
    if (livePauseBtn) {
      livePauseBtn.hidden = mode !== 'interactive' || !msg.can_pause;
      livePauseBtn.textContent = msg.paused ? 'Resume' : 'Pause';
    }
    if (livePauseBanner) livePauseBanner.hidden = !msg.paused;
    if (livePauseText) livePauseText.textContent = msg.pause_reason || 'Paused for human action';
  }

  function connect() {
    if (!sessionId || !embedToken) {
      setStatus('disconnected', 'Invalid embed link');
      if (liveEmpty) liveEmpty.textContent = 'Invalid embed session.';
      return;
    }
    setStatus('connecting', 'Connecting...');
    const proto = location.protocol === 'https:' ? 'wss://' : 'ws://';
    const wsUrl = proto + location.host + '/v1/live/' + encodeURIComponent(sessionId) + '/ws?embed=' + encodeURIComponent(embedToken);
    const ws = new WebSocket(wsUrl);
    liveWs = ws;
    ws.addEventListener('open', function () {
      setStatus('connected', mode === 'interactive' ? 'Live (Interactive)' : 'Live (Read-only)');
    });
    ws.addEventListener('message', function (ev) {
      if (liveWs !== ws) return;
      let msg;
      try {
        msg = JSON.parse(ev.data);
      } catch (_) {
        return;
      }
      if (msg.type === 'frame' && typeof msg.data === 'string' && /^[A-Za-z0-9+/=]+$/.test(msg.data)) {
        frameWidth = Number(msg.width) || frameWidth;
        frameHeight = Number(msg.height) || frameHeight;
        if (liveMeta) liveMeta.textContent = frameWidth + 'x' + frameHeight;
        liveFrame.src = 'data:image/jpeg;base64,' + msg.data;
        liveFrame.hidden = false;
        if (liveEmpty) liveEmpty.hidden = true;
        const now = performance.now();
        fpsWindow.push(now);
        while (fpsWindow.length && fpsWindow[0] < now - 1000) fpsWindow.shift();
        if (liveFps) liveFps.textContent = fpsWindow.length + ' fps';
      } else if (msg.type === 'status') {
        applyStatus(msg);
      } else if (msg.type === 'ended') {
        setStatus('disconnected', 'Ended');
        if (liveEmpty) {
          liveEmpty.textContent = 'Session ended: ' + (msg.reason || 'closed');
          liveEmpty.hidden = false;
        }
      } else if (msg.type === 'error') {
        if (liveEmpty) {
          liveEmpty.textContent = msg.message || 'Live stream error';
          liveEmpty.hidden = false;
        }
      }
    });
    ws.addEventListener('close', function () {
      if (liveWs === ws) {
        liveWs = null;
        mouseDragging = false;
        setStatus('disconnected', 'Disconnected');
        if (liveFrame.hidden && liveEmpty) {
          liveEmpty.hidden = false;
        }
      }
    });
  }

  if (mode === 'interactive') {
    function mapCoords(e) {
      const rect = liveFrame.getBoundingClientRect();
      if (!rect.width || !rect.height) return { x: 0, y: 0 };
      const rawX = ((e.clientX - rect.left) / rect.width) * frameWidth;
      const rawY = ((e.clientY - rect.top) / rect.height) * frameHeight;
      return {
        x: Math.round(Math.min(frameWidth, Math.max(0, rawX)) * 100) / 100,
        y: Math.round(Math.min(frameHeight, Math.max(0, rawY)) * 100) / 100,
      };
    }
    function mouseButton(b) {
      if (b === 0) return 'left';
      if (b === 1) return 'middle';
      if (b === 2) return 'right';
      return 'none';
    }
    function mouseButtonFromMask(mask) {
      if (mask & 1) return 'left';
      if (mask & 2) return 'right';
      if (mask & 4) return 'middle';
      return 'none';
    }
    function modBits(e) {
      return (e.altKey ? 1 : 0) | (e.ctrlKey ? 2 : 0) | (e.metaKey ? 4 : 0) | (e.shiftKey ? 8 : 0);
    }
    liveFrame.addEventListener('contextmenu', function (e) { e.preventDefault(); });
    liveFrame.addEventListener('mousedown', function (e) {
      e.preventDefault();
      mouseDragging = true;
      if (liveStage) liveStage.focus();
      const pt = mapCoords(e);
      sendLive({
        type: 'mouse',
        event: 'mousePressed',
        x: pt.x,
        y: pt.y,
        button: mouseButton(e.button),
        buttons: e.buttons || 0,
        clickCount: e.detail || 1,
        modifiers: modBits(e),
      });
    });
    window.addEventListener('mouseup', function (e) {
      if (!mouseDragging && e.target !== liveFrame) return;
      mouseDragging = false;
      if (e.target === liveFrame) e.preventDefault();
      const pt = mapCoords(e);
      sendLive({
        type: 'mouse',
        event: 'mouseReleased',
        x: pt.x,
        y: pt.y,
        button: mouseButton(e.button),
        buttons: e.buttons || 0,
        clickCount: e.detail || 1,
        modifiers: modBits(e),
      });
    });
    window.addEventListener('mousemove', function (e) {
      if (!mouseDragging && e.target !== liveFrame) return;
      const now = performance.now();
      if (now - lastMoveAt < 33) return;
      lastMoveAt = now;
      const pt = mapCoords(e);
      const mask = e.buttons || 0;
      sendLive({
        type: 'mouse',
        event: 'mouseMoved',
        x: pt.x,
        y: pt.y,
        button: mouseButtonFromMask(mask),
        buttons: mask,
        modifiers: modBits(e),
      });
    });
    liveFrame.addEventListener('wheel', function (e) {
      e.preventDefault();
      const pt = mapCoords(e);
      sendLive({
        type: 'mouse',
        event: 'mouseWheel',
        x: pt.x,
        y: pt.y,
        button: 'none',
        modifiers: modBits(e),
        deltaX: e.deltaX,
        deltaY: e.deltaY,
      });
    }, { passive: false });
    function buildKeyPayload(e, kind) {
      const text = (kind === 'keyDown' && e.key === 'Enter')
        ? '\r'
        : (kind === 'keyDown' && e.key.length === 1 && !e.ctrlKey && !e.metaKey && !e.altKey ? e.key : '');
      return {
        type: 'key',
        event: kind,
        key: e.key,
        code: e.code,
        text: text,
        modifiers: modBits(e),
        windowsVirtualKeyCode: e.keyCode || 0,
      };
    }
    if (liveStage) {
      liveStage.addEventListener('keydown', function (e) {
        if (e.isComposing) return;
        e.preventDefault();
        sendLive(buildKeyPayload(e, 'keyDown'));
      });
      liveStage.addEventListener('keyup', function (e) {
        if (e.isComposing) return;
        e.preventDefault();
        sendLive(buildKeyPayload(e, 'keyUp'));
      });
    }
    if (liveUrlForm) {
      liveUrlForm.addEventListener('submit', function (e) {
        e.preventDefault();
        const nextUrl = liveUrlInput ? liveUrlInput.value.trim() : '';
        if (nextUrl) {
          sendLive({ type: 'navigate', url: nextUrl });
          if (liveStage) liveStage.focus();
        }
      });
    }
    if (livePauseBtn) {
      livePauseBtn.addEventListener('click', function () {
        if (!liveSessionState) return;
        sendLive({ type: liveSessionState.paused ? 'resume' : 'pause' });
      });
    }
    if (liveBannerResumeBtn) {
      liveBannerResumeBtn.addEventListener('click', function () {
        sendLive({ type: 'resume' });
      });
    }
    if (liveEscBtn) {
      liveEscBtn.addEventListener('click', function () {
        sendLive({ type: 'key', event: 'keyDown', key: 'Escape', code: 'Escape', windowsVirtualKeyCode: 27 });
        sendLive({ type: 'key', event: 'keyUp', key: 'Escape', code: 'Escape', windowsVirtualKeyCode: 27 });
        if (liveStage) liveStage.focus();
      });
    }
  }

  connect();
})();"##;

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
            parse_client_command(r#"{"type":"mouse","event":"mousePressed","x":12,"y":34,"button":"left","buttons":1}"#, false),
            Ok(ClientCommand::Mouse(ref p)) if p["buttons"] == json!(1)
        ));
        assert!(matches!(
            parse_client_command(r#"{"type":"mouse","event":"mouseMoved","x":50,"y":60,"button":"left","buttons":1}"#, false),
            Ok(ClientCommand::Mouse(ref p)) if p["type"] == "mouseMoved" && p["buttons"] == json!(1)
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
