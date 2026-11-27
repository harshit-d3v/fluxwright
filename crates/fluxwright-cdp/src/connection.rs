use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::{connect_async_tls_with_config, tungstenite::Message, Connector};
use tracing::{debug, warn};

use crate::error::{Error, Result};

#[derive(Debug, Clone)]
pub struct CdpEvent {
    pub method: String,
    pub params: Value,
    pub session_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Incoming {
    id: Option<u64>,
    method: Option<String>,
    params: Option<Value>,
    result: Option<Value>,
    error: Option<Value>,
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
}

enum CdpReply {
    Ok(Value),
    Err(Value),
}

struct Pending {
    tx: oneshot::Sender<Result<CdpReply>>,
}

/// Every subscriber of a connection, each handed a pointer to the same parsed event.
type Watchers = Mutex<Vec<mpsc::UnboundedSender<Arc<CdpEvent>>>>;

fn parse_cdp_debug_env(val: &str) -> bool {
    val.split(',').any(|tok| {
        let t = tok.trim();
        t == "1"
            || t.eq_ignore_ascii_case("true")
            || t.eq_ignore_ascii_case("all")
            || t.eq_ignore_ascii_case("cdp")
    })
}

fn cdp_debug_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("FLUXWRIGHT_DEBUG")
            .map(|v| parse_cdp_debug_env(&v))
            .unwrap_or(false)
    })
}

/// Handle for dynamically adding/removing child session routes (e.g. OOPIF iframes, workers)
/// to an active [`SessionSubscription`]. Dropping the last handle and subscription cleans up
/// the subscriber's route entries.
#[derive(Clone)]
pub(crate) struct SessionRouter {
    token: Arc<SessionRouteToken>,
}

struct SessionRouteToken {
    routes: Arc<Mutex<SessionRoutes>>,
    sub_id: Option<u64>,
}

impl Drop for SessionRouteToken {
    fn drop(&mut self) {
        if let Some(sub_id) = self.sub_id {
            self.routes.lock().unwrap().unsubscribe(sub_id);
        }
    }
}

impl SessionRouter {
    pub(crate) fn add_session(&self, session_id: impl Into<String>) {
        if let Some(sub_id) = self.token.sub_id {
            self.token
                .routes
                .lock()
                .unwrap()
                .add_session(sub_id, session_id.into());
        }
    }

    pub(crate) fn remove_session(&self, session_id: &str) {
        if let Some(sub_id) = self.token.sub_id {
            self.token
                .routes
                .lock()
                .unwrap()
                .remove_session(sub_id, session_id);
        }
    }
}

pub(crate) struct SessionSubscription {
    pub(crate) rx: mpsc::UnboundedReceiver<Arc<CdpEvent>>,
    pub(crate) router: SessionRouter,
}

impl SessionSubscription {
    #[allow(dead_code)]
    pub(crate) async fn recv(&mut self) -> Option<Arc<CdpEvent>> {
        self.rx.recv().await
    }

    #[allow(dead_code)]
    pub(crate) fn add_session(&self, session_id: impl Into<String>) {
        self.router.add_session(session_id);
    }

    #[allow(dead_code)]
    pub(crate) fn remove_session(&self, session_id: &str) {
        self.router.remove_session(session_id);
    }

    #[allow(dead_code)]
    pub(crate) fn router(&self) -> SessionRouter {
        self.router.clone()
    }

    pub(crate) fn into_parts(self) -> (mpsc::UnboundedReceiver<Arc<CdpEvent>>, SessionRouter) {
        (self.rx, self.router)
    }
}

struct SessionSubscriber {
    tx: mpsc::UnboundedSender<Arc<CdpEvent>>,
    sessions: HashSet<String>,
}

#[derive(Default)]
struct SessionRoutes {
    next_sub_id: u64,
    subscribers: HashMap<u64, SessionSubscriber>,
    by_session: HashMap<String, Vec<u64>>,
}

impl SessionRoutes {
    fn subscribe(
        &mut self,
        session_id: String,
        tx: mpsc::UnboundedSender<Arc<CdpEvent>>,
    ) -> u64 {
        let id = self.next_sub_id;
        self.next_sub_id += 1;
        let mut sessions = HashSet::with_capacity(1);
        sessions.insert(session_id.clone());
        self.subscribers.insert(id, SessionSubscriber { tx, sessions });
        self.by_session.entry(session_id).or_default().push(id);
        id
    }

