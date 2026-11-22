//! Pooled CDP WebSocket gateway (`GET /cdp` and `GET /json/version`).
//!
//! Each WebSocket client receives its own isolated `ContextLease` from the engine pool. Browser-level
//! CDP commands and events are strictly scoped to the leased context (and up to 4 client-created child
//! contexts), so concurrent callers share pooled Chrome processes safely without touching each
//! other's targets, cookies, storage, or browser lifecycle.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{RawQuery, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use fluxwright::{Connection, ContextLease, JobOptions, Priority, ResourcePreset};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinSet;

use crate::{ApiError, Inner};

const CDP_CMD_TIMEOUT: Duration = Duration::from_secs(60);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_WS_PAYLOAD_BYTES: usize = 128 << 20;
const MAX_EXTRA_CONTEXTS: usize = 4;

const SAFE_CREATE_CONTEXT_FIELDS: &[&str] = &[
    "disposeOnDetach",
    "proxyServer",
    "proxyBypassList",
    "originsWithUniversalNetworkAccess",
];

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct CdpQuery {
    priority: Priority,
    resource_preset: Option<ResourcePreset>,
    timeout_ms: Option<u64>,
    initial_page: bool,
}

impl CdpQuery {
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
                "token" => {}
                "priority" => {
                    out.priority = match val.to_ascii_lowercase().as_str() {
                        "low" => Priority::Low,
                        "normal" => Priority::Normal,
                        "high" => Priority::High,
                        _ => {
                            return Err(ApiError::new(
                                StatusCode::BAD_REQUEST,
                                "invalid_query",
                                "priority must be low, normal, or high",
                            ));
                        }
                    };
                }
                "resource_preset" => {
                    out.resource_preset = Some(ResourcePreset::parse(&val).ok_or_else(|| {
                        ApiError::new(
                            StatusCode::BAD_REQUEST,
                            "invalid_query",
                            "resource_preset must be scraping, minimal, or no-media",
                        )
                    })?);
                }
                "timeout_ms" => {
                    let ms = val.parse::<u64>().map_err(|_| {
                        ApiError::new(
                            StatusCode::BAD_REQUEST,
                            "invalid_query",
                            "timeout_ms must be an integer between 1 and 300000",
                        )
                    })?;
                    if !(1..=300_000).contains(&ms) {
                        return Err(ApiError::new(
                            StatusCode::BAD_REQUEST,
                            "invalid_query",
                            "timeout_ms must be between 1 and 300000",
                        ));
                    }
                    out.timeout_ms = Some(ms);
                }
                "initial_page" => {
                    out.initial_page = match val.as_str() {
                        "true" => true,
                        "false" => false,
                        _ => {
                            return Err(ApiError::new(
                                StatusCode::BAD_REQUEST,
                                "invalid_query",
                                "initial_page must be true or false",
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

/// Extracts and percent-decodes a single `token=` parameter from a raw query string.
/// Returns `None` if missing, malformed, or duplicated (to prevent parameter pollution).
pub(crate) fn extract_query_token(query: &str) -> Option<Vec<u8>> {
    let mut found: Option<Vec<u8>> = None;
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = pair.split_once('=')?;
        if k == "token" {
            if found.is_some() {
                return None;
            }
            found = percent_decode_bytes(v.as_bytes());
        }
    }
    found
}

fn percent_decode_bytes(input: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        if input[i] == b'%' {
            let hi = hex_val(*input.get(i + 1)?)?;
            let lo = hex_val(*input.get(i + 2)?)?;
            out.push((hi << 4) | lo);
            i += 3;
        } else {
            out.push(input[i]);
            i += 1;
        }
    }
    Some(out)
}

pub(crate) fn percent_decode_str(input: &str) -> Option<String> {
    let bytes = percent_decode_bytes(input.as_bytes())?;
    String::from_utf8(bytes).ok()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Standard DevTools `/json/version` discovery endpoint pointing to `/cdp`.
pub(crate) async fn json_version(headers: HeaderMap) -> Response {
    let scheme = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|s| s.eq_ignore_ascii_case("https"))
        .then_some("wss")
        .unwrap_or("ws");

    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|h| {
            !h.is_empty()
                && h.bytes()
                    .all(|b| b.is_ascii_graphic() && !matches!(b, b'/' | b'\\' | b'@'))
        })
        .unwrap_or("127.0.0.1:7331");

    Json(json!({
        "Browser": "Fluxwright/Chrome",
        "Protocol-Version": "1.3",
        "webSocketDebuggerUrl": format!("{scheme}://{host}/cdp"),
    }))
    .into_response()
}

/// Upgrades a client WebSocket onto a pooled, context-isolated CDP lease.
pub(crate) async fn ws_handler(
    State(inner): State<Arc<Inner>>,
    RawQuery(raw_query): RawQuery,
    ws: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    let query = CdpQuery::parse(raw_query.as_deref())?;
    let Some(engine) = inner.engine.clone() else {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            "CDP gateway is unavailable in coordinator mode",
        ));
    };
    if !inner.with_registry(|r, _| r.accepting) {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "shutting_down",
            "the server is shutting down",
        ));
    }

    let mut opts = JobOptions::default().priority(query.priority);
    if let Some(preset) = query.resource_preset {
        opts = opts.resource_preset(preset);
    }
    if let Some(timeout_ms) = query.timeout_ms {
        opts = opts.timeout(Duration::from_millis(timeout_ms));
    }

    let lease = engine.acquire_context_with(&opts).await.map_err(|e| match e {
        fluxwright::Error::ShuttingDown => ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "shutting_down",
            e.to_string(),
        ),
        fluxwright::Error::QueueFull => ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "capacity_exceeded",
            e.to_string(),
        ),
        fluxwright::Error::AcquireTimeout => ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "acquire_timeout",
            e.to_string(),
        ),
        other => ApiError::new(StatusCode::BAD_GATEWAY, "browser_error", other.to_string()),
    })?;

    lease.discard_warm_page().await;
    if query.resource_preset.is_none() {
        lease.mark_external_context(lease.context_id());
    }

    Ok(ws
        .max_frame_size(MAX_WS_PAYLOAD_BYTES)
        .max_message_size(MAX_WS_PAYLOAD_BYTES)
        .on_upgrade(move |socket| run_gateway(socket, inner, lease, query.initial_page)))
}

