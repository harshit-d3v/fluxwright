//! HAR 1.2 recording and offline route replay (`recordHar` / `routeFromHAR`).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::connection::{CdpEvent, Connection};
use crate::error::{Error, Result};
use crate::page::url_glob_matches;
use crate::route::Fulfill;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HarMode {
    #[default]
    Full,
    Minimal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HarContentPolicy {
    #[default]
    Embed,
    Omit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HarNotFound {
    #[default]
    Abort,
    Fallback,
}

/// Configuration for HAR recording (`recordHar`).
#[derive(Debug, Clone)]
pub struct RecordHarOptions {
    pub path: PathBuf,
    pub omit_content: bool,
    pub content: Option<HarContentPolicy>,
    pub mode: HarMode,
    pub url_filter: Option<String>,
}

impl RecordHarOptions {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            omit_content: false,
            content: None,
            mode: HarMode::Full,
            url_filter: None,
        }
    }

    pub fn effective_content_policy(&self) -> HarContentPolicy {
        self.content.unwrap_or(if self.omit_content {
            HarContentPolicy::Omit
        } else {
            HarContentPolicy::Embed
        })
    }
}

/// Configuration for HAR replay or update (`routeFromHAR`).
#[derive(Debug, Clone, Default)]
pub struct HarRouteOptions {
    pub url: Option<String>,
    pub not_found: HarNotFound,
    pub update: bool,
    pub update_mode: HarMode,
    pub update_content: HarContentPolicy,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Har {
    pub log: HarLog,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct HarLog {
    pub version: String,
    pub creator: HarCreator,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub browser: Option<HarBrowser>,
    #[serde(default)]
    pub entries: Vec<HarEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HarCreator {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HarBrowser {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct HarEntry {
    pub started_date_time: String,
    pub time: f64,
    pub request: HarRequest,
    pub response: HarResponse,
    #[serde(default)]
    pub cache: Value,
    pub timings: HarTimings,
    #[serde(rename = "serverIPAddress", default, skip_serializing_if = "Option::is_none")]
    pub server_ip_address: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct HarRequest {
    pub method: String,
    pub url: String,
    pub http_version: String,
    #[serde(default)]
    pub headers: Vec<HarHeader>,
    #[serde(default)]
    pub query_string: Vec<HarQueryParam>,
    #[serde(default)]
    pub cookies: Vec<Value>,
    pub headers_size: i64,
    pub body_size: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub post_data: Option<HarPostData>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct HarResponse {
    pub status: u16,
    pub status_text: String,
    pub http_version: String,
    #[serde(default)]
    pub headers: Vec<HarHeader>,
    #[serde(default)]
    pub cookies: Vec<Value>,
    pub content: HarContent,
    #[serde(rename = "redirectURL")]
    pub redirect_url: String,
    pub headers_size: i64,
    pub body_size: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HarHeader {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HarQueryParam {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct HarPostData {
    pub mime_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct HarContent {
    pub size: i64,
    pub mime_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoding: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HarTimings {
    pub send: f64,
    pub wait: f64,
    pub receive: f64,
}

impl Har {
    pub fn from_json_str(json: &str) -> Result<Self> {
        serde_json::from_str(json).map_err(|e| Error::Other(format!("invalid HAR JSON: {e}")))
    }

    pub fn from_file(path: impl AsRef<Path>) -> Result<Self> {
        let data = std::fs::read_to_string(path.as_ref())
            .map_err(|e| Error::Other(format!("read HAR {}: {e}", path.as_ref().display())))?;
        Self::from_json_str(&data)
    }

    /// Saves the HAR JSON to `path`, creating missing parent directories and restricting file
    /// permissions to the owner on Unix.
    pub fn save(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?;
        }
        let payload = serde_json::to_vec_pretty(self)
            .map_err(|e| Error::Other(format!("serialize HAR: {e}")))?;
        write_private_bytes(path, &payload)
    }
}

fn write_private_bytes(path: &Path, data: &[u8]) -> Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        file.write_all(data)?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, data)?;
        Ok(())
    }
}

/// Formats a UTC `SystemTime` as `YYYY-MM-DDTHH:MM:SS.mmmZ` using Hinnant's civil-from-days math.
pub(crate) fn format_iso8601_utc(time: SystemTime) -> String {
    let dur = time.duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO);
    let total_secs = dur.as_secs() as i64;
    let millis = dur.subsec_millis();
    let days = total_secs.div_euclid(86_400);
    let secs_of_day = total_secs.rem_euclid(86_400);

    let hour = secs_of_day / 3_600;
    let minute = (secs_of_day % 3_600) / 60;
    let second = secs_of_day % 60;

    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = y + if m <= 2 { 1 } else { 0 };

    format!("{year:04}-{m:02}-{d:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

pub(crate) fn strip_fragment(url: &str) -> &str {
    url.split('#').next().unwrap_or(url)
}

pub(crate) fn parse_query_params(url: &str) -> Vec<HarQueryParam> {
    let no_frag = strip_fragment(url);
    let Some((_, q)) = no_frag.split_once('?') else {
        return Vec::new();
    };
    if q.is_empty() {
        return Vec::new();
    }
    q.split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (raw_k, raw_v) = pair.split_once('=').unwrap_or((pair, ""));
            HarQueryParam {
                name: percent_decode_form(raw_k),
                value: percent_decode_form(raw_v),
            }
        })
        .collect()
}

fn percent_decode_form(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                if let (Some(h), Some(l)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                    out.push((h << 4) | l);
                    i += 3;
                } else {
                    out.push(b'%');
                    i += 1;
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn cdp_headers(v: &Value) -> Vec<HarHeader> {
    v.as_object()
        .map(|m| {
            m.iter()
                .map(|(k, v)| HarHeader {
                    name: k.clone(),
                    value: v
                        .as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| v.to_string()),
                })
                .collect()
        })
        .unwrap_or_default()
}

fn header_ci(headers: &[HarHeader], name: &str) -> Option<String> {
    headers
        .iter()
        .find(|h| h.name.eq_ignore_ascii_case(name))
        .map(|h| h.value.clone())
}

fn header_slice_ci(headers: &[(String, String)], name: &str) -> Option<String> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.clone())
}

fn normalize_http_version(protocol: &str) -> String {
    match protocol.to_ascii_lowercase().as_str() {
        "" | "http/1.1" => "HTTP/1.1".to_string(),
        "http/1.0" => "HTTP/1.0".to_string(),
        "h2" | "http/2" | "http/2.0" => "HTTP/2.0".to_string(),
        "h3" | "http/3" | "http/3.0" => "HTTP/3.0".to_string(),
        _ => protocol.to_uppercase(),
    }
}

fn normalize_mime_type(mime: &str) -> String {
    mime.split(';').next().unwrap_or(mime).trim().to_string()
}

fn is_probably_text(bytes: &[u8], mime: &str) -> bool {
    let lower_mime = normalize_mime_type(mime).to_ascii_lowercase();
    let binary_mime = lower_mime.starts_with("image/")
        || lower_mime.starts_with("audio/")
        || lower_mime.starts_with("video/")
        || lower_mime.starts_with("font/")
        || matches!(
            lower_mime.as_str(),
            "application/octet-stream"
                | "application/pdf"
                | "application/zip"
                | "application/gzip"
                | "application/wasm"
        );
    if binary_mime {
        return false;
    }
    match std::str::from_utf8(bytes) {
        Ok(s) => !s.bytes().any(|b| b == 0 || (b < 0x20 && !matches!(b, b'\t' | b'\r' | b'\n'))),
        Err(_) => false,
    }
}

fn url_recorded(url: &str, filter: Option<&str>) -> bool {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return false;
    }
    match filter {
        Some(pat) => url_glob_matches(pat, url),
        None => true,
    }
}

struct InFlightHar {
    seq: u64,
    started_at: std::time::Instant,
    started_iso: String,
    request: HarRequest,
    response_received: bool,
    response_at: Option<std::time::Instant>,
    status: u16,
    status_text: String,
    http_version: String,
    response_headers: Vec<HarHeader>,
    mime_type: String,
    server_ip: Option<String>,
}

struct RecordedSlot {
    seq: u64,
    entry: HarEntry,
    body_task: Option<JoinHandle<Option<(i64, String, Option<String>)>>>,
}

enum RecorderCmd {
    Flush {
        session_id: Option<String>,
        done: oneshot::Sender<()>,
    },
}

/// Handle to flush pending HAR captures on a recording session before closing its target.
#[derive(Clone)]
pub struct HarFlusher {
    cmd_tx: tokio::sync::mpsc::UnboundedSender<RecorderCmd>,
}

impl HarFlusher {
    /// Waits for pending in-flight network bodies on `session_id` (up to 200 ms) so
    /// closing a page target does not drop the response body of its final requests.
    pub async fn flush_session(&self, session_id: Option<String>) {
        let (tx, rx) = oneshot::channel();
        if self
            .cmd_tx
            .send(RecorderCmd::Flush {
                session_id,
                done: tx,
            })
            .is_ok()
        {
            let _ = rx.await;
        }
    }
}

/// Active CDP listener recording HTTP traffic in HAR 1.2 format until [`finish`](Self::finish).
pub struct HarRecorder {
    path: PathBuf,
    mode: HarMode,
    stop_tx: Option<oneshot::Sender<()>>,
    cmd_tx: tokio::sync::mpsc::UnboundedSender<RecorderCmd>,
    task: JoinHandle<Vec<RecordedSlot>>,
}

impl HarRecorder {
    pub(crate) fn start(
        conn: Arc<Connection>,
        context_id: String,
        initial_sessions: Vec<String>,
        scope_page_target_id: Option<String>,
        opts: RecordHarOptions,
    ) -> Self {
        let mut events = conn.subscribe_unbounded();
        let (stop_tx, mut stop_rx) = oneshot::channel();
        let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::unbounded_channel::<RecorderCmd>();
        let path = opts.path.clone();
        let mode = opts.mode;
        let policy = opts.effective_content_policy();
        let url_filter = opts.url_filter.clone();

        let task = tokio::spawn(async move {
            let mut sessions: HashSet<String> = initial_sessions.into_iter().collect();
            let mut in_flight: HashMap<(String, String), InFlightHar> = HashMap::new();
            let mut completed: Vec<RecordedSlot> = Vec::new();
            let mut next_seq: u64 = 0;

            let handle_event = |ev: Arc<CdpEvent>,
                                sessions: &mut HashSet<String>,
                                in_flight: &mut HashMap<(String, String), InFlightHar>,
                                completed: &mut Vec<RecordedSlot>,
                                next_seq: &mut u64| {
                match ev.method.as_str() {
                    "Target.attachedToTarget" => {
                        let info = &ev.params["targetInfo"];
                        let ty = info["type"].as_str().unwrap_or("");
                        let parent_tracked = ev
                            .session_id
                            .as_deref()
                            .is_some_and(|psid| sessions.contains(psid));
                        let same_ctx =
                            info["browserContextId"].as_str() == Some(context_id.as_str());
                        if ty == "worker" {
                            if parent_tracked {
                                if let Some(sid) = ev.params["sessionId"].as_str() {
                                    sessions.insert(sid.to_string());
                                }
                            }
                            return;
                        }
                        if !same_ctx || (ty != "page" && ty != "iframe") {
                            return;
                        }
                        let matches_scope = match scope_page_target_id.as_deref() {
                            None => true,
                            Some(pid) => {
                                info["targetId"].as_str() == Some(pid)
                                    || info["openerId"].as_str() == Some(pid)
                                    || parent_tracked
                            }
                        };
                        if matches_scope {
                            if let Some(sid) = ev.params["sessionId"].as_str() {
                                sessions.insert(sid.to_string());
                            }
                        }
                    }
                    "Target.detachedFromTarget" => {
                        if let Some(sid) = ev.params["sessionId"].as_str() {
                            sessions.remove(sid);
                        }
                    }
                    "Network.requestWillBeSent" => {
                        let Some(sid) = ev.session_id.as_deref() else {
                            return;
                        };
                        if !sessions.contains(sid) {
                            return;
                        }
                        let req_id = ev.params["requestId"].as_str().unwrap_or("");
                        if req_id.is_empty() {
                            return;
                        }
                        let key = (sid.to_string(), req_id.to_string());
                        if ev.params["redirectResponse"].is_object() {
                            if let Some(prev) = in_flight.remove(&key) {
                                let r = &ev.params["redirectResponse"];
                                let resp_headers = cdp_headers(&r["headers"]);
                                let redirect_url =
                                    header_ci(&resp_headers, "location").unwrap_or_default();
                                let elapsed_ms =
                                    prev.started_at.elapsed().as_secs_f64() * 1000.0;
                                let proto = normalize_http_version(
                                    r["protocol"].as_str().unwrap_or("http/1.1"),
                                );
                                let status = r["status"].as_u64().unwrap_or(0) as u16;
                                let status_text =
                                    r["statusText"].as_str().unwrap_or("").to_string();
                                let mime_type = r["mimeType"]
                                    .as_str()
                                    .map(normalize_mime_type)
                                    .unwrap_or_default();
                                completed.push(RecordedSlot {
                                    seq: prev.seq,
                                    entry: HarEntry {
                                        started_date_time: prev.started_iso,
                                        time: if mode == HarMode::Minimal { -1.0 } else { elapsed_ms },
                                        request: prev.request,
                                        response: HarResponse {
                                            status,
                                            status_text,
                                            http_version: proto,
                                            headers: resp_headers,
                                            cookies: Vec::new(),
                                            content: HarContent {
                                                size: 0,
                                                mime_type,
                                                text: None,
                                                encoding: None,
                                            },
                                            redirect_url,
                                            headers_size: -1,
                                            body_size: 0,
                                        },
                                        cache: json!({}),
                                        timings: HarTimings {
                                            send: 0.0,
                                            wait: if mode == HarMode::Minimal { -1.0 } else { elapsed_ms },
                                            receive: 0.0,
                                        },
                                        server_ip_address: if mode == HarMode::Minimal {
                                            None
                                        } else {
                                            r["remoteIPAddress"].as_str().map(str::to_owned)
                                        },
                                    },
                                    body_task: None,
                                });
                            }
                        }

                        let req = &ev.params["request"];
                        let url = req["url"].as_str().unwrap_or("");
                        if !url_recorded(url, url_filter.as_deref()) {
                            return;
                        }
                        let method = req["method"].as_str().unwrap_or("GET").to_string();
                        let req_headers = cdp_headers(&req["headers"]);
                        let post_data = req["postData"].as_str().map(|text| HarPostData {
                            mime_type: header_ci(&req_headers, "content-type")
                                .unwrap_or_else(|| "application/octet-stream".into()),
                            text: Some(text.to_string()),
                        });
                        let body_size = post_data
                            .as_ref()
                            .and_then(|p| p.text.as_ref())
                            .map_or(0, |t| t.len() as i64);
                        let seq = *next_seq;
                        *next_seq += 1;
                        in_flight.insert(
                            key,
                            InFlightHar {
                                seq,
                                started_at: std::time::Instant::now(),
                                started_iso: format_iso8601_utc(SystemTime::now()),
                                request: HarRequest {
                                    method,
                                    url: url.to_string(),
                                    http_version: "HTTP/1.1".to_string(),
                                    headers: req_headers,
                                    query_string: parse_query_params(url),
                                    cookies: Vec::new(),
                                    headers_size: -1,
                                    body_size,
                                    post_data,
                                },
                                response_received: false,
                                response_at: None,
                                status: 0,
                                status_text: String::new(),
                                http_version: "HTTP/1.1".to_string(),
                                response_headers: Vec::new(),
                                mime_type: String::new(),
                                server_ip: None,
                            },
                        );
                    }
                    "Network.responseReceived" => {
                        let Some(sid) = ev.session_id.as_deref() else {
                            return;
                        };
                        let req_id = ev.params["requestId"].as_str().unwrap_or("");
                        let key = (sid.to_string(), req_id.to_string());
                        if let Some(entry) = in_flight.get_mut(&key) {
                            let r = &ev.params["response"];
                            entry.response_received = true;
                            entry.response_at = Some(std::time::Instant::now());
                            entry.status = r["status"].as_u64().unwrap_or(0) as u16;
                            entry.status_text =
                                r["statusText"].as_str().unwrap_or("").to_string();
                            entry.http_version = normalize_http_version(
                                r["protocol"].as_str().unwrap_or("http/1.1"),
                            );
                            entry.response_headers = cdp_headers(&r["headers"]);
                            entry.mime_type = r["mimeType"]
                                .as_str()
                                .map(normalize_mime_type)
                                .or_else(|| {
                                    header_ci(&entry.response_headers, "content-type")
                                        .map(|m| normalize_mime_type(&m))
                                })
                                .unwrap_or_default();
                            entry.server_ip = r["remoteIPAddress"].as_str().map(str::to_owned);
                            entry.request.http_version = entry.http_version.clone();
                        }
                    }
                    "Network.loadingFinished" => {
                        let Some(sid) = ev.session_id.as_deref() else {
                            return;
                        };
                        let req_id = ev.params["requestId"].as_str().unwrap_or("");
                        let key = (sid.to_string(), req_id.to_string());
                        let Some(finished) = in_flight.remove(&key) else {
                            return;
                        };
                        if !finished.response_received {
                            return;
                        }
                        let total_ms = finished.started_at.elapsed().as_secs_f64() * 1000.0;
                        let wait_ms = finished
                            .response_at
                            .map(|t| t.saturating_duration_since(finished.started_at).as_secs_f64() * 1000.0)
                            .unwrap_or(total_ms);
                        let receive_ms = (total_ms - wait_ms).max(0.0);
                        let redirect_url =
                            header_ci(&finished.response_headers, "location").unwrap_or_default();
                        let encoded_len =
                            ev.params["encodedDataLength"].as_i64().unwrap_or(-1);
                        let fetch_body = policy == HarContentPolicy::Embed
                            && finished.status != 204
                            && finished.status != 304
                            && !finished.request.method.eq_ignore_ascii_case("HEAD");
                        let body_task = if fetch_body {
                            let conn_c = conn.clone();
                            let sid_c = sid.to_string();
                            let rid_c = req_id.to_string();
                            let mime_c = finished.mime_type.clone();
                            Some(tokio::spawn(async move {
                                let res = conn_c
                                    .call(
                                        "Network.getResponseBody",
                                        json!({ "requestId": rid_c }),
                                        Some(&sid_c),
                                        Duration::from_secs(2),
                                    )
                                    .await
                                    .ok()?;
                                let raw = res["body"].as_str()?;
                                if res["base64Encoded"].as_bool().unwrap_or(false) {
                                    use base64::Engine;
                                    let decoded = base64::engine::general_purpose::STANDARD
                                        .decode(raw)
                                        .ok()?;
                                    let size = decoded.len() as i64;
                                    if is_probably_text(&decoded, &mime_c) {
                                        let text = String::from_utf8(decoded).ok()?;
                                        Some((size, text, None))
                                    } else {
                                        Some((size, raw.to_string(), Some("base64".to_string())))
                                    }
                                } else {
                                    let size = raw.len() as i64;
                                    Some((size, raw.to_string(), None))
                                }
                            }))
                        } else {
                            None
                        };

                        completed.push(RecordedSlot {
                            seq: finished.seq,
                            entry: HarEntry {
                                started_date_time: finished.started_iso,
                                time: if mode == HarMode::Minimal { -1.0 } else { total_ms },
                                request: finished.request,
                                response: HarResponse {
                                    status: finished.status,
                                    status_text: finished.status_text,
                                    http_version: finished.http_version,
                                    headers: finished.response_headers,
                                    cookies: Vec::new(),
                                    content: HarContent {
                                        size: 0,
                                        mime_type: finished.mime_type,
                                        text: None,
                                        encoding: None,
                                    },
                                    redirect_url,
                                    headers_size: -1,
                                    body_size: if mode == HarMode::Minimal { -1 } else { encoded_len },
                                },
                                cache: json!({}),
                                timings: if mode == HarMode::Minimal {
                                    HarTimings { send: -1.0, wait: -1.0, receive: -1.0 }
                                } else {
                                    HarTimings { send: 0.0, wait: wait_ms, receive: receive_ms }
                                },
                                server_ip_address: if mode == HarMode::Minimal {
                                    None
                                } else {
                                    finished.server_ip
                                },
                            },
                            body_task,
                        });
                    }
                    "Network.loadingFailed" => {
                        if let Some(sid) = ev.session_id.as_deref() {
                            if let Some(req_id) = ev.params["requestId"].as_str() {
                                in_flight.remove(&(sid.to_string(), req_id.to_string()));
                            }
                        }
                    }
                    _ => {}
                }
            };

            loop {
                tokio::select! {
                    _ = &mut stop_rx => {
                        while let Ok(ev) = events.try_recv() {
                            handle_event(
                                ev,
                                &mut sessions,
                                &mut in_flight,
                                &mut completed,
                                &mut next_seq,
                            );
                        }
                        let deadline = tokio::time::Instant::now() + Duration::from_millis(150);
                        while in_flight.values().any(|f| f.response_received)
                            && tokio::time::Instant::now() < deadline
                        {
                            match tokio::time::timeout_at(deadline, events.recv()).await {
                                Ok(Some(ev)) => {
                                    handle_event(
                                        ev,
                                        &mut sessions,
                                        &mut in_flight,
                                        &mut completed,
                                        &mut next_seq,
                                    );
                                }
                                _ => break,
                            }
                        }
                        while let Ok(ev) = events.try_recv() {
                            handle_event(
                                ev,
                                &mut sessions,
                                &mut in_flight,
                                &mut completed,
                                &mut next_seq,
                            );
                        }
                        break;
                    }
                    Some(RecorderCmd::Flush { session_id, done }) = cmd_rx.recv() => {
                        while let Ok(ev) = events.try_recv() {
                            handle_event(
                                ev,
                                &mut sessions,
                                &mut in_flight,
                                &mut completed,
                                &mut next_seq,
                            );
                        }
                        let has_pending = |map: &HashMap<(String, String), InFlightHar>| match session_id.as_deref() {
                            Some(sid) => map.keys().any(|(s, _)| s == sid),
                            None => map.values().any(|f| f.response_received),
                        };
                        let deadline = tokio::time::Instant::now() + Duration::from_millis(200);
                        while has_pending(&in_flight) && tokio::time::Instant::now() < deadline {
                            match tokio::time::timeout_at(deadline, events.recv()).await {
                                Ok(Some(ev)) => {
                                    handle_event(
                                        ev,
                                        &mut sessions,
                                        &mut in_flight,
                                        &mut completed,
                                        &mut next_seq,
                                    );
                                }
                                _ => break,
                            }
                        }
                        while let Ok(ev) = events.try_recv() {
                            handle_event(
                                ev,
                                &mut sessions,
                                &mut in_flight,
                                &mut completed,
                                &mut next_seq,
                            );
                        }
                        for slot in &mut completed {
                            if let Some(handle) = slot.body_task.take() {
                                if let Ok(Some((size, text, encoding))) = handle.await {
                                    slot.entry.response.content.size = size;
                                    slot.entry.response.content.text = Some(text);
                                    slot.entry.response.content.encoding = encoding;
                                }
                            }
                        }
                        let _ = done.send(());
                    }
                    ev = events.recv() => {
                        let Some(ev) = ev else {
                            break;
                        };
                        handle_event(
                            ev,
                            &mut sessions,
                            &mut in_flight,
                            &mut completed,
                            &mut next_seq,
                        );
                    }
                }
            }

            completed
        });

        Self {
            path,
            mode,
            stop_tx: Some(stop_tx),
            cmd_tx,
            task,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn flusher(&self) -> HarFlusher {
        HarFlusher {
            cmd_tx: self.cmd_tx.clone(),
        }
    }

    /// Stops recording, awaits pending response body fetches, writes HAR 1.2 JSON to disk, and
    /// returns the captured archive.
    pub async fn finish(mut self) -> Result<Har> {
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(());
        }
        let mut slots = self
            .task
            .await
            .map_err(|e| Error::Other(format!("HAR recorder task failed: {e}")))?;

        let mut entries = Vec::with_capacity(slots.len());
        slots.sort_by_key(|s| s.seq);
        for mut slot in slots {
            if let Some(handle) = slot.body_task.take() {
                if let Ok(Some((size, text, encoding))) = handle.await {
                    slot.entry.response.content.size = size;
                    slot.entry.response.content.text = Some(text);
                    slot.entry.response.content.encoding = encoding;
                }
            }
            entries.push(slot.entry);
        }

        let har = Har {
            log: HarLog {
                version: "1.2".to_string(),
                creator: HarCreator {
                    name: "Fluxwright".to_string(),
                    version: env!("CARGO_PKG_VERSION").to_string(),
                },
                browser: (self.mode == HarMode::Full).then(|| HarBrowser {
                    name: "Chromium".to_string(),
                    version: "unknown".to_string(),
                }),
                entries,
            },
        };
        har.save(&self.path)?;
        Ok(har)
    }
}

/// Offline HAR 1.2 matcher for `routeFromHAR` replay.
pub struct HarRouter {
    entries: Vec<HarEntry>,
    served: Mutex<Vec<bool>>,
}

impl HarRouter {
    pub fn new(har: Har) -> Self {
        let len = har.log.entries.len();
        Self {
            entries: har.log.entries,
            served: Mutex::new(vec![false; len]),
        }
    }

    pub fn from_file(path: impl AsRef<Path>) -> Result<Self> {
        Ok(Self::new(Har::from_file(path)?))
    }

    /// Finds the best matching HAR entry and constructs a [`Fulfill`] payload.
    pub fn match_request(
        &self,
        url: &str,
        method: &str,
        headers: &[(String, String)],
        post_data: Option<&str>,
    ) -> Option<Fulfill> {
        let target_url = strip_fragment(url);
        let mut scored: Vec<(usize, i32)> = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| {
                strip_fragment(&e.request.url) == target_url
                    && e.request.method.eq_ignore_ascii_case(method)
            })
            .map(|(idx, e)| {
                let mut score = 0i32;
                if let (Some(req_pd), Some(har_pd)) = (post_data, e.request.post_data.as_ref()) {
                    if har_pd.text.as_deref() == Some(req_pd) {
                        score += 100;
                    }
                }
                for name in ["content-type", "accept"] {
                    if let (Some(rv), Some(hv)) = (
                        header_slice_ci(headers, name),
                        header_ci(&e.request.headers, name),
                    ) {
                        if rv.eq_ignore_ascii_case(&hv) {
                            score += 10;
                        }
                    }
                }
                (idx, score)
            })
            .collect();

        if scored.is_empty() {
            return None;
        }
        let max_score = scored.iter().map(|(_, s)| *s).max().unwrap_or(0);
        scored.retain(|(_, s)| *s == max_score);

        let chosen_idx = {
            let mut served = self.served.lock().unwrap();
            if let Some(&(idx, _)) = scored.iter().find(|&&(i, _)| !served[i]) {
                served[idx] = true;
                idx
            } else {
                scored.last().unwrap().0
            }
        };

        Some(har_entry_to_fulfill(&self.entries[chosen_idx]))
    }
}

fn har_entry_to_fulfill(entry: &HarEntry) -> Fulfill {
    let headers: Vec<(String, String)> = entry
        .response
        .headers
        .iter()
        .filter(|h| {
            !matches!(
                h.name.to_ascii_lowercase().as_str(),
                "connection" | "content-encoding" | "content-length" | "transfer-encoding"
            )
        })
        .map(|h| (h.name.clone(), h.value.clone()))
        .collect();

    let body = match entry.response.content.text.as_deref() {
        Some(text) => {
            if entry
                .response
                .content
                .encoding
                .as_deref()
                .is_some_and(|e| e.eq_ignore_ascii_case("base64"))
            {
                use base64::Engine;
                base64::engine::general_purpose::STANDARD
                    .decode(text)
                    .unwrap_or_else(|_| text.as_bytes().to_vec())
            } else {
                text.as_bytes().to_vec()
            }
        }
        None => Vec::new(),
    };

    Fulfill {
        status: Some(if entry.response.status == 0 {
            200
        } else {
            entry.response.status
        }),
        headers,
        body,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_entry(url: &str, method: &str, status: u16, body: &str, post_data: Option<&str>) -> HarEntry {
        HarEntry {
            started_date_time: "2026-01-01T00:00:00.000Z".into(),
            time: 12.5,
            request: HarRequest {
                method: method.into(),
                url: url.into(),
                http_version: "HTTP/1.1".into(),
                headers: vec![HarHeader { name: "Accept".into(), value: "application/json".into() }],
                query_string: parse_query_params(url),
                cookies: Vec::new(),
                headers_size: -1,
                body_size: post_data.map_or(0, |s| s.len() as i64),
                post_data: post_data.map(|t| HarPostData {
                    mime_type: "application/json".into(),
                    text: Some(t.into()),
                }),
            },
            response: HarResponse {
                status,
                status_text: "OK".into(),
                http_version: "HTTP/1.1".into(),
                headers: vec![
                    HarHeader { name: "Content-Type".into(), value: "application/json".into() },
                    HarHeader { name: "Content-Length".into(), value: body.len().to_string() },
                    HarHeader { name: "Transfer-Encoding".into(), value: "chunked".into() },
                ],
                cookies: Vec::new(),
                content: HarContent {
                    size: body.len() as i64,
                    mime_type: "application/json".into(),
                    text: Some(body.into()),
                    encoding: None,
                },
                redirect_url: String::new(),
                headers_size: -1,
                body_size: body.len() as i64,
            },
            cache: json!({}),
            timings: HarTimings { send: 0.0, wait: 10.0, receive: 2.5 },
            server_ip_address: None,
        }
    }

    #[test]
    fn iso8601_utc_formats_epoch_and_modern_dates() {
        assert_eq!(format_iso8601_utc(UNIX_EPOCH), "1970-01-01T00:00:00.000Z");
        let ts = UNIX_EPOCH + Duration::from_millis(1_767_225_600_123);
        assert_eq!(format_iso8601_utc(ts), "2026-01-01T00:00:00.123Z");
    }

    #[test]
    fn query_params_decode_form_values_and_strip_fragments() {
        let qp = parse_query_params("https://example.test/search?q=hello+world&k=a%2Fb#sec");
        assert_eq!(
            qp,
            vec![
                HarQueryParam { name: "q".into(), value: "hello world".into() },
                HarQueryParam { name: "k".into(), value: "a/b".into() },
            ]
        );
    }

    #[test]
    fn har_json_roundtrip_and_router_matching() {
        use base64::Engine;

        let mut bin_entry = sample_entry("https://api.test/bin", "GET", 200, "", None);
        bin_entry.response.content = HarContent {
            size: 3,
            mime_type: "application/octet-stream".into(),
            text: Some(base64::engine::general_purpose::STANDARD.encode([0u8, 159, 255])),
            encoding: Some("base64".into()),
        };

        let har = Har {
            log: HarLog {
                version: "1.2".into(),
                creator: HarCreator { name: "Fluxwright".into(), version: "0.1.0".into() },
                browser: Some(HarBrowser { name: "Chromium".into(), version: "unknown".into() }),
                entries: vec![
                    sample_entry("https://api.test/items", "POST", 200, r#"{"n":1}"#, Some("{\"a\":1}")),
                    sample_entry("https://api.test/items", "POST", 200, r#"{"n":2}"#, Some("{\"a\":2}")),
                    sample_entry("https://api.test/seq", "GET", 200, "first", None),
                    sample_entry("https://api.test/seq", "GET", 200, "second", None),
                    bin_entry,
                ],
            },
        };

        let raw = serde_json::to_string(&har).unwrap();
        let parsed = Har::from_json_str(&raw).unwrap();
        assert_eq!(parsed, har);

        let router = HarRouter::new(parsed);
        // Fragment stripping + POST payload disambiguation.
        let r2 = router
            .match_request("https://api.test/items#hash", "POST", &[], Some("{\"a\":2}"))
            .unwrap();
        assert_eq!(r2.body, br#"{"n":2}"#);
        assert!(r2.headers.iter().all(|(k, _)| {
            !k.eq_ignore_ascii_case("content-length") && !k.eq_ignore_ascii_case("transfer-encoding")
        }));

        // Sequential multi-call replay clamps at last entry.
        assert_eq!(router.match_request("https://api.test/seq", "GET", &[], None).unwrap().body, b"first");
        assert_eq!(router.match_request("https://api.test/seq", "GET", &[], None).unwrap().body, b"second");
        assert_eq!(router.match_request("https://api.test/seq", "GET", &[], None).unwrap().body, b"second");

        // Binary base64 decode.
        assert_eq!(
            router.match_request("https://api.test/bin", "GET", &[], None).unwrap().body,
            vec![0u8, 159, 255]
        );
        assert!(router.match_request("https://api.test/missing", "GET", &[], None).is_none());
    }
}
