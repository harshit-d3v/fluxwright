//! Playwright Trace Viewer-compatible `.zip` context tracing (`context.tracing()`).
//!
//! Captures action `before`/`after` events, CDP `Page.startScreencast` JPEG frames, console
//! messages, page errors, and network activity, bundling them into a standard Store-mode ZIP
//! archive containing `trace.trace`, `trace.network`, and `resources/<sha1>.jpeg`.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fluxwright_cdp::{CdpPage, CdpSession, LogEntry, NetEvent, NetRequest, PageLog};
use serde_json::{json, Value};
use tokio::sync::Mutex as AsyncMutex;
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::error::{Error, Result};

const CDP_TIMEOUT: Duration = Duration::from_secs(5);
const SCREENCAST_WAIT_MAX: Duration = Duration::from_millis(250);
const SCREENCAST_POLL_STEP: Duration = Duration::from_millis(20);
const FLUSH_SETTLE_DELAY: Duration = Duration::from_millis(20);

/// Options for [`Tracing::start`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TracingStartOptions {
    pub name: Option<String>,
    pub title: Option<String>,
    pub screenshots: bool,
    pub snapshots: bool,
}

/// Options for [`Tracing::start_chunk`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TracingChunkOptions {
    pub name: Option<String>,
    pub title: Option<String>,
}

/// Playwright-compatible context tracer attached to a [`crate::ContextLease`].
#[derive(Clone)]
pub struct Tracing {
    inner: Arc<ContextTracing>,
}

impl Tracing {
    pub(crate) fn new(inner: Arc<ContextTracing>) -> Self {
        Self { inner }
    }

    /// Starts tracing on this browser context and begins the initial trace chunk.
    pub async fn start(&self, options: TracingStartOptions) -> Result<()> {
        self.inner.start(options).await
    }

    /// Starts a new trace chunk after [`Self::stop_chunk`] while keeping listeners active.
    pub async fn start_chunk(&self, options: TracingChunkOptions) -> Result<()> {
        self.inner.start_chunk(options).await
    }

    /// Finishes the current trace chunk and writes a `.zip` archive when `path` is provided.
    pub async fn stop_chunk(&self, path: Option<&Path>) -> Result<()> {
        self.inner.stop_chunk(path).await
    }

    /// Stops tracing and writes the remaining chunk as a `.zip` archive when `path` is provided.
    pub async fn stop(&self, path: Option<&Path>) -> Result<()> {
        self.inner.stop(path).await
    }
}

#[derive(Clone)]
pub(crate) struct TrackedPage {
    pub(crate) lease_id: Uuid,
    pub(crate) page: CdpPage,
    pub(crate) log: Arc<std::sync::Mutex<PageLog>>,
}

struct PageWatchHandles {
    tasks: Vec<JoinHandle<()>>,
    cdp_session: Option<CdpSession>,
}

struct InFlightNet {
    pageref: String,
    started_iso: String,
    start_mono_ms: f64,
    method: String,
    url: String,
    req_headers: Vec<(String, String)>,
    post_data: Option<String>,
    response_mono_ms: Option<f64>,
    status: u16,
    status_text: String,
    resp_headers: Vec<(String, String)>,
    mime_type: String,
    failed_text: Option<String>,
}

struct TraceState {
    active: bool,
    chunk_active: bool,
    closed: bool,
    screenshots: bool,
    snapshots: bool,
    name: Option<String>,
    title: Option<String>,
    origin_instant: Instant,
    origin_wall_ms: f64,
    next_call_seq: u64,
    next_page_seq: u64,
    page_ids: HashMap<String, String>,
    trace_lines: Vec<String>,
    network_lines: Vec<String>,
    resources: BTreeMap<String, Vec<u8>>,
    chunk_frames: usize,
    in_flight_net: HashMap<(String, String), InFlightNet>,
    open_pages: Vec<TrackedPage>,
    watchers: HashMap<Uuid, PageWatchHandles>,
}

impl TraceState {
    fn new() -> Self {
        Self {
            active: false,
            chunk_active: false,
            closed: false,
            screenshots: false,
            snapshots: false,
            name: None,
            title: None,
            origin_instant: Instant::now(),
            origin_wall_ms: unix_now_ms(),
            next_call_seq: 0,
            next_page_seq: 0,
            page_ids: HashMap::new(),
            trace_lines: Vec::new(),
            network_lines: Vec::new(),
            resources: BTreeMap::new(),
            chunk_frames: 0,
            in_flight_net: HashMap::new(),
            open_pages: Vec::new(),
            watchers: HashMap::new(),
        }
    }

    fn mono_ms(&self) -> f64 {
        self.origin_instant.elapsed().as_secs_f64() * 1000.0
    }

    fn ensure_page_id(&mut self, target_id: &str) -> String {
        if let Some(existing) = self.page_ids.get(target_id) {
            return existing.clone();
        }
        self.next_page_seq += 1;
        let id = format!("page@{}", self.next_page_seq);
        self.page_ids.insert(target_id.to_string(), id.clone());
        id
    }

