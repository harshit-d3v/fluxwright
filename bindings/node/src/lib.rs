use std::collections::HashMap;

use napi::bindgen_prelude::*;
use napi::threadsafe_function::{ThreadsafeFunction, ThreadsafeFunctionCallMode};
use napi_derive::napi;
use std::sync::{Arc, LazyLock};
use tokio::runtime::Runtime;
use tokio::sync::{Mutex, RwLock};

static RT: LazyLock<Runtime> = LazyLock::new(|| Runtime::new().expect("tokio"));

#[napi]
pub struct Browser {
    inner: fluxwright::BrowserEngine,
}

/// The page's lease, read-locked by every page call, so calls run side by side (a download wait
/// and the click that starts it), and write-locked by `close`, which raises `closing` first: calls
/// still running give up at once instead of holding the lease until their own timeouts.
struct PageSlot {
    page: RwLock<Option<fluxwright::PageLease>>,
    closing: tokio::sync::watch::Sender<bool>,
}

type LeaseSlot = Arc<PageSlot>;

#[napi]
pub struct Page {
    lease: LeaseSlot,
    /// The JavaScript route dispatcher, once `page.route` was called.
    routes: Arc<std::sync::Mutex<Option<Arc<Callback<InterceptedRoute>>>>>,
}

/// A thread-safe JavaScript callback (errors are not passed to it) that does not keep Node
/// running on its own.
type Callback<T> = ThreadsafeFunction<T, (), T, Status, false, true>;

fn callback<T: ToNapiValue + 'static>(f: Function<T, ()>) -> Result<Callback<T>> {
    f.build_threadsafe_function().callee_handled::<false>().weak::<true>().build()
}

/// A paused request handed to `page.route` handlers; `addon.js` wraps it in Playwright's
/// `Route` and `Request`.
#[napi]
pub struct InterceptedRoute {
    request: Arc<Mutex<Option<fluxwright::InterceptedRequest>>>,
    url: String,
    method: String,
    headers: HashMap<String, String>,
    post_data: Option<String>,
    resource_type: String,
}

#[napi(object)]
pub struct FulfillOptions {
    pub status: Option<u32>,
    pub headers: Option<HashMap<String, String>>,
    pub body: Option<Either<String, Buffer>>,
}

#[napi(object)]
pub struct ContinueOptions {
    pub url: Option<String>,
    pub method: Option<String>,
    /// Replaces all headers.
    pub headers: Option<HashMap<String, String>>,
    pub post_data: Option<Either<String, Buffer>>,
}

fn bytes(b: Either<String, Buffer>) -> Vec<u8> {
    match b {
        Either::A(text) => text.into_bytes(),
        Either::B(buf) => buf.to_vec(),
    }
}

impl InterceptedRoute {
    fn new(req: fluxwright::InterceptedRequest) -> Self {
        InterceptedRoute {
            url: req.url.clone(),
            method: req.method.clone(),
            headers: req.headers.iter().cloned().collect(),
            post_data: req.post_data.clone(),
            resource_type: req.resource_type.clone(),
            request: Arc::new(Mutex::new(Some(req))),
        }
    }

    /// Takes the request to answer it; a second answer fails, as in Playwright.
    async fn take(slot: Arc<Mutex<Option<fluxwright::InterceptedRequest>>>) -> std::result::Result<fluxwright::InterceptedRequest, String> {
        slot.lock().await.take().ok_or_else(|| "route is already handled".to_string())
    }
}

#[napi]
impl InterceptedRoute {
    #[napi(getter)]
    pub fn url(&self) -> String {
        self.url.clone()
    }

    #[napi(getter)]
    pub fn method(&self) -> String {
        self.method.clone()
    }

    #[napi(getter)]
    pub fn headers(&self) -> HashMap<String, String> {
        self.headers.clone()
    }

    #[napi(getter)]
    pub fn post_data(&self) -> Option<String> {
        self.post_data.clone()
    }

    #[napi(getter)]
    pub fn resource_type(&self) -> String {
        self.resource_type.clone()
    }

    #[napi]
    pub async fn fulfill(&self, options: Option<FulfillOptions>) -> Result<()> {
        let o = options.unwrap_or(FulfillOptions { status: None, headers: None, body: None });
        if let Some(status) = o.status.filter(|s| !(100..=599).contains(s)) {
            return Err(Error::from_reason(format!("status must be between 100 and 599, not {status}")));
        }
        let f = fluxwright::Fulfill {
            status: o.status.map(|s| s as u16),
            headers: o.headers.unwrap_or_default().into_iter().collect(),
            body: o.body.map(bytes).unwrap_or_default(),
        };
        let slot = self.request.clone();
        RT.spawn(async move { Self::take(slot).await?.fulfill(f).await.map_err(|e| e.to_string()) })
            .await
            .map_err(|e| Error::from_reason(e.to_string()))?
            .map_err(Error::from_reason)
    }

    #[napi(js_name = "continue")]
    pub async fn continue_request(&self, options: Option<ContinueOptions>) -> Result<()> {
        let o = options.unwrap_or(ContinueOptions { url: None, method: None, headers: None, post_data: None });
        let overrides = fluxwright::Overrides {
            url: o.url,
            method: o.method,
            headers: o.headers.map(|h| h.into_iter().collect()),
            post_data: o.post_data.map(bytes),
        };
        let slot = self.request.clone();
        RT.spawn(async move { Self::take(slot).await?.continue_with(overrides).await.map_err(|e| e.to_string()) })
            .await
            .map_err(|e| Error::from_reason(e.to_string()))?
            .map_err(Error::from_reason)
    }

    /// `errorCode` as in Playwright: `failed` (default), `aborted`, `blockedbyclient`, ...
    #[napi]
    pub async fn abort(&self, error_code: Option<String>) -> Result<()> {
        let code = error_code.unwrap_or_else(|| "failed".into());
        let slot = self.request.clone();
        RT.spawn(async move { Self::take(slot).await?.abort(&code).await.map_err(|e| e.to_string()) })
            .await
            .map_err(|e| Error::from_reason(e.to_string()))?
            .map_err(Error::from_reason)
    }
}

/// A request, as network events hand it over; `addon.js` wraps it in Playwright's `Request`.
#[napi(object)]
pub struct NetworkRequest {
    /// Chrome's request id: the same for every event of one request.
    pub id: String,
    pub url: String,
    pub method: String,
    /// Names lower-cased, as Playwright reports them.
    pub headers: HashMap<String, String>,
    pub post_data: Option<String>,
    pub resource_type: String,
}

impl From<fluxwright::NetRequest> for NetworkRequest {
    fn from(r: fluxwright::NetRequest) -> Self {
        NetworkRequest {
            id: r.id,
            url: r.url,
            method: r.method,
            headers: r.headers.into_iter().collect(),
            post_data: r.post_data,
            resource_type: r.resource_type,
        }
    }
}

