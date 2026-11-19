use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::{mpsc, watch};

use crate::browser::{SessionId, TargetId};
use crate::connection::Connection;
use crate::error::{Error, Result};

/// A raw CDP event delivered to a [`CdpSession`].
#[derive(Debug, Clone)]
pub struct CdpSessionEvent {
    pub method: String,
    pub params: Value,
}

struct CdpSessionInner {
    browser: Arc<Connection>,
    browser_pid: u32,
    session_id: SessionId,
    target_id: TargetId,
    detached: Arc<AtomicBool>,
    detach_tx: watch::Sender<bool>,
    runtime: tokio::runtime::Handle,
}

impl Drop for CdpSessionInner {
    fn drop(&mut self) {
        if !self.detached.swap(true, Ordering::SeqCst) {
            let _ = self.detach_tx.send(true);
            if !self.browser.is_dead() {
                let browser = self.browser.clone();
                let sid = self.session_id.0.clone();
                self.runtime.spawn(async move {
                    let _ = browser
                        .call(
                            "Target.detachFromTarget",
                            json!({ "sessionId": sid }),
                            None,
                            Duration::from_secs(5),
                        )
                        .await;
                });
            }
        }
    }
}

/// An explicit raw CDP session attached via `context.newCDPSession(page)`.
#[derive(Clone)]
pub struct CdpSession(Arc<CdpSessionInner>);

impl CdpSession {
    pub(crate) fn new(
        browser: Arc<Connection>,
        browser_pid: u32,
        session_id: SessionId,
        target_id: TargetId,
    ) -> Self {
        let (detach_tx, _) = watch::channel(false);
        Self(Arc::new(CdpSessionInner {
            browser,
            browser_pid,
            session_id,
            target_id,
            detached: Arc::new(AtomicBool::new(false)),
            detach_tx,
            runtime: tokio::runtime::Handle::current(),
        }))
    }

    pub fn session_id(&self) -> &str {
        &self.0.session_id.0
    }

    pub fn target_id(&self) -> &str {
        &self.0.target_id.0
    }

    pub fn is_detached(&self) -> bool {
        self.0.detached.load(Ordering::SeqCst) || self.0.browser.is_dead()
    }

    /// Sends a raw CDP command over this session and returns Chrome's result payload.
    pub async fn send(&self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        if self.0.detached.load(Ordering::SeqCst) {
            return Err(Error::Other("CDP session is detached".into()));
        }
        if self.0.browser.is_dead() {
            return Err(Error::BrowserDead {
                pid: self.0.browser_pid,
            });
        }
        self.0
            .browser
            .call(method, params, Some(&self.0.session_id.0), timeout)
            .await
    }

    /// Subscribes to CDP events emitted on this session; closed on `detach()` or disconnect.
    pub fn subscribe_events(&self) -> mpsc::UnboundedReceiver<CdpSessionEvent> {
        let (tx, rx) = mpsc::unbounded_channel();
        if self.is_detached() {
            return rx;
        }
        // Subscribe synchronously before spawning so events triggered right after are never missed.
        let mut raw = self.0.browser.subscribe_unbounded();
        let mut detach_rx = self.0.detach_tx.subscribe();
        let sid = self.0.session_id.0.clone();
        let detached = self.0.detached.clone();
        self.0.runtime.spawn(async move {
            loop {
                tokio::select! {
                    _ = detach_rx.wait_for(|d| *d) => break,
                    ev = raw.recv() => {
                        let Some(ev) = ev else {
                            detached.store(true, Ordering::SeqCst);
                            break;
                        };
                        if ev.method == "Fluxwright.disconnected" {
                            detached.store(true, Ordering::SeqCst);
                            break;
                        }
                        if ev.method == "Target.detachedFromTarget"
                            && ev.params["sessionId"].as_str() == Some(sid.as_str())
                        {
                            detached.store(true, Ordering::SeqCst);
                            break;
                        }
                        if ev.session_id.as_deref() == Some(sid.as_str())
                            && tx.send(CdpSessionEvent {
                                method: ev.method.clone(),
                                params: ev.params.clone(),
                            }).is_err()
                        {
                            break;
                        }
                    }
                }
            }
        });
        rx
    }

    /// Explicitly detaches the CDP session cleanly without tearing down the target page.
    pub async fn detach(&self, timeout: Duration) -> Result<()> {
        if !self.0.detached.swap(true, Ordering::SeqCst) {
            let _ = self.0.detach_tx.send(true);
            if !self.0.browser.is_dead() {
                let _ = self
                    .0
                    .browser
                    .call(
                        "Target.detachFromTarget",
                        json!({ "sessionId": self.0.session_id.0 }),
                        None,
                        timeout,
                    )
                    .await;
            }
        }
        Ok(())
    }
}