    fn push_context_options(&mut self) {
        let mut entry = json!({
            "type": "context-options",
            "version": 6,
            "origin": "library",
            "browserName": "chromium",
            "platform": trace_platform(),
            "wallTime": self.origin_wall_ms,
            "monotonicTime": 0.0,
            "sdkLanguage": "rust",
            "options": {
                "screenshots": self.screenshots,
                "snapshots": self.snapshots,
            }
        });
        if let Some(ref title) = self.title {
            entry["title"] = json!(title);
        }
        if let Some(ref name) = self.name {
            entry["contextId"] = json!(name);
        }
        self.trace_lines.push(entry.to_string());
    }

    fn reset_chunk(&mut self, name: Option<String>, title: Option<String>) {
        self.name = name;
        self.title = title;
        self.trace_lines.clear();
        self.network_lines.clear();
        self.resources.clear();
        self.in_flight_net.clear();
        self.chunk_frames = 0;
        self.chunk_active = true;
        self.push_context_options();
        let open_ids: Vec<String> = self
            .open_pages
            .iter()
            .map(|p| p.page.target_id.0.clone())
            .collect();
        let now_ms = self.mono_ms();
        for tid in open_ids {
            let page_id = self.ensure_page_id(&tid);
            self.trace_lines.push(
                json!({
                    "type": "event",
                    "time": now_ms,
                    "class": "BrowserContext",
                    "method": "page",
                    "params": { "pageId": page_id },
                    "pageId": page_id,
                })
                .to_string(),
            );
        }
    }

    fn flush_in_flight_network(&mut self) {
        let end_ms = self.mono_ms();
        let pending = std::mem::take(&mut self.in_flight_net);
        for (_, item) in pending {
            if self.chunk_active {
                self.network_lines
                    .push(build_resource_snapshot(item, end_ms).to_string());
            }
        }
    }
}

pub(crate) struct ContextTracing {
    active_flag: AtomicBool,
    op_lock: AsyncMutex<()>,
    state: std::sync::Mutex<TraceState>,
}