/// A response; `addon.js` wraps it in Playwright's `Response`.
#[napi]
pub struct NetworkResponse {
    inner: fluxwright::NetResponse,
}

#[napi]
impl NetworkResponse {
    #[napi(getter)]
    pub fn url(&self) -> String {
        self.inner.url.clone()
    }

    #[napi(getter)]
    pub fn status(&self) -> u32 {
        self.inner.status as u32
    }

    #[napi(getter)]
    pub fn status_text(&self) -> String {
        self.inner.status_text.clone()
    }

    #[napi(getter)]
    pub fn headers(&self) -> HashMap<String, String> {
        self.inner.headers.iter().cloned().collect()
    }

    #[napi(getter)]
    pub fn request(&self) -> NetworkRequest {
        self.inner.request.clone().into()
    }

    /// The body, once the response finished loading, within `timeoutMs` (default 30 s; 0 waits
    /// as long as it takes).
    #[napi]
    pub async fn body(&self, timeout_ms: Option<u32>) -> Result<Buffer> {
        let inner = self.inner.clone();
        let timeout = queue_wait(Some(timeout_ms.unwrap_or(30_000)));
        let body = RT
            .spawn(async move { inner.body(timeout).await.map_err(|e| e.to_string()) })
            .await
            .map_err(|e| Error::from_reason(e.to_string()))?
            .map_err(Error::from_reason)?;
        Ok(body.into())
    }
}

/// One network event, for `page.on`.
#[napi(object, object_from_js = false)]
pub struct NetworkEvent {
    /// `request`, `response`, `requestfinished` or `requestfailed`.
    pub event: String,
    pub request: NetworkRequest,
    pub response: Option<NetworkResponse>,
    /// Why a failed request failed (`net::ERR_FAILED`, ...).
    pub failure: Option<String>,
}

impl From<fluxwright::NetEvent> for NetworkEvent {
    fn from(e: fluxwright::NetEvent) -> Self {
        let (event, request, response, failure) = match e {
            fluxwright::NetEvent::Request(r) => ("request", r, None, None),
            fluxwright::NetEvent::Response(r) => ("response", r.request.clone(), Some(NetworkResponse { inner: r }), None),
            fluxwright::NetEvent::Finished(r) => ("requestfinished", r, None, None),
            fluxwright::NetEvent::Failed(r, why) => ("requestfailed", r, None, Some(why)),
        };
        NetworkEvent { event: event.into(), request: request.into(), response, failure }
    }
}

/// A dialog waiting for an answer; `addon.js` wraps it in Playwright's `Dialog`. One dropped
/// unanswered is dismissed (`beforeunload` accepted).
#[napi]
pub struct PendingDialog {
    inner: Arc<Mutex<Option<fluxwright::Dialog>>>,
    kind: String,
    message: String,
    default_value: String,
}

#[napi]
impl PendingDialog {
    /// `alert`, `confirm`, `prompt` or `beforeunload`.
    #[napi(getter, js_name = "type")]
    pub fn kind(&self) -> String {
        self.kind.clone()
    }

    #[napi(getter)]
    pub fn message(&self) -> String {
        self.message.clone()
    }

    #[napi(getter)]
    pub fn default_value(&self) -> String {
        self.default_value.clone()
    }

    /// `promptText` is what a `prompt` returns.
    #[napi]
    pub async fn accept(&self, prompt_text: Option<String>) -> Result<()> {
        let slot = self.inner.clone();
        RT.spawn(async move { Self::take(slot).await?.accept(prompt_text).await.map_err(|e| e.to_string()) })
            .await
            .map_err(|e| Error::from_reason(e.to_string()))?
            .map_err(Error::from_reason)
    }

    #[napi]
    pub async fn dismiss(&self) -> Result<()> {
        let slot = self.inner.clone();
        RT.spawn(async move { Self::take(slot).await?.dismiss().await.map_err(|e| e.to_string()) })
            .await
            .map_err(|e| Error::from_reason(e.to_string()))?
            .map_err(Error::from_reason)
    }
}

impl PendingDialog {
    fn new(d: fluxwright::Dialog) -> Self {
        PendingDialog {
            kind: d.kind.clone(),
            message: d.message.clone(),
            default_value: d.default_value.clone(),
            inner: Arc::new(Mutex::new(Some(d))),
        }
    }

    /// Takes the dialog to answer it; a second answer fails, as in Playwright.
    async fn take(slot: Arc<Mutex<Option<fluxwright::Dialog>>>) -> std::result::Result<fluxwright::Dialog, String> {
        slot.lock().await.take().ok_or_else(|| "dialog is already handled".to_string())
    }
}

/// One console message or uncaught error, for `page.on`.
#[napi(object)]
pub struct LogEvent {
    /// `console` or `pageerror`.
    pub event: String,
    pub message: Option<ConsoleMessage>,
    pub error: Option<PageErrorInfo>,
}

#[napi(object)]
pub struct DownloadInfo {
    pub url: String,
    pub suggested_filename: String,
    /// Deleted when the page closes.
    pub path: String,
}

/// Runs `f` against the page on the engine's runtime; fails with "page closed" after close(), and
/// at once when close() is called while it runs.
async fn with_page<T, F>(slot: LeaseSlot, f: F) -> Result<T>
where
    T: Send + 'static,
    F: for<'a> FnOnce(
            &'a fluxwright::PageLease,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = fluxwright::Result<T>> + Send + 'a>,
        > + Send
        + 'static,
{
    with_page_in(slot, None, f).await
}

/// Playwright's `timeout` option in milliseconds: 0 means no timeout, and anything that is
/// not a time (negative, NaN) leaves the default in place.
fn call_timeout(ms: Option<f64>) -> Option<std::time::Duration> {
    const YEAR: std::time::Duration = std::time::Duration::from_secs(365 * 24 * 3600);
    match ms {
        Some(ms) if ms == 0.0 => Some(YEAR),
        Some(ms) if ms > 0.0 && ms.is_finite() => {
            Some(std::time::Duration::try_from_secs_f64(ms / 1000.0).unwrap_or(YEAR).min(YEAR))
        }
        _ => None,
    }
}

/// `with_page` with the call's own timeout in place of the page's and the engine's.
async fn with_page_in<T, F>(slot: LeaseSlot, timeout: Option<std::time::Duration>, f: F) -> Result<T>
where
    T: Send + 'static,
    F: for<'a> FnOnce(
            &'a fluxwright::PageLease,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = fluxwright::Result<T>> + Send + 'a>,
        > + Send
        + 'static,
{
    RT.spawn(async move {
        let mut closing = slot.closing.subscribe();
        let g = slot.page.read().await;
        let Some(p) = g.as_ref() else {
            return Err("page closed".to_string());
        };
        tokio::select! {
            r = fluxwright::with_timeout(timeout, f(p)) => r.map_err(|e| e.to_string()),
            _ = async { let _ = closing.wait_for(|closed| *closed).await; } => Err("page closed".to_string()),
        }
    })
    .await
    .map_err(|e| Error::from_reason(e.to_string()))?
    .map_err(Error::from_reason)
}

