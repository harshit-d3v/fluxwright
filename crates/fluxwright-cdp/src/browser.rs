use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::process::Child;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::connection::{CdpEvent, Connection};
use crate::context::{
    filter_cookies, permission_types, Cookie, CookieFilter, Emulation, StorageState,
};
use crate::error::{Error, LaunchOptions, Result};
use crate::har::{HarRecorder, RecordHarOptions};
use crate::http_client::{
    send_context_request, ApiRequestOptions, ApiResponse, HttpContextDefaults,
};
use crate::launch::{launch_chrome, Launched, Transport};
use crate::page::{fetch_params_combined, CdpPage, ContextSetup, ResourceType};
use crate::route::InterceptedRequest;
use crate::worker::{Worker, WorkerRegistry};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BrowserId(pub Uuid);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ContextId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TargetId(pub String);

/// A proxy for one browser context (one lease), like Playwright's `newContext({ proxy })`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Proxy {
    /// `http://host:port`, `socks5://host:port`, ...
    pub server: String,
    /// Comma-separated hosts that skip the proxy, e.g. `.internal.example,localhost`.
    pub bypass: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
}

impl Proxy {
    pub fn new(server: impl Into<String>) -> Self {
        Self {
            server: server.into(),
            ..Self::default()
        }
    }

    /// Credentials, answered when the proxy asks (HTTP 407).
    pub fn auth(mut self, username: impl Into<String>, password: impl Into<String>) -> Self {
        self.username = Some(username.into());
        self.password = Some(password.into());
        self
    }

    pub fn bypass(mut self, hosts: impl Into<String>) -> Self {
        self.bypass = Some(hosts.into());
        self
    }
}

pub(crate) struct NetActivity {
    pub(crate) inflight: HashSet<String>,
    pub(crate) last_change: std::time::Instant,
    pub(crate) idle_confirmed: bool,
}

impl Default for NetActivity {
    fn default() -> Self {
        Self {
            inflight: HashSet::new(),
            last_change: std::time::Instant::now(),
            idle_confirmed: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameInfo {
    pub id: String,
    pub parent_id: Option<String>,
    pub name: String,
    pub url: String,
}

#[derive(Debug, Clone)]
pub(crate) struct FrameEntry {
    pub(crate) id: String,
    pub(crate) parent_id: Option<String>,
    pub(crate) name: String,
    pub(crate) url: String,
    pub(crate) session_id: String,
    pub(crate) default_context_id: Option<i64>,
}

#[derive(Debug, Default)]
pub(crate) struct FrameTree {
    order: Vec<String>,
    frames: HashMap<String, FrameEntry>,
}

impl FrameTree {
    pub(crate) fn new(main_id: String, session_id: String, url: String) -> Self {
        let mut tree = Self::default();
        let initial_url = if url.is_empty() { "about:blank".into() } else { url };
        tree.order.push(main_id.clone());
        tree.frames.insert(
            main_id.clone(),
            FrameEntry {
                id: main_id,
                parent_id: None,
                name: String::new(),
                url: initial_url,
                session_id,
                default_context_id: None,
            },
        );
        tree
    }

    pub(crate) fn list(&self) -> Vec<FrameInfo> {
        self.order
            .iter()
            .filter_map(|id| self.frames.get(id))
            .map(|f| FrameInfo {
                id: f.id.clone(),
                parent_id: f.parent_id.clone(),
                name: f.name.clone(),
                url: f.url.clone(),
            })
            .collect()
    }

    pub(crate) fn get(&self, id: &str) -> Option<&FrameEntry> {
        self.frames.get(id)
    }

    pub(crate) fn set_root_session(&mut self, main_id: &str, session_id: String, url: &str) {
        if let Some(entry) = self.frames.get_mut(main_id) {
            entry.session_id = session_id;
            if !url.is_empty() {
                entry.url = url.to_owned();
            }
        } else {
            *self = Self::new(main_id.to_owned(), session_id, url.to_owned());
        }
    }

    pub(crate) fn ancestors(&self, id: &str) -> Option<Vec<(String, String)>> {
        if !self.frames.contains_key(id) {
            return None;
        }
        let mut hops = Vec::new();
        let mut cur = id;
        for _ in 0..=self.frames.len() {
            let entry = self.frames.get(cur)?;
            match &entry.parent_id {
                Some(parent) => {
                    if !self.frames.contains_key(parent) {
                        return None;
                    }
                    hops.push((parent.clone(), cur.to_string()));
                    cur = parent.as_str();
                }
                None => {
                    hops.reverse();
                    return Some(hops);
                }
            }
        }
        None
    }

    pub(crate) fn attach(&mut self, id: String, parent_id: String, session_id: String) {
        if let Some(entry) = self.frames.get_mut(&id) {
            if entry.parent_id.is_none() {
                entry.parent_id = Some(parent_id);
            }
            if entry.session_id.is_empty() {
                entry.session_id = session_id;
            }
            return;
        }
        self.order.push(id.clone());
        self.frames.insert(
            id.clone(),
            FrameEntry {
                id,
                parent_id: Some(parent_id),
                name: String::new(),
                url: "about:blank".into(),
                session_id,
                default_context_id: None,
            },
        );
    }

    pub(crate) fn navigate(
        &mut self,
        id: String,
        parent_id: Option<String>,
        name: Option<String>,
        url: String,
        session_id: String,
        main_id: &str,
    ) {
        let resolved_parent = if id == main_id { None } else { parent_id };
        if let Some(entry) = self.frames.get_mut(&id) {
            if !url.is_empty() {
                entry.url = url;
            }
            if let Some(n) = name {
                entry.name = n;
            }
            // OOPIF sessions emit Page.frameNavigated with parentId = null; never overwrite a known parent.
            if let Some(p) = resolved_parent {
                entry.parent_id = Some(p);
            }
            entry.session_id = session_id;
            return;
        }
        self.order.push(id.clone());
        self.frames.insert(
            id.clone(),
            FrameEntry {
                id,
                parent_id: resolved_parent,
                name: name.unwrap_or_default(),
                url: if url.is_empty() { "about:blank".into() } else { url },
                session_id,
                default_context_id: None,
            },
        );
    }

    pub(crate) fn update_url(&mut self, id: &str, url: String) {
        if !url.is_empty() {
            if let Some(entry) = self.frames.get_mut(id) {
                entry.url = url;
            }
        }
    }

    pub(crate) fn set_session(
        &mut self,
        id: String,
        session_id: String,
        fallback_parent: Option<String>,
        url: Option<String>,
    ) {
        if let Some(entry) = self.frames.get_mut(&id) {
            entry.session_id = session_id;
            entry.default_context_id = None;
            if entry.parent_id.is_none() {
                entry.parent_id = fallback_parent;
            }
            if let Some(u) = url.filter(|s| !s.is_empty()) {
                entry.url = u;
            }
            return;
        }
        self.order.push(id.clone());
        self.frames.insert(
            id.clone(),
            FrameEntry {
                id,
                parent_id: fallback_parent,
                name: String::new(),
                url: url.filter(|s| !s.is_empty()).unwrap_or_else(|| "about:blank".into()),
                session_id,
                default_context_id: None,
            },
        );
    }

    pub(crate) fn detach(&mut self, id: &str) -> Vec<String> {
        let mut dead = HashSet::from([id.to_string()]);
        loop {
            let mut added = false;
            for (fid, entry) in &self.frames {
                if !dead.contains(fid)
                    && entry.parent_id.as_ref().is_some_and(|p| dead.contains(p))
                {
                    dead.insert(fid.clone());
                    added = true;
                }
            }
            if !added {
                break;
            }
        }
        for fid in &dead {
            self.frames.remove(fid);
        }
        self.order.retain(|fid| !dead.contains(fid));
        dead.into_iter().collect()
    }

    pub(crate) fn set_default_context(&mut self, frame_id: &str, session_id: &str, ctx_id: i64) {
        if let Some(entry) = self.frames.get_mut(frame_id) {
            if entry.session_id == session_id || entry.session_id.is_empty() {
                entry.default_context_id = Some(ctx_id);
            }
        }
    }

    pub(crate) fn remove_context(&mut self, ctx_id: i64) {
        for entry in self.frames.values_mut() {
            if entry.default_context_id == Some(ctx_id) {
                entry.default_context_id = None;
            }
        }
    }

    pub(crate) fn clear_session_contexts(&mut self, session_id: &str) -> Vec<String> {
        let mut cleared = Vec::new();
        for entry in self.frames.values_mut() {
            if entry.session_id == session_id {
                entry.default_context_id = None;
                cleared.push(entry.id.clone());
            }
        }
        cleared
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct PageSetup {
    pub(crate) routes: Option<tokio::sync::mpsc::UnboundedSender<InterceptedRequest>>,
    pub(crate) dialogs: Option<tokio::sync::mpsc::UnboundedSender<crate::events::Dialog>>,
    pub(crate) file_choosers: Option<tokio::sync::mpsc::UnboundedSender<crate::events::FileChooser>>,
    pub(crate) bindings: Option<tokio::sync::mpsc::UnboundedSender<crate::events::BindingCall>>,
    pub(crate) bindings_enabled: bool,
    pub(crate) exposed_bindings: HashSet<String>,
    pub(crate) init_scripts: Vec<String>,
    pub(crate) popups: Option<tokio::sync::mpsc::UnboundedSender<CdpPage>>,
}

pub(crate) struct TargetState {
    session_id: Option<String>,
    /// Populated immediately on Target.attachedToTarget so events emitted during prepare_session see this session.
    active_session_id: Option<String>,
    r#type: String,
    url: String,
    url_tx: Arc<tokio::sync::watch::Sender<String>>,
    net: Arc<std::sync::Mutex<NetActivity>>,
    browser_context_id: Option<String>,
    opener_id: Option<String>,
    owner_page: Option<String>,
    frames: Arc<std::sync::Mutex<FrameTree>>,
    workers: Arc<std::sync::Mutex<WorkerRegistry>>,
    worlds: Arc<std::sync::Mutex<HashMap<String, i64>>>,
    closed: Arc<AtomicBool>,
    waiting: Vec<tokio::sync::oneshot::Sender<Result<String>>>,
}

impl TargetState {
    #[allow(clippy::too_many_arguments)]
    fn new(
        target_id: &str,
        r#type: String,
        url: String,
        browser_context_id: Option<String>,
        opener_id: Option<String>,
        owner_page: Option<String>,
        frames: Option<Arc<std::sync::Mutex<FrameTree>>>,
        workers: Option<Arc<std::sync::Mutex<WorkerRegistry>>>,
        worlds: Option<Arc<std::sync::Mutex<HashMap<String, i64>>>>,
    ) -> Self {
        let initial = if url.is_empty() { "about:blank".into() } else { url.clone() };
        let frames = frames.unwrap_or_else(|| {
            Arc::new(std::sync::Mutex::new(if r#type == "page" {
                FrameTree::new(target_id.to_owned(), String::new(), initial.clone())
            } else {
                FrameTree::default()
            }))
        });
        let workers = workers.unwrap_or_default();
        let worlds = worlds.unwrap_or_default();
        Self {
            session_id: None,
            active_session_id: None,
            r#type,
            url,
            url_tx: Arc::new(tokio::sync::watch::Sender::new(initial)),
            net: Default::default(),
            browser_context_id,
            opener_id,
            owner_page,
            frames,
            workers,
            worlds,
            closed: Arc::new(AtomicBool::new(false)),
            waiting: Vec::new(),
        }
    }
}

pub(crate) fn frame_url(frame: &Value) -> String {
    let base = frame["url"].as_str().unwrap_or("");
    let frag = frame["urlFragment"].as_str().unwrap_or("");
    if !frag.is_empty() && !base.contains('#') {
        format!("{base}{frag}")
    } else {
        base.to_string()
    }
}

pub struct CdpBrowser {
    pub id: BrowserId,
    pub pid: u32,
    pub headless: bool,
    pub ws_endpoint: Option<String>,
    conn: Arc<Connection>,
    _child: Mutex<Option<Child>>,
    _profile: Option<tempfile::TempDir>,
    targets: Arc<Mutex<HashMap<String, TargetState>>>,
    /// Blocking and proxy credentials per browser context, so iframes and tabs that attach later
    /// get them too.
    setups: Arc<Mutex<HashMap<String, ContextSetup>>>,
    /// Per-page setup (routes, dialogs, file choosers, popups, page-scoped bindings/init scripts).
    page_setups: Arc<Mutex<HashMap<String, PageSetup>>>,
    /// Browser contexts this connection created. On a remote browser (`connect`), targets in
    /// any other context belong to someone else: attaching prepares their pages and our dialog
    /// and route handlers then answer theirs.
    contexts: Arc<Mutex<HashSet<String>>>,
    /// Browser contexts delegated to external CDP gateway clients (not prepared by `CdpBrowser`).
    external_contexts: Arc<std::sync::Mutex<HashSet<String>>>,
    /// Sessions owned by external CDP gateway clients (child targets belong to those clients).
    external_sessions: Arc<std::sync::Mutex<HashSet<String>>>,
    /// Dedicated worker session_id -> (worker target_id, owning page worker registry).
    worker_sessions: Arc<std::sync::Mutex<HashMap<String, (String, Arc<std::sync::Mutex<WorkerRegistry>>)>>>,
}

impl CdpBrowser {
    pub async fn launch(opts: LaunchOptions) -> Result<Arc<Self>> {
        let launched = launch_chrome(&opts).await?;
        Self::from_launched(launched, opts.headless).await
    }

    async fn from_launched(launched: Launched, headless: bool) -> Result<Arc<Self>> {
        let (conn, ws_endpoint) = match launched.transport {
            #[cfg(windows)]
            Transport::WebSocket(url) => {
                let conn = Connection::connect(&url).await?;
                (conn, Some(url))
            }
            #[cfg(unix)]
            Transport::Pipe {
                to_chrome,
                from_chrome,
            } => {
                let conn = Connection::from_pipes(to_chrome, from_chrome)?;
                (conn, None)
            }
        };
        // Over a pipe nothing has waited for Chrome to start yet. A first, cold start on a busy CI
        // machine has taken over 20 s.
        conn.call(
            "Browser.getVersion",
            json!({}),
            None,
            Duration::from_secs(60),
        )
        .await
        .map_err(|e| {
            Error::Launch(format!(
                "chromium did not answer on its DevTools connection: {e}"
            ))
        })?;
        let browser = Arc::new(Self {
            id: BrowserId(Uuid::new_v4()),
            pid: launched.pid,
            headless,
            ws_endpoint,
            conn: conn.clone(),
            _child: Mutex::new(Some(launched.child)),
            _profile: Some(launched.user_data_dir),
            targets: Arc::new(Mutex::new(HashMap::new())),
            setups: Default::default(),
            page_setups: Default::default(),
            contexts: Default::default(),
            external_contexts: Default::default(),
            external_sessions: Default::default(),
            worker_sessions: Default::default(),
        });

        let b = browser.clone();
        let mut sub = conn.subscribe_unbounded();
        tokio::spawn(async move {
            while let Some(ev) = sub.recv().await {
                b.on_event(&ev).await;
            }
        });

        let waiter = browser.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(200)).await;
                let mut child = waiter._child.lock().await;
                match child.as_mut().map(|c| c.try_wait()) {
                    Some(Ok(Some(status))) => {
                        drop(child);
                        warn!(pid = waiter.pid, ?status, "chromium process exited");
                        waiter.conn.mark_dead();
                        break;
                    }
                    Some(Ok(None)) => {}
                    Some(Err(_)) | None => break,
                }
            }
        });

        let handshake = async {
            browser
                .conn
                .call(
                    "Target.setDiscoverTargets",
                    json!({ "discover": true }),
                    None,
                    Duration::from_secs(5),
                )
                .await?;
            browser
                .conn
                .call(
                    "Target.setAutoAttach",
                    json!({
                        "autoAttach": true,
                        "waitForDebuggerOnStart": true,
                        "flatten": true
                    }),
                    None,
                    Duration::from_secs(5),
                )
                .await
        };
        if let Err(e) = handshake.await {
            // The tasks above hold the browser: end Chrome so they end too and it is freed.
            let _ = browser.kill_process().await;
            return Err(e);
        }

        info!(browser_id = %browser.id.0, pid = browser.pid, "browser ready");
        Ok(browser)
    }

