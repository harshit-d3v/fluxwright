use std::collections::HashMap;
use std::fmt;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::sync::Mutex;
use tokio::time::sleep;

use crate::browser::{BrowserId, CdpBrowser, ContextId, SessionId, TargetId, TargetState};
use crate::connection::{CdpEvent, Connection};
use crate::context::{permission_types, Cookie, Emulation, OriginStorage, StorageState, LOCAL_STORAGE_JS};
use crate::log::{DownloadResult, PageLog};

/// Finished downloads, oldest first.
pub type DownloadQueue = tokio::sync::mpsc::UnboundedReceiver<DownloadResult>;
use crate::route::InterceptedRequest;
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
/// `query` engines, as in Playwright: CSS (plain or `css=`), `text=Sign in` or
/// `text="Sign in"i` (case-insensitive substring), `text="Sign in"` (exact),
/// `role=button[name="Sign in"]` (case-insensitive substring of the accessible name;
/// `[name="Sign in"s]` for exact), `label=` and `placeholder=` (matched like `text=`), and
/// `testid="id"` (`data-testid`). Parts joined by ` >> ` narrow the match: another selector
/// searched inside each match, `nth=N` (negative counts from the end), or `has-text="..."i`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Selector {
    pub frames: Vec<String>,
    pub query: String,
}

impl Selector {
    /// `engine="text"` (exact) or `engine="text"i` (case-insensitive substring). Quoted either
    /// way, so a `>>` inside the text does not split the selector.
    fn matching(engine: &str, text: &str, exact: bool) -> String {
        format!("{engine}={}{}", serde_json::to_string(text).unwrap(), if exact { "" } else { "i" })
    }

    /// The `text=` query behind `get_by_text`.
    pub fn text(text: &str, exact: bool) -> String {
        Self::matching("text", text, exact)
    }

    /// The `label=` query behind `get_by_label`: `<label>`, `aria-labelledby` or `aria-label`.
    pub fn label(text: &str, exact: bool) -> String {
        Self::matching("label", text, exact)
    }

    /// The `placeholder=` query behind `get_by_placeholder`.
    pub fn placeholder(text: &str, exact: bool) -> String {
        Self::matching("placeholder", text, exact)
    }

    /// The `testid=` query behind `get_by_test_id`: `data-testid`, exact.
    pub fn test_id(id: &str) -> String {
        format!("testid={}", serde_json::to_string(id).unwrap())
    }

    /// The `has-text=` part behind `filter({ hasText })`: case-insensitive substring.
    pub fn has_text(text: &str) -> String {
        Self::matching("has-text", text, false)
    }

    /// This selector narrowed by `part`: another selector searched inside each match,
    /// `nth=N`, or `has-text="..."i`.
    pub fn then(&self, part: &str) -> Selector {
        Selector { frames: self.frames.clone(), query: format!("{} >> {part}", self.query) }
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
    /// Visible, scrolled into view and unmoved across two polls: for element screenshots.
    Box,
    /// Attached; reports the box without scrolling, `null` when hidden.
    Rect,
}

/// An element's box in CSS pixels, relative to the top-level page's viewport.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
pub struct BoundingBox {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// Where selector JS runs: a CDP session, an execution context in it (`None`: the main world),
/// and that frame's viewport offset in top-level page coordinates.
#[derive(Clone)]
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
    pub(crate) setups: Arc<Mutex<HashMap<String, ContextSetup>>>,
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
            .await
            .map_err(|e| match e {
                // "Cannot navigate to invalid URL" names no URL by itself.
                Error::Command { method, message } => Error::Command { method, message: format!("{message}: {url}") },
                other => other,
            })?;
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
    /// Element code always runs in an isolated world, where page scripts cannot patch what it
    /// calls (`querySelector`, `getBoundingClientRect`, ...). `evaluate` stays in the page's world.
    /// Also returns each iframe hop as (parent scope, iframe selector), for [`Self::covered`].
    async fn scope(
        &self,
        frames: &[String],
        scroll: bool,
        timeout: Duration,
    ) -> Result<std::result::Result<(Scope, Vec<(Scope, String)>), String>> {
        // A page target's main frame id is its target id.
        let Some(mut scope) = self.world(self.sid(), &self.target_id.0, 0.0, 0.0, timeout).await else {
            return Ok(Err("page not ready".into()));
        };
        let mut hops = Vec::new();
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
            let session = CdpBrowser::session_for_target(&self.targets, &frame_id).await.unwrap_or(s);
            match self.world(&session, &frame_id, dx, dy, timeout).await {
                Some(next) => hops.push((std::mem::replace(&mut scope, next), css.clone())),
                // Still loading, or a cross-site frame whose session is not attached yet.
                None => return Ok(Err(format!("{css}: frame not ready"))),
            }
        }
        Ok(Ok((scope, hops)))
    }