#[napi(object)]
pub struct GetByTextOptions {
    /// Default false: case-insensitive substring.
    pub exact: Option<bool>,
}

#[napi(object)]
pub struct GetByRoleOptions {
    /// Accessible name. Default: case-insensitive substring.
    pub name: Option<String>,
    pub exact: Option<bool>,
}

/// Lazy, like Playwright's: every action runs the query again.
#[napi]
pub struct Locator {
    lease: LeaseSlot,
    selector: fluxwright::Selector,
    /// From `_withTimeout`: the `timeout` option of the call this locator was made for.
    timeout: Option<std::time::Duration>,
}

#[napi]
impl Locator {
    /// This locator with `timeout` (milliseconds; 0 for none) for its actions.
    #[napi(js_name = "_withTimeout")]
    pub fn with_timeout(&self, timeout: f64) -> Locator {
        Locator { lease: self.lease.clone(), selector: self.selector.clone(), timeout: call_timeout(Some(timeout)) }
    }

    #[napi]
    pub async fn click(&self) -> Result<()> {
        let sel = self.selector.clone();
        with_page_in(self.lease.clone(), self.timeout, move |p| Box::pin(async move { p.click(&sel).await })).await
    }

    #[napi]
    pub async fn fill(&self, value: String) -> Result<()> {
        let sel = self.selector.clone();
        with_page_in(self.lease.clone(), self.timeout, move |p| Box::pin(async move { p.fill(&sel, &value).await })).await
    }

    /// Waits until `state`: attached, detached, visible (the default) or hidden.
    #[napi]
    pub async fn wait_for(&self, state: Option<String>) -> Result<()> {
        let sel = self.selector.clone();
        let state = match state {
            Some(s) => s.parse::<fluxwright::WaitState>().map_err(|e| Error::from_reason(e.to_string()))?,
            None => Default::default(),
        };
        with_page_in(self.lease.clone(), self.timeout, move |p| {
            Box::pin(async move { p.wait_for_selector_state(&sel, state).await })
        })
        .await
    }

    #[napi]
    pub async fn text_content(&self) -> Result<Option<String>> {
        let sel = self.selector.clone();
        with_page_in(self.lease.clone(), self.timeout, move |p| Box::pin(async move { p.text_content(&sel).await })).await
    }

    /// The `index`th match (0-based; negative counts from the end).
    #[napi]
    pub fn nth(&self, index: i32) -> Locator {
        self.then(format!("nth={index}"))
    }

    #[napi]
    pub fn first(&self) -> Locator {
        self.nth(0)
    }

    #[napi]
    pub fn last(&self) -> Locator {
        self.nth(-1)
    }

    /// Keeps matches containing `hasText` (case-insensitive substring), as in Playwright.
    #[napi]
    pub fn filter(&self, options: FilterOptions) -> Locator {
        match options.has_text {
            Some(text) => self.then(fluxwright::Selector::has_text(&text)),
            None => self.then_selector(self.selector.clone()),
        }
    }

    /// Searches inside this locator's matches.
    #[napi]
    pub fn locator(&self, selector: String) -> Locator {
        self.then(selector)
    }

    #[napi]
    pub fn get_by_text(&self, text: String, options: Option<GetByTextOptions>) -> Locator {
        self.then(fluxwright::Selector::text(&text, exact(options.and_then(|o| o.exact))))
    }

    #[napi]
    pub fn get_by_role(&self, role: String, options: Option<GetByRoleOptions>) -> Locator {
        let (name, exact) = options.map(|o| (o.name, o.exact.unwrap_or(false))).unwrap_or_default();
        self.then(fluxwright::Selector::role(&role, name.as_deref(), exact))
    }

    #[napi]
    pub fn get_by_label(&self, text: String, options: Option<GetByTextOptions>) -> Locator {
        self.then(fluxwright::Selector::label(&text, exact(options.and_then(|o| o.exact))))
    }

    #[napi]
    pub fn get_by_placeholder(&self, text: String, options: Option<GetByTextOptions>) -> Locator {
        self.then(fluxwright::Selector::placeholder(&text, exact(options.and_then(|o| o.exact))))
    }

    #[napi]
    pub fn get_by_alt_text(&self, text: String, options: Option<GetByTextOptions>) -> Locator {
        self.then(fluxwright::Selector::alt_text(&text, exact(options.and_then(|o| o.exact))))
    }

    #[napi]
    pub fn get_by_title(&self, text: String, options: Option<GetByTextOptions>) -> Locator {
        self.then(fluxwright::Selector::title(&text, exact(options.and_then(|o| o.exact))))
    }

    #[napi]
    pub fn get_by_test_id(&self, test_id: String) -> Locator {
        self.then(fluxwright::Selector::test_id(&test_id))
    }

    /// PNG or JPEG of the element, once it is visible and still.
    #[napi]
    pub async fn screenshot(&self, options: Option<ScreenshotOptions>) -> Result<Buffer> {
        let sel = self.selector.clone();
        let timeout = call_timeout(options.as_ref().and_then(|o| o.timeout)).or(self.timeout);
        let options = shot_options(options)?;
        let image = with_page_in(self.lease.clone(), timeout, move |p| {
            Box::pin(async move { p.element_screenshot_with(&sel, &options).await })
        })
        .await?;
        Ok(image.into())
    }

    /// The element's box relative to the viewport, without scrolling; null when it is not visible.
    #[napi]
    pub async fn bounding_box(&self) -> Result<Option<BoundingBox>> {
        let sel = self.selector.clone();
        let b = with_page_in(self.lease.clone(), self.timeout, move |p| Box::pin(async move { p.bounding_box(&sel).await })).await?;
        Ok(b.map(|b| BoundingBox { x: b.x, y: b.y, width: b.width, height: b.height }))
    }

    /// `function` is JavaScript source taking the element and `arg`; `addon.js` passes a
    /// function's source.
    #[napi]
    pub async fn evaluate(&self, function_source: String, arg: Option<serde_json::Value>) -> Result<serde_json::Value> {
        let sel = self.selector.clone();
        with_page_in(self.lease.clone(), self.timeout, move |p| Box::pin(async move { p.evaluate_on(&sel, &function_source, arg).await }))
            .await
    }
}

impl Locator {
    fn then(&self, part: String) -> Locator {
        self.then_selector(self.selector.then(&part))
    }