    fn add_session(&mut self, sub_id: u64, session_id: String) {
        let Some(sub) = self.subscribers.get_mut(&sub_id) else {
            return;
        };
        if sub.sessions.insert(session_id.clone()) {
            self.by_session.entry(session_id).or_default().push(sub_id);
        }
    }

    fn remove_session(&mut self, sub_id: u64, session_id: &str) {
        let Some(sub) = self.subscribers.get_mut(&sub_id) else {
            return;
        };
        if sub.sessions.remove(session_id) {
            if let Some(ids) = self.by_session.get_mut(session_id) {
                ids.retain(|&id| id != sub_id);
                if ids.is_empty() {
                    self.by_session.remove(session_id);
                }
            }
        }
    }

    fn unsubscribe(&mut self, sub_id: u64) {
        if let Some(sub) = self.subscribers.remove(&sub_id) {
            for sid in sub.sessions {
                if let Some(ids) = self.by_session.get_mut(&sid) {
                    ids.retain(|&id| id != sub_id);
                    if ids.is_empty() {
                        self.by_session.remove(&sid);
                    }
                }
            }
        }
    }

    fn alias_child(&mut self, parent_sid: &str, child_sid: &str) {
        let Some(sub_ids) = self.by_session.get(parent_sid) else {
            return;
        };
        if sub_ids.len() == 1 {
            let sub_id = sub_ids[0];
            self.add_session(sub_id, child_sid.to_string());
        } else {
            let ids = sub_ids.clone();
            for sub_id in ids {
                self.add_session(sub_id, child_sid.to_string());
            }
        }
    }

    fn detach_session(&mut self, sid: &str) {
        if let Some(ids) = self.by_session.remove(sid) {
            for sub_id in ids {
                if let Some(sub) = self.subscribers.get_mut(&sub_id) {
                    sub.sessions.remove(sid);
                }
            }
        }
    }

    fn dispatch(&mut self, ev: &Arc<CdpEvent>) {
        if self.subscribers.is_empty() {
            return;
        }
        match ev.session_id.as_deref() {
            None => {
                let mut dead: Option<Vec<u64>> = None;
                for (&id, sub) in &self.subscribers {
                    if sub.tx.send(ev.clone()).is_err() {
                        dead.get_or_insert_with(Vec::new).push(id);
                    }
                }
                if let Some(dead) = dead {
                    for id in dead {
                        self.unsubscribe(id);
                    }
                }
                if ev.method == "Target.detachedFromTarget" {
                    if let Some(dsid) = ev.params["sessionId"].as_str() {
                        self.detach_session(dsid);
                    }
                }
            }
            Some(sid) => {
                if ev.method == "Target.attachedToTarget"
                    && matches!(
                        ev.params["targetInfo"]["type"].as_str(),
                        Some("iframe" | "worker")
                    )
                {
                    if let Some(child_sid) = ev.params["sessionId"].as_str() {
                        self.alias_child(sid, child_sid);
                    }
                }
                if let Some(ids) = self.by_session.get(sid) {
                    let mut dead: Option<Vec<u64>> = None;
                    for &id in ids {
                        if let Some(sub) = self.subscribers.get(&id) {
                            if sub.tx.send(ev.clone()).is_err() {
                                dead.get_or_insert_with(Vec::new).push(id);
                            }
                        }
                    }
                    if let Some(dead) = dead {
                        for id in dead {
                            self.unsubscribe(id);
                        }
                    }
                }
                if ev.method == "Target.detachedFromTarget" {
                    if let Some(dsid) = ev.params["sessionId"].as_str() {
                        self.detach_session(dsid);
                    }
                }
            }
        }
    }

    fn drain_with(&mut self, ev: &Arc<CdpEvent>) {
        self.by_session.clear();
        for (_, sub) in self.subscribers.drain() {
            let _ = sub.tx.send(ev.clone());
        }
    }
}

pub struct Connection {
    next_id: AtomicU64,
    tx: mpsc::UnboundedSender<Outgoing>,
    pending: Arc<Mutex<HashMap<u64, Pending>>>,
    watchers: Arc<Watchers>,
    session_routes: Arc<Mutex<SessionRoutes>>,
    dead: Arc<AtomicBool>,
}

enum Outgoing {
    Json(String),
    /// Ends the transport: a WebSocket close handshake, or the pipe's write end.
    Close,
}

