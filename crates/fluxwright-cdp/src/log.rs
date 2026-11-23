//! Console messages, uncaught errors and downloads from a page and its iframes,
//! kept so a job can check them (Playwright's `page.consoleMessages()`, `page.pageErrors()` and
//! download events).

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::Serialize;
use serde_json::Value;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::connection::{CdpEvent, Connection, SessionRouter, SessionSubscription};
use crate::events::{decode_ws_frame, BodySource, Loaded, NetEvent, NetRequest, NetResponse, WebSocketEvent};

/// Per kind; older entries are dropped first.
const KEEP: usize = 1000;

/// Longest console text or error kept, in bytes. A page logging megabyte strings in a loop
/// otherwise made each page hold a gigabyte (KEEP of them) and every listener's queue grow.
const MAX_TEXT: usize = 16 * 1024;

/// `s` cut to [`MAX_TEXT`] bytes on a character boundary, marked with `…` when cut.
fn capped(mut s: String) -> String {
    if s.len() > MAX_TEXT {
        let mut cut = MAX_TEXT;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        s.truncate(cut);
        s.push('…');
    }
    s
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ConsoleMessage {
    /// `log`, `error`, `warning`, `info`, `debug`, ..., as Playwright's `type()` reports them.
    pub kind: String,
    pub text: String,
}

/// An exception nothing caught.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PageError {
    /// `TypeError`, `Error`, ...; empty when something other than an Error was thrown.
    pub name: String,
    pub message: String,
    pub stack: String,
}

/// A file the page downloaded, under a temporary name until the job's page closes.
#[derive(Debug, Clone, PartialEq)]
pub struct Download {
    pub url: String,
    /// The name the site suggested, from `Content-Disposition` or the URL.
    pub suggested_filename: String,
    /// Deleted when the page closes; keep it with [`save_as`](Self::save_as) first.
    pub path: PathBuf,
}

impl Download {
    /// Copies the file to `dest`, creating missing folders, as Playwright's `saveAs` does.
    pub fn save_as(&self, dest: impl AsRef<Path>) -> std::io::Result<()> {
        if let Some(dir) = dest.as_ref().parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::copy(&self.path, dest).map(|_| ())
    }
}

/// A finished download, or why it did not finish.
pub type DownloadResult = std::result::Result<Download, String>;

/// One console call or uncaught error, as it happens.
#[derive(Debug, Clone, PartialEq)]
pub enum LogEntry {
    Console(ConsoleMessage),
    Error(PageError),
}

#[derive(Debug, Default)]
pub struct PageLog {
    pub console: VecDeque<ConsoleMessage>,
    pub errors: VecDeque<PageError>,
    /// One queue per listener, so a burst of output never drops an entry.
    live: Vec<UnboundedSender<LogEntry>>,
    /// The same for network events; requests are tracked only while one is listening.
    net_live: Vec<UnboundedSender<NetEvent>>,
    /// WebSocket lifecycle and frame events.
    ws_live: Vec<UnboundedSender<WebSocketEvent>>,
}

impl PageLog {
    /// Every entry from now on, for Playwright's `page.on('console' | 'pageerror')`.
    pub fn subscribe(&mut self) -> UnboundedReceiver<LogEntry> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        self.live.push(tx);
        rx
    }

    fn tell(&mut self, entry: LogEntry) {
        self.live.retain(|tx| tx.send(entry.clone()).is_ok());
    }

    /// Every network event from now on, for `page.on('request' | 'response' | ...)`.
    pub fn subscribe_network(&mut self) -> UnboundedReceiver<NetEvent> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        self.net_live.push(tx);
        rx
    }

    fn tell_net(&mut self, event: NetEvent) {
        self.net_live.retain(|tx| tx.send(event.clone()).is_ok());
    }

    /// Every WebSocket lifecycle and frame event from now on, for `page.on('websocket')`.
    pub fn subscribe_websockets(&mut self) -> UnboundedReceiver<WebSocketEvent> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        self.ws_live.push(tx);
        rx
    }

    fn tell_ws(&mut self, event: WebSocketEvent) {
        self.ws_live.retain(|tx| tx.send(event.clone()).is_ok());
    }
}