    fn then_selector(&self, selector: fluxwright::Selector) -> Locator {
        Locator { lease: self.lease.clone(), selector, timeout: self.timeout }
    }
}

fn exact(flag: Option<bool>) -> bool {
    flag.unwrap_or(false)
}

#[napi(object)]
pub struct FilterOptions {
    /// Case-insensitive substring of the element's text.
    pub has_text: Option<String>,
}

/// CSS pixels, relative to the top-level page's viewport.
#[napi(object)]
pub struct BoundingBox {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[napi(object)]
pub struct ConsoleMessage {
    /// `log`, `error`, `warning`, `info`, `debug`, ...
    #[napi(js_name = "type")]
    pub kind: String,
    pub text: String,
}

/// An exception nothing caught; `page.pageErrors()` turns these into `Error` objects.
#[napi(object)]
pub struct PageErrorInfo {
    pub name: String,
    pub message: String,
    pub stack: String,
}

/// An iframe, same- or cross-origin, to find elements in.
#[napi]
pub struct FrameLocator {
    lease: LeaseSlot,
    frames: Vec<String>,
}

impl FrameLocator {
    fn find(&self, query: String) -> Locator {
        Locator {
            lease: self.lease.clone(),
            selector: fluxwright::Selector { frames: self.frames.clone(), query },
            timeout: None,
        }
    }
}

#[napi]
impl FrameLocator {
    #[napi]
    pub fn locator(&self, selector: String) -> Locator {
        self.find(selector)
    }

    #[napi]
    pub fn get_by_text(&self, text: String, options: Option<GetByTextOptions>) -> Locator {
        let exact = options.and_then(|o| o.exact).unwrap_or(false);
        self.find(fluxwright::Selector::text(&text, exact))
    }

    #[napi]
    pub fn get_by_role(&self, role: String, options: Option<GetByRoleOptions>) -> Locator {
        let (name, exact) = options.map(|o| (o.name, o.exact.unwrap_or(false))).unwrap_or_default();
        self.find(fluxwright::Selector::role(&role, name.as_deref(), exact))
    }

    /// `<label>`, `aria-labelledby` or `aria-label`; case-insensitive substring unless `exact`.
    #[napi]
    pub fn get_by_label(&self, text: String, options: Option<GetByTextOptions>) -> Locator {
        self.find(fluxwright::Selector::label(&text, exact(options.and_then(|o| o.exact))))
    }

    #[napi]
    pub fn get_by_placeholder(&self, text: String, options: Option<GetByTextOptions>) -> Locator {
        self.find(fluxwright::Selector::placeholder(&text, exact(options.and_then(|o| o.exact))))
    }

    #[napi]
    pub fn get_by_alt_text(&self, text: String, options: Option<GetByTextOptions>) -> Locator {
        self.find(fluxwright::Selector::alt_text(&text, exact(options.and_then(|o| o.exact))))
    }

    #[napi]
    pub fn get_by_title(&self, text: String, options: Option<GetByTextOptions>) -> Locator {
        self.find(fluxwright::Selector::title(&text, exact(options.and_then(|o| o.exact))))
    }

    /// `data-testid`, exact.
    #[napi]
    pub fn get_by_test_id(&self, test_id: String) -> Locator {
        self.find(fluxwright::Selector::test_id(&test_id))
    }