impl ContextTracing {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            active_flag: AtomicBool::new(false),
            op_lock: AsyncMutex::new(()),
            state: std::sync::Mutex::new(TraceState::new()),
        })
    }

    #[inline]
    pub(crate) fn is_active(&self) -> bool {
        self.active_flag.load(Ordering::Acquire)
    }

    pub(crate) async fn register_page(
        self: &Arc<Self>,
        lease_id: Uuid,
        page: CdpPage,
        log: Arc<std::sync::Mutex<PageLog>>,
    ) {
        let should_watch = {
            let mut st = self.state.lock().unwrap();
            if st.closed {
                return;
            }
            st.open_pages.push(TrackedPage {
                lease_id,
                page: page.clone(),
                log: log.clone(),
            });
            if st.active {
                let page_id = st.ensure_page_id(&page.target_id.0);
                if st.chunk_active {
                    let t = st.mono_ms();
                    st.trace_lines.push(
                        json!({
                            "type": "event",
                            "time": t,
                            "class": "BrowserContext",
                            "method": "page",
                            "params": { "pageId": page_id },
                            "pageId": page_id,
                        })
                        .to_string(),
                    );
                }
                Some((st.screenshots, page_id))
            } else {
                None
            }
        };
        if let Some((screenshots, page_id)) = should_watch {
            self.attach_page_watchers(lease_id, page, log, page_id, screenshots)
                .await;
        }
    }

    pub(crate) fn unregister_page(&self, lease_id: Uuid) {
        let (removed, watch) = {
            let mut st = self.state.lock().unwrap();
            let pos = st.open_pages.iter().position(|p| p.lease_id == lease_id);
            let removed = pos.map(|i| st.open_pages.remove(i));
            if let Some(ref tp) = removed {
                if st.active && st.chunk_active {
                    let page_id = st.ensure_page_id(&tp.page.target_id.0);
                    let t = st.mono_ms();
                    st.trace_lines.push(
                        json!({
                            "type": "event",
                            "time": t,
                            "class": "Page",
                            "method": "close",
                            "params": {},
                            "pageId": page_id,
                        })
                        .to_string(),
                    );
                }
            }
            let watch = st.watchers.remove(&lease_id);
            (removed, watch)
        };
        drop(removed);
        if let Some(handles) = watch {
            for h in handles.tasks {
                h.abort();
            }
            if let Some(session) = handles.cdp_session {
                if let Ok(rt) = tokio::runtime::Handle::try_current() {
                    rt.spawn(async move {
                        let _ = session
                            .send("Page.stopScreencast", json!({}), Duration::from_secs(2))
                            .await;
                        let _ = session.detach(Duration::from_secs(2)).await;
                    });
                }
            }
        }
    }

    pub(crate) fn shutdown(&self) {
        self.active_flag.store(false, Ordering::Release);
        let watches = {
            let mut st = self.state.lock().unwrap();
            st.closed = true;
            st.active = false;
            st.chunk_active = false;
            st.open_pages.clear();
            st.trace_lines.clear();
            st.network_lines.clear();
            st.resources.clear();
            st.in_flight_net.clear();
            std::mem::take(&mut st.watchers)
        };
        for (_, handles) in watches {
            for h in handles.tasks {
                h.abort();
            }
            if let Some(session) = handles.cdp_session {
                if let Ok(rt) = tokio::runtime::Handle::try_current() {
                    rt.spawn(async move {
                        let _ = session
                            .send("Page.stopScreencast", json!({}), Duration::from_secs(2))
                            .await;
                        let _ = session.detach(Duration::from_secs(2)).await;
                    });
                }
            }
        }
    }

    pub(crate) async fn trace_action<F, T>(
        &self,
        target_id: &str,
        class_name: &str,
        method_name: &str,
        api_name: &str,
        params: Value,
        fut: F,
    ) -> Result<T>
    where
        F: std::future::Future<Output = Result<T>>,
    {
        let call_id = if self.is_active() {
            let mut st = self.state.lock().unwrap();
            if st.active && st.chunk_active {
                st.next_call_seq += 1;
                let seq = st.next_call_seq;
                let call_id = format!("call@{seq}");
                let page_id = st.ensure_page_id(target_id);
                let start_time = st.mono_ms();
                st.trace_lines.push(
                    json!({
                        "type": "before",
                        "callId": call_id,
                        "startTime": start_time,
                        "class": class_name,
                        "method": method_name,
                        "apiName": api_name,
                        "params": params,
                        "pageId": page_id,
                        "stepId": call_id,
                    })
                    .to_string(),
                );
                Some(call_id)
            } else {
                None
            }
        } else {
            None
        };

        let result = fut.await;

        if let Some(call_id) = call_id {
            let mut st = self.state.lock().unwrap();
            if st.active && st.chunk_active {
                let end_time = st.mono_ms();
                let mut after = json!({
                    "type": "after",
                    "callId": call_id,
                    "endTime": end_time,
                });
                if let Err(ref err) = result {
                    after["error"] = json!({ "message": err.to_string() });
                }
                st.trace_lines.push(after.to_string());
            }
        }

        result
    }

    async fn start(self: &Arc<Self>, options: TracingStartOptions) -> Result<()> {
        let _guard = self.op_lock.lock().await;
        let existing_pages = {
            let mut st = self.state.lock().unwrap();
            if st.closed {
                return Err(Error::Other("browser context is closed".into()));
            }
            if st.active {
                return Err(Error::Other("Tracing has been already started".into()));
            }
            st.active = true;
            st.screenshots = options.screenshots;
            st.snapshots = options.snapshots;
            st.origin_instant = Instant::now();
            st.origin_wall_ms = unix_now_ms();
            st.reset_chunk(options.name, options.title);
            let pages: Vec<(Uuid, CdpPage, Arc<std::sync::Mutex<PageLog>>, String)> = st
                .open_pages
                .iter()
                .map(|tp| {
                    let pid = tp.page.target_id.0.clone();
                    (tp.lease_id, tp.page.clone(), tp.log.clone(), pid)
                })
                .collect();
            pages
                .into_iter()
                .map(|(lid, page, log, tid)| {
                    let page_id = st.ensure_page_id(&tid);
                    (lid, page, log, page_id)
                })
                .collect::<Vec<_>>()
        };
        self.active_flag.store(true, Ordering::Release);

        for (lease_id, page, log, page_id) in existing_pages {
            self.attach_page_watchers(lease_id, page, log, page_id, options.screenshots)
                .await;
        }
        Ok(())
    }

    async fn start_chunk(&self, options: TracingChunkOptions) -> Result<()> {
        let _guard = self.op_lock.lock().await;
        let mut st = self.state.lock().unwrap();
        if st.closed {
            return Err(Error::Other("browser context is closed".into()));
        }
        if !st.active {
            return Err(Error::Other(
                "Must start tracing before starting a new chunk".into(),
            ));
        }
        st.reset_chunk(options.name, options.title);
        Ok(())
    }

    async fn stop_chunk(&self, path: Option<&Path>) -> Result<()> {
        let _guard = self.op_lock.lock().await;
        {
            let st = self.state.lock().unwrap();
            if st.closed {
                return Err(Error::Other("browser context is closed".into()));
            }
            if !st.active {
                return Err(Error::Other("Must start tracing before stopping".into()));
            }
            if !st.chunk_active {
                return Err(Error::Other("Must start a chunk before stopping".into()));
            }
        }
        self.finalize_chunk(path).await
    }

    async fn stop(&self, path: Option<&Path>) -> Result<()> {
        let _guard = self.op_lock.lock().await;
        let (was_active, chunk_active) = {
            let st = self.state.lock().unwrap();
            if st.closed {
                return Err(Error::Other("browser context is closed".into()));
            }
            (st.active, st.chunk_active)
        };
        if !was_active {
            if path.is_some() {
                return Err(Error::Other("Tracing has not been started".into()));
            }
            return Ok(());
        }
        if path.is_some() && !chunk_active {
            return Err(Error::Other(
                "No active trace chunk; call start_chunk before stop(Some(path))".into(),
            ));
        }

        let write_res = if chunk_active {
            self.finalize_chunk(path).await
        } else {
            Ok(())
        };

        self.teardown_recording().await;
        write_res
    }

    async fn finalize_chunk(&self, path: Option<&Path>) -> Result<()> {
        let want_fallback = {
            let st = self.state.lock().unwrap();
            path.is_some() && st.screenshots && st.chunk_frames == 0 && !st.open_pages.is_empty()
        };
        if want_fallback {
            let deadline = Instant::now() + SCREENCAST_WAIT_MAX;
            while Instant::now() < deadline {
                if self.state.lock().unwrap().chunk_frames > 0 {
                    break;
                }
                tokio::time::sleep(SCREENCAST_POLL_STEP).await;
            }
            if self.state.lock().unwrap().chunk_frames == 0 {
                let targets: Vec<(String, Option<CdpSession>, CdpPage)> = {
                    let mut st = self.state.lock().unwrap();
                    let open = st.open_pages.clone();
                    let mut list = Vec::new();
                    for tp in open {
                        let page_id = st.ensure_page_id(&tp.page.target_id.0);
                        let sess = st
                            .watchers
                            .get(&tp.lease_id)
                            .and_then(|w| w.cdp_session.clone());
                        list.push((page_id, sess, tp.page));
                    }
                    list
                };
                for (page_id, sess, page) in targets {
                    if let Some(jpeg) = capture_fallback_jpeg(sess.as_ref(), &page).await {
                        self.record_jpeg_frame(&page_id, jpeg, None, None, None);
                    }
                }
            }
        }

        tokio::time::sleep(FLUSH_SETTLE_DELAY).await;

        let bundle = {
            let mut st = self.state.lock().unwrap();
            st.flush_in_flight_network();
            st.chunk_active = false;
            if path.is_some() {
                let mut trace_data = st.trace_lines.join("\n");
                if !trace_data.is_empty() {
                    trace_data.push('\n');
                }
                let mut net_data = st.network_lines.join("\n");
                if !net_data.is_empty() {
                    net_data.push('\n');
                }
                let resources = std::mem::take(&mut st.resources);
                Some((trace_data, net_data, resources))
            } else {
                st.resources.clear();
                None
            }
        };

        if let (Some(dest), Some((trace_data, net_data, resources))) = (path, bundle) {
            let zip_bytes = build_trace_zip(&trace_data, &net_data, &resources)?;
            let dest_owned = dest.to_path_buf();
            tokio::task::spawn_blocking(move || write_zip_file(&dest_owned, &zip_bytes))
                .await
                .map_err(|e| Error::Other(format!("tracing write join error: {e}")))??;
        }
        Ok(())
    }

    async fn teardown_recording(&self) {
        self.active_flag.store(false, Ordering::Release);
        let watches = {
            let mut st = self.state.lock().unwrap();
            st.active = false;
            st.chunk_active = false;
            st.trace_lines.clear();
            st.network_lines.clear();
            st.resources.clear();
            st.in_flight_net.clear();
            std::mem::take(&mut st.watchers)
        };
        for (_, handles) in watches {
            for h in handles.tasks {
                h.abort();
            }
            if let Some(session) = handles.cdp_session {
                let _ = session
                    .send("Page.stopScreencast", json!({}), Duration::from_secs(2))
                    .await;
                let _ = session.detach(Duration::from_secs(2)).await;
            }
        }
    }

    fn record_jpeg_frame(
        &self,
        page_id: &str,
        bytes: Vec<u8>,
        width: Option<f64>,
        height: Option<f64>,
        frame_swap_wall_ms: Option<f64>,
    ) {
        if bytes.is_empty() {
            return;
        }
        let (parsed_w, parsed_h) = jpeg_dimensions(&bytes).unwrap_or((1280, 720));
        let w = width.filter(|v| *v > 0.0).unwrap_or(parsed_w as f64);
        let h = height.filter(|v| *v > 0.0).unwrap_or(parsed_h as f64);
        let hash = sha1_hex(&bytes);
        let sha1_name = format!("{hash}.jpeg");
        let res_path = format!("resources/{sha1_name}");
        let mut st = self.state.lock().unwrap();
        if !st.active || !st.chunk_active {
            return;
        }
        let ts = st.mono_ms();
        let wall = frame_swap_wall_ms.unwrap_or_else(unix_now_ms);
        st.resources.entry(res_path).or_insert(bytes);
        st.chunk_frames += 1;
        st.trace_lines.push(
            json!({
                "type": "screencast-frame",
                "pageId": page_id,
                "sha1": sha1_name,
                "width": w,
                "height": h,
                "timestamp": ts,
                "frameSwapWallTime": wall,
            })
            .to_string(),
        );
    }

    async fn attach_page_watchers(
        self: &Arc<Self>,
        lease_id: Uuid,
        page: CdpPage,
        log: Arc<std::sync::Mutex<PageLog>>,
        page_id: String,
        screenshots: bool,
    ) {
        let (mut log_rx, mut net_rx) = {
            let mut guard = log.lock().unwrap();
            (guard.subscribe(), guard.subscribe_network())
        };

        let mut tasks = Vec::new();

        let me_log = Arc::clone(self);
        let pid_log = page_id.clone();
        tasks.push(tokio::spawn(async move {
            while let Some(entry) = log_rx.recv().await {
                let mut st = me_log.state.lock().unwrap();
                if !st.active || !st.chunk_active {
                    continue;
                }
                let time = st.mono_ms();
                match entry {
                    LogEntry::Console(msg) => {
                        st.trace_lines.push(
                            json!({
                                "type": "console",
                                "messageType": msg.kind,
                                "text": msg.text,
                                "args": [],
                                "location": { "url": "", "lineNumber": 0, "columnNumber": 0 },
                                "pageId": pid_log,
                                "time": time,
                            })
                            .to_string(),
                        );
                    }
                    LogEntry::Error(err) => {
                        st.trace_lines.push(
                            json!({
                                "type": "event",
                                "time": time,
                                "class": "BrowserContext",
                                "method": "pageError",
                                "params": {
                                    "error": {
                                        "error": {
                                            "name": err.name,
                                            "message": err.message,
                                            "stack": err.stack,
                                        }
                                    }
                                },
                                "pageId": pid_log,
                            })
                            .to_string(),
                        );
                    }
                }
            }
        }));

        let me_net = Arc::clone(self);
        let pid_net = page_id.clone();
        tasks.push(tokio::spawn(async move {
            while let Some(ev) = net_rx.recv().await {
                let mut st = me_net.state.lock().unwrap();
                if !st.active || !st.chunk_active {
                    continue;
                }
                let now_ms = st.mono_ms();
                match ev {
                    NetEvent::Request(req) => {
                        let key = (pid_net.clone(), req.id.clone());
                        st.in_flight_net.insert(
                            key,
                            InFlightNet {
                                pageref: pid_net.clone(),
                                started_iso: format_iso8601_utc(SystemTime::now()),
                                start_mono_ms: now_ms,
                                method: req.method,
                                url: req.url,
                                req_headers: req.headers,
                                post_data: req.post_data,
                                response_mono_ms: None,
                                status: 0,
                                status_text: String::new(),
                                resp_headers: Vec::new(),
                                mime_type: String::new(),
                                failed_text: None,
                            },
                        );
                    }
                    NetEvent::Response(resp) => {
                        let key = (pid_net.clone(), resp.request.id.clone());
                        let mime = header_ci(&resp.headers, "content-type")
                            .map(|v| v.split(';').next().unwrap_or(&v).trim().to_string())
                            .unwrap_or_default();
                        let entry = st.in_flight_net.entry(key).or_insert_with(|| {
                            new_inflight_from_req(&pid_net, &resp.request, now_ms)
                        });
                        entry.response_mono_ms = Some(now_ms);
                        entry.status = resp.status;
                        entry.status_text = resp.status_text;
                        entry.resp_headers = resp.headers;
                        entry.mime_type = mime;
                    }
                    NetEvent::Finished(req) => {
                        let key = (pid_net.clone(), req.id.clone());
                        let item = st
                            .in_flight_net
                            .remove(&key)
                            .unwrap_or_else(|| new_inflight_from_req(&pid_net, &req, now_ms));
                        st.network_lines
                            .push(build_resource_snapshot(item, now_ms).to_string());
                    }
                    NetEvent::Failed(req, reason) => {
                        let key = (pid_net.clone(), req.id.clone());
                        let mut item = st
                            .in_flight_net
                            .remove(&key)
                            .unwrap_or_else(|| new_inflight_from_req(&pid_net, &req, now_ms));
                        item.failed_text = Some(reason);
                        st.network_lines
                            .push(build_resource_snapshot(item, now_ms).to_string());
                    }
                }
            }
        }));

        let cdp_session = if screenshots && !page.is_closed() {
            match page.new_cdp_session(CDP_TIMEOUT).await {
                Ok(session) => {
                    let mut cdp_rx = session.subscribe_events();
                    let _ = session.send("Page.enable", json!({}), CDP_TIMEOUT).await;
                    let start_res = session
                        .send(
                            "Page.startScreencast",
                            json!({
                                "format": "jpeg",
                                "quality": 80,
                                "maxWidth": 1280,
                                "maxHeight": 720,
                                "everyNthFrame": 1,
                            }),
                            CDP_TIMEOUT,
                        )
                        .await;
                    if start_res.is_ok() {
                        let me_sc = Arc::clone(self);
                        let sess_sc = session.clone();
                        let pid_sc = page_id.clone();
                        tasks.push(tokio::spawn(async move {
                            while let Some(ev) = cdp_rx.recv().await {
                                if ev.method != "Page.screencastFrame" {
                                    continue;
                                }
                                if let Some(ack_id) = ev.params.get("sessionId").cloned() {
                                    let ack_sess = sess_sc.clone();
                                    tokio::spawn(async move {
                                        let _ = ack_sess
                                            .send(
                                                "Page.screencastFrameAck",
                                                json!({ "sessionId": ack_id }),
                                                Duration::from_secs(2),
                                            )
                                            .await;
                                    });
                                }
                                let Some(b64) = ev.params["data"].as_str() else {
                                    continue;
                                };
                                let Some(bytes) = decode_base64(b64) else {
                                    continue;
                                };
                                let meta = &ev.params["metadata"];
                                let w = meta["deviceWidth"].as_f64();
                                let h = meta["deviceHeight"].as_f64();
                                let wall = meta["timestamp"].as_f64().map(|s| s * 1000.0);
                                me_sc.record_jpeg_frame(&pid_sc, bytes, w, h, wall);
                            }
                        }));
                        Some(session)
                    } else {
                        let _ = session.detach(Duration::from_secs(2)).await;
                        None
                    }
                }
                Err(_) => None,
            }
        } else {
            None
        };

        let mut old_watch = None;
        {
            let mut st = self.state.lock().unwrap();
            if st.active && !st.closed {
                old_watch = st
                    .watchers
                    .insert(lease_id, PageWatchHandles { tasks, cdp_session });
            } else {
                for t in tasks {
                    t.abort();
                }
                if let Some(session) = cdp_session {
                    tokio::spawn(async move {
                        let _ = session
                            .send("Page.stopScreencast", json!({}), Duration::from_secs(2))
                            .await;
                        let _ = session.detach(Duration::from_secs(2)).await;
                    });
                }
            }
        }
        if let Some(old) = old_watch {
            for t in old.tasks {
                t.abort();
            }
            if let Some(s) = old.cdp_session {
                let _ = s.detach(Duration::from_secs(2)).await;
            }
        }
    }
}