    /// A click inside an iframe lands only if nothing in a parent document covers the iframe
    /// at that point: the element's own hit test cannot see a parent's overlay.
    async fn covered(&self, hops: &[(Scope, String)], x: f64, y: f64, timeout: Duration) -> Option<String> {
        for (parent, css) in hops {
            let css = serde_json::to_string(css).unwrap();
            let expr = format!(
                "(() => {{ const f = document.querySelector({css}); const h = document.elementFromPoint({}, {}); \
                 return f && h === f ? '' : 'obscured by <' + (h ? h.tagName.toLowerCase() + (h.id ? '#' + h.id : '') : 'nothing') + '> over ' + {css}; }})()",
                x - parent.dx,
                y - parent.dy
            );
            match self.eval_in(parent, &expr, true, timeout).await {
                Ok(v) if v["value"] == "" => {}
                Ok(v) => return Some(v["value"].as_str().unwrap_or("frame hit test failed").to_string()),
                Err(e) => return Some(e.to_string()),
            }
        }
        None
    }

    /// The isolated world of `frame_id`, made once per document and cached.
    async fn world(&self, session: &str, frame_id: &str, dx: f64, dy: f64, timeout: Duration) -> Option<Scope> {
        let cached = self.worlds.lock().await.get(frame_id).copied();
        let id = match cached {
            Some(id) => id,
            None => {
                let w = self
                    .session_call(session, "Page.createIsolatedWorld",
                        json!({ "frameId": frame_id, "worldName": "fluxwright" }), timeout)
                    .await
                    .ok()?;
                let id = w["executionContextId"].as_i64()?;
                self.worlds.lock().await.insert(frame_id.to_string(), id);
                id
            }
        };
        Some(Scope { session: session.to_string(), context: Some(id), world_of: Some(frame_id.to_string()), dx, dy })
    }

    /// Navigation destroys isolated worlds; drop a dead one so the next poll makes a new one.
    async fn forget_world(&self, scope: &Scope) {
        if let Some(frame) = &scope.world_of {
            self.worlds.lock().await.remove(frame);
        }
    }

    /// Applies `e` to this page now, and to iframes and popups that attach later.
    pub async fn emulate(&self, e: &Emulation, timeout: Duration) -> Result<()> {
        if e.is_empty() {
            return Ok(());
        }
        let mut e = e.clone();
        if e.locale.is_some() && e.user_agent.is_none() {
            let v = self.browser.call("Browser.getVersion", json!({}), None, timeout).await?;
            e.user_agent = v["userAgent"].as_str().map(str::to_owned);
        }
        self.setups.lock().await.entry(self.context_id.0.clone()).or_default().emulation = Some(e.clone());
        self.pipeline(e.calls(true), timeout).await
    }

    /// Grants Playwright-named permissions (`geolocation`, `notifications`, ...) to every origin
    /// in this page's context.
    pub async fn grant_permissions(&self, names: &[String], timeout: Duration) -> Result<()> {
        let permissions = permission_types(names)?;
        if permissions.is_empty() {
            return Ok(());
        }
        self.browser
            .call(
                "Browser.grantPermissions",
                json!({ "permissions": permissions, "browserContextId": self.context_id.0 }),
                None,
                timeout,
            )
            .await?;
        Ok(())
    }

    /// Starts this page's context from saved cookies and localStorage. Call it before the
    /// first navigation.
    pub async fn set_storage_state(&self, state: &StorageState, timeout: Duration) -> Result<()> {
        if !state.cookies.is_empty() {
            let cookies: Vec<Value> = state.cookies.iter().map(Cookie::to_cdp).collect();
            self.browser
                .call(
                    "Storage.setCookies",
                    json!({ "cookies": cookies, "browserContextId": self.context_id.0 }),
                    None,
                    timeout,
                )
                .await?;
        }
        if let Some(source) = state.restore_script() {
            self.setups.lock().await.entry(self.context_id.0.clone()).or_default().init_scripts.push(source.clone());
            self.call("Page.addScriptToEvaluateOnNewDocument", json!({ "source": source }), timeout).await?;
        }
        Ok(())
    }