    /// A nested iframe inside this one.
    #[napi]
    pub fn frame_locator(&self, selector: String) -> FrameLocator {
        let mut frames = self.frames.clone();
        frames.push(selector);
        FrameLocator { lease: self.lease.clone(), frames }
    }
}

#[napi(object)]
pub struct LaunchOptions {
    /// Default: one per 4 CPUs, with 8 pages each. The fleet's memory budget is 75% of the
    /// memory free at launch: new pages wait while Chrome uses more.
    pub max_browsers: Option<u32>,
    /// Browser binary. Default: chrome-headless-shell when headless and installed, else Chrome.
    pub executable_path: Option<String>,
    /// Default true.
    pub headless: Option<bool>,
    /// Milliseconds `newPage` waits for a page before it fails: for a free slot, and for a
    /// browser to start when one is needed. 0 or unset waits as long as the queue takes.
    pub queue_timeout: Option<u32>,
    /// Memory budget in MB for all Chrome processes together: new pages wait while they use
    /// more. Default: 75% of the memory free at launch.
    pub memory_ceiling_mb: Option<u32>,
    /// Pages that may be on one host at once, across the fleet: a `goto` to a full host waits.
    /// 0 or unset: no limit.
    pub max_concurrency_per_host: Option<u32>,
    /// Milliseconds between the starts of two `goto`s to one host, at least. 0 or unset: none.
    pub min_delay_per_host: Option<u32>,
}

/// `queueTimeout` as the engine's acquire timeout.
fn queue_wait(ms: Option<u32>) -> std::time::Duration {
    match ms {
        Some(ms) if ms > 0 => std::time::Duration::from_millis(ms as u64),
        // ponytail: a year stands in for "no limit", so deadline arithmetic never overflows.
        _ => std::time::Duration::from_secs(365 * 24 * 60 * 60),
    }
}

#[napi(object)]
pub struct ProxyOptions {
    /// `http://host:port`, `socks5://host:port`, ...
    pub server: String,
    /// Comma-separated hosts that skip the proxy.
    pub bypass: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
}

/// Playwright's `newContext` options that Fluxwright supports.
#[napi(object)]
pub struct NewPageOptions {
    pub proxy: Option<ProxyOptions>,
    pub user_agent: Option<String>,
    /// BCP 47, e.g. `de-DE`: `navigator.language`, `Accept-Language` and `Intl`.
    pub locale: Option<String>,
    /// IANA, e.g. `Asia/Tokyo`.
    pub timezone_id: Option<String>,
    /// Also needs `permissions: ['geolocation']`, as in Playwright.
    pub geolocation: Option<Geolocation>,
    /// `geolocation`, `notifications`, `clipboard-read`, ... granted to every origin.
    pub permissions: Option<Vec<String>>,
    pub viewport: Option<ViewportSize>,
    pub device_scale_factor: Option<f64>,
    #[napi(ts_type = "'light' | 'dark' | 'no-preference'")]
    pub color_scheme: Option<String>,
    /// The mobile viewport: the meta viewport tag counts.
    pub is_mobile: Option<bool>,
    /// Touch events and `navigator.maxTouchPoints`.
    pub has_touch: Option<bool>,
    /// Sent with every request the page makes.
    #[napi(js_name = "extraHTTPHeaders")]
    pub extra_http_headers: Option<HashMap<String, String>>,
    /// Load pages whose certificate Chrome would refuse.
    #[napi(js_name = "ignoreHTTPSErrors")]
    pub ignore_https_errors: Option<bool>,
    /// False stops the page's own scripts; `evaluate` still runs.
    pub java_script_enabled: Option<bool>,
    /// No network: requests fail and `navigator.onLine` is false.
    pub offline: Option<bool>,
    /// Base URL for relative `goto` calls.
    #[napi(js_name = "baseURL")]
    pub base_url: Option<String>,
    /// HTTP Basic/Digest credentials.
    pub http_credentials: Option<HttpCredentials>,
    /// Bypass the page's Content-Security-Policy.
    #[napi(js_name = "bypassCSP")]
    pub bypass_csp: Option<bool>,
    /// Cookies and localStorage to start from: what `page.storageState()` returned, or the
    /// path of the JSON file it saved.
    #[napi(ts_type = "StorageState | string")]
    pub storage_state: Option<StorageState>,
}

#[napi(object)]
pub struct Geolocation {
    pub latitude: f64,
    pub longitude: f64,
    /// Meters. Default 0.
    pub accuracy: Option<f64>,
}

/// Cookies and localStorage, in Playwright's `storageState` format: files work in both.
#[napi(object)]
pub struct StorageState {
    pub cookies: Vec<Cookie>,
    pub origins: Vec<OriginStorage>,
}

#[napi(object)]
pub struct Cookie {
    pub name: String,
    pub value: String,
    pub domain: String,
    pub path: String,
    /// Unix seconds; -1 for a session cookie.
    pub expires: f64,
    pub http_only: bool,
    pub secure: bool,
    #[napi(ts_type = "'Strict' | 'Lax' | 'None'")]
    pub same_site: String,
    /// Top-level site of a partitioned (CHIPS) cookie.
    pub partition_key: Option<String>,
    /// The rest of Chrome's partition key, under Playwright's name for it.
    #[napi(js_name = "_crHasCrossSiteAncestor")]
    pub cr_has_cross_site_ancestor: Option<bool>,
}

#[napi(object)]
pub struct OriginStorage {
    pub origin: String,
    pub local_storage: Vec<StorageItem>,
}

#[napi(object)]
pub struct StorageItem {
    pub name: String,
    pub value: String,
}

impl From<StorageState> for fluxwright::StorageState {
    fn from(s: StorageState) -> Self {
        fluxwright::StorageState {
            cookies: s
                .cookies
                .into_iter()
                .map(|c| fluxwright::Cookie {
                    name: c.name,
                    value: c.value,
                    domain: c.domain,
                    path: c.path,
                    expires: c.expires,
                    http_only: c.http_only,
                    secure: c.secure,
                    same_site: c.same_site,
                    partition_key: c.partition_key,
                    cross_site_ancestor: c.cr_has_cross_site_ancestor,
                })
                .collect(),
            origins: s
                .origins
                .into_iter()
                .map(|o| fluxwright::OriginStorage {
                    origin: o.origin,
                    local_storage: o
                        .local_storage
                        .into_iter()
                        .map(|i| fluxwright::StorageItem { name: i.name, value: i.value })
                        .collect(),
                })
                .collect(),
        }
    }
}

impl From<fluxwright::StorageState> for StorageState {
    fn from(s: fluxwright::StorageState) -> Self {
        StorageState {
            cookies: s
                .cookies
                .into_iter()
                .map(|c| Cookie {
                    name: c.name,
                    value: c.value,
                    domain: c.domain,
                    path: c.path,
                    expires: c.expires,
                    http_only: c.http_only,
                    secure: c.secure,
                    same_site: c.same_site,
                    partition_key: c.partition_key,
                    cr_has_cross_site_ancestor: c.cross_site_ancestor,
                })
                .collect(),
            origins: s
                .origins
                .into_iter()
                .map(|o| OriginStorage {
                    origin: o.origin,
                    local_storage: o.local_storage.into_iter().map(|i| StorageItem { name: i.name, value: i.value }).collect(),
                })
                .collect(),
        }
    }
}

fn job_options(o: NewPageOptions) -> Result<fluxwright::JobOptions> {
    let color_scheme = match o.color_scheme.as_deref() {
        None => None,
        Some(s) => Some(fluxwright::ColorScheme::parse(s).ok_or_else(|| {
            Error::from_reason(format!("colorScheme must be light, dark or no-preference, not `{s}`"))
        })?),
    };
    Ok(fluxwright::JobOptions {
        proxy: o.proxy.map(|p| fluxwright::Proxy {
            server: p.server,
            bypass: p.bypass,
            username: p.username,
            password: p.password,
        }),
        emulation: fluxwright::Emulation {
            user_agent: o.user_agent,
            locale: o.locale,
            timezone_id: o.timezone_id,
            geolocation: o.geolocation.map(|g| fluxwright::Geolocation {
                latitude: g.latitude,
                longitude: g.longitude,
                accuracy: g.accuracy.unwrap_or(0.0),
            }),
            viewport: o.viewport.map(|v| (v.width, v.height)),
            device_scale_factor: o.device_scale_factor,
            color_scheme,
            is_mobile: o.is_mobile.unwrap_or(false),
            has_touch: o.has_touch.unwrap_or(false),
            extra_http_headers: o.extra_http_headers.unwrap_or_default().into_iter().collect(),
            ignore_https_errors: o.ignore_https_errors.unwrap_or(false),
            javascript_enabled: o.java_script_enabled,
            offline: o.offline.unwrap_or(false),
            bypass_csp: o.bypass_csp.unwrap_or(false),
        },
        permissions: o.permissions.unwrap_or_default(),
        storage_state: o.storage_state.map(Into::into),
        base_url: o.base_url,
        http_credentials: o.http_credentials.map(|c| (c.username, c.password)),
        ..Default::default()
    })
}

#[napi(object)]
pub struct HttpCredentials {
    pub username: String,
    pub password: String,
}

#[napi(object)]
pub struct GotoOptions {
    /// Default "load".
    #[napi(ts_type = "'load' | 'domcontentloaded' | 'networkidle' | 'commit'")]
    pub wait_until: Option<String>,
    /// Milliseconds; 0 for no timeout. Default: the page's or the engine's navigation timeout.
    pub timeout: Option<f64>,
}

#[napi(object)]
pub struct ScreenshotOptions {
    /// The whole scrollable page (page screenshots only).
    pub full_page: Option<bool>,
    /// Default "png".
    #[napi(js_name = "type", ts_type = "'png' | 'jpeg'")]
    pub kind: Option<String>,
    /// 0–100, JPEG only.
    pub quality: Option<u32>,
    /// Relative to the viewport, or to the page with `fullPage` (page screenshots only).
    pub clip: Option<BoundingBox>,
    /// A transparent background instead of the default white one (PNG).
    pub omit_background: Option<bool>,
    /// Milliseconds; 0 for no timeout. Default: the page's or the engine's action timeout.
    pub timeout: Option<f64>,
}

/// Lengths: a number of pixels, or a string with `px`, `in`, `cm` or `mm`.
#[napi(object)]
pub struct PdfMargin {
    pub top: Option<String>,
    pub right: Option<String>,
    pub bottom: Option<String>,
    pub left: Option<String>,
}

#[napi(object)]
pub struct PdfOptions {
    /// Letter (default), Legal, Tabloid, Ledger, A0–A6. `width` and `height` win over it.
    pub format: Option<String>,
    pub width: Option<String>,
    pub height: Option<String>,
    pub margin: Option<PdfMargin>,
    pub landscape: Option<bool>,
    pub print_background: Option<bool>,
    /// 0.1–2, default 1.
    pub scale: Option<f64>,
    /// Such as "1-5, 8". All pages when unset.
    pub page_ranges: Option<String>,
    #[napi(js_name = "preferCSSPageSize")]
    pub prefer_css_page_size: Option<bool>,
    pub display_header_footer: Option<bool>,
    pub header_template: Option<String>,
    pub footer_template: Option<String>,
    pub outline: Option<bool>,
    pub tagged: Option<bool>,
}

fn shot_options(options: Option<ScreenshotOptions>) -> Result<fluxwright::ScreenshotOptions> {
    let Some(o) = options else {
        return Ok(Default::default());
    };
    let format = match o.kind.as_deref() {
        None | Some("png") => fluxwright::ImageFormat::Png,
        Some("jpeg") => fluxwright::ImageFormat::Jpeg,
        Some(t) => return Err(Error::from_reason(format!("screenshot: type {t:?} is not png or jpeg"))),
    };
    if format == fluxwright::ImageFormat::Png && o.quality.is_some() {
        return Err(Error::from_reason("screenshot: quality is for JPEG only"));
    }
    Ok(fluxwright::ScreenshotOptions {
        full_page: o.full_page.unwrap_or(false),
        format,
        // Over 255 becomes 255, which the engine rejects as over 100.
        quality: o.quality.map(|q| q.min(255) as u8),
        clip: o.clip.map(|c| fluxwright::Clip { x: c.x, y: c.y, width: c.width, height: c.height }),
        omit_background: o.omit_background.unwrap_or(false),
    })
}

fn pdf_options(options: Option<PdfOptions>) -> fluxwright::PdfOptions {
    let Some(o) = options else {
        return Default::default();
    };
    let m = o.margin.map(|m| fluxwright::PdfMargin { top: m.top, right: m.right, bottom: m.bottom, left: m.left });
    fluxwright::PdfOptions {
        format: o.format,
        width: o.width,
        height: o.height,
        margin: m.unwrap_or_default(),
        landscape: o.landscape.unwrap_or(false),
        print_background: o.print_background.unwrap_or(false),
        scale: o.scale,
        page_ranges: o.page_ranges,
        prefer_css_page_size: o.prefer_css_page_size.unwrap_or(false),
        display_header_footer: o.display_header_footer.unwrap_or(false),
        header_template: o.header_template,
        footer_template: o.footer_template,
        outline: o.outline.unwrap_or(false),
        tagged: o.tagged.unwrap_or(false),
    }
}

#[napi(object)]
pub struct ViewportSize {
    pub width: u32,
    pub height: u32,
}

#[napi]
pub struct Chromium {}

#[napi]
impl Chromium {
    #[napi]
    pub async fn launch(options: Option<LaunchOptions>) -> Result<Browser> {
        let (max, exe, headless, queue_timeout, ceiling, per_host, host_delay) = match options {
            Some(o) => (
                o.max_browsers,
                o.executable_path,
                o.headless,
                o.queue_timeout,
                o.memory_ceiling_mb,
                o.max_concurrency_per_host,
                o.min_delay_per_host,
            ),
            None => (None, None, None, None, None, None, None),
        };
        let engine = RT
            .spawn(async move {
                // newPage waits its turn however long the queue is, so starting every job at
                // once works; the engine's own default (256 waiting, then an error) is for a
                // server shedding load.
                let mut b = fluxwright::BrowserEngine::builder()
                    .max_contexts_per_browser(8)
                    .queue_capacity(usize::MAX)
                    .acquire_timeout(queue_wait(queue_timeout));
                if let Some(n) = max {
                    b = b.max_browsers(n as usize);
                }
                if let Some(mb) = ceiling {
                    b = b.memory_ceiling_mb(mb as u64);
                }
                if let Some(n) = per_host {
                    b = b.max_concurrency_per_host(n as usize);
                }
                if let Some(ms) = host_delay {
                    b = b.min_delay_per_host(std::time::Duration::from_millis(ms as u64));
                }
                if let Some(exe) = exe {
                    b = b.chrome(exe);
                }
                if let Some(h) = headless {
                    b = b.headless(h);
                }
                b.build().await
            })
            .await
            .map_err(|e| Error::from_reason(e.to_string()))?
            .map_err(|e| Error::from_reason(e.to_string()))?;
        Ok(Browser { inner: engine })
    }

