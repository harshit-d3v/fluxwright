use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::sync::Mutex;
use tokio::time::sleep;

use crate::browser::{
    install_binding_on_sessions, BrowserId, CdpBrowser, ContextId, FrameInfo, FrameTree,
    NetActivity, PageSetup, Proxy, SessionId, TargetId, TargetState,
};
use crate::cdp_session::CdpSession;
use crate::clock::{
    parse_clock_ticks, parse_clock_time, ClockInstallOptions, ClockTicks, ClockTime, CLOCK_JS,
};
use crate::connection::Connection;
use crate::context::{
    filter_cookies, permission_types, Cookie, CookieFilter, Emulation, OriginStorage,
    StorageState, LOCAL_STORAGE_JS, STORAGE_WITH_INDEXED_DB_JS,
};
use crate::input::{
    canonicalize_local_paths, dispatch_key_event, encode_file_payloads, parse_chord, resolve_key,
    CheckOptions, ClickOptions, FileInput, HoverOptions, KeyboardState, MouseButton, MouseState,
    SelectEntry,
};
use crate::log::{DownloadResult, PageLog};

/// Finished downloads, oldest first.
pub type DownloadQueue = tokio::sync::mpsc::UnboundedReceiver<DownloadResult>;
use crate::render::ScreenshotOptions;
use crate::route::InterceptedRequest;
use crate::snapshot::{extract_ax_ref, AriaSnapshot, MarkdownOptions, SnapshotOptions};
use crate::error::{Error, Result};
use crate::worker::{Worker, WorkerEvent, WorkerRegistry};

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

/// How [`CdpPage::wait_for_function`] polls the expression.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WaitPolling {
    #[default]
    Raf,
    Interval(Duration),
}

enum NavStep {
    Continue,
    Done,
}

#[derive(Clone)]
enum GlobTok {
    Any,
    NonSlash,
    Lit(char),
    Group(Vec<Vec<GlobTok>>),
}

fn parse_glob(chars: &[char], stop_at_brace: bool) -> (Vec<GlobTok>, usize) {
    let mut toks = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if stop_at_brace && (c == '}' || c == ',') {
            break;
        }
        if c == '*' {
            if chars.get(i + 1) == Some(&'*') {
                toks.push(GlobTok::Any);
                i += 2;
            } else {
                toks.push(GlobTok::NonSlash);
                i += 1;
            }
        } else if !stop_at_brace && c == '{' {
            let mut alts = Vec::new();
            let mut j = i + 1;
            let mut closed = false;
            while j <= chars.len() {
                let (alt, used) = parse_glob(&chars[j..], true);
                alts.push(alt);
                j += used;
                if j >= chars.len() {
                    break;
                }
                if chars[j] == ',' {
                    j += 1;
                } else if chars[j] == '}' {
                    j += 1;
                    closed = true;
                    break;
                }
            }
            if closed {
                toks.push(GlobTok::Group(alts));
                i = j;
            } else {
                toks.push(GlobTok::Lit('{'));
                i += 1;
            }
        } else {
            toks.push(GlobTok::Lit(c));
            i += 1;
        }
    }
    (toks, i)
}

fn advance_glob(toks: &[GlobTok], chars: &[char], mut states: Vec<bool>) -> Vec<bool> {
    let n = chars.len();
    for tok in toks {
        let mut next = vec![false; n + 1];
        match tok {
            GlobTok::Lit(ch) => {
                for i in 0..n {
                    if states[i] && chars[i] == *ch {
                        next[i + 1] = true;
                    }
                }
            }
            GlobTok::NonSlash => {
                let mut active = false;
                for i in 0..=n {
                    if states[i] {
                        active = true;
                    }
                    if active {
                        next[i] = true;
                    }
                    if i < n && chars[i] == '/' {
                        active = false;
                    }
                }
            }
            GlobTok::Any => {
                let mut active = false;
                for i in 0..=n {
                    if states[i] {
                        active = true;
                    }
                    if active {
                        next[i] = true;
                    }
                }
            }
            GlobTok::Group(alts) => {
                for alt in alts {
                    let res = advance_glob(alt, chars, states.clone());
                    for i in 0..=n {
                        next[i] |= res[i];
                    }
                }
            }
        }
        states = next;
    }
    states
}

/// Playwright-style URL glob match (`**` any chars, `*` non-`/`, `{a,b}` alternatives).
pub fn url_glob_matches(pattern: &str, url: &str) -> bool {
    let pat: Vec<char> = pattern.chars().collect();
    let chars: Vec<char> = url.chars().collect();
    let (toks, _) = parse_glob(&pat, false);
    let mut states = vec![false; chars.len() + 1];
    states[0] = true;
    advance_glob(&toks, &chars, states)[chars.len()]
}

fn is_js_truthy(obj: &Value) -> bool {
    match obj["type"].as_str() {
        Some("undefined") => false,
        Some("boolean") => obj["value"].as_bool().unwrap_or(false),
        Some("string") => obj["value"].as_str().is_some_and(|s| !s.is_empty()),
        Some("number") => {
            if let Some(un) = obj["unserializableValue"].as_str() {
                un != "NaN" && un != "-0"
            } else {
                obj["value"].as_f64().is_some_and(|n| n != 0.0 && !n.is_nan())
            }
        }
        Some("bigint") => obj["unserializableValue"].as_str() != Some("0n"),
        Some("object") => obj["subtype"].as_str() != Some("null"),
        _ => true,
    }
}

/// Selector engine plus element checks, run in the page or an iframe on every poll.
const QUERY_JS: &str = include_str!("query.js");

/// Compact DOM-to-Markdown serializer.
const MARKDOWN_JS: &str = include_str!("markdown.js");

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
    /// Reject matching more than one element (Playwright's strict mode).
    pub strict: bool,
}

/// Filter options for [`Selector::role_with`] / `get_by_role_with`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RoleOptions {
    pub name: Option<String>,
    pub exact: bool,
    /// Pattern and JS RegExp flags for regex `name` matching (takes precedence over `name`).
    pub name_regex: Option<(String, String)>,
    pub checked: Option<bool>,
    pub disabled: Option<bool>,
    pub expanded: Option<bool>,
    pub include_hidden: bool,
    pub level: Option<u32>,
    pub pressed: Option<bool>,
    pub selected: Option<bool>,
}

impl Selector {
    pub fn new(query: impl Into<String>) -> Self {
        Self { frames: Vec::new(), query: query.into(), strict: false }
    }

    pub fn strict(mut self, strict: bool) -> Self {
        self.strict = strict;
        self
    }

    /// `engine="text"` (exact) or `engine="text"i` (case-insensitive substring). Quoted either
    /// way, so a `>>` inside the text does not split the selector.
    fn matching(engine: &str, text: &str, exact: bool) -> String {
        format!("{engine}={}{}", serde_json::to_string(text).unwrap(), if exact { "" } else { "i" })
    }

    /// Serializes a JS RegExp as `re:"pattern","flags"` so `>>` inside it never splits the selector.
    pub fn regex(pattern: &str, flags: &str) -> String {
        format!(
            "re:{},{}",
            serde_json::to_string(pattern).unwrap(),
            serde_json::to_string(flags).unwrap()
        )
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

    /// The `alt=` query behind `get_by_alt_text`: `<img>`, `<area>` or `<input type="image">`.
    pub fn alt_text(text: &str, exact: bool) -> String {
        Self::matching("alt", text, exact)
    }

    /// The `title=` query behind `get_by_title`.
    pub fn title(text: &str, exact: bool) -> String {
        Self::matching("title", text, exact)
    }

    /// The `testid=` query behind `get_by_test_id`: `data-testid`, exact.
    pub fn test_id(id: &str) -> String {
        format!("testid={}", serde_json::to_string(id).unwrap())
    }

    /// The `has-text=` part behind `filter({ hasText })`: case-insensitive substring.
    pub fn has_text(text: &str) -> String {
        Self::matching("has-text", text, false)
    }

    pub fn has_text_regex(pattern: &str, flags: &str) -> String {
        format!("has-text={}", Self::regex(pattern, flags))
    }

    pub fn has_not_text(text: &str) -> String {
        Self::matching("has-not-text", text, false)
    }

    pub fn has_not_text_regex(pattern: &str, flags: &str) -> String {
        format!("has-not-text={}", Self::regex(pattern, flags))
    }

    /// This selector narrowed by `part`: another selector searched inside each match,
    /// `nth=N`, `has-text="..."i`, or a combinator part.
    pub fn then(&self, part: &str) -> Selector {
        Selector {
            frames: self.frames.clone(),
            query: format!("{} >> {part}", self.query),
            strict: self.strict,
        }
    }

    fn combine(&self, op: &str, other: &Selector) -> Selector {
        if self.frames != other.frames {
            return Selector {
                frames: self.frames.clone(),
                query: "role=[frame-mismatch]".to_string(),
                strict: self.strict,
            };
        }
        self.then(&format!("{op}={}", serde_json::to_string(&other.query).unwrap()))
    }

    pub fn has(&self, inner: &Selector) -> Selector {
        self.combine("has", inner)
    }

    pub fn has_not(&self, inner: &Selector) -> Selector {
        self.combine("has-not", inner)
    }

    pub fn and(&self, other: &Selector) -> Selector {
        self.combine("and", other)
    }

    pub fn or(&self, other: &Selector) -> Selector {
        self.combine("or", other)
    }

    /// The `role=` query behind `get_by_role_with`.
    pub fn role_with(role: &str, opts: &RoleOptions) -> String {
        let mut q = format!("role={role}");
        if let Some((ref pat, ref flags)) = opts.name_regex {
            q.push_str(&format!("[name={}]", Self::regex(pat, flags)));
        } else if let Some(ref n) = opts.name {
            q.push_str(&format!(
                "[name={}{}]",
                serde_json::to_string(n).unwrap(),
                if opts.exact { "s" } else { "" }
            ));
        }
        for (k, v) in [
            ("checked", opts.checked),
            ("disabled", opts.disabled),
            ("expanded", opts.expanded),
            ("pressed", opts.pressed),
            ("selected", opts.selected),
        ] {
            if let Some(b) = v {
                q.push_str(&format!("[{k}={b}]"));
            }
        }
        if opts.include_hidden {
            q.push_str("[include-hidden=true]");
        }
        if let Some(level) = opts.level {
            q.push_str(&format!("[level={level}]"));
        }
        q
    }

    /// The `role=` query behind `get_by_role`.
    pub fn role(role: &str, name: Option<&str>, exact: bool) -> String {
        Self::role_with(
            role,
            &RoleOptions { name: name.map(String::from), exact, ..Default::default() },
        )
    }
}

impl From<&str> for Selector {
    fn from(query: &str) -> Self {
        Self::new(query)
    }
}

impl From<String> for Selector {
    fn from(query: String) -> Self {
        Self::new(query)
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

/// Sentinel prefix in `Selector.frames[0]` identifying a target frame by CDP frame ID.
pub const FRAME_ID_PREFIX: &str = "\0fw-frame:";

impl Selector {
    pub fn frame_id_hop(id: &str) -> String {
        format!("{FRAME_ID_PREFIX}{id}")
    }
}

impl fmt::Display for Selector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for frame in &self.frames {
            if let Some(id) = frame.strip_prefix(FRAME_ID_PREFIX) {
                write!(f, "frame({id}) >> ")?;
            } else {
                write!(f, "{frame} >> ")?;
            }
        }
        f.write_str(&self.query)
    }
}

/// What `wait_for_selector_state` waits for: Playwright's `state` option.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WaitState {
    /// In the DOM, visible or not.
    Attached,
    /// Not in the DOM.
    Detached,
    /// In the DOM with a non-empty box and not `visibility: hidden`.
    #[default]
    Visible,
    /// Not visible, or not in the DOM.
    Hidden,
}