async fn run_ws_writer<S>(mut sink: S, mut out_rx: mpsc::UnboundedReceiver<Outgoing>)
where
    S: futures_util::Sink<Message> + Unpin,
{
    while let Some(out) = out_rx.recv().await {
        let mut should_close = false;
        let mut err = false;
        match out {
            Outgoing::Json(s) => {
                if sink.feed(Message::Text(s.into())).await.is_err() {
                    break;
                }
                while let Ok(next) = out_rx.try_recv() {
                    match next {
                        Outgoing::Json(s) => {
                            if sink.feed(Message::Text(s.into())).await.is_err() {
                                err = true;
                                break;
                            }
                        }
                        Outgoing::Close => {
                            should_close = true;
                            break;
                        }
                    }
                }
            }
            Outgoing::Close => {
                should_close = true;
            }
        }
        if err {
            break;
        }
        if should_close {
            // The peer answers the close, which ends the reader below.
            let _ = sink.close().await;
            break;
        }
        if sink.flush().await.is_err() {
            break;
        }
    }
}

async fn run_pipe_writer<W>(mut writer: W, mut out_rx: mpsc::UnboundedReceiver<Outgoing>)
where
    W: tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::AsyncWriteExt;
    // Close ends the loop and drops the write end.
    let mut buf = Vec::with_capacity(4096);
    while let Some(out) = out_rx.recv().await {
        let Outgoing::Json(s) = out else {
            break;
        };
        buf.clear();
        buf.extend_from_slice(s.as_bytes());
        buf.push(0);
        let mut close_after = false;
        while let Ok(next) = out_rx.try_recv() {
            match next {
                Outgoing::Json(s) => {
                    buf.extend_from_slice(s.as_bytes());
                    buf.push(0);
                }
                Outgoing::Close => {
                    close_after = true;
                    break;
                }
            }
        }
        if writer.write_all(&buf).await.is_err() {
            break;
        }
        if buf.capacity() > 64 * 1024 {
            buf = Vec::with_capacity(4096);
        }
        if close_after {
            break;
        }
    }
}

impl Connection {
    pub async fn connect(ws_url: &str) -> Result<Arc<Self>> {
        // Chrome sends every CDP message as a single frame, and a full-page screenshot can
        // pass tungstenite's 16 MiB default, which closes the socket and every job on it.
        // 256 MB is Playwright's cap. Nagle off: CDP is many small request/response pairs.
        let limit = Some(256 << 20);
        let config = WebSocketConfig::default()
            .max_message_size(limit)
            .max_frame_size(limit);
        let connector = ws_url.starts_with("wss://").then(tls_connector);
        let (ws, _) = connect_async_tls_with_config(ws_url, Some(config), true, connector)
            .await
            .map_err(|e| Error::WebSocket(e.to_string()))?;
        let (sink, mut stream) = ws.split();
        let (conn, out_rx, inbound) = Self::open();

        tokio::spawn(run_ws_writer(sink, out_rx));

        tokio::spawn(async move {
            while let Some(msg) = stream.next().await {
                match msg {
                    Ok(Message::Text(text)) => inbound.handle(&text),
                    Ok(Message::Ping(_)) | Ok(Message::Pong(_)) | Ok(Message::Binary(_)) => {}
                    Ok(Message::Close(_)) | Err(_) => break,
                    Ok(Message::Frame(_)) => {}
                }
            }
            inbound.closed();
        });

        Ok(conn)
    }

    /// `--remote-debugging-pipe`: NUL-separated JSON over two pipes.
    #[cfg(unix)]
    pub fn from_pipes(
        to_chrome: std::os::fd::OwnedFd,
        from_chrome: std::os::fd::OwnedFd,
    ) -> Result<Arc<Self>> {
        use tokio::io::{AsyncBufReadExt, BufReader};
        use tokio::net::unix::pipe;

        let writer = pipe::Sender::from_owned_fd(to_chrome)?;
        let reader = pipe::Receiver::from_owned_fd(from_chrome)?;
        let (conn, out_rx, inbound) = Self::open();

        tokio::spawn(run_pipe_writer(writer, out_rx));

        tokio::spawn(async move {
            let mut reader = BufReader::new(reader);
            let mut buf = Vec::new();
            loop {
                buf.clear();
                match reader.read_until(0, &mut buf).await {
                    Ok(0) | Err(_) => break, // Chrome exited
                    Ok(_) => {
                        if buf.last() == Some(&0) {
                            buf.pop();
                        }
                        match std::str::from_utf8(&buf) {
                            Ok(text) => inbound.handle(text),
                            Err(e) => warn!(error = %e, "cdp pipe: invalid utf-8"),
                        }
                    }
                }
            }
            inbound.closed();
        });

        Ok(conn)
    }