fn keep<T>(q: &mut VecDeque<T>, v: T) {
    if q.len() == KEEP {
        q.pop_front();
    }
    q.push_back(v);
}

fn insert_bounded<K: Eq + std::hash::Hash + Clone, V>(
    map: &mut HashMap<K, V>,
    order: &mut VecDeque<K>,
    key: K,
    val: V,
) {
    if !map.contains_key(&key) {
        while map.len() >= KEEP {
            let Some(old) = order.pop_front() else { break };
            map.remove(&old);
        }
        if order.len() >= KEEP * 2 {
            order.retain(|k| map.contains_key(k));
        }
        // A key removed from the map and inserted again (a redirect reuses its request id)
        // still has its old place: drop it, or evicting that copy would evict the new entry.
        if let Some(stale) = order.iter().position(|k| k == &key) {
            order.remove(stale);
        }
        order.push_back(key.clone());
    }
    map.insert(key, val);
}

/// How a console argument reads: strings as they are, other values as Chrome describes them.
fn arg_text(o: &Value) -> String {
    match o.get("value") {
        Some(Value::String(s)) => s.clone(),
        Some(v) => v.to_string(),
        None => o["unserializableValue"]
            .as_str()
            .or_else(|| o["description"].as_str())
            .or_else(|| o["type"].as_str())
            .unwrap_or("")
            .to_string(),
    }
}

pub(crate) fn console_message_from_cdp(params: &Value) -> ConsoleMessage {
    let text = params["args"]
        .as_array()
        .map(|a| a.iter().map(arg_text).collect::<Vec<_>>().join(" "));
    ConsoleMessage {
        kind: params["type"].as_str().unwrap_or("log").to_string(),
        text: capped(text.unwrap_or_default()),
    }
}

fn page_error(details: &Value) -> PageError {
    let ex = &details["exception"];
    let stack = capped(
        ex["description"]
            .as_str()
            .map(str::to_owned)
            .or_else(|| ex.get("value").map(|_| arg_text(ex)))
            .unwrap_or_else(|| details["text"].as_str().unwrap_or("Uncaught").to_string()),
    );
    if ex["subtype"] != "error" {
        return PageError { name: String::new(), message: stack.clone(), stack };
    }
    // "TypeError: x is not a function\n    at ...": the first line is "name: message".
    let first = stack.lines().next().unwrap_or("").to_string();
    let (name, message) = match first.split_once(": ") {
        Some((n, m)) => (n.to_string(), m.to_string()),
        None => (first, String::new()),
    };
    PageError { name, message, stack }
}

/// Requests in flight, by session and request id, with the sender that marks their body loaded.
type InFlight = HashMap<(String, String), (NetRequest, Option<tokio::sync::watch::Sender<Loaded>>)>;

/// What the collector watches: the page's first session and main frame, its context, and
/// where its downloads land.
pub(crate) struct Watch {
    pub(crate) session: String,
    pub(crate) main_frame: String,
    pub(crate) context: String,
    pub(crate) download_dir: PathBuf,
}

/// Runs until aborted: records console calls, uncaught exceptions and finished downloads from
/// the page and every child target session (iframes in other processes, workers) that attaches
/// under it. Separate top-level popup page targets get their own `CdpPage` and `PageLog`.
pub(crate) async fn collect(
    events: UnboundedReceiver<Arc<CdpEvent>>,
    watch: Watch,
    log: Arc<Mutex<PageLog>>,
    downloads: UnboundedSender<DownloadResult>,
    conn: Arc<Connection>,
) {
    collect_routed(events, None, watch, log, downloads, conn).await;
}

