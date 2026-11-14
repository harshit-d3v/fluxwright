use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::process::Child;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::connection::{CdpEvent, Connection};
use crate::error::{Error, LaunchOptions, Result};
use crate::launch::{launch_chrome, Launched, Transport};
use crate::page::{fetch_params, CdpPage, ContextSetup};
use crate::route::InterceptedRequest;

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

pub(crate) struct TargetState {
    session_id: Option<String>,
    r#type: String,
    url: String,
    browser_context_id: Option<String>,
    waiting: Vec<tokio::sync::oneshot::Sender<String>>,
}

pub struct CdpBrowser {
    pub id: BrowserId,
    pub pid: u32,
    pub ws_endpoint: Option<String>,
    conn: Arc<Connection>,
    _child: Mutex<Option<Child>>,
    _profile: Option<tempfile::TempDir>,
    targets: Arc<Mutex<HashMap<String, TargetState>>>,
    /// Blocking and proxy credentials per browser context, so iframes that attach later
    /// get them too.
    setups: Arc<Mutex<HashMap<String, ContextSetup>>>,
    /// Browser contexts this connection created. On a remote browser (`connect`), targets in
    /// any other context belong to someone else: attaching prepares their pages and our dialog
    /// and route handlers then answer theirs.
    contexts: Arc<Mutex<HashSet<String>>>,
}

impl CdpBrowser {
    pub async fn launch(opts: LaunchOptions) -> Result<Arc<Self>> {
        let launched = launch_chrome(&opts).await?;
        Self::from_launched(launched).await
    }