struct ActiveCdpSessionGuard {
    inner: Arc<Inner>,
}

impl ActiveCdpSessionGuard {
    fn enter(inner: Arc<Inner>) -> Self {
        inner.active_cdp_sessions.fetch_add(1, Ordering::Relaxed);
        inner.total_cdp_sessions.fetch_add(1, Ordering::Relaxed);
        Self { inner }
    }
}

impl Drop for ActiveCdpSessionGuard {
    fn drop(&mut self) {
        self.inner
            .active_cdp_sessions
            .fetch_sub(1, Ordering::Relaxed);
    }
}

struct GatewayState {
    primary_context: String,
    allowed_contexts: HashSet<String>,
    extra_contexts: Vec<String>,
    owned_targets: HashSet<String>,
    owned_sessions: HashMap<String, String>,
    auto_attached_targets: HashSet<String>,
    discover_targets: bool,
    auto_attach: bool,
    live_target: Arc<std::sync::Mutex<Option<String>>>,
    live_url: Arc<std::sync::Mutex<String>>,
}

impl GatewayState {
    fn new(
        primary_context: String,
        live_target: Arc<std::sync::Mutex<Option<String>>>,
        live_url: Arc<std::sync::Mutex<String>>,
    ) -> Self {
        let mut allowed_contexts = HashSet::new();
        allowed_contexts.insert(primary_context.clone());
        Self {
            primary_context,
            allowed_contexts,
            extra_contexts: Vec::new(),
            owned_targets: HashSet::new(),
            owned_sessions: HashMap::new(),
            auto_attached_targets: HashSet::new(),
            discover_targets: false,
            auto_attach: false,
            live_target,
            live_url,
        }
    }
}