    /// Connects to an existing browser over the Chrome DevTools Protocol (`ws://...` or `http://...`).
    #[napi(js_name = "connectOverCDP")]
    pub async fn connect_over_cdp(endpoint_url: String) -> Result<Browser> {
        let engine = RT
            .spawn(async move {
                fluxwright::BrowserEngine::builder()
                    .connect_over_cdp(endpoint_url)
                    .max_contexts_per_browser(8)
                    .queue_capacity(usize::MAX)
                    .acquire_timeout(queue_wait(None))
                    .build()
                    .await
            })
            .await
            .map_err(|e| Error::from_reason(e.to_string()))?
            .map_err(|e| Error::from_reason(e.to_string()))?;
        Ok(Browser { inner: engine })
    }
}

#[napi]
impl Browser {
    /// A fresh browser context. Every option applies to this page only.
    #[napi]
    pub async fn new_page(&self, options: Option<NewPageOptions>) -> Result<Page> {
        let opts = match options {
            Some(o) => job_options(o)?,
            None => fluxwright::JobOptions::default(),
        };
        let eng = self.inner.clone();
        let lease = RT
            .spawn(async move { eng.acquire_job(&opts).await })
            .await
            .map_err(|e| Error::from_reason(e.to_string()))?
            .map_err(|e| Error::from_reason(e.to_string()))?;
        Ok(Page {
            lease: Arc::new(PageSlot { page: RwLock::new(Some(lease)), closing: tokio::sync::watch::Sender::new(false) }),
            routes: Default::default(),
        })
    }