    /// Connects directly to an existing Chrome DevTools Protocol endpoint (e.g. `ws://...` or `http://...`).
    ///
    /// Does not spawn a local child process or manage user profile directories. Runs target discovery
    /// and auto-attach handshake.
    pub async fn connect(endpoint_url: &str) -> Result<Arc<Self>> {
        let ws_url = resolve_ws_endpoint(endpoint_url).await?;
        let conn = Connection::connect(&ws_url).await?;
        if let Err(e) = conn
            .call(
                "Browser.getVersion",
                json!({}),
                None,
                Duration::from_secs(30),
            )
            .await
        {
            // A failed handshake must not leave the socket and its tasks running.
            conn.close();
            return Err(Error::Launch(format!(
                "remote chromium did not answer on its DevTools connection: {e}"
            )));
        }

        let browser = Arc::new(Self {
            id: BrowserId(Uuid::new_v4()),
            pid: 0,
            headless: true,
            ws_endpoint: Some(ws_url),
            conn: conn.clone(),
            _child: Mutex::new(None),
            _profile: None,
            targets: Arc::new(Mutex::new(HashMap::new())),
            setups: Default::default(),
            page_setups: Default::default(),
            contexts: Default::default(),
            external_contexts: Default::default(),
            external_sessions: Default::default(),
            worker_sessions: Default::default(),
        });

        let b = browser.clone();
        let mut sub = conn.subscribe_unbounded();
        tokio::spawn(async move {
            while let Some(ev) = sub.recv().await {
                b.on_event(&ev).await;
            }
        });

        let handshake = async {
            browser
                .conn
                .call(
                    "Target.setDiscoverTargets",
                    json!({ "discover": true }),
                    None,
                    Duration::from_secs(10),
                )
                .await?;
            browser
                .conn
                .call(
                    "Target.setAutoAttach",
                    json!({
                        "autoAttach": true,
                        "waitForDebuggerOnStart": true,
                        "flatten": true
                    }),
                    None,
                    Duration::from_secs(10),
                )
                .await
        };

        if let Err(e) = handshake.await {
            // The event loop above holds the browser until the connection ends.
            browser.conn.close();
            return Err(e);
        }
        spawn_keepalive(browser.conn.clone());

        info!(browser_id = %browser.id.0, "remote browser ready over CDP");
        Ok(browser)
    }

    pub fn ws_endpoint(&self) -> Option<&str> {
        self.ws_endpoint.as_deref()
    }

    pub fn is_remote(&self) -> bool {
        self.pid == 0
    }

    pub fn connection_dead(&self) -> bool {
        self.conn.is_dead()
    }

    pub fn connection(&self) -> Arc<Connection> {
        self.conn.clone()
    }

    pub fn mark_external_context(&self, ctx_id: &str) {
        self.external_contexts
            .lock()
            .unwrap()
            .insert(ctx_id.to_string());
    }

    pub fn unmark_external_context(&self, ctx_id: &str) {
        self.external_contexts.lock().unwrap().remove(ctx_id);
    }

    pub fn mark_external_session(&self, session_id: &str) {
        self.external_sessions
            .lock()
            .unwrap()
            .insert(session_id.to_string());
    }

    pub fn unmark_external_session(&self, session_id: &str) {
        self.external_sessions.lock().unwrap().remove(session_id);
    }