async fn run_gateway(
    socket: WebSocket,
    inner: Arc<Inner>,
    lease: ContextLease,
    initial_page: bool,
) {
    let _active_guard = ActiveCdpSessionGuard::enter(inner.clone());
    let conn = lease.cdp_connection();
    let lease = Arc::new(lease);
    let primary_context = lease.context_id().to_string();
    let live_target = Arc::new(std::sync::Mutex::new(None));
    let live_url = Arc::new(std::sync::Mutex::new(String::from("about:blank")));
    let _live_guard = inner.live.register_cdp(
        format!("cdp-{}", uuid::Uuid::new_v4().simple()),
        conn.clone(),
        live_target.clone(),
        live_url.clone(),
        crate::now_ms(),
    );
    let state = Arc::new(Mutex::new(GatewayState::new(
        primary_context.clone(),
        live_target,
        live_url,
    )));

    let (mut ws_tx, mut ws_rx) = socket.split();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Message>();

    let writer_task = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            if ws_tx.send(msg).await.is_err() {
                break;
            }
        }
    });

    // Subscribe to Chrome events before spawning any targets or reading client commands.
    let mut cdp_events = conn.subscribe_unbounded();
    if initial_page {
        if let Ok(Ok(res)) = conn
            .call_raw(
                "Target.createTarget",
                json!({ "url": "about:blank", "browserContextId": primary_context }),
                None,
                CDP_CMD_TIMEOUT,
            )
            .await
        {
            if let Some(tid) = res["targetId"].as_str() {
                let mut g = state.lock().await;
                g.owned_targets.insert(tid.to_string());
                *g.live_target.lock().unwrap() = Some(tid.to_string());
            }
        }
    }

    let event_task = {
        let state = state.clone();
        let conn = conn.clone();
        let lease = lease.clone();
        let out_tx = out_tx.clone();
        tokio::spawn(async move {
            while let Some(ev) = cdp_events.recv().await {
                if ev.method == "Fluxwright.disconnected" {
                    let _ = out_tx.send(Message::Close(None));
                    break;
                }
                if let Some(sid) = ev.session_id.as_deref() {
                    let forward = {
                        let mut g = state.lock().await;
                        if !g.owned_sessions.contains_key(sid) {
                            false
                        } else {
                            if ev.method == "Target.attachedToTarget" {
                                if let Some(child_sid) = ev.params["sessionId"].as_str() {
                                    let child_tid = ev.params["targetInfo"]["targetId"]
                                        .as_str()
                                        .unwrap_or_default()
                                        .to_string();
                                    g.owned_sessions
                                        .insert(child_sid.to_string(), child_tid.clone());
                                    if !child_tid.is_empty() {
                                        g.owned_targets.insert(child_tid);
                                    }
                                    lease.mark_external_session(child_sid);
                                }
                            } else if ev.method == "Target.detachedFromTarget" {
                                if let Some(child_sid) = ev.params["sessionId"].as_str() {
                                    g.owned_sessions.remove(child_sid);
                                    lease.unmark_external_session(child_sid);
                                }
                            }
                            true
                        }
                    };
                    if forward {
                        send_event(&out_tx, &ev.method, &ev.params, Some(sid));
                    }
                    continue;
                }

                match ev.method.as_str() {
                    "Target.targetCreated" | "Target.targetInfoChanged" => {
                        let info = &ev.params["targetInfo"];
                        let Some(tid) = info["targetId"].as_str() else {
                            continue;
                        };
                        let ctx_id = info["browserContextId"].as_str();
                        let (owned, discover, should_attach) = {
                            let mut g = state.lock().await;
                            let is_ours = ctx_id
                                .is_some_and(|c| g.allowed_contexts.contains(c))
                                || g.owned_targets.contains(tid);
                            if !is_ours {
                                (false, false, false)
                            } else {
                                g.owned_targets.insert(tid.to_string());
                                let ty = info["type"].as_str().unwrap_or("");
                                if ty == "page" {
                                    let mut slot = g.live_target.lock().unwrap();
                                    if slot.is_none() || ev.method == "Target.targetCreated" {
                                        *slot = Some(tid.to_string());
                                    }
                                    if slot.as_deref() == Some(tid) {
                                        if let Some(u) = info["url"].as_str().filter(|s| !s.is_empty()) {
                                            *g.live_url.lock().unwrap() = u.to_string();
                                        }
                                    }
                                }
                                let attach = ev.method == "Target.targetCreated"
                                    && g.auto_attach
                                    && matches!(ty, "page" | "iframe" | "worker")
                                    && g.auto_attached_targets.insert(tid.to_string());
                                (true, g.discover_targets, attach)
                            }
                        };
                        if !owned {
                            continue;
                        }
                        if discover {
                            send_event(&out_tx, &ev.method, &ev.params, None);
                        }
                        if should_attach {
                            let conn = conn.clone();
                            let state = state.clone();
                            let lease = lease.clone();
                            let out_tx = out_tx.clone();
                            let tid = tid.to_string();
                            let info = info.clone();
                            tokio::spawn(async move {
                                let _ = attach_target(
                                    &conn,
                                    &state,
                                    &lease,
                                    &out_tx,
                                    &tid,
                                    Some(info),
                                )
                                .await;
                            });
                        }
                    }
                    "Target.targetDestroyed" => {
                        let Some(tid) = ev.params["targetId"].as_str() else {
                            continue;
                        };
                        let (was_owned, discover) = {
                            let mut g = state.lock().await;
                            let was_owned = g.owned_targets.remove(tid);
                            g.auto_attached_targets.remove(tid);
                            let mut slot = g.live_target.lock().unwrap();
                            if slot.as_deref() == Some(tid) {
                                *slot = None;
                                g.live_url.lock().unwrap().clear();
                            }
                            (was_owned, g.discover_targets)
                        };
                        if was_owned && discover {
                            send_event(&out_tx, &ev.method, &ev.params, None);
                        }
                    }
                    "Target.detachedFromTarget" => {
                        let Some(sid) = ev.params["sessionId"].as_str() else {
                            continue;
                        };
                        let was_owned = state.lock().await.owned_sessions.remove(sid).is_some();
                        if was_owned {
                            lease.unmark_external_session(sid);
                            send_event(&out_tx, &ev.method, &ev.params, None);
                        }
                    }
                    _ => {}
                }
            }
        })
    };

    let mut shutdown_rx = inner.shutdown_tx.subscribe();
    let mut tasks = JoinSet::new();

    loop {
        if *shutdown_rx.borrow() {
            let _ = out_tx.send(Message::Close(None));
            break;
        }
        tokio::select! {
            res = shutdown_rx.changed() => {
                if res.is_err() || *shutdown_rx.borrow() {
                    let _ = out_tx.send(Message::Close(None));
                    break;
                }
            }
            Some(_) = tasks.join_next(), if !tasks.is_empty() => {}
            msg = ws_rx.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        if handle_client_text(
                            text.as_str(),
                            &conn,
                            &state,
                            &lease,
                            &out_tx,
                            &mut tasks,
                        ).await {
                            break;
                        }
                    }
                    Some(Ok(Message::Ping(payload))) => {
                        let _ = out_tx.send(Message::Pong(payload));
                    }
                    Some(Ok(Message::Pong(_))) => {}
                    Some(Ok(Message::Binary(_))) => {
                        send_error(&out_tx, None, None, -32600, "binary CDP frames are not supported");
                    }
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                }
            }
        }
    }

    event_task.abort();
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}

    let (sessions_to_detach, contexts_to_dispose) = {
        let mut g = state.lock().await;
        let sessions: Vec<String> = g.owned_sessions.drain().map(|(s, _)| s).collect();
        let contexts: Vec<String> = std::mem::take(&mut g.extra_contexts);
        (sessions, contexts)
    };

    for sid in sessions_to_detach {
        lease.unmark_external_session(&sid);
        let _ = conn
            .call_raw(
                "Target.detachFromTarget",
                json!({ "sessionId": sid }),
                None,
                CLEANUP_TIMEOUT,
            )
            .await;
    }
    for ctx_id in contexts_to_dispose {
        let _ = conn
            .call_raw(
                "Target.disposeBrowserContext",
                json!({ "browserContextId": ctx_id }),
                None,
                CLEANUP_TIMEOUT,
            )
            .await;
        lease.unmark_external_context(&ctx_id);
    }
    lease.unmark_external_context(&primary_context);
    let _ = lease.close().await;

    drop(out_tx);
    let _ = tokio::time::timeout(Duration::from_millis(500), writer_task).await;
}

