//! Session replay timeline, network/API inspector, HAR 1.2 export, and bounded
//! closed-session replay retention.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use fluxwright::{
    Har, HarContent, HarCreator, HarEntry, HarHeader, HarLog, HarPostData, HarQueryParam,
    HarRequest, HarResponse, HarTimings,
};
use serde::Serialize;
use serde_json::{json, Value};

use crate::job::truncate_bytes;

pub(crate) const MAX_TIMELINE_EVENTS: usize = 256;
pub(crate) const MAX_NETWORK_ENTRIES: usize = 256;
pub(crate) const MAX_RETAINED_FRAMES: usize = 40;
pub(crate) const MAX_FRAME_BYTES: usize = 2 * 1024 * 1024;
pub(crate) const MAX_RETAINED_REPLAYS: usize = 32;
pub(crate) const RETAINED_REPLAY_TTL_MS: u64 = 900_000;

const MAX_URL_BYTES: usize = 2048;
const MAX_SUMMARY_BYTES: usize = 2048;
const MAX_HEADERS_PER_MSG: usize = 64;
const MAX_HEADER_NAME_BYTES: usize = 256;
const MAX_HEADER_VALUE_BYTES: usize = 2048;
const MAX_POST_DATA_BYTES: usize = 16 * 1024;
const MAX_STATUS_TEXT_BYTES: usize = 256;
const MAX_ERROR_TEXT_BYTES: usize = 512;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct ReplayEvent {
    pub(crate) ts: u64,
    pub(crate) kind: String,
    pub(crate) level: String,
    pub(crate) url: String,
    pub(crate) summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) frame_base64: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct SessionNetEntry {
    pub(crate) id: String,
    pub(crate) started_at: u64,
    #[serde(skip)]
    pub(crate) started_mono: Option<Instant>,
    pub(crate) method: String,
    pub(crate) url: String,
    pub(crate) resource_type: String,
    pub(crate) request_headers: Vec<(String, String)>,
    pub(crate) post_data: Option<String>,
    pub(crate) status: Option<u16>,
    pub(crate) status_text: Option<String>,
    pub(crate) response_headers: Vec<(String, String)>,
    pub(crate) response_content_type: Option<String>,
    pub(crate) failed: Option<String>,
    pub(crate) duration_ms: Option<u64>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct SessionRecorder {
    pub(crate) timeline: VecDeque<ReplayEvent>,
    pub(crate) network: VecDeque<SessionNetEntry>,
    pub(crate) frame_count: usize,
    pub(crate) frame_bytes: usize,
}

impl SessionRecorder {
    fn push_event(&mut self, event: ReplayEvent) {
        while self.timeline.len() >= MAX_TIMELINE_EVENTS {
            if let Some(evicted) = self.timeline.pop_front() {
                if let Some(old_frame) = evicted.frame_base64 {
                    self.frame_count = self.frame_count.saturating_sub(1);
                    self.frame_bytes = self.frame_bytes.saturating_sub(old_frame.len());
                }
            }
        }
        self.timeline.push_back(event);
    }

    pub(crate) fn push_nav(&mut self, ts: u64, url: &str) {
        self.push_event(ReplayEvent {
            ts,
            kind: "nav".to_string(),
            level: "info".to_string(),
            url: truncate_bytes(url, MAX_URL_BYTES),
            summary: truncate_bytes(&format!("navigated to {url}"), MAX_SUMMARY_BYTES),
            status: None,
            frame_base64: None,
        });
    }

    pub(crate) fn push_console(&mut self, ts: u64, level: &str, url: &str, text: &str) {
        self.push_event(ReplayEvent {
            ts,
            kind: "console".to_string(),
            level: level.to_string(),
            url: truncate_bytes(url, MAX_URL_BYTES),
            summary: truncate_bytes(text, MAX_SUMMARY_BYTES),
            status: None,
            frame_base64: None,
        });
    }

    pub(crate) fn push_frame(&mut self, ts: u64, url: &str, base64_jpeg: String) {
        if base64_jpeg.is_empty() || base64_jpeg.len() > MAX_FRAME_BYTES {
            return;
        }
        while self.frame_count >= MAX_RETAINED_FRAMES
            || self.frame_bytes.saturating_add(base64_jpeg.len()) > MAX_FRAME_BYTES
        {
            let Some(idx) = self
                .timeline
                .iter()
                .position(|ev| ev.frame_base64.is_some())
            else {
                break;
            };
            if let Some(removed) = self.timeline.remove(idx) {
                if let Some(old) = removed.frame_base64 {
                    self.frame_count = self.frame_count.saturating_sub(1);
                    self.frame_bytes = self.frame_bytes.saturating_sub(old.len());
                }
            }
        }
        self.frame_count += 1;
        self.frame_bytes += base64_jpeg.len();
        self.push_event(ReplayEvent {
            ts,
            kind: "frame".to_string(),
            level: "info".to_string(),
            url: truncate_bytes(url, MAX_URL_BYTES),
            summary: truncate_bytes(&format!("frame captured at {url}"), MAX_SUMMARY_BYTES),
            status: None,
            frame_base64: Some(base64_jpeg),
        });
    }

    pub(crate) fn record_cdp_event(
        &mut self,
        ts: u64,
        method: &str,
        params: &Value,
        current_url: &str,
    ) {
        match method {
            "Runtime.consoleAPICalled" => {
                let raw_kind = params["type"].as_str().unwrap_or("log");
                let level = match raw_kind {
                    "warning" => "warn",
                    "error" | "assert" => "error",
                    "debug" => "debug",
                    _ => "info",
                };
                let text = params["args"]
                    .as_array()
                    .map(|args| args.iter().map(arg_text).collect::<Vec<_>>().join(" "))
                    .unwrap_or_default();
                let summary = if text.is_empty() {
                    raw_kind.to_string()
                } else {
                    text
                };
                self.push_console(ts, level, current_url, &summary);
            }
            "Runtime.exceptionThrown" => {
                let details = &params["exceptionDetails"];
                let ex = &details["exception"];
                let text = ex["description"]
                    .as_str()
                    .map(str::to_owned)
                    .or_else(|| ex.get("value").map(|_| arg_text(ex)))
                    .or_else(|| details["text"].as_str().map(str::to_owned))
                    .unwrap_or_else(|| "Uncaught exception".to_string());
                let url = details["url"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .unwrap_or(current_url);
                self.push_console(ts, "error", url, &text);
            }
            "Network.requestWillBeSent" => self.record_request_will_be_sent(ts, params),
            "Network.responseReceived" => self.record_response_received(ts, params),
            "Network.loadingFinished" => self.record_loading_finished(params),
            "Network.loadingFailed" => self.record_loading_failed(ts, params),
            _ => {}
        }
    }

    pub(crate) fn record_request_will_be_sent(&mut self, ts: u64, params: &Value) {
        let req_id = params["requestId"].as_str().unwrap_or("");
        if req_id.is_empty() {
            return;
        }
        if params["redirectResponse"].is_object() {
            self.apply_response(ts, req_id, &params["redirectResponse"]);
        }

        let req = &params["request"];
        let url = req["url"].as_str().unwrap_or("");
        if !is_recorded_net_url(url) {
            return;
        }

        let method = truncate_bytes(req["method"].as_str().unwrap_or("GET"), 32);
        let resource_type = truncate_bytes(params["type"].as_str().unwrap_or("Other"), 64);
        let request_headers = parse_cdp_headers(&req["headers"]);
        let post_data = req["postData"]
            .as_str()
            .map(|s| truncate_bytes(s, MAX_POST_DATA_BYTES));

        if self.network.len() >= MAX_NETWORK_ENTRIES {
            self.network.pop_front();
        }
        self.network.push_back(SessionNetEntry {
            id: truncate_bytes(req_id, 128),
            started_at: ts,
            started_mono: Some(Instant::now()),
            method,
            url: truncate_bytes(url, MAX_URL_BYTES),
            resource_type,
            request_headers,
            post_data,
            status: None,
            status_text: None,
            response_headers: Vec::new(),
            response_content_type: None,
            failed: None,
            duration_ms: None,
        });
    }

    pub(crate) fn record_response_received(&mut self, ts: u64, params: &Value) {
        let req_id = params["requestId"].as_str().unwrap_or("");
        if req_id.is_empty() {
            return;
        }
        self.apply_response(ts, req_id, &params["response"]);
    }

    fn apply_response(&mut self, ts: u64, req_id: &str, r: &Value) {
        let Some(entry) = self
            .network
            .iter_mut()
            .rev()
            .find(|e| e.id == req_id && e.status.is_none() && e.failed.is_none())
        else {
            return;
        };

        let status = r["status"].as_u64().and_then(|s| u16::try_from(s).ok());
        let status_text = r["statusText"]
            .as_str()
            .map(|s| truncate_bytes(s, MAX_STATUS_TEXT_BYTES));
        let response_headers = parse_cdp_headers(&r["headers"]);
        let response_content_type = r["mimeType"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(|s| truncate_bytes(s, 256))
            .or_else(|| {
                header_ci(&response_headers, "content-type").map(|s| truncate_bytes(&s, 256))
            });

        entry.status = status;
        entry.status_text = status_text;
        entry.response_headers = response_headers;
        entry.response_content_type = response_content_type;
        entry.duration_ms = entry
            .started_mono
            .map(|m| m.elapsed().as_millis().min(u64::MAX as u128) as u64);

        let method = entry.method.clone();
        let url = entry.url.clone();

        if let Some(st) = status.filter(|&s| s >= 400) {
            let level = if st >= 500 { "error" } else { "warn" };
            self.push_event(ReplayEvent {
                ts,
                kind: "network".to_string(),
                level: level.to_string(),
                url: url.clone(),
                summary: truncate_bytes(&format!("{method} {url} -> {st}"), MAX_SUMMARY_BYTES),
                status: Some(st),
                frame_base64: None,
            });
        }
    }

    pub(crate) fn record_loading_finished(&mut self, params: &Value) {
        let req_id = params["requestId"].as_str().unwrap_or("");
        if req_id.is_empty() {
            return;
        }
        if let Some(entry) = self
            .network
            .iter_mut()
            .rev()
            .find(|e| e.id == req_id && e.failed.is_none())
        {
            entry.duration_ms = entry
                .started_mono
                .map(|m| m.elapsed().as_millis().min(u64::MAX as u128) as u64);
        }
    }

    pub(crate) fn record_loading_failed(&mut self, ts: u64, params: &Value) {
        let req_id = params["requestId"].as_str().unwrap_or("");
        if req_id.is_empty() {
            return;
        }
        let Some(entry) = self
            .network
            .iter_mut()
            .rev()
            .find(|e| e.id == req_id && e.failed.is_none())
        else {
            return;
        };

        let reason = params["errorText"]
            .as_str()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| {
                if params["canceled"].as_bool().unwrap_or(false) {
                    "canceled"
                } else {
                    "loading failed"
                }
            });
        entry.failed = Some(truncate_bytes(reason, MAX_ERROR_TEXT_BYTES));
        entry.duration_ms = entry
            .started_mono
            .map(|m| m.elapsed().as_millis().min(u64::MAX as u128) as u64);

        let method = entry.method.clone();
        let url = entry.url.clone();
        let status = entry.status;
        self.push_event(ReplayEvent {
            ts,
            kind: "network".to_string(),
            level: "error".to_string(),
            url: url.clone(),
            summary: truncate_bytes(
                &format!("{method} {url} failed: {reason}"),
                MAX_SUMMARY_BYTES,
            ),
            status,
            frame_base64: None,
        });
    }

    pub(crate) fn query_replay(
        &self,
        q: Option<&str>,
        kind: Option<&str>,
        include_frames: bool,
    ) -> (usize, Vec<ReplayEvent>) {
        let total_events = self.timeline.len();
        let q_lower = q.map(|s| s.to_ascii_lowercase());
        let events = self
            .timeline
            .iter()
            .filter(|ev| {
                if let Some(k) = kind {
                    if !ev.kind.eq_ignore_ascii_case(k) {
                        return false;
                    }
                }
                if let Some(ref needle) = q_lower {
                    let hit = ev.url.to_ascii_lowercase().contains(needle)
                        || ev.summary.to_ascii_lowercase().contains(needle)
                        || ev.kind.to_ascii_lowercase().contains(needle)
                        || ev.level.to_ascii_lowercase().contains(needle);
                    if !hit {
                        return false;
                    }
                }
                true
            })
            .map(|ev| {
                let mut cloned = ev.clone();
                if !include_frames {
                    cloned.frame_base64 = None;
                }
                cloned
            })
            .collect();
        (total_events, events)
    }

    pub(crate) fn export_network_entries(
        &self,
        filter: Option<&str>,
        api_only: bool,
        include_auth: bool,
    ) -> (usize, Vec<SessionNetEntry>) {
        let total_entries = self.network.len();
        let filter_lower = filter.map(|s| s.to_ascii_lowercase());
        let entries = self
            .network
            .iter()
            .filter(|entry| {
                if let Some(ref needle) = filter_lower {
                    if !entry.url.to_ascii_lowercase().contains(needle) {
                        return false;
                    }
                }
                if api_only && !is_api_entry(entry) {
                    return false;
                }
                true
            })
            .map(|entry| {
                let mut cloned = entry.clone();
                if !include_auth {
                    redact_headers(&mut cloned.request_headers);
                    redact_headers(&mut cloned.response_headers);
                }
                cloned
            })
            .collect();
        (total_entries, entries)
    }

    pub(crate) fn export_har(
        &self,
        filter: Option<&str>,
        api_only: bool,
        include_auth: bool,
    ) -> Har {
        let (_, entries) = self.export_network_entries(filter, api_only, include_auth);
        let har_entries = entries.into_iter().map(entry_to_har).collect();
        Har {
            log: HarLog {
                version: "1.2".to_string(),
                creator: HarCreator {
                    name: "fluxwright".to_string(),
                    version: env!("CARGO_PKG_VERSION").to_string(),
                },
                browser: None,
                entries: har_entries,
            },
        }
    }
}

#[derive(Clone)]
pub(crate) struct RetainedReplay {
    pub(crate) id: String,
    pub(crate) created_at: u64,
    pub(crate) closed_at: u64,
    pub(crate) url: String,
    pub(crate) keep_alive: bool,
    pub(crate) profile: Option<String>,
    pub(crate) proxy_session: Option<String>,
    pub(crate) byob: bool,
    pub(crate) record_frames: bool,
    pub(crate) recorder: SessionRecorder,
}

impl RetainedReplay {
    pub(crate) fn dashboard_summary(&self) -> Value {
        json!({
            "id": self.id,
            "status": "closed",
            "keep_alive": self.keep_alive,
            "connected": false,
            "url": if self.url.is_empty() { "about:blank" } else { &self.url },
            "profile": self.profile,
            "proxy_session": self.proxy_session,
            "byob": self.byob,
            "record_frames": self.record_frames,
            "created_at": self.created_at,
            "expires_at": Value::Null,
            "closed_at": self.closed_at,
            "event_count": self.recorder.timeline.len(),
            "network_count": self.recorder.network.len(),
            "frame_count": self.recorder.frame_count,
        })
    }
}

#[derive(Default)]
pub(crate) struct RetainedReplayStore {
    entries: VecDeque<RetainedReplay>,
}

impl RetainedReplayStore {
    pub(crate) fn purge_expired(&mut self, now_ms: u64) {
        self.entries
            .retain(|e| now_ms.saturating_sub(e.closed_at) < RETAINED_REPLAY_TTL_MS);
    }

    pub(crate) fn push(&mut self, item: RetainedReplay, now_ms: u64) {
        self.purge_expired(now_ms);
        self.entries.retain(|e| e.id != item.id);
        while self.entries.len() >= MAX_RETAINED_REPLAYS {
            self.entries.pop_front();
        }
        self.entries.push_back(item);
    }

    pub(crate) fn with_entry<R>(
        &mut self,
        id: &str,
        now_ms: u64,
        f: impl FnOnce(&RetainedReplay) -> R,
    ) -> Option<R> {
        self.purge_expired(now_ms);
        self.entries.iter().find(|e| e.id == id).map(f)
    }

    pub(crate) fn summaries(&mut self, now_ms: u64) -> Vec<Value> {
        self.purge_expired(now_ms);
        self.entries
            .iter()
            .map(RetainedReplay::dashboard_summary)
            .collect()
    }
}

fn is_recorded_net_url(url: &str) -> bool {
    let Some((scheme, _)) = url.split_once("://") else {
        return false;
    };
    scheme.eq_ignore_ascii_case("http")
        || scheme.eq_ignore_ascii_case("https")
        || scheme.eq_ignore_ascii_case("ws")
        || scheme.eq_ignore_ascii_case("wss")
}

fn is_api_entry(entry: &SessionNetEntry) -> bool {
    if entry.resource_type.eq_ignore_ascii_case("xhr")
        || entry.resource_type.eq_ignore_ascii_case("fetch")
    {
        return true;
    }
    entry
        .response_content_type
        .as_deref()
        .is_some_and(|ct| {
            let low = ct.to_ascii_lowercase();
            low.contains("json") || low.contains("graphql")
        })
}

pub(crate) fn is_sensitive_header(name: &str) -> bool {
    matches!(
        name.trim().to_ascii_lowercase().as_str(),
        "authorization" | "cookie" | "set-cookie" | "x-api-key" | "proxy-authorization"
    )
}

fn redact_headers(headers: &mut [(String, String)]) {
    for (name, value) in headers {
        if is_sensitive_header(name) {
            *value = String::from("[REDACTED]");
        }
    }
}

fn parse_cdp_headers(v: &Value) -> Vec<(String, String)> {
    let Some(map) = v.as_object() else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity(map.len().min(MAX_HEADERS_PER_MSG));
    for (k, val) in map.iter().take(MAX_HEADERS_PER_MSG) {
        let raw_val = val
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| val.to_string());
        out.push((
            truncate_bytes(k, MAX_HEADER_NAME_BYTES),
            truncate_bytes(&raw_val, MAX_HEADER_VALUE_BYTES),
        ));
    }
    out
}

fn header_ci(headers: &[(String, String)], name: &str) -> Option<String> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.clone())
}

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