async fn capture_fallback_jpeg(session: Option<&CdpSession>, page: &CdpPage) -> Option<Vec<u8>> {
    if page.is_closed() {
        return None;
    }
    if let Some(sess) = session {
        if !sess.is_detached() {
            if let Ok(val) = sess
                .send(
                    "Page.captureScreenshot",
                    json!({ "format": "jpeg", "quality": 80 }),
                    Duration::from_secs(3),
                )
                .await
            {
                if let Some(bytes) = val["data"].as_str().and_then(decode_base64) {
                    return Some(bytes);
                }
            }
        }
    }
    page.screenshot(
        &fluxwright_cdp::ScreenshotOptions {
            format: fluxwright_cdp::ImageFormat::Jpeg,
            quality: Some(80),
            ..Default::default()
        },
        Duration::from_secs(3),
    )
    .await
    .ok()
}

fn new_inflight_from_req(pageref: &str, req: &NetRequest, now_ms: f64) -> InFlightNet {
    InFlightNet {
        pageref: pageref.to_string(),
        started_iso: format_iso8601_utc(SystemTime::now()),
        start_mono_ms: now_ms,
        method: req.method.clone(),
        url: req.url.clone(),
        req_headers: req.headers.clone(),
        post_data: req.post_data.clone(),
        response_mono_ms: None,
        status: 0,
        status_text: String::new(),
        resp_headers: Vec::new(),
        mime_type: String::new(),
        failed_text: None,
    }
}

