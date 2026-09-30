use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::process::Child;
use tokio::sync::{broadcast, Mutex};
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::connection::{CdpEvent, Connection};
use crate::error::{Error, LaunchOptions, Result};
use crate::launch::{launch_chrome, Launched, Transport};
use crate::page::{fetch_params, CdpPage, ContextSetup};

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
        Self { server: server.into(), ..Self::default() }
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
    #[allow(dead_code)]
    target_id: String,
    session_id: Option<String>,
    r#type: String,
    url: String,
    browser_context_id: Option<String>,
    waiting: Vec<tokio::sync::oneshot::Sender<String>>,
}

pub struct CdpBrowser {
    pub id: BrowserId,
    pub pid: u32,
    conn: Arc<Connection>,
    _child: Mutex<Option<Child>>,
    _profile: Option<tempfile::TempDir>,
    targets: Arc<Mutex<HashMap<String, TargetState>>>,
    /// Blocking and proxy credentials per browser context, so iframes that attach later
    /// get them too.
    setups: Arc<Mutex<HashMap<String, ContextSetup>>>,
    pub events: broadcast::Sender<CdpEvent>,
}

impl CdpBrowser {
    pub async fn launch(opts: LaunchOptions) -> Result<Arc<Self>> {
        let launched = launch_chrome(&opts).await?;
        Self::from_launched(launched).await
    }