    async fn on_event(self: &Arc<Self>, ev: &CdpEvent) {
        match ev.method.as_str() {
            "Target.attachedToTarget" => {
                if ev
                    .session_id
                    .as_deref()
                    .is_some_and(|s| self.external_sessions.lock().unwrap().contains(s))
                {
                    return;
                }
                let info = &ev.params["targetInfo"];
                let target_id = info["targetId"].as_str().unwrap_or_default().to_string();
                let session_id = ev.params["sessionId"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                let ty = info["type"].as_str().unwrap_or("").to_string();
                let url = info["url"].as_str().unwrap_or("").to_string();
                let worker_url = url.clone();
                let ctx = info["browserContextId"].as_str().map(|s| s.to_string());
                let opener_from_info = info["openerId"].as_str().map(|s| s.to_string());
                let opener_frame = info["openerFrameId"].as_str().map(|s| s.to_string());
                let waiting = ev.params["waitingForDebugger"].as_bool().unwrap_or(false);
                if ctx
                    .as_deref()
                    .is_some_and(|c| self.external_contexts.lock().unwrap().contains(c))
                {
                    if waiting {
                        let conn = self.conn.clone();
                        tokio::spawn(async move {
                            let t = Duration::from_secs(5);
                            let _ = conn
                                .call("Runtime.runIfWaitingForDebugger", json!({}), Some(&session_id), t)
                                .await;
                            let _ = conn
                                .call(
                                    "Target.detachFromTarget",
                                    json!({ "sessionId": session_id }),
                                    None,
                                    t,
                                )
                                .await;
                        });
                    }
                    return;
                }
                let already_attached = self.targets.lock().await.get(&target_id).is_some_and(|t| {
                    t.active_session_id.is_some()
                        && t.active_session_id.as_deref() != Some(session_id.as_str())
                });
                if already_attached && !waiting {
                    // Secondary session (e.g. Target.attachToTarget from newCDPSession): keep primary session intact.
                    return;
                }
                debug!(%target_id, %session_id, %ty, "attachedToTarget");

                let (owner_page, effective_opener, effective_ctx, url_tx, net, frames, workers, worlds, closed) = {
                    let mut targets = self.targets.lock().await;
                    let (detected_owner, owner_frames, owner_workers, owner_worlds, parent_ctx, fallback_parent) = if ty == "page" {
                        (Some(target_id.clone()), None, None, None, None, None)
                    } else if ty == "iframe" {
                        let parent_by_sid = ev.session_id.as_deref().and_then(|psid| {
                            targets.iter().find_map(|(pid, pt)| {
                                (pt.active_session_id.as_deref() == Some(psid)
                                    || pt.session_id.as_deref() == Some(psid))
                                .then(|| (pt.owner_page.clone(), pt.frames.clone(), pt.workers.clone(), pt.worlds.clone(), pt.browser_context_id.clone(), opener_frame.clone().or_else(|| Some(pid.clone()))))
                            })
                        });
                        if let Some((owner, fr, wk, wd, pctx, par)) = parent_by_sid {
                            (owner, Some(fr), Some(wk), Some(wd), pctx, par)
                        } else {
                            let by_frame_tree = targets.iter().find_map(|(tid, pt)| {
                                (pt.r#type == "page"
                                    && pt.browser_context_id == ctx
                                    && pt.frames.lock().unwrap().get(&target_id).is_some())
                                .then(|| (Some(tid.clone()), pt.frames.clone(), pt.workers.clone(), pt.worlds.clone(), pt.browser_context_id.clone(), opener_frame.clone()))
                            });
                            if let Some((owner, fr, wk, wd, pctx, par)) = by_frame_tree {
                                (owner, Some(fr), Some(wk), Some(wd), pctx, par)
                            } else {
                                targets
                                    .iter()
                                    .find_map(|(tid, pt)| {
                                        (pt.r#type == "page" && pt.browser_context_id == ctx)
                                            .then(|| (Some(tid.clone()), Some(pt.frames.clone()), Some(pt.workers.clone()), Some(pt.worlds.clone()), pt.browser_context_id.clone(), opener_frame.clone().or_else(|| Some(tid.clone()))))
                                    })
                                    .unwrap_or((None, None, None, None, None, opener_frame.clone()))
                            }
                        }
                    } else if ty == "worker" {
                        ev.session_id
                            .as_deref()
                            .and_then(|psid| {
                                targets.values().find_map(|pt| {
                                    (pt.active_session_id.as_deref() == Some(psid)
                                        || pt.session_id.as_deref() == Some(psid))
                                    .then(|| {
                                        (
                                            pt.owner_page.clone(),
                                            Some(pt.frames.clone()),
                                            Some(pt.workers.clone()),
                                            Some(pt.worlds.clone()),
                                            pt.browser_context_id.clone(),
                                            None,
                                        )
                                    })
                                })
                            })
                            .unwrap_or((None, None, None, None, None, None))
                    } else {
                        (None, None, None, None, None, None)
                    };

                    let effective_ctx = ctx.clone().or(parent_ctx);
                    let state = targets.entry(target_id.clone()).or_insert_with(|| {
                        TargetState::new(
                            &target_id,
                            ty.clone(),
                            url.clone(),
                            effective_ctx.clone(),
                            opener_from_info.clone(),
                            detected_owner.clone(),
                            owner_frames.clone(),
                            owner_workers.clone(),
                            owner_worlds.clone(),
                        )
                    });
                    state.r#type = ty.clone();
                    state.active_session_id = Some(session_id.clone());
                    if state.opener_id.is_none() {
                        state.opener_id = opener_from_info;
                    }
                    if state.owner_page.is_none() {
                        state.owner_page = detected_owner;
                    }
                    if let Some(fr) = owner_frames {
                        state.frames = fr;
                    }
                    if let Some(wk) = owner_workers {
                        state.workers = wk;
                    }
                    if let Some(wd) = owner_worlds {
                        state.worlds = wd;
                    }
                    if ty == "page" {
                        state.frames.lock().unwrap().set_root_session(&target_id, session_id.clone(), &url);
                    } else if ty == "iframe" {
                        state.frames.lock().unwrap().set_session(
                            target_id.clone(),
                            session_id.clone(),
                            fallback_parent,
                            (!url.is_empty()).then(|| url.clone()),
                        );
                    }
                    if !url.is_empty() {
                        state.url = url.clone();
                        state.url_tx.send_if_modified(|cur| {
                            if *cur != url {
                                *cur = url;
                                true
                            } else {
                                false
                            }
                        });
                    }
                    state.browser_context_id = effective_ctx.clone();
                    (
                        state.owner_page.clone(),
                        state.opener_id.clone(),
                        effective_ctx,
                        state.url_tx.clone(),
                        state.net.clone(),
                        state.frames.clone(),
                        state.workers.clone(),
                        state.worlds.clone(),
                        state.closed.clone(),
                    )
                };

                // Pages, out-of-process iframes, and dedicated workers count only in a context this
                // connection created; anything else on a remote browser belongs to its owner.
                let ours = match &effective_ctx {
                    Some(c) => self.contexts.lock().await.contains(c),
                    None => false,
                };
                let is_our_worker = ty == "worker" && ours && owner_page.is_some();

                let setup = match &effective_ctx {
                    Some(c) => self.setups.lock().await.get(c).cloned(),
                    None => None,
                };
                let page_setup = {
                    let ps = self.page_setups.lock().await;
                    if ty == "iframe" {
                        owner_page.as_deref().and_then(|op| ps.get(op).cloned())
                    } else if ty == "page" {
                        // A popup without its own route handler can still match its opener's routes.
                        effective_opener
                            .as_deref()
                            .and_then(|op| ps.get(op).filter(|p| p.routes.is_some()).map(|p| PageSetup { routes: p.routes.clone(), ..Default::default() }))
                    } else {
                        None
                    }
                };

                // Do not await CDP on this task: it is the only consumer of
                // attachedToTarget and must stay ahead of createTarget.
                if (ty != "page" && ty != "iframe" && !is_our_worker) || !ours {
                    // Chrome's own targets (browser_ui, extension/service workers, background pages):
                    // nothing drives them, and setup hung on ~1 in 3 browser_ui sessions. Detach, as
                    // Playwright does; resume first in case one is paused.
                    let conn = self.conn.clone();
                    tokio::spawn(async move {
                        let t = Duration::from_secs(5);
                        if waiting {
                            let _ = conn
                                .call(
                                    "Runtime.runIfWaitingForDebugger",
                                    json!({}),
                                    Some(&session_id),
                                    t,
                                )
                                .await;
                        }
                        let _ = conn
                            .call(
                                "Target.detachFromTarget",
                                json!({ "sessionId": session_id }),
                                None,
                                t,
                            )
                            .await;
                    });
                    return;
                }
                if is_our_worker {
                    self.worker_sessions
                        .lock()
                        .unwrap()
                        .insert(session_id.clone(), (target_id.clone(), workers.clone()));
                    let me = self.clone();
                    tokio::spawn(async move {
                        match prepare_worker_session(&me.conn, &session_id, waiting).await {
                            Ok(()) => {
                                let still_present = match me.targets.lock().await.get_mut(&target_id) {
                                    Some(t) => {
                                        t.session_id = Some(session_id.clone());
                                        true
                                    }
                                    None => false,
                                };
                                if still_present {
                                    let w = Worker::new(
                                        target_id,
                                        session_id,
                                        worker_url,
                                        me.conn.clone(),
                                        me.pid,
                                    );
                                    workers.lock().unwrap().add(w);
                                } else {
                                    me.worker_sessions.lock().unwrap().remove(&session_id);
                                }
                            }
                            Err(_) => {
                                me.worker_sessions.lock().unwrap().remove(&session_id);
                                let _ = me
                                    .conn
                                    .call(
                                        "Target.detachFromTarget",
                                        json!({ "sessionId": session_id }),
                                        None,
                                        Duration::from_secs(5),
                                    )
                                    .await;
                            }
                        }
                    });
                    return;
                }
                // The session is published (and waiters woken) only once prepared, so a
                // page never reaches a caller with its domains still being enabled.
                // Subscribe before resuming a popup so its initial console/network events
                // are buffered until `collect_logs` takes ownership of the stream.
                let early_log_events = if ty == "page" && effective_opener.is_some() {
                    Some(self.conn.subscribe_unbounded())
                } else {
                    None
                };
                let me = self.clone();
                tokio::spawn(async move {
                    match prepare_session(&me.conn, &session_id, &ty, waiting, setup, page_setup).await {
                        Ok(()) => {
                            let waiters = match me.targets.lock().await.get_mut(&target_id) {
                                Some(t) => {
                                    t.session_id = Some(session_id.clone());
                                    std::mem::take(&mut t.waiting)
                                }
                                None => Vec::new(), // destroyed while preparing
                            };
                            for w in waiters {
                                let _ = w.send(Ok(session_id.clone()));
                            }
                            if ty == "page" {
                                if let (Some(opener), Some(ctx_id)) = (effective_opener, effective_ctx) {
                                    let popup_tx = me
                                        .page_setups
                                        .lock()
                                        .await
                                        .get(&opener)
                                        .and_then(|p| p.popups.clone());
                                    if let Some(tx) = popup_tx {
                                        let page = CdpPage {
                                            browser: me.conn.clone(),
                                            browser_pid: me.pid,
                                            browser_id: me.id,
                                            headless: me.headless,
                                            is_remote: me.is_remote(),
                                            context_id: ContextId(ctx_id),
                                            target_id: TargetId(target_id),
                                            opener_id: Some(TargetId(opener)),
                                            session_id: SessionId(session_id),
                                            targets: me.targets.clone(),
                                            worlds,
                                            setups: me.setups.clone(),
                                            page_setups: me.page_setups.clone(),
                                            frames,
                                            workers,
                                            closed,
                                            mouse: Default::default(),
                                            keyboard: Default::default(),
                                            url: url_tx,
                                            net,
                                            ax_refs: Default::default(),
                                            early_log_events: Arc::new(std::sync::Mutex::new(
                                                early_log_events,
                                            )),
                                        };
                                        let _ = tx.send(page);
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            let _ = me
                                .conn
                                .call(
                                    "Target.detachFromTarget",
                                    json!({ "sessionId": session_id }),
                                    None,
                                    Duration::from_secs(5),
                                )
                                .await;
                            // Wakes wait_for_session immediately with the prepare failure instead
                            // of exposing an unprepared session or stalling until timeout.
                            let waiters = me
                                .targets
                                .lock()
                                .await
                                .get_mut(&target_id)
                                .map(|t| std::mem::take(&mut t.waiting))
                                .unwrap_or_default();
                            let msg = e.to_string();
                            let mut err = Some(e);
                            for w in waiters {
                                let _ = w.send(Err(err
                                    .take()
                                    .unwrap_or_else(|| Error::Other(msg.clone()))));
                            }
                        }
                    }
                });
            }
            "Target.targetCreated" => {
                let info = &ev.params["targetInfo"];
                let target_id = info["targetId"].as_str().unwrap_or_default().to_string();
                let ty = info["type"].as_str().unwrap_or("").to_string();
                let ctx = info["browserContextId"].as_str().map(|s| s.to_string());
                let opener = info["openerId"].as_str().map(|s| s.to_string());
                let owner = if ty == "page" { Some(target_id.clone()) } else { None };
                let mut targets = self.targets.lock().await;
                targets.entry(target_id.clone()).or_insert_with(|| {
                    TargetState::new(
                        &target_id,
                        ty,
                        info["url"].as_str().unwrap_or("").to_string(),
                        ctx,
                        opener,
                        owner,
                        None,
                        None,
                        None,
                    )
                });
            }
            "Target.targetDestroyed" => {
                if let Some(id) = ev.params["targetId"].as_str() {
                    if let Some(t) = self.targets.lock().await.remove(id) {
                        t.closed.store(true, Ordering::SeqCst);
                        if t.r#type == "worker" {
                            let mut ws = self.worker_sessions.lock().unwrap();
                            for sid in [&t.active_session_id, &t.session_id].into_iter().flatten() {
                                ws.remove(sid);
                            }
                            drop(ws);
                            t.workers.lock().unwrap().remove_by_target(id);
                        } else if t.r#type == "page" {
                            t.workers.lock().unwrap().close_all();
                        }
                    }
                    self.page_setups.lock().await.remove(id);
                }
            }
            "Target.detachedFromTarget" => {
                if let Some(sid) = ev.params["sessionId"].as_str() {
                    self.external_sessions.lock().unwrap().remove(sid);
                    let removed = self.worker_sessions.lock().unwrap().remove(sid);
                    if let Some((tid, workers)) = removed {
                        self.targets.lock().await.remove(&tid);
                        workers.lock().unwrap().remove_by_session(sid);
                    }
                    let frames_to_clear: Vec<(
                        Arc<std::sync::Mutex<FrameTree>>,
                        Arc<std::sync::Mutex<HashMap<String, i64>>>,
                    )> = {
                        let mut targets = self.targets.lock().await;
                        let mut list = Vec::new();
                        for t in targets.values_mut() {
                            if t.session_id.as_deref() == Some(sid)
                                || t.active_session_id.as_deref() == Some(sid)
                            {
                                t.session_id = None;
                                t.active_session_id = None;
                                list.push((t.frames.clone(), t.worlds.clone()));
                            }
                        }
                        list
                    };
                    for (fr, worlds) in frames_to_clear {
                        let cleared = fr.lock().unwrap().clear_session_contexts(sid);
                        let mut w = worlds.lock().unwrap();
                        for fid in cleared {
                            w.remove(&fid);
                        }
                    }
                }
            }
            // Fetch pauses only blocked resource types, unless the context has proxy
            // credentials: then it pauses everything (Chrome reports auth challenges only for
            // paused requests) and the rest continue. Answered here, on the loop that sees
            // every session and never lags.
            "Fetch.requestPaused" => {
                let (Some(sid), Some(id)) =
                    (ev.session_id.clone(), ev.params["requestId"].as_str())
                else {
                    return;
                };
                let (ctx, owner_page, opener_id) = self.session_owner_context(&sid).await;
                let ctx_setup = match ctx.as_deref() {
                    Some(c) => self.setups.lock().await.get(c).cloned(),
                    None => None,
                };
                let route_tx = {
                    let ps = self.page_setups.lock().await;
                    owner_page
                        .as_deref()
                        .and_then(|op| ps.get(op).and_then(|p| p.routes.clone()))
                        .or_else(|| {
                            opener_id
                                .as_deref()
                                .and_then(|op| ps.get(op).and_then(|p| p.routes.clone()))
                        })
                        .or_else(|| ctx_setup.as_ref().and_then(|s| s.routes.clone()))
                };
                // Paused because its type is blocked, or because everything is paused (proxy
                // credentials, interception). No context or page setup means the context is gone.
                let ty = ev.params["resourceType"].as_str().unwrap_or("");
                let block = match ctx_setup.as_ref() {
                    Some(s) => s.types.iter().any(|t| t.as_str() == ty),
                    None => route_tx.is_none(),
                };
                if !block {
                    if let Some(routes) = route_tx {
                        // The job answers it; if it can't (gone), dropping it continues it.
                        let defaults = ctx_setup
                            .as_ref()
                            .map(|s| s.http_defaults(None))
                            .unwrap_or_else(HttpContextDefaults::default);
                        if let Some(req) = InterceptedRequest::from_paused(
                            &ev.params,
                            self.conn.clone(),
                            sid.clone(),
                            ctx.unwrap_or_default(),
                            defaults,
                        ) {
                            let _ = routes.send(req);
                            return;
                        }
                    }
                }
                let (method, params) = if block {
                    (
                        "Fetch.failRequest",
                        json!({ "requestId": id, "errorReason": "BlockedByClient" }),
                    )
                } else {
                    ("Fetch.continueRequest", json!({ "requestId": id }))
                };
                let conn = self.conn.clone();
                tokio::spawn(async move {
                    let _ = conn
                        .call(method, params, Some(&sid), Duration::from_secs(5))
                        .await;
                });
            }
            // Nothing handles dialogs, and an open one blocks every call on its page until it
            // times out. As Playwright does by default: accept beforeunload so navigation goes
            // on, dismiss alert/confirm/prompt.
            "Page.javascriptDialogOpening" => {
                if let Some(sid) = ev.session_id.clone() {
                    let (_, owner_page, opener_id) = self.session_owner_context(&sid).await;
                    let dialog_tx = {
                        let ps = self.page_setups.lock().await;
                        owner_page
                            .as_deref()
                            .and_then(|op| ps.get(op).and_then(|p| p.dialogs.clone()))
                            .or_else(|| {
                                opener_id
                                    .as_deref()
                                    .and_then(|op| ps.get(op).and_then(|p| p.dialogs.clone()))
                            })
                    };
                    // The job answers it (page.on('dialog')). A dialog it cannot take is
                    // dropped, which answers it as below.
                    if let Some(tx) = dialog_tx {
                        let _ = tx.send(crate::events::Dialog::new(
                            &ev.params,
                            self.conn.clone(),
                            sid,
                        ));
                        return;
                    }
                    let accept = ev.params["type"] == "beforeunload";
                    debug!(message = %ev.params["message"], accept, "answering dialog");
                    let conn = self.conn.clone();
                    tokio::spawn(async move {
                        let _ = conn
                            .call(
                                "Page.handleJavaScriptDialog",
                                json!({ "accept": accept }),
                                Some(&sid),
                                Duration::from_secs(5),
                            )
                            .await;
                    });
                }
            }
            // Proxy credentials. A second challenge for the same request means they were
            // rejected: cancel, or Chrome and the proxy loop forever.
            "Fetch.authRequired" => {
                let (Some(sid), Some(id)) = (
                    ev.session_id.clone(),
                    ev.params["requestId"].as_str().map(String::from),
                ) else {
                    return;
                };
                let ctx = self.context_of_session(&sid).await;
                let is_proxy = ev.params["authChallenge"]["source"] == "Proxy";
                let mut response = json!({ "response": "Default" });
                if let Some(ctx) = ctx {
                    if let Some(setup) = self.setups.lock().await.get_mut(&ctx) {
                        let creds = if is_proxy {
                            setup.proxy_auth.clone()
                        } else {
                            setup.http_auth.clone()
                        };
                        if let Some((user, pass)) = creds {
                            response = if setup.answered.insert(format!("{is_proxy}:{id}")) {
                                json!({ "response": "ProvideCredentials", "username": user, "password": pass })
                            } else {
                                json!({ "response": "CancelAuth" })
                            };
                        }
                    }
                }
                let conn = self.conn.clone();
                tokio::spawn(async move {
                    let _ = conn
                        .call(
                            "Fetch.continueWithAuth",
                            json!({ "requestId": id, "authChallengeResponse": response }),
                            Some(&sid),
                            Duration::from_secs(5),
                        )
                        .await;
                });
            }
            "Page.fileChooserOpened" => {
                if let Some(sid) = ev.session_id.clone() {
                    let (_, owner_page, _) = self.session_owner_context(&sid).await;
                    let fc_tx = match owner_page.as_deref() {
                        Some(op) => self.page_setups.lock().await.get(op).and_then(|p| p.file_choosers.clone()),
                        None => None,
                    };
                    if let Some(tx) = fc_tx {
                        let _ = tx.send(crate::events::FileChooser::new(
                            &ev.params,
                            self.conn.clone(),
                            sid,
                            self.is_remote(),
                        ));
                    }
                }
            }
            "Runtime.bindingCalled" if ev.params["name"] == "__fluxwright_binding" => {
                if let (Some(sid), Some(raw), Some(ctx_id)) = (
                    ev.session_id.clone(),
                    ev.params["payload"].as_str(),
                    ev.params["executionContextId"].as_i64(),
                ) {
                    if let Ok(v) = serde_json::from_str::<Value>(raw) {
                        if let (Some(name), Some(seq)) = (v["name"].as_str(), v["seq"].as_u64()) {
                            let (ctx, owner_page, _) = self.session_owner_context(&sid).await;
                            let (page_has, page_tx) = match owner_page.as_deref() {
                                Some(op) => match self.page_setups.lock().await.get(op) {
                                    Some(p) => (p.exposed_bindings.contains(name), p.bindings.clone()),
                                    None => (false, None),
                                },
                                None => (false, None),
                            };
                            let (ctx_has, ctx_tx) = match ctx.as_deref() {
                                Some(c) => match self.setups.lock().await.get(c) {
                                    Some(s) => (s.exposed_bindings.contains(name), s.bindings.clone()),
                                    None => (false, None),
                                },
                                None => (false, None),
                            };
                            let tx = if page_has {
                                page_tx
                            } else if ctx_has {
                                ctx_tx
                            } else {
                                page_tx.or(ctx_tx)
                            };
                            if let Some(tx) = tx {
                                let args = v["args"].as_array().cloned().unwrap_or_default();
                                let _ = tx.send(crate::events::BindingCall::new(
                                    name.to_owned(),
                                    seq,
                                    args,
                                    self.conn.clone(),
                                    sid,
                                    ctx_id,
                                ));
                            }
                        }
                    }
                }
            }
            "Page.frameAttached" => {
                if let (Some(sid), Some(frame_id), Some(parent_id)) = (
                    ev.session_id.as_deref(),
                    ev.params["frameId"].as_str(),
                    ev.params["parentFrameId"].as_str(),
                ) {
                    if let Some((_, frames, _)) = self.session_frame_tree(sid).await {
                        frames.lock().unwrap().attach(
                            frame_id.to_owned(),
                            parent_id.to_owned(),
                            sid.to_owned(),
                        );
                    }
                }
            }
            "Page.frameNavigated" => {
                if let Some(frame_id) = ev.params["frame"]["id"].as_str() {
                    let full = frame_url(&ev.params["frame"]);
                    let parent_id = ev.params["frame"]["parentId"].as_str().map(str::to_owned);
                    let name = ev.params["frame"]["name"].as_str().map(str::to_owned);
                    let bfcache = ev.params["type"].as_str() == Some("BackForwardCacheRestore");
                    let sid = ev.session_id.clone().unwrap_or_default();
                    let info = if !sid.is_empty() {
                        self.session_frame_tree(&sid).await
                    } else {
                        None
                    };
                    if let Some((main_id, frames, worlds)) = info {
                        frames.lock().unwrap().navigate(
                            frame_id.to_owned(),
                            parent_id.clone(),
                            name,
                            full.clone(),
                            sid,
                            &main_id,
                        );
                        let mut w = worlds.lock().unwrap();
                        if frame_id == main_id {
                            w.clear();
                        } else {
                            w.remove(frame_id);
                        }
                    }
                    if parent_id.is_none() {
                        let mut targets = self.targets.lock().await;
                        if let Some(t) = targets.get_mut(frame_id) {
                            if !full.is_empty() {
                                t.url = full.clone();
                                t.url_tx.send_if_modified(|cur| {
                                    if *cur != full {
                                        *cur = full;
                                        true
                                    } else {
                                        false
                                    }
                                });
                            }
                            if !bfcache {
                                let mut net = t.net.lock().unwrap();
                                net.idle_confirmed = false;
                                net.last_change = std::time::Instant::now();
                            }
                        }
                    }
                }
            }
            "Page.navigatedWithinDocument" => {
                if let (Some(frame_id), Some(url)) = (
                    ev.params["frameId"].as_str(),
                    ev.params["url"].as_str(),
                ) {
                    if !url.is_empty() {
                        if let Some(sid) = ev.session_id.as_deref() {
                            if let Some((_, frames, _)) = self.session_frame_tree(sid).await {
                                frames.lock().unwrap().update_url(frame_id, url.to_string());
                            }
                        }
                        let mut targets = self.targets.lock().await;
                        if let Some(t) = targets.get_mut(frame_id) {
                            t.url = url.to_string();
                            t.url_tx.send_if_modified(|cur| {
                                if cur.as_str() != url {
                                    *cur = url.to_string();
                                    true
                                } else {
                                    false
                                }
                            });
                        }
                    }
                }
            }
            "Page.frameDetached" if ev.params["reason"] != "swap" => {
                if let (Some(sid), Some(frame_id)) = (
                    ev.session_id.as_deref(),
                    ev.params["frameId"].as_str(),
                ) {
                    if let Some((_, frames, worlds)) = self.session_frame_tree(sid).await {
                        let dead = frames.lock().unwrap().detach(frame_id);
                        let mut w = worlds.lock().unwrap();
                        for fid in dead {
                            w.remove(&fid);
                        }
                    }
                }
            }
            "Runtime.executionContextCreated" => {
                let aux = &ev.params["context"]["auxData"];
                if aux["isDefault"].as_bool() == Some(true) {
                    if let (Some(sid), Some(frame_id), Some(ctx_id)) = (
                        ev.session_id.as_deref(),
                        aux["frameId"].as_str(),
                        ev.params["context"]["id"].as_i64(),
                    ) {
                        if let Some((_, frames, _)) = self.session_frame_tree(sid).await {
                            frames.lock().unwrap().set_default_context(frame_id, sid, ctx_id);
                        }
                    }
                }
            }
            "Runtime.executionContextDestroyed" => {
                if let (Some(sid), Some(ctx_id)) = (
                    ev.session_id.as_deref(),
                    ev.params["executionContextId"].as_i64(),
                ) {
                    if let Some((_, frames, worlds)) = self.session_frame_tree(sid).await {
                        frames.lock().unwrap().remove_context(ctx_id);
                        worlds.lock().unwrap().retain(|_, id| *id != ctx_id);
                    }
                }
            }
            "Runtime.executionContextsCleared" => {
                if let Some(sid) = ev.session_id.as_deref() {
                    if let Some((main_id, frames, worlds)) = self.session_frame_tree(sid).await {
                        let cleared = frames.lock().unwrap().clear_session_contexts(sid);
                        let mut w = worlds.lock().unwrap();
                        if cleared.iter().any(|fid| fid == &main_id) {
                            w.clear();
                        } else {
                            for fid in cleared {
                                w.remove(&fid);
                            }
                        }
                    }
                }
            }
            "Network.requestWillBeSent" => {
                if let (Some(sid), Some(req_id)) = (
                    ev.session_id.as_deref(),
                    ev.params["requestId"].as_str(),
                ) {
                    if !ev.params["request"]["url"]
                        .as_str()
                        .unwrap_or("")
                        .starts_with("data:")
                    {
                        let targets = self.targets.lock().await;
                        if let Some(t) = targets.values().find(|t| t.active_session_id.as_deref() == Some(sid)) {
                            let mut net = t.net.lock().unwrap();
                            if net.inflight.len() < 1024 {
                                net.inflight.insert(req_id.to_string());
                            }
                            net.idle_confirmed = false;
                            net.last_change = std::time::Instant::now();
                        }
                    }
                }
            }
            "Network.loadingFinished" | "Network.loadingFailed" => {
                if let (Some(sid), Some(req_id)) = (
                    ev.session_id.as_deref(),
                    ev.params["requestId"].as_str(),
                ) {
                    let targets = self.targets.lock().await;
                    if let Some(t) = targets.values().find(|t| t.active_session_id.as_deref() == Some(sid)) {
                        let mut net = t.net.lock().unwrap();
                        net.inflight.remove(req_id);
                        net.last_change = std::time::Instant::now();
                    }
                }
            }
            "Page.lifecycleEvent" if ev.params["name"] == "networkIdle" => {
                if let Some(frame_id) = ev.params["frameId"].as_str() {
                    let targets = self.targets.lock().await;
                    if let Some(t) = targets.get(frame_id) {
                        let mut net = t.net.lock().unwrap();
                        net.inflight.clear();
                        net.idle_confirmed = true;
                        net.last_change = std::time::Instant::now();
                    }
                }
            }
            "Runtime.consoleAPICalled" => {
                if let Some(sid) = ev.session_id.as_deref() {
                    let entry = self.worker_sessions.lock().unwrap().get(sid).cloned();
                    if let Some((tid, workers)) = entry {
                        let mut wk = workers.lock().unwrap();
                        if wk.has_subscribers() {
                            wk.emit_console(tid, crate::log::console_message_from_cdp(&ev.params));
                        }
                    }
                }
            }
            "Target.targetCrashed" => {
                warn!(params = %ev.params, "target crashed");
            }
            "Fluxwright.disconnected" => {
                self.conn.mark_dead();
            }
            _ => {}
        }
    }

    pub async fn call(
        &self,
        method: &str,
        params: Value,
        session_id: Option<&str>,
        timeout: Duration,
    ) -> Result<Value> {
        if self.conn.is_dead() {
            return Err(Error::BrowserDead { pid: self.pid });
        }
        self.conn.call(method, params, session_id, timeout).await
    }

    pub async fn create_context(&self, proxy: Option<&Proxy>) -> Result<ContextId> {
        let mut params = json!({ "disposeOnDetach": true });
        if let Some(p) = proxy {
            params["proxyServer"] = json!(p.server);
            if let Some(bypass) = &p.bypass {
                params["proxyBypassList"] = json!(bypass);
            }
        }
        let res = self
            .call(
                "Target.createBrowserContext",
                params,
                None,
                Duration::from_secs(10),
            )
            .await?;
        let id = res["browserContextId"]
            .as_str()
            .ok_or_else(|| Error::Other("createBrowserContext: no id".into()))?
            .to_string();
        self.contexts.lock().await.insert(id.clone());
        if let Some(p) = proxy {
            let mut setups = self.setups.lock().await;
            let setup = setups.entry(id.clone()).or_default();
            setup.proxy = Some(p.clone());
            if let Some(user) = &p.username {
                setup.proxy_auth = Some((user.clone(), p.password.clone().unwrap_or_default()));
            }
        }
        Ok(ContextId(id))
    }

    pub async fn dispose_context(&self, id: &ContextId) -> Result<()> {
        self.contexts.lock().await.remove(&id.0);
        self.external_contexts.lock().unwrap().remove(&id.0);
        self.setups.lock().await.remove(&id.0);
        let removed_targets: Vec<String> = {
            let mut targets = self.targets.lock().await;
            let keys: Vec<String> = targets
                .iter()
                .filter(|(_, t)| t.browser_context_id.as_deref() == Some(&id.0))
                .map(|(k, _)| k.clone())
                .collect();
            for k in &keys {
                if let Some(t) = targets.remove(k) {
                    t.closed.store(true, Ordering::SeqCst);
                    if t.r#type == "worker" {
                        let mut ws = self.worker_sessions.lock().unwrap();
                        for sid in [&t.active_session_id, &t.session_id].into_iter().flatten() {
                            ws.remove(sid);
                        }
                    } else if t.r#type == "page" {
                        t.workers.lock().unwrap().close_all();
                    }
                }
            }
            keys
        };
        if !removed_targets.is_empty() {
            let mut ps = self.page_setups.lock().await;
            for k in removed_targets {
                ps.remove(&k);
            }
        }
        let _ = self
            .call(
                "Target.disposeBrowserContext",
                json!({ "browserContextId": id.0 }),
                None,
                Duration::from_secs(10),
            )
            .await;
        Ok(())
    }

    pub async fn close_target(&self, target_id: &TargetId) -> Result<()> {
        if let Some(t) = self.targets.lock().await.remove(&target_id.0) {
            t.closed.store(true, Ordering::SeqCst);
            if t.r#type == "page" {
                t.workers.lock().unwrap().close_all();
            }
        }
        self.page_setups.lock().await.remove(&target_id.0);
        let _ = self
            .call(
                "Target.closeTarget",
                json!({ "targetId": target_id.0 }),
                None,
                Duration::from_secs(5),
            )
            .await;
        Ok(())
    }

