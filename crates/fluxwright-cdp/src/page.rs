use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::time::sleep;
use tracing::debug;

use crate::browser::{BrowserId, ContextId, SessionId, TargetId};
use crate::connection::{CdpEvent, Connection};
use crate::error::{Error, Result};

#[derive(Clone)]
pub struct CdpPage {
    pub browser: Arc<Connection>,
    pub browser_pid: u32,
    pub browser_id: BrowserId,
    pub context_id: ContextId,
    pub target_id: TargetId,
    pub session_id: SessionId,
    pub events: tokio::sync::broadcast::Sender<CdpEvent>,
}

impl CdpPage {
    fn sid(&self) -> &str {
        &self.session_id.0
    }

    pub async fn call(&self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        if self.browser.is_dead() {
            return Err(Error::BrowserDead {
                pid: self.browser_pid,
            });
        }
        self.browser
            .call(method, params, Some(self.sid()), timeout)
            .await
    }

    pub async fn goto(&self, url: &str, timeout: Duration) -> Result<()> {
        self.call("Page.navigate", json!({ "url": url }), timeout)
            .await?;
        let start = Instant::now();
        loop {
            if start.elapsed() > timeout {
                return Err(Error::timeout("navigation", timeout.as_millis() as u64));
            }
            if self.browser.is_dead() {
                return Err(Error::BrowserDead {
                    pid: self.browser_pid,
                });
            }
            let state = self
                .evaluate(
                    "document.readyState + ' ' + location.href",
                    Duration::from_secs(5),
                )
                .await;
            match state {
                Ok(v) => {
                    let s = v.as_str().unwrap_or("");
                    if s.starts_with("complete") {
                        return Ok(());
                    }
                }
                Err(e) if e.is_retryable() => return Err(e),
                Err(_) => {}
            }
            sleep(Duration::from_millis(50)).await;
        }
    }

    pub async fn title(&self, timeout: Duration) -> Result<String> {
        let v = self
            .evaluate("document.title", timeout)
            .await?;
        Ok(v.as_str().unwrap_or("").to_string())
    }

    pub async fn content(&self, timeout: Duration) -> Result<String> {
        let v = self
            .evaluate("document.documentElement.outerHTML", timeout)
            .await?;
        Ok(v.as_str().unwrap_or("").to_string())
    }

    pub async fn evaluate(&self, expression: &str, timeout: Duration) -> Result<Value> {
        let res = self
            .call(
                "Runtime.evaluate",
                json!({
                    "expression": expression,
                    "returnByValue": true,
                    "awaitPromise": true
                }),
                timeout,
            )
            .await?;
        if res["exceptionDetails"].is_object() {
            return Err(Error::Command {
                method: "Runtime.evaluate".into(),
                message: res["exceptionDetails"].to_string(),
            });
        }
        Ok(res["result"]["value"].clone())
    }

    pub async fn screenshot(&self, timeout: Duration) -> Result<Vec<u8>> {
        let res = self
            .call(
                "Page.captureScreenshot",
                json!({ "format": "png" }),
                timeout,
            )
            .await?;
        let b64 = res["data"]
            .as_str()
            .ok_or_else(|| Error::Other("screenshot: no data".into()))?;
        use base64::Engine;
        base64::engine::general_purpose::STANDARD
            .decode(b64)
            .map_err(|e| Error::Other(e.to_string()))
    }

    pub async fn wait_for_selector(&self, selector: &str, timeout: Duration) -> Result<Value> {
        self.wait_actionable(selector, timeout).await
    }