/// Handles one incoming client JSON-RPC frame. Returns `true` when the connection should close.
async fn handle_client_text(
    text: &str,
    conn: &Arc<Connection>,
    state: &Arc<Mutex<GatewayState>>,
    lease: &Arc<ContextLease>,
    out_tx: &mpsc::UnboundedSender<Message>,
    tasks: &mut JoinSet<()>,
) -> bool {
    let Ok(val) = serde_json::from_str::<Value>(text) else {
        send_error(out_tx, None, None, -32700, "invalid JSON");
        return false;
    };
    let id = val.get("id").filter(|v| !v.is_null()).cloned();
    let Some(method) = val.get("method").and_then(Value::as_str).map(str::to_owned) else {
        send_error(out_tx, id.as_ref(), None, -32600, "missing CDP method");
        return false;
    };
    let params = val.get("params").cloned().unwrap_or_else(|| json!({}));
    let session_id = val
        .get("sessionId")
        .and_then(Value::as_str)
        .map(str::to_owned);

    if let Some(sid) = session_id {
        if !state.lock().await.owned_sessions.contains_key(&sid) {
            send_error(
                out_tx,
                id.as_ref(),
                Some(&sid),
                -32000,
                "session not owned by this CDP lease",
            );
            return false;
        }
        let conn = conn.clone();
        let out_tx = out_tx.clone();
        tasks.spawn(async move {
            let res = conn
                .call_raw(&method, params, Some(&sid), CDP_CMD_TIMEOUT)
                .await;
            relay_call_result(&out_tx, id.as_ref(), Some(&sid), res);
        });
        return false;
    }

    if method == "Browser.close" {
        send_result(out_tx, id.as_ref(), None, json!({}));
        let _ = out_tx.send(Message::Close(None));
        return true;
    }

    let conn = conn.clone();
    let state = state.clone();
    let lease = lease.clone();
    let out_tx = out_tx.clone();
    tasks.spawn(async move {
        handle_browser_command(&method, params, id.as_ref(), &conn, &state, &lease, &out_tx).await;
    });
    false
}

