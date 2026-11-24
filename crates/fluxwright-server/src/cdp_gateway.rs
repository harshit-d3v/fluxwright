//! Pooled CDP WebSocket gateway (`GET /cdp` and `GET /json/version`).
//!
//! Each WebSocket client receives its own isolated `ContextLease` from the engine pool. Browser-level
//! CDP commands and events are strictly scoped to the leased context (and up to 4 client-created child
//! contexts), so concurrent callers share pooled Chrome processes safely without touching each
//! other's targets, cookies, storage, or browser lifecycle.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{RawQuery, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use fluxwright::{netguard, Connection, ContextLease, JobOptions, Priority, ResourcePreset};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::{mpsc, watch, Mutex};
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
];

#[derive(Clone)]
pub(crate) struct GatewayPolicy {
    pub(crate) allow_any_url: bool,
    pub(crate) enforce_server_proxy: bool,
    pub(crate) resource_preset: Option<ResourcePreset>,
    pub(crate) download_dir: PathBuf,
}

pub(crate) fn compute_enforce_server_proxy(engine: &fluxwright::BrowserEngine) -> bool {
    engine.proxy_pool().is_some()
        || engine
            .config()
            .launch
            .extra_args
            .iter()
            .any(|a| a == "--proxy-server" || a.starts_with("--proxy-server="))
}

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

pub(crate) fn host_base(headers: &HeaderMap) -> (&'static str, &str) {
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

    (scheme, host)
}

/// Standard DevTools `/json/version` discovery endpoint pointing to `/cdp`.
pub(crate) async fn json_version(headers: HeaderMap) -> Response {
    let (scheme, host) = host_base(&headers);

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
    if let Some(timeout_ms) = query.timeout_ms {
        opts = opts.timeout(Duration::from_millis(timeout_ms));
    }

    let enforce_server_proxy = compute_enforce_server_proxy(&engine);

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
    // Always mark gateway contexts external so CdpBrowser never double-attaches or installs
    // internal dialog auto-dismissers; `resource_preset` is enforced inside the gateway via
    // lightweight Fetch.enable across both primary and child contexts.
    lease.mark_external_context(lease.context_id());

    let policy = GatewayPolicy {
        allow_any_url: inner.allow_any_url(),
        enforce_server_proxy,
        resource_preset: query.resource_preset,
        download_dir: std::env::temp_dir().join(format!(
            "fluxwright-cdp-dl-{}",
            uuid::Uuid::new_v4().simple()
        )),
    };

    Ok(ws
        .max_frame_size(MAX_WS_PAYLOAD_BYTES)
        .max_message_size(MAX_WS_PAYLOAD_BYTES)
        .on_upgrade(move |socket| run_gateway(socket, inner, lease, query.initial_page, policy)))
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

pub(crate) struct GatewayState {
    primary_context: String,
    allowed_contexts: HashSet<String>,
    extra_contexts: Vec<String>,
    pending_extra_contexts: usize,
    closing: bool,
    owned_targets: HashSet<String>,
    owned_pages: Vec<String>,
    owned_page_urls: HashMap<String, String>,
    owned_sessions: HashMap<String, String>,
    owned_window_ids: HashSet<i64>,
    blocker_sessions: HashMap<String, String>,
    blocker_targets: HashSet<String>,
    blocker_waiters: HashMap<String, watch::Receiver<bool>>,
    download_dir_created: bool,
    auto_attached_targets: HashSet<String>,
    auto_attach_waiters: HashMap<String, watch::Receiver<bool>>,
    discover_targets: bool,
    auto_attach: bool,
    pub(crate) live_target: Arc<std::sync::Mutex<Option<String>>>,
    pub(crate) live_url: Arc<std::sync::Mutex<String>>,
}

impl GatewayState {
    pub(crate) fn new(
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
            pending_extra_contexts: 0,
            closing: false,
            owned_targets: HashSet::new(),
            owned_pages: Vec::new(),
            owned_page_urls: HashMap::new(),
            owned_sessions: HashMap::new(),
            owned_window_ids: HashSet::new(),
            blocker_sessions: HashMap::new(),
            blocker_targets: HashSet::new(),
            blocker_waiters: HashMap::new(),
            download_dir_created: false,
            auto_attached_targets: HashSet::new(),
            auto_attach_waiters: HashMap::new(),
            discover_targets: false,
            auto_attach: false,
            live_target,
            live_url,
        }
    }
}

pub(crate) async fn create_initial_page(
    conn: &Arc<Connection>,
    state: &Arc<Mutex<GatewayState>>,
    lease: &Arc<ContextLease>,
    policy: &GatewayPolicy,
    viewport: Option<(u32, u32)>,
) -> Option<String> {
    let primary_context = state.lock().await.primary_context.clone();
    let mut params = json!({
        "url": "about:blank",
        "browserContextId": primary_context,
    });
    if let Some((width, height)) = viewport {
        if let Some(obj) = params.as_object_mut() {
            obj.insert("width".into(), json!(width));
            obj.insert("height".into(), json!(height));
        }
    }
    let res = conn
        .call_raw("Target.createTarget", params, None, CDP_CMD_TIMEOUT)
        .await
        .ok()?
        .ok()?;
    let tid = res["targetId"].as_str()?.to_string();
    {
        let mut g = state.lock().await;
        g.owned_targets.insert(tid.clone());
        if !g.owned_pages.iter().any(|p| p == &tid) {
            g.owned_pages.push(tid.clone());
        }
        g.owned_page_urls
            .insert(tid.clone(), String::from("about:blank"));
        *g.live_target.lock().unwrap() = Some(tid.clone());
    }
    if let Some(preset) = policy.resource_preset {
        ensure_preset_blocker(conn, state, lease, &tid, preset).await;
    }
    Some(tid)
}