fn build_resource_snapshot(item: InFlightNet, end_mono_ms: f64) -> Value {
    let total_ms = (end_mono_ms - item.start_mono_ms).max(0.0);
    let wait_ms = item
        .response_mono_ms
        .map(|r| (r - item.start_mono_ms).max(0.0))
        .unwrap_or(total_ms);
    let receive_ms = (total_ms - wait_ms).max(0.0);
    let req_headers: Vec<Value> = item
        .req_headers
        .iter()
        .map(|(name, value)| json!({ "name": name, "value": value }))
        .collect();
    let resp_headers: Vec<Value> = item
        .resp_headers
        .iter()
        .map(|(name, value)| json!({ "name": name, "value": value }))
        .collect();
    let body_size = item.post_data.as_ref().map_or(0, |s| s.len() as i64);
    let mut request_obj = json!({
        "method": item.method,
        "url": item.url,
        "httpVersion": "HTTP/1.1",
        "cookies": [],
        "headers": req_headers,
        "queryString": [],
        "headersSize": -1,
        "bodySize": body_size,
    });
    if let Some(ref post_text) = item.post_data {
        let mime = header_ci(&item.req_headers, "content-type")
            .unwrap_or_else(|| "application/octet-stream".into());
        request_obj["postData"] = json!({ "mimeType": mime, "text": post_text });
    }
    let redirect_url = header_ci(&item.resp_headers, "location").unwrap_or_default();
    let mut snapshot = json!({
        "pageref": item.pageref,
        "startedDateTime": item.started_iso,
        "time": total_ms,
        "_monotonicTime": item.start_mono_ms,
        "request": request_obj,
        "response": {
            "status": item.status,
            "statusText": item.status_text,
            "httpVersion": "HTTP/1.1",
            "cookies": [],
            "headers": resp_headers,
            "content": { "size": 0, "mimeType": item.mime_type },
            "redirectURL": redirect_url,
            "headersSize": -1,
            "bodySize": -1,
        },
        "cache": {},
        "timings": { "send": 0.0, "wait": wait_ms, "receive": receive_ms },
    });
    if let Some(err_text) = item.failed_text {
        snapshot["response"]["_failureText"] = json!(err_text);
    }
    json!({
        "type": "resource-snapshot",
        "snapshot": snapshot,
    })
}