async fn handle_browser_command(
    method: &str,
    mut params: Value,
    id: Option<&Value>,
    conn: &Arc<Connection>,
    state: &Arc<Mutex<GatewayState>>,
    lease: &Arc<ContextLease>,
    out_tx: &mpsc::UnboundedSender<Message>,
) {
    match method {
        "Browser.getVersion" => {
            let res = conn.call_raw(method, params, None, CDP_CMD_TIMEOUT).await;
            relay_call_result(out_tx, id, None, res);
        }
        "Target.getBrowserContexts" => {
            let ids: Vec<String> = state
                .lock()
                .await
                .allowed_contexts
                .iter()
                .cloned()
                .collect();
            send_result(out_tx, id, None, json!({ "browserContextIds": ids }));
        }
        "Target.createBrowserContext" => {
            let allowed = {
                let g = state.lock().await;
                g.extra_contexts.len() < MAX_EXTRA_CONTEXTS
            };
            if !allowed {
                send_error(
                    out_tx,
                    id,
                    None,
                    -32000,
                    "browser context limit reached for this CDP lease",
                );
                return;
            }
            let filtered = filter_create_context_params(&params);
            match conn
                .call_raw("Target.createBrowserContext", filtered, None, CDP_CMD_TIMEOUT)
                .await
            {
                Ok(Ok(result)) => {
                    if let Some(ctx_id) = result["browserContextId"].as_str() {
                        lease.mark_external_context(ctx_id);
                        let mut g = state.lock().await;
                        g.allowed_contexts.insert(ctx_id.to_string());
                        g.extra_contexts.push(ctx_id.to_string());
                    }
                    send_result(out_tx, id, None, result);
                }
                other => relay_call_result(out_tx, id, None, other),
            }
        }
        "Target.disposeBrowserContext" => {
            let Some(ctx_id) = params["browserContextId"].as_str().map(str::to_owned) else {
                send_error(out_tx, id, None, -32602, "missing browserContextId");
                return;
            };
            let is_primary = {
                let g = state.lock().await;
                if !g.allowed_contexts.contains(&ctx_id) {
                    send_error(
                        out_tx,
                        id,
                        None,
                        -32000,
                        "browserContextId not owned by this CDP lease",
                    );
                    return;
                }
                ctx_id == g.primary_context
            };
            if is_primary {
                // Primary context is owned by the lease and disposed when the WebSocket closes.
                send_result(out_tx, id, None, json!({}));
                return;
            }
            let res = conn
                .call_raw(
                    "Target.disposeBrowserContext",
                    json!({ "browserContextId": ctx_id }),
                    None,
                    CDP_CMD_TIMEOUT,
                )
                .await;
            if matches!(&res, Ok(Ok(_))) {
                let mut g = state.lock().await;
                g.allowed_contexts.remove(&ctx_id);
                g.extra_contexts.retain(|c| c != &ctx_id);
                drop(g);
                lease.unmark_external_context(&ctx_id);
            }
            relay_call_result(out_tx, id, None, res);
        }
        "Target.createTarget" => {
            let Some(obj) = params.as_object_mut() else {
                send_error(out_tx, id, None, -32602, "params must be an object");
                return;
            };
            {
                let g = state.lock().await;
                if let Some(ctx) = obj.get("browserContextId").and_then(Value::as_str) {
                    if !g.allowed_contexts.contains(ctx) {
                        send_error(
                            out_tx,
                            id,
                            None,
                            -32000,
                            "browserContextId not owned by this CDP lease",
                        );
                        return;
                    }
                } else {
                    obj.insert(
                        "browserContextId".into(),
                        Value::String(g.primary_context.clone()),
                    );
                }
            }
            match conn
                .call_raw("Target.createTarget", params, None, CDP_CMD_TIMEOUT)
                .await
            {
                Ok(Ok(result)) => {
                    let maybe_attach = if let Some(tid) = result["targetId"].as_str() {
                        let mut g = state.lock().await;
                        g.owned_targets.insert(tid.to_string());
                        {
                            let mut slot = g.live_target.lock().unwrap();
                            if slot.is_none() {
                                *slot = Some(tid.to_string());
                            }
                        }
                        if g.auto_attach && g.auto_attached_targets.insert(tid.to_string()) {
                            Some(tid.to_string())
                        } else {
                            None
                        }
                    } else {
                        None
                    };
                    send_result(out_tx, id, None, result);
                    if let Some(tid) = maybe_attach {
                        let _ = attach_target(conn, state, lease, out_tx, &tid, None).await;
                    }
                }
                other => relay_call_result(out_tx, id, None, other),
            }
        }
        "Target.getTargets" => {
            match conn
                .call_raw("Target.getTargets", json!({}), None, CDP_CMD_TIMEOUT)
                .await
            {
                Ok(Ok(result)) => {
                    let infos = result["targetInfos"].as_array().cloned().unwrap_or_default();
                    let filtered = filter_and_record_targets(state, infos).await;
                    send_result(out_tx, id, None, json!({ "targetInfos": filtered }));
                }
                other => relay_call_result(out_tx, id, None, other),
            }
        }
        "Target.attachToTarget" => {
            let Some(tid) = params["targetId"].as_str().map(str::to_owned) else {
                send_error(out_tx, id, None, -32602, "missing targetId");
                return;
            };
            if !ensure_target_owned(conn, state, out_tx, id, &tid).await {
                return;
            }
            match attach_target(conn, state, lease, out_tx, &tid, None).await {
                Ok(sid) => send_result(out_tx, id, None, json!({ "sessionId": sid })),
                Err(ErrPayload::Raw(err)) => send_cdp_err(out_tx, id, None, err),
                Err(ErrPayload::Msg(msg)) => send_error(out_tx, id, None, -32000, &msg),
            }
        }
        "Target.detachFromTarget" => {
            let Some(sid) = params["sessionId"].as_str().map(str::to_owned) else {
                send_error(out_tx, id, None, -32602, "missing sessionId");
                return;
            };
            if state.lock().await.owned_sessions.remove(&sid).is_none() {
                send_error(
                    out_tx,
                    id,
                    None,
                    -32000,
                    "session not owned by this CDP lease",
                );
                return;
            }
            lease.unmark_external_session(&sid);
            let res = conn
                .call_raw(
                    "Target.detachFromTarget",
                    json!({ "sessionId": sid }),
                    None,
                    CDP_CMD_TIMEOUT,
                )
                .await;
            relay_call_result(out_tx, id, None, res);
        }
        "Target.closeTarget" | "Target.activateTarget" => {
            let Some(tid) = params["targetId"].as_str().map(str::to_owned) else {
                send_error(out_tx, id, None, -32602, "missing targetId");
                return;
            };
            if !ensure_target_owned(conn, state, out_tx, id, &tid).await {
                return;
            }
            let res = conn.call_raw(method, params, None, CDP_CMD_TIMEOUT).await;
            relay_call_result(out_tx, id, None, res);
        }
        "Target.getTargetInfo" => {
            let tid = match parse_optional_target_id(&params) {
                Ok(tid) => tid.map(str::to_owned),
                Err(msg) => {
                    send_error(out_tx, id, None, -32602, msg);
                    return;
                }
            };
            let forward_params = if let Some(tid) = tid.as_deref() {
                if !ensure_target_owned(conn, state, out_tx, id, tid).await {
                    return;
                }
                params
            } else {
                // Omitted targetId queries the browser target itself (used by Playwright connectOverCDP).
                json!({})
            };
            let res = conn
                .call_raw("Target.getTargetInfo", forward_params, None, CDP_CMD_TIMEOUT)
                .await;
            relay_call_result(out_tx, id, None, res);
        }
        "Target.setDiscoverTargets" => {
            let discover = params["discover"].as_bool().unwrap_or(false);
            state.lock().await.discover_targets = discover;
            if discover {
                if let Ok(Ok(res)) = conn
                    .call_raw("Target.getTargets", json!({}), None, CDP_CMD_TIMEOUT)
                    .await
                {
                    let infos = res["targetInfos"].as_array().cloned().unwrap_or_default();
                    for info in filter_and_record_targets(state, infos).await {
                        send_event(out_tx, "Target.targetCreated", &json!({ "targetInfo": info }), None);
                    }
                }
            }
            send_result(out_tx, id, None, json!({}));
        }
        "Target.setAutoAttach" => {
            let auto = params["autoAttach"].as_bool().unwrap_or(false);
            state.lock().await.auto_attach = auto;
            let mut to_attach = Vec::new();
            if auto {
                if let Ok(Ok(res)) = conn
                    .call_raw("Target.getTargets", json!({}), None, CDP_CMD_TIMEOUT)
                    .await
                {
                    let infos = res["targetInfos"].as_array().cloned().unwrap_or_default();
                    let owned = filter_and_record_targets(state, infos).await;
                    let mut g = state.lock().await;
                    for info in owned {
                        let ty = info["type"].as_str().unwrap_or("");
                        if let Some(tid) = info["targetId"].as_str() {
                            if matches!(ty, "page" | "iframe" | "worker")
                                && g.auto_attached_targets.insert(tid.to_string())
                            {
                                to_attach.push((tid.to_string(), info));
                            }
                        }
                    }
                }
            }
            for (tid, info) in to_attach {
                let _ = attach_target(conn, state, lease, out_tx, &tid, Some(info)).await;
            }
            send_result(out_tx, id, None, json!({}));
        }
        "Storage.getCookies"
        | "Storage.setCookies"
        | "Storage.clearCookies"
        | "Browser.grantPermissions"
        | "Browser.resetPermissions"
        | "Browser.setDownloadBehavior" => {
            let Some(obj) = params.as_object_mut() else {
                send_error(out_tx, id, None, -32602, "params must be an object");
                return;
            };
            {
                let g = state.lock().await;
                if let Some(ctx) = obj.get("browserContextId").and_then(Value::as_str) {
                    if !g.allowed_contexts.contains(ctx) {
                        send_error(
                            out_tx,
                            id,
                            None,
                            -32000,
                            "browserContextId not owned by this CDP lease",
                        );
                        return;
                    }
                } else {
                    obj.insert(
                        "browserContextId".into(),
                        Value::String(g.primary_context.clone()),
                    );
                }
            }
            let res = conn.call_raw(method, params, None, CDP_CMD_TIMEOUT).await;
            relay_call_result(out_tx, id, None, res);
        }
        _ => {
            send_error(
                out_tx,
                id,
                None,
                -32601,
                &format!("Method '{method}' is not permitted on a pooled browser connection"),
            );
        }
    }
}

