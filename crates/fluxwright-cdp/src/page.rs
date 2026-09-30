use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::sync::Mutex;
use tokio::time::sleep;
use tracing::debug;

use crate::browser::{BrowserId, CdpBrowser, ContextId, SessionId, TargetId, TargetState};
use crate::connection::{CdpEvent, Connection};
use crate::error::{Error, Result};

/// When `goto` returns. Same meanings as Playwright's `waitUntil`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WaitUntil {
    /// The response arrived and the new document replaced the old one.
    Commit,
    /// `DOMContentLoaded`: HTML parsed, subresources (images, styles) may still be loading.
    DomContentLoaded,
    /// The `load` event: subresources done.
    #[default]
    Load,
    /// `load`, then no network requests for 500 ms (Chrome's `networkIdle` lifecycle event).
    NetworkIdle,
}

/// Selector engine plus element checks, run in the page or an iframe on every poll.
const QUERY_JS: &str = include_str!("query.js");

/// What an element call acts on: `query`, inside zero or more iframes (`frames`: one CSS
/// selector per iframe to enter, outermost first).
///
/// `query` engines, as in Playwright: CSS (plain or `css=`), `text=Sign in` (case-insensitive
/// substring), `text="Sign in"` (exact), `role=button[name="Sign in"]` (case-insensitive
/// substring of the accessible name; `[name="Sign in"s]` for exact).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Selector {
    pub frames: Vec<String>,
    pub query: String,
}

impl Selector {
    /// The `text=` query behind `get_by_text`.
    pub fn text(text: &str, exact: bool) -> String {
        if exact {
            format!("text={}", serde_json::to_string(text).unwrap())
        } else {
            format!("text={text}")
        }
    }

    /// The `role=` query behind `get_by_role`.
    pub fn role(role: &str, name: Option<&str>, exact: bool) -> String {
        match name {
            None => format!("role={role}"),
            Some(n) => format!(
                "role={role}[name={}{}]",
                serde_json::to_string(n).unwrap(),
                if exact { "s" } else { "" }
            ),
        }
    }
}

impl From<&str> for Selector {
    fn from(query: &str) -> Self {
        Self { frames: Vec::new(), query: query.into() }
    }
}

impl From<String> for Selector {
    fn from(query: String) -> Self {
        Self { frames: Vec::new(), query }
    }
}

impl From<&String> for Selector {
    fn from(query: &String) -> Self {
        query.as_str().into()
    }
}

impl From<&Selector> for Selector {
    fn from(s: &Selector) -> Self {
        s.clone()
    }
}

impl fmt::Display for Selector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for frame in &self.frames {
            write!(f, "{frame} >> ")?;
        }
        f.write_str(&self.query)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Wait {
    Attached,
    Visible,
    Click,
}

/// Where selector JS runs: a CDP session, an execution context in it (`None`: the main world),
/// and that frame's viewport offset in top-level page coordinates.
struct Scope {
    session: String,
    context: Option<i64>,
    /// Frame id of a cached isolated world, so a dead one can be dropped.
    world_of: Option<String>,
    dx: f64,
    dy: f64,
}

fn engine_call(query: &str, mode: &str) -> String {
    format!(
        "({QUERY_JS})({}, {})",
        serde_json::to_string(query).unwrap(),
        serde_json::to_string(mode).unwrap()
    )
}

/// Maps `exceptionDetails` to a JS-style error. `description` carries
/// "TypeError: msg\n    at <anonymous>:line:col"; a thrown non-Error only `text` + `value`.
fn js_error(res: &Value) -> Option<Error> {
    let d = &res["exceptionDetails"];
    if !d.is_object() {
        return None;
    }
    let msg = d["exception"]["description"]
        .as_str()
        .map(String::from)
        .unwrap_or_else(|| {
            format!("{} {}", d["text"].as_str().unwrap_or("Uncaught"), d["exception"]["value"])
        });
    Some(Error::JavaScript(msg))
}

/// Content-box origin of an iframe element, in its parent's viewport.
const FRAME_ORIGIN_JS: &str = "function() { const r = this.getBoundingClientRect(), s = getComputedStyle(this); \
    return [r.left + this.clientLeft + parseFloat(s.paddingLeft), r.top + this.clientTop + parseFloat(s.paddingTop)]; }";

#[derive(Clone)]
pub struct CdpPage {
    pub browser: Arc<Connection>,
    pub browser_pid: u32,
    pub browser_id: BrowserId,
    pub context_id: ContextId,
    pub target_id: TargetId,
    pub session_id: SessionId,
    pub events: tokio::sync::broadcast::Sender<CdpEvent>,
    pub(crate) targets: Arc<Mutex<HashMap<String, TargetState>>>,
    /// Isolated worlds made for same-process iframes, by frame id.
    pub(crate) worlds: Arc<Mutex<HashMap<String, i64>>>,
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