fn entry_to_har(entry: SessionNetEntry) -> HarEntry {
    let time = entry.duration_ms.map(|d| d as f64).unwrap_or(0.0);
    let post_data = entry.post_data.as_ref().map(|text| HarPostData {
        mime_type: header_ci(&entry.request_headers, "content-type")
            .unwrap_or_else(|| "application/octet-stream".to_string()),
        text: Some(text.clone()),
    });
    let body_size = post_data
        .as_ref()
        .and_then(|p| p.text.as_ref())
        .map_or(0, |t| t.len() as i64);
    let redirect_url = header_ci(&entry.response_headers, "location").unwrap_or_default();
    let status = entry.status.unwrap_or(0);
    let status_text = entry
        .status_text
        .or(entry.failed)
        .unwrap_or_default();

    HarEntry {
        started_date_time: format_iso8601_utc_ms(entry.started_at),
        time,
        request: HarRequest {
            method: entry.method,
            url: entry.url.clone(),
            http_version: "HTTP/1.1".to_string(),
            headers: entry
                .request_headers
                .into_iter()
                .map(|(name, value)| HarHeader { name, value })
                .collect(),
            query_string: parse_query_params(&entry.url),
            cookies: Vec::new(),
            headers_size: -1,
            body_size,
            post_data,
        },
        response: HarResponse {
            status,
            status_text,
            http_version: "HTTP/1.1".to_string(),
            headers: entry
                .response_headers
                .into_iter()
                .map(|(name, value)| HarHeader { name, value })
                .collect(),
            cookies: Vec::new(),
            content: HarContent {
                size: -1,
                mime_type: entry.response_content_type.unwrap_or_default(),
                text: None,
                encoding: None,
            },
            redirect_url,
            headers_size: -1,
            body_size: -1,
        },
        cache: json!({}),
        timings: HarTimings {
            send: 0.0,
            wait: time,
            receive: 0.0,
        },
        server_ip_address: None,
    }
}

fn format_iso8601_utc_ms(epoch_ms: u64) -> String {
    let dur = Duration::from_millis(epoch_ms);
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

fn parse_query_params(url: &str) -> Vec<HarQueryParam> {
    let no_frag = url.split('#').next().unwrap_or(url);
    let Some((_, q)) = no_frag.split_once('?') else {
        return Vec::new();
    };
    if q.is_empty() {
        return Vec::new();
    }
    q.split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            HarQueryParam {
                name: k.to_string(),
                value: v.to_string(),
            }
        })
        .collect()
}