fn filter_create_context_params(params: &Value) -> Value {
    let mut map = serde_json::Map::new();
    if let Some(obj) = params.as_object() {
        for key in SAFE_CREATE_CONTEXT_FIELDS {
            if let Some(val) = obj.get(*key) {
                map.insert((*key).to_string(), val.clone());
            }
        }
    }
    Value::Object(map)
}

async fn filter_and_record_targets(
    state: &Arc<Mutex<GatewayState>>,
    infos: Vec<Value>,
) -> Vec<Value> {
    let mut g = state.lock().await;
    let mut out = Vec::new();
    for info in infos {
        let Some(tid) = info["targetId"].as_str() else {
            continue;
        };
        let owned = info["browserContextId"]
            .as_str()
            .is_some_and(|c| g.allowed_contexts.contains(c))
            || g.owned_targets.contains(tid);
        if owned {
            g.owned_targets.insert(tid.to_string());
            out.push(info);
        }
    }
    out
}

fn parse_optional_target_id(params: &Value) -> Result<Option<&str>, &'static str> {
    match params.get("targetId") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(tid)) => Ok(Some(tid.as_str())),
        Some(_) => Err("invalid targetId"),
    }
}

async fn ensure_target_owned(
    conn: &Arc<Connection>,
    state: &Arc<Mutex<GatewayState>>,
    out_tx: &mpsc::UnboundedSender<Message>,
    id: Option<&Value>,
    target_id: &str,
) -> bool {
    match verify_target_owned(conn, state, target_id).await {
        Ok(true) => true,
        Ok(false) => {
            send_error(
                out_tx,
                id,
                None,
                -32000,
                "targetId not owned by this CDP lease",
            );
            false
        }
        Err(e) => {
            send_error(out_tx, id, None, -32000, &e);
            false
        }
    }
}