    /// Navigates and waits for the main frame's lifecycle event, like Playwright's `waitUntil`.
    pub async fn goto(&self, url: &str, wait_until: WaitUntil, timeout: Duration) -> Result<()> {
        let deadline = tokio::time::Instant::now() + timeout;
        // Subscribed before navigating, so no lifecycle event can slip past; unbounded, so a
        // busy browser cannot make it lag.
        let mut events = self.browser.subscribe_unbounded();
        let res = self
            .call("Page.navigate", json!({ "url": url }), timeout)
            .await?;
        if let Some(err) = res["errorText"].as_str().filter(|s| !s.is_empty()) {
            return Err(Error::Command {
                method: "Page.navigate".into(),
                message: format!("{err} at {url}"),
            });
        }
        // No loaderId: a same-document navigation (hash change), which is already done.
        let (Some(frame), Some(loader)) = (res["frameId"].as_str(), res["loaderId"].as_str())
        else {
            return Ok(());
        };
        let (event, ready_states): (&str, &[&str]) = match wait_until {
            WaitUntil::Commit => return Ok(()),
            WaitUntil::DomContentLoaded => ("DOMContentLoaded", &["interactive", "complete"]),
            WaitUntil::Load => ("load", &["complete"]),
            WaitUntil::NetworkIdle => ("networkIdle", &[]),
        };
        let mut tick = tokio::time::interval_at(
            tokio::time::Instant::now() + Duration::from_millis(500),
            Duration::from_millis(500),
        );
        loop {
            tokio::select! {
                ev = events.recv() => {
                    let Some(ev) = ev else {
                        return Err(Error::BrowserDead { pid: self.browser_pid });
                    };
                    if ev.method == "Fluxwright.disconnected" {
                        return Err(Error::BrowserDead { pid: self.browser_pid });
                    }
                    if ev.session_id.as_deref() != Some(self.sid()) {
                        continue;
                    }
                    match ev.method.as_str() {
                        "Page.lifecycleEvent"
                            if ev.params["name"] == event
                                && ev.params["frameId"] == frame
                                && ev.params["loaderId"] == loader =>
                        {
                            return Ok(())
                        }
                        "Inspector.targetCrashed" => {
                            return Err(Error::TargetCrashed { status: format!("during navigation to {url}") })
                        }
                        _ => {}
                    }
                }
                // Lifecycle events are switched on in prepare_session, which ignores errors.
                // Check readyState twice a second so a page that missed them still finishes.
                _ = tick.tick() => {
                    if self.browser.is_dead() {
                        return Err(Error::BrowserDead { pid: self.browser_pid });
                    }
                    if !ready_states.is_empty() {
                        if let Ok(v) = self.evaluate("document.readyState", Duration::from_secs(2)).await {
                            if ready_states.contains(&v.as_str().unwrap_or("")) {
                                return Ok(());
                            }
                        }
                    }
                }
                _ = tokio::time::sleep_until(deadline) => {
                    return Err(Error::timeout(
                        format!("navigation to {url} ({wait_until:?})"),
                        timeout.as_millis() as u64,
                    ));
                }
            }
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
        let res = self.eval_in(&self.main_scope(), expression, true, timeout).await?;
        Ok(res["value"].clone())
    }

    fn main_scope(&self) -> Scope {
        Scope { session: self.sid().into(), context: None, world_of: None, dx: 0.0, dy: 0.0 }
    }

    async fn session_call(&self, session: &str, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        if self.browser.is_dead() {
            return Err(Error::BrowserDead { pid: self.browser_pid });
        }
        self.browser.call(method, params, Some(session), timeout).await
    }

    /// `Runtime.evaluate` in a scope; returns the whole RemoteObject.
    async fn eval_in(&self, scope: &Scope, expression: &str, by_value: bool, timeout: Duration) -> Result<Value> {
        let mut params = json!({ "expression": expression, "returnByValue": by_value, "awaitPromise": true });
        if let Some(id) = scope.context {
            params["contextId"] = json!(id);
        }
        let res = self.session_call(&scope.session, "Runtime.evaluate", params, timeout).await?;
        match js_error(&res) {
            Some(e) => Err(e),
            None => Ok(res["result"].clone()),
        }
    }

    /// Enters `frames` one by one. `Ok(Err(reason))` means not there yet, so the caller retries.
    /// `scroll` brings each iframe into view first, which a click inside it needs.
    async fn scope(&self, frames: &[String], scroll: bool, timeout: Duration) -> Result<std::result::Result<Scope, String>> {
        let mut scope = self.main_scope();
        for css in frames {
            let find = format!(
                "(() => {{ const f = document.querySelector({}); if (f && {scroll}) f.scrollIntoViewIfNeeded(true); return f; }})()",
                serde_json::to_string(css).unwrap()
            );
            let el = match self.eval_in(&scope, &find, false, timeout).await {
                Ok(v) => v,
                Err(Error::JavaScript(m)) => {
                    return Err(Error::NotActionable { selector: css.clone(), reason: format!("invalid frame selector: {m}") })
                }
                Err(e) if e.is_retryable() => return Err(e),
                Err(e) => {
                    self.forget_world(&scope).await;
                    return Ok(Err(e.to_string()));
                }
            };
            let Some(obj) = el["objectId"].as_str().map(String::from) else {
                return Ok(Err(format!("no iframe matches {css}")));
            };
            let s = scope.session.clone();
            let origin = self
                .session_call(&s, "Runtime.callFunctionOn",
                    json!({ "objectId": obj, "returnByValue": true, "functionDeclaration": FRAME_ORIGIN_JS }), timeout)
                .await;
            let node = self.session_call(&s, "DOM.describeNode", json!({ "objectId": obj }), timeout).await;
            let _ = self.session_call(&s, "Runtime.releaseObject", json!({ "objectId": obj }), timeout).await;
            let (Ok(origin), Ok(node)) = (origin, node) else {
                return Ok(Err(format!("{css} went away")));
            };
            let Some(frame_id) = node["node"]["frameId"].as_str().map(String::from) else {
                return Ok(Err(format!("{css} is not an iframe, or has not loaded")));
            };
            let o = &origin["result"]["value"];
            let dx = scope.dx + o[0].as_f64().unwrap_or(0.0);
            let dy = scope.dy + o[1].as_f64().unwrap_or(0.0);
            // A cross-site iframe runs in its own process: its own target and session.
            if let Some(session) = CdpBrowser::session_for_target(&self.targets, &frame_id).await {
                scope = Scope { session, context: None, world_of: None, dx, dy };
                continue;
            }
            // Same process: an isolated world in that frame, made once and cached.
            let cached = self.worlds.lock().await.get(&frame_id).copied();
            let context = match cached {
                Some(id) => id,
                None => {
                    let made = self
                        .session_call(&s, "Page.createIsolatedWorld",
                            json!({ "frameId": frame_id, "worldName": "fluxwright" }), timeout)
                        .await;
                    // Fails while the frame loads, or for a cross-site frame not attached yet.
                    let Some(id) = made.as_ref().ok().and_then(|w| w["executionContextId"].as_i64()) else {
                        return Ok(Err(format!("{css}: frame not ready")));
                    };
                    self.worlds.lock().await.insert(frame_id.clone(), id);
                    id
                }
            };
            scope = Scope { session: s, context: Some(context), world_of: Some(frame_id), dx, dy };
        }
        Ok(Ok(scope))
    }

    /// Navigation destroys isolated worlds; drop a dead one so the next poll makes a new one.
    async fn forget_world(&self, scope: &Scope) {
        if let Some(frame) = &scope.world_of {
            self.worlds.lock().await.remove(frame);
        }
    }

    pub async fn set_viewport(&self, width: u32, height: u32, timeout: Duration) -> Result<()> {
        self.call(
            "Emulation.setDeviceMetricsOverride",
            json!({ "width": width, "height": height, "deviceScaleFactor": 0, "mobile": false }),
            timeout,
        )
        .await?;
        Ok(())
    }

    pub async fn screenshot(&self, full_page: bool, timeout: Duration) -> Result<Vec<u8>> {
        let mut params = json!({ "format": "png" });
        if full_page {
            let m = self.call("Page.getLayoutMetrics", json!({}), timeout).await?;
            let size = if m["cssContentSize"].is_object() { &m["cssContentSize"] } else { &m["contentSize"] };
            params["captureBeyondViewport"] = json!(true);
            params["clip"] = json!({
                "x": 0, "y": 0, "width": size["width"], "height": size["height"], "scale": 1
            });
        }
        let res = self
            .call("Page.captureScreenshot", params, timeout)
            .await?;
        let b64 = res["data"]
            .as_str()
            .ok_or_else(|| Error::Other("screenshot: no data".into()))?;
        use base64::Engine;
        base64::engine::general_purpose::STANDARD
            .decode(b64)
            .map_err(|e| Error::Other(e.to_string()))
    }

    /// Waits until the element is visible (Playwright's default `waitForSelector` state):
    /// non-empty box, not `visibility: hidden`. Opacity 0 counts as visible, as in Playwright.
    pub async fn wait_for_selector(&self, sel: &Selector, timeout: Duration) -> Result<Value> {
        Ok(self.wait_element(sel, Wait::Visible, timeout).await?.0)
    }

    /// `textContent` of the first match, once it exists (visible or not).
    pub async fn text_content(&self, sel: &Selector, timeout: Duration) -> Result<Option<String>> {
        let (v, _) = self.wait_element(sel, Wait::Attached, timeout).await?;
        Ok(v["text"].as_str().map(String::from))
    }

    /// Polls until the element is there (and visible, for `Visible`). `Click` also scrolls it
    /// into view and requires it enabled, unmoved across two polls, and topmost at its centre;
    /// the returned `x`/`y` are in top-level page coordinates.
    async fn wait_element(&self, sel: &Selector, wait: Wait, timeout: Duration) -> Result<(Value, Scope)> {
        let start = Instant::now();
        let mut last_pos: Option<(f64, f64)> = None;
        let mut last_reason = String::from("never checked");
        let mode = match wait {
            Wait::Attached => "attached",
            Wait::Visible => "visible",
            Wait::Click => "click",
        };
        let expr = engine_call(&sel.query, mode);
        loop {
            if start.elapsed() > timeout {
                return Err(Error::NotActionable {
                    selector: sel.to_string(),
                    reason: format!("timeout after {}ms (last: {last_reason})", timeout.as_millis()),
                });
            }
            if self.browser.is_dead() {
                return Err(Error::BrowserDead { pid: self.browser_pid });
            }
            let scope = match self.scope(&sel.frames, wait == Wait::Click, timeout).await? {
                Ok(scope) => scope,
                Err(reason) => {
                    last_reason = reason;
                    sleep(Duration::from_millis(50)).await;
                    continue;
                }
            };
            let v = match self.eval_in(&scope, &expr, true, timeout).await {
                Ok(r) => r["value"].clone(),
                Err(e) if e.is_retryable() => return Err(e),
                // Mid-navigation the context can vanish; keep polling.
                Err(e) => {
                    self.forget_world(&scope).await;
                    json!({ "ok": false, "reason": e.to_string() })
                }
            };
            if v["ok"].as_bool() == Some(true) {
                if wait != Wait::Click {
                    return Ok((v, scope));
                }
                let x = v["x"].as_f64().unwrap_or(0.0) + scope.dx;
                let y = v["y"].as_f64().unwrap_or(0.0) + scope.dy;
                if let Some((px, py)) = last_pos {
                    if (px - x).abs() < 1.0 && (py - y).abs() < 1.0 {
                        return Ok((json!({ "x": x, "y": y }), scope));
                    }
                }
                last_pos = Some((x, y));
                last_reason = "still moving".into();
            } else {
                last_reason = v["reason"].as_str().unwrap_or("unknown").to_string();
                if v["fatal"].as_bool() == Some(true) {
                    return Err(Error::NotActionable { selector: sel.to_string(), reason: last_reason });
                }
            }
            sleep(Duration::from_millis(50)).await;
        }
    }

    pub async fn click(&self, sel: &Selector, timeout: Duration) -> Result<()> {
        self.click_in_scope(sel, timeout).await.map(|_| ())
    }

    /// Clicks, and returns the scope the element lives in (for fill's follow-up steps).
    async fn click_in_scope(&self, sel: &Selector, timeout: Duration) -> Result<Scope> {
        let (pt, scope) = self.wait_element(sel, Wait::Click, timeout).await?;
        let x = pt["x"].as_f64().unwrap_or(0.0);
        let y = pt["y"].as_f64().unwrap_or(0.0);
        // Top-level session and coordinates: Chrome routes the event into the right frame.
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
        Ok(scope)
    }

    pub async fn fill(&self, sel: &Selector, value: &str, timeout: Duration) -> Result<()> {
        let scope = self.click_in_scope(sel, timeout).await?;
        let _ = self.eval_in(&scope, &engine_call(&sel.query, "clear"), true, timeout).await;
        // Sent to the frame's own session: a cross-site iframe has its own input handler.
        self.session_call(&scope.session, "Input.insertText", json!({ "text": value }), timeout)
            .await?;
        let _ = self.eval_in(&scope, &engine_call(&sel.query, "changed"), true, timeout).await;
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