    pub async fn create_page(&self, ctx: &ContextId, timeout: Duration) -> Result<CdpPage> {
        let res = self
            .call(
                "Target.createTarget",
                json!({
                    "url": "about:blank",
                    "browserContextId": ctx.0
                }),
                None,
                timeout,
            )
            .await?;
        let target_id = res["targetId"]
            .as_str()
            .ok_or_else(|| Error::Other("createTarget: no targetId".into()))?
            .to_string();

        // Auto-attach prepares the session before announcing it; see on_event.
        let session_id = self.wait_for_session(&target_id, timeout).await?;
        let (url, net, frames, workers, worlds, closed, opener_id) = {
            let mut targets = self.targets.lock().await;
            match targets.get_mut(&target_id) {
                Some(t) => {
                    t.owner_page.get_or_insert_with(|| target_id.clone());
                    (
                        t.url_tx.clone(),
                        t.net.clone(),
                        t.frames.clone(),
                        t.workers.clone(),
                        t.worlds.clone(),
                        t.closed.clone(),
                        t.opener_id.clone().map(TargetId),
                    )
                }
                None => (
                    Arc::new(tokio::sync::watch::Sender::new("about:blank".into())),
                    Default::default(),
                    Arc::new(std::sync::Mutex::new(FrameTree::new(
                        target_id.clone(),
                        session_id.clone(),
                        "about:blank".into(),
                    ))),
                    Default::default(),
                    Default::default(),
                    Default::default(),
                    None,
                ),
            }
        };
        Ok(CdpPage {
            browser: self.conn.clone(),
            browser_pid: self.pid,
            browser_id: self.id,
            headless: self.headless,
            is_remote: self.is_remote(),
            context_id: ctx.clone(),
            target_id: TargetId(target_id),
            opener_id,
            session_id: SessionId(session_id),
            targets: self.targets.clone(),
            worlds,
            setups: self.setups.clone(),
            page_setups: self.page_setups.clone(),
            frames,
            workers,
            closed,
            mouse: Default::default(),
            keyboard: Default::default(),
            url,
            net,
            ax_refs: Default::default(),
            early_log_events: Default::default(),
        })
    }