async fn verify_target_owned(
    conn: &Arc<Connection>,
    state: &Arc<Mutex<GatewayState>>,
    target_id: &str,
) -> Result<bool, String> {
    if state.lock().await.owned_targets.contains(target_id) {
        return Ok(true);
    }
    let res = conn
        .call_raw(
            "Target.getTargetInfo",
            json!({ "targetId": target_id }),
            None,
            CDP_CMD_TIMEOUT,
        )
        .await
        .map_err(|e| e.to_string())?;
    match res {
        Ok(val) => {
            let ctx = val["targetInfo"]["browserContextId"].as_str();
            let mut g = state.lock().await;
            let owned = ctx.is_some_and(|c| g.allowed_contexts.contains(c));
            if owned {
                g.owned_targets.insert(target_id.to_string());
            }
            Ok(owned)
        }
        Err(_) => Ok(false),
    }
}

enum ErrPayload {
    Raw(Value),
    Msg(String),
}

async fn attach_target(
    conn: &Arc<Connection>,
    state: &Arc<Mutex<GatewayState>>,
    lease: &Arc<ContextLease>,
    out_tx: &mpsc::UnboundedSender<Message>,
    target_id: &str,
    info_hint: Option<Value>,
) -> Result<String, ErrPayload> {
    let attached = conn
        .call_raw(
            "Target.attachToTarget",
            json!({ "targetId": target_id, "flatten": true }),
            None,
            CDP_CMD_TIMEOUT,
        )
        .await
        .map_err(|e| ErrPayload::Msg(e.to_string()))?;

    let result = match attached {
        Ok(v) => v,
        Err(err) => {
            state.lock().await.auto_attached_targets.remove(target_id);
            return Err(ErrPayload::Raw(err));
        }
    };

    let sid = result["sessionId"]
        .as_str()
        .ok_or_else(|| ErrPayload::Msg("attachToTarget: missing sessionId".into()))?
        .to_string();

    let info = match info_hint {
        Some(v) => v,
        None => conn
            .call_raw(
                "Target.getTargetInfo",
                json!({ "targetId": target_id }),
                None,
                CDP_CMD_TIMEOUT,
            )
            .await
            .ok()
            .and_then(|r| r.ok())
            .and_then(|v| v.get("targetInfo").cloned())
            .unwrap_or_else(|| json!({ "targetId": target_id, "type": "page", "url": "" })),
    };

    lease.mark_external_session(&sid);
    {
        let mut g = state.lock().await;
        g.owned_targets.insert(target_id.to_string());
        g.owned_sessions.insert(sid.clone(), target_id.to_string());
    }

    send_event(
        out_tx,
        "Target.attachedToTarget",
        &json!({
            "sessionId": sid,
            "targetInfo": info,
            "waitingForDebugger": false,
        }),
        None,
    );

    Ok(sid)
}

fn relay_call_result<E: std::fmt::Display>(
    out_tx: &mpsc::UnboundedSender<Message>,
    id: Option<&Value>,
    session_id: Option<&str>,
    res: Result<Result<Value, Value>, E>,
) {
    match res {
        Ok(Ok(val)) => send_result(out_tx, id, session_id, val),
        Ok(Err(err)) => send_cdp_err(out_tx, id, session_id, err),
        Err(e) => send_error(out_tx, id, session_id, -32000, &e.to_string()),
    }
}

fn send_result(
    out_tx: &mpsc::UnboundedSender<Message>,
    id: Option<&Value>,
    session_id: Option<&str>,
    result: Value,
) {
    let Some(id) = id else { return };
    let mut payload = json!({ "id": id, "result": result });
    if let Some(sid) = session_id {
        payload["sessionId"] = json!(sid);
    }
    let _ = out_tx.send(Message::Text(payload.to_string().into()));
}

fn send_cdp_err(
    out_tx: &mpsc::UnboundedSender<Message>,
    id: Option<&Value>,
    session_id: Option<&str>,
    error: Value,
) {
    let Some(id) = id else { return };
    let mut payload = json!({ "id": id, "error": error });
    if let Some(sid) = session_id {
        payload["sessionId"] = json!(sid);
    }
    let _ = out_tx.send(Message::Text(payload.to_string().into()));
}

fn send_error(
    out_tx: &mpsc::UnboundedSender<Message>,
    id: Option<&Value>,
    session_id: Option<&str>,
    code: i64,
    message: &str,
) {
    let Some(id) = id else { return };
    let mut payload = json!({
        "id": id,
        "error": { "code": code, "message": message },
    });
    if let Some(sid) = session_id {
        payload["sessionId"] = json!(sid);
    }
    let _ = out_tx.send(Message::Text(payload.to_string().into()));
}