    /// A connection with no transport yet: the caller spawns a writer draining the
    /// receiver and a reader feeding `Inbound`.
    fn open() -> (Arc<Self>, mpsc::UnboundedReceiver<Outgoing>, Inbound) {
        let (out_tx, out_rx) = mpsc::unbounded_channel::<Outgoing>();
        let inbound = Inbound {
            pending: Arc::new(Mutex::new(HashMap::new())),
            watchers: Arc::new(Mutex::new(Vec::new())),
            session_routes: Arc::new(Mutex::new(SessionRoutes::default())),
            dead: Arc::new(AtomicBool::new(false)),
        };
        let conn = Arc::new(Self {
            next_id: AtomicU64::new(1),
            tx: out_tx,
            pending: inbound.pending.clone(),
            watchers: inbound.watchers.clone(),
            session_routes: inbound.session_routes.clone(),
            dead: inbound.dead.clone(),
        });
        (conn, out_rx, inbound)
    }

    pub fn is_dead(&self) -> bool {
        self.dead.load(Ordering::SeqCst)
    }

    /// Never-lagging event stream for the browser session state machine. Ends when the
    /// connection does (at once if it already has). Every subscriber shares one copy of each
    /// event: a browser's pages each subscribe, and a deep copy per page cost pages² work.
    pub fn subscribe_unbounded(&self) -> mpsc::UnboundedReceiver<Arc<CdpEvent>> {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut watchers = self.watchers.lock().unwrap();
        // Checked under the lock `closed` clears the list with, so no sender outlives it.
        if !self.is_dead() {
            watchers.push(tx);
        }
        rx
    }

    /// Session-routed event stream for per-page collectors (`PageLog::collect`): receives only
    /// events matching `session_id` (plus dynamically attached child iframe/worker sessions) or
    /// browser-wide events (`session_id.is_none()`), avoiding O(pages²) event fanout.
    pub(crate) fn subscribe_session(&self, session_id: impl Into<String>) -> SessionSubscription {
        let (tx, rx) = mpsc::unbounded_channel();
        let sub_id = {
            let mut routes = self.session_routes.lock().unwrap();
            if !self.is_dead() {
                Some(routes.subscribe(session_id.into(), tx))
            } else {
                None
            }
        };
        SessionSubscription {
            rx,
            router: SessionRouter {
                token: Arc::new(SessionRouteToken {
                    routes: self.session_routes.clone(),
                    sub_id,
                }),
            },
        }
    }

    pub async fn call(
        &self,
        method: &str,
        params: Value,
        session_id: Option<&str>,
        timeout: Duration,
    ) -> Result<Value> {
        match self.call_raw(method, params, session_id, timeout).await? {
            Ok(val) => Ok(val),
            Err(err) => {
                let message = err
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("cdp error")
                    .to_string();
                Err(Error::Command {
                    method: method.to_string(),
                    message,
                })
            }
        }
    }