/// Session-routed variant of [`collect`] that dynamically registers child iframe/worker sessions
/// with the connection's session router as they attach and removes them as they detach.
pub(crate) async fn collect_session(
    sub: SessionSubscription,
    watch: Watch,
    log: Arc<Mutex<PageLog>>,
    downloads: UnboundedSender<DownloadResult>,
    conn: Arc<Connection>,
) {
    let (events, router) = sub.into_parts();
    collect_routed(events, Some(router), watch, log, downloads, conn).await;
}

async fn collect_routed(
    mut events: UnboundedReceiver<Arc<CdpEvent>>,
    router: Option<SessionRouter>,
    watch: Watch,
    log: Arc<Mutex<PageLog>>,
    downloads: UnboundedSender<DownloadResult>,
    conn: Arc<Connection>,
) {
    let context = watch.context;
    let main_frame = watch.main_frame;
    let mut sessions = HashSet::from([watch.session]);
    // Download events are browser-wide; the frame that started one tells whose it is.
    let mut frames = HashSet::from([main_frame]);
    let mut started: HashMap<String, (String, String)> = HashMap::new();
    let mut started_order: VecDeque<String> = VecDeque::new();
    // Requests in flight, by session and request id, with the sender that marks their body
    // loaded once a response arrived.
    let mut requests: InFlight = HashMap::new();
    let mut request_order: VecDeque<(String, String)> = VecDeque::new();
    while let Some(e) = events.recv().await {
        let ours = e.session_id.as_ref().is_some_and(|s| sessions.contains(s));
        let browser_wide = e.session_id.is_none();
        let str_of = |k: &str| e.params[k].as_str().unwrap_or("").to_string();
        match e.method.as_str() {
            "Target.attachedToTarget" => {
                let ctx_matches = e.params["targetInfo"]["browserContextId"]
                    .as_str()
                    .is_none_or(|c| c == context.as_str());
                let ty = e.params["targetInfo"]["type"].as_str().unwrap_or("");
                let is_child_target = ctx_matches
                    && matches!(ty, "iframe" | "worker")
                    && e.session_id.as_ref().is_some_and(|s| sessions.contains(s));
                if is_child_target {
                    if let Some(s) = e.params["sessionId"].as_str() {
                        sessions.insert(s.to_string());
                        if let Some(ref router) = router {
                            router.add_session(s);
                        }
                    }
                    if ty == "iframe" {
                        if let Some(t) = e.params["targetInfo"]["targetId"].as_str() {
                            frames.insert(t.to_string());
                        }
                    }
                } else if !ctx_matches && matches!(ty, "iframe" | "worker") {
                    if let Some(s) = e.params["sessionId"].as_str() {
                        if let Some(ref router) = router {
                            router.remove_session(s);
                        }
                    }
                }
            }
            "Page.frameAttached" if ours => {
                frames.insert(str_of("frameId"));
            }
            // A "swap" moves the frame to its own process: it comes back as a target.
            "Page.frameDetached" if ours && e.params["reason"] != "swap" => {
                frames.remove(&str_of("frameId"));
            }
            "Target.detachedFromTarget" => {
                let sid = str_of("sessionId");
                if sessions.remove(&sid) {
                    frames.remove(&str_of("targetId"));
                    if let Some(ref router) = router {
                        router.remove_session(&sid);
                    }
                }
            }
            "Browser.downloadWillBegin" if browser_wide && frames.contains(&str_of("frameId")) => {
                insert_bounded(
                    &mut started,
                    &mut started_order,
                    str_of("guid"),
                    (str_of("url"), str_of("suggestedFilename")),
                );
            }
            "Browser.downloadProgress" if browser_wide => match e.params["state"].as_str() {
                Some("completed") => {
                    if let Some((url, name)) = started.remove(&str_of("guid")) {
                        let path = watch.download_dir.join(str_of("guid"));
                        let _ = downloads.send(Ok(Download { url, suggested_filename: name, path }));
                    }
                }
                Some("canceled") => {
                    if let Some((url, _)) = started.remove(&str_of("guid")) {
                        let _ = downloads.send(Err(format!("the download of {url} was canceled")));
                    }
                }
                _ => {}
            },
            "Runtime.consoleAPICalled" if ours => {
                let msg = console_message_from_cdp(&e.params);
                let mut log = log.lock().unwrap();
                log.tell(LogEntry::Console(msg.clone()));
                keep(&mut log.console, msg);
            }
            "Network.webSocketCreated"
            | "Network.webSocketFrameSent"
            | "Network.webSocketFrameReceived"
            | "Network.webSocketFrameError"
            | "Network.webSocketClosed"
                if ours =>
            {
                let mut log = log.lock().unwrap();
                if log.ws_live.is_empty() {
                    continue;
                }
                let sid = e.session_id.as_deref().unwrap_or("");
                let request_id = format!("{sid}:{}", str_of("requestId"));
                match e.method.as_str() {
                    "Network.webSocketCreated" => {
                        log.tell_ws(WebSocketEvent::Created {
                            request_id,
                            url: str_of("url"),
                        });
                    }
                    "Network.webSocketFrameSent" => {
                        let (opcode, payload, is_text) = decode_ws_frame(&e.params);
                        log.tell_ws(WebSocketEvent::FrameSent {
                            request_id,
                            opcode,
                            payload,
                            is_text,
                        });
                    }
                    "Network.webSocketFrameReceived" => {
                        let (opcode, payload, is_text) = decode_ws_frame(&e.params);
                        log.tell_ws(WebSocketEvent::FrameReceived {
                            request_id,
                            opcode,
                            payload,
                            is_text,
                        });
                    }
                    "Network.webSocketFrameError" => {
                        log.tell_ws(WebSocketEvent::Error {
                            request_id,
                            error_message: str_of("errorMessage"),
                        });
                    }
                    _ => {
                        log.tell_ws(WebSocketEvent::Closed { request_id });
                    }
                }
            }
            "Network.requestWillBeSent" | "Network.responseReceived" | "Network.loadingFinished" | "Network.loadingFailed"
                if ours =>
            {
                let mut log = log.lock().unwrap();
                if log.net_live.is_empty() {
                    continue;
                }
                let sid = e.session_id.clone().unwrap_or_default();
                let key = (sid.clone(), str_of("requestId"));
                match e.method.as_str() {
                    "Network.requestWillBeSent" => {
                        // A redirect reuses the request id: the previous hop got its response.
                        if e.params["redirectResponse"].is_object() {
                            if let Some((prev, _)) = requests.remove(&key) {
                                let (_, done) = tokio::sync::watch::channel(Some(Ok(())));
                                let body = BodySource { conn: conn.clone(), session: sid, request_id: key.1.clone(), done };
                                let redirect = serde_json::json!({ "response": e.params["redirectResponse"] });
                                log.tell_net(NetEvent::Response(NetResponse::from_received(prev.clone(), &redirect, body)));
                                log.tell_net(NetEvent::Finished(prev));
                            }
                        }
                        let req = NetRequest::from_sent(&e.params);
                        insert_bounded(&mut requests, &mut request_order, key, (req.clone(), None));
                        log.tell_net(NetEvent::Request(req));
                    }
                    "Network.responseReceived" => {
                        if let Some((req, loaded)) = requests.get_mut(&key) {
                            let (tx, done) = tokio::sync::watch::channel(None);
                            *loaded = Some(tx);
                            let body = BodySource { conn: conn.clone(), session: sid, request_id: key.1.clone(), done };
                            log.tell_net(NetEvent::Response(NetResponse::from_received(req.clone(), &e.params, body)));
                        }
                    }
                    "Network.loadingFinished" => {
                        if let Some((req, loaded)) = requests.remove(&key) {
                            if let Some(tx) = loaded {
                                let _ = tx.send(Some(Ok(())));
                            }
                            log.tell_net(NetEvent::Finished(req));
                        }
                    }
                    _ => {
                        if let Some((req, loaded)) = requests.remove(&key) {
                            let reason = str_of("errorText");
                            if let Some(tx) = loaded {
                                let _ = tx.send(Some(Err(reason.clone())));
                            }
                            log.tell_net(NetEvent::Failed(req, reason));
                        }
                    }
                }
            }
            "Runtime.exceptionThrown" if ours => {
                let err = page_error(&e.params["exceptionDetails"]);
                let mut log = log.lock().unwrap();
                log.tell(LogEntry::Error(err.clone()));
                keep(&mut log.errors, err);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn console_arguments_read_like_the_devtools_console() {
        let args = [
            json!({ "type": "string", "value": "count" }),
            json!({ "type": "number", "value": 3 }),
            json!({ "type": "object", "subtype": "null", "value": null }),
            json!({ "type": "undefined" }),
            json!({ "type": "object", "description": "Object" }),
        ];
        let text: Vec<String> = args.iter().map(arg_text).collect();
        assert_eq!(text.join(" "), "count 3 null undefined Object");
    }

    #[test]
    fn errors_split_into_name_and_message() {
        let e = page_error(&json!({ "exception": { "subtype": "error",
            "description": "TypeError: x is not a function\n    at <anonymous>:1:1" } }));
        assert_eq!((e.name.as_str(), e.message.as_str()), ("TypeError", "x is not a function"));
        let thrown = page_error(&json!({ "exception": { "type": "string", "value": "boom" } }));
        assert_eq!((thrown.name.as_str(), thrown.message.as_str()), ("", "boom"));
    }

    #[test]
    fn huge_console_text_and_errors_are_capped() {
        // 3-byte characters: the cut lands mid-character and must step back.
        let cut = capped("€".repeat(MAX_TEXT));
        assert!(cut.len() <= MAX_TEXT + '…'.len_utf8() && cut.ends_with('…'), "{}", cut.len());
        assert_eq!(capped("short".into()), "short");
        let e = page_error(&json!({ "exception": { "subtype": "error",
            "description": format!("Error: {}", "x".repeat(10 * MAX_TEXT)) } }));
        assert!(e.stack.len() <= MAX_TEXT + 3 && e.message.len() <= MAX_TEXT + 3);
    }

    #[test]
    fn reinserted_keys_keep_one_place_in_order() {
        let (mut map, mut order) = (HashMap::new(), VecDeque::new());
        insert_bounded(&mut map, &mut order, "a", 1);
        map.remove("a");
        insert_bounded(&mut map, &mut order, "a", 2);
        insert_bounded(&mut map, &mut order, "a", 3);
        assert_eq!(order, ["a"]);
        assert_eq!(map["a"], 3);
    }

    #[tokio::test]
    async fn collect_caps_in_flight_requests_and_started_at_keep() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                let _ = tokio_tungstenite::accept_async(stream).await;
            }
        });
        let conn = Connection::connect(&format!("ws://{addr}")).await.unwrap();

        let (events_tx, events_rx) = tokio::sync::mpsc::unbounded_channel();
        let log = Arc::new(Mutex::new(PageLog::default()));
        let mut net_rx = log.lock().unwrap().subscribe_network();
        let (dl_tx, mut dl_rx) = tokio::sync::mpsc::unbounded_channel();

        let watch = Watch {
            session: "s1".into(),
            main_frame: "f1".into(),
            context: "c1".into(),
            download_dir: PathBuf::from("downloads"),
        };
        let handle = tokio::spawn(collect(events_rx, watch, log, dl_tx, conn));

        for i in 0..(KEEP + 5) {
            events_tx
                .send(Arc::new(CdpEvent {
                    method: "Network.requestWillBeSent".into(),
                    params: json!({
                        "requestId": format!("req-{i}"),
                        "request": { "url": format!("https://example.test/{i}"), "method": "GET", "headers": {} },
                        "type": "Fetch"
                    }),
                    session_id: Some("s1".into()),
                }))
                .unwrap();
            events_tx
                .send(Arc::new(CdpEvent {
                    method: "Browser.downloadWillBegin".into(),
                    params: json!({
                        "frameId": "f1",
                        "guid": format!("dl-{i}"),
                        "url": format!("https://example.test/dl/{i}"),
                        "suggestedFilename": format!("file-{i}.bin")
                    }),
                    session_id: None,
                }))
                .unwrap();
        }

        // req-0 and dl-0 were evicted once 1005 entries arrived; req-5 and dl-5 are the oldest kept.
        for id in ["req-0", "req-5"] {
            events_tx
                .send(Arc::new(CdpEvent {
                    method: "Network.loadingFinished".into(),
                    params: json!({ "requestId": id }),
                    session_id: Some("s1".into()),
                }))
                .unwrap();
        }
        for guid in ["dl-0", "dl-5"] {
            events_tx
                .send(Arc::new(CdpEvent {
                    method: "Browser.downloadProgress".into(),
                    params: json!({ "guid": guid, "state": "completed" }),
                    session_id: None,
                }))
                .unwrap();
        }

        drop(events_tx);
        handle.await.unwrap();

        let mut finished_urls = Vec::new();
        while let Ok(ev) = net_rx.try_recv() {
            if let NetEvent::Finished(req) = ev {
                finished_urls.push(req.url);
            }
        }
        assert_eq!(finished_urls, vec!["https://example.test/5"]);

        let mut finished_downloads = Vec::new();
        while let Ok(Ok(dl)) = dl_rx.try_recv() {
            finished_downloads.push(dl.suggested_filename);
        }
        assert_eq!(finished_downloads, vec!["file-5.bin"]);
    }

    #[tokio::test]
    async fn collect_emits_websocket_text_binary_error_and_close() {
        use base64::Engine;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                let _ = tokio_tungstenite::accept_async(stream).await;
            }
        });
        let conn = Connection::connect(&format!("ws://{addr}")).await.unwrap();

        let (events_tx, events_rx) = tokio::sync::mpsc::unbounded_channel();
        let log = Arc::new(Mutex::new(PageLog::default()));
        let mut ws_rx = log.lock().unwrap().subscribe_websockets();
        let (dl_tx, _dl_rx) = tokio::sync::mpsc::unbounded_channel();
        let watch = Watch {
            session: "s1".into(),
            main_frame: "f1".into(),
            context: "c1".into(),
            download_dir: PathBuf::from("downloads"),
        };
        let handle = tokio::spawn(collect(events_rx, watch, log, dl_tx, conn));

        let send = |method: &str, params: Value| {
            events_tx
                .send(Arc::new(CdpEvent {
                    method: method.into(),
                    params,
                    session_id: Some("s1".into()),
                }))
                .unwrap();
        };

        send("Network.webSocketCreated", json!({ "requestId": "ws1", "url": "ws://127.0.0.1/ws" }));
        send("Network.webSocketFrameSent", json!({ "requestId": "ws1", "response": { "opcode": 1, "payloadData": "hello" } }));
        let bin_b64 = base64::engine::general_purpose::STANDARD.encode([1u8, 2, 3, 255]);
        send("Network.webSocketFrameReceived", json!({ "requestId": "ws1", "response": { "opcode": 2, "payloadData": bin_b64 } }));
        send("Network.webSocketFrameError", json!({ "requestId": "ws1", "errorMessage": "reset" }));
        send("Network.webSocketClosed", json!({ "requestId": "ws1" }));

        drop(events_tx);
        handle.await.unwrap();

        let mut received = Vec::new();
        while let Ok(ev) = ws_rx.try_recv() {
            received.push(ev);
        }
        assert_eq!(
            received,
            vec![
                WebSocketEvent::Created { request_id: "s1:ws1".into(), url: "ws://127.0.0.1/ws".into() },
                WebSocketEvent::FrameSent { request_id: "s1:ws1".into(), opcode: 1, payload: b"hello".to_vec(), is_text: true },
                WebSocketEvent::FrameReceived { request_id: "s1:ws1".into(), opcode: 2, payload: vec![1, 2, 3, 255], is_text: false },
                WebSocketEvent::Error { request_id: "s1:ws1".into(), error_message: "reset".into() },
                WebSocketEvent::Closed { request_id: "s1:ws1".into() },
            ]
        );
    }

    #[tokio::test]
    async fn collect_isolates_popup_pages_while_tracking_child_iframes_and_workers() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                let _ = tokio_tungstenite::accept_async(stream).await;
            }
        });
        let conn = Connection::connect(&format!("ws://{addr}")).await.unwrap();

        let (events_tx, events_rx) = tokio::sync::mpsc::unbounded_channel();
        let log = Arc::new(Mutex::new(PageLog::default()));
        let mut net_rx = log.lock().unwrap().subscribe_network();
        let (dl_tx, _dl_rx) = tokio::sync::mpsc::unbounded_channel();
        let watch = Watch {
            session: "s-opener".into(),
            main_frame: "t-opener".into(),
            context: "c1".into(),
            download_dir: PathBuf::from("downloads"),
        };
        let handle = tokio::spawn(collect(events_rx, watch, log.clone(), dl_tx, conn));

        // Popup page target with openerId == our main_frame must NOT be recorded on opener.
        events_tx
            .send(Arc::new(CdpEvent {
                method: "Target.attachedToTarget".into(),
                params: json!({
                    "sessionId": "s-popup",
                    "targetInfo": {
                        "targetId": "t-popup",
                        "type": "page",
                        "openerId": "t-opener",
                        "browserContextId": "c1"
                    }
                }),
                session_id: None,
            }))
            .unwrap();
        // Child OOPIF iframe under opener session MUST be recorded.
        events_tx
            .send(Arc::new(CdpEvent {
                method: "Target.attachedToTarget".into(),
                params: json!({
                    "sessionId": "s-iframe",
                    "targetInfo": {
                        "targetId": "t-iframe",
                        "type": "iframe",
                        "browserContextId": "c1"
                    }
                }),
                session_id: Some("s-opener".into()),
            }))
            .unwrap();
        // Dedicated worker under opener session MUST be recorded.
        events_tx
            .send(Arc::new(CdpEvent {
                method: "Target.attachedToTarget".into(),
                params: json!({
                    "sessionId": "s-worker",
                    "targetInfo": {
                        "targetId": "t-worker",
                        "type": "worker"
                    }
                }),
                session_id: Some("s-opener".into()),
            }))
            .unwrap();

        for (sid, msg) in [
            ("s-opener", "from-opener"),
            ("s-popup", "from-popup"),
            ("s-iframe", "from-iframe"),
            ("s-worker", "from-worker"),
        ] {
            events_tx
                .send(Arc::new(CdpEvent {
                    method: "Runtime.consoleAPICalled".into(),
                    params: json!({
                        "type": "log",
                        "args": [{ "type": "string", "value": msg }]
                    }),
                    session_id: Some(sid.into()),
                }))
                .unwrap();
        }
        events_tx
            .send(Arc::new(CdpEvent {
                method: "Runtime.exceptionThrown".into(),
                params: json!({
                    "exceptionDetails": {
                        "exception": { "subtype": "error", "description": "Error: popup-boom" }
                    }
                }),
                session_id: Some("s-popup".into()),
            }))
            .unwrap();
        for (sid, url) in [
            ("s-opener", "https://example.test/opener"),
            ("s-popup", "https://example.test/popup"),
            ("s-iframe", "https://example.test/iframe"),
        ] {
            events_tx
                .send(Arc::new(CdpEvent {
                    method: "Network.requestWillBeSent".into(),
                    params: json!({
                        "requestId": format!("r-{sid}"),
                        "request": { "url": url, "method": "GET", "headers": {} },
                        "type": "Document"
                    }),
                    session_id: Some(sid.into()),
                }))
                .unwrap();
        }

        drop(events_tx);
        handle.await.unwrap();

        let texts: Vec<String> = log
            .lock()
            .unwrap()
            .console
            .iter()
            .map(|m| m.text.clone())
            .collect();
        assert_eq!(texts, vec!["from-opener", "from-iframe", "from-worker"]);
        assert!(log.lock().unwrap().errors.is_empty());

        let mut net_urls = Vec::new();
        while let Ok(ev) = net_rx.try_recv() {
            if let NetEvent::Request(req) = ev {
                net_urls.push(req.url);
            }
        }
        assert_eq!(
            net_urls,
            vec!["https://example.test/opener", "https://example.test/iframe"]
        );
    }

    #[tokio::test]
    async fn collect_session_receives_child_and_browser_wide_events_skips_unrelated() {
        use futures_util::SinkExt;
        use tokio_tungstenite::tungstenite::Message;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let events = [
                json!({
                    "method": "Runtime.consoleAPICalled",
                    "params": { "type": "log", "args": [{ "type": "string", "value": "from-page" }] },
                    "sessionId": "s-page"
                }),
                json!({
                    "method": "Runtime.consoleAPICalled",
                    "params": { "type": "log", "args": [{ "type": "string", "value": "from-other" }] },
                    "sessionId": "s-other"
                }),
                json!({
                    "method": "Target.attachedToTarget",
                    "params": {
                        "sessionId": "s-iframe",
                        "targetInfo": { "targetId": "f-iframe", "type": "iframe", "browserContextId": "c1" }
                    },
                    "sessionId": "s-page"
                }),
                json!({
                    "method": "Runtime.consoleAPICalled",
                    "params": { "type": "log", "args": [{ "type": "string", "value": "from-iframe" }] },
                    "sessionId": "s-iframe"
                }),
                json!({
                    "method": "Browser.downloadWillBegin",
                    "params": {
                        "frameId": "f-main",
                        "guid": "dl-1",
                        "url": "https://example.test/dl",
                        "suggestedFilename": "f.bin"
                    }
                }),
                json!({
                    "method": "Browser.downloadProgress",
                    "params": { "guid": "dl-1", "state": "completed" }
                }),
            ];
            for ev in events {
                ws.send(Message::Text(ev.to_string().into())).await.unwrap();
            }
            let _ = ws.close(None).await;
        });

        let conn = Connection::connect(&format!("ws://{addr}")).await.unwrap();
        let sub = conn.subscribe_session("s-page");
        let log = Arc::new(Mutex::new(PageLog::default()));
        let (dl_tx, mut dl_rx) = tokio::sync::mpsc::unbounded_channel();
        let watch = Watch {
            session: "s-page".into(),
            main_frame: "f-main".into(),
            context: "c1".into(),
            download_dir: PathBuf::from("downloads"),
        };
        let handle = tokio::spawn(collect_session(sub, watch, log.clone(), dl_tx, conn));
        tokio::time::timeout(std::time::Duration::from_secs(5), handle)
            .await
            .unwrap()
            .unwrap();
        let _ = server.await;

        let texts: Vec<String> = log
            .lock()
            .unwrap()
            .console
            .iter()
            .map(|m| m.text.clone())
            .collect();
        assert_eq!(texts, vec!["from-page", "from-iframe"]);
        let dl = dl_rx.try_recv().unwrap().unwrap();
        assert_eq!(dl.suggested_filename, "f.bin");
    }
}