impl std::str::FromStr for WaitState {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        Ok(match s {
            "attached" => Self::Attached,
            "detached" => Self::Detached,
            "visible" => Self::Visible,
            "hidden" => Self::Hidden,
            _ => return Err(Error::Other(format!("state must be attached, detached, visible or hidden, not {s:?}"))),
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Wait {
    Query { mode: &'static str, scroll: bool },
    Click { force: bool },
    /// Visible, scrolled into view and unmoved across two polls: for element screenshots.
    Box,
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
/// and how that frame's viewport maps onto the top-level one.
#[derive(Clone)]
struct Scope {
    session: String,
    context: Option<i64>,
    /// Frame id of a cached isolated world, so a dead one can be dropped.
    world_of: Option<String>,
    /// This frame's viewport to the top-level viewport.
    map: Affine,
    /// Same for the session's root frame, whose viewport is what the DOM domain reports in.
    root: Affine,
}

/// `x' = a·x + c·y + e`, `y' = b·x + d·y + f`, as in DOMMatrix: enough for iframes that CSS
/// scales, rotates or skews. ponytail: a perspective (3D) transform is not affine; such a frame
/// gets the map through three of its four corners.
#[derive(Clone, Copy)]
struct Affine([f64; 6]);

impl Affine {
    const ID: Self = Self([1.0, 0.0, 0.0, 1.0, 0.0, 0.0]);

    fn apply(&self, x: f64, y: f64) -> (f64, f64) {
        let [a, b, c, d, e, f] = self.0;
        (a * x + c * y + e, b * x + d * y + f)
    }

    fn invert(&self, x: f64, y: f64) -> (f64, f64) {
        let [a, b, c, d, e, f] = self.0;
        let (x, y, det) = (x - e, y - f, a * d - b * c);
        ((d * x - c * y) / det, (a * y - b * x) / det)
    }

    /// The map taking the `w`×`h` rectangle at the origin onto quad `q`, corners clockwise from
    /// the top-left one, as CDP gives quads.
    fn onto(q: [(f64, f64); 4], w: f64, h: f64) -> Self {
        let [(x1, y1), (x2, y2), _, (x4, y4)] = q;
        Self([(x2 - x1) / w, (y2 - y1) / w, (x4 - x1) / h, (y4 - y1) / h, x1, y1])
    }

    /// The same map moved so that this frame's origin lands on `(x, y)` of its own viewport.
    fn shifted(&self, x: f64, y: f64) -> Self {
        let [a, b, c, d, ..] = self.0;
        let (e, f) = self.apply(x, y);
        Self([a, b, c, d, e, f])
    }

    /// Box around the mapped rectangle: x, y, width, height.
    fn rect(&self, x: f64, y: f64, w: f64, h: f64) -> [f64; 4] {
        let pts = [(x, y), (x + w, y), (x, y + h), (x + w, y + h)].map(|(x, y)| self.apply(x, y));
        let min = |v: [f64; 4]| v.into_iter().fold(f64::INFINITY, f64::min);
        let max = |v: [f64; 4]| v.into_iter().fold(f64::NEG_INFINITY, f64::max);
        let (xs, ys) = (pts.map(|p| p.0), pts.map(|p| p.1));
        [min(xs), min(ys), max(xs) - min(xs), max(ys) - min(ys)]
    }
}

fn effective_arg(sel: &Selector, arg: &Value) -> Value {
    if sel.strict {
        let mut obj = arg.as_object().cloned().unwrap_or_default();
        obj.insert("strict".into(), Value::Bool(true));
        Value::Object(obj)
    } else {
        arg.clone()
    }
}

fn engine_call_with(sel: &Selector, mode: &str, arg: &Value) -> String {
    let effective = effective_arg(sel, arg);
    format!(
        "({QUERY_JS})({}, {}, {})",
        serde_json::to_string(&sel.query).unwrap(),
        serde_json::to_string(mode).unwrap(),
        serde_json::to_string(&effective).unwrap(),
    )
}

fn json_texts(v: Option<Value>) -> Vec<String> {
    v.and_then(|v| v["texts"].as_array().cloned())
        .unwrap_or_default()
        .into_iter()
        .map(|s| s.as_str().unwrap_or("").to_string())
        .collect()
}

/// Maps `exceptionDetails` to a JS-style error. `description` carries
/// "TypeError: msg\n    at <anonymous>:line:col"; a thrown non-Error only `text` + `value`.
pub(crate) fn js_error(res: &Value) -> Option<Error> {
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

fn is_destroyed_context_error(err: &Error) -> bool {
    matches!(
        err,
        Error::Command { message, .. }
            if message.contains("Cannot find context")
                || message.contains("Execution context was destroyed")
                || message.contains("Inspected target navigated")
    )
}

fn is_stale_context_error(err: &Error) -> bool {
    is_destroyed_context_error(err)
        || matches!(err, Error::Command { message, .. } if message.contains("-32000"))
}

#[derive(Debug, Clone, Default)]
pub struct ScriptTagOptions {
    pub url: Option<String>,
    pub content: Option<String>,
    pub script_type: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct StyleTagOptions {
    pub url: Option<String>,
    pub content: Option<String>,
}

/// An iframe element's content box before transforms: width and height (its frame's viewport),
/// then its origin in the parent's viewport, which is right only when nothing transforms it.
const FRAME_BOX_JS: &str = "function() { const r = this.getBoundingClientRect(), s = getComputedStyle(this), \
    px = (k) => parseFloat(s[k]); \
    return [this.clientWidth - px('paddingLeft') - px('paddingRight'), this.clientHeight - px('paddingTop') - px('paddingBottom'), \
        r.left + this.clientLeft + px('paddingLeft'), r.top + this.clientTop + px('paddingTop')]; }";

#[derive(Clone)]
pub struct CdpPage {
    pub browser: Arc<Connection>,
    pub browser_pid: u32,
    pub browser_id: BrowserId,
    pub headless: bool,
    pub is_remote: bool,
    pub context_id: ContextId,
    pub target_id: TargetId,
    pub opener_id: Option<TargetId>,
    pub session_id: SessionId,
    pub(crate) targets: Arc<Mutex<HashMap<String, TargetState>>>,
    /// Isolated worlds made for frames on this page, by frame id.
    pub(crate) worlds: Arc<std::sync::Mutex<HashMap<String, i64>>>,
    pub(crate) setups: Arc<Mutex<HashMap<String, ContextSetup>>>,
    pub(crate) page_setups: Arc<Mutex<HashMap<String, PageSetup>>>,
    pub(crate) frames: Arc<std::sync::Mutex<FrameTree>>,
    pub(crate) workers: Arc<std::sync::Mutex<WorkerRegistry>>,
    pub(crate) closed: Arc<AtomicBool>,
    pub(crate) mouse: Arc<std::sync::Mutex<MouseState>>,
    pub(crate) keyboard: Arc<std::sync::Mutex<KeyboardState>>,
    pub(crate) url: Arc<tokio::sync::watch::Sender<String>>,
    pub(crate) net: Arc<std::sync::Mutex<NetActivity>>,
    /// Maps the latest accessibility snapshot's short ref (`"e1"`, `"e2"`, ...) to `backendDOMNodeId`.
    pub(crate) ax_refs: Arc<std::sync::Mutex<HashMap<String, i64>>>,
    /// Cached formatted tree from the most recent `aria_snapshot` call for `diff: true`.
    pub(crate) last_snapshot: Arc<std::sync::Mutex<Option<String>>>,
    /// Pre-subscribed session-routed CDP event stream for popups so early console/network events aren't missed.
    #[allow(clippy::type_complexity)]
    pub(crate) early_log_events: Arc<
        std::sync::Mutex<Option<crate::connection::SessionSubscription>>,
    >,
}

impl CdpPage {
    fn sid(&self) -> &str {
        &self.session_id.0
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst) || self.browser.is_dead()
    }

    pub fn target_id(&self) -> &str {
        &self.target_id.0
    }

    pub fn opener_id(&self) -> Option<&str> {
        self.opener_id.as_ref().map(|t| t.0.as_str())
    }

    pub fn main_frame_id(&self) -> &str {
        &self.target_id.0
    }

    pub fn frames(&self) -> Vec<FrameInfo> {
        self.frames.lock().unwrap().list()
    }

    pub fn workers(&self) -> Vec<Worker> {
        self.workers.lock().unwrap().list()
    }

    pub fn worker(&self, target_id: &str) -> Option<Worker> {
        self.workers.lock().unwrap().get(target_id)
    }

    pub fn subscribe_workers(&self) -> tokio::sync::mpsc::UnboundedReceiver<WorkerEvent> {
        self.workers.lock().unwrap().subscribe()
    }

    pub async fn bring_to_front(&self, timeout: Duration) -> Result<()> {
        self.call("Page.bringToFront", json!({}), timeout).await?;
        Ok(())
    }

    /// Fires for each popup window opened from this page (`window.open` / `target=_blank`).
    pub async fn popups(&self) -> tokio::sync::mpsc::UnboundedReceiver<CdpPage> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        self.page_setups
            .lock()
            .await
            .entry(self.target_id.0.clone())
            .or_default()
            .popups = Some(tx);
        rx
    }

    async fn attach_cdp_session(&self, target_id: &str, timeout: Duration) -> Result<CdpSession> {
        if self.browser.is_dead() {
            return Err(Error::BrowserDead {
                pid: self.browser_pid,
            });
        }
        if self.is_closed() {
            return Err(Error::Other("page is closed".into()));
        }
        let res = self
            .browser
            .call(
                "Target.attachToTarget",
                json!({ "targetId": target_id, "flatten": true }),
                None,
                timeout,
            )
            .await?;
        let sid = res["sessionId"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| Error::Other("Target.attachToTarget returned no sessionId".into()))?;
        Ok(CdpSession::new(
            self.browser.clone(),
            self.browser_pid,
            SessionId(sid.to_owned()),
            TargetId(target_id.to_owned()),
        ))
    }

    pub async fn new_cdp_session(&self, timeout: Duration) -> Result<CdpSession> {
        self.attach_cdp_session(&self.target_id.0, timeout).await
    }

    pub async fn new_frame_cdp_session(&self, frame_id: &str, timeout: Duration) -> Result<CdpSession> {
        if frame_id == self.target_id.0
            || CdpBrowser::session_for_target(&self.targets, frame_id)
                .await
                .is_some()
        {
            self.attach_cdp_session(frame_id, timeout).await
        } else {
            Err(Error::Other(
                "This frame does not have a separate CDP session, it is a part of the parent frame's session".into(),
            ))
        }
    }

    pub fn url(&self) -> String {
        self.url.borrow().clone()
    }

    pub fn subscribe_url(&self) -> tokio::sync::watch::Receiver<String> {
        self.url.subscribe()
    }

    fn set_url(&self, url: &str) {
        if !url.is_empty() {
            self.url.send_if_modified(|cur| {
                if cur.as_str() != url {
                    *cur = url.to_owned();
                    true
                } else {
                    false
                }
            });
        }
    }

    fn drain_url_events(
        &self,
        events: &mut tokio::sync::mpsc::UnboundedReceiver<Arc<crate::connection::CdpEvent>>,
    ) -> bool {
        let mut saw_url = false;
        while let Ok(ev) = events.try_recv() {
            if ev.session_id.as_deref() == Some(self.sid()) {
                match ev.method.as_str() {
                    "Page.frameNavigated" if ev.params["frame"]["id"] == self.target_id.0.as_str() => {
                        self.ax_refs.lock().unwrap().clear();
                        self.worlds.lock().unwrap().clear();
                        let full = crate::browser::frame_url(&ev.params["frame"]);
                        if !full.is_empty() {
                            self.set_url(&full);
                            saw_url = true;
                        }
                    }
                    "Page.navigatedWithinDocument" if ev.params["frameId"] == self.target_id.0.as_str() => {
                        let u = ev.params["url"].as_str().unwrap_or("");
                        if !u.is_empty() {
                            self.set_url(u);
                            saw_url = true;
                        }
                    }
                    _ => {}
                }
            }
        }
        saw_url
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
        self.ax_refs.lock().unwrap().clear();
        self.worlds.lock().unwrap().clear();
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
        let (Some(_frame), Some(loader)) = (res["frameId"].as_str(), res["loaderId"].as_str()) else {
            if !self.drain_url_events(&mut events) {
                self.set_url(url);
            }
            return Ok(());
        };
        if wait_until == WaitUntil::Commit {
            if !self.drain_url_events(&mut events) {
                self.set_url(url);
            }
            return Ok(());
        }
        let waited_url = self
            .wait_for_navigation(
                events,
                wait_until,
                Some(loader),
                false,
                deadline,
                timeout,
                &format!("navigation to {url}"),
            )
            .await?;
        if !waited_url {
            self.set_url(url);
        }
        Ok(())
    }

    pub async fn reload(&self, wait_until: WaitUntil, timeout: Duration) -> Result<()> {
        self.ax_refs.lock().unwrap().clear();
        self.worlds.lock().unwrap().clear();
        let deadline = tokio::time::Instant::now() + timeout;
        let events = self.browser.subscribe_unbounded();
        self.call("Page.reload", json!({}), timeout).await?;
        self.wait_for_navigation(events, wait_until, None, false, deadline, timeout, "reload")
            .await?;
        Ok(())
    }

    pub async fn go_back(&self, wait_until: WaitUntil, timeout: Duration) -> Result<bool> {
        self.navigate_history(-1, wait_until, timeout).await
    }

    pub async fn go_forward(&self, wait_until: WaitUntil, timeout: Duration) -> Result<bool> {
        self.navigate_history(1, wait_until, timeout).await
    }

    async fn navigate_history(&self, delta: isize, wait_until: WaitUntil, timeout: Duration) -> Result<bool> {
        self.ax_refs.lock().unwrap().clear();
        self.worlds.lock().unwrap().clear();
        let deadline = tokio::time::Instant::now() + timeout;
        let hist = self.call("Page.getNavigationHistory", json!({}), timeout).await?;
        let current = hist["currentIndex"].as_i64().unwrap_or(0) as isize;
        let target_idx = current + delta;
        let Some(entries) = hist["entries"].as_array() else {
            return Ok(false);
        };
        if target_idx < 0 || (target_idx as usize) >= entries.len() {
            return Ok(false);
        }
        let entry = &entries[target_idx as usize];
        let Some(entry_id) = entry["id"].as_i64() else {
            return Ok(false);
        };
        let entry_url = entry["url"].as_str().unwrap_or("");
        let events = self.browser.subscribe_unbounded();
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(Error::timeout(
                if delta < 0 { "go_back" } else { "go_forward" },
                timeout.as_millis() as u64,
            ));
        }
        self.call("Page.navigateToHistoryEntry", json!({ "entryId": entry_id }), remaining)
            .await?;
        let what = if delta < 0 { "go_back" } else { "go_forward" };
        let saw_url = self
            .wait_for_navigation(events, wait_until, None, true, deadline, timeout, what)
            .await?;
        if !saw_url && !entry_url.is_empty() {
            self.set_url(entry_url);
        }
        Ok(true)
    }

    pub async fn set_content(&self, html: &str, wait_until: WaitUntil, timeout: Duration) -> Result<()> {
        self.ax_refs.lock().unwrap().clear();
        self.worlds.lock().unwrap().clear();
        let deadline = tokio::time::Instant::now() + timeout;
        {
            let mut net = self.net.lock().unwrap();
            net.idle_confirmed = false;
            net.last_change = std::time::Instant::now();
        }
        let wait_str = match wait_until {
            WaitUntil::Commit => "commit",
            WaitUntil::DomContentLoaded => "domcontentloaded",
            WaitUntil::Load | WaitUntil::NetworkIdle => "load",
        };
        let check_doc = self.evaluate("typeof document.open === 'function'", timeout).await;
        if check_doc.ok().and_then(|v| v.as_bool()) == Some(true) {
            let script = format!(
                r#"((html, waitUntil) => {{
                    window.stop();
                    document.open();
                    let resolved = false;
                    const p = waitUntil === 'commit'
                        ? Promise.resolve()
                        : new Promise((resolve) => {{
                            const done = () => {{ resolved = true; resolve(); }};
                            if (waitUntil === 'domcontentloaded') {{
                                document.addEventListener('DOMContentLoaded', done, {{ once: true }});
                            }} else {{
                                window.addEventListener('load', done, {{ once: true }});
                            }}
                        }});
                    document.write(html);
                    document.close();
                    if (waitUntil === 'commit') return;
                    if (waitUntil === 'domcontentloaded' && (document.readyState === 'interactive' || document.readyState === 'complete')) return;
                    if (waitUntil !== 'domcontentloaded' && document.readyState === 'complete') return;
                    if (resolved) return;
                    return p;
                }})({}, {})"#,
                serde_json::to_string(html).unwrap(),
                serde_json::to_string(wait_str).unwrap(),
            );
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            self.eval_in(&self.main_scope(), &script, true, remaining).await?;
        } else {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            self.call(
                "Page.setDocumentContent",
                json!({ "frameId": self.target_id.0, "html": html }),
                remaining,
            )
            .await?;
            if wait_until != WaitUntil::Commit {
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                self.wait_for_load_state(WaitUntil::Load, remaining).await?;
            }
        }
        if wait_until == WaitUntil::NetworkIdle {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            self.wait_for_load_state(WaitUntil::NetworkIdle, remaining).await?;
        }
        self.worlds.lock().unwrap().clear();
        Ok(())
    }

    pub async fn wait_for_load_state(&self, state: WaitUntil, timeout: Duration) -> Result<()> {
        let deadline = tokio::time::Instant::now() + timeout;
        match state {
            WaitUntil::Commit => Ok(()),
            WaitUntil::DomContentLoaded | WaitUntil::Load => {
                self.wait_for_document_state(state, deadline, timeout).await
            }
            WaitUntil::NetworkIdle => {
                self.wait_for_document_state(WaitUntil::Load, deadline, timeout).await?;
                let mut events = self.browser.subscribe_unbounded();
                let (mut inflight, idle_confirmed, elapsed) = {
                    let net = self.net.lock().unwrap();
                    (net.inflight.clone(), net.idle_confirmed, net.last_change.elapsed())
                };
                let quiet = Duration::from_millis(500);
                if idle_confirmed || (inflight.is_empty() && elapsed >= quiet) {
                    return Ok(());
                }
                let mut idle_until = if inflight.is_empty() {
                    Some(tokio::time::Instant::now() + quiet.saturating_sub(elapsed))
                } else {
                    None
                };
                loop {
                    tokio::select! {
                        _ = async {
                            match idle_until {
                                Some(t) => tokio::time::sleep_until(t).await,
                                None => std::future::pending::<()>().await,
                            }
                        } => {
                            return Ok(());
                        }
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
                                "Inspector.targetCrashed" => {
                                    return Err(Error::TargetCrashed { status: "during wait_for_load_state".into() });
                                }
                                "Page.lifecycleEvent"
                                    if ev.params["name"] == "networkIdle"
                                        && ev.params["frameId"] == self.target_id.0.as_str() =>
                                {
                                    return Ok(());
                                }
                                "Network.requestWillBeSent" => {
                                    if let Some(req_id) = ev.params["requestId"].as_str() {
                                        if !ev.params["request"]["url"].as_str().unwrap_or("").starts_with("data:") {
                                            inflight.insert(req_id.to_owned());
                                            idle_until = None;
                                        }
                                    }
                                }
                                "Network.loadingFinished" | "Network.loadingFailed" => {
                                    if let Some(req_id) = ev.params["requestId"].as_str() {
                                        inflight.remove(req_id);
                                        if inflight.is_empty() {
                                            idle_until = Some(tokio::time::Instant::now() + quiet);
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }
                        _ = tokio::time::sleep_until(deadline) => {
                            return Err(Error::timeout("wait_for_load_state (NetworkIdle)", timeout.as_millis() as u64));
                        }
                    }
                }
            }
        }
    }

    async fn wait_for_document_state(
        &self,
        state: WaitUntil,
        deadline: tokio::time::Instant,
        timeout: Duration,
    ) -> Result<()> {
        let (events_needed, ready_states): (&[&str], &[&str]) = match state {
            WaitUntil::DomContentLoaded => (&["DOMContentLoaded", "load"], &["interactive", "complete"]),
            WaitUntil::Load => (&["load"], &["complete"]),
            WaitUntil::Commit | WaitUntil::NetworkIdle => return Ok(()),
        };
        let mut events = self.browser.subscribe_unbounded();
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if let Ok(v) = self.evaluate("document.readyState", remaining.min(Duration::from_secs(2))).await {
            if ready_states.contains(&v.as_str().unwrap_or("")) {
                return Ok(());
            }
        }
        let mut tick = tokio::time::interval_at(
            tokio::time::Instant::now() + Duration::from_millis(250),
            Duration::from_millis(250),
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
                            if ev.params["frameId"] == self.target_id.0.as_str()
                                && ev.params["name"].as_str().is_some_and(|n| events_needed.contains(&n)) =>
                        {
                            return Ok(());
                        }
                        "Inspector.targetCrashed" => {
                            return Err(Error::TargetCrashed { status: "during wait_for_load_state".into() });
                        }
                        _ => {}
                    }
                }
                _ = tick.tick() => {
                    if self.browser.is_dead() {
                        return Err(Error::BrowserDead { pid: self.browser_pid });
                    }
                    if let Ok(v) = self.evaluate("document.readyState", Duration::from_secs(2)).await {
                        if ready_states.contains(&v.as_str().unwrap_or("")) {
                            return Ok(());
                        }
                    }
                }
                _ = tokio::time::sleep_until(deadline) => {
                    return Err(Error::timeout(format!("wait_for_load_state ({state:?})"), timeout.as_millis() as u64));
                }
            }
        }
    }

    pub async fn wait_for_url<F>(&self, mut predicate: F, wait_until: WaitUntil, timeout: Duration) -> Result<()>
    where
        F: FnMut(&str) -> bool,
    {
        let deadline = tokio::time::Instant::now() + timeout;
        let mut rx = self.subscribe_url();
        let mut events = self.browser.subscribe_unbounded();
        loop {
            let current = rx.borrow_and_update().clone();
            if predicate(&current) {
                if wait_until == WaitUntil::Commit {
                    return Ok(());
                }
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                return self.wait_for_load_state(wait_until, remaining).await;
            }
            tokio::select! {
                res = rx.changed() => {
                    if res.is_err() {
                        return Err(Error::BrowserDead { pid: self.browser_pid });
                    }
                }
                ev = events.recv() => {
                    let Some(ev) = ev else {
                        return Err(Error::BrowserDead { pid: self.browser_pid });
                    };
                    if ev.method == "Fluxwright.disconnected" {
                        return Err(Error::BrowserDead { pid: self.browser_pid });
                    }
                    if ev.session_id.as_deref() == Some(self.sid()) && ev.method == "Inspector.targetCrashed" {
                        return Err(Error::TargetCrashed { status: "during wait_for_url".into() });
                    }
                }
                _ = tokio::time::sleep_until(deadline) => {
                    return Err(Error::timeout(format!("URL matching (current: {current})"), timeout.as_millis() as u64));
                }
            }
        }
    }

    pub async fn wait_for_url_change(&self, after_url: &str, timeout: Duration) -> Result<String> {
        let deadline = tokio::time::Instant::now() + timeout;
        let mut rx = self.subscribe_url();
        let mut events = self.browser.subscribe_unbounded();
        loop {
            let current = rx.borrow_and_update().clone();
            if current != after_url {
                return Ok(current);
            }
            tokio::select! {
                res = rx.changed() => {
                    if res.is_err() {
                        return Err(Error::BrowserDead { pid: self.browser_pid });
                    }
                }
                ev = events.recv() => {
                    let Some(ev) = ev else {
                        return Err(Error::BrowserDead { pid: self.browser_pid });
                    };
                    if ev.method == "Fluxwright.disconnected" {
                        return Err(Error::BrowserDead { pid: self.browser_pid });
                    }
                    if ev.session_id.as_deref() == Some(self.sid()) && ev.method == "Inspector.targetCrashed" {
                        return Err(Error::TargetCrashed { status: "during wait_for_url".into() });
                    }
                }
                _ = tokio::time::sleep_until(deadline) => {
                    return Err(Error::timeout(format!("URL change from {after_url}"), timeout.as_millis() as u64));
                }
            }
        }
    }