    /// Sends a CDP command and returns Chrome's raw JSON-RPC `result` (`Ok`) or `error` (`Err`)
    /// object verbatim without stripping error codes/data.
    pub async fn call_raw(
        &self,
        method: &str,
        params: Value,
        session_id: Option<&str>,
        timeout: Duration,
    ) -> Result<std::result::Result<Value, Value>> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        {
            // Checked under the lock `closed` drains the map with: a call that lands as the
            // connection dies fails now instead of waiting out its timeout.
            let mut pending = self.pending.lock().unwrap();
            if self.is_dead() {
                return Err(Error::WebSocket("connection closed".into()));
            }
            pending.insert(id, Pending { tx });
        }
        let mut body = json!({ "id": id, "method": method, "params": params });
        if let Some(sid) = session_id {
            body["sessionId"] = json!(sid);
        }
        let debug_start = cdp_debug_enabled().then(Instant::now);
        self.tx
            .send(Outgoing::Json(body.to_string()))
            .map_err(|_| Error::WebSocket("writer gone".into()))?;
        let outcome = match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(Ok(CdpReply::Ok(val)))) => Ok(Ok(val)),
            Ok(Ok(Ok(CdpReply::Err(err)))) => Ok(Err(err)),
            Ok(Ok(Err(transport_err))) => Err(transport_err),
            Ok(Err(_)) => Err(Error::WebSocket("cancelled".into())),
            Err(_) => {
                self.pending.lock().unwrap().remove(&id);
                Err(Error::timeout(method, timeout.as_millis() as u64))
            }
        };
        if let Some(t0) = debug_start {
            let status = match &outcome {
                Ok(Ok(_)) => "ok",
                Ok(Err(_)) => "cdp_error",
                Err(Error::Timeout { .. }) => "timeout",
                Err(_) => "transport_error",
            };
            eprintln!(
                "[fluxwright:cdp] id={id} session={} method={method} status={status} elapsed_ms={}",
                session_id.unwrap_or("-"),
                t0.elapsed().as_millis(),
            );
        }
        outcome
    }

    pub fn mark_dead(&self) {
        self.dead.store(true, Ordering::SeqCst);
    }

    /// Marks the connection dead and ends its transport, so its reader stops and everything
    /// subscribed is told and let go. For a remote browser this is the only thing that ends
    /// the connection: nothing kills a process on the other end. If the peer never answers the
    /// close (a hung browser behind a live socket), pending calls are failed and subscribers
    /// told after 5 s anyway, so nothing waits forever on a dead link.
    pub fn close(&self) {
        self.mark_dead();
        let _ = self.tx.send(Outgoing::Close);
        let pending = self.pending.clone();
        let watchers = self.watchers.clone();
        let session_routes = self.session_routes.clone();
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            return;
        };
        rt.spawn(async move {
            tokio::time::sleep(Duration::from_secs(5)).await;
            fail_all(&pending, Error::WebSocket("connection closed".into()));
            notify_and_drain(&watchers, &session_routes);
        });
    }
}

/// TLS for `wss://` remote browsers. The provider is explicit: `ClientConfig::builder()` needs a
/// process-wide default that the app embedding this library may never install.
fn tls_connector() -> Connector {
    let roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .expect("ring supports the default TLS versions")
    .with_root_certificates(roots)
    .with_no_client_auth();
    Connector::Rustls(Arc::new(config))
}

/// The reader's half of a connection, whatever the transport.
struct Inbound {
    pending: Arc<Mutex<HashMap<u64, Pending>>>,
    watchers: Arc<Watchers>,
    session_routes: Arc<Mutex<SessionRoutes>>,
    dead: Arc<AtomicBool>,
}

impl Inbound {
    fn handle(&self, text: &str) {
        if let Err(e) = dispatch(text, &self.pending, &self.watchers, &self.session_routes) {
            warn!(error = %e, "cdp dispatch");
        }
    }

    /// The browser end went away: fail every pending call, tell subscribers, then drop their
    /// senders so their loops end. The browser's own loop holds the browser: without this,
    /// a closed browser (its profile folder, tasks and pipes) is never freed.
    fn closed(&self) {
        self.dead.store(true, Ordering::SeqCst);
        fail_all(&self.pending, Error::WebSocket("connection closed".into()));
        notify_and_drain(&self.watchers, &self.session_routes);
    }
}

fn notify_and_drain(watchers: &Watchers, session_routes: &Mutex<SessionRoutes>) {
    let ev = Arc::new(CdpEvent {
        method: "Fluxwright.disconnected".into(),
        params: json!({}),
        session_id: None,
    });
    {
        let mut watchers = watchers.lock().unwrap();
        for tx in watchers.drain(..) {
            let _ = tx.send(ev.clone());
        }
    }
    session_routes.lock().unwrap().drain_with(&ev);
}

fn fanout(watchers: &Watchers, session_routes: &Mutex<SessionRoutes>, ev: CdpEvent) {
    let ev = Arc::new(ev);
    watchers
        .lock()
        .unwrap()
        .retain(|tx| tx.send(ev.clone()).is_ok());
    session_routes.lock().unwrap().dispatch(&ev);
}

fn dispatch(
    text: &str,
    pending: &Mutex<HashMap<u64, Pending>>,
    watchers: &Watchers,
    session_routes: &Mutex<SessionRoutes>,
) -> Result<()> {
    let msg: Incoming = match serde_json::from_str(text) {
        Ok(msg) => msg,
        Err(e) => match replace_lone_surrogates(text) {
            Some(fixed) => serde_json::from_str(&fixed)?,
            None => return Err(e.into()),
        },
    };
    if let Some(id) = msg.id {
        if let Some(p) = pending.lock().unwrap().remove(&id) {
            let reply = match msg.error {
                Some(err) => CdpReply::Err(err),
                None => CdpReply::Ok(msg.result.unwrap_or(Value::Null)),
            };
            let _ = p.tx.send(Ok(reply));
        }
        return Ok(());
    }
    if let Some(method) = msg.method {
        debug!(%method, session = ?msg.session_id, "cdp event");
        let ev = CdpEvent {
            method,
            params: msg.params.unwrap_or(Value::Null),
            session_id: msg.session_id,
        };
        fanout(watchers, session_routes, ev);
    }
    Ok(())
}