fn header_ci(headers: &[(String, String)], name: &str) -> Option<String> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.clone())
}

fn trace_platform() -> &'static str {
    if cfg!(target_os = "windows") {
        "win32"
    } else if cfg!(target_os = "macos") {
        "darwin"
    } else {
        "linux"
    }
}

fn unix_now_ms() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs_f64()
        * 1000.0
}

fn format_iso8601_utc(time: SystemTime) -> String {
    let dur = time.duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO);
    let total_secs = dur.as_secs() as i64;
    let millis = dur.subsec_millis();
    let (year, month, day, hour, minute, second) = utc_components(total_secs);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

fn utc_components(total_secs: i64) -> (i64, u64, u64, i64, i64, i64) {
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
    (year, m, d, hour, minute, second)
}

fn dos_datetime_now() -> (u16, u16) {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs() as i64;
    let (year, month, day, hour, minute, second) = utc_components(secs);
    let dos_year = (year - 1980).clamp(0, 127) as u16;
    let dos_time = ((hour as u16) << 11) | ((minute as u16) << 5) | ((second as u16) / 2);
    let dos_date = (dos_year << 9) | ((month as u16) << 5) | (day as u16);
    (dos_time, dos_date)
}

fn write_zip_file(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, bytes)?;
    Ok(())
}

