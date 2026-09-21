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
use crate::launch::{launch_chrome, Launched};
use crate::page::CdpPage;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BrowserId(pub Uuid);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ContextId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TargetId(pub String);

struct TargetState {
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
    targets: Mutex<HashMap<String, TargetState>>,
    pub events: broadcast::Sender<CdpEvent>,
}

impl CdpBrowser {
    pub async fn launch(opts: LaunchOptions) -> Result<Arc<Self>> {
        let launched = launch_chrome(&opts).await?;
        Self::from_launched(launched).await
    }

    async fn from_launched(launched: Launched) -> Result<Arc<Self>> {
        let conn = Connection::connect(&launched.ws_url).await?;
        let (fanout, _) = broadcast::channel(512);
        let browser = Arc::new(Self {
            id: BrowserId(Uuid::new_v4()),
            pid: launched.pid,
            conn: conn.clone(),
            _child: Mutex::new(Some(launched.child)),
            _profile: Some(launched.user_data_dir),
            targets: Mutex::new(HashMap::new()),
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
                    "waitForDebuggerOnStart": false,
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

    async fn on_event(&self, ev: CdpEvent) {
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

                let waiters = {
                    let mut targets = self.targets.lock().await;
                    let state = targets.entry(target_id.clone()).or_insert(TargetState {
                        target_id: target_id.clone(),
                        session_id: None,
                        r#type: ty.clone(),
                        url: url.clone(),
                        browser_context_id: ctx.clone(),
                        waiting: Vec::new(),
                    });
                    state.session_id = Some(session_id.clone());
                    state.r#type = ty.clone();
                    state.url = url;
                    state.browser_context_id = ctx;
                    std::mem::take(&mut state.waiting)
                };
                for w in waiters {
                    let _ = w.send(session_id.clone());
                }

                // Do not await CDP on this task: it is the only consumer of
                // attachedToTarget and must stay ahead of createTarget.
                let conn = self.conn.clone();
                let sid = session_id;
                let waiting = ev.params["waitingForDebugger"].as_bool().unwrap_or(false);
                tokio::spawn(async move {
                    prepare_session(&conn, &sid, &ty, waiting).await;
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

    pub async fn create_context(&self) -> Result<ContextId> {
        let res = self
            .call(
                "Target.createBrowserContext",
                json!({ "disposeOnDetach": true }),
                None,
                Duration::from_secs(10),
            )
            .await?;
        let id = res["browserContextId"]
            .as_str()
            .ok_or_else(|| Error::Other("createBrowserContext: no id".into()))?;
        Ok(ContextId(id.to_string()))
    }

    pub async fn dispose_context(&self, id: &ContextId) -> Result<()> {
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

        let session_id = self.wait_for_session(&target_id, timeout).await?;
        prepare_session(&self.conn, &session_id, "page", false).await;
        Ok(CdpPage {
            browser: self.conn.clone(),
            browser_pid: self.pid,
            browser_id: self.id,
            context_id: ctx.clone(),
            target_id: TargetId(target_id),
            session_id: SessionId(session_id),
            events: self.events.clone(),
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
            // Also try explicit attach in case auto-attach missed it.
            let conn = self.conn.clone();
            let tid = target_id.to_string();
            tokio::spawn(async move {
                let _ = conn
                    .call(
                        "Target.attachToTarget",
                        json!({ "targetId": tid, "flatten": true }),
                        None,
                        Duration::from_secs(10),
                    )
                    .await;
            });
            match tokio::time::timeout(timeout, rx).await {
                Ok(Ok(sid)) => Ok(sid),
                Ok(Err(_)) => Err(Error::SessionGone(target_id.into())),
                Err(_) => Err(Error::timeout("attachToTarget", timeout.as_millis() as u64)),
            }
        }
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
) {
    let _ = conn
        .call(
            "Target.setAutoAttach",
            json!({
                "autoAttach": true,
                "waitForDebuggerOnStart": false,
                "flatten": true
            }),
            Some(session_id),
            Duration::from_secs(5),
        )
        .await;
    if waiting_for_debugger {
        let _ = conn
            .call(
                "Runtime.runIfWaitingForDebugger",
                json!({}),
                Some(session_id),
                Duration::from_secs(5),
            )
            .await;
    }
    if ty == "page" || ty == "iframe" {
        for method in ["Page.enable", "Runtime.enable", "Network.enable", "DOM.enable"] {
            let _ = conn
                .call(method, json!({}), Some(session_id), Duration::from_secs(5))
                .await;
        }
        let _ = conn
            .call(
                "Page.setLifecycleEventsEnabled",
                json!({ "enabled": true }),
                Some(session_id),
                Duration::from_secs(5),
            )
            .await;
    }
}
