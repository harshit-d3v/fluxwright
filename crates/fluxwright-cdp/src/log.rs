//! Console messages, uncaught errors and downloads from a page, its popups and its iframes,
//! kept so a job can check them (Playwright's `page.consoleMessages()`, `page.pageErrors()` and
//! download events).

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::Serialize;
use serde_json::Value;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::connection::{CdpEvent, Connection};
use crate::events::{BodySource, Loaded, NetEvent, NetRequest, NetResponse};

/// Per kind; older entries are dropped first.
const KEEP: usize = 1000;

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
}

fn keep<T>(q: &mut VecDeque<T>, v: T) {
    if q.len() == KEEP {
        q.pop_front();
    }
    q.push_back(v);
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

fn page_error(details: &Value) -> PageError {
    let ex = &details["exception"];
    let stack = ex["description"]
        .as_str()
        .map(str::to_owned)
        .or_else(|| ex.get("value").map(|_| arg_text(ex)))
        .unwrap_or_else(|| details["text"].as_str().unwrap_or("Uncaught").to_string());
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
/// the page and every session that later attaches in its context (popups, iframes in other
/// processes).
pub(crate) async fn collect(
    mut events: UnboundedReceiver<CdpEvent>,
    watch: Watch,
    log: Arc<Mutex<PageLog>>,
    downloads: UnboundedSender<DownloadResult>,
    conn: Arc<Connection>,
) {
    let context = watch.context;
    let mut sessions = HashSet::from([watch.session]);
    // Download events are browser-wide; the frame that started one tells whose it is.
    let mut frames = HashSet::from([watch.main_frame]);
    let mut started: HashMap<String, (String, String)> = HashMap::new();
    // Requests in flight, by session and request id, with the sender that marks their body
    // loaded once a response arrived.
    let mut requests: InFlight = HashMap::new();
    while let Some(e) = events.recv().await {
        let ours = e.session_id.as_ref().is_some_and(|s| sessions.contains(s));
        let browser_wide = e.session_id.is_none();
        let str_of = |k: &str| e.params[k].as_str().unwrap_or("").to_string();
        match e.method.as_str() {
            "Target.attachedToTarget" if e.params["targetInfo"]["browserContextId"] == context.as_str() => {
                if let Some(s) = e.params["sessionId"].as_str() {
                    sessions.insert(s.to_string());
                }
                if let Some(t) = e.params["targetInfo"]["targetId"].as_str() {
                    frames.insert(t.to_string());
                }
            }
            "Page.frameAttached" if ours => {
                frames.insert(str_of("frameId"));
            }
            "Browser.downloadWillBegin" if browser_wide && frames.contains(&str_of("frameId")) => {
                started.insert(str_of("guid"), (str_of("url"), str_of("suggestedFilename")));
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
                let text = e.params["args"].as_array().map(|a| a.iter().map(arg_text).collect::<Vec<_>>().join(" "));
                let msg = ConsoleMessage {
                    kind: e.params["type"].as_str().unwrap_or("log").to_string(),
                    text: text.unwrap_or_default(),
                };
                let mut log = log.lock().unwrap();
                log.tell(LogEntry::Console(msg.clone()));
                keep(&mut log.console, msg);
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
                        requests.insert(key, (req.clone(), None));
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
}