    /// Every cookie in this page's context, plus localStorage of the page's current origin.
    pub async fn storage_state(&self, timeout: Duration) -> Result<StorageState> {
        let res = self
            .browser
            .call("Storage.getCookies", json!({ "browserContextId": self.context_id.0 }), None, timeout)
            .await?;
        let cookies = res["cookies"].as_array().map(|a| a.iter().filter_map(Cookie::from_cdp).collect()).unwrap_or_default();
        let local: Option<OriginStorage> = serde_json::from_value(self.evaluate(LOCAL_STORAGE_JS, timeout).await?).unwrap_or(None);
        let origins = local.into_iter().filter(|o| !o.local_storage.is_empty()).collect();
        Ok(StorageState { cookies, origins })
    }

    /// Sends `calls` in one round trip (Chrome runs a session's commands in order) and
    /// returns the first error.
    async fn pipeline(&self, calls: Vec<(&'static str, Value)>, timeout: Duration) -> Result<()> {
        let results = futures_util::future::join_all(calls.into_iter().map(|(m, p)| self.call(m, p, timeout))).await;
        results.into_iter().find(|r| r.is_err()).unwrap_or(Ok(Value::Null)).map(|_| ())
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

    /// The first match's box relative to the top-level viewport, without scrolling; `None`
    /// when it is not visible. Waits until the element exists.
    pub async fn bounding_box(&self, sel: &Selector, timeout: Duration) -> Result<Option<BoundingBox>> {
        let (v, scope) = self.wait_element(sel, Wait::Rect, timeout).await?;
        let b = &v["box"];
        if b.is_null() {
            return Ok(None);
        }
        let f = |k: &str| b[k].as_f64().unwrap_or(0.0);
        Ok(Some(BoundingBox { x: f("x") + scope.dx, y: f("y") + scope.dy, width: f("width"), height: f("height") }))
    }

    /// Calls `function` (JavaScript source, such as `(el, arg) => el.value`) with the first match
    /// and `arg`, in the page's own world as Playwright does, and returns its JSON result.
    pub async fn evaluate_on(&self, sel: &Selector, function: &str, arg: Option<Value>, timeout: Duration) -> Result<Value> {
        let (_, scope) = self.wait_element(sel, Wait::Attached, timeout).await?;
        let found = self.eval_in(&scope, &engine_call(&sel.query, "element"), false, timeout).await?;
        let Some(isolated) = found["objectId"].as_str().filter(|_| found["subtype"] == "node") else {
            return Err(Error::NotActionable { selector: sel.to_string(), reason: "element went away".into() });
        };
        // The engine ran in an isolated world; hand the same node to the page's main world,
        // where page scripts' globals live.
        let call = |method: &'static str, params: Value| self.session_call(&scope.session, method, params, timeout);
        let node = call("DOM.describeNode", json!({ "objectId": isolated })).await?;
        let main = call("DOM.resolveNode", json!({ "backendNodeId": node["node"]["backendNodeId"] })).await?;
        let element = main["object"]["objectId"].clone();
        let mut args = vec![json!({ "objectId": element })];
        if let Some(arg) = arg {
            args.push(json!({ "value": arg }));
        }
        let res = call(
            "Runtime.callFunctionOn",
            json!({ "functionDeclaration": function, "objectId": element, "arguments": args,
                    "returnByValue": true, "awaitPromise": true }),
        )
        .await;
        for id in [json!(isolated), element] {
            let _ = call("Runtime.releaseObject", json!({ "objectId": id })).await;
        }
        let res = res?;
        match js_error(&res) {
            Some(e) => Err(e),
            None => Ok(res["result"]["value"].clone()),
        }
    }

    /// Hands every request of this page, its popups and iframes to the returned channel, as
    /// Playwright's `page.route` does. Resource blocking still applies first. Call it before
    /// navigating; a request dropped unanswered continues.
    pub async fn intercept(&self, timeout: Duration) -> Result<tokio::sync::mpsc::UnboundedReceiver<InterceptedRequest>> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let params = {
            let mut setups = self.setups.lock().await;
            let setup = setups.entry(self.context_id.0.clone()).or_default();
            setup.routes = Some(tx);
            fetch_params(setup)
        };
        if let Some(params) = params {
            // Sessions already open in this context too: iframes in other processes, popups.
            let mut sessions = CdpBrowser::sessions_in_context(&self.targets, &self.context_id.0).await;
            if !sessions.iter().any(|s| s == self.sid()) {
                sessions.push(self.sid().to_string());
            }
            for s in sessions {
                self.session_call(&s, "Fetch.enable", params.clone(), timeout).await?;
            }
        }
        Ok(rx)
    }

    /// Starts recording console messages, uncaught errors and finished downloads (saved under
    /// `download_dir`) from this page, its popups and its iframes. Recording stops when the
    /// returned task is aborted.
    pub fn collect_logs(&self, download_dir: &Path) -> (Arc<std::sync::Mutex<PageLog>>, DownloadQueue, tokio::task::JoinHandle<()>) {
        let log = Arc::new(std::sync::Mutex::new(PageLog::default()));
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let watch = crate::log::Watch {
            session: self.session_id.0.clone(),
            main_frame: self.target_id.0.clone(),
            context: self.context_id.0.clone(),
            download_dir: download_dir.to_path_buf(),
        };
        let task = tokio::spawn(crate::log::collect(self.browser.subscribe_unbounded(), watch, log.clone(), tx));
        (log, rx, task)
    }

    /// Lets this page's context download files into `dir`, named by download id.
    pub async fn allow_downloads(&self, dir: &Path, timeout: Duration) -> Result<()> {
        let params = json!({
            "behavior": "allowAndName",
            "browserContextId": self.context_id.0,
            "downloadPath": dir.to_string_lossy(),
            "eventsEnabled": true
        });
        self.browser.call("Browser.setDownloadBehavior", params, None, timeout).await?;
        Ok(())
    }

    /// PNG of the first match, once it is visible and still. Elements in iframes, below the
    /// fold, or taller than the viewport work too.
    pub async fn element_screenshot(&self, sel: &Selector, timeout: Duration) -> Result<Vec<u8>> {
        let (b, _) = self.wait_element(sel, Wait::Box, timeout).await?;
        // The box is in viewport coordinates; the clip is in document coordinates.
        let m = self.call("Page.getLayoutMetrics", json!({}), timeout).await?;
        let vp = if m["cssLayoutViewport"].is_object() { &m["cssLayoutViewport"] } else { &m["layoutViewport"] };
        let f = |v: &Value, k: &str| v[k].as_f64().unwrap_or(0.0);
        let res = self
            .call(
                "Page.captureScreenshot",
                json!({
                    "format": "png",
                    "captureBeyondViewport": true,
                    "clip": {
                        "x": f(&b, "x") + f(vp, "pageX"),
                        "y": f(&b, "y") + f(vp, "pageY"),
                        "width": f(&b, "width"),
                        "height": f(&b, "height"),
                        "scale": 1
                    }
                }),
                timeout,
            )
            .await?;
        let b64 = res["data"].as_str().ok_or_else(|| Error::Other("screenshot: no data".into()))?;
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.decode(b64).map_err(|e| Error::Other(e.to_string()))
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
        let mut last_box: Option<[f64; 4]> = None;
        let mut last_reason = String::from("never checked");
        let mode = match wait {
            Wait::Attached => "attached",
            Wait::Visible => "visible",
            Wait::Click => "click",
            Wait::Box => "box",
            Wait::Rect => "rect",
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
            let scroll = matches!(wait, Wait::Click | Wait::Box);
            let (scope, hops) = match self.scope(&sel.frames, scroll, timeout).await? {
                Ok(found) => found,
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
                if wait == Wait::Box {
                    let f = |k: &str| v[k].as_f64().unwrap_or(0.0);
                    let rect = [f("x") + scope.dx, f("y") + scope.dy, f("width"), f("height")];
                    if last_box.is_some_and(|b| b.iter().zip(rect).all(|(a, b)| (a - b).abs() < 1.0)) {
                        return Ok((json!({ "x": rect[0], "y": rect[1], "width": rect[2], "height": rect[3] }), scope));
                    }
                    last_box = Some(rect);
                    last_reason = "still moving".into();
                    sleep(Duration::from_millis(50)).await;
                    continue;
                }
                if wait != Wait::Click {
                    return Ok((v, scope));
                }
                let x = v["x"].as_f64().unwrap_or(0.0) + scope.dx;
                let y = v["y"].as_f64().unwrap_or(0.0) + scope.dy;
                let stable = last_pos.is_some_and(|(px, py)| (px - x).abs() < 1.0 && (py - y).abs() < 1.0);
                last_pos = Some((x, y));
                if !stable {
                    last_reason = "still moving".into();
                } else {
                    match self.covered(&hops, x, y, timeout).await {
                        None => return Ok((json!({ "x": x, "y": y }), scope)),
                        Some(reason) => last_reason = reason,
                    }
                }
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
        // Both steps act on the clicked element (see query.js); any failure is an error, never
        // text typed into whatever else has focus.
        let step = |mode: &'static str| {
            let scope = &scope;
            async move {
                let r = self.eval_in(scope, &engine_call(&sel.query, mode), true, timeout).await?;
                match r["value"]["ok"].as_bool() {
                    Some(true) => Ok(()),
                    _ => Err(Error::NotActionable {
                        selector: sel.to_string(),
                        reason: r["value"]["reason"].as_str().unwrap_or("fill failed").to_string(),
                    }),
                }
            }
        };
        step("clear").await?;
        // Sent to the frame's own session: a cross-site iframe has its own input handler.
        self.session_call(&scope.session, "Input.insertText", json!({ "text": value }), timeout)
            .await?;
        step("changed").await
    }

    /// Blocks requests whose URL matches any pattern, in this page and its iframes.
    pub async fn set_blocked_urls(&self, urls: &[String], timeout: Duration) -> Result<()> {
        self.setups.lock().await.entry(self.context_id.0.clone()).or_default().urls = urls.to_vec();
        self.call("Network.enable", json!({}), timeout).await?;
        self.call("Network.setBlockedURLs", json!({ "urls": urls }), timeout).await?;
        Ok(())
    }

    /// Fails requests of these types, in this page and its iframes (cross-origin ones
    /// included). The browser's event loop answers the paused requests; see
    /// `Fetch.requestPaused` in browser.rs.
    pub async fn block_resource_types(&self, types: &[ResourceType], timeout: Duration) -> Result<()> {
        if types.is_empty() {
            return Ok(());
        }
        let fetch = {
            let mut setups = self.setups.lock().await;
            let setup = setups.entry(self.context_id.0.clone()).or_default();
            setup.types = types.to_vec();
            fetch_params(setup)
        };
        if let Some(params) = fetch {
            self.call("Fetch.enable", params, timeout).await?;
        }
        Ok(())
    }
}

/// Per-context setup, applied to each of the context's sessions as they attach (iframes in
/// other processes, popups): resource blocking, proxy credentials, emulation, init scripts.
#[derive(Debug, Clone, Default)]
pub(crate) struct ContextSetup {
    /// Where paused requests go when the job intercepts them (`page.route`).
    pub(crate) routes: Option<tokio::sync::mpsc::UnboundedSender<InterceptedRequest>>,
    pub(crate) emulation: Option<Emulation>,
    pub(crate) init_scripts: Vec<String>,
    pub(crate) types: Vec<ResourceType>,
    pub(crate) urls: Vec<String>,
    pub(crate) proxy_auth: Option<(String, String)>,
    /// Requests already given credentials: a second challenge means they were rejected.
    pub(crate) answered: std::collections::HashSet<String>,
}

/// `Fetch.enable` for a context, or `None` when it needs no Fetch at all. Proxy credentials
/// need every request paused: Chrome only reports auth challenges for paused requests (and
/// rejects an empty pattern list with `handleAuthRequests`). So does interception, which hands
/// each request to the job.
pub(crate) fn fetch_params(setup: &ContextSetup) -> Option<Value> {
    let patterns: Vec<Value> = if setup.proxy_auth.is_some() || setup.routes.is_some() {
        vec![json!({ "urlPattern": "*", "requestStage": "Request" })]
    } else {
        setup
            .types
            .iter()
            .map(|t| json!({ "urlPattern": "*", "resourceType": t.as_str(), "requestStage": "Request" }))
            .collect()
    };
    if patterns.is_empty() {
        return None;
    }
    Some(json!({ "patterns": patterns, "handleAuthRequests": setup.proxy_auth.is_some() }))
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