fn send_event(
    out_tx: &mpsc::UnboundedSender<Message>,
    method: &str,
    params: &Value,
    session_id: Option<&str>,
) {
    let mut payload = json!({ "method": method, "params": params });
    if let Some(sid) = session_id {
        payload["sessionId"] = json!(sid);
    }
    let _ = out_tx.send(Message::Text(payload.to_string().into()));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{HttpServer, ServerConfig};
    use axum::body::{to_bytes, Body};
    use axum::http::Request;
    use tower::ServiceExt;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn parses_cdp_query_options_and_rejects_invalid() {
        let q = CdpQuery::parse(None).unwrap();
        assert_eq!(
            q,
            CdpQuery {
                priority: Priority::Normal,
                resource_preset: None,
                timeout_ms: None,
                initial_page: false,
            }
        );

        let q = CdpQuery::parse(Some(
            "priority=high&resource_preset=no-media&timeout_ms=15000&initial_page=true&token=abc",
        ))
        .unwrap();
        assert_eq!(q.priority, Priority::High);
        assert_eq!(q.resource_preset, Some(ResourcePreset::NoMedia));
        assert_eq!(q.timeout_ms, Some(15_000));
        assert!(q.initial_page);

        assert!(CdpQuery::parse(Some("priority=urgent")).is_err());
        assert!(CdpQuery::parse(Some("timeout_ms=0")).is_err());
        assert!(CdpQuery::parse(Some("timeout_ms=300001")).is_err());
        assert!(CdpQuery::parse(Some("resource_preset=fast")).is_err());
        assert!(CdpQuery::parse(Some("initial_page=yes")).is_err());
        assert!(CdpQuery::parse(Some("unknown=1")).is_err());
        assert!(CdpQuery::parse(Some("priority=low&priority=high")).is_err());
    }

    #[test]
    fn query_token_extraction_decodes_and_rejects_duplicates() {
        assert_eq!(extract_query_token("a=1&token=abc%2Bdef"), Some(b"abc+def".to_vec()));
        assert_eq!(extract_query_token("token=raw+plus"), Some(b"raw+plus".to_vec()));
        assert_eq!(extract_query_token("priority=high"), None);
        assert_eq!(extract_query_token("token=a&token=b"), None);
    }

    #[tokio::test]
    async fn json_version_and_coordinator_gate_auth() {
        // Note: `json_version` only inspects request headers; construct standalone router via
        // `connect_over_cdp` unlaunched endpoint (never connects lazily until acquire) OR test
        // coordinator vs header middleware directly.
        let cfg = ServerConfig::new(TOKEN.into()).unwrap();
        let coordinator = HttpServer::coordinator(cfg).unwrap();
        let coord_res = coordinator
            .router()
            .oneshot(
                Request::builder()
                    .uri("/json/version")
                    .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(coord_res.status(), StatusCode::NOT_FOUND);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mock = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            while let Some(Ok(tokio_tungstenite::tungstenite::Message::Text(txt))) = ws.next().await
            {
                let v: Value = serde_json::from_str(&txt).unwrap();
                let reply = json!({ "id": v["id"], "result": { "product": "Chrome/Mock" } });
                let _ = ws
                    .send(tokio_tungstenite::tungstenite::Message::Text(
                        reply.to_string().into(),
                    ))
                    .await;
            }
        });

        let eng = fluxwright::BrowserEngine::connect_over_cdp(format!("ws://{addr}"))
            .await
            .unwrap();

        let srv_no_qt = HttpServer::new(eng.clone(), ServerConfig::new(TOKEN.into()).unwrap()).unwrap();
        let unauth = srv_no_qt
            .router()
            .oneshot(Request::builder().uri("/json/version").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(unauth.status(), StatusCode::UNAUTHORIZED);

        let rejected_qt = srv_no_qt
            .router()
            .oneshot(
                Request::builder()
                    .uri(format!("/json/version?token={TOKEN}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(rejected_qt.status(), StatusCode::UNAUTHORIZED);

        let ok_hdr = srv_no_qt
            .router()
            .oneshot(
                Request::builder()
                    .uri("/json/version")
                    .header(header::HOST, "example.internal:8443")
                    .header("x-forwarded-proto", "https")
                    .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(ok_hdr.status(), StatusCode::OK);
        let body: Value =
            serde_json::from_slice(&to_bytes(ok_hdr.into_body(), 16_384).await.unwrap()).unwrap();
        assert_eq!(
            body["webSocketDebuggerUrl"],
            "wss://example.internal:8443/cdp"
        );
        drop(srv_no_qt);

        let mut cfg_qt = ServerConfig::new(TOKEN.into()).unwrap();
        cfg_qt.allow_query_token = true;
        let srv_qt = HttpServer::new(eng, cfg_qt).unwrap();
        let ok_qt = srv_qt
            .router()
            .oneshot(
                Request::builder()
                    .uri(format!("/json/version?token={TOKEN}"))
                    .header(header::HOST, "127.0.0.1:7331")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(ok_qt.status(), StatusCode::OK);

        // `?token=` is strictly scoped to `/cdp` and `/json/version`, never `/v1/stats`.
        let rejected_stats = srv_qt
            .router()
            .oneshot(
                Request::builder()
                    .uri(format!("/v1/stats?token={TOKEN}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(rejected_stats.status(), StatusCode::UNAUTHORIZED);

        srv_qt.shutdown(Duration::from_secs(2)).await.unwrap();
        let _ = mock.await;
    }

    #[test]
    fn optional_target_id_parses_browser_and_target_queries() {
        assert_eq!(parse_optional_target_id(&json!({})), Ok(None));
        assert_eq!(parse_optional_target_id(&json!(null)), Ok(None));
        assert_eq!(parse_optional_target_id(&json!({ "targetId": null })), Ok(None));
        assert_eq!(
            parse_optional_target_id(&json!({ "targetId": "T-1" })),
            Ok(Some("T-1"))
        );
        assert_eq!(
            parse_optional_target_id(&json!({ "targetId": 123 })),
            Err("invalid targetId")
        );
    }
}