fn fail_all(pending: &Mutex<HashMap<u64, Pending>>, err: Error) {
    let mut map = pending.lock().unwrap();
    for (_, p) in map.drain() {
        let _ = p.tx.send(Err(Error::WebSocket(err.to_string())));
    }
}

/// Chrome writes each UTF-16 unit of a JS string as its own `\uXXXX`, so a string cut through
/// an emoji arrives with a lone surrogate, which serde_json rejects, and with it the whole
/// message (the call then waited out its timeout). Replaces lone surrogate escapes with
/// U+FFFD, as a JS-to-UTF-8 conversion does; `None` when there are none. In JSON text every
/// backslash starts an escape inside a string, so no string tracking is needed.
fn replace_lone_surrogates(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let unit = |at: usize| -> Option<u32> {
        if bytes.get(at) != Some(&b'\\') || bytes.get(at + 1) != Some(&b'u') {
            return None;
        }
        let hex = std::str::from_utf8(bytes.get(at + 2..at + 6)?).ok()?;
        u32::from_str_radix(hex, 16).ok()
    };
    let (mut out, mut copied, mut i) = (String::new(), 0, 0);
    while i < bytes.len() {
        if bytes[i] != b'\\' {
            i += 1;
            continue;
        }
        let lone = match unit(i) {
            Some(0xD800..=0xDBFF) => !matches!(unit(i + 6), Some(0xDC00..=0xDFFF)),
            Some(0xDC00..=0xDFFF) => true,
            _ => false,
        };
        if lone {
            out.push_str(&text[copied..i]);
            out.push_str("\\ufffd");
            copied = i + 6;
            i += 6;
        } else if matches!(unit(i), Some(0xD800..=0xDBFF)) {
            i += 12; // a valid pair
        } else {
            i += 2; // any other escape, `\\` included
        }
    }
    if copied == 0 {
        return None;
    }
    out.push_str(&text[copied..]);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::replace_lone_surrogates as fix;

    /// JSON text with `%` standing for a backslash, so the escapes stay escapes in this file.
    fn json(s: &str) -> String {
        s.replace('%', "\\")
    }

    #[test]
    fn lone_surrogates_become_replacement_characters() {
        let case = |input: &str| fix(&json(input));
        assert_eq!(
            case(r#"{"a":"%ude00 hi"}"#),
            Some(json(r#"{"a":"%ufffd hi"}"#))
        );
        assert_eq!(case(r#"{"a":"x%ud83d"}"#), Some(json(r#"{"a":"x%ufffd"}"#)));
        assert_eq!(
            case(r#"{"a":"%ud83d%u0041"}"#),
            Some(json(r#"{"a":"%ufffd%u0041"}"#))
        );
        // A valid pair, an escaped backslash and plain text are left alone.
        assert_eq!(case(r#"{"a":"%ud83d%ude00"}"#), None);
        assert_eq!(case(r#"{"a":"%%ud83d é"}"#), None);
        let fixed = case(r#"{"result":{"value":"%ude00 tail"}}"#).unwrap();
        let v: serde_json::Value = serde_json::from_str(&fixed).unwrap();
        assert_eq!(v["result"]["value"], "\u{FFFD} tail");
    }

    #[test]
    fn tls_connector_builds_without_a_process_default_provider() {
        assert!(matches!(
            super::tls_connector(),
            tokio_tungstenite::Connector::Rustls(_)
        ));
    }

    #[tokio::test]
    async fn call_raw_preserves_cdp_error_code_and_data() {
        use futures_util::{SinkExt, StreamExt};
        use serde_json::json;
        use tokio_tungstenite::tungstenite::Message;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            while let Some(Ok(Message::Text(text))) = ws.next().await {
                let req: serde_json::Value = serde_json::from_str(&text).unwrap();
                let reply = json!({
                    "id": req["id"],
                    "error": { "code": -32602, "message": "Invalid params", "data": "bad" }
                });
                if ws.send(Message::Text(reply.to_string().into())).await.is_err() {
                    break;
                }
            }
        });

        let conn = super::Connection::connect(&format!("ws://{addr}")).await.unwrap();
        let raw = conn
            .call_raw("Bad.method", json!({}), None, std::time::Duration::from_secs(5))
            .await
            .expect("transport ok");
        let err = raw.expect_err("expected CDP error payload");
        assert_eq!(err["code"], -32602);
        assert_eq!(err["message"], "Invalid params");
        assert_eq!(err["data"], "bad");

        let std_err = conn
            .call("Bad.method", json!({}), None, std::time::Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(matches!(std_err, crate::error::Error::Command { .. }));

        conn.close();
        let _ = server.await;
    }

    #[tokio::test]
    async fn session_subscription_routes_own_child_and_browser_wide_events_only() {
        use futures_util::SinkExt;
        use serde_json::json;
        use tokio_tungstenite::tungstenite::Message;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let send_manual = std::sync::Arc::new(tokio::sync::Notify::new());
        let server = tokio::spawn({
            let send_manual = send_manual.clone();
            async move {
                let (stream, _) = listener.accept().await.unwrap();
                let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
                let initial = [
                    json!({"method":"Runtime.consoleAPICalled","params":{"n":1},"sessionId":"s1"}),
                    json!({"method":"Runtime.consoleAPICalled","params":{"n":2},"sessionId":"s2"}),
                    json!({"method":"Browser.downloadWillBegin","params":{"guid":"g1"}}),
                    json!({
                        "method":"Target.attachedToTarget",
                        "params":{
                            "sessionId":"s1-child",
                            "targetInfo":{"type":"iframe","targetId":"f-child"}
                        },
                        "sessionId":"s1"
                    }),
                    json!({"method":"Runtime.consoleAPICalled","params":{"n":3},"sessionId":"s1-child"}),
                ];
                for ev in initial {
                    ws.send(Message::Text(ev.to_string().into())).await.unwrap();
                }
                send_manual.notified().await;
                let manual = json!({"method":"Runtime.consoleAPICalled","params":{"n":4},"sessionId":"s1-manual"});
                ws.send(Message::Text(manual.to_string().into())).await.unwrap();
                let _ = ws.close(None).await;
            }
        });

        let conn = super::Connection::connect(&format!("ws://{addr}")).await.unwrap();
        let mut sub_s1 = conn.subscribe_session("s1");
        let mut sub_s2 = conn.subscribe_session("s2");

        let mut got_s1 = Vec::new();
        for _ in 0..4 {
            let ev = tokio::time::timeout(std::time::Duration::from_secs(2), sub_s1.recv())
                .await
                .unwrap()
                .unwrap();
            got_s1.push((ev.method.clone(), ev.session_id.clone(), ev.params.clone()));
        }
        assert_eq!(got_s1[0].0, "Runtime.consoleAPICalled");
        assert_eq!(got_s1[0].1.as_deref(), Some("s1"));
        assert_eq!(got_s1[0].2["n"], 1);
        assert_eq!(got_s1[1].0, "Browser.downloadWillBegin");
        assert_eq!(got_s1[1].1, None);
        assert_eq!(got_s1[2].0, "Target.attachedToTarget");
        assert_eq!(got_s1[2].1.as_deref(), Some("s1"));
        assert_eq!(got_s1[3].0, "Runtime.consoleAPICalled");
        assert_eq!(got_s1[3].1.as_deref(), Some("s1-child"));
        assert_eq!(got_s1[3].2["n"], 3);

        sub_s1.add_session("s1-manual");
        send_manual.notify_one();

        let manual_ev = tokio::time::timeout(std::time::Duration::from_secs(2), sub_s1.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(manual_ev.session_id.as_deref(), Some("s1-manual"));
        assert_eq!(manual_ev.params["n"], 4);

        let disc_s1 = tokio::time::timeout(std::time::Duration::from_secs(2), sub_s1.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(disc_s1.method, "Fluxwright.disconnected");
        assert!(sub_s1.recv().await.is_none());

        let mut got_s2 = Vec::new();
        while let Some(ev) = sub_s2.recv().await {
            got_s2.push((ev.method.clone(), ev.session_id.clone()));
        }
        assert_eq!(
            got_s2,
            vec![
                ("Runtime.consoleAPICalled".into(), Some("s2".into())),
                ("Browser.downloadWillBegin".into(), None),
                ("Fluxwright.disconnected".into(), None),
            ]
        );
        assert!(conn.is_dead());
        assert!(conn.subscribe_session("s-after-close").recv().await.is_none());

        let _ = server.await;
    }

    #[tokio::test]
    async fn writers_coalesce_pipelined_outgoing_messages() {
        use std::pin::Pin;
        use std::sync::{Arc, Mutex};
        use std::task::{Context, Poll};
        use tokio_tungstenite::tungstenite::Message;

        struct RecordingWsSink {
            ops: Arc<Mutex<Vec<String>>>,
            flushed: Arc<tokio::sync::Notify>,
        }
        impl futures_util::Sink<Message> for RecordingWsSink {
            type Error = std::io::Error;
            fn poll_ready(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
                Poll::Ready(Ok(()))
            }
            fn start_send(self: Pin<&mut Self>, item: Message) -> std::io::Result<()> {
                if let Message::Text(text) = item {
                    self.ops.lock().unwrap().push(format!("feed:{text}"));
                }
                Ok(())
            }
            fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
                self.ops.lock().unwrap().push("flush".into());
                self.flushed.notify_one();
                Poll::Ready(Ok(()))
            }
            fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
                self.ops.lock().unwrap().push("close".into());
                Poll::Ready(Ok(()))
            }
        }

        let ops = Arc::new(Mutex::new(Vec::new()));
        let flushed = Arc::new(tokio::sync::Notify::new());
        let (ws_tx, ws_rx) = tokio::sync::mpsc::unbounded_channel();
        ws_tx.send(super::Outgoing::Json("m1".into())).unwrap();
        ws_tx.send(super::Outgoing::Json("m2".into())).unwrap();
        ws_tx.send(super::Outgoing::Json("m3".into())).unwrap();
        let ws_handle = tokio::spawn(super::run_ws_writer(
            RecordingWsSink { ops: ops.clone(), flushed: flushed.clone() },
            ws_rx,
        ));
        tokio::time::timeout(std::time::Duration::from_secs(2), flushed.notified())
            .await
            .unwrap();
        assert_eq!(
            *ops.lock().unwrap(),
            vec!["feed:m1", "feed:m2", "feed:m3", "flush"]
        );
        ws_tx.send(super::Outgoing::Close).unwrap();
        ws_handle.await.unwrap();
        assert_eq!(
            *ops.lock().unwrap(),
            vec!["feed:m1", "feed:m2", "feed:m3", "flush", "close"]
        );

        struct RecordingPipeWriter {
            writes: Arc<Mutex<Vec<Vec<u8>>>>,
            wrote: Arc<tokio::sync::Notify>,
        }
        impl tokio::io::AsyncWrite for RecordingPipeWriter {
            fn poll_write(
                self: Pin<&mut Self>,
                _cx: &mut Context<'_>,
                buf: &[u8],
            ) -> Poll<std::io::Result<usize>> {
                self.writes.lock().unwrap().push(buf.to_vec());
                self.wrote.notify_one();
                Poll::Ready(Ok(buf.len()))
            }
            fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
                Poll::Ready(Ok(()))
            }
            fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
                Poll::Ready(Ok(()))
            }
        }

        let writes = Arc::new(Mutex::new(Vec::new()));
        let wrote = Arc::new(tokio::sync::Notify::new());
        let (pipe_tx, pipe_rx) = tokio::sync::mpsc::unbounded_channel();
        pipe_tx.send(super::Outgoing::Json("p1".into())).unwrap();
        pipe_tx.send(super::Outgoing::Json("p2".into())).unwrap();
        pipe_tx.send(super::Outgoing::Json("p3".into())).unwrap();
        let pipe_handle = tokio::spawn(super::run_pipe_writer(
            RecordingPipeWriter { writes: writes.clone(), wrote: wrote.clone() },
            pipe_rx,
        ));
        tokio::time::timeout(std::time::Duration::from_secs(2), wrote.notified())
            .await
            .unwrap();
        pipe_tx.send(super::Outgoing::Close).unwrap();
        pipe_handle.await.unwrap();
        let recorded = writes.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0], b"p1\0p2\0p3\0");
    }

    #[test]
    fn parse_cdp_debug_env_recognizes_flags() {
        assert!(super::parse_cdp_debug_env("1"));
        assert!(super::parse_cdp_debug_env("true"));
        assert!(super::parse_cdp_debug_env("CDP"));
        assert!(super::parse_cdp_debug_env("http, cdp"));
        assert!(super::parse_cdp_debug_env("all"));
        assert!(!super::parse_cdp_debug_env("0"));
        assert!(!super::parse_cdp_debug_env(""));
        assert!(!super::parse_cdp_debug_env("http"));
    }
}