fn build_trace_zip(
    trace_ndjson: &str,
    network_ndjson: &str,
    resources: &BTreeMap<String, Vec<u8>>,
) -> Result<Vec<u8>> {
    let mut entries: Vec<(&str, &[u8])> = Vec::with_capacity(2 + resources.len());
    entries.push(("trace.trace", trace_ndjson.as_bytes()));
    entries.push(("trace.network", network_ndjson.as_bytes()));
    for (name, data) in resources {
        entries.push((name.as_str(), data.as_slice()));
    }
    build_zip_store(&entries)
}

/// Minimal pure-Rust ZIP Store writer (CRC32 + local file headers + central directory + EOCD).
pub(crate) fn build_zip_store(entries: &[(&str, &[u8])]) -> Result<Vec<u8>> {
    struct CdRecord<'a> {
        name: &'a [u8],
        crc: u32,
        size: u32,
        offset: u32,
    }

    let count = u16::try_from(entries.len())
        .map_err(|_| Error::Other("trace archive has too many entries".into()))?;
    let (dos_time, dos_date) = dos_datetime_now();

    let approx_cap: usize = entries
        .iter()
        .map(|(n, d)| 96 + n.len() * 2 + d.len())
        .sum();
    let mut out = Vec::with_capacity(approx_cap + 22);
    let mut records = Vec::with_capacity(entries.len());

    for &(name, data) in entries {
        let name_bytes = name.as_bytes();
        let name_len = u16::try_from(name_bytes.len())
            .map_err(|_| Error::Other(format!("zip entry name too long: {name}")))?;
        let size = u32::try_from(data.len())
            .map_err(|_| Error::Other(format!("zip entry too large: {name}")))?;
        let offset = u32::try_from(out.len())
            .map_err(|_| Error::Other("trace archive exceeds 4 GiB ZIP limit".into()))?;
        let crc = crc32(data);

        // Local file header (30 bytes + filename).
        out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes()); // version needed: 2.0
        out.extend_from_slice(&0x0800u16.to_le_bytes()); // UTF-8 filename flag
        out.extend_from_slice(&0u16.to_le_bytes()); // compression: 0 (Store)
        out.extend_from_slice(&dos_time.to_le_bytes());
        out.extend_from_slice(&dos_date.to_le_bytes());
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&size.to_le_bytes());
        out.extend_from_slice(&size.to_le_bytes());
        out.extend_from_slice(&name_len.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // extra field length
        out.extend_from_slice(name_bytes);
        out.extend_from_slice(data);

        records.push(CdRecord {
            name: name_bytes,
            crc,
            size,
            offset,
        });
    }

    let cd_offset = u32::try_from(out.len())
        .map_err(|_| Error::Other("trace archive exceeds 4 GiB ZIP limit".into()))?;

    for rec in &records {
        let name_len = rec.name.len() as u16;
        // Central directory file header (46 bytes + filename).
        out.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        out.extend_from_slice(&0x0314u16.to_le_bytes()); // version made by: Unix 2.0
        out.extend_from_slice(&20u16.to_le_bytes()); // version needed: 2.0
        out.extend_from_slice(&0x0800u16.to_le_bytes()); // UTF-8 filename flag
        out.extend_from_slice(&0u16.to_le_bytes()); // compression: 0 (Store)
        out.extend_from_slice(&dos_time.to_le_bytes());
        out.extend_from_slice(&dos_date.to_le_bytes());
        out.extend_from_slice(&rec.crc.to_le_bytes());
        out.extend_from_slice(&rec.size.to_le_bytes());
        out.extend_from_slice(&rec.size.to_le_bytes());
        out.extend_from_slice(&name_len.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // extra length
        out.extend_from_slice(&0u16.to_le_bytes()); // comment length
        out.extend_from_slice(&0u16.to_le_bytes()); // disk number start
        out.extend_from_slice(&0u16.to_le_bytes()); // internal file attributes
        out.extend_from_slice(&(0o100644u32 << 16).to_le_bytes()); // external file attributes
        out.extend_from_slice(&rec.offset.to_le_bytes());
        out.extend_from_slice(rec.name);
    }

    let cd_size = u32::try_from(out.len() - cd_offset as usize)
        .map_err(|_| Error::Other("trace archive central directory exceeds 4 GiB".into()))?;

    // End of central directory record (22 bytes).
    out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // disk number
    out.extend_from_slice(&0u16.to_le_bytes()); // start disk
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&cd_size.to_le_bytes());
    out.extend_from_slice(&cd_offset.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // comment length

    Ok(out)
}