async fn run_gateway(
    socket: WebSocket,
    inner: Arc<Inner>,
    lease: ContextLease,
    initial_page: bool,
    policy: GatewayPolicy,
) {
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
        primary_context,
        live_target,
        live_url,
    )));
    if initial_page {
        let _ = create_initial_page(&conn, &state, &lease, &policy, None).await;
    }
    let (_close_tx, close_rx) = watch::channel(false);
    run_gateway_loop(
        socket,
        inner,
        lease.clone(),
        state.clone(),
        policy.clone(),
        close_rx,
    )
    .await;
    teardown_gateway_state(&conn, &state, &lease, &policy).await;
}

pub(crate) async fn run_gateway_loop(
    socket: WebSocket,
    inner: Arc<Inner>,
    lease: Arc<ContextLease>,
    state: Arc<Mutex<GatewayState>>,
    policy: GatewayPolicy,
    mut close_rx: watch::Receiver<bool>,
) {
    let _active_guard = ActiveCdpSessionGuard::enter(inner.clone());
    let conn = lease.cdp_connection();

    let (mut ws_tx, mut ws_rx) = socket.split();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Message>();

    let writer_task = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            if ws_tx.send(msg).await.is_err() {
                break;
            }
        }
    });

    let mut cdp_events = conn.subscribe_unbounded();

    let mut event_task = {
        let state = state.clone();
        let conn = conn.clone();
        let lease = lease.clone();
        let out_tx = out_tx.clone();
        let preset = policy.resource_preset;
        tokio::spawn(async move {
            while let Some(ev) = cdp_events.recv().await {
                if ev.method == "Fluxwright.disconnected" {
                    let _ = out_tx.send(Message::Close(None));
                    break;
                }
                if let Some(sid) = ev.session_id.as_deref() {
                    let (is_blocker, forward, detach_child, blocker_child_tid) = {
                        let mut g = state.lock().await;
                        if g.blocker_sessions.contains_key(sid) {
                            (true, false, None, None)
                        } else if !g.owned_sessions.contains_key(sid) {
                            (false, false, None, None)
                        } else {
                            let mut detach_child = None;
                            let mut blocker_child_tid = None;
                            if ev.method == "Target.attachedToTarget" {
                                if let Some(child_sid) = ev.params["sessionId"].as_str() {
                                    let child_tid = ev.params["targetInfo"]["targetId"]
                                        .as_str()
                                        .unwrap_or_default()
                                        .to_string();
                                    let child_ty = ev.params["targetInfo"]["type"]
                                        .as_str()
                                        .unwrap_or("");
                                    if g.closing {
                                        detach_child = Some(child_sid.to_string());
                                    } else {
                                        g.owned_sessions
                                            .insert(child_sid.to_string(), child_tid.clone());
                                        if !child_tid.is_empty() {
                                            g.owned_targets.insert(child_tid.clone());
                                            if child_ty == "page"
                                                && !g.owned_pages.iter().any(|p| p == &child_tid)
                                            {
                                                g.owned_pages.push(child_tid.clone());
                                            }
                                            if preset.is_some()
                                                && matches!(child_ty, "page" | "iframe")
                                            {
                                                blocker_child_tid = Some(child_tid);
                                            }
                                        }
                                        lease.mark_external_session(child_sid);
                                    }
                                }
                            } else if ev.method == "Target.detachedFromTarget" {
                                if let Some(child_sid) = ev.params["sessionId"].as_str() {
                                    g.owned_sessions.remove(child_sid);
                                    lease.unmark_external_session(child_sid);
                                }
                            }
                            (false, !g.closing, detach_child, blocker_child_tid)
                        }
                    };
                    if is_blocker {
                        if ev.method == "Fetch.requestPaused" {
                            if let Some(req_id) = ev.params["requestId"].as_str().map(str::to_owned)
                            {
                                let conn = conn.clone();
                                let sid = sid.to_string();
                                tokio::spawn(async move {
                                    let _ = conn
                                        .call_raw(
                                            "Fetch.failRequest",
                                            json!({
                                                "requestId": req_id,
                                                "errorReason": "BlockedByClient",
                                            }),
                                            Some(&sid),
                                            Duration::from_secs(5),
                                        )
                                        .await;
                                });
                            }
                        }
                        continue;
                    }
                    if let Some(child_sid) = detach_child {
                        let conn = conn.clone();
                        tokio::spawn(async move {
                            let _ = conn
                                .call_raw(
                                    "Target.detachFromTarget",
                                    json!({ "sessionId": child_sid }),
                                    None,
                                    CLEANUP_TIMEOUT,
                                )
                                .await;
                        });
                    }
                    if let (Some(child_tid), Some(p)) = (blocker_child_tid, preset) {
                        let conn = conn.clone();
                        let state = state.clone();
                        let lease = lease.clone();
                        tokio::spawn(async move {
                            ensure_preset_blocker(&conn, &state, &lease, &child_tid, p).await;
                        });
                    }
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
                        let ty = info["type"].as_str().unwrap_or("");
                        let (owned, discover, attach_tx, should_block) = {
                            let mut g = state.lock().await;
                            let is_ours = !g.closing
                                && (ctx_id
                                    .is_some_and(|c| g.allowed_contexts.contains(c))
                                    || g.owned_targets.contains(tid));
                            if !is_ours {
                                (false, false, None, false)
                            } else {
                                g.owned_targets.insert(tid.to_string());
                                if ty == "page" {
                                    if !g.owned_pages.iter().any(|p| p == tid) {
                                        g.owned_pages.push(tid.to_string());
                                    }
                                    if let Some(u) =
                                        info["url"].as_str().filter(|s| !s.is_empty())
                                    {
                                        g.owned_page_urls
                                            .insert(tid.to_string(), u.to_string());
                                    }
                                    let mut slot = g.live_target.lock().unwrap();
                                    if slot.is_none() || ev.method == "Target.targetCreated" {
                                        *slot = Some(tid.to_string());
                                    }
                                    if slot.as_deref() == Some(tid) {
                                        if let Some(u) =
                                            info["url"].as_str().filter(|s| !s.is_empty())
                                        {
                                            *g.live_url.lock().unwrap() = u.to_string();
                                        }
                                    }
                                }
                                let attach_tx = if ev.method == "Target.targetCreated"
                                    && g.auto_attach
                                    && matches!(ty, "page" | "iframe" | "worker")
                                    && g.auto_attached_targets.insert(tid.to_string())
                                {
                                    let (tx, rx) = watch::channel(false);
                                    g.auto_attach_waiters.insert(tid.to_string(), rx);
                                    Some(tx)
                                } else {
                                    None
                                };
                                let should_block = ev.method == "Target.targetCreated"
                                    && preset.is_some()
                                    && matches!(ty, "page" | "iframe")
                                    && !g.blocker_targets.contains(tid);
                                (true, g.discover_targets, attach_tx, should_block)
                            }
                        };
                        if !owned {
                            continue;
                        }
                        if should_block {
                            if let Some(p) = preset {
                                let conn = conn.clone();
                                let state = state.clone();
                                let lease = lease.clone();
                                let tid = tid.to_string();
                                tokio::spawn(async move {
                                    ensure_preset_blocker(&conn, &state, &lease, &tid, p).await;
                                });
                            }
                        }
                        if discover {
                            send_event(&out_tx, &ev.method, &ev.params, None);
                        }
                        if let Some(tx) = attach_tx {
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
                                    preset,
                                )
                                .await;
                                state.lock().await.auto_attach_waiters.remove(&tid);
                                let _ = tx.send(true);
                            });
                        }
                    }
                    "Target.targetDestroyed" => {
                        let Some(tid) = ev.params["targetId"].as_str() else {
                            continue;
                        };
                        let (was_owned, discover, blocker_sids) = {
                            let mut g = state.lock().await;
                            let was_owned = g.owned_targets.remove(tid);
                            g.owned_pages.retain(|p| p != tid);
                            g.owned_page_urls.remove(tid);
                            g.auto_attached_targets.remove(tid);
                            g.auto_attach_waiters.remove(tid);
                            g.blocker_targets.remove(tid);
                            g.blocker_waiters.remove(tid);
                            let mut bsids = Vec::new();
                            g.blocker_sessions.retain(|bsid, t| {
                                if t == tid {
                                    bsids.push(bsid.clone());
                                    false
                                } else {
                                    true
                                }
                            });
                            let mut slot = g.live_target.lock().unwrap();
                            if slot.as_deref() == Some(tid) {
                                let fallback = g.owned_pages.last().cloned();
                                *slot = fallback.clone();
                                let next_url = fallback
                                    .and_then(|id| g.owned_page_urls.get(&id).cloned())
                                    .unwrap_or_default();
                                *g.live_url.lock().unwrap() = next_url;
                            }
                            (was_owned, g.discover_targets, bsids)
                        };
                        for bsid in blocker_sids {
                            lease.unmark_external_session(&bsid);
                        }
                        if was_owned && discover {
                            send_event(&out_tx, &ev.method, &ev.params, None);
                        }
                    }
                    "Target.detachedFromTarget" => {
                        let Some(sid) = ev.params["sessionId"].as_str() else {
                            continue;
                        };
                        let (was_blocker, was_owned) = {
                            let mut g = state.lock().await;
                            let b = g.blocker_sessions.remove(sid).is_some();
                            let o = g.owned_sessions.remove(sid).is_some();
                            (b, o)
                        };
                        if was_blocker {
                            lease.unmark_external_session(sid);
                        }
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
        if *shutdown_rx.borrow() || *close_rx.borrow() {
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
            res = close_rx.changed() => {
                if res.is_err() || *close_rx.borrow() {
                    let _ = out_tx.send(Message::Close(None));
                    break;
                }
            }
            _ = &mut event_task => {
                let _ = out_tx.send(Message::Close(None));
                break;
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
                            &policy,
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

    drop(out_tx);
    let _ = tokio::time::timeout(Duration::from_millis(500), writer_task).await;
}

/// Detaches only client-owned CDP target sessions while keeping pages, cookies, blocker
/// sessions, and browser contexts alive so another CDP client can reconnect later.
pub(crate) async fn detach_client_sessions(
    conn: &Arc<Connection>,
    state: &Arc<Mutex<GatewayState>>,
    lease: &Arc<ContextLease>,
) {
    let sessions: Vec<String> = {
        let mut g = state.lock().await;
        g.discover_targets = false;
        g.auto_attach = false;
        g.auto_attached_targets.clear();
        g.auto_attach_waiters.clear();
        g.owned_sessions.drain().map(|(s, _)| s).collect()
    };
    for sid in sessions {
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
}

/// Tears down all CDP sessions, extra contexts, ephemeral download directories, and the primary
/// browser context lease.
pub(crate) async fn teardown_gateway_state(
    conn: &Arc<Connection>,
    state: &Arc<Mutex<GatewayState>>,
    lease: &Arc<ContextLease>,
    policy: &GatewayPolicy,
) {
    let (primary_context, sessions_to_detach, contexts_to_dispose, remove_download_dir) = {
        let mut g = state.lock().await;
        // Mark closing so any in-flight createBrowserContext / attachToTarget RPCs self-dispose
        // instead of leaking Chrome state.
        g.closing = true;
        let mut sessions: Vec<String> = g.owned_sessions.drain().map(|(s, _)| s).collect();
        sessions.extend(g.blocker_sessions.drain().map(|(s, _)| s));
        let contexts: Vec<String> = std::mem::take(&mut g.extra_contexts);
        let remove_dir = g.download_dir_created;
        g.download_dir_created = false;
        (g.primary_context.clone(), sessions, contexts, remove_dir)
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

    if remove_download_dir {
        let dir = policy.download_dir.clone();
        let _ = tokio::task::spawn_blocking(move || std::fs::remove_dir_all(dir)).await;
    }
}

/// Handles one incoming client JSON-RPC frame. Returns `true` when the connection should close.
async fn handle_client_text(
    text: &str,
    conn: &Arc<Connection>,
    state: &Arc<Mutex<GatewayState>>,
    lease: &Arc<ContextLease>,
    policy: &GatewayPolicy,
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
    let mut params = val.get("params").cloned().unwrap_or_else(|| json!({}));
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
        match method.as_str() {
            "DOM.setFileInputFiles" => {
                send_error(
                    out_tx,
                    id.as_ref(),
                    Some(&sid),
                    -32000,
                    "DOM.setFileInputFiles is blocked over /cdp to prevent host file reads",
                );
                return false;
            }
            "Page.navigate" => {
                if !policy.allow_any_url {
                    match params.get("url") {
                        Some(Value::String(url)) => {
                            if let Err(why) = netguard::url_allowed(url) {
                                send_error(
                                    out_tx,
                                    id.as_ref(),
                                    Some(&sid),
                                    -32000,
                                    &format!("navigation blocked: {why}"),
                                );
                                return false;
                            }
                        }
                        Some(_) => {
                            send_error(out_tx, id.as_ref(), Some(&sid), -32602, "invalid url");
                            return false;
                        }
                        None => {}
                    }
                }
            }
            "Page.setDownloadBehavior" | "Browser.setDownloadBehavior" => {
                if method == "Browser.setDownloadBehavior"
                    && !scope_browser_context(&mut params, state, out_tx, id.as_ref(), Some(&sid))
                        .await
                {
                    return false;
                }
                if let Err((code, msg)) = prepare_download_behavior(&mut params, state, policy).await
                {
                    send_error(out_tx, id.as_ref(), Some(&sid), code, msg);
                    return false;
                }
            }
            "Storage.getCookies"
            | "Storage.setCookies"
            | "Storage.clearCookies"
            | "Browser.grantPermissions"
            | "Browser.resetPermissions"
            | "Browser.setPermission"
            | "Browser.cancelDownload" => {
                if !scope_browser_context(&mut params, state, out_tx, id.as_ref(), Some(&sid)).await
                {
                    return false;
                }
            }
            "Browser.getWindowForTarget" => {
                match parse_optional_target_id(&params) {
                    Ok(Some(tid)) => {
                        let tid = tid.to_owned();
                        if !ensure_target_owned(conn, state, out_tx, id.as_ref(), Some(&sid), &tid)
                            .await
                        {
                            return false;
                        }
                    }
                    Ok(None) => {}
                    Err(msg) => {
                        send_error(out_tx, id.as_ref(), Some(&sid), -32602, msg);
                        return false;
                    }
                }
            }
            "Browser.setWindowBounds" => {
                let Some(wid) = params.get("windowId").and_then(Value::as_i64) else {
                    send_error(
                        out_tx,
                        id.as_ref(),
                        Some(&sid),
                        -32602,
                        "missing or invalid windowId",
                    );
                    return false;
                };
                if !state.lock().await.owned_window_ids.contains(&wid) {
                    send_error(
                        out_tx,
                        id.as_ref(),
                        Some(&sid),
                        -32000,
                        "windowId not owned by this CDP lease",
                    );
                    return false;
                }
            }
            _ => {}
        }
        let conn = conn.clone();
        let state = state.clone();
        let out_tx = out_tx.clone();
        tasks.spawn(async move {
            let res = conn
                .call_raw(&method, params, Some(&sid), CDP_CMD_TIMEOUT)
                .await;
            if method == "Browser.getWindowForTarget" {
                if let Ok(Ok(ref val)) = res {
                    if let Some(wid) = val["windowId"].as_i64() {
                        state.lock().await.owned_window_ids.insert(wid);
                    }
                }
            }
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
    let policy = policy.clone();
    let out_tx = out_tx.clone();
    tasks.spawn(async move {
        handle_browser_command(
            &method,
            params,
            id.as_ref(),
            &conn,
            &state,
            &lease,
            &policy,
            &out_tx,
        )
        .await;
    });
    false
}

#[allow(clippy::too_many_arguments)]
async fn handle_browser_command(
    method: &str,
    mut params: Value,
    id: Option<&Value>,
    conn: &Arc<Connection>,
    state: &Arc<Mutex<GatewayState>>,
    lease: &Arc<ContextLease>,
    policy: &GatewayPolicy,
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
            let filtered = match filter_create_context_params(&params, policy.enforce_server_proxy) {
                Ok(v) => v,
                Err(msg) => {
                    send_error(out_tx, id, None, -32000, msg);
                    return;
                }
            };
            {
                let mut g = state.lock().await;
                if g.closing {
                    return;
                }
                if g.extra_contexts.len() + g.pending_extra_contexts >= MAX_EXTRA_CONTEXTS {
                    send_error(
                        out_tx,
                        id,
                        None,
                        -32000,
                        "browser context limit reached for this CDP lease",
                    );
                    return;
                }
                g.pending_extra_contexts += 1;
            }
            // Spawn so disconnect/cancellation of the caller task cannot abandon a newly created
            // Chrome BrowserContext without disposing it.
            let create_handle = tokio::spawn({
                let conn = conn.clone();
                let state = state.clone();
                let lease = lease.clone();
                async move {
                    let res = conn
                        .call_raw("Target.createBrowserContext", filtered, None, CDP_CMD_TIMEOUT)
                        .await
                        .map_err(|e| e.to_string());
                    match res {
                        Ok(Ok(result)) => {
                            if let Some(ctx_id) = result["browserContextId"].as_str() {
                                let should_dispose = {
                                    let mut g = state.lock().await;
                                    g.pending_extra_contexts =
                                        g.pending_extra_contexts.saturating_sub(1);
                                    if g.closing {
                                        true
                                    } else {
                                        lease.mark_external_context(ctx_id);
                                        g.allowed_contexts.insert(ctx_id.to_string());
                                        g.extra_contexts.push(ctx_id.to_string());
                                        false
                                    }
                                };
                                if should_dispose {
                                    let _ = conn
                                        .call_raw(
                                            "Target.disposeBrowserContext",
                                            json!({ "browserContextId": ctx_id }),
                                            None,
                                            CLEANUP_TIMEOUT,
                                        )
                                        .await;
                                    return Err("CDP gateway disconnected".to_string());
                                }
                            } else {
                                let mut g = state.lock().await;
                                g.pending_extra_contexts =
                                    g.pending_extra_contexts.saturating_sub(1);
                            }
                            Ok(Ok(result))
                        }
                        other => {
                            let mut g = state.lock().await;
                            g.pending_extra_contexts =
                                g.pending_extra_contexts.saturating_sub(1);
                            other
                        }
                    }
                }
            });
            match create_handle.await {
                Ok(res) => relay_call_result(out_tx, id, None, res),
                Err(e) => send_error(out_tx, id, None, -32000, &e.to_string()),
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
            if !policy.allow_any_url {
                if let Some(url_val) = obj.get("url") {
                    match url_val.as_str() {
                        Some(u) if u.is_empty() => {}
                        Some(u) => {
                            if let Err(why) = netguard::url_allowed(u) {
                                send_error(
                                    out_tx,
                                    id,
                                    None,
                                    -32000,
                                    &format!("navigation blocked: {why}"),
                                );
                                return;
                            }
                        }
                        None => {
                            send_error(out_tx, id, None, -32602, "invalid url");
                            return;
                        }
                    }
                }
            }
            let initial_url = obj
                .get("url")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .unwrap_or("about:blank")
                .to_string();
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
                    enum PostCreateAttach {
                        AttachNow(String),
                        WaitFor(watch::Receiver<bool>),
                        None,
                    }
                    let created_tid = result["targetId"].as_str().map(str::to_owned);
                    let post_attach = if let Some(ref tid) = created_tid {
                        let mut g = state.lock().await;
                        g.owned_targets.insert(tid.clone());
                        if !g.owned_pages.iter().any(|p| p == tid) {
                            g.owned_pages.push(tid.clone());
                        }
                        g.owned_page_urls
                            .entry(tid.clone())
                            .or_insert_with(|| initial_url.clone());
                        {
                            let mut slot = g.live_target.lock().unwrap();
                            if slot.is_none() {
                                *slot = Some(tid.clone());
                            }
                        }
                        if g.auto_attach {
                            if g.auto_attached_targets.insert(tid.clone()) {
                                PostCreateAttach::AttachNow(tid.clone())
                            } else if let Some(rx) = g.auto_attach_waiters.get(tid).cloned() {
                                PostCreateAttach::WaitFor(rx)
                            } else {
                                PostCreateAttach::None
                            }
                        } else {
                            PostCreateAttach::None
                        }
                    } else {
                        PostCreateAttach::None
                    };
                    if let (Some(ref tid), Some(preset)) = (&created_tid, policy.resource_preset) {
                        ensure_preset_blocker(conn, state, lease, tid, preset).await;
                    }
                    // Emit `Target.attachedToTarget` before the `Target.createTarget` result so
                    // clients like Playwright `connectOverCDP` already have the page session registered.
                    match post_attach {
                        PostCreateAttach::AttachNow(tid) => {
                            let _ = attach_target(
                                conn,
                                state,
                                lease,
                                out_tx,
                                &tid,
                                None,
                                policy.resource_preset,
                            )
                            .await;
                        }
                        PostCreateAttach::WaitFor(mut rx) => {
                            while !*rx.borrow() {
                                if rx.changed().await.is_err() {
                                    break;
                                }
                            }
                        }
                        PostCreateAttach::None => {}
                    }
                    send_result(out_tx, id, None, result);
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
            if !ensure_target_owned(conn, state, out_tx, id, None, &tid).await {
                return;
            }
            match attach_target(
                conn,
                state,
                lease,
                out_tx,
                &tid,
                None,
                policy.resource_preset,
            )
            .await
            {
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
            if !ensure_target_owned(conn, state, out_tx, id, None, &tid).await {
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
                if !ensure_target_owned(conn, state, out_tx, id, None, tid).await {
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
                let _ = attach_target(
                    conn,
                    state,
                    lease,
                    out_tx,
                    &tid,
                    Some(info),
                    policy.resource_preset,
                )
                .await;
            }
            send_result(out_tx, id, None, json!({}));
        }
        "Browser.setDownloadBehavior" => {
            if !scope_browser_context(&mut params, state, out_tx, id, None).await {
                return;
            }
            if let Err((code, msg)) = prepare_download_behavior(&mut params, state, policy).await {
                send_error(out_tx, id, None, code, msg);
                return;
            }
            let res = conn.call_raw(method, params, None, CDP_CMD_TIMEOUT).await;
            relay_call_result(out_tx, id, None, res);
        }
        "Browser.getWindowForTarget" => {
            let Some(tid) = params["targetId"].as_str().map(str::to_owned) else {
                send_error(out_tx, id, None, -32602, "missing targetId");
                return;
            };
            if !ensure_target_owned(conn, state, out_tx, id, None, &tid).await {
                return;
            }
            let res = conn.call_raw(method, params, None, CDP_CMD_TIMEOUT).await;
            if let Ok(Ok(ref val)) = res {
                if let Some(wid) = val["windowId"].as_i64() {
                    state.lock().await.owned_window_ids.insert(wid);
                }
            }
            relay_call_result(out_tx, id, None, res);
        }
        "Browser.setWindowBounds" => {
            let Some(wid) = params.get("windowId").and_then(Value::as_i64) else {
                send_error(out_tx, id, None, -32602, "missing or invalid windowId");
                return;
            };
            if !state.lock().await.owned_window_ids.contains(&wid) {
                send_error(
                    out_tx,
                    id,
                    None,
                    -32000,
                    "windowId not owned by this CDP lease",
                );
                return;
            }
            let res = conn.call_raw(method, params, None, CDP_CMD_TIMEOUT).await;
            relay_call_result(out_tx, id, None, res);
        }
        "Storage.getCookies"
        | "Storage.setCookies"
        | "Storage.clearCookies"
        | "Browser.grantPermissions"
        | "Browser.resetPermissions"
        | "Browser.setPermission"
        | "Browser.cancelDownload" => {
            if !scope_browser_context(&mut params, state, out_tx, id, None).await {
                return;
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

async fn scope_browser_context(
    params: &mut Value,
    state: &Arc<Mutex<GatewayState>>,
    out_tx: &mpsc::UnboundedSender<Message>,
    id: Option<&Value>,
    session_id: Option<&str>,
) -> bool {
    let Some(obj) = params.as_object_mut() else {
        send_error(out_tx, id, session_id, -32602, "params must be an object");
        return false;
    };
    let g = state.lock().await;
    match obj.get("browserContextId") {
        Some(Value::String(ctx)) => {
            if !g.allowed_contexts.contains(ctx.as_str()) {
                send_error(
                    out_tx,
                    id,
                    session_id,
                    -32000,
                    "browserContextId not owned by this CDP lease",
                );
                return false;
            }
        }
        None | Some(Value::Null) => {
            obj.remove("browserContextId");
            if session_id.is_none() {
                obj.insert(
                    "browserContextId".into(),
                    Value::String(g.primary_context.clone()),
                );
            }
        }
        Some(_) => {
            send_error(out_tx, id, session_id, -32602, "invalid browserContextId");
            return false;
        }
    }
    true
}

/// Validates and sanitizes `Browser.setDownloadBehavior` / `Page.setDownloadBehavior` params.
///
/// Caller-supplied `downloadPath` values are NEVER forwarded to Chrome (Playwright's
/// `connectOverCDP` routinely passes a local temp `downloadPath` during handshake, so stripping
/// and clamping to the lease's isolated ephemeral temp dir keeps Playwright working while
/// strictly preventing arbitrary host filesystem writes).
fn validate_download_behavior_params(params: &mut Value) -> Result<bool, (i64, &'static str)> {
    let Some(obj) = params.as_object_mut() else {
        return Err((-32602, "params must be an object"));
    };
    match obj.get("downloadPath") {
        None | Some(Value::Null) | Some(Value::String(_)) => {
            obj.remove("downloadPath");
        }
        Some(_) => {
            return Err((-32602, "invalid downloadPath"));
        }
    }
    let allows_downloads = matches!(
        obj.get("behavior").and_then(Value::as_str),
        Some("allow" | "allowAndName")
    );
    Ok(allows_downloads)
}

async fn prepare_download_behavior(
    params: &mut Value,
    state: &Arc<Mutex<GatewayState>>,
    policy: &GatewayPolicy,
) -> Result<(), (i64, &'static str)> {
    let allows_downloads = validate_download_behavior_params(params)?;
    if allows_downloads {
        let already_created = state.lock().await.download_dir_created;
        if !already_created {
            let dir = policy.download_dir.clone();
            tokio::task::spawn_blocking(move || std::fs::create_dir_all(dir))
                .await
                .map_err(|_| (-32000, "failed to prepare ephemeral download directory"))?
                .map_err(|_| (-32000, "failed to prepare ephemeral download directory"))?;
            state.lock().await.download_dir_created = true;
        }
        if let Some(obj) = params.as_object_mut() {
            obj.insert(
                "downloadPath".into(),
                Value::String(policy.download_dir.to_string_lossy().into_owned()),
            );
        }
    }
    Ok(())
}

fn preset_fetch_patterns(preset: ResourcePreset) -> Vec<Value> {
    let types: &[&str] = match preset {
        ResourcePreset::Scraping => &["Image", "Font", "Media"],
        ResourcePreset::Minimal => &["Image", "Font", "Media", "Stylesheet"],
        ResourcePreset::NoMedia => &["Image", "Media"],
    };
    types
        .iter()
        .map(|t| json!({ "urlPattern": "*", "resourceType": t, "requestStage": "Request" }))
        .collect()
}

async fn ensure_preset_blocker(
    conn: &Arc<Connection>,
    state: &Arc<Mutex<GatewayState>>,
    lease: &Arc<ContextLease>,
    target_id: &str,
    preset: ResourcePreset,
) {
    enum BlockerPlan {
        Install(watch::Sender<bool>),
        Wait(watch::Receiver<bool>),
        Done,
    }
    let plan = {
        let mut g = state.lock().await;
        if g.closing {
            BlockerPlan::Done
        } else if g.blocker_targets.insert(target_id.to_string()) {
            let (tx, rx) = watch::channel(false);
            g.blocker_waiters.insert(target_id.to_string(), rx);
            BlockerPlan::Install(tx)
        } else if let Some(rx) = g.blocker_waiters.get(target_id).cloned() {
            BlockerPlan::Wait(rx)
        } else {
            BlockerPlan::Done
        }
    };

    match plan {
        BlockerPlan::Done => {}
        BlockerPlan::Wait(mut rx) => {
            while !*rx.borrow() {
                if rx.changed().await.is_err() {
                    break;
                }
            }
        }
        BlockerPlan::Install(tx) => {
            // Spawn so disconnect/cancellation never leaks the internal Fetch blocker session.
            let handle = tokio::spawn({
                let conn = conn.clone();
                let state = state.clone();
                let lease = lease.clone();
                let target_id = target_id.to_string();
                async move {
                    let attached = conn
                        .call_raw(
                            "Target.attachToTarget",
                            json!({ "targetId": target_id, "flatten": true }),
                            None,
                            CDP_CMD_TIMEOUT,
                        )
                        .await;
                    let blocker_sid = attached
                        .ok()
                        .and_then(|r| r.ok())
                        .and_then(|v| v["sessionId"].as_str().map(str::to_owned));
                    let Some(bsid) = blocker_sid else {
                        let mut g = state.lock().await;
                        g.blocker_targets.remove(&target_id);
                        g.blocker_waiters.remove(&target_id);
                        let _ = tx.send(true);
                        return;
                    };
                    let should_detach = {
                        let mut g = state.lock().await;
                        if g.closing {
                            true
                        } else {
                            lease.mark_external_session(&bsid);
                            g.blocker_sessions.insert(bsid.clone(), target_id.clone());
                            false
                        }
                    };
                    if should_detach {
                        let _ = conn
                            .call_raw(
                                "Target.detachFromTarget",
                                json!({ "sessionId": bsid }),
                                None,
                                CLEANUP_TIMEOUT,
                            )
                            .await;
                        let mut g = state.lock().await;
                        g.blocker_waiters.remove(&target_id);
                        let _ = tx.send(true);
                        return;
                    }
                    let enabled = conn
                        .call_raw(
                            "Fetch.enable",
                            json!({
                                "patterns": preset_fetch_patterns(preset),
                                "handleAuthRequests": false,
                            }),
                            Some(&bsid),
                            Duration::from_secs(5),
                        )
                        .await;
                    if !matches!(enabled, Ok(Ok(_))) {
                        {
                            let mut g = state.lock().await;
                            g.blocker_sessions.remove(&bsid);
                            g.blocker_targets.remove(&target_id);
                        }
                        lease.unmark_external_session(&bsid);
                        let _ = conn
                            .call_raw(
                                "Target.detachFromTarget",
                                json!({ "sessionId": bsid }),
                                None,
                                CLEANUP_TIMEOUT,
                            )
                            .await;
                    }
                    state.lock().await.blocker_waiters.remove(&target_id);
                    let _ = tx.send(true);
                }
            });
            let _ = handle.await;
        }
    }
}

fn filter_create_context_params(
    params: &Value,
    enforce_server_proxy: bool,
) -> Result<Value, &'static str> {
    let mut map = serde_json::Map::new();
    if let Some(obj) = params.as_object() {
        if enforce_server_proxy
            && (obj.get("proxyServer").is_some_and(|v| !v.is_null())
                || obj.get("proxyBypassList").is_some_and(|v| !v.is_null()))
        {
            return Err(
                "custom proxyServer/proxyBypassList is not permitted when server proxy is configured",
            );
        }
        for key in SAFE_CREATE_CONTEXT_FIELDS {
            if enforce_server_proxy && matches!(*key, "proxyServer" | "proxyBypassList") {
                continue;
            }
            if let Some(val) = obj.get(*key) {
                map.insert((*key).to_string(), val.clone());
            }
        }
    }
    Ok(Value::Object(map))
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
            if info["type"].as_str() == Some("page") {
                if !g.owned_pages.iter().any(|p| p == tid) {
                    g.owned_pages.push(tid.to_string());
                }
                if let Some(u) = info["url"].as_str().filter(|s| !s.is_empty()) {
                    g.owned_page_urls.insert(tid.to_string(), u.to_string());
                }
            }
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
    session_id: Option<&str>,
    target_id: &str,
) -> bool {
    match verify_target_owned(conn, state, target_id).await {
        Ok(true) => true,
        Ok(false) => {
            send_error(
                out_tx,
                id,
                session_id,
                -32000,
                "targetId not owned by this CDP lease",
            );
            false
        }
        Err(e) => {
            send_error(out_tx, id, session_id, -32000, &e);
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
                if val["targetInfo"]["type"].as_str() == Some("page")
                    && !g.owned_pages.iter().any(|p| p == target_id)
                {
                    g.owned_pages.push(target_id.to_string());
                }
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
    resource_preset: Option<ResourcePreset>,
) -> Result<String, ErrPayload> {
    if state.lock().await.closing {
        return Err(ErrPayload::Msg("CDP gateway disconnected".into()));
    }

    // Spawn the attach + record-or-detach step so caller cancellation cannot leak an attached
    // Chrome session on disconnect.
    let attach_handle = tokio::spawn({
        let conn = conn.clone();
        let state = state.clone();
        let lease = lease.clone();
        let target_id = target_id.to_string();
        async move {
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
                    state.lock().await.auto_attached_targets.remove(&target_id);
                    return Err(ErrPayload::Raw(err));
                }
            };

            let sid = result["sessionId"]
                .as_str()
                .ok_or_else(|| ErrPayload::Msg("attachToTarget: missing sessionId".into()))?
                .to_string();

            let should_detach = {
                let mut g = state.lock().await;
                if g.closing {
                    true
                } else {
                    lease.mark_external_session(&sid);
                    g.owned_targets.insert(target_id.clone());
                    g.owned_sessions.insert(sid.clone(), target_id.clone());
                    false
                }
            };
            if should_detach {
                let _ = conn
                    .call_raw(
                        "Target.detachFromTarget",
                        json!({ "sessionId": sid }),
                        None,
                        CLEANUP_TIMEOUT,
                    )
                    .await;
                return Err(ErrPayload::Msg("CDP gateway disconnected".into()));
            }
            Ok(sid)
        }
    });

    let sid = attach_handle
        .await
        .map_err(|e| ErrPayload::Msg(e.to_string()))??;

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

    let ty = info["type"].as_str().unwrap_or("");
    if ty == "page" {
        let mut g = state.lock().await;
        if !g.owned_pages.iter().any(|p| p == target_id) {
            g.owned_pages.push(target_id.to_string());
        }
    }
    if let Some(preset) = resource_preset {
        if matches!(ty, "page" | "iframe" | "") {
            ensure_preset_blocker(conn, state, lease, target_id, preset).await;
        }
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

    #[test]
    fn create_context_filter_strips_universal_access_and_enforces_server_proxy() {
        let raw = json!({
            "disposeOnDetach": true,
            "originsWithUniversalNetworkAccess": ["https://evil.example"],
            "proxyServer": "http://127.0.0.1:8888",
            "proxyBypassList": "localhost",
            "unknownField": 1
        });

        let allowed = filter_create_context_params(&raw, false).unwrap();
        assert_eq!(allowed["disposeOnDetach"], json!(true));
        assert_eq!(allowed["proxyServer"], json!("http://127.0.0.1:8888"));
        assert_eq!(allowed["proxyBypassList"], json!("localhost"));
        assert!(allowed.get("originsWithUniversalNetworkAccess").is_none());
        assert!(allowed.get("unknownField").is_none());

        assert!(filter_create_context_params(&raw, true).is_err());

        let no_proxy = json!({
            "disposeOnDetach": true,
            "originsWithUniversalNetworkAccess": ["https://evil.example"]
        });
        let enforced_ok = filter_create_context_params(&no_proxy, true).unwrap();
        assert_eq!(enforced_ok, json!({ "disposeOnDetach": true }));
    }

    #[tokio::test]
    async fn download_behavior_strips_and_clamps_caller_download_path() {
        let mut bad_type = json!({ "behavior": "allow", "downloadPath": 42 });
        assert_eq!(
            validate_download_behavior_params(&mut bad_type),
            Err((-32602, "invalid downloadPath"))
        );

        let mut deny = json!({ "behavior": "deny", "downloadPath": "/tmp/host-escape" });
        assert_eq!(validate_download_behavior_params(&mut deny), Ok(false));
        assert!(deny.get("downloadPath").is_none());

        let mut allow = json!({ "behavior": "allowAndName", "downloadPath": "/tmp/host-escape" });
        assert_eq!(validate_download_behavior_params(&mut allow), Ok(true));
        assert!(allow.get("downloadPath").is_none());

        // prepare_download_behavior clamps allowed downloads strictly to the lease's ephemeral dir.
        let ephemeral = std::env::temp_dir().join(format!(
            "fluxwright-cdp-dl-unit-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let policy = GatewayPolicy {
            allow_any_url: false,
            enforce_server_proxy: false,
            resource_preset: None,
            download_dir: ephemeral.clone(),
        };
        let state = Arc::new(Mutex::new(GatewayState::new(
            "ctx-1".into(),
            Arc::new(std::sync::Mutex::new(None)),
            Arc::new(std::sync::Mutex::new(String::new())),
        )));
        let mut clamped = json!({ "behavior": "allow", "downloadPath": "/etc/cron.d" });
        prepare_download_behavior(&mut clamped, &state, &policy)
            .await
            .unwrap();
        assert_eq!(
            clamped["downloadPath"],
            Value::String(ephemeral.to_string_lossy().into_owned())
        );
        assert!(state.lock().await.download_dir_created);
        let _ = std::fs::remove_dir_all(&ephemeral);
    }

    #[test]
    fn preset_fetch_patterns_cover_expected_resource_types() {
        let scraping = preset_fetch_patterns(ResourcePreset::Scraping);
        let types: Vec<&str> = scraping
            .iter()
            .filter_map(|v| v["resourceType"].as_str())
            .collect();
        assert_eq!(types, vec!["Image", "Font", "Media"]);

        let minimal = preset_fetch_patterns(ResourcePreset::Minimal);
        let m_types: Vec<&str> = minimal
            .iter()
            .filter_map(|v| v["resourceType"].as_str())
            .collect();
        assert_eq!(m_types, vec!["Image", "Font", "Media", "Stylesheet"]);

        let no_media = preset_fetch_patterns(ResourcePreset::NoMedia);
        let nm_types: Vec<&str> = no_media
            .iter()
            .filter_map(|v| v["resourceType"].as_str())
            .collect();
        assert_eq!(nm_types, vec!["Image", "Media"]);
    }
}