    async fn wait_actionable(&self, selector: &str, timeout: Duration) -> Result<Value> {
        let start = Instant::now();
        let mut last_pos: Option<(f64, f64)> = None;
        let expr = format!(
            r#"(function() {{
                const sel = {sel};
                const el = document.querySelector(sel);
                if (!el) return {{ ok: false, reason: 'detached' }};
                const st = getComputedStyle(el);
                if (st.display === 'none' || st.visibility === 'hidden' || Number(st.opacity) === 0)
                    return {{ ok: false, reason: 'hidden' }};
                const r = el.getBoundingClientRect();
                if (r.width === 0 || r.height === 0) return {{ ok: false, reason: 'hidden' }};
                const disabled = el.disabled === true || el.getAttribute('aria-disabled') === 'true';
                if (disabled) return {{ ok: false, reason: 'disabled' }};
                return {{ ok: true, x: r.x + r.width/2, y: r.y + r.height/2, enabled: !disabled }};
            }})()"#,
            sel = serde_json::to_string(selector).unwrap()
        );
        loop {
            if start.elapsed() > timeout {
                return Err(Error::NotActionable {
                    selector: selector.into(),
                    reason: "timeout".into(),
                });
            }
            if self.browser.is_dead() {
                return Err(Error::BrowserDead {
                    pid: self.browser_pid,
                });
            }
            let v = self.evaluate(&expr, timeout).await.unwrap_or(json!({ "ok": false, "reason": "eval" }));
            if v["ok"].as_bool() == Some(true) {
                let x = v["x"].as_f64().unwrap_or(0.0);
                let y = v["y"].as_f64().unwrap_or(0.0);
                if let Some((px, py)) = last_pos {
                    if (px - x).abs() < 1.0 && (py - y).abs() < 1.0 {
                        return Ok(v);
                    }
                }
                last_pos = Some((x, y));
            }
            sleep(Duration::from_millis(50)).await;
        }
    }

    pub async fn click(&self, selector: &str, timeout: Duration) -> Result<()> {
        let box_ = self.wait_actionable(selector, timeout).await?;
        let x = box_["x"].as_f64().unwrap_or(0.0);
        let y = box_["y"].as_f64().unwrap_or(0.0);
        let _ = self
            .evaluate(
                &format!(
                    "document.querySelector({}).scrollIntoView({{block:'center'}})",
                    serde_json::to_string(selector).unwrap()
                ),
                timeout,
            )
            .await;
        for (ty, btn) in [("mouseMoved", "none"), ("mousePressed", "left"), ("mouseReleased", "left")]
        {
            self.call(
                "Input.dispatchMouseEvent",
                json!({
                    "type": ty,
                    "x": x,
                    "y": y,
                    "button": btn,
                    "clickCount": if ty == "mouseMoved" { 0 } else { 1 }
                }),
                timeout,
            )
            .await?;
        }
        Ok(())
    }

    pub async fn fill(&self, selector: &str, value: &str, timeout: Duration) -> Result<()> {
        self.click(selector, timeout).await?;
        let _ = self
            .evaluate(
                &format!(
                    r#"(function(){{ const el = document.querySelector({sel});
                        if (!el) return;
                        el.focus();
                        if ('value' in el) el.value = '';
                    }})()"#,
                    sel = serde_json::to_string(selector).unwrap()
                ),
                timeout,
            )
            .await;
        self.call(
            "Input.insertText",
            json!({ "text": value }),
            timeout,
        )
        .await?;
        let _ = self
            .evaluate(
                &format!(
                    r#"(function(){{ const el = document.querySelector({sel});
                        if (!el) return;
                        el.dispatchEvent(new Event('input', {{ bubbles: true }}));
                        el.dispatchEvent(new Event('change', {{ bubbles: true }}));
                    }})()"#,
                    sel = serde_json::to_string(selector).unwrap()
                ),
                timeout,
            )
            .await;
        Ok(())
    }

    pub async fn set_blocked_urls(&self, urls: &[String], timeout: Duration) -> Result<()> {
        self.call(
            "Network.setBlockedURLs",
            json!({ "urls": urls }),
            timeout,
        )
        .await?;
        Ok(())
    }

    pub async fn block_resource_types(
        &self,
        types: &[ResourceType],
        timeout: Duration,
    ) -> Result<()> {
        if types.is_empty() {
            return Ok(());
        }
        self.call(
            "Fetch.enable",
            json!({
                "patterns": types.iter().map(|t| json!({
                    "urlPattern": "*",
                    "resourceType": t.as_str(),
                    "requestStage": "Request"
                })).collect::<Vec<_>>()
            }),
            timeout,
        )
        .await?;
        let mut rx = self.events.subscribe();
        let sid = self.sid().to_string();
        let conn = self.browser.clone();
        let pid = self.browser_pid;
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(ev) if ev.method == "Fetch.requestPaused" && ev.session_id.as_deref() == Some(sid.as_str()) => {
                        let id = ev.params["requestId"].as_str().unwrap_or("");
                        let _ = conn
                            .call(
                                "Fetch.failRequest",
                                json!({ "requestId": id, "errorReason": "BlockedByClient" }),
                                Some(&sid),
                                Duration::from_secs(5),
                            )
                            .await;
                    }
                    Ok(ev) if ev.method == "Fluxwright.disconnected" => break,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    Err(_) => {
                        if conn.is_dead() {
                            debug!(pid, "fetch blocker stopping");
                            break;
                        }
                    }
                    _ => {}
                }
            }
        });
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceType {
    Image,
    Font,
    Media,
    Stylesheet,
    Script,
}

impl ResourceType {
    pub fn as_str(self) -> &'static str {
        match self {
            ResourceType::Image => "Image",
            ResourceType::Font => "Font",
            ResourceType::Media => "Media",
            ResourceType::Stylesheet => "Stylesheet",
            ResourceType::Script => "Script",
        }
    }
}