    async fn wait_for_session(&self, target_id: &str, timeout: Duration) -> Result<String> {
        {
            let mut targets = self.targets.lock().await;
            if let Some(t) = targets.get(target_id) {
                if let Some(sid) = &t.session_id {
                    return Ok(sid.clone());
                }
            }
            let (tx, rx) = tokio::sync::oneshot::channel();
            targets
                .entry(target_id.to_string())
                .or_insert_with(|| {
                    TargetState::new(target_id, "page".into(), String::new(), None, None, Some(target_id.to_string()), None, None, None)
                })
                .waiting
                .push(tx);
            drop(targets);
            // No explicit Target.attachToTarget: browser-level auto-attach covers new pages,
            // and a second attach opens a second session that duplicates every event.
            match tokio::time::timeout(timeout, rx).await {
                Ok(Ok(Ok(sid))) => Ok(sid),
                // SessionGone keeps it retryable: the job moves to another browser instead of failing.
                Ok(Ok(Err(e))) => Err(Error::SessionGone(format!("{target_id}: {e}"))),
                Ok(Err(_)) => Err(Error::SessionGone(target_id.into())),
                Err(_) => Err(Error::timeout("attachToTarget", timeout.as_millis() as u64)),
            }
        }
    }

    async fn session_owner_context(&self, sid: &str) -> (Option<String>, Option<String>, Option<String>) {
        self.targets
            .lock()
            .await
            .values()
            .find(|t| t.active_session_id.as_deref() == Some(sid) || t.session_id.as_deref() == Some(sid))
            .map(|t| (t.browser_context_id.clone(), t.owner_page.clone(), t.opener_id.clone()))
            .unwrap_or((None, None, None))
    }

    #[allow(clippy::type_complexity)]
    async fn session_frame_tree(
        &self,
        sid: &str,
    ) -> Option<(
        String,
        Arc<std::sync::Mutex<FrameTree>>,
        Arc<std::sync::Mutex<HashMap<String, i64>>>,
    )> {
        let targets = self.targets.lock().await;
        let t = targets.values().find(|t| {
            t.active_session_id.as_deref() == Some(sid) || t.session_id.as_deref() == Some(sid)
        })?;
        let main_id = t.owner_page.clone()?;
        Some((main_id, t.frames.clone(), t.worlds.clone()))
    }

    async fn context_of_session(&self, sid: &str) -> Option<String> {
        self.targets
            .lock()
            .await
            .values()
            .find(|t| t.active_session_id.as_deref() == Some(sid) || t.session_id.as_deref() == Some(sid))
            .and_then(|t| t.browser_context_id.clone())
    }