const CRC32_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = i as u32;
        let mut j = 0;
        while j < 8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ 0xEDB8_8320;
            } else {
                crc >>= 1;
            }
            j += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
};

pub(crate) fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc = CRC32_TABLE[((crc ^ u32::from(b)) & 0xFF) as usize] ^ (crc >> 8);
    }
    !crc
}

/// Pure-Rust SHA-1 (RFC 3174) returning 40 lowercase hex digits.
pub(crate) fn sha1_hex(data: &[u8]) -> String {
    let mut h0: u32 = 0x6745_2301;
    let mut h1: u32 = 0xEFCD_AB89;
    let mut h2: u32 = 0x98BA_DCFE;
    let mut h3: u32 = 0x1032_5476;
    let mut h4: u32 = 0xC3D2_E1F0;

    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut msg = Vec::with_capacity((data.len() + 9).div_ceil(64) * 64);
    msg.extend_from_slice(data);
    msg.push(0x80);
    while (msg.len() % 64) != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in msg.chunks_exact(64) {
        let mut w = [0u32; 80];
        for (i, word) in w.iter_mut().take(16).enumerate() {
            let j = i * 4;
            *word = u32::from_be_bytes([chunk[j], chunk[j + 1], chunk[j + 2], chunk[j + 3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }

        let (mut a, mut b, mut c, mut d, mut e) = (h0, h1, h2, h3, h4);
        for (i, &wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5A82_7999u32),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1u32),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                _ => (b ^ c ^ d, 0xCA62_C1D6),
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }

        h0 = h0.wrapping_add(a);
        h1 = h1.wrapping_add(b);
        h2 = h2.wrapping_add(c);
        h3 = h3.wrapping_add(d);
        h4 = h4.wrapping_add(e);
    }

    format!("{h0:08x}{h1:08x}{h2:08x}{h3:08x}{h4:08x}")
}

fn decode_base64(input: &str) -> Option<Vec<u8>> {
    fn val(b: u8) -> Option<u8> {
        match b {
            b'A'..=b'Z' => Some(b - b'A'),
            b'a'..=b'z' => Some(b - b'a' + 26),
            b'0'..=b'9' => Some(b - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }

    let bytes: Vec<u8> = input.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    if bytes.len() % 4 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity((bytes.len() / 4) * 3);
    for quad in bytes.chunks_exact(4) {
        let a = val(quad[0])?;
        let b = val(quad[1])?;
        let c = if quad[2] == b'=' {
            None
        } else {
            Some(val(quad[2])?)
        };
        let d = if quad[3] == b'=' {
            None
        } else {
            Some(val(quad[3])?)
        };
        out.push((a << 2) | (b >> 4));
        if let Some(cv) = c {
            out.push((b << 4) | (cv >> 2));
            if let Some(dv) = d {
                out.push((cv << 6) | dv);
            }
        } else if d.is_some() {
            return None;
        }
    }
    Some(out)
}

fn jpeg_dimensions(data: &[u8]) -> Option<(u16, u16)> {
    if data.len() < 4 || data[0] != 0xFF || data[1] != 0xD8 {
        return None;
    }
    let mut i = 2usize;
    while i + 3 < data.len() {
        if data[i] != 0xFF {
            i += 1;
            continue;
        }
        while i < data.len() && data[i] == 0xFF {
            i += 1;
        }
        if i >= data.len() {
            break;
        }
        let marker = data[i];
        i += 1;
        if marker == 0xD8 || marker == 0xD9 || marker == 0x00 || (0xD0..=0xD7).contains(&marker) {
            continue;
        }
        if i + 1 >= data.len() {
            break;
        }
        let seg_len = u16::from_be_bytes([data[i], data[i + 1]]) as usize;
        if seg_len < 2 || i + seg_len > data.len() {
            break;
        }
        if matches!(marker, 0xC0 | 0xC1 | 0xC2 | 0xC3) && seg_len >= 7 {
            let h = u16::from_be_bytes([data[i + 3], data[i + 4]]);
            let w = u16::from_be_bytes([data[i + 5], data[i + 6]]);
            if w > 0 && h > 0 {
                return Some((w, h));
            }
        }
        i += seg_len;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_and_sha1_match_standard_vectors() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(sha1_hex(b""), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(sha1_hex(b"abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
    }

    #[test]
    fn base64_and_zip_store_round_trip() {
        assert_eq!(decode_base64("SGVsbG8=").unwrap(), b"Hello");
        let zip = build_zip_store(&[
            ("trace.trace", b"{\"type\":\"context-options\"}\n"),
            ("trace.network", b""),
            ("resources/test.jpeg", &[0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10]),
        ])
        .unwrap();
        assert_eq!(&zip[0..4], b"PK\x03\x04");
        let eocd = &zip[zip.len() - 22..];
        assert_eq!(&eocd[0..4], b"PK\x05\x06");
        assert_eq!(u16::from_le_bytes([eocd[8], eocd[9]]), 3);
        assert_eq!(u16::from_le_bytes([eocd[10], eocd[11]]), 3);
    }
}