    pub async fn wait_for_function(
        &self,
        expression: &str,
        polling: WaitPolling,
        timeout: Duration,
    ) -> Result<Value> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(Error::timeout("wait_for_function", timeout.as_millis() as u64));
            }
            match self.eval_in(&self.main_scope(), expression, false, remaining).await {
                Err(Error::Command { ref message, .. })
                    if message.contains("Execution context was destroyed")
                        || message.contains("Cannot find context") =>
                {
                    let rem = deadline.saturating_duration_since(tokio::time::Instant::now());
                    if rem.is_zero() {
                        return Err(Error::timeout("wait_for_function", timeout.as_millis() as u64));
                    }
                    sleep(Duration::from_millis(50).min(rem)).await;
                    continue;
                }
                Err(e) => return Err(e),
                Ok(obj) => {
                    if is_js_truthy(&obj) {
                        if let Some(v) = obj.get("value") {
                            return Ok(v.clone());
                        }
                        if let Some(oid) = obj["objectId"].as_str() {
                            let rem = deadline.saturating_duration_since(tokio::time::Instant::now());
                            let out = if rem.is_zero() {
                                Value::Bool(true)
                            } else {
                                match self
                                    .call(
                                        "Runtime.callFunctionOn",
                                        json!({
                                            "objectId": oid,
                                            "functionDeclaration": "function() { return this; }",
                                            "returnByValue": true
                                        }),
                                        rem.min(Duration::from_secs(5)),
                                    )
                                    .await
                                {
                                    Ok(res) => res["result"].get("value").cloned().unwrap_or(Value::Bool(true)),
                                    Err(_) => Value::Bool(true),
                                }
                            };
                            let _ = self
                                .call("Runtime.releaseObject", json!({ "objectId": oid }), Duration::from_secs(2))
                                .await;
                            return Ok(out);
                        }
                        return Ok(Value::Bool(true));
                    }
                    if let Some(oid) = obj["objectId"].as_str() {
                        let _ = self
                            .call("Runtime.releaseObject", json!({ "objectId": oid }), Duration::from_secs(2))
                            .await;
                    }
                    let rem = deadline.saturating_duration_since(tokio::time::Instant::now());
                    if rem.is_zero() {
                        return Err(Error::timeout("wait_for_function", timeout.as_millis() as u64));
                    }
                    match polling {
                        WaitPolling::Raf => {
                            let raf_js = "new Promise(r => { const t = setTimeout(r, 100); requestAnimationFrame(() => { clearTimeout(t); r(); }); })";
                            if let Err(e) = self.eval_in(&self.main_scope(), raf_js, true, rem.min(Duration::from_millis(200))).await {
                                if !matches!(e, Error::Command { .. }) && e.is_retryable() {
                                    return Err(e);
                                }
                                let rem2 = deadline.saturating_duration_since(tokio::time::Instant::now());
                                if rem2.is_zero() {
                                    return Err(Error::timeout("wait_for_function", timeout.as_millis() as u64));
                                }
                                sleep(Duration::from_millis(50).min(rem2)).await;
                            }
                        }
                        WaitPolling::Interval(d) => {
                            sleep(d.min(rem)).await;
                        }
                    }
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn wait_for_navigation(
        &self,
        mut events: tokio::sync::mpsc::UnboundedReceiver<Arc<crate::connection::CdpEvent>>,
        wait_until: WaitUntil,
        expected_loader: Option<&str>,
        allow_same_document: bool,
        deadline: tokio::time::Instant,
        timeout: Duration,
        what: &str,
    ) -> Result<bool> {
        let adopt_loader = expected_loader.is_none();
        let mut loader = expected_loader.map(str::to_owned);
        let mut committed = expected_loader.is_some();
        let mut saw_url = false;
        let mut saw_load = false;
        let mut saw_idle = false;
        let mut inflight: HashSet<String> = HashSet::new();
        let mut idle_until: Option<tokio::time::Instant> = None;
        let quiet = Duration::from_millis(500);

        let handle = |ev: &crate::connection::CdpEvent,
                          loader: &mut Option<String>,
                          committed: &mut bool,
                          saw_url: &mut bool,
                          saw_load: &mut bool,
                          saw_idle: &mut bool,
                          inflight: &mut HashSet<String>,
                          idle_until: &mut Option<tokio::time::Instant>|
         -> Result<NavStep> {
            if ev.method == "Fluxwright.disconnected" {
                return Err(Error::BrowserDead { pid: self.browser_pid });
            }
            if ev.session_id.as_deref() != Some(self.sid()) {
                return Ok(NavStep::Continue);
            }
            match ev.method.as_str() {
                "Inspector.targetCrashed" => {
                    return Err(Error::TargetCrashed { status: format!("during {what}") });
                }
                "Page.frameNavigated" if ev.params["frame"]["id"] == self.target_id.0.as_str() => {
                    self.ax_refs.lock().unwrap().clear();
                    self.worlds.lock().unwrap().clear();
                    let full = crate::browser::frame_url(&ev.params["frame"]);
                    if !full.is_empty() {
                        self.set_url(&full);
                        *saw_url = true;
                    }
                    *committed = true;
                    let bfcache = ev.params["type"].as_str() == Some("BackForwardCacheRestore");
                    if !bfcache {
                        *loader = ev.params["frame"]["loaderId"].as_str().map(str::to_owned);
                        *saw_load = false;
                        *saw_idle = false;
                    }
                    if wait_until == WaitUntil::Commit || bfcache {
                        return Ok(NavStep::Done);
                    }
                }
                "Page.navigatedWithinDocument" if ev.params["frameId"] == self.target_id.0.as_str() => {
                    let u = ev.params["url"].as_str().unwrap_or("");
                    if !u.is_empty() {
                        self.set_url(u);
                        *saw_url = true;
                    }
                    if allow_same_document {
                        return Ok(NavStep::Done);
                    }
                }
                "Page.lifecycleEvent" if ev.params["frameId"] == self.target_id.0.as_str() => {
                    let name = ev.params["name"].as_str().unwrap_or("");
                    let ev_loader = ev.params["loaderId"].as_str();
                    if name == "init" && adopt_loader {
                        if let Some(l) = ev_loader {
                            *loader = Some(l.to_owned());
                        }
                    }
                    let matches = loader.is_some() && ev_loader == loader.as_deref();
                    if matches {
                        if name == "DOMContentLoaded" && wait_until == WaitUntil::DomContentLoaded {
                            return Ok(NavStep::Done);
                        }
                        if name == "load" {
                            if matches!(wait_until, WaitUntil::DomContentLoaded | WaitUntil::Load) {
                                return Ok(NavStep::Done);
                            }
                            if wait_until == WaitUntil::NetworkIdle {
                                *saw_load = true;
                                if *saw_idle {
                                    return Ok(NavStep::Done);
                                }
                                if inflight.is_empty() && idle_until.is_none() {
                                    *idle_until = Some(tokio::time::Instant::now() + quiet);
                                }
                            }
                        }
                        if name == "networkIdle" {
                            *saw_idle = true;
                            if wait_until == WaitUntil::NetworkIdle {
                                return Ok(NavStep::Done);
                            }
                        }
                    }
                }
                "Network.requestWillBeSent" if wait_until == WaitUntil::NetworkIdle => {
                    if let Some(req_id) = ev.params["requestId"].as_str() {
                        if !ev.params["request"]["url"].as_str().unwrap_or("").starts_with("data:") {
                            inflight.insert(req_id.to_owned());
                            *idle_until = None;
                        }
                    }
                }
                "Network.loadingFinished" | "Network.loadingFailed" if wait_until == WaitUntil::NetworkIdle => {
                    if let Some(req_id) = ev.params["requestId"].as_str() {
                        inflight.remove(req_id);
                        if *saw_load && inflight.is_empty() {
                            *idle_until = Some(tokio::time::Instant::now() + quiet);
                        }
                    }
                }
                _ => {}
            }
            Ok(NavStep::Continue)
        };

        while let Ok(ev) = events.try_recv() {
            if matches!(
                handle(
                    &ev,
                    &mut loader,
                    &mut committed,
                    &mut saw_url,
                    &mut saw_load,
                    &mut saw_idle,
                    &mut inflight,
                    &mut idle_until,
                )?,
                NavStep::Done
            ) {
                return Ok(saw_url);
            }
        }
        if wait_until == WaitUntil::Commit && committed {
            return Ok(saw_url);
        }

        let ready_states: &[&str] = match wait_until {
            WaitUntil::Commit => &[],
            WaitUntil::DomContentLoaded => &["interactive", "complete"],
            WaitUntil::Load | WaitUntil::NetworkIdle => &["complete"],
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
                    if matches!(
                        handle(
                            &ev,
                            &mut loader,
                            &mut committed,
                            &mut saw_url,
                            &mut saw_load,
                            &mut saw_idle,
                            &mut inflight,
                            &mut idle_until,
                        )?,
                        NavStep::Done
                    ) {
                        return Ok(saw_url);
                    }
                }
                _ = async {
                    match idle_until {
                        Some(t) => tokio::time::sleep_until(t).await,
                        None => std::future::pending::<()>().await,
                    }
                } => {
                    if wait_until == WaitUntil::NetworkIdle && saw_load && inflight.is_empty() {
                        return Ok(saw_url);
                    }
                }
                // Lifecycle events are switched on in prepare_session, which ignores errors.
                // Check readyState twice a second so a page that missed them still finishes.
                _ = tick.tick() => {
                    if self.browser.is_dead() {
                        return Err(Error::BrowserDead { pid: self.browser_pid });
                    }
                    if committed && !ready_states.is_empty() {
                        if let Ok(v) = self.evaluate("document.readyState", Duration::from_secs(2)).await {
                            if ready_states.contains(&v.as_str().unwrap_or("")) {
                                if wait_until == WaitUntil::NetworkIdle {
                                    saw_load = true;
                                    if saw_idle {
                                        return Ok(saw_url);
                                    }
                                    if inflight.is_empty() && idle_until.is_none() {
                                        idle_until = Some(tokio::time::Instant::now() + quiet);
                                    }
                                } else {
                                    return Ok(saw_url);
                                }
                            }
                        }
                    }
                }
                _ = tokio::time::sleep_until(deadline) => {
                    return Err(Error::timeout(
                        format!("{what} ({wait_until:?})"),
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

    pub async fn aria_snapshot(&self, opts: &SnapshotOptions, timeout: Duration) -> Result<AriaSnapshot> {
        self.capture_aria_snapshot(opts, true, timeout).await
    }

    pub(crate) async fn capture_aria_snapshot(
        &self,
        opts: &SnapshotOptions,
        update_diff_cache: bool,
        timeout: Duration,
    ) -> Result<AriaSnapshot> {
        let res = self.call("Accessibility.getFullAXTree", json!({}), timeout).await?;
        let nodes = res["nodes"].as_array().map(Vec::as_slice).unwrap_or(&[]);
        let mut snap = crate::snapshot::build_aria_snapshot(nodes, opts);
        {
            let mut refs = self.ax_refs.lock().unwrap();
            refs.clear();
            for item in &snap.refs {
                refs.insert(item.id.clone(), item.backend_node_id);
            }
        }
        if update_diff_cache {
            let mut last = self.last_snapshot.lock().unwrap();
            let full_tree = snap.tree.clone();
            if opts.diff {
                if let Some(ref prev_tree) = *last {
                    if prev_tree == &full_tree {
                        snap.tree = "(no changes)".into();
                        snap.is_diff = true;
                        snap.changed = false;
                    } else {
                        snap.tree = crate::snapshot::diff_snapshot_trees(prev_tree, &full_tree);
                        snap.is_diff = true;
                        snap.changed = true;
                        *last = Some(full_tree);
                    }
                    return Ok(snap);
                }
            }
            *last = Some(full_tree);
            snap.is_diff = false;
            snap.changed = true;
        }
        Ok(snap)
    }

    pub async fn markdown(&self, opts: &MarkdownOptions, timeout: Duration) -> Result<String> {
        let opts_json = json!({
            "mainContentOnly": opts.main_content_only,
            "includeLinks": opts.include_links,
            "includeImages": opts.include_images,
        });
        if let Some(ref sel_str) = opts.selector {
            let sel = Selector::new(sel_str.clone());
            let (_, scope) = self
                .wait_element(&sel, Wait::Query { mode: "attached", scroll: false }, timeout)
                .await?;
            let found = self
                .eval_engine(&scope, &sel, "element", &Value::Null, false, timeout)
                .await?;
            let Some(obj_id) = found["objectId"].as_str().filter(|_| found["subtype"] == "node") else {
                return Err(Error::NotActionable {
                    selector: sel.to_string(),
                    reason: "element went away".into(),
                });
            };
            let decl = format!("function(o) {{ return ({MARKDOWN_JS})(o, this); }}");
            let res = self
                .session_call(
                    &scope.session,
                    "Runtime.callFunctionOn",
                    json!({
                        "objectId": obj_id,
                        "functionDeclaration": decl,
                        "arguments": [{ "value": opts_json }],
                        "returnByValue": true,
                        "awaitPromise": false,
                    }),
                    timeout,
                )
                .await;
            let _ = self
                .session_call(
                    &scope.session,
                    "Runtime.releaseObject",
                    json!({ "objectId": obj_id }),
                    timeout,
                )
                .await;
            let res = res?;
            if let Some(err) = js_error(&res) {
                return Err(err);
            }
            return Ok(res["result"]["value"].as_str().unwrap_or("").to_string());
        }
        let expr = format!("({MARKDOWN_JS})({}, null)", serde_json::to_string(&opts_json).unwrap());
        let v = self.evaluate(&expr, timeout).await?;
        Ok(v.as_str().unwrap_or("").to_string())
    }

    pub async fn evaluate(&self, expression: &str, timeout: Duration) -> Result<Value> {
        let res = self.eval_in(&self.main_scope(), expression, true, timeout).await?;
        Ok(res["value"].clone())
    }

    pub async fn frame_evaluate(&self, frame_id: &str, expression: &str, timeout: Duration) -> Result<Value> {
        if frame_id == self.target_id.0 {
            return self.evaluate(expression, timeout).await;
        }
        let deadline = tokio::time::Instant::now() + timeout;
        for attempt in 0..2 {
            let mut entry = self
                .frames
                .lock()
                .unwrap()
                .get(frame_id)
                .cloned()
                .ok_or_else(|| Error::Other("frame was detached".into()))?;
            if entry.session_id == self.sid() && entry.default_context_id.is_none() {
                let wait_cap = deadline
                    .saturating_duration_since(tokio::time::Instant::now())
                    .min(Duration::from_secs(2));
                let wait_until = tokio::time::Instant::now() + wait_cap;
                loop {
                    let cur = self
                        .frames
                        .lock()
                        .unwrap()
                        .get(frame_id)
                        .cloned()
                        .ok_or_else(|| Error::Other("frame was detached".into()))?;
                    if cur.default_context_id.is_some() || cur.session_id != self.sid() {
                        entry = cur;
                        break;
                    }
                    if tokio::time::Instant::now() >= wait_until {
                        entry = cur;
                        break;
                    }
                    sleep(Duration::from_millis(20)).await;
                }
            }
            let rem = deadline.saturating_duration_since(tokio::time::Instant::now());
            if rem.is_zero() {
                return Err(Error::timeout("frame_evaluate", timeout.as_millis() as u64));
            }
            let ctx_opt = if entry.session_id != self.sid() && entry.default_context_id.is_none() {
                None
            } else {
                entry.default_context_id
            };
            let mut params = json!({
                "expression": expression,
                "returnByValue": true,
                "awaitPromise": true,
            });
            if let Some(id) = ctx_opt {
                params["contextId"] = json!(id);
            }
            match self.session_call(&entry.session_id, "Runtime.evaluate", params, rem).await {
                Ok(res) => {
                    return match js_error(&res) {
                        Some(e) => Err(e),
                        None => Ok(res["result"]["value"].clone()),
                    };
                }
                Err(Error::Command { ref message, .. })
                    if attempt == 0
                        && (message.contains("Cannot find context")
                            || message.contains("Execution context was destroyed")) =>
                {
                    sleep(Duration::from_millis(20)).await;
                }
                Err(e) => return Err(e),
            }
        }
        Err(Error::Other("frame execution context not available".into()))
    }

    pub async fn frame_goto(
        &self,
        frame_id: &str,
        url: &str,
        wait_until: WaitUntil,
        timeout: Duration,
    ) -> Result<()> {
        if frame_id == self.target_id.0 {
            return self.goto(url, wait_until, timeout).await;
        }
        self.worlds.lock().unwrap().remove(frame_id);
        let deadline = tokio::time::Instant::now() + timeout;
        let mut events = self.browser.subscribe_unbounded();
        let mut res = None;
        for attempt in 0..2 {
            let entry = self
                .frames
                .lock()
                .unwrap()
                .get(frame_id)
                .cloned()
                .ok_or_else(|| Error::Other("frame was detached".into()))?;
            let rem = deadline.saturating_duration_since(tokio::time::Instant::now());
            if rem.is_zero() {
                return Err(Error::timeout("frame_goto", timeout.as_millis() as u64));
            }
            match self
                .session_call(
                    &entry.session_id,
                    "Page.navigate",
                    json!({ "url": url, "frameId": frame_id }),
                    rem,
                )
                .await
            {
                Ok(v) => {
                    res = Some(v);
                    break;
                }
                Err(Error::Command { ref message, .. })
                    if attempt == 0 && (message.contains("No frame") || message.contains("Cannot find")) =>
                {
                    sleep(Duration::from_millis(20)).await;
                }
                Err(e) => {
                    return Err(match e {
                        Error::Command { method, message } => {
                            Error::Command { method, message: format!("{message}: {url}") }
                        }
                        other => other,
                    });
                }
            }
        }
        let res = res.ok_or_else(|| Error::Other("frame was detached".into()))?;
        if let Some(err) = res["errorText"].as_str().filter(|s| !s.is_empty()) {
            return Err(Error::Command {
                method: "Page.navigate".into(),
                message: format!("{err} at {url}"),
            });
        }
        let Some(loader_val) = res["loaderId"].as_str() else {
            return Ok(());
        };
        let mut loader = Some(loader_val.to_string());
        let mut committed = false;
        let mut saw_idle = false;
        let ready_states: &[&str] = match wait_until {
            WaitUntil::Commit => &[],
            WaitUntil::DomContentLoaded => &["interactive", "complete"],
            WaitUntil::Load | WaitUntil::NetworkIdle => &["complete"],
        };
        let mut tick = tokio::time::interval_at(
            tokio::time::Instant::now() + Duration::from_millis(250),
            Duration::from_millis(250),
        );
        loop {
            while let Ok(ev) = events.try_recv() {
                if ev.method == "Fluxwright.disconnected" {
                    return Err(Error::BrowserDead { pid: self.browser_pid });
                }
                match ev.method.as_str() {
                    "Page.frameNavigated" if ev.params["frame"]["id"] == frame_id => {
                        self.worlds.lock().unwrap().remove(frame_id);
                        committed = true;
                        loader = ev.params["frame"]["loaderId"].as_str().map(str::to_owned).or(loader);
                        if wait_until == WaitUntil::Commit {
                            return Ok(());
                        }
                    }
                    "Page.navigatedWithinDocument" if ev.params["frameId"] == frame_id => {
                        return Ok(());
                    }
                    "Page.frameDetached"
                        if ev.params["frameId"] == frame_id && ev.params["reason"] != "swap" =>
                    {
                        return Err(Error::Other("frame was detached".into()));
                    }
                    "Page.lifecycleEvent" if ev.params["frameId"] == frame_id => {
                        let name = ev.params["name"].as_str().unwrap_or("");
                        let ev_loader = ev.params["loaderId"].as_str();
                        if loader.as_deref() == ev_loader {
                            if name == "DOMContentLoaded" && wait_until == WaitUntil::DomContentLoaded {
                                return Ok(());
                            }
                            if name == "load" {
                                if matches!(wait_until, WaitUntil::DomContentLoaded | WaitUntil::Load) {
                                    return Ok(());
                                }
                                if saw_idle {
                                    return Ok(());
                                }
                            }
                            if name == "networkIdle" {
                                saw_idle = true;
                                if wait_until == WaitUntil::NetworkIdle {
                                    return Ok(());
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            tokio::select! {
                ev = events.recv() => {
                    let Some(ev) = ev else {
                        return Err(Error::BrowserDead { pid: self.browser_pid });
                    };
                    if ev.method == "Fluxwright.disconnected" {
                        return Err(Error::BrowserDead { pid: self.browser_pid });
                    }
                    match ev.method.as_str() {
                        "Page.frameNavigated" if ev.params["frame"]["id"] == frame_id => {
                            self.worlds.lock().unwrap().remove(frame_id);
                            committed = true;
                            loader = ev.params["frame"]["loaderId"].as_str().map(str::to_owned).or(loader);
                            if wait_until == WaitUntil::Commit {
                                return Ok(());
                            }
                        }
                        "Page.navigatedWithinDocument" if ev.params["frameId"] == frame_id => {
                            return Ok(());
                        }
                        "Page.frameDetached"
                            if ev.params["frameId"] == frame_id && ev.params["reason"] != "swap" =>
                        {
                            return Err(Error::Other("frame was detached".into()));
                        }
                        "Page.lifecycleEvent" if ev.params["frameId"] == frame_id => {
                            let name = ev.params["name"].as_str().unwrap_or("");
                            let ev_loader = ev.params["loaderId"].as_str();
                            if loader.as_deref() == ev_loader {
                                if name == "DOMContentLoaded" && wait_until == WaitUntil::DomContentLoaded {
                                    return Ok(());
                                }
                                if name == "load" {
                                    if matches!(wait_until, WaitUntil::DomContentLoaded | WaitUntil::Load) {
                                        return Ok(());
                                    }
                                    if saw_idle {
                                        return Ok(());
                                    }
                                }
                                if name == "networkIdle" {
                                    saw_idle = true;
                                    if wait_until == WaitUntil::NetworkIdle {
                                        return Ok(());
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
                _ = tick.tick() => {
                    if self.frames.lock().unwrap().get(frame_id).is_none() {
                        return Err(Error::Other("frame was detached".into()));
                    }
                    if committed && !ready_states.is_empty() && wait_until != WaitUntil::NetworkIdle {
                        if let Ok(v) = self.frame_evaluate(frame_id, "document.readyState", Duration::from_secs(2)).await {
                            if ready_states.contains(&v.as_str().unwrap_or("")) {
                                return Ok(());
                            }
                        }
                    }
                }
                _ = tokio::time::sleep_until(deadline) => {
                    return Err(Error::timeout(
                        format!("frame navigation to {url} ({wait_until:?})"),
                        timeout.as_millis() as u64,
                    ));
                }
            }
        }
    }

    fn main_scope(&self) -> Scope {
        Scope { session: self.sid().into(), context: None, world_of: None, map: Affine::ID, root: Affine::ID }
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

    /// Evaluates the selector engine, resolving leading `@eN` / `ref=eN` references through the
    /// latest accessibility snapshot's `backendDOMNodeId` map before delegating to `QUERY_JS`.
    async fn eval_engine(
        &self,
        scope: &Scope,
        sel: &Selector,
        mode: &str,
        arg: &Value,
        return_by_value: bool,
        timeout: Duration,
    ) -> Result<Value> {
        let Some(ref_key) = (mode != "clear" && mode != "changed")
            .then(|| extract_ax_ref(&sel.query))
            .flatten()
        else {
            return self
                .eval_in(scope, &engine_call_with(sel, mode, arg), return_by_value, timeout)
                .await;
        };

        let backend_id = self.ax_refs.lock().unwrap().get(&ref_key).copied();
        let Some(backend_id) = backend_id else {
            let reason = format!("unknown element ref @{ref_key}; take a snapshot first");
            return if return_by_value {
                Ok(json!({ "value": { "ok": false, "fatal": true, "reason": reason } }))
            } else {
                Err(Error::NotActionable { selector: sel.to_string(), reason })
            };
        };

        let mut resolve_params = json!({ "backendNodeId": backend_id });
        if let Some(ctx_id) = scope.context {
            resolve_params["executionContextId"] = json!(ctx_id);
        }
        let resolved = match self
            .session_call(&scope.session, "DOM.resolveNode", resolve_params, timeout)
            .await
        {
            Ok(v) => Some(v),
            Err(e) if e.is_retryable() || is_destroyed_context_error(&e) => return Err(e),
            Err(_) => None,
        };
        let obj_id = resolved
            .as_ref()
            .and_then(|v| v["object"]["objectId"].as_str())
            .map(str::to_owned);
        let Some(obj_id) = obj_id else {
            let reason = format!("element ref @{ref_key} is stale or detached; take a new snapshot");
            return if return_by_value {
                Ok(json!({
                    "value": {
                        "ok": false,
                        "fatal": true,
                        "detached": true,
                        "reason": reason
                    }
                }))
            } else {
                Err(Error::NotActionable { selector: sel.to_string(), reason })
            };
        };

        let effective = effective_arg(sel, arg);
        let decl = format!("function(s, m, a) {{ return ({QUERY_JS})(s, m, a, this); }}");
        let res = self
            .session_call(
                &scope.session,
                "Runtime.callFunctionOn",
                json!({
                    "objectId": obj_id,
                    "functionDeclaration": decl,
                    "arguments": [
                        { "value": sel.query },
                        { "value": mode },
                        { "value": effective },
                    ],
                    "returnByValue": return_by_value,
                    "awaitPromise": true,
                }),
                timeout,
            )
            .await;
        let _ = self
            .session_call(
                &scope.session,
                "Runtime.releaseObject",
                json!({ "objectId": obj_id }),
                timeout,
            )
            .await;
        let res = res?;
        match js_error(&res) {
            Some(e) => Err(e),
            None => Ok(res["result"].clone()),
        }
    }
}

#[derive(Clone)]
enum Hop {
    Css(String),
    BackendNode(i64),
}

impl CdpPage {
    async fn frame_owner_node(&self, session: &str, frame_id: &str, timeout: Duration) -> Result<i64> {
        for attempt in 0..2 {
            match self
                .session_call(session, "DOM.getFrameOwner", json!({ "frameId": frame_id }), timeout)
                .await
            {
                Ok(res) => {
                    if let Some(id) = res["backendNodeId"].as_i64() {
                        return Ok(id);
                    }
                    return Err(Error::Other(format!("frame {frame_id} has no owner node")));
                }
                Err(Error::Command { ref message, .. })
                    if attempt == 0 && message.contains("DOM") =>
                {
                    let _ = self.session_call(session, "DOM.enable", json!({}), timeout).await;
                }
                Err(e) => return Err(e),
            }
        }
        Err(Error::Other(format!("frame {frame_id} has no owner node")))
    }

    async fn resolve_frame_hop(
        &self,
        parent: &Scope,
        obj: &str,
        frame_id: &str,
        timeout: Duration,
    ) -> std::result::Result<Scope, String> {
        let s = parent.session.clone();
        let size = self
            .session_call(
                &s,
                "Runtime.callFunctionOn",
                json!({ "objectId": obj, "returnByValue": true, "functionDeclaration": FRAME_BOX_JS }),
                timeout,
            )
            .await;
        let model = self
            .session_call(&s, "DOM.getBoxModel", json!({ "objectId": obj }), timeout)
            .await;
        let Ok(size) = size else {
            return Err("iframe went away".into());
        };
        let b = &size["result"]["value"];
        let b = |i: usize| b[i].as_f64().unwrap_or(0.0);
        let quad: Vec<f64> = model
            .ok()
            .and_then(|m| m["model"]["content"].as_array().cloned())
            .unwrap_or_default()
            .iter()
            .filter_map(Value::as_f64)
            .collect();
        let map = if quad.len() == 8 && b(0) > 0.0 && b(1) > 0.0 {
            let pt = |i: usize| parent.root.apply(quad[2 * i], quad[2 * i + 1]);
            Affine::onto([pt(0), pt(1), pt(2), pt(3)], b(0), b(1))
        } else {
            parent.map.shifted(b(2), b(3))
        };
        let (session, root) = match CdpBrowser::session_for_target(&self.targets, frame_id).await {
            Some(own) => (own, map),
            None => (s, parent.root),
        };
        self.world(&session, frame_id, map, root, timeout)
            .await
            .ok_or_else(|| "frame not ready".into())
    }

    /// Enters `frames` one by one. `Ok(Err(reason))` means not there yet, so the caller retries.
    /// `scroll` brings each iframe into view first, which a click inside it needs.
    /// Element code always runs in an isolated world, where page scripts cannot patch what it
    /// calls (`querySelector`, `getBoundingClientRect`, ...). `evaluate` stays in the page's world.
    /// Also returns each iframe hop for [`Self::covered`].
    async fn scope(
        &self,
        frames: &[String],
        scroll: bool,
        timeout: Duration,
    ) -> Result<std::result::Result<(Scope, Vec<(Scope, Hop)>), String>> {
        // A page target's main frame id is its target id.
        let Some(mut scope) = self.world(self.sid(), &self.target_id.0, Affine::ID, Affine::ID, timeout).await else {
            return Ok(Err("page not ready".into()));
        };
        let mut hops = Vec::new();
        let mut rest = frames;
        if let Some(target_fid) = frames.first().and_then(|s| s.strip_prefix(FRAME_ID_PREFIX)) {
            rest = &frames[1..];
            if target_fid != self.target_id.0 {
                let Some(path) = self.frames.lock().unwrap().ancestors(target_fid) else {
                    return Err(Error::Other("frame was detached".into()));
                };
                for (_parent_fid, child_fid) in path {
                    let backend_node_id = self.frame_owner_node(&scope.session, &child_fid, timeout).await?;
                    let resolved = self
                        .session_call(
                            &scope.session,
                            "DOM.resolveNode",
                            json!({ "backendNodeId": backend_node_id }),
                            timeout,
                        )
                        .await;
                    let Ok(resolved) = resolved else {
                        return Ok(Err(format!("frame {child_fid} went away")));
                    };
                    let Some(obj) = resolved["object"]["objectId"].as_str().map(String::from) else {
                        return Ok(Err(format!("frame {child_fid} went away")));
                    };
                    if scroll {
                        let _ = self
                            .session_call(
                                &scope.session,
                                "Runtime.callFunctionOn",
                                json!({ "objectId": obj, "functionDeclaration": "function() { this.scrollIntoViewIfNeeded(true); }" }),
                                timeout,
                            )
                            .await;
                    }
                    let next = self.resolve_frame_hop(&scope, &obj, &child_fid, timeout).await;
                    let _ = self
                        .session_call(&scope.session, "Runtime.releaseObject", json!({ "objectId": obj }), timeout)
                        .await;
                    match next {
                        Ok(next) => hops.push((std::mem::replace(&mut scope, next), Hop::BackendNode(backend_node_id))),
                        Err(reason) => return Ok(Err(format!("frame {child_fid}: {reason}"))),
                    }
                }
            }
        }
        for css in rest {
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
                    self.forget_world(&scope);
                    return Ok(Err(e.to_string()));
                }
            };
            let Some(obj) = el["objectId"].as_str().map(String::from) else {
                return Ok(Err(format!("no iframe matches {css}")));
            };
            let s = scope.session.clone();
            let node = self.session_call(&s, "DOM.describeNode", json!({ "objectId": obj }), timeout).await;
            let frame_id = node
                .as_ref()
                .ok()
                .and_then(|n| n["node"]["frameId"].as_str().map(String::from));
            let next = match frame_id {
                Some(ref fid) => self.resolve_frame_hop(&scope, &obj, fid, timeout).await,
                None => Err("is not an iframe, or has not loaded".into()),
            };
            let _ = self.session_call(&s, "Runtime.releaseObject", json!({ "objectId": obj }), timeout).await;
            let Some(fid) = frame_id else {
                return Ok(Err(format!("{css} is not an iframe, or has not loaded")));
            };
            match next {
                Ok(next) => hops.push((std::mem::replace(&mut scope, next), Hop::Css(css.clone()))),
                Err(reason) => return Ok(Err(format!("{css} ({fid}): {reason}"))),
            }
        }
        Ok(Ok((scope, hops)))
    }

    /// A click inside an iframe lands only if nothing in a parent document covers the iframe
    /// at that point: the element's own hit test cannot see a parent's overlay.
    async fn covered(&self, hops: &[(Scope, Hop)], x: f64, y: f64, timeout: Duration) -> Option<String> {
        let start = Instant::now();
        for (parent, hop) in hops {
            let remaining = timeout.saturating_sub(start.elapsed());
            if remaining.is_zero() {
                return Some("timed out checking frame coverage".into());
            }
            let (px, py) = parent.map.invert(x, y);
            match hop {
                Hop::Css(css) => {
                    let css = serde_json::to_string(css).unwrap();
                    let expr = format!(
                        "(() => {{ const f = document.querySelector({css}); let h = document.elementFromPoint({px}, {py}); \
                         while (h && h !== f && h.shadowRoot) {{ const i = h.shadowRoot.elementFromPoint({px}, {py}); if (!i || i === h) break; h = i; }} \
                         return f && h === f ? '' : 'obscured by <' + (h ? h.tagName.toLowerCase() + (h.id ? '#' + h.id : '') : 'nothing') + '> over ' + {css}; }})()"
                    );
                    match self.eval_in(parent, &expr, true, remaining).await {
                        Ok(v) if v["value"] == "" => {}
                        Ok(v) => return Some(v["value"].as_str().unwrap_or("frame hit test failed").to_string()),
                        Err(e) => return Some(e.to_string()),
                    }
                }
                Hop::BackendNode(backend_node_id) => {
                    let Ok(resolved) = self
                        .session_call(
                            &parent.session,
                            "DOM.resolveNode",
                            json!({ "backendNodeId": backend_node_id }),
                            remaining,
                        )
                        .await
                    else {
                        return Some("iframe went away".into());
                    };
                    let Some(obj) = resolved["object"]["objectId"].as_str().map(String::from) else {
                        return Some("iframe went away".into());
                    };
                    let rem = timeout.saturating_sub(start.elapsed());
                    let res = if rem.is_zero() {
                        Err(Error::timeout("frame coverage", timeout.as_millis() as u64))
                    } else {
                        self.session_call(
                            &parent.session,
                            "Runtime.callFunctionOn",
                            json!({
                                "objectId": obj,
                                "returnByValue": true,
                                "arguments": [{ "value": px }, { "value": py }],
                                "functionDeclaration": "function(x, y) { let h = this.ownerDocument.elementFromPoint(x, y); while (h && h !== this && h.shadowRoot) { const i = h.shadowRoot.elementFromPoint(x, y); if (!i || i === h) break; h = i; } return h === this ? '' : 'obscured by <' + (h ? h.tagName.toLowerCase() + (h.id ? '#' + h.id : '') : 'nothing') + '> over iframe'; }"
                            }),
                            rem,
                        )
                        .await
                    };
                    let _ = self
                        .session_call(&parent.session, "Runtime.releaseObject", json!({ "objectId": obj }), Duration::from_secs(2))
                        .await;
                    match res {
                        Ok(v) if v["result"]["value"] == "" => {}
                        Ok(v) => return Some(v["result"]["value"].as_str().unwrap_or("frame hit test failed").to_string()),
                        Err(e) => return Some(e.to_string()),
                    }
                }
            }
        }
        None
    }

    /// The isolated world of `frame_id`, made once per document and cached.
    async fn world(&self, session: &str, frame_id: &str, map: Affine, root: Affine, timeout: Duration) -> Option<Scope> {
        let cached = self.worlds.lock().unwrap().get(frame_id).copied();
        let id = match cached {
            Some(id) => id,
            None => {
                let w = self
                    .session_call(session, "Page.createIsolatedWorld",
                        json!({ "frameId": frame_id, "worldName": "fluxwright" }), timeout)
                    .await
                    .ok()?;
                let id = w["executionContextId"].as_i64()?;
                self.worlds.lock().unwrap().insert(frame_id.to_string(), id);
                id
            }
        };
        Some(Scope { session: session.to_string(), context: Some(id), world_of: Some(frame_id.to_string()), map, root })
    }

    /// Navigation destroys isolated worlds; drop a dead one so the next poll makes a new one.
    fn forget_world(&self, scope: &Scope) {
        if let Some(frame) = &scope.world_of {
            self.worlds.lock().unwrap().remove(frame);
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

    /// Adds cookies to this page's context.
    pub async fn add_cookies(&self, cookies: &[Cookie], timeout: Duration) -> Result<()> {
        if cookies.is_empty() {
            return Ok(());
        }
        let cookies: Vec<Value> = cookies.iter().map(Cookie::to_cdp).collect();
        self.browser
            .call(
                "Storage.setCookies",
                json!({ "cookies": cookies, "browserContextId": self.context_id.0 }),
                None,
                timeout,
            )
            .await?;
        Ok(())
    }

    /// Clears every cookie in this page's context.
    pub async fn clear_cookies(&self, timeout: Duration) -> Result<()> {
        self.browser
            .call(
                "Storage.clearCookies",
                json!({ "browserContextId": self.context_id.0 }),
                None,
                timeout,
            )
            .await?;
        Ok(())
    }

    /// Returns cookies in this page's context matching `urls` (or all cookies when empty).
    pub async fn cookies_for_urls(&self, urls: &[String], timeout: Duration) -> Result<Vec<Cookie>> {
        let res = self
            .browser
            .call(
                "Storage.getCookies",
                json!({ "browserContextId": self.context_id.0 }),
                None,
                timeout,
            )
            .await?;
        let cookies: Vec<Cookie> = res["cookies"]
            .as_array()
            .map(|a| a.iter().filter_map(Cookie::from_cdp).collect())
            .unwrap_or_default();
        filter_cookies(&cookies, urls)
    }

    /// Clears cookies matching `filter` in this page's context.
    pub async fn clear_cookies_filtered(&self, filter: &CookieFilter, timeout: Duration) -> Result<()> {
        if filter.is_empty() {
            return self.clear_cookies(timeout).await;
        }
        let all = self.cookies_for_urls(&[], timeout).await?;
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
            self.clear_cookies(timeout).await?;
            self.add_cookies(&keep, timeout).await?;
        }
        Ok(())
    }

    /// Changes the CSS media type and/or `prefers-color-scheme` (`""` resets either).
    pub async fn emulate_media(
        &self,
        media: Option<&str>,
        color_scheme: Option<&str>,
        timeout: Duration,
    ) -> Result<()> {
        let mut params = json!({});
        if let Some(m) = media {
            if !matches!(m, "screen" | "print" | "") {
                return Err(Error::Other(format!(
                    "media must be screen, print or null, not {m:?}"
                )));
            }
            params["media"] = json!(m);
        }
        if let Some(cs) = color_scheme {
            if !matches!(cs, "light" | "dark" | "no-preference" | "") {
                return Err(Error::Other(format!(
                    "colorScheme must be light, dark, no-preference or null, not {cs:?}"
                )));
            }
            params["features"] = json!([{ "name": "prefers-color-scheme", "value": cs }]);
        }
        self.call("Emulation.setEmulatedMedia", params, timeout).await?;
        Ok(())
    }

    /// Supplies HTTP Basic/Digest authentication credentials for this context.
    pub async fn set_http_credentials(
        &self,
        credentials: &(String, String),
        timeout: Duration,
    ) -> Result<()> {
        let page_setup = self.page_setups.lock().await.get(&self.target_id.0).cloned();
        let fetch = {
            let mut setups = self.setups.lock().await;
            let setup = setups.entry(self.context_id.0.clone()).or_default();
            setup.http_auth = Some(credentials.clone());
            fetch_params_combined(Some(setup), page_setup.as_ref())
        };
        if let Some(params) = fetch {
            self.call("Fetch.enable", params, timeout).await?;
        }
        Ok(())
    }

    /// Starts this page's context from saved cookies and localStorage. Call it before the
    /// first navigation.
    pub async fn set_storage_state(&self, state: &StorageState, timeout: Duration) -> Result<()> {
        self.add_cookies(&state.cookies, timeout).await?;
        if let Some(source) = state.restore_script() {
            self.setups
                .lock()
                .await
                .entry(self.context_id.0.clone())
                .or_default()
                .init_scripts
                .push(source.clone());
            let mut sessions = CdpBrowser::sessions_in_context(&self.targets, &self.context_id.0).await;
            if !sessions.iter().any(|s| s == self.sid()) {
                sessions.push(self.sid().to_string());
            }
            for s in sessions {
                let res = self
                    .session_call(
                        &s,
                        "Page.addScriptToEvaluateOnNewDocument",
                        json!({ "source": &source }),
                        timeout,
                    )
                    .await;
                if s == self.sid() {
                    res?;
                }
            }
        }
        Ok(())
    }

    /// Registers `source` to run before any page script on every navigation in this page
    /// and its iframes.
    pub async fn add_init_script(&self, source: &str, timeout: Duration) -> Result<()> {
        self.page_setups
            .lock()
            .await
            .entry(self.target_id.0.clone())
            .or_default()
            .init_scripts
            .push(source.to_string());
        let mut sessions = CdpBrowser::sessions_for_page(&self.targets, &self.target_id.0).await;
        if !sessions.iter().any(|s| s == self.sid()) {
            sessions.push(self.sid().to_string());
        }
        for s in sessions {
            let res = self
                .session_call(
                    &s,
                    "Page.addScriptToEvaluateOnNewDocument",
                    json!({ "source": source }),
                    timeout,
                )
                .await;
            if s == self.sid() {
                res?;
            }
        }
        Ok(())
    }

    /// Installs or resets the deterministic fake clock on this page and across subsequent navigations.
    pub async fn clock_install(&self, opts: &ClockInstallOptions, timeout: Duration) -> Result<()> {
        let mut cmd = json!({ "op": "install" });
        if let Some(ref t) = opts.time {
            cmd["time"] = parse_clock_time(t)?;
        }
        self.run_clock_command(cmd, timeout).await
    }

    /// Jumps to `time` (if ahead of the current clock) and pauses the fake clock.
    pub async fn clock_pause_at(&self, time: &ClockTime, timeout: Duration) -> Result<()> {
        let cmd = json!({ "op": "pauseAt", "time": parse_clock_time(time)? });
        self.run_clock_command(cmd, timeout).await
    }

    /// Freezes `Date.now()` / `new Date()` to `time` while allowing timers to continue running.
    pub async fn clock_set_fixed_time(&self, time: &ClockTime, timeout: Duration) -> Result<()> {
        let cmd = json!({ "op": "setFixedTime", "time": parse_clock_time(time)? });
        self.run_clock_command(cmd, timeout).await
    }

    /// Sets the fake system clock to `time` without freezing `Date` or firing timers.
    pub async fn clock_set_system_time(&self, time: &ClockTime, timeout: Duration) -> Result<()> {
        let cmd = json!({ "op": "setSystemTime", "time": parse_clock_time(time)? });
        self.run_clock_command(cmd, timeout).await
    }

    /// Jumps simulated time forward by `ticks`, firing due timers at most once.
    pub async fn clock_fast_forward(&self, ticks: &ClockTicks, timeout: Duration) -> Result<()> {
        let cmd = json!({ "op": "fastForward", "ticks": parse_clock_ticks(ticks)? });
        self.run_clock_command(cmd, timeout).await
    }

    /// Advances simulated time by `ticks`, executing due timers and intervals in chronological order.
    pub async fn clock_run_for(&self, ticks: &ClockTicks, timeout: Duration) -> Result<()> {
        let cmd = json!({ "op": "runFor", "ticks": parse_clock_ticks(ticks)? });
        self.run_clock_command(cmd, timeout).await
    }

    /// Resumes natural wall-clock progression from the current fake clock state.
    pub async fn clock_resume(&self, timeout: Duration) -> Result<()> {
        self.run_clock_command(json!({ "op": "resume" }), timeout).await
    }

    async fn run_clock_command(&self, cmd: Value, timeout: Duration) -> Result<()> {
        let cmd_json = serde_json::to_string(&cmd).unwrap();
        let expr = format!("({CLOCK_JS}).command({cmd_json})");
        let state = self.evaluate(&expr, timeout).await?;
        let state_json = serde_json::to_string(&state).unwrap();
        let init_script = format!("({CLOCK_JS}).restore({state_json});");
        self.sync_clock_init_script(init_script, timeout).await
    }

    async fn sync_clock_init_script(&self, source: String, timeout: Duration) -> Result<()> {
        let old_ids = {
            let mut ps = self.page_setups.lock().await;
            let setup = ps.entry(self.target_id.0.clone()).or_default();
            setup.clock_init_script = Some(source.clone());
            std::mem::take(&mut setup.clock_script_ids)
        };
        let mut sessions = CdpBrowser::sessions_for_page(&self.targets, &self.target_id.0).await;
        if !sessions.iter().any(|s| s == self.sid()) {
            sessions.push(self.sid().to_string());
        }
        let mut new_ids = HashMap::new();
        for s in sessions {
            if let Some(old_id) = old_ids.get(&s) {
                let _ = self
                    .session_call(
                        &s,
                        "Page.removeScriptToEvaluateOnNewDocument",
                        json!({ "identifier": old_id }),
                        timeout,
                    )
                    .await;
            }
            let res = self
                .session_call(
                    &s,
                    "Page.addScriptToEvaluateOnNewDocument",
                    json!({ "source": &source }),
                    timeout,
                )
                .await;
            match res {
                Ok(val) => {
                    if let Some(id) = val["identifier"].as_str() {
                        new_ids.insert(s, id.to_string());
                    }
                }
                Err(e) if s == self.sid() => return Err(e),
                Err(_) => {}
            }
        }
        if !new_ids.is_empty() {
            let mut ps = self.page_setups.lock().await;
            let setup = ps.entry(self.target_id.0.clone()).or_default();
            setup.clock_script_ids = new_ids;
        }
        Ok(())
    }

    /// Invocations of functions exposed via `expose_binding` on this page and its frames.
    pub async fn bindings(&self) -> tokio::sync::mpsc::UnboundedReceiver<crate::events::BindingCall> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        self.page_setups
            .lock()
            .await
            .entry(self.target_id.0.clone())
            .or_default()
            .bindings = Some(tx);
        rx
    }

    /// Exposes `globalThis[name]` on this page and its frames across navigations; calls arrive
    /// on the receiver from [`Self::bindings`].
    pub async fn expose_binding(&self, name: &str, timeout: Duration) -> Result<()> {
        if name.trim().is_empty() {
            return Err(Error::Other("exposeFunction: name must not be empty".into()));
        }
        let (shim, eval_shim) = binding_shims(name);
        if self
            .setups
            .lock()
            .await
            .get(&self.context_id.0)
            .is_some_and(|s| s.exposed_bindings.contains(name))
        {
            return Err(Error::Other(format!(
                "Function \"{name}\" has been already registered"
            )));
        }
        let need_binding = {
            let mut ps = self.page_setups.lock().await;
            let setup = ps.entry(self.target_id.0.clone()).or_default();
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
        let mut sessions = CdpBrowser::sessions_for_page(&self.targets, &self.target_id.0).await;
        if !sessions.iter().any(|s| s == self.sid()) {
            sessions.push(self.sid().to_string());
        }
        install_binding_on_sessions(
            &self.browser,
            &sessions,
            Some(self.sid()),
            need_binding,
            &shim,
            &eval_shim,
            timeout,
        )
        .await
    }

    /// Injects a `<script>` element into the page by external URL or inline content.
    pub async fn add_script_tag(&self, opts: &ScriptTagOptions, timeout: Duration) -> Result<()> {
        if opts.url.is_some() == opts.content.is_some() {
            return Err(Error::Other(
                "add_script_tag: specify exactly one of url or content".into(),
            ));
        }
        if opts.url.as_ref().is_some_and(|u| u.trim().is_empty()) {
            return Err(Error::Other("add_script_tag: url must not be empty".into()));
        }
        let type_json = serde_json::to_string(&opts.script_type).unwrap();
        let js = if let Some(ref url) = opts.url {
            format!(
                r#"((url, type) => new Promise((resolve, reject) => {{
                    const script = document.createElement('script');
                    if (type) script.type = type;
                    script.onload = () => resolve();
                    script.onerror = () => reject(new Error('Failed to load script: ' + url));
                    script.src = url;
                    (document.head || document.documentElement).appendChild(script);
                }}))({}, {type_json})"#,
                serde_json::to_string(url).unwrap(),
            )
        } else {
            let content = opts.content.as_deref().unwrap_or("");
            format!(
                r#"((content, type) => {{
                    const script = document.createElement('script');
                    if (type) script.type = type;
                    let err = null;
                    const onError = (e) => {{ err = e.error || new Error(e.message || 'Script error'); }};
                    window.addEventListener('error', onError);
                    try {{
                        script.text = content;
                        (document.head || document.documentElement).appendChild(script);
                    }} finally {{
                        window.removeEventListener('error', onError);
                    }}
                    if (err) throw err;
                }})({}, {type_json})"#,
                serde_json::to_string(content).unwrap(),
            )
        };
        self.eval_in(&self.main_scope(), &js, true, timeout).await?;
        Ok(())
    }

    /// Injects a `<link rel="stylesheet">` or `<style>` element into the page.
    pub async fn add_style_tag(&self, opts: &StyleTagOptions, timeout: Duration) -> Result<()> {
        if opts.url.is_some() == opts.content.is_some() {
            return Err(Error::Other(
                "add_style_tag: specify exactly one of url or content".into(),
            ));
        }
        if opts.url.as_ref().is_some_and(|u| u.trim().is_empty()) {
            return Err(Error::Other("add_style_tag: url must not be empty".into()));
        }
        let js = if let Some(ref url) = opts.url {
            format!(
                r#"((url) => new Promise((resolve, reject) => {{
                    const link = document.createElement('link');
                    link.rel = 'stylesheet';
                    link.onload = () => resolve();
                    link.onerror = () => reject(new Error('Failed to load stylesheet: ' + url));
                    link.href = url;
                    (document.head || document.documentElement).appendChild(link);
                }}))({})"#,
                serde_json::to_string(url).unwrap(),
            )
        } else {
            let content = opts.content.as_deref().unwrap_or("");
            format!(
                r#"((content) => {{
                    const style = document.createElement('style');
                    style.textContent = content;
                    (document.head || document.documentElement).appendChild(style);
                }})({})"#,
                serde_json::to_string(content).unwrap(),
            )
        };
        self.eval_in(&self.main_scope(), &js, true, timeout).await?;
        Ok(())
    }

    /// Every cookie in this page's context, plus localStorage (and optionally IndexedDB) of the current origin.
    pub async fn storage_state(&self, timeout: Duration) -> Result<StorageState> {
        self.storage_state_with(false, timeout).await
    }

    /// Every cookie in this page's context, plus localStorage (and IndexedDB when requested) of the current origin.
    pub async fn storage_state_with(&self, include_indexed_db: bool, timeout: Duration) -> Result<StorageState> {
        let res = self
            .browser
            .call("Storage.getCookies", json!({ "browserContextId": self.context_id.0 }), None, timeout)
            .await?;
        let cookies = res["cookies"].as_array().map(|a| a.iter().filter_map(Cookie::from_cdp).collect()).unwrap_or_default();
        let js = if include_indexed_db { STORAGE_WITH_INDEXED_DB_JS } else { LOCAL_STORAGE_JS };
        let mut local: Option<OriginStorage> = serde_json::from_value(self.evaluate(js, timeout).await?).unwrap_or(None);
        if let Some(ref mut orig) = local {
            if orig.indexed_db.as_ref().is_some_and(Vec::is_empty) {
                orig.indexed_db = None;
            }
        }
        let origins = local
            .into_iter()
            .filter(|o| !o.local_storage.is_empty() || o.indexed_db.as_ref().is_some_and(|d| !d.is_empty()))
            .collect();
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

    pub async fn screenshot(&self, options: &ScreenshotOptions, timeout: Duration) -> Result<Vec<u8>> {
        options.validate()?;
        let mut params = json!({});
        if options.full_page || options.clip.is_some() {
            let m = self.call("Page.getLayoutMetrics", json!({}), timeout).await?;
            params["captureBeyondViewport"] = json!(true);
            params["clip"] = match options.clip {
                Some(c) => {
                    // Without full_page the clip is relative to the viewport: add the scroll.
                    let (dx, dy) = if options.full_page {
                        (0.0, 0.0)
                    } else {
                        let vp = if m["cssLayoutViewport"].is_object() { &m["cssLayoutViewport"] } else { &m["layoutViewport"] };
                        (vp["pageX"].as_f64().unwrap_or(0.0), vp["pageY"].as_f64().unwrap_or(0.0))
                    };
                    json!({ "x": c.x + dx, "y": c.y + dy, "width": c.width, "height": c.height, "scale": 1 })
                }
                None => {
                    let size = if m["cssContentSize"].is_object() { &m["cssContentSize"] } else { &m["contentSize"] };
                    json!({ "x": 0, "y": 0, "width": size["width"], "height": size["height"], "scale": 1 })
                }
            };
        }
        self.capture(params, options, timeout).await
    }

    /// The first match's box relative to the top-level viewport, without scrolling; `None`
    /// when it is not visible. Waits until the element exists.
    pub async fn bounding_box(&self, sel: &Selector, timeout: Duration) -> Result<Option<BoundingBox>> {
        let (v, scope) = self.wait_element(sel, Wait::Query { mode: "rect", scroll: false }, timeout).await?;
        let b = &v["box"];
        if b.is_null() {
            return Ok(None);
        }
        let f = |k: &str| b[k].as_f64().unwrap_or(0.0);
        let [x, y, width, height] = scope.map.rect(f("x"), f("y"), f("width"), f("height"));
        Ok(Some(BoundingBox { x, y, width, height }))
    }

    /// Calls `function` (JavaScript source, such as `(el, arg) => el.value`) with the first match
    /// and `arg`, in the page's own world as Playwright does, and returns its JSON result.
    pub async fn evaluate_on(&self, sel: &Selector, function: &str, arg: Option<Value>, timeout: Duration) -> Result<Value> {
        let (_, scope) = self.wait_element(sel, Wait::Query { mode: "attached", scroll: false }, timeout).await?;
        let found = self.eval_engine(&scope, sel, "element", &Value::Null, false, timeout).await?;
        let Some(isolated) = found["objectId"].as_str().filter(|_| found["subtype"] == "node") else {
            return Err(Error::NotActionable { selector: sel.to_string(), reason: "element went away".into() });
        };
        // The engine ran in an isolated world; hand the same node to the page's main world,
        // where page scripts' globals live.
        let call = |method: &'static str, params: Value| self.session_call(&scope.session, method, params, timeout);
        let mut element: Option<String> = None;
        let res = async {
            let node = call("DOM.describeNode", json!({ "objectId": isolated })).await?;
            let main = call("DOM.resolveNode", json!({ "backendNodeId": node["node"]["backendNodeId"] })).await?;
            let el = main["object"]["objectId"]
                .as_str()
                .ok_or_else(|| Error::NotActionable {
                    selector: sel.to_string(),
                    reason: "element went away".into(),
                })?
                .to_string();
            let mut args = vec![json!({ "objectId": el })];
            if let Some(arg) = arg {
                args.push(json!({ "value": arg }));
            }
            element = Some(el.clone());
            call(
                "Runtime.callFunctionOn",
                json!({ "functionDeclaration": function, "objectId": el, "arguments": args,
                        "returnByValue": true, "awaitPromise": true }),
            )
            .await
        }
        .await;
        let _ = call("Runtime.releaseObject", json!({ "objectId": isolated })).await;
        if let Some(el) = element {
            let _ = call("Runtime.releaseObject", json!({ "objectId": el })).await;
        }
        let res = res?;
        match js_error(&res) {
            Some(e) => Err(e),
            None => Ok(res["result"]["value"].clone()),
        }
    }

    /// Hands every request of this page and its iframes to the returned channel, as
    /// Playwright's `page.route` does. Resource blocking still applies first. Call it before
    /// navigating; a request dropped unanswered continues.
    pub async fn intercept(&self, timeout: Duration) -> Result<tokio::sync::mpsc::UnboundedReceiver<InterceptedRequest>> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let params = {
            let mut ps = self.page_setups.lock().await;
            let page_setup = ps.entry(self.target_id.0.clone()).or_default();
            page_setup.routes = Some(tx);
            let ctx_setup = self.setups.lock().await.get(&self.context_id.0).cloned();
            fetch_params_combined(ctx_setup.as_ref(), Some(page_setup))
        };
        if let Some(params) = params {
            let mut sessions = CdpBrowser::sessions_for_page(&self.targets, &self.target_id.0).await;
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
    /// `download_dir`) from this page and its iframes. Recording stops when the returned task
    /// is aborted.
    pub fn collect_logs(&self, download_dir: &Path) -> (Arc<std::sync::Mutex<PageLog>>, DownloadQueue, tokio::task::JoinHandle<()>) {
        let log = Arc::new(std::sync::Mutex::new(PageLog::default()));
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let watch = crate::log::Watch {
            session: self.session_id.0.clone(),
            main_frame: self.target_id.0.clone(),
            context: self.context_id.0.clone(),
            download_dir: download_dir.to_path_buf(),
        };
        let events = self
            .early_log_events
            .lock()
            .unwrap()
            .take()
            .unwrap_or_else(|| self.browser.subscribe_session(&self.session_id.0));
        let task = tokio::spawn(crate::log::collect_session(events, watch, log.clone(), tx, self.browser.clone()));
        (log, rx, task)
    }

    /// Installs channels for dialogs, file choosers, exposed bindings, and popups in a single
    /// `page_setups` lock acquisition.
    #[allow(clippy::type_complexity)]
    pub async fn install_event_channels(
        &self,
    ) -> (
        tokio::sync::mpsc::UnboundedReceiver<crate::events::Dialog>,
        tokio::sync::mpsc::UnboundedReceiver<crate::events::FileChooser>,
        tokio::sync::mpsc::UnboundedReceiver<crate::events::BindingCall>,
        tokio::sync::mpsc::UnboundedReceiver<CdpPage>,
    ) {
        let (dlg_tx, dlg_rx) = tokio::sync::mpsc::unbounded_channel();
        let (fc_tx, fc_rx) = tokio::sync::mpsc::unbounded_channel();
        let (bind_tx, bind_rx) = tokio::sync::mpsc::unbounded_channel();
        let (pop_tx, pop_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut ps = self.page_setups.lock().await;
        let setup = ps.entry(self.target_id.0.clone()).or_default();
        setup.dialogs = Some(dlg_tx);
        setup.file_choosers = Some(fc_tx);
        setup.bindings = Some(bind_tx);
        setup.popups = Some(pop_tx);
        (dlg_rx, fc_rx, bind_rx, pop_rx)
    }

    /// Hands dialogs of this page and its iframes to the returned queue instead of
    /// answering them; one the queue drops is answered as before (see [`Dialog`](crate::Dialog)).
    pub async fn dialogs(&self) -> tokio::sync::mpsc::UnboundedReceiver<crate::events::Dialog> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        self.page_setups
            .lock()
            .await
            .entry(self.target_id.0.clone())
            .or_default()
            .dialogs = Some(tx);
        rx
    }

    /// Hands intercepted file chooser dialogs of this page and its frames to the returned queue.
    pub async fn file_choosers(&self) -> tokio::sync::mpsc::UnboundedReceiver<crate::events::FileChooser> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        self.page_setups
            .lock()
            .await
            .entry(self.target_id.0.clone())
            .or_default()
            .file_choosers = Some(tx);
        rx
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

    pub(crate) async fn eval_isolated(&self, expression: &str, timeout: Duration) -> Result<Value> {
        let scope = self
            .world(self.sid(), &self.target_id.0, Affine::ID, Affine::ID, timeout)
            .await
            .unwrap_or_else(|| self.main_scope());
        self.eval_in(&scope, expression, true, timeout).await
    }

    pub(crate) async fn collect_mask_rects(
        &self,
        selectors: &[String],
        timeout: Duration,
    ) -> Result<Vec<(f64, f64, f64, f64)>> {
        if selectors.is_empty() {
            return Ok(Vec::new());
        }
        let scope = self
            .world(self.sid(), &self.target_id.0, Affine::ID, Affine::ID, timeout)
            .await
            .unwrap_or_else(|| self.main_scope());
        let mut rects = Vec::new();
        for raw_sel in selectors {
            let sel = Selector::new(raw_sel.clone());
            let res = self
                .eval_engine(&scope, &sel, "rects", &Value::Null, true, timeout)
                .await?;
            let val = &res["value"];
            if val["ok"].as_bool() != Some(true) {
                let reason = val["reason"].as_str().unwrap_or("invalid mask selector");
                return Err(Error::Other(format!("screenshot mask {raw_sel:?}: {reason}")));
            }
            if let Some(arr) = val["rects"].as_array() {
                for r in arr {
                    let (Some(x), Some(y), Some(w), Some(h)) = (
                        r["x"].as_f64(),
                        r["y"].as_f64(),
                        r["width"].as_f64(),
                        r["height"].as_f64(),
                    ) else {
                        continue;
                    };
                    if x.is_finite() && y.is_finite() && w.is_finite() && h.is_finite() && w > 0.0 && h > 0.0 {
                        rects.push((x, y, w, h));
                    }
                }
            }
        }
        Ok(rects)
    }

    pub(crate) async fn collect_som_boxes(
        &self,
        include_offscreen: bool,
        timeout: Duration,
    ) -> Result<Vec<(String, f64, f64, f64, f64)>> {
        let is_html = self
            .eval_isolated(
                "Boolean(document && document.documentElement && (document.body || (typeof HTMLDocument !== 'undefined' && document instanceof HTMLDocument)))",
                timeout,
            )
            .await
            .ok()
            .and_then(|v| v["value"].as_bool())
            .unwrap_or(false);
        if !is_html {
            return Ok(Vec::new());
        }
        let Ok(snap) = self
            .capture_aria_snapshot(
                &SnapshotOptions {
                    interactive_only: true,
                    ..Default::default()
                },
                false,
                timeout,
            )
            .await
        else {
            return Ok(Vec::new());
        };
        if snap.refs.is_empty() {
            return Ok(Vec::new());
        }

        const SOM_GROUP: &str = "__fw_som";
        let resolve_futs = snap.refs.iter().map(|r| {
            self.call(
                "DOM.resolveNode",
                json!({ "backendNodeId": r.backend_node_id, "objectGroup": SOM_GROUP }),
                timeout,
            )
        });
        let resolved = futures_util::future::join_all(resolve_futs).await;

        let mut pairs = Vec::with_capacity(snap.refs.len());
        for (aria_ref, res) in snap.refs.iter().zip(resolved.into_iter()) {
            if let Ok(v) = res {
                if let Some(obj_id) = v["object"]["objectId"].as_str() {
                    pairs.push((aria_ref.id.clone(), obj_id.to_string()));
                }
            }
        }
        if pairs.is_empty() {
            return Ok(Vec::new());
        }

        const BOX_FN: &str = r#"function(includeOffscreen) {
            if (!this || this.nodeType !== 1 || !this.isConnected) return null;
            const win = (this.ownerDocument && this.ownerDocument.defaultView) || window;
            const st = win.getComputedStyle(this);
            if (st.display === 'none' || st.visibility === 'hidden') return null;
            const r = this.getBoundingClientRect();
            if (r.width <= 0 || r.height <= 0) return null;
            let x = r.x, y = r.y;
            let curWin = win;
            while (curWin && curWin !== window) {
                const fe = curWin.frameElement;
                if (!fe) break;
                const fr = fe.getBoundingClientRect();
                x += fr.left + (fe.clientLeft || 0);
                y += fr.top + (fe.clientTop || 0);
                curWin = curWin.parent;
            }
            if (!includeOffscreen) {
                if (x + r.width <= 0 || x >= window.innerWidth || y + r.height <= 0 || y >= window.innerHeight) {
                    return null;
                }
            }
            return { x, y, width: r.width, height: r.height };
        }"#;

        let box_futs = pairs.iter().map(|(_, obj_id)| {
            self.call(
                "Runtime.callFunctionOn",
                json!({
                    "objectId": obj_id,
                    "functionDeclaration": BOX_FN,
                    "arguments": [{ "value": include_offscreen }],
                    "returnByValue": true,
                    "awaitPromise": false,
                }),
                timeout,
            )
        });
        let box_results = futures_util::future::join_all(box_futs).await;
        let _ = self
            .call(
                "Runtime.releaseObjectGroup",
                json!({ "objectGroup": SOM_GROUP }),
                timeout.max(Duration::from_secs(1)),
            )
            .await;

        let mut boxes = Vec::new();
        for ((id, _), res) in pairs.into_iter().zip(box_results.into_iter()) {
            let Ok(v) = res else {
                continue;
            };
            let b = &v["result"]["value"];
            if let (Some(x), Some(y), Some(w), Some(h)) = (
                b["x"].as_f64(),
                b["y"].as_f64(),
                b["width"].as_f64(),
                b["height"].as_f64(),
            ) {
                if x.is_finite() && y.is_finite() && w.is_finite() && h.is_finite() && w > 0.0 && h > 0.0 {
                    boxes.push((id, x, y, w, h));
                }
            }
        }
        Ok(boxes)
    }

    async fn element_screenshot_with_scope_styles(
        &self,
        scope: &Scope,
        params: Value,
        options: &ScreenshotOptions,
        has_iframe: bool,
        timeout: Duration,
    ) -> Result<Vec<u8>> {
        let style_css = if has_iframe {
            options.style_css()
        } else {
            None
        };
        if let Some(ref css) = style_css {
            let js = crate::render::inject_style_js(css);
            let _ = self.eval_in(scope, &js, true, timeout).await;
        }
        let res = self.capture(params, options, timeout).await;
        if style_css.is_some() {
            let _ = self
                .eval_in(
                    scope,
                    crate::render::REMOVE_SCREENSHOT_STYLE_JS,
                    true,
                    timeout.max(Duration::from_secs(2)),
                )
                .await;
        }
        res
    }

    /// PNG of the first match, once it is visible and still. Elements in iframes, below the
    /// fold, or taller than the viewport work too.
    pub async fn element_screenshot(&self, sel: &Selector, options: &ScreenshotOptions, timeout: Duration) -> Result<Vec<u8>> {
        options.validate()?;
        if options.full_page || options.clip.is_some() {
            return Err(Error::Other("element screenshot: full_page and clip are for page screenshots".into()));
        }
        let (b, scope) = self.wait_element(sel, Wait::Box, timeout).await?;
        // The box is in viewport coordinates; the clip is in document coordinates.
        let m = self.call("Page.getLayoutMetrics", json!({}), timeout).await?;
        let vp = if m["cssLayoutViewport"].is_object() { &m["cssLayoutViewport"] } else { &m["layoutViewport"] };
        let f = |v: &Value, k: &str| v[k].as_f64().unwrap_or(0.0);
        let params = json!({
            "captureBeyondViewport": true,
            "clip": {
                "x": f(&b, "x") + f(vp, "pageX"),
                "y": f(&b, "y") + f(vp, "pageY"),
                "width": f(&b, "width"),
                "height": f(&b, "height"),
                "scale": 1
            }
        });
        self.element_screenshot_with_scope_styles(&scope, params, options, !sel.frames.is_empty(), timeout)
            .await
    }

    /// Waits until the element is visible (Playwright's default `waitForSelector` state):
    /// non-empty box, not `visibility: hidden`. Opacity 0 counts as visible, as in Playwright.
    pub async fn wait_for_selector(&self, sel: &Selector, timeout: Duration) -> Result<Value> {
        Ok(self.wait_element(sel, Wait::Query { mode: "visible", scroll: false }, timeout).await?.0)
    }

    /// Waits for the selector to reach `state`, as Playwright's `waitForSelector({ state })`.
    pub async fn wait_for_selector_state(&self, sel: &Selector, state: WaitState, timeout: Duration) -> Result<()> {
        let (mode, present) = match state {
            WaitState::Attached => ("attached", true),
            WaitState::Visible => ("visible", true),
            WaitState::Detached => ("attached", false),
            WaitState::Hidden => ("visible", false),
        };
        if present {
            return self
                .wait_element(sel, Wait::Query { mode, scroll: false }, timeout)
                .await
                .map(|_| ());
        }
        // Waiting for it to go: poll until the check that `mode` makes fails.
        let start = Instant::now();
        let what = if mode == "attached" { "still attached" } else { "still visible" };
        loop {
            if self.browser.is_dead() {
                return Err(Error::BrowserDead { pid: self.browser_pid });
            }
            let remaining = timeout.saturating_sub(start.elapsed());
            if remaining.is_zero() {
                return Err(Error::timeout(format!("{sel} (last: {what})"), timeout.as_millis() as u64));
            }
            match self.scope(&sel.frames, false, remaining).await {
                Err(e) if e.is_timeout() => {
                    return Err(Error::timeout(format!("{sel} (last: {what})"), timeout.as_millis() as u64));
                }
                Err(e) => return Err(e),
                // Its iframe is gone or not there yet, so the element is not there either.
                Ok(Err(_)) => {
                    if timeout.saturating_sub(start.elapsed()).is_zero() {
                        return Err(Error::timeout(format!("{sel} (last: {what})"), timeout.as_millis() as u64));
                    }
                    return Ok(());
                }
                Ok(Ok((scope, _))) => {
                    let remaining = timeout.saturating_sub(start.elapsed());
                    if remaining.is_zero() {
                        return Err(Error::timeout(format!("{sel} (last: {what})"), timeout.as_millis() as u64));
                    }
                    match self.eval_engine(&scope, sel, mode, &Value::Null, true, remaining).await {
                        Ok(r) if r["value"]["ok"].as_bool() == Some(true) => {}
                        Ok(r) if r["value"]["detached"].as_bool() == Some(true) => return Ok(()),
                        Ok(r) if r["value"]["fatal"].as_bool() == Some(true) => {
                            let reason = r["value"]["reason"].as_str().unwrap_or("unknown").to_string();
                            return Err(Error::NotActionable { selector: sel.to_string(), reason });
                        }
                        Ok(_) => return Ok(()),
                        Err(e) if e.is_retryable() => return Err(e),
                        Err(e) if e.is_timeout() => {
                            return Err(Error::timeout(format!("{sel} (last: {what})"), timeout.as_millis() as u64));
                        }
                        // Mid-navigation the context can vanish; ask the new document.
                        Err(_) => self.forget_world(&scope),
                    }
                }
            }
            let remaining = timeout.saturating_sub(start.elapsed());
            if remaining.is_zero() {
                return Err(Error::timeout(format!("{sel} (last: {what})"), timeout.as_millis() as u64));
            }
            sleep(Duration::from_millis(50).min(remaining)).await;
        }
    }

    /// `textContent` of the first match, once it exists (visible or not).
    pub async fn text_content(&self, sel: &Selector, timeout: Duration) -> Result<Option<String>> {
        let (v, _) = self.wait_element(sel, Wait::Query { mode: "attached", scroll: false }, timeout).await?;
        Ok(v["text"].as_str().map(String::from))
    }

    pub async fn inner_text(&self, sel: &Selector, timeout: Duration) -> Result<String> {
        let (v, _) = self.wait_element(sel, Wait::Query { mode: "inner_text", scroll: false }, timeout).await?;
        Ok(v["text"].as_str().unwrap_or("").to_string())
    }

    pub async fn inner_html(&self, sel: &Selector, timeout: Duration) -> Result<String> {
        let (v, _) = self.wait_element(sel, Wait::Query { mode: "inner_html", scroll: false }, timeout).await?;
        Ok(v["html"].as_str().unwrap_or("").to_string())
    }

    pub async fn input_value(&self, sel: &Selector, timeout: Duration) -> Result<String> {
        let (v, _) = self.wait_element(sel, Wait::Query { mode: "input_value", scroll: false }, timeout).await?;
        Ok(v["value"].as_str().unwrap_or("").to_string())
    }

    pub async fn get_attribute(&self, sel: &Selector, name: &str, timeout: Duration) -> Result<Option<String>> {
        let (v, _) = self
            .wait_element_with(
                sel,
                Wait::Query { mode: "get_attribute", scroll: false },
                &json!({ "name": name }),
                timeout,
            )
            .await?;
        Ok(v["value"].as_str().map(String::from))
    }

    pub async fn is_enabled(&self, sel: &Selector, timeout: Duration) -> Result<bool> {
        let (v, _) = self.wait_element(sel, Wait::Query { mode: "is_enabled", scroll: false }, timeout).await?;
        Ok(v["enabled"].as_bool().unwrap_or(false))
    }

    pub async fn is_disabled(&self, sel: &Selector, timeout: Duration) -> Result<bool> {
        Ok(!self.is_enabled(sel, timeout).await?)
    }

    pub async fn is_checked(&self, sel: &Selector, timeout: Duration) -> Result<bool> {
        let (v, _) = self.wait_element(sel, Wait::Query { mode: "is_checked", scroll: false }, timeout).await?;
        Ok(v["checked"].as_bool().unwrap_or(false))
    }

    pub async fn is_editable(&self, sel: &Selector, timeout: Duration) -> Result<bool> {
        let (v, _) = self.wait_element(sel, Wait::Query { mode: "is_editable", scroll: false }, timeout).await?;
        Ok(v["editable"].as_bool().unwrap_or(false))
    }

    /// Single-shot evaluation without waiting; returns `None` when the frame does not exist yet.
    async fn query_once(&self, sel: &Selector, mode: &str, arg: &Value, timeout: Duration) -> Result<Option<Value>> {
        let start = Instant::now();
        for attempt in 0..2 {
            if self.browser.is_dead() {
                return Err(Error::BrowserDead { pid: self.browser_pid });
            }
            let remaining = timeout.saturating_sub(start.elapsed());
            if remaining.is_zero() {
                return Err(Error::timeout(sel.to_string(), timeout.as_millis() as u64));
            }
            let Ok((scope, _)) = self.scope(&sel.frames, false, remaining).await? else {
                if timeout.saturating_sub(start.elapsed()).is_zero() {
                    return Err(Error::timeout(sel.to_string(), timeout.as_millis() as u64));
                }
                return Ok(None);
            };
            let remaining = timeout.saturating_sub(start.elapsed());
            if remaining.is_zero() {
                return Err(Error::timeout(sel.to_string(), timeout.as_millis() as u64));
            }
            let v = match self.eval_engine(&scope, sel, mode, arg, true, remaining).await {
                Ok(r) => r["value"].clone(),
                Err(e) if attempt == 0 && !e.is_retryable() && is_stale_context_error(&e) => {
                    self.forget_world(&scope);
                    continue;
                }
                Err(e) => return Err(e),
            };
            if v["ok"].as_bool() == Some(true) {
                return Ok(Some(v));
            }
            if v["detached"].as_bool() == Some(true) {
                return Ok(None);
            }
            if v["fatal"].as_bool() == Some(true) {
                let reason = v["reason"].as_str().unwrap_or("unknown").to_string();
                return Err(Error::NotActionable { selector: sel.to_string(), reason });
            }
            return Ok(None);
        }
        Ok(None)
    }

    pub async fn count(&self, sel: &Selector, timeout: Duration) -> Result<usize> {
        Ok(self
            .query_once(sel, "count", &Value::Null, timeout)
            .await?
            .and_then(|v| v["count"].as_u64())
            .unwrap_or(0) as usize)
    }

    pub async fn all_text_contents(&self, sel: &Selector, timeout: Duration) -> Result<Vec<String>> {
        Ok(json_texts(self.query_once(sel, "all_text_contents", &Value::Null, timeout).await?))
    }

    pub async fn all_inner_texts(&self, sel: &Selector, timeout: Duration) -> Result<Vec<String>> {
        Ok(json_texts(self.query_once(sel, "all_inner_texts", &Value::Null, timeout).await?))
    }

    pub async fn is_visible(&self, sel: &Selector, timeout: Duration) -> Result<bool> {
        Ok(self
            .query_once(sel, "is_visible", &Value::Null, timeout)
            .await?
            .and_then(|v| v["visible"].as_bool())
            .unwrap_or(false))
    }

    pub async fn is_hidden(&self, sel: &Selector, timeout: Duration) -> Result<bool> {
        Ok(!self.is_visible(sel, timeout).await?)
    }

    async fn wait_element(&self, sel: &Selector, wait: Wait, timeout: Duration) -> Result<(Value, Scope)> {
        self.wait_element_with(sel, wait, &Value::Null, timeout).await
    }

    /// Polls until `wait` succeeds (and is unmoved / topmost for `Click` and `Box`);
    /// for `Click`, the returned `x`/`y` are in top-level page coordinates.
    async fn wait_element_with(&self, sel: &Selector, wait: Wait, arg: &Value, timeout: Duration) -> Result<(Value, Scope)> {
        let start = Instant::now();
        let mut last_pos: Option<(f64, f64)> = None;
        let mut last_box: Option<[f64; 4]> = None;
        let mut last_reason = String::from("never checked");
        let (mode, scroll, force) = match wait {
            Wait::Query { mode, scroll } => (mode, scroll, false),
            Wait::Click { force } => ("point", true, force),
            Wait::Box => ("box", true, false),
        };
        loop {
            if self.browser.is_dead() {
                return Err(Error::BrowserDead { pid: self.browser_pid });
            }
            let remaining = timeout.saturating_sub(start.elapsed());
            if remaining.is_zero() {
                // A timeout like any other, so callers (Python's `except TimeoutError`) see one.
                return Err(Error::timeout(format!("{sel} (last: {last_reason})"), timeout.as_millis() as u64));
            }
            let (scope, hops) = match self.scope(&sel.frames, scroll, remaining).await {
                Ok(Ok(found)) => found,
                Ok(Err(reason)) => {
                    last_reason = reason;
                    let rem = timeout.saturating_sub(start.elapsed());
                    if rem.is_zero() {
                        return Err(Error::timeout(format!("{sel} (last: {last_reason})"), timeout.as_millis() as u64));
                    }
                    sleep(Duration::from_millis(50).min(rem)).await;
                    continue;
                }
                Err(e) if e.is_timeout() => {
                    return Err(Error::timeout(format!("{sel} (last: {last_reason})"), timeout.as_millis() as u64));
                }
                Err(e) => return Err(e),
            };
            let remaining = timeout.saturating_sub(start.elapsed());
            if remaining.is_zero() {
                return Err(Error::timeout(format!("{sel} (last: {last_reason})"), timeout.as_millis() as u64));
            }
            let v = match self.eval_engine(&scope, sel, mode, arg, true, remaining).await {
                Ok(r) => r["value"].clone(),
                Err(e) if e.is_retryable() => return Err(e),
                Err(e) if e.is_timeout() => {
                    return Err(Error::timeout(format!("{sel} (last: {last_reason})"), timeout.as_millis() as u64));
                }
                // Mid-navigation the context can vanish; keep polling.
                Err(e) => {
                    self.forget_world(&scope);
                    json!({ "ok": false, "reason": e.to_string() })
                }
            };
            if v["ok"].as_bool() == Some(true) {
                if wait == Wait::Box {
                    let f = |k: &str| v[k].as_f64().unwrap_or(0.0);
                    let rect = scope.map.rect(f("x"), f("y"), f("width"), f("height"));
                    if last_box.is_some_and(|b| b.iter().zip(rect).all(|(a, b)| (a - b).abs() < 1.0)) {
                        return Ok((json!({ "x": rect[0], "y": rect[1], "width": rect[2], "height": rect[3] }), scope));
                    }
                    last_box = Some(rect);
                    last_reason = "still moving".into();
                    let rem = timeout.saturating_sub(start.elapsed());
                    if rem.is_zero() {
                        return Err(Error::timeout(format!("{sel} (last: {last_reason})"), timeout.as_millis() as u64));
                    }
                    sleep(Duration::from_millis(50).min(rem)).await;
                    continue;
                }
                if !matches!(wait, Wait::Click { .. }) {
                    return Ok((v, scope));
                }
                let (x, y) = scope.map.apply(v["x"].as_f64().unwrap_or(0.0), v["y"].as_f64().unwrap_or(0.0));
                if force {
                    return Ok((json!({ "x": x, "y": y }), scope));
                }
                let stable = last_pos.is_some_and(|(px, py)| (px - x).abs() < 1.0 && (py - y).abs() < 1.0);
                last_pos = Some((x, y));
                if !stable {
                    last_reason = "still moving".into();
                } else {
                    let remaining = timeout.saturating_sub(start.elapsed());
                    if remaining.is_zero() {
                        return Err(Error::timeout(format!("{sel} (last: {last_reason})"), timeout.as_millis() as u64));
                    }
                    match self.covered(&hops, x, y, remaining).await {
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
            let rem = timeout.saturating_sub(start.elapsed());
            if rem.is_zero() {
                return Err(Error::timeout(format!("{sel} (last: {last_reason})"), timeout.as_millis() as u64));
            }
            sleep(Duration::from_millis(50).min(rem)).await;
        }
    }

    pub async fn click(&self, sel: &Selector, timeout: Duration) -> Result<()> {
        self.click_with(sel, &ClickOptions::default(), timeout).await
    }

    pub async fn click_with(&self, sel: &Selector, opts: &ClickOptions, timeout: Duration) -> Result<()> {
        self.click_in_scope(sel, opts, timeout).await.map(|_| ())
    }

    pub async fn dblclick(&self, sel: &Selector, opts: &ClickOptions, timeout: Duration) -> Result<()> {
        let mut opts = opts.clone();
        opts.click_count = 2;
        self.click_with(sel, &opts, timeout).await
    }

    pub async fn hover(&self, sel: &Selector, opts: &HoverOptions, timeout: Duration) -> Result<()> {
        let arg = point_arg(opts.position, opts.force, false);
        let (pt, _) = self
            .wait_element_with(sel, Wait::Click { force: opts.force }, &arg, timeout)
            .await?;
        if opts.trial {
            return Ok(());
        }
        let x = pt["x"].as_f64().unwrap_or(0.0);
        let y = pt["y"].as_f64().unwrap_or(0.0);
        self.mouse_move_with(x, y, 1, opts.modifiers, timeout).await
    }

    pub async fn focus(&self, sel: &Selector, timeout: Duration) -> Result<()> {
        self.wait_element(sel, Wait::Query { mode: "focus", scroll: true }, timeout).await.map(|_| ())
    }

    pub async fn blur(&self, sel: &Selector, timeout: Duration) -> Result<()> {
        self.wait_element(sel, Wait::Query { mode: "blur", scroll: false }, timeout).await.map(|_| ())
    }

    pub async fn scroll_into_view_if_needed(&self, sel: &Selector, timeout: Duration) -> Result<()> {
        self.wait_element(sel, Wait::Query { mode: "scroll", scroll: true }, timeout).await.map(|_| ())
    }

    // shortcut: dispatches pointer/mouse drag events; pages relying on HTML5 DataTransfer dragstart need evaluate.
    pub async fn drag_and_drop(&self, source: &Selector, target: &Selector, force: bool, timeout: Duration) -> Result<()> {
        let arg = point_arg(None, force, false);
        let (src, _) = self.wait_element_with(source, Wait::Click { force }, &arg, timeout).await?;
        let sx = src["x"].as_f64().unwrap_or(0.0);
        let sy = src["y"].as_f64().unwrap_or(0.0);
        self.mouse_move(sx, sy, 1, timeout).await?;
        self.mouse_down(MouseButton::Left, 1, timeout).await?;
        let finish = async {
            let (dst, _) = self.wait_element_with(target, Wait::Click { force }, &arg, timeout).await?;
            let tx = dst["x"].as_f64().unwrap_or(0.0);
            let ty = dst["y"].as_f64().unwrap_or(0.0);
            self.mouse_move(tx, ty, 5, timeout).await
        }
        .await;
        let release = self.mouse_up(MouseButton::Left, 1, timeout).await;
        finish?;
        release
    }

    /// Clicks, and returns the scope the element lives in (for fill's follow-up steps).
    async fn click_in_scope(&self, sel: &Selector, opts: &ClickOptions, timeout: Duration) -> Result<Scope> {
        let arg = point_arg(opts.position, opts.force, true);
        let (pt, scope) = self
            .wait_element_with(sel, Wait::Click { force: opts.force }, &arg, timeout)
            .await?;
        if opts.trial {
            return Ok(scope);
        }
        let x = pt["x"].as_f64().unwrap_or(0.0);
        let y = pt["y"].as_f64().unwrap_or(0.0);
        self.dispatch_mouse_click(x, y, opts, timeout).await?;
        Ok(scope)
    }

    pub async fn fill(&self, sel: &Selector, value: &str, timeout: Duration) -> Result<()> {
        self.fill_with(sel, value, false, timeout).await
    }

    pub async fn fill_with(&self, sel: &Selector, value: &str, force: bool, timeout: Duration) -> Result<()> {
        let opts = ClickOptions { force, ..Default::default() };
        let scope = self.click_in_scope(sel, &opts, timeout).await?;
        // Both steps act on the clicked element (see query.js); any failure is an error, never
        // text typed into whatever else has focus.
        let step = |mode: &'static str| {
            let scope = &scope;
            async move {
                let r = self.eval_engine(scope, sel, mode, &Value::Null, true, timeout).await?;
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

    pub async fn press(&self, sel: &Selector, key: &str, delay: Option<Duration>, timeout: Duration) -> Result<()> {
        let (_, scope) = self
            .wait_element(sel, Wait::Query { mode: "focus", scroll: true }, timeout)
            .await?;
        self.keyboard_press_in(Some(&scope.session), key, delay, timeout).await
    }

    pub async fn press_sequentially(
        &self,
        sel: &Selector,
        text: &str,
        delay: Option<Duration>,
        timeout: Duration,
    ) -> Result<()> {
        let (_, scope) = self
            .wait_element(sel, Wait::Query { mode: "focus", scroll: true }, timeout)
            .await?;
        self.keyboard_type_in(Some(&scope.session), text, delay, timeout).await
    }

    pub async fn set_checked(&self, sel: &Selector, checked: bool, opts: &CheckOptions, timeout: Duration) -> Result<()> {
        let (st, _) = self
            .wait_element_with(
                sel,
                Wait::Query { mode: "check_state", scroll: true },
                &json!({ "want": checked }),
                timeout,
            )
            .await?;
        if st["checked"].as_bool() == Some(checked) {
            if opts.trial {
                let arg = point_arg(opts.position, opts.force, true);
                self.wait_element_with(sel, Wait::Click { force: opts.force }, &arg, timeout).await?;
            }
            return Ok(());
        }
        let click_opts = ClickOptions {
            position: opts.position,
            force: opts.force,
            trial: opts.trial,
            ..Default::default()
        };
        let scope = self.click_in_scope(sel, &click_opts, timeout).await?;
        if opts.trial {
            return Ok(());
        }
        let r = self
            .eval_engine(
                &scope,
                sel,
                "check_state",
                &json!({ "want": checked }),
                true,
                timeout,
            )
            .await?;
        if r["value"]["checked"].as_bool() == Some(checked) {
            Ok(())
        } else {
            Err(Error::NotActionable {
                selector: sel.to_string(),
                reason: "clicking the checkbox did not change its state".into(),
            })
        }
    }

    pub async fn select_option(
        &self,
        sel: &Selector,
        items: &[SelectEntry],
        force: bool,
        timeout: Duration,
    ) -> Result<Vec<String>> {
        let encoded: Vec<Value> = items.iter().map(SelectEntry::to_json).collect();
        let (v, _) = self
            .wait_element_with(
                sel,
                Wait::Query { mode: "select_options", scroll: true },
                &json!({ "items": encoded, "force": force }),
                timeout,
            )
            .await?;
        Ok(v["values"]
            .as_array()
            .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
            .unwrap_or_default())
    }

    pub async fn set_input_files(&self, sel: &Selector, files: &FileInput, timeout: Duration) -> Result<()> {
        if let FileInput::Paths(paths) = files {
            if !self.is_remote && !paths.is_empty() {
                let strs = canonicalize_local_paths(paths).await?;
                let (_, scope) = self
                    .wait_element_with(
                        sel,
                        Wait::Query { mode: "check_file_input", scroll: false },
                        &json!({ "count": strs.len() }),
                        timeout,
                    )
                    .await?;
                let found = self.eval_engine(&scope, sel, "element", &Value::Null, false, timeout).await?;
                let Some(isolated) = found["objectId"].as_str().filter(|_| found["subtype"] == "node") else {
                    return Err(Error::NotActionable {
                        selector: sel.to_string(),
                        reason: "element went away".into(),
                    });
                };
                let res = async {
                    let node = self
                        .session_call(&scope.session, "DOM.describeNode", json!({ "objectId": isolated }), timeout)
                        .await?;
                    let backend_id = node["node"]["backendNodeId"]
                        .as_i64()
                        .ok_or_else(|| Error::Other("missing backendNodeId".into()))?;
                    self.session_call(
                        &scope.session,
                        "DOM.setFileInputFiles",
                        json!({ "files": strs, "backendNodeId": backend_id }),
                        timeout,
                    )
                    .await
                    .map(|_| ())
                }
                .await;
                let _ = self
                    .session_call(&scope.session, "Runtime.releaseObject", json!({ "objectId": isolated }), timeout)
                    .await;
                return res;
            }
        }
        let encoded = encode_file_payloads(files).await?;
        self.wait_element_with(
            sel,
            Wait::Query { mode: "set_files", scroll: false },
            &json!({ "files": encoded }),
            timeout,
        )
        .await
        .map(|_| ())
    }

    pub async fn tap(&self, sel: &Selector, opts: &HoverOptions, timeout: Duration) -> Result<()> {
        let arg = point_arg(opts.position, opts.force, true);
        let (pt, _) = self
            .wait_element_with(sel, Wait::Click { force: opts.force }, &arg, timeout)
            .await?;
        if opts.trial {
            return Ok(());
        }
        let x = pt["x"].as_f64().unwrap_or(0.0);
        let y = pt["y"].as_f64().unwrap_or(0.0);
        self.touchscreen_tap_with(x, y, opts.modifiers, timeout).await
    }

    pub async fn mouse_move(&self, x: f64, y: f64, steps: u32, timeout: Duration) -> Result<()> {
        self.mouse_move_with(x, y, steps, 0, timeout).await
    }

    async fn mouse_move_with(&self, x: f64, y: f64, steps: u32, extra_mods: u8, timeout: Duration) -> Result<()> {
        let steps = steps.max(1);
        let (from_x, from_y, buttons) = {
            let st = self.mouse.lock().unwrap();
            (st.x, st.y, st.buttons)
        };
        let mods = self.keyboard.lock().unwrap().modifiers | extra_mods;
        for step in 1..=steps {
            let t = step as f64 / steps as f64;
            let cx = from_x + (x - from_x) * t;
            let cy = from_y + (y - from_y) * t;
            self.call(
                "Input.dispatchMouseEvent",
                json!({
                    "type": "mouseMoved",
                    "x": cx,
                    "y": cy,
                    "button": "none",
                    "buttons": buttons,
                    "modifiers": mods,
                }),
                timeout,
            )
            .await?;
            let mut st = self.mouse.lock().unwrap();
            st.x = cx;
            st.y = cy;
        }
        Ok(())
    }

    pub async fn mouse_down(&self, button: MouseButton, click_count: u32, timeout: Duration) -> Result<()> {
        self.mouse_button("mousePressed", button, click_count, 0, timeout).await
    }

    pub async fn mouse_up(&self, button: MouseButton, click_count: u32, timeout: Duration) -> Result<()> {
        self.mouse_button("mouseReleased", button, click_count, 0, timeout).await
    }

    async fn mouse_button(
        &self,
        ty: &'static str,
        button: MouseButton,
        click_count: u32,
        extra_mods: u8,
        timeout: Duration,
    ) -> Result<()> {
        let (x, y, buttons) = {
            let mut st = self.mouse.lock().unwrap();
            if ty == "mousePressed" {
                st.buttons |= button.bit();
            } else {
                st.buttons &= !button.bit();
            }
            (st.x, st.y, st.buttons)
        };
        let mods = self.keyboard.lock().unwrap().modifiers | extra_mods;
        self.call(
            "Input.dispatchMouseEvent",
            json!({
                "type": ty,
                "x": x,
                "y": y,
                "button": button.as_cdp_str(),
                "buttons": buttons,
                "clickCount": click_count.max(1),
                "modifiers": mods,
            }),
            timeout,
        )
        .await
        .map(|_| ())
    }

    async fn dispatch_mouse_click(&self, x: f64, y: f64, opts: &ClickOptions, timeout: Duration) -> Result<()> {
        self.mouse_move_with(x, y, 1, opts.modifiers, timeout).await?;
        let count = opts.click_count.max(1);
        for n in 1..=count {
            self.mouse_button("mousePressed", opts.button, n, opts.modifiers, timeout).await?;
            if let Some(d) = opts.delay.filter(|d| !d.is_zero()) {
                sleep(d).await;
            }
            self.mouse_button("mouseReleased", opts.button, n, opts.modifiers, timeout).await?;
        }
        Ok(())
    }

    pub async fn mouse_click(
        &self,
        x: f64,
        y: f64,
        button: MouseButton,
        click_count: u32,
        delay: Option<Duration>,
        timeout: Duration,
    ) -> Result<()> {
        let opts = ClickOptions { button, click_count, delay, ..Default::default() };
        self.dispatch_mouse_click(x, y, &opts, timeout).await
    }

    pub async fn mouse_dblclick(
        &self,
        x: f64,
        y: f64,
        button: MouseButton,
        delay: Option<Duration>,
        timeout: Duration,
    ) -> Result<()> {
        self.mouse_click(x, y, button, 2, delay, timeout).await
    }

    pub async fn mouse_wheel(&self, delta_x: f64, delta_y: f64, timeout: Duration) -> Result<()> {
        let (x, y) = {
            let st = self.mouse.lock().unwrap();
            (st.x, st.y)
        };
        let mods = self.keyboard.lock().unwrap().modifiers;
        self.call(
            "Input.dispatchMouseEvent",
            json!({
                "type": "mouseWheel",
                "x": x,
                "y": y,
                "deltaX": delta_x,
                "deltaY": delta_y,
                "modifiers": mods,
            }),
            timeout,
        )
        .await
        .map(|_| ())
    }

    pub async fn touchscreen_tap(&self, x: f64, y: f64, timeout: Duration) -> Result<()> {
        self.touchscreen_tap_with(x, y, 0, timeout).await
    }

    async fn touchscreen_tap_with(&self, x: f64, y: f64, extra_mods: u8, timeout: Duration) -> Result<()> {
        let mods = self.keyboard.lock().unwrap().modifiers | extra_mods;
        let rx = x.round();
        let ry = y.round();
        self.call(
            "Input.dispatchTouchEvent",
            json!({
                "type": "touchStart",
                "touchPoints": [{ "x": rx, "y": ry }],
                "modifiers": mods,
            }),
            timeout,
        )
        .await?;
        self.call(
            "Input.dispatchTouchEvent",
            json!({
                "type": "touchEnd",
                "touchPoints": [],
                "modifiers": mods,
            }),
            timeout,
        )
        .await
        .map(|_| ())
    }

    pub async fn keyboard_down(&self, key: &str, timeout: Duration) -> Result<()> {
        self.keyboard_down_in(None, key, timeout).await
    }

    pub async fn keyboard_up(&self, key: &str, timeout: Duration) -> Result<()> {
        self.keyboard_up_in(None, key, timeout).await
    }

    pub async fn keyboard_press(&self, key: &str, delay: Option<Duration>, timeout: Duration) -> Result<()> {
        self.keyboard_press_in(None, key, delay, timeout).await
    }

    pub async fn keyboard_type(&self, text: &str, delay: Option<Duration>, timeout: Duration) -> Result<()> {
        self.keyboard_type_in(None, text, delay, timeout).await
    }

    pub async fn keyboard_insert_text(&self, text: &str, timeout: Duration) -> Result<()> {
        self.call("Input.insertText", json!({ "text": text }), timeout)
            .await
            .map(|_| ())
    }

    async fn keyboard_down_in(&self, session: Option<&str>, key: &str, timeout: Duration) -> Result<()> {
        let spec = resolve_key(key)?;
        let mods = {
            let mut kb = self.keyboard.lock().unwrap();
            if let Some(bit) = spec.is_modifier {
                kb.modifiers |= bit;
            }
            kb.modifiers
        };
        dispatch_key_event(&self.browser, session.unwrap_or(self.sid()), true, &spec, mods, timeout).await
    }

    async fn keyboard_up_in(&self, session: Option<&str>, key: &str, timeout: Duration) -> Result<()> {
        let spec = resolve_key(key)?;
        let mods = {
            let mut kb = self.keyboard.lock().unwrap();
            if let Some(bit) = spec.is_modifier {
                kb.modifiers &= !bit;
            }
            kb.modifiers
        };
        dispatch_key_event(&self.browser, session.unwrap_or(self.sid()), false, &spec, mods, timeout).await
    }

    async fn keyboard_press_in(
        &self,
        session: Option<&str>,
        combo: &str,
        delay: Option<Duration>,
        timeout: Duration,
    ) -> Result<()> {
        let (mods, main) = parse_chord(combo)?;
        for m in &mods {
            self.keyboard_down_in(session, m, timeout).await?;
        }
        let body = async {
            self.keyboard_down_in(session, main, timeout).await?;
            if let Some(d) = delay.filter(|d| !d.is_zero()) {
                sleep(d).await;
            }
            self.keyboard_up_in(session, main, timeout).await
        }
        .await;
        for m in mods.iter().rev() {
            let _ = self.keyboard_up_in(session, m, timeout).await;
        }
        body
    }

    async fn keyboard_type_in(
        &self,
        session: Option<&str>,
        text: &str,
        delay: Option<Duration>,
        timeout: Duration,
    ) -> Result<()> {
        let sid = session.unwrap_or(self.sid());
        for ch in text.chars() {
            if ch == '\r' || ch == '\n' {
                self.keyboard_press_in(session, "Enter", None, timeout).await?;
            } else if ch.is_ascii() && !ch.is_ascii_control() {
                let s = ch.to_string();
                self.keyboard_down_in(session, &s, timeout).await?;
                self.keyboard_up_in(session, &s, timeout).await?;
            } else {
                self.session_call(sid, "Input.insertText", json!({ "text": ch.to_string() }), timeout)
                    .await?;
            }
            if let Some(d) = delay.filter(|d| !d.is_zero()) {
                sleep(d).await;
            }
        }
        Ok(())
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
        let page_setup = self.page_setups.lock().await.get(&self.target_id.0).cloned();
        let fetch = {
            let mut setups = self.setups.lock().await;
            let setup = setups.entry(self.context_id.0.clone()).or_default();
            setup.types = types.to_vec();
            fetch_params_combined(Some(setup), page_setup.as_ref())
        };
        if let Some(params) = fetch {
            self.call("Fetch.enable", params, timeout).await?;
        }
        Ok(())
    }
}

/// Per-context setup, applied to each of the context's sessions as they attach (iframes in
/// other processes, popups, tabs): resource blocking, proxy credentials, emulation, init scripts.
#[derive(Debug, Clone, Default)]
pub(crate) struct ContextSetup {
    /// Where paused requests go when the job intercepts them (`context.route`).
    pub(crate) routes: Option<tokio::sync::mpsc::UnboundedSender<InterceptedRequest>>,
    /// Where invocations from `context.expose_binding` / `expose_function` go.
    pub(crate) bindings: Option<tokio::sync::mpsc::UnboundedSender<crate::events::BindingCall>>,
    pub(crate) bindings_enabled: bool,
    pub(crate) exposed_bindings: std::collections::HashSet<String>,
    pub(crate) emulation: Option<Emulation>,
    pub(crate) init_scripts: Vec<String>,
    pub(crate) types: Vec<ResourceType>,
    pub(crate) urls: Vec<String>,
    pub(crate) proxy: Option<Proxy>,
    pub(crate) proxy_auth: Option<(String, String)>,
    pub(crate) http_auth: Option<(String, String)>,
    /// Requests already given credentials: a second challenge means they were rejected.
    pub(crate) answered: std::collections::HashSet<String>,
}

impl ContextSetup {
    pub(crate) fn http_defaults(&self, base_url: Option<String>) -> crate::http_client::HttpContextDefaults {
        let em = self.emulation.as_ref();
        crate::http_client::HttpContextDefaults {
            base_url,
            extra_http_headers: em.map(|e| e.extra_http_headers.clone()).unwrap_or_default(),
            user_agent: em.and_then(|e| e.user_agent.clone()),
            http_credentials: self.http_auth.clone(),
            proxy: self.proxy.clone(),
        }
    }
}

pub(crate) fn binding_shims(name: &str) -> (String, String) {
    let name_json = serde_json::to_string(name).unwrap();
    let install = format!(
        r#"const name = {name_json};
        const install = (w) => {{
            const root = w.__fwBindings || (w.__fwBindings = {{ seq: 0, pending: new w.Map() }});
            if (typeof w.__fwBindingResolve !== 'function') {{
                w.__fwBindingResolve = (seq, ok, value, errMsg) => {{
                    const entry = root.pending.get(seq);
                    if (!entry) return;
                    root.pending.delete(seq);
                    if (ok) entry.resolve(value);
                    else entry.reject(new w.Error(errMsg || 'exposed function failed'));
                }};
            }}
            w[name] = (...args) => new w.Promise((resolve, reject) => {{
                const seq = ++root.seq;
                root.pending.set(seq, {{ resolve, reject }});
                try {{
                    w.__fluxwright_binding(JSON.stringify({{ name, seq, args }}));
                }} catch (e) {{
                    root.pending.delete(seq);
                    reject(e);
                }}
            }});
        }};"#
    );
    let shim = format!("(() => {{ {install} install(globalThis); }})()");
    let eval_shim = format!(
        "(() => {{ {install} const walk = (w) => {{ install(w); for (let i = 0; i < w.frames.length; i++) {{ try {{ if (w.frames[i].document) walk(w.frames[i]); }} catch {{}} }} }}; walk(globalThis); }})()"
    );
    (shim, eval_shim)
}

/// `Fetch.enable` for a context/page pair, or `None` when it needs no Fetch at all.
pub(crate) fn fetch_params_combined(setup: Option<&ContextSetup>, page: Option<&PageSetup>) -> Option<Value> {
    let auth = setup.is_some_and(|s| s.proxy_auth.is_some() || s.http_auth.is_some());
    let has_routes = setup.is_some_and(|s| s.routes.is_some()) || page.is_some_and(|p| p.routes.is_some());
    let patterns: Vec<Value> = if auth || has_routes {
        vec![json!({ "urlPattern": "*", "requestStage": "Request" })]
    } else {
        setup
            .map(|s| {
                s.types
                    .iter()
                    .map(|t| json!({ "urlPattern": "*", "resourceType": t.as_str(), "requestStage": "Request" }))
                    .collect()
            })
            .unwrap_or_default()
    };
    if patterns.is_empty() {
        return None;
    }
    Some(json!({ "patterns": patterns, "handleAuthRequests": auth }))
}

fn point_arg(position: Option<(f64, f64)>, force: bool, require_enabled: bool) -> Value {
    json!({
        "position": position.map(|(x, y)| json!({ "x": x, "y": y })),
        "force": force,
        "requireEnabled": require_enabled,
    })
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

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "image" => Some(ResourceType::Image),
            "font" => Some(ResourceType::Font),
            "media" => Some(ResourceType::Media),
            "stylesheet" => Some(ResourceType::Stylesheet),
            "script" => Some(ResourceType::Script),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_matches_playwright_patterns() {
        assert!(url_glob_matches("**/done", "http://127.0.0.1:8000/a/b/done"));
        assert!(url_glob_matches("**/done", "http://127.0.0.1/done"));
        assert!(!url_glob_matches("**/done", "http://127.0.0.1/done/extra"));
        assert!(url_glob_matches("https://*.example.com/*", "https://app.example.com/home"));
        assert!(!url_glob_matches("https://*.example.com/*", "https://app/sub.example.com/home"));
        assert!(!url_glob_matches("https://*.example.com/*", "https://app.example.com/a/b"));
        assert!(url_glob_matches("**/{foo,bar}*", "https://host/bar?x=1"));
        assert!(!url_glob_matches("**/{foo,bar}*", "https://host/baz"));
    }

    #[test]
    fn js_truthiness_matches_ecmascript() {
        assert!(!is_js_truthy(&json!({ "type": "undefined" })));
        assert!(!is_js_truthy(&json!({ "type": "boolean", "value": false })));
        assert!(is_js_truthy(&json!({ "type": "boolean", "value": true })));
        assert!(!is_js_truthy(&json!({ "type": "string", "value": "" })));
        assert!(is_js_truthy(&json!({ "type": "string", "value": "0" })));
        assert!(!is_js_truthy(&json!({ "type": "number", "value": 0 })));
        assert!(!is_js_truthy(&json!({ "type": "number", "unserializableValue": "NaN" })));
        assert!(is_js_truthy(&json!({ "type": "number", "unserializableValue": "Infinity" })));
        assert!(!is_js_truthy(&json!({ "type": "bigint", "unserializableValue": "0n" })));
        assert!(is_js_truthy(&json!({ "type": "bigint", "unserializableValue": "1n" })));
        assert!(!is_js_truthy(&json!({ "type": "object", "subtype": "null", "value": null })));
        assert!(is_js_truthy(&json!({ "type": "object", "objectId": "obj-1" })));
    }

    fn mock_page(conn: Arc<Connection>) -> CdpPage {
        CdpPage {
            browser: conn,
            browser_pid: 0,
            browser_id: BrowserId(uuid::Uuid::new_v4()),
            headless: true,
            is_remote: false,
            context_id: ContextId("ctx".into()),
            target_id: TargetId("main".into()),
            opener_id: None,
            session_id: SessionId("sid".into()),
            targets: Default::default(),
            worlds: Default::default(),
            setups: Default::default(),
            page_setups: Default::default(),
            frames: Default::default(),
            workers: Default::default(),
            closed: Default::default(),
            mouse: Default::default(),
            keyboard: Default::default(),
            url: Arc::new(tokio::sync::watch::Sender::new("about:blank".into())),
            net: Default::default(),
            ax_refs: Default::default(),
            last_snapshot: Default::default(),
            early_log_events: Default::default(),
        }
    }

    #[tokio::test]
    async fn is_closed_reports_dead_browser_connection() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                let _ = tokio_tungstenite::accept_async(stream).await;
            }
        });
        let conn = Connection::connect(&format!("ws://{addr}")).await.unwrap();
        let page = mock_page(conn.clone());
        assert!(!page.is_closed());
        conn.mark_dead();
        assert!(page.is_closed());
    }

    #[tokio::test]
    async fn query_once_recovers_from_stale_isolated_world() {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            while let Some(Ok(Message::Text(text))) = ws.next().await {
                let req: Value = serde_json::from_str(&text).unwrap();
                let id = req["id"].clone();
                let method = req["method"].as_str().unwrap_or("");
                let reply = match method {
                    "Page.createIsolatedWorld" => {
                        json!({ "id": id, "result": { "executionContextId": 22 } })
                    }
                    "Runtime.evaluate" => {
                        if req["params"]["contextId"].as_i64() == Some(11) {
                            json!({
                                "id": id,
                                "error": { "code": -32000, "message": "Cannot find context with specified id" }
                            })
                        } else {
                            json!({
                                "id": id,
                                "result": { "result": { "value": { "ok": true, "count": 3 } } }
                            })
                        }
                    }
                    _ => json!({ "id": id, "result": {} }),
                };
                if ws.send(Message::Text(reply.to_string().into())).await.is_err() {
                    break;
                }
            }
        });

        let conn = Connection::connect(&format!("ws://{addr}")).await.unwrap();
        let page = mock_page(conn);
        // Seed a stale isolated world id (11) as would happen across navigation.
        page.worlds.lock().unwrap().insert("main".into(), 11);

        let count = page
            .count(&Selector::new("h1"), Duration::from_secs(5))
            .await
            .expect("query_once should drop stale world 11 and retry in world 22");
        assert_eq!(count, 3);
        assert_eq!(page.worlds.lock().unwrap().get("main").copied(), Some(22));
    }

    #[tokio::test]
    async fn wait_element_and_selector_state_enforce_remaining_timeout_budget() {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let mut eval_calls = 0usize;
            while let Some(Ok(Message::Text(text))) = ws.next().await {
                let req: Value = serde_json::from_str(&text).unwrap();
                let id = req["id"].clone();
                let method = req["method"].as_str().unwrap_or("");
                let reply = match method {
                    "Page.createIsolatedWorld" => {
                        json!({ "id": id, "result": { "executionContextId": 1 } })
                    }
                    "Runtime.evaluate" => {
                        eval_calls += 1;
                        if eval_calls == 1 {
                            json!({
                                "id": id,
                                "result": { "result": { "value": { "ok": false, "reason": "no element matches selector" } } }
                            })
                        } else {
                            // Stall the second poll well past the overall 250ms budget.
                            tokio::time::sleep(Duration::from_secs(5)).await;
                            json!({
                                "id": id,
                                "result": { "result": { "value": { "ok": false, "reason": "late" } } }
                            })
                        }
                    }
                    _ => json!({ "id": id, "result": {} }),
                };
                if ws.send(Message::Text(reply.to_string().into())).await.is_err() {
                    break;
                }
            }
        });

        let conn = Connection::connect(&format!("ws://{addr}")).await.unwrap();
        let page = mock_page(conn);
        let started = Instant::now();
        let err = page
            .wait_for_selector(&Selector::new("#never"), Duration::from_millis(250))
            .await
            .unwrap_err();
        let elapsed = started.elapsed();
        assert!(err.is_timeout(), "expected timeout, got: {err}");
        assert!(
            elapsed < Duration::from_millis(900),
            "timeout budget leaked across poll calls: {elapsed:?}"
        );
    }
}