    pub(crate) async fn session_for_target(
        targets: &Mutex<HashMap<String, TargetState>>,
        target_id: &str,
    ) -> Option<String> {
        targets
            .lock()
            .await
            .get(target_id)
            .and_then(|t| t.session_id.clone())
    }

    /// Sessions of every target in a browser context (with page type flag).
    pub(crate) async fn sessions_in_context(
        targets: &Mutex<HashMap<String, TargetState>>,
        context: &str,
    ) -> Vec<String> {
        targets
            .lock()
            .await
            .values()
            .filter(|t| t.browser_context_id.as_deref() == Some(context))
            .filter_map(|t| t.session_id.clone())
            .collect()
    }

    /// Sessions belonging to one top-level page and its out-of-process child frames.
    pub(crate) async fn sessions_for_page(
        targets: &Mutex<HashMap<String, TargetState>>,
        page_target_id: &str,
    ) -> Vec<String> {
        targets
            .lock()
            .await
            .values()
            .filter(|t| t.owner_page.as_deref() == Some(page_target_id))
            .filter_map(|t| t.session_id.clone())
            .collect()
    }

    /// Sessions in `context` annotated with their owning top-level page id and whether they are a page session.
    async fn context_session_details(&self, context: &str) -> Vec<(String, String, bool)> {
        self.targets
            .lock()
            .await
            .values()
            .filter(|t| t.browser_context_id.as_deref() == Some(context))
            .filter_map(|t| {
                Some((
                    t.session_id.clone()?,
                    t.owner_page.clone().unwrap_or_default(),
                    t.r#type == "page",
                ))
            })
            .collect()
    }

    pub async fn context_allow_downloads(&self, ctx: &ContextId, dir: &Path, timeout: Duration) -> Result<()> {
        let params = json!({
            "behavior": "allowAndName",
            "browserContextId": ctx.0,
            "downloadPath": dir.to_string_lossy(),
            "eventsEnabled": true
        });
        self.call("Browser.setDownloadBehavior", params, None, timeout).await?;
        Ok(())
    }

    pub async fn start_har_recording(
        &self,
        ctx: &ContextId,
        scope_page_target_id: Option<String>,
        opts: RecordHarOptions,
    ) -> HarRecorder {
        let initial_sessions = match scope_page_target_id.as_deref() {
            Some(pid) => Self::sessions_for_page(&self.targets, pid).await,
            None => Self::sessions_in_context(&self.targets, &ctx.0).await,
        };
        HarRecorder::start(
            self.conn.clone(),
            ctx.0.clone(),
            initial_sessions,
            scope_page_target_id,
            opts,
        )
    }

    pub async fn context_emulate(&self, ctx: &ContextId, e: &Emulation, timeout: Duration) -> Result<()> {
        if e.is_empty() {
            return Ok(());
        }
        let mut e = e.clone();
        if e.locale.is_some() && e.user_agent.is_none() {
            let v = self.call("Browser.getVersion", json!({}), None, timeout).await?;
            e.user_agent = v["userAgent"].as_str().map(str::to_owned);
        }
        self.setups.lock().await.entry(ctx.0.clone()).or_default().emulation = Some(e.clone());
        let sessions = self.context_session_details(&ctx.0).await;
        for (sid, _, is_page) in sessions {
            let calls = e.calls(is_page);
            let _ = futures_util::future::join_all(
                calls.into_iter().map(|(m, p)| self.call(m, p, Some(&sid), timeout)),
            )
            .await;
        }
        Ok(())
    }

    pub async fn context_grant_permissions(&self, ctx: &ContextId, names: &[String], timeout: Duration) -> Result<()> {
        let permissions = permission_types(names)?;
        if permissions.is_empty() {
            return Ok(());
        }
        self.call(
            "Browser.grantPermissions",
            json!({ "permissions": permissions, "browserContextId": ctx.0 }),
            None,
            timeout,
        )
        .await?;
        Ok(())
    }

    pub async fn context_cookies(&self, ctx: &ContextId, timeout: Duration) -> Result<Vec<Cookie>> {
        let res = self
            .call(
                "Storage.getCookies",
                json!({ "browserContextId": ctx.0 }),
                None,
                timeout,
            )
            .await?;
        Ok(res["cookies"]
            .as_array()
            .map(|a| a.iter().filter_map(Cookie::from_cdp).collect())
            .unwrap_or_default())
    }

    pub async fn context_cookies_for_urls(
        &self,
        ctx: &ContextId,
        urls: &[String],
        timeout: Duration,
    ) -> Result<Vec<Cookie>> {
        let all = self.context_cookies(ctx, timeout).await?;
        filter_cookies(&all, urls)
    }

    pub async fn context_add_cookies(&self, ctx: &ContextId, cookies: &[Cookie], timeout: Duration) -> Result<()> {
        if cookies.is_empty() {
            return Ok(());
        }
        let cookies: Vec<Value> = cookies.iter().map(Cookie::to_cdp).collect();
        self.call(
            "Storage.setCookies",
            json!({ "cookies": cookies, "browserContextId": ctx.0 }),
            None,
            timeout,
        )
        .await?;
        Ok(())
    }

    pub async fn context_clear_cookies(&self, ctx: &ContextId, timeout: Duration) -> Result<()> {
        self.call(
            "Storage.clearCookies",
            json!({ "browserContextId": ctx.0 }),
            None,
            timeout,
        )
        .await?;
        Ok(())
    }

    pub async fn context_clear_cookies_filtered(
        &self,
        ctx: &ContextId,
        filter: &CookieFilter,
        timeout: Duration,
    ) -> Result<()> {
        if filter.is_empty() {
            return self.context_clear_cookies(ctx, timeout).await;
        }
        let all = self.context_cookies(ctx, timeout).await?;
        let mut removed = false;
        let mut keep = Vec::new();
        for c in all {
            if filter.matches(&c) {
                removed = true;
            } else {
                keep.push(c);
            }
        }
        if removed {
            self.context_replace_cookies(ctx, &keep, timeout).await?;
        }
        Ok(())
    }

    pub async fn context_replace_cookies(
        &self,
        ctx: &ContextId,
        cookies: &[Cookie],
        timeout: Duration,
    ) -> Result<()> {
        self.context_clear_cookies(ctx, timeout).await?;
        self.context_add_cookies(ctx, cookies, timeout).await
    }

    pub async fn context_api_fetch(
        &self,
        ctx: &ContextId,
        defaults: &HttpContextDefaults,
        url: &str,
        opts: &ApiRequestOptions,
    ) -> Result<ApiResponse> {
        send_context_request(&self.conn, &ctx.0, defaults, url, opts, None).await
    }

    pub async fn context_set_http_credentials(
        &self,
        ctx: &ContextId,
        credentials: &(String, String),
        timeout: Duration,
    ) -> Result<()> {
        let fetch = {
            let mut setups = self.setups.lock().await;
            let setup = setups.entry(ctx.0.clone()).or_default();
            setup.http_auth = Some(credentials.clone());
            fetch_params_combined(Some(setup), None)
        };
        if let Some(params) = fetch {
            for (sid, _, _) in self.context_session_details(&ctx.0).await {
                let _ = self.call("Fetch.enable", params.clone(), Some(&sid), timeout).await;
            }
        }
        Ok(())
    }

    pub async fn context_set_storage_state(&self, ctx: &ContextId, state: &StorageState, timeout: Duration) -> Result<()> {
        self.context_add_cookies(ctx, &state.cookies, timeout).await?;
        if let Some(source) = state.restore_script() {
            self.context_add_init_script(ctx, &source, timeout).await?;
        }
        Ok(())
    }

    pub async fn context_set_blocked_urls(&self, ctx: &ContextId, urls: &[String], timeout: Duration) -> Result<()> {
        self.setups.lock().await.entry(ctx.0.clone()).or_default().urls = urls.to_vec();
        for (sid, _, _) in self.context_session_details(&ctx.0).await {
            let _ = self.call("Network.enable", json!({}), Some(&sid), timeout).await;
            let _ = self
                .call("Network.setBlockedURLs", json!({ "urls": urls }), Some(&sid), timeout)
                .await;
        }
        Ok(())
    }

    pub async fn context_block_resource_types(&self, ctx: &ContextId, types: &[ResourceType], timeout: Duration) -> Result<()> {
        if types.is_empty() {
            return Ok(());
        }
        let ctx_setup = {
            let mut setups = self.setups.lock().await;
            let setup = setups.entry(ctx.0.clone()).or_default();
            setup.types = types.to_vec();
            setup.clone()
        };
        let sessions = self.context_session_details(&ctx.0).await;
        if !sessions.is_empty() {
            let ps = self.page_setups.lock().await;
            for (sid, page_id, _) in sessions {
                if let Some(params) = fetch_params_combined(Some(&ctx_setup), ps.get(&page_id)) {
                    let _ = self.call("Fetch.enable", params, Some(&sid), timeout).await;
                }
            }
        }
        Ok(())
    }

    pub async fn context_add_init_script(&self, ctx: &ContextId, source: &str, timeout: Duration) -> Result<()> {
        self.setups
            .lock()
            .await
            .entry(ctx.0.clone())
            .or_default()
            .init_scripts
            .push(source.to_string());
        for sid in Self::sessions_in_context(&self.targets, &ctx.0).await {
            let _ = self
                .call(
                    "Page.addScriptToEvaluateOnNewDocument",
                    json!({ "source": source }),
                    Some(&sid),
                    timeout,
                )
                .await;
        }
        Ok(())
    }

    pub async fn context_bindings(&self, ctx: &ContextId) -> tokio::sync::mpsc::UnboundedReceiver<crate::events::BindingCall> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        self.setups
            .lock()
            .await
            .entry(ctx.0.clone())
            .or_default()
            .bindings = Some(tx);
        rx
    }

    pub async fn context_expose_binding(&self, ctx: &ContextId, name: &str, timeout: Duration) -> Result<()> {
        if name.trim().is_empty() {
            return Err(Error::Other("exposeFunction: name must not be empty".into()));
        }
        let (shim, eval_shim) = crate::page::binding_shims(name);
        {
            let page_ids: Vec<String> = self
                .targets
                .lock()
                .await
                .iter()
                .filter(|(_, t)| t.r#type == "page" && t.browser_context_id.as_deref() == Some(&ctx.0))
                .map(|(id, _)| id.clone())
                .collect();
            let ps = self.page_setups.lock().await;
            if page_ids
                .iter()
                .any(|id| ps.get(id).is_some_and(|p| p.exposed_bindings.contains(name)))
            {
                return Err(Error::Other(format!(
                    "Function \"{name}\" has been already registered"
                )));
            }
        }
        let need_binding = {
            let mut setups = self.setups.lock().await;
            let setup = setups.entry(ctx.0.clone()).or_default();
            if !setup.exposed_bindings.insert(name.to_string()) {
                return Err(Error::Other(format!(
                    "Function \"{name}\" has been already registered"
                )));
            }
            let first = !setup.bindings_enabled;
            setup.bindings_enabled = true;
            setup.init_scripts.push(shim.clone());
            first
        };
        let sessions = Self::sessions_in_context(&self.targets, &ctx.0).await;
        install_binding_on_sessions(&self.conn, &sessions, None, need_binding, &shim, &eval_shim, timeout).await
    }

    pub async fn context_intercept(
        &self,
        ctx: &ContextId,
        timeout: Duration,
    ) -> Result<tokio::sync::mpsc::UnboundedReceiver<InterceptedRequest>> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let params = {
            let mut setups = self.setups.lock().await;
            let setup = setups.entry(ctx.0.clone()).or_default();
            setup.routes = Some(tx);
            fetch_params_combined(Some(setup), None)
        };
        if let Some(params) = params {
            for sid in Self::sessions_in_context(&self.targets, &ctx.0).await {
                let _ = self.call("Fetch.enable", params.clone(), Some(&sid), timeout).await;
            }
        }
        Ok(rx)
    }

    /// A launched browser is closed and its process ended. A remote one is left running for its
    /// owner: only this connection to it ends, which also disposes of the contexts it created.
    pub async fn close(&self) -> Result<()> {
        if !self.is_remote() {
            let _ = self
                .call("Browser.close", json!({}), None, Duration::from_secs(5))
                .await;
            if let Some(mut child) = self._child.lock().await.take() {
                let _ = child.kill().await;
                let _ = child.wait().await;
            }
        }
        self.conn.close();
        Ok(())
    }

    /// Kills Chrome without waiting, for code that cannot await (a dropped engine). The
    /// connection then closes and the browser is freed.
    pub fn start_kill(self: &Arc<Self>) {
        self.conn.close();
        match self._child.try_lock() {
            Ok(mut child) => {
                if let Some(child) = child.as_mut() {
                    let _ = child.start_kill();
                }
            }
            // The exit watcher holds it for a moment: kill from a task instead.
            Err(_) => {
                if let Ok(rt) = tokio::runtime::Handle::try_current() {
                    let browser = self.clone();
                    rt.spawn(async move {
                        let _ = browser.kill_process().await;
                    });
                }
            }
        }
    }

    /// The Chrome profile folder; deleted when the browser is freed.
    pub fn profile_dir(&self) -> Option<&std::path::Path> {
        self._profile.as_ref().map(|d| d.path())
    }

    pub async fn kill_process(&self) -> Result<()> {
        if let Some(mut child) = self._child.lock().await.take() {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
        self.conn.close();
        Ok(())
    }
}