    #[napi]
    pub async fn close(&self) -> Result<()> {
        let eng = self.inner.clone();
        RT.spawn(async move {
            let _ = eng.shutdown(std::time::Duration::from_secs(5)).await;
        })
        .await
        .map_err(|e| Error::from_reason(e.to_string()))?;
        Ok(())
    }
}

#[napi]
impl Page {
    /// Every cookie in this page's context, plus localStorage of the current origin. Pass it
    /// (or a file it was saved to) as `newPage({ storageState })` to skip logging in again.
    #[napi]
    pub async fn storage_state(&self) -> Result<StorageState> {
        with_page(self.lease.clone(), |p| Box::pin(async move { p.storage_state().await }))
            .await
            .map(Into::into)
    }

    #[napi]
    pub async fn goto(&self, url: String, options: Option<GotoOptions>) -> Result<()> {
        let timeout = call_timeout(options.as_ref().and_then(|o| o.timeout));
        let wait_until = match options.and_then(|o| o.wait_until).as_deref() {
            None | Some("load") => fluxwright::WaitUntil::Load,
            Some("domcontentloaded") => fluxwright::WaitUntil::DomContentLoaded,
            Some("networkidle") => fluxwright::WaitUntil::NetworkIdle,
            Some("commit") => fluxwright::WaitUntil::Commit,
            Some(other) => {
                return Err(Error::from_reason(format!(
                    "waitUntil must be load, domcontentloaded, networkidle or commit, got {other}"
                )))
            }
        };
        with_page_in(self.lease.clone(), timeout, move |p| Box::pin(async move { p.goto_with(&url, wait_until).await }))
            .await
    }

    #[napi]
    pub async fn title(&self) -> Result<String> {
        with_page(self.lease.clone(), |p| Box::pin(async move { p.title().await })).await
    }

    #[napi]
    pub async fn content(&self) -> Result<String> {
        with_page(self.lease.clone(), |p| Box::pin(async move { p.content().await })).await
    }

    #[napi]
    pub async fn evaluate(&self, expression: String) -> Result<serde_json::Value> {
        with_page(self.lease.clone(), move |p| Box::pin(async move { p.evaluate(&expression).await })).await
    }

    #[napi]
    pub async fn screenshot(&self, options: Option<ScreenshotOptions>) -> Result<Buffer> {
        let timeout = call_timeout(options.as_ref().and_then(|o| o.timeout));
        let options = shot_options(options)?;
        let image =
            with_page_in(self.lease.clone(), timeout, move |p| Box::pin(async move { p.screenshot_with(&options).await }))
                .await?;
        Ok(image.into())
    }

    /// The page printed to PDF (headless only).
    #[napi]
    pub async fn pdf(&self, options: Option<PdfOptions>) -> Result<Buffer> {
        let options = pdf_options(options);
        let pdf = with_page(self.lease.clone(), move |p| Box::pin(async move { p.pdf(&options).await })).await?;
        Ok(pdf.into())
    }

    #[napi]
    pub async fn set_viewport_size(&self, size: ViewportSize) -> Result<()> {
        with_page(self.lease.clone(), move |p| Box::pin(async move { p.set_viewport_size(size.width, size.height).await }))
            .await
    }

    /// CSS, `text=`, or `role=` selector.
    #[napi]
    pub fn locator(&self, selector: String) -> Locator {
        self.top().locator(selector)
    }

    #[napi]
    pub fn get_by_text(&self, text: String, options: Option<GetByTextOptions>) -> Locator {
        self.top().get_by_text(text, options)
    }

    #[napi]
    pub fn get_by_role(&self, role: String, options: Option<GetByRoleOptions>) -> Locator {
        self.top().get_by_role(role, options)
    }

    #[napi]
    pub fn get_by_label(&self, text: String, options: Option<GetByTextOptions>) -> Locator {
        self.top().get_by_label(text, options)
    }

    #[napi]
    pub fn get_by_placeholder(&self, text: String, options: Option<GetByTextOptions>) -> Locator {
        self.top().get_by_placeholder(text, options)
    }

    #[napi]
    pub fn get_by_alt_text(&self, text: String, options: Option<GetByTextOptions>) -> Locator {
        self.top().get_by_alt_text(text, options)
    }

    #[napi]
    pub fn get_by_title(&self, text: String, options: Option<GetByTextOptions>) -> Locator {
        self.top().get_by_title(text, options)
    }

    #[napi]
    pub fn get_by_test_id(&self, test_id: String) -> Locator {
        self.top().get_by_test_id(test_id)
    }

    #[napi]
    pub fn frame_locator(&self, selector: String) -> FrameLocator {
        self.top().frame_locator(selector)
    }

    /// Calls `handler` with each console message and uncaught error from now on; `addon.js`
    /// builds `page.on('console' | 'pageerror')` on it.
    #[napi(js_name = "_onLog")]
    pub fn on_log(&self, handler: Function<LogEvent, ()>) -> Result<()> {
        let tsfn = callback(handler)?;
        // Subscribed before this returns (a message logged right after it is seen), and the task
        // holds only the subscription: holding the lease kept a page never closed alive.
        let mut entries = self.now(|p| p.subscribe_logs())?;
        RT.spawn(async move {
            // Ends when the page closes.
            while let Some(entry) = entries.recv().await {
                let event = match entry {
                    fluxwright::LogEntry::Console(m) => LogEvent {
                        event: "console".into(),
                        message: Some(ConsoleMessage { kind: m.kind, text: m.text }),
                        error: None,
                    },
                    fluxwright::LogEntry::Error(e) => LogEvent {
                        event: "pageerror".into(),
                        message: None,
                        error: Some(PageErrorInfo { name: e.name, message: e.message, stack: e.stack }),
                    },
                };
                tsfn.call(event, ThreadsafeFunctionCallMode::NonBlocking);
            }
        });
        Ok(())
    }

    /// Calls `handler` with each network event from now on; `addon.js` builds
    /// `page.on('request' | 'response' | 'requestfinished' | 'requestfailed')` on it. Subscribed
    /// before this returns, so a navigation right after it is seen whole.
    #[napi(js_name = "_onNetwork")]
    pub fn on_network(&self, handler: Function<NetworkEvent, ()>) -> Result<()> {
        let tsfn = callback(handler)?;
        let mut events = self.now(|p| p.subscribe_network())?;
        RT.spawn(async move {
            // Ends when the page closes.
            while let Some(e) = events.recv().await {
                tsfn.call(e.into(), ThreadsafeFunctionCallMode::NonBlocking);
            }
        });
        Ok(())
    }