    async fn from_launched(launched: Launched) -> Result<Arc<Self>> {
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
            ws_endpoint,
            conn: conn.clone(),
            _child: Mutex::new(Some(launched.child)),
            _profile: Some(launched.user_data_dir),
            targets: Arc::new(Mutex::new(HashMap::new())),
            setups: Default::default(),
            contexts: Default::default(),
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
        spawn_keepalive(browser.conn.clone());

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
            ws_endpoint: Some(ws_url),
            conn: conn.clone(),
            _child: Mutex::new(None),
            _profile: None,
            targets: Arc::new(Mutex::new(HashMap::new())),
            setups: Default::default(),
            contexts: Default::default(),
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

    async fn on_event(self: &Arc<Self>, ev: &CdpEvent) {
        match ev.method.as_str() {
            "Target.attachedToTarget" => {
                let info = &ev.params["targetInfo"];
                let target_id = info["targetId"].as_str().unwrap_or_default().to_string();
                let session_id = ev.params["sessionId"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                let ty = info["type"].as_str().unwrap_or("").to_string();
                let url = info["url"].as_str().unwrap_or("").to_string();
                let ctx = info["browserContextId"].as_str().map(|s| s.to_string());
                // Pages and out-of-process iframes count only in a context this connection
                // created; anything else on a remote browser belongs to its owner.
                let ours = match &ctx {
                    Some(c) => self.contexts.lock().await.contains(c),
                    None => false,
                };
                debug!(%target_id, %session_id, %ty, "attachedToTarget");
                let setup = match &ctx {
                    Some(c) => self.setups.lock().await.get(c).cloned(),
                    None => None,
                };

                {
                    let mut targets = self.targets.lock().await;
                    let state = targets.entry(target_id.clone()).or_insert(TargetState {
                        session_id: None,
                        r#type: ty.clone(),
                        url: url.clone(),
                        browser_context_id: ctx.clone(),
                        waiting: Vec::new(),
                    });
                    state.r#type = ty.clone();
                    state.url = url;
                    state.browser_context_id = ctx;
                }

                // Do not await CDP on this task: it is the only consumer of
                // attachedToTarget and must stay ahead of createTarget.
                let waiting = ev.params["waitingForDebugger"].as_bool().unwrap_or(false);
                if (ty != "page" && ty != "iframe") || !ours {
                    // Chrome's own targets (browser_ui, extension workers, background pages)
                    // and workers: nothing drives them, and setup hung on ~1 in 3 browser_ui
                    // sessions. Detach, as Playwright does; resume first in case one is paused.
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
                // The session is published (and waiters woken) only once prepared, so a
                // page never reaches a caller with its domains still being enabled.
                let me = self.clone();
                tokio::spawn(async move {
                    if prepare_session(&me.conn, &session_id, &ty, waiting, setup)
                        .await
                        .is_ok()
                    {
                        let waiters = match me.targets.lock().await.get_mut(&target_id) {
                            Some(t) => {
                                t.session_id = Some(session_id.clone());
                                std::mem::take(&mut t.waiting)
                            }
                            None => Vec::new(), // destroyed while preparing
                        };
                        for w in waiters {
                            let _ = w.send(session_id.clone());
                        }
                    } else {
                        let _ = me
                            .conn
                            .call(
                                "Target.detachFromTarget",
                                json!({ "sessionId": session_id }),
                                None,
                                Duration::from_secs(5),
                            )
                            .await;
                        // Dropping `waiting` wakes wait_for_session immediately with SessionGone
                        // instead of exposing an unprepared session or stalling until timeout.
                        let _ = me
                            .targets
                            .lock()
                            .await
                            .get_mut(&target_id)
                            .map(|t| std::mem::take(&mut t.waiting));
                    }
                });
            }
            "Target.targetCreated" => {
                let info = &ev.params["targetInfo"];
                let target_id = info["targetId"].as_str().unwrap_or_default().to_string();
                let mut targets = self.targets.lock().await;
                targets.entry(target_id).or_insert(TargetState {
                    session_id: None,
                    r#type: info["type"].as_str().unwrap_or("").to_string(),
                    url: info["url"].as_str().unwrap_or("").to_string(),
                    browser_context_id: info["browserContextId"].as_str().map(|s| s.to_string()),
                    waiting: Vec::new(),
                });
            }
            "Target.targetDestroyed" => {
                if let Some(id) = ev.params["targetId"].as_str() {
                    self.targets.lock().await.remove(id);
                }
            }
            "Target.detachedFromTarget" => {
                if let Some(sid) = ev.params["sessionId"].as_str() {
                    let mut targets = self.targets.lock().await;
                    for t in targets.values_mut() {
                        if t.session_id.as_deref() == Some(sid) {
                            t.session_id = None;
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
                let setup = self.setup_for_session(&sid).await;
                // Paused because its type is blocked, or because everything is paused (proxy
                // credentials, interception). No setup means the context is gone.
                let ty = ev.params["resourceType"].as_str().unwrap_or("");
                let block = setup
                    .as_ref()
                    .is_none_or(|s| s.types.iter().any(|t| t.as_str() == ty));
                if !block {
                    if let Some(routes) = setup.as_ref().and_then(|s| s.routes.as_ref()) {
                        // The job answers it; if it can't (gone), dropping it continues it.
                        if let Some(req) = InterceptedRequest::from_paused(
                            &ev.params,
                            self.conn.clone(),
                            sid.clone(),
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
                    // The job answers it (page.on('dialog')). A dialog it cannot take is
                    // dropped, which answers it as below.
                    if let Some(tx) = self.setup_for_session(&sid).await.and_then(|s| s.dialogs) {
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
                let mut response = json!({ "response": "Default" });
                if let (Some(ctx), true) = (ctx, ev.params["authChallenge"]["source"] == "Proxy") {
                    if let Some(setup) = self.setups.lock().await.get_mut(&ctx) {
                        if let Some((user, pass)) = setup.proxy_auth.clone() {
                            response = if setup.answered.insert(id.clone()) {
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
        if let Some(Proxy {
            username: Some(user),
            password,
            ..
        }) = proxy
        {
            self.setups
                .lock()
                .await
                .entry(id.clone())
                .or_default()
                .proxy_auth = Some((user.clone(), password.clone().unwrap_or_default()));
        }
        Ok(ContextId(id))
    }

    pub async fn dispose_context(&self, id: &ContextId) -> Result<()> {
        self.contexts.lock().await.remove(&id.0);
        self.setups.lock().await.remove(&id.0);
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
        Ok(CdpPage {
            browser: self.conn.clone(),
            browser_pid: self.pid,
            browser_id: self.id,
            context_id: ctx.clone(),
            target_id: TargetId(target_id),
            session_id: SessionId(session_id),
            targets: self.targets.clone(),
            worlds: Default::default(),
            setups: self.setups.clone(),
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
                .or_insert(TargetState {
                    session_id: None,
                    r#type: "page".into(),
                    url: String::new(),
                    browser_context_id: None,
                    waiting: Vec::new(),
                })
                .waiting
                .push(tx);
            drop(targets);
            // No explicit Target.attachToTarget: browser-level auto-attach covers new pages,
            // and a second attach opens a second session that duplicates every event.
            match tokio::time::timeout(timeout, rx).await {
                Ok(Ok(sid)) => Ok(sid),
                Ok(Err(_)) => Err(Error::SessionGone(target_id.into())),
                Err(_) => Err(Error::timeout("attachToTarget", timeout.as_millis() as u64)),
            }
        }
    }

    async fn context_of_session(&self, sid: &str) -> Option<String> {
        self.targets
            .lock()
            .await
            .values()
            .find(|t| t.session_id.as_deref() == Some(sid))
            .and_then(|t| t.browser_context_id.clone())
    }

    async fn setup_for_session(&self, sid: &str) -> Option<ContextSetup> {
        let ctx = self.context_of_session(sid).await?;
        self.setups.lock().await.get(&ctx).cloned()
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

    /// Sessions of every target in a browser context.
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
        // The authority ends at the first `/`, `?` or `#`; a query string or trailing slash
        // before /json/version is tolerated.
        let rest = rest.split(['?', '#']).next().unwrap_or(rest);
        let (host_port, path) = match rest.split_once('/') {
            Some((hp, p)) => (hp, format!("/{}", p.trim_matches('/'))),
            None => (rest, String::new()),
        };
        let path = if path.is_empty() || path == "/json/version" {
            "/json/version".to_string()
        } else {
            format!("{}/json/version", path.trim_end_matches('/'))
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
            let mut resp = String::new();
            (&mut stream)
                .take(64 * 1024)
                .read_to_string(&mut resp)
                .await
                .map_err(|e| Error::Launch(e.to_string()))?;
            if let Some((_hdr, body)) = resp.split_once("\r\n\r\n") {
                let json: Value = serde_json::from_str(body)
                    .map_err(|e| Error::Launch(format!("invalid JSON from {path}: {e}")))?;
                if let Some(ws) = json["webSocketDebuggerUrl"].as_str() {
                    return Ok(ws.to_string());
                }
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

async fn prepare_session(
    conn: &Connection,
    session_id: &str,
    ty: &str,
    waiting_for_debugger: bool,
    setup: Option<ContextSetup>,
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
    if let Some(setup) = setup.filter(|_| ty == "page" || ty == "iframe") {
        if let Some(e) = &setup.emulation {
            calls.extend(e.calls(ty == "page"));
        }
        for source in &setup.init_scripts {
            calls.push((
                "Page.addScriptToEvaluateOnNewDocument",
                json!({ "source": source }),
            ));
        }
        if let Some(params) = fetch_params(&setup) {
            calls.push(("Fetch.enable", params));
        }
        if !setup.urls.is_empty() {
            calls.push(("Network.enable", json!({})));
            calls.push(("Network.setBlockedURLs", json!({ "urls": setup.urls })));
        }
    }
    if waiting_for_debugger {
        calls.push(("Runtime.runIfWaitingForDebugger", json!({})));
    }
    // Pipelined: Chrome runs a session's commands in order, so one round trip instead of
    // one per command. Sequential calls cost ~1 s per page on a busy browser.
    let methods: Vec<&'static str> = calls.iter().map(|(m, _)| *m).collect();
    let results = futures_util::future::join_all(
        calls
            .into_iter()
            .map(|(m, p)| conn.call(m, p, Some(session_id), Duration::from_secs(5))),
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
            ws_endpoint: Some(ws_url),
            conn: conn.clone(),
            _child: Mutex::new(None),
            _profile: None,
            targets: Arc::new(Mutex::new(HashMap::new())),
            setups: Default::default(),
            contexts: Default::default(),
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
            ws_endpoint: Some(ws_url),
            conn: conn.clone(),
            _child: Mutex::new(None),
            _profile: None,
            targets: Arc::new(Mutex::new(HashMap::new())),
            setups: Default::default(),
            contexts: Default::default(),
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