/// A browser that stops answering without dropping the socket (a hung renderer host, a broken
/// network link to a remote browser) used to fail every job with a timeout and keep its socket
/// forever. `Browser.getVersion` is cheap and browser-wide; ten silent seconds means dead.
fn spawn_keepalive(conn: Arc<Connection>) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(15)).await;
            if conn.is_dead() {
                break;
            }
            match conn
                .call(
                    "Browser.getVersion",
                    json!({}),
                    None,
                    Duration::from_secs(10),
                )
                .await
            {
                Ok(_) => {}
                Err(e) => {
                    warn!(error = %e, "cdp keepalive failed; marking the connection dead");
                    conn.mark_dead();
                    conn.close();
                    break;
                }
            }
        }
    });
}

fn parse_host_port(host_port: &str) -> Result<(&str, u16)> {
    if let Some(rest) = host_port.strip_prefix('[') {
        let (host, after) = rest.split_once(']').ok_or_else(|| {
            Error::Launch(format!(
                "unclosed IPv6 bracket in CDP endpoint: {host_port}"
            ))
        })?;
        if host.is_empty() {
            return Err(Error::Launch(format!(
                "empty host in CDP endpoint: {host_port}"
            )));
        }
        if after.is_empty() {
            return Ok((host, 80));
        }
        let port_str = after.strip_prefix(':').ok_or_else(|| {
            Error::Launch(format!("invalid host:port in CDP endpoint: {host_port}"))
        })?;
        let port = port_str
            .parse::<u16>()
            .map_err(|_| Error::Launch(format!("invalid port in CDP endpoint: {host_port}")))?;
        Ok((host, port))
    } else if let Some((host, port_str)) = host_port.split_once(':') {
        if host.is_empty() {
            return Err(Error::Launch(format!(
                "empty host in CDP endpoint: {host_port}"
            )));
        }
        let port = port_str
            .parse::<u16>()
            .map_err(|_| Error::Launch(format!("invalid port in CDP endpoint: {host_port}")))?;
        Ok((host, port))
    } else {
        if host_port.is_empty() {
            return Err(Error::Launch("empty host in CDP endpoint".into()));
        }
        Ok((host_port, 80))
    }
}

async fn resolve_ws_endpoint(endpoint: &str) -> Result<String> {
    if endpoint.starts_with("ws://") || endpoint.starts_with("wss://") {
        return Ok(endpoint.to_string());
    }
    if let Some(rest) = endpoint.strip_prefix("http://") {
        // The authority ends at the first `/`, `?` or `#`. A query string (an auth token)
        // goes along to /json/version; a trailing slash before it is tolerated.
        let rest = rest.split('#').next().unwrap_or(rest);
        let (rest, query) = rest.split_once('?').unwrap_or((rest, ""));
        let (host_port, path) = match rest.split_once('/') {
            Some((hp, p)) => (hp, format!("/{}", p.trim_matches('/'))),
            None => (rest, String::new()),
        };
        let path = if path.is_empty() || path == "/json/version" {
            "/json/version".to_string()
        } else {
            format!("{}/json/version", path.trim_end_matches('/'))
        };
        let path = if query.is_empty() {
            path
        } else {
            format!("{path}?{query}")
        };
        let (host, port) = parse_host_port(host_port)?;
        return tokio::time::timeout(Duration::from_secs(10), async {
            let mut stream = tokio::net::TcpStream::connect((host, port))
                .await
                .map_err(|e| {
                    Error::Launch(format!(
                        "failed to connect to CDP endpoint at {host}:{port}: {e}"
                    ))
                })?;
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let req =
                format!("GET {path} HTTP/1.1\r\nHost: {host_port}\r\nConnection: close\r\n\r\n");
            stream
                .write_all(req.as_bytes())
                .await
                .map_err(|e| Error::Launch(e.to_string()))?;
            // Chrome keeps the connection open whatever the request asks: stop once the body is
            // whole JSON instead of waiting for a close that never comes.
            let mut resp = Vec::new();
            let mut buf = [0u8; 4096];
            let json: Value = loop {
                let n = stream
                    .read(&mut buf)
                    .await
                    .map_err(|e| Error::Launch(e.to_string()))?;
                resp.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&resp);
                if let Some(Ok(json)) = text
                    .split_once("\r\n\r\n")
                    .map(|(_, body)| serde_json::from_str::<Value>(body))
                {
                    break json;
                }
                if n == 0 || resp.len() > 64 * 1024 {
                    return Err(Error::Launch(format!("no JSON answer from {path}")));
                }
            };
            if let Some(ws) = json["webSocketDebuggerUrl"].as_str() {
                return Ok(ws.to_string());
            }
            Err(Error::Launch(format!(
                "could not obtain webSocketDebuggerUrl from {endpoint}"
            )))
        })
        .await
        .map_err(|_| Error::timeout("resolve_ws_endpoint", 10_000))?;
    }
    Err(Error::Launch(format!(
        "unsupported CDP endpoint URL scheme: {endpoint}"
    )))
}

pub(crate) async fn install_binding_on_sessions(
    conn: &Connection,
    sessions: &[String],
    primary_session: Option<&str>,
    need_binding: bool,
    shim: &str,
    eval_shim: &str,
    timeout: Duration,
) -> Result<()> {
    for s in sessions {
        let is_primary = primary_session == Some(s.as_str());
        if need_binding {
            let r = conn
                .call(
                    "Runtime.addBinding",
                    json!({ "name": "__fluxwright_binding" }),
                    Some(s),
                    timeout,
                )
                .await;
            if is_primary {
                r?;
            }
        }
        let r1 = conn
            .call(
                "Page.addScriptToEvaluateOnNewDocument",
                json!({ "source": shim }),
                Some(s),
                timeout,
            )
            .await;
        if is_primary {
            r1?;
        }
        let r2 = conn
            .call(
                "Runtime.evaluate",
                json!({ "expression": eval_shim, "returnByValue": true }),
                Some(s),
                timeout,
            )
            .await;
        if is_primary {
            r2?;
        }
    }
    Ok(())
}

async fn prepare_session(
    conn: &Connection,
    session_id: &str,
    ty: &str,
    waiting_for_debugger: bool,
    setup: Option<ContextSetup>,
    page_setup: Option<PageSetup>,
) -> Result<()> {
    let mut calls = vec![(
        "Target.setAutoAttach",
        json!({ "autoAttach": true, "waitForDebuggerOnStart": true, "flatten": true }),
    )];
    if ty == "page" || ty == "iframe" {
        calls.push(("Page.enable", json!({})));
        calls.push(("Runtime.enable", json!({})));
        calls.push(("Network.enable", json!({})));
        calls.push(("Page.setLifecycleEventsEnabled", json!({ "enabled": true })));
    }
    if ty == "page" {
        calls.push(("Page.setInterceptFileChooserDialog", json!({ "enabled": true })));
    }
    if ty == "page" || ty == "iframe" {
        if let Some(ref s) = setup {
            if let Some(e) = &s.emulation {
                calls.extend(e.calls(ty == "page"));
            }
        }
        let bindings_enabled = setup.as_ref().is_some_and(|s| s.bindings_enabled)
            || page_setup.as_ref().is_some_and(|p| p.bindings_enabled);
        if bindings_enabled {
            calls.push(("Runtime.addBinding", json!({ "name": "__fluxwright_binding" })));
        }
        for source in setup
            .iter()
            .flat_map(|s| s.init_scripts.iter())
            .chain(page_setup.iter().flat_map(|p| p.init_scripts.iter()))
        {
            calls.push((
                "Page.addScriptToEvaluateOnNewDocument",
                json!({ "source": source }),
            ));
        }
        if let Some(params) = fetch_params_combined(setup.as_ref(), page_setup.as_ref()) {
            calls.push(("Fetch.enable", params));
        }
        if let Some(ref s) = setup {
            if !s.urls.is_empty() {
                calls.push(("Network.enable", json!({})));
                calls.push(("Network.setBlockedURLs", json!({ "urls": s.urls })));
            }
        }
    }
    if waiting_for_debugger {
        calls.push(("Runtime.runIfWaitingForDebugger", json!({})));
    }
    // Pipelined: Chrome runs a session's commands in order, so one round trip instead of
    // one per command. Sequential calls cost ~1 s per page on a busy browser.
    let methods: Vec<&'static str> = calls.iter().map(|(m, _)| *m).collect();
    // Busy runners (macOS full Chrome under 300-page churn) can take >5 s to boot a renderer
    // and process pipelined domain enables + runIfWaitingForDebugger; match navigation_timeout.
    let results = futures_util::future::join_all(
        calls
            .into_iter()
            .map(|(m, p)| conn.call(m, p, Some(session_id), Duration::from_secs(30))),
    )
    .await;
    check_prepare_results(session_id, methods.into_iter().zip(results).collect())
}

async fn prepare_worker_session(
    conn: &Connection,
    session_id: &str,
    waiting_for_debugger: bool,
) -> Result<()> {
    let mut calls = vec![("Runtime.enable", json!({}))];
    if waiting_for_debugger {
        calls.push(("Runtime.runIfWaitingForDebugger", json!({})));
    }
    let methods: Vec<&'static str> = calls.iter().map(|(m, _)| *m).collect();
    let results = futures_util::future::join_all(
        calls
            .into_iter()
            .map(|(m, p)| conn.call(m, p, Some(session_id), Duration::from_secs(30))),
    )
    .await;
    check_prepare_results(session_id, methods.into_iter().zip(results).collect())
}