    /// Hands this page's dialogs to `handler` instead of dismissing them, from before this
    /// returns; `addon.js` builds `page.on('dialog')` on it.
    #[napi(js_name = "_onDialog")]
    pub fn on_dialog(&self, handler: Function<PendingDialog, ()>) -> Result<()> {
        let tsfn = callback(handler)?;
        let mut dialogs = self.now(|p| p.subscribe_dialogs())?;
        RT.spawn(async move {
            while let Some(d) = dialogs.recv().await {
                tsfn.call(PendingDialog::new(d), ThreadsafeFunctionCallMode::NonBlocking);
            }
        });
        Ok(())
    }

    /// Sets the function that receives intercepted requests; `addon.js` passes its route
    /// dispatcher, then calls `intercept`.
    #[napi(js_name = "_setRouteHandler")]
    pub fn set_route_handler(&self, handler: Function<InterceptedRoute, ()>) -> Result<()> {
        *self.routes.lock().unwrap() = Some(Arc::new(callback(handler)?));
        Ok(())
    }

    /// Starts handing this page's requests to the route handler. Resolves once interception
    /// is on, so a `goto` after it is covered.
    #[napi(js_name = "_intercept")]
    pub async fn intercept(&self) -> Result<()> {
        let Some(tsfn) = self.routes.lock().unwrap().clone() else {
            return Err(Error::from_reason("set a route handler first"));
        };
        let mut requests = with_page(self.lease.clone(), |p| Box::pin(async move { p.intercept().await })).await?;
        RT.spawn(async move {
            while let Some(req) = requests.recv().await {
                tsfn.call(InterceptedRoute::new(req), ThreadsafeFunctionCallMode::NonBlocking);
            }
        });
        Ok(())
    }

    /// The next download this page finished (finished ones queue up), within `timeoutMs`
    /// (default 30 s).
    #[napi(js_name = "_waitForDownload")]
    pub async fn wait_for_download(&self, timeout_ms: Option<u32>) -> Result<DownloadInfo> {
        // 0 waits as long as it takes, as in Playwright (it used to fail at once).
        let timeout = queue_wait(Some(timeout_ms.unwrap_or(30_000)));
        // Hold no page lock while waiting: the click that starts the download needs the page.
        let downloads = with_page(self.lease.clone(), |p| Box::pin(async move { Ok(p.downloads()) })).await?;
        let d = RT
            .spawn(async move { downloads.next(timeout).await.map_err(|e| e.to_string()) })
            .await
            .map_err(|e| Error::from_reason(e.to_string()))?
            .map_err(Error::from_reason)?;
        Ok(DownloadInfo { url: d.url, suggested_filename: d.suggested_filename, path: d.path.to_string_lossy().into_owned() })
    }

    /// Console messages so far from this page, its popups and its iframes (the last 1000).
    #[napi]
    pub async fn console_messages(&self) -> Result<Vec<ConsoleMessage>> {
        let all = with_page(self.lease.clone(), |p| Box::pin(async move { Ok(p.console_messages()) })).await?;
        Ok(all.into_iter().map(|m| ConsoleMessage { kind: m.kind, text: m.text }).collect())
    }

    /// Exceptions nothing caught so far (the last 1000).
    #[napi]
    pub async fn page_errors(&self) -> Result<Vec<PageErrorInfo>> {
        let all = with_page(self.lease.clone(), |p| Box::pin(async move { Ok(p.page_errors()) })).await?;
        Ok(all.into_iter().map(|e| PageErrorInfo { name: e.name, message: e.message, stack: e.stack }).collect())
    }

    #[napi]
    pub async fn close(&self) -> Result<()> {
        // The route callback keeps the JS dispatcher and its handlers: free them now.
        self.routes.lock().unwrap().take();
        // Calls still running give up now (see `with_page`), so the write lock comes at once.
        self.lease.closing.send_replace(true);
        let slot = self.lease.clone();
        RT.spawn(async move {
            *slot.page.write().await = None;
        })
        .await
        .map_err(|e| Error::from_reason(e.to_string()))?;
        Ok(())
    }

    /// Whether `close()` was called (or is under way).
    #[napi]
    pub fn is_closed(&self) -> bool {
        *self.lease.closing.borrow()
    }

    /// The timeout in milliseconds for this page's calls from now on, navigation included; 0
    /// for none. A call's own `timeout` option wins.
    #[napi]
    pub fn set_default_timeout(&self, timeout: f64) {
        self.set_timeouts(timeout, false);
    }

    /// The timeout in milliseconds for this page's `goto` from now on; 0 for none.
    #[napi]
    pub fn set_default_navigation_timeout(&self, timeout: f64) {
        self.set_timeouts(timeout, true);
    }

    #[napi(js_name = "_addCookies")]
    pub async fn add_cookies(&self, cookies: Vec<Cookie>) -> Result<()> {
        let cookies: Vec<fluxwright::Cookie> = cookies
            .into_iter()
            .map(|c| fluxwright::Cookie {
                name: c.name,
                value: c.value,
                domain: c.domain,
                path: c.path,
                expires: c.expires,
                http_only: c.http_only,
                secure: c.secure,
                same_site: c.same_site,
                partition_key: c.partition_key,
                cross_site_ancestor: c.cr_has_cross_site_ancestor,
            })
            .collect();
        with_page(self.lease.clone(), move |p| Box::pin(async move { p.add_cookies(&cookies).await })).await
    }

    #[napi(js_name = "_clearCookies")]
    pub async fn clear_cookies(&self) -> Result<()> {
        with_page(self.lease.clone(), |p| Box::pin(async move { p.clear_cookies().await })).await
    }

    #[napi(js_name = "_emulateMedia")]
    pub async fn emulate_media(&self, media: Option<String>, color_scheme: Option<String>) -> Result<()> {
        with_page(self.lease.clone(), move |p| {
            Box::pin(async move { p.emulate_media(media.as_deref(), color_scheme.as_deref()).await })
        })
        .await
    }
}

impl Page {
    fn set_timeouts(&self, ms: f64, navigation: bool) {
        // Only close() holds the page for writing, and a closed page needs no timeout.
        let Ok(page) = self.lease.page.try_read() else {
            return;
        };
        if let (Some(page), Some(timeout)) = (page.as_ref(), call_timeout(Some(ms))) {
            if navigation {
                page.set_default_navigation_timeout(timeout);
            } else {
                page.set_default_timeout(timeout);
            }
        }
    }

    fn top(&self) -> FrameLocator {
        FrameLocator { lease: self.lease.clone(), frames: Vec::new() }
    }

    /// Runs `f` on the page at once, for subscriptions, which must be in place when they return;
    /// fails once `close()` began.
    fn now<T>(&self, f: impl FnOnce(&fluxwright::PageLease) -> T) -> Result<T> {
        let page = self.lease.page.try_read().map_err(|_| Error::from_reason("page closed"))?;
        match page.as_ref() {
            Some(p) if !*self.lease.closing.borrow() => Ok(f(p)),
            _ => Err(Error::from_reason("page closed")),
        }
    }
}