    async fn from_launched(launched: Launched) -> Result<Arc<Self>> {
        let conn = match launched.transport {
            #[cfg(windows)]
            Transport::WebSocket(url) => Connection::connect(&url).await?,
            #[cfg(unix)]
            Transport::Pipe { to_chrome, from_chrome } => Connection::from_pipes(to_chrome, from_chrome)?,
        };
        // Over a pipe nothing has waited for Chrome to start yet.
        conn.call("Browser.getVersion", json!({}), None, Duration::from_secs(20))
            .await
            .map_err(|e| Error::Launch(format!("chromium did not answer on its DevTools connection: {e}")))?;
        let (fanout, _) = broadcast::channel(512);
        let browser = Arc::new(Self {
            id: BrowserId(Uuid::new_v4()),
            pid: launched.pid,
            conn: conn.clone(),
            _child: Mutex::new(Some(launched.child)),
            _profile: Some(launched.user_data_dir),
            targets: Arc::new(Mutex::new(HashMap::new())),
            setups: Default::default(),
            events: fanout.clone(),
        });

        let b = browser.clone();
        let mut sub = conn.subscribe_unbounded();
        tokio::spawn(async move {
            while let Some(ev) = sub.recv().await {
                b.on_event(ev).await;
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
                        let _ = waiter.events.send(CdpEvent {
                            method: "Fluxwright.disconnected".into(),
                            params: json!({ "pid": waiter.pid }),
                            session_id: None,
                        });
                        break;
                    }
                    Some(Ok(None)) => {}
                    Some(Err(_)) | None => break,
                }
            }
        });

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
            .await?;

        info!(browser_id = %browser.id.0, pid = browser.pid, "browser ready");
        Ok(browser)
    }

    pub fn connection_dead(&self) -> bool {
        self.conn.is_dead()
    }

    async fn on_event(self: &Arc<Self>, ev: CdpEvent) {
        let _ = self.events.send(ev.clone());
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
                debug!(%target_id, %session_id, %ty, "attachedToTarget");
                let setup = match &ctx {
                    Some(c) => self.setups.lock().await.get(c).cloned(),
                    None => None,
                };

                {
                    let mut targets = self.targets.lock().await;
                    let state = targets.entry(target_id.clone()).or_insert(TargetState {
                        target_id: target_id.clone(),
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
                if ty != "page" && ty != "iframe" {
                    // Chrome's own targets (browser_ui, extension workers, background pages)
                    // and workers: nothing drives them, and setup hung on ~1 in 3 browser_ui
                    // sessions. Detach, as Playwright does; resume first in case one is paused.
                    let conn = self.conn.clone();
                    tokio::spawn(async move {
                        let t = Duration::from_secs(5);
                        if waiting {
                            let _ = conn
                                .call("Runtime.runIfWaitingForDebugger", json!({}), Some(&session_id), t)
                                .await;
                        }
                        let _ = conn
                            .call("Target.detachFromTarget", json!({ "sessionId": session_id }), None, t)
                            .await;
                    });
                    return;
                }
                // The session is published (and waiters woken) only once prepared, so a
                // page never reaches a caller with its domains still being enabled.
                let me = self.clone();
                tokio::spawn(async move {
                    prepare_session(&me.conn, &session_id, &ty, waiting, setup).await;
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
                });
            }
            "Target.targetCreated" => {
                let info = &ev.params["targetInfo"];
                let target_id = info["targetId"].as_str().unwrap_or_default().to_string();
                let mut targets = self.targets.lock().await;
                targets.entry(target_id.clone()).or_insert(TargetState {
                    target_id,
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
                let (Some(sid), Some(id)) = (ev.session_id.clone(), ev.params["requestId"].as_str()) else {
                    return;
                };
                let block = match self.setup_for_session(&sid).await {
                    Some(s) if s.proxy_auth.is_some() => {
                        let ty = ev.params["resourceType"].as_str().unwrap_or("");
                        s.types.iter().any(|t| t.as_str() == ty)
                    }
                    _ => true,
                };
                let (method, params) = if block {
                    ("Fetch.failRequest", json!({ "requestId": id, "errorReason": "BlockedByClient" }))
                } else {
                    ("Fetch.continueRequest", json!({ "requestId": id }))
                };
                let conn = self.conn.clone();
                tokio::spawn(async move {
                    let _ = conn.call(method, params, Some(&sid), Duration::from_secs(5)).await;
                });
            }
            // Nothing handles dialogs, and an open one blocks every call on its page until it
            // times out. As Playwright does by default: accept beforeunload so navigation goes
            // on, dismiss alert/confirm/prompt.
            "Page.javascriptDialogOpening" => {
                if let Some(sid) = ev.session_id.clone() {
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
                let (Some(sid), Some(id)) =
                    (ev.session_id.clone(), ev.params["requestId"].as_str().map(String::from))
                else {
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
            .call("Target.createBrowserContext", params, None, Duration::from_secs(10))
            .await?;
        let id = res["browserContextId"]
            .as_str()
            .ok_or_else(|| Error::Other("createBrowserContext: no id".into()))?
            .to_string();
        if let Some(Proxy { username: Some(user), password, .. }) = proxy {
            self.setups.lock().await.entry(id.clone()).or_default().proxy_auth =
                Some((user.clone(), password.clone().unwrap_or_default()));
        }
        Ok(ContextId(id))
    }

    pub async fn dispose_context(&self, id: &ContextId) -> Result<()> {
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
            events: self.events.clone(),
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
                    target_id: target_id.to_string(),
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
        targets.lock().await.get(target_id).and_then(|t| t.session_id.clone())
    }

    pub async fn iframe_session_for_url(&self, needle: &str) -> Option<String> {
        let targets = self.targets.lock().await;
        targets
            .values()
            .find(|t| t.r#type == "iframe" && t.url.contains(needle))
            .and_then(|t| t.session_id.clone())
    }

    pub async fn close(&self) -> Result<()> {
        let _ = self
            .call("Browser.close", json!({}), None, Duration::from_secs(5))
            .await;
        if let Some(mut child) = self._child.lock().await.take() {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
        self.conn.mark_dead();
        Ok(())
    }

    pub async fn kill_process(&self) -> Result<()> {
        if let Some(mut child) = self._child.lock().await.take() {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
        self.conn.mark_dead();
        Ok(())
    }
}

async fn prepare_session(
    conn: &Connection,
    session_id: &str,
    ty: &str,
    waiting_for_debugger: bool,
    setup: Option<ContextSetup>,
) {
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
    futures_util::future::join_all(
        calls
            .into_iter()
            .map(|(m, p)| conn.call(m, p, Some(session_id), Duration::from_secs(5))),
    )
    .await;
}