fn check_prepare_results(
    session_id: &str,
    results: Vec<(&'static str, Result<Value>)>,
) -> Result<()> {
    let mut failure = None;
    for (method, res) in results {
        if let Err(e) = res {
            warn!(%session_id, method, error = %e, "prepare_session command failed");
            // Emulation command rejections on secondary iframe targets are non-fatal unless
            // transport/session itself failed. A slow answer is not a dead session either,
            // except where the session cannot work without it: interception and the
            // debugger release.
            let fatal = if e.is_timeout() {
                matches!(method, "Fetch.enable" | "Runtime.runIfWaitingForDebugger")
            } else if method == "Page.setInterceptFileChooserDialog" && matches!(e, Error::Command { .. }) {
                false
            } else {
                !method.starts_with("Emulation.") || !matches!(e, Error::Command { .. })
            };
            if fatal && failure.is_none() {
                failure = Some(e);
            }
        }
    }
    match failure {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A closed browser is freed: its tasks end and its profile folder goes. Its own event loop
    /// used to keep it alive until the process exited.
    #[tokio::test(flavor = "multi_thread")]
    async fn closed_browser_is_freed() {
        let opts = LaunchOptions {
            no_sandbox: std::env::var("FLUXWRIGHT_NO_SANDBOX").is_ok(),
            ..LaunchOptions::default()
        };
        let browser = CdpBrowser::launch(opts).await.expect("launch");
        let dir = browser.profile_dir().expect("profile folder").to_path_buf();
        let weak = Arc::downgrade(&browser);
        browser.close().await.unwrap();
        drop(browser);
        for _ in 0..100 {
            if weak.upgrade().is_none() && !dir.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(
            weak.upgrade().is_none(),
            "the closed browser is still alive"
        );
        assert!(
            !dir.exists(),
            "profile folder left behind: {}",
            dir.display()
        );
    }

    #[tokio::test]
    async fn resolve_ws_endpoint_handles_schemes() {
        assert_eq!(
            resolve_ws_endpoint("ws://127.0.0.1:9222/devtools/browser/1")
                .await
                .unwrap(),
            "ws://127.0.0.1:9222/devtools/browser/1"
        );
        assert_eq!(
            resolve_ws_endpoint("wss://secure.cdp.example/devtools/browser/2")
                .await
                .unwrap(),
            "wss://secure.cdp.example/devtools/browser/2"
        );
        assert!(resolve_ws_endpoint("ftp://invalid").await.is_err());
    }

    #[tokio::test]
    async fn resolve_ws_endpoint_parses_ipv6_and_rejects_invalid_ports() {
        assert_eq!(parse_host_port("[::1]:9222").unwrap(), ("::1", 9222));
        assert_eq!(parse_host_port("[::1]").unwrap(), ("::1", 80));
        assert!(parse_host_port("[::1]:abc").is_err());
        assert!(parse_host_port("localhost:abc").is_err());
        assert!(resolve_ws_endpoint("http://localhost:abc").await.is_err());
        assert!(resolve_ws_endpoint("http://[::1]:abc").await.is_err());

        if let Ok(listener) = tokio::net::TcpListener::bind("[::1]:0").await {
            let port = listener.local_addr().unwrap().port();
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                if let Ok((mut stream, _)) = listener.accept().await {
                    let mut buf = [0u8; 256];
                    let _ = stream.read(&mut buf).await;
                    let body =
                        r#"{"webSocketDebuggerUrl":"ws://[::1]:9222/devtools/browser/ipv6"}"#;
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = stream.write_all(resp.as_bytes()).await;
                }
            });
            let ws = resolve_ws_endpoint(&format!("http://[::1]:{port}"))
                .await
                .unwrap();
            assert_eq!(ws, "ws://[::1]:9222/devtools/browser/ipv6");
        }
    }

    #[tokio::test]
    async fn resolve_ws_endpoint_keeps_the_query_string() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (seen_tx, seen) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 512];
            let n = stream.read(&mut buf).await.unwrap();
            let _ = seen_tx.send(String::from_utf8_lossy(&buf[..n]).into_owned());
            let body = r#"{"webSocketDebuggerUrl":"ws://127.0.0.1:9222/devtools/browser/q"}"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(resp.as_bytes()).await.unwrap();
            // As Chrome does: the connection stays open after the answer.
            std::future::pending::<()>().await;
        });
        let ws = resolve_ws_endpoint(&format!("http://127.0.0.1:{port}/?token=x"))
            .await
            .unwrap();
        assert_eq!(ws, "ws://127.0.0.1:9222/devtools/browser/q");
        let request = seen.await.unwrap();
        assert!(
            request.starts_with("GET /json/version?token=x HTTP/1.1"),
            "{request}"
        );
    }

    /// A page in a context this connection did not create (someone else's tab on a shared
    /// remote browser) is detached and left alone: no domains enabled, no debugger resume.
    #[tokio::test]
    async fn foreign_targets_are_only_detached() {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (msg_tx, mut msg_rx) = tokio::sync::mpsc::unbounded_channel::<Value>();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            while let Some(Ok(Message::Text(text))) = ws.next().await {
                let msg: Value = serde_json::from_str(&text).unwrap();
                let reply = json!({ "id": msg["id"], "result": {} }).to_string();
                ws.send(Message::Text(reply.into())).await.unwrap();
                if msg["method"] == "Target.setAutoAttach" && msg.get("sessionId").is_none() {
                    let event = json!({
                        "method": "Target.attachedToTarget",
                        "params": {
                            "sessionId": "FOREIGN",
                            "waitingForDebugger": false,
                            "targetInfo": {
                                "targetId": "T1",
                                "type": "page",
                                "url": "https://example.com/",
                                "browserContextId": "someone-else",
                            },
                        },
                    });
                    ws.send(Message::Text(event.to_string().into()))
                        .await
                        .unwrap();
                }
                let _ = msg_tx.send(msg);
            }
        });

        let browser = CdpBrowser::connect(&format!("ws://{addr}"))
            .await
            .expect("connect");
        let mut about_foreign = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while let Ok(Some(msg)) = tokio::time::timeout_at(deadline, msg_rx.recv()).await {
            if msg["sessionId"] == "FOREIGN" || msg["params"]["sessionId"] == "FOREIGN" {
                about_foreign.push(msg["method"].clone());
            }
        }
        assert_eq!(about_foreign, vec![json!("Target.detachFromTarget")]);
        browser.close().await.unwrap();
    }

    #[test]
    fn prepare_results_timeouts_are_fatal_only_where_the_session_needs_them() {
        let slow = |m: &'static str| (m, Err(Error::timeout(m, 5_000)));
        assert!(check_prepare_results("s1", vec![slow("Page.enable")]).is_ok());
        assert!(check_prepare_results("s1", vec![slow("Fetch.enable")]).is_err());
    }

    #[tokio::test]
    async fn remote_browser_close_does_not_send_browser_close() {
        use futures_util::StreamExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (msg_tx, mut msg_rx) = tokio::sync::mpsc::unbounded_channel::<String>();

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            while let Some(Ok(msg)) = ws.next().await {
                if let tokio_tungstenite::tungstenite::Message::Text(text) = msg {
                    let _ = msg_tx.send(text.to_string());
                }
            }
        });

        let ws_url = format!("ws://{addr}");
        let conn = Connection::connect(&ws_url).await.unwrap();
        let browser = CdpBrowser {
            id: BrowserId(Uuid::new_v4()),
            pid: 0,
            headless: true,
            ws_endpoint: Some(ws_url),
            conn: conn.clone(),
            _child: Mutex::new(None),
            _profile: None,
            targets: Arc::new(Mutex::new(HashMap::new())),
            setups: Default::default(),
            page_setups: Default::default(),
            contexts: Default::default(),
            external_contexts: Default::default(),
            external_sessions: Default::default(),
            worker_sessions: Default::default(),
        };

        assert!(browser.is_remote());
        assert!(!browser.connection_dead());

        browser.close().await.unwrap();
        assert!(browser.connection_dead());

        drop(browser);
        drop(conn);
        let _ = tokio::time::timeout(Duration::from_secs(2), server).await;

        let mut received = Vec::new();
        while let Ok(m) = msg_rx.try_recv() {
            received.push(m);
        }
        assert!(
            !received.iter().any(|m| m.contains("Browser.close")),
            "remote close() must not send Browser.close, got: {received:?}"
        );
    }

    /// A closed remote browser is freed and its socket closed. Closing used to only flag the
    /// connection: the socket stayed open and the event loop held the browser for good, one
    /// leak per recycle.
    #[tokio::test]
    async fn closed_remote_browser_is_freed() {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        // A stand-in browser: answers every command, and ends when the client closes.
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            while let Some(Ok(msg)) = ws.next().await {
                if let Message::Text(text) = msg {
                    let text = text.to_string();
                    let id = serde_json::from_str::<Value>(&text).unwrap()["id"].clone();
                    let reply = json!({ "id": id, "result": {} }).to_string();
                    if ws.send(Message::Text(reply.into())).await.is_err() {
                        break;
                    }
                }
            }
        });

        let browser = CdpBrowser::connect(&format!("ws://{addr}"))
            .await
            .expect("connect");
        let weak = Arc::downgrade(&browser);
        browser.close().await.unwrap();
        drop(browser);
        for _ in 0..100 {
            if weak.upgrade().is_none() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            weak.upgrade().is_none(),
            "the closed remote browser is still alive"
        );
        tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .expect("the socket was never closed")
            .unwrap();
    }

    #[tokio::test]
    async fn start_kill_on_remote_browser_closes_socket() {
        use futures_util::StreamExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            while ws.next().await.is_some() {}
        });

        let ws_url = format!("ws://{addr}");
        let conn = Connection::connect(&ws_url).await.unwrap();
        let browser = Arc::new(CdpBrowser {
            id: BrowserId(Uuid::new_v4()),
            pid: 0,
            headless: true,
            ws_endpoint: Some(ws_url),
            conn: conn.clone(),
            _child: Mutex::new(None),
            _profile: None,
            targets: Arc::new(Mutex::new(HashMap::new())),
            setups: Default::default(),
            page_setups: Default::default(),
            contexts: Default::default(),
            external_contexts: Default::default(),
            external_sessions: Default::default(),
            worker_sessions: Default::default(),
        });
        let b = browser.clone();
        let mut sub = conn.subscribe_unbounded();
        tokio::spawn(async move {
            while let Some(ev) = sub.recv().await {
                b.on_event(&ev).await;
            }
        });
        drop(conn);

        let weak = Arc::downgrade(&browser);
        browser.start_kill();
        drop(browser);

        for _ in 0..100 {
            if weak.upgrade().is_none() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            weak.upgrade().is_none(),
            "remote browser leaked after start_kill"
        );
        tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .expect("start_kill never closed the remote socket")
            .unwrap();
    }

    #[test]
    fn prepare_results_rejects_fetch_or_resume_errors() {
        assert!(check_prepare_results("s1", vec![("Page.enable", Ok(json!({})))]).is_ok());

        let err = check_prepare_results(
            "s1",
            vec![
                ("Page.enable", Ok(json!({}))),
                (
                    "Fetch.enable",
                    Err(Error::Command {
                        method: "Fetch.enable".into(),
                        message: "bad pattern".into(),
                    }),
                ),
            ],
        );
        assert!(err.is_err());

        let resume_err = check_prepare_results(
            "s1",
            vec![(
                "Runtime.runIfWaitingForDebugger",
                Err(Error::SessionGone("s1".into())),
            )],
        );
        assert!(resume_err.is_err());
    }

    #[test]
    fn prepare_results_tolerates_nonfatal_emulation_command_error() {
        let res = check_prepare_results(
            "s1",
            vec![(
                "Emulation.setLocaleOverride",
                Err(Error::Command {
                    method: "Emulation.setLocaleOverride".into(),
                    message: "already set".into(),
                }),
            )],
        );
        assert!(res.is_ok());

        let dead = check_prepare_results(
            "s1",
            vec![(
                "Emulation.setLocaleOverride",
                Err(Error::SessionGone("s1".into())),
            )],
        );
        assert!(dead.is_err());
    }
}
