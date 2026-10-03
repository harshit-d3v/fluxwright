//! The native half of the `fluxwright` Python package: the engine, pages and locators, with
//! awaitable methods. `fluxwright/async_api.py` adds what needs Python (routes, events), and
//! `fluxwright/sync_api.py` the blocking API.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::PyBytes;
use pyo3_async_runtimes::tokio::future_into_py;
use serde::Deserialize;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::{Mutex, RwLock};

/// Read-locked by every page call, so calls run side by side (a download wait and the click
/// that starts it); write-locked by `close`.
type LeaseSlot = Arc<RwLock<Option<fluxwright::PageLease>>>;

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// What an awaitable that answers nothing resolves with: `None` (a bare `()` would arrive as an
/// empty tuple).
type Nothing = Option<()>;

fn none<T>(_: T) -> Nothing {
    None
}

/// `fluxwright.Error` with `message`, or `fluxwright.TimeoutError` (`class`).
fn raise(class: &str, message: String) -> PyErr {
    Python::attach(|py| {
        py.import("fluxwright._errors")
            .and_then(|m| m.getattr(class))
            .and_then(|cls| cls.call1((message,)))
            .map_or_else(|e| e, PyErr::from_value)
    })
}

/// An engine error as Playwright raises it: `TimeoutError` when something ran out of time.
fn error(e: impl Into<fluxwright::Error>) -> PyErr {
    use fluxwright::Error as E;
    let e = e.into();
    let timeout = match &e {
        E::AcquireTimeout | E::JobTimeout | E::Cdp(fluxwright_cdp::Error::Timeout { .. }) => true,
        // How `Downloads::next` words its timeout.
        E::Other(m) => m.starts_with("no download finished within"),
        _ => false,
    };
    raise(
        if timeout { "TimeoutError" } else { "Error" },
        e.to_string(),
    )
}

fn closed() -> PyErr {
    raise("Error", "page closed".into())
}

/// Runs `f` against the page; fails with "page closed" after `close()`.
async fn with_page<T, F>(lease: &LeaseSlot, f: F) -> PyResult<T>
where
    F: for<'a> FnOnce(&'a fluxwright::PageLease) -> BoxFuture<'a, fluxwright::Result<T>>,
{
    match lease.read().await.as_ref() {
        Some(p) => f(p).await.map_err(error),
        None => Err(closed()),
    }
}

/// `with_page` as a Python awaitable.
fn run<'py, T, F>(py: Python<'py>, lease: &LeaseSlot, f: F) -> PyResult<Bound<'py, PyAny>>
where
    T: for<'a> IntoPyObject<'a> + Send + 'static,
    F: for<'a> FnOnce(&'a fluxwright::PageLease) -> BoxFuture<'a, fluxwright::Result<T>>
        + Send
        + 'static,
{
    let lease = lease.clone();
    future_into_py(py, async move { with_page(&lease, f).await })
}

/// A JSON value as what `json.loads` makes of it.
struct Json(serde_json::Value);

impl<'py> IntoPyObject<'py> for Json {
    type Target = PyAny;
    type Output = Bound<'py, PyAny>;
    type Error = PyErr;

    fn into_pyobject(self, py: Python<'py>) -> PyResult<Self::Output> {
        py.import("json")?
            .call_method1("loads", (self.0.to_string(),))
    }
}

/// A Python value as JSON, through `json.dumps`.
fn to_json(value: &Bound<'_, PyAny>) -> PyResult<serde_json::Value> {
    let text: String = value
        .py()
        .import("json")?
        .call_method1("dumps", (value,))?
        .extract()?;
    serde_json::from_str(&text).map_err(|e| PyValueError::new_err(e.to_string()))
}

/// An options dict as `T`; `what` names it in the error.
fn options<T: serde::de::DeserializeOwned>(value: &Bound<'_, PyAny>, what: &str) -> PyResult<T> {
    serde_json::from_value(to_json(value)?)
        .map_err(|e| PyValueError::new_err(format!("{what}: {e}")))
}

/// `str` or `bytes`.
fn bytes(value: &Bound<'_, PyAny>) -> PyResult<Vec<u8>> {
    match value.cast::<PyBytes>() {
        Ok(b) => Ok(b.as_bytes().to_vec()),
        Err(_) => Ok(value.extract::<String>()?.into_bytes()),
    }
}

/// Writes `data`, creating missing folders, as Playwright does with `.auth/state.json`.
fn save(path: &Path, data: &[u8]) -> PyResult<()> {
    create_parent(path, &mut std::fs::DirBuilder::new())?;
    Ok(std::fs::write(path, data)?)
}

/// `save` for saved cookies: new folders and the file are the owner's alone.
#[cfg(unix)]
fn save_private(path: &Path, data: &[u8]) -> PyResult<()> {
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
    create_parent(path, std::fs::DirBuilder::new().mode(0o700))?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    // A file that already existed keeps its mode on open: narrow it before the cookies go in.
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    Ok(file.write_all(data)?)
}

/// Windows has no mode bits: a new file takes its folder's access list.
#[cfg(not(unix))]
fn save_private(path: &Path, data: &[u8]) -> PyResult<()> {
    save(path, data)
}

fn create_parent(path: &Path, folders: &mut std::fs::DirBuilder) -> PyResult<()> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        folders.recursive(true).create(dir)?;
    }
    Ok(())
}

/// The script for `page.evaluate(expression, arg)`, read as Playwright reads it: a function
/// (arrow, `function` or `async`) is called with `arg`, which is `null` for Python's `None`;
/// anything else is an expression, or statements when there is no `arg`.
fn page_function(expression: &str, arg: Option<&serde_json::Value>) -> String {
    let e = expression
        .trim()
        .trim_end_matches(|c: char| c == ';' || c.is_whitespace());
    if arg.is_none() && !looks_like_function(e) {
        return expression.to_string();
    }
    let arg = arg.map_or_else(|| "null".to_string(), |a| a.to_string());
    // The newlines keep a trailing `// comment` from swallowing the parenthesis.
    format!(
        "(() => {{ const __fluxwright_fn = (\n{e}\n); \
         return typeof __fluxwright_fn === 'function' ? __fluxwright_fn({arg}) : __fluxwright_fn; }})()"
    )
}

fn looks_like_function(e: &str) -> bool {
    if e.starts_with("function") || e.starts_with("async") || e.starts_with('(') {
        return true;
    }
    let rest = e.trim_start_matches(|c: char| c.is_alphanumeric() || c == '_' || c == '$');
    rest.len() < e.len() && rest.trim_start().starts_with("=>")
}

#[derive(Deserialize)]
struct ProxyOptions {
    server: String,
    bypass: Option<String>,
    username: Option<String>,
    password: Option<String>,
}

#[derive(Deserialize)]
struct GeolocationOptions {
    latitude: f64,
    longitude: f64,
    #[serde(default)]
    accuracy: f64,
}

#[derive(Deserialize)]
struct Size {
    width: u32,
    height: u32,
}

/// Saved storage: what `storage_state()` returned, or the path of the JSON file it saved.
fn storage(value: &Bound<'_, PyAny>) -> PyResult<fluxwright::StorageState> {
    match value.extract::<PathBuf>() {
        Ok(path) => serde_json::from_str(&std::fs::read_to_string(&path)?)
            .map_err(|e| PyValueError::new_err(format!("storage_state {}: {e}", path.display()))),
        Err(_) => options(value, "storage_state"),
    }
}

/// The engine: a pool of Chrome processes that pages are leased from.
#[pyclass(frozen, module = "fluxwright._native")]
struct Engine {
    inner: fluxwright::BrowserEngine,
}

#[pymethods]
impl Engine {
    #[staticmethod]
    #[pyo3(signature = (*, max_browsers=None, executable_path=None, headless=None))]
    fn launch(
        py: Python<'_>,
        max_browsers: Option<usize>,
        executable_path: Option<PathBuf>,
        headless: Option<bool>,
    ) -> PyResult<Bound<'_, PyAny>> {
        future_into_py(py, async move {
            let mut b = fluxwright::BrowserEngine::builder()
                .max_browsers(max_browsers.unwrap_or(4))
                .max_contexts_per_browser(8);
            if let Some(exe) = executable_path {
                b = b.chrome(exe);
            }
            if let Some(h) = headless {
                b = b.headless(h);
            }
            Ok(Engine {
                inner: b.build().await.map_err(error)?,
            })
        })
    }

    /// A fresh browser context; every option applies to this page only.
    #[allow(clippy::too_many_arguments)]
    #[pyo3(signature = (*, proxy=None, user_agent=None, locale=None, timezone_id=None, geolocation=None,
                        permissions=None, viewport=None, device_scale_factor=None, color_scheme=None,
                        storage_state=None))]
    fn new_page<'py>(
        &self,
        py: Python<'py>,
        proxy: Option<Bound<'py, PyAny>>,
        user_agent: Option<String>,
        locale: Option<String>,
        timezone_id: Option<String>,
        geolocation: Option<Bound<'py, PyAny>>,
        permissions: Option<Vec<String>>,
        viewport: Option<Bound<'py, PyAny>>,
        device_scale_factor: Option<f64>,
        color_scheme: Option<String>,
        storage_state: Option<Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let color_scheme = match color_scheme {
            None => None,
            Some(s) => Some(fluxwright::ColorScheme::parse(&s).ok_or_else(|| {
                PyValueError::new_err(format!(
                    "color_scheme must be light, dark or no-preference, not {s:?}"
                ))
            })?),
        };
        let proxy: Option<ProxyOptions> = proxy.map(|p| options(&p, "proxy")).transpose()?;
        let geolocation: Option<GeolocationOptions> = geolocation
            .map(|g| options(&g, "geolocation"))
            .transpose()?;
        let viewport: Option<Size> = viewport.map(|v| options(&v, "viewport")).transpose()?;
        let opts = fluxwright::JobOptions {
            proxy: proxy.map(|p| fluxwright::Proxy {
                server: p.server,
                bypass: p.bypass,
                username: p.username,
                password: p.password,
            }),
            emulation: fluxwright::Emulation {
                user_agent,
                locale,
                timezone_id,
                geolocation: geolocation.map(|g| fluxwright::Geolocation {
                    latitude: g.latitude,
                    longitude: g.longitude,
                    accuracy: g.accuracy,
                }),
                viewport: viewport.map(|v| (v.width, v.height)),
                device_scale_factor,
                color_scheme,
            },
            permissions: permissions.unwrap_or_default(),
            storage_state: storage_state.map(|s| storage(&s)).transpose()?,
            ..Default::default()
        };
        let engine = self.inner.clone();
        future_into_py(py, async move {
            let lease = engine.acquire_job(&opts).await.map_err(error)?;
            Ok(Lease(std::sync::Mutex::new(Some(lease))))
        })
    }

    fn close<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let engine = self.inner.clone();
        future_into_py(py, async move {
            let _ = engine.shutdown(Duration::from_secs(5)).await;
            Ok(None::<()>)
        })
    }
}

/// A leased page on its way to becoming a `Page`.
#[pyclass(frozen, module = "fluxwright._native")]
struct Lease(std::sync::Mutex<Option<fluxwright::PageLease>>);

/// A browser tab in a fresh context. Subclassed by `fluxwright.async_api.Page`.
#[pyclass(frozen, subclass, module = "fluxwright._native")]
struct Page {
    lease: LeaseSlot,
}

#[pymethods]
impl Page {
    #[new]
    fn new(lease: &Bound<'_, Lease>) -> PyResult<Self> {
        let lease = lease.get().0.lock().unwrap().take();
        let lease = lease.ok_or_else(|| PyValueError::new_err("this lease already has a page"))?;
        Ok(Page {
            lease: Arc::new(RwLock::new(Some(lease))),
        })
    }

    #[pyo3(signature = (url, *, wait_until=None))]
    fn goto<'py>(
        &self,
        py: Python<'py>,
        url: String,
        wait_until: Option<String>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let wait = match wait_until.as_deref() {
            None | Some("load") => fluxwright::WaitUntil::Load,
            Some("domcontentloaded") => fluxwright::WaitUntil::DomContentLoaded,
            Some("networkidle") => fluxwright::WaitUntil::NetworkIdle,
            Some("commit") => fluxwright::WaitUntil::Commit,
            Some(other) => {
                return Err(PyValueError::new_err(format!(
                "wait_until must be load, domcontentloaded, networkidle or commit, not {other:?}"
            )))
            }
        };
        run(py, &self.lease, move |p| {
            Box::pin(async move { p.goto_with(&url, wait).await.map(none) })
        })
    }

    fn title<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        run(py, &self.lease, |p| Box::pin(p.title()))
    }

    fn content<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        run(py, &self.lease, |p| Box::pin(p.content()))
    }

    fn click<'py>(&self, py: Python<'py>, selector: String) -> PyResult<Bound<'py, PyAny>> {
        run(py, &self.lease, move |p| {
            Box::pin(async move { p.click(selector.as_str()).await.map(none) })
        })
    }

    fn fill<'py>(
        &self,
        py: Python<'py>,
        selector: String,
        value: String,
    ) -> PyResult<Bound<'py, PyAny>> {
        run(py, &self.lease, move |p| {
            Box::pin(async move { p.fill(selector.as_str(), &value).await.map(none) })
        })
    }

    fn wait_for_selector<'py>(
        &self,
        py: Python<'py>,
        selector: String,
    ) -> PyResult<Bound<'py, PyAny>> {
        run(py, &self.lease, move |p| {
            Box::pin(async move { p.wait_for_selector(selector.as_str()).await.map(none) })
        })
    }

    #[pyo3(signature = (expression, arg=None))]
    fn evaluate<'py>(
        &self,
        py: Python<'py>,
        expression: String,
        arg: Option<Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let arg = arg.map(|a| to_json(&a)).transpose()?;
        let script = page_function(&expression, arg.as_ref());
        run(py, &self.lease, move |p| {
            Box::pin(async move { p.evaluate(&script).await.map(Json) })
        })
    }

    #[pyo3(signature = (*, path=None, full_page=None))]
    fn screenshot<'py>(
        &self,
        py: Python<'py>,
        path: Option<PathBuf>,
        full_page: Option<bool>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let full_page = full_page.unwrap_or(false);
        let lease = self.lease.clone();
        future_into_py(py, async move {
            let png = with_page(&lease, move |p| {
                Box::pin(async move {
                    if full_page {
                        p.screenshot_full_page().await
                    } else {
                        p.screenshot().await
                    }
                })
            })
            .await?;
            if let Some(path) = path {
                save(&path, &png)?;
            }
            Ok(png)
        })
    }

    fn set_viewport_size<'py>(
        &self,
        py: Python<'py>,
        viewport_size: Bound<'py, PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let size: Size = options(&viewport_size, "viewport_size")?;
        run(py, &self.lease, move |p| {
            Box::pin(async move { p.set_viewport_size(size.width, size.height).await.map(none) })
        })
    }

    /// Every cookie in this page's context, plus localStorage of the current origin, in
    /// Playwright's format; with `path`, also saved as JSON for `new_page(storage_state=path)`.
    #[pyo3(signature = (*, path=None))]
    fn storage_state<'py>(
        &self,
        py: Python<'py>,
        path: Option<PathBuf>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let lease = self.lease.clone();
        future_into_py(py, async move {
            let state = with_page(&lease, |p| Box::pin(p.storage_state())).await?;
            let json =
                serde_json::to_value(&state).map_err(|e| PyValueError::new_err(e.to_string()))?;
            if let Some(path) = path {
                save_private(
                    &path,
                    serde_json::to_string_pretty(&json).unwrap().as_bytes(),
                )?;
            }
            Ok(Json(json))
        })
    }

    /// Console messages so far from this page, its popups and its iframes (the last 1000).
    fn console_messages<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        run(py, &self.lease, |p| {
            Box::pin(async move {
                Ok(p.console_messages()
                    .into_iter()
                    .map(ConsoleMessage::from)
                    .collect::<Vec<_>>())
            })
        })
    }

    /// Exceptions nothing caught so far (the last 1000), as `fluxwright.Error` objects.
    fn page_errors<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        run(py, &self.lease, |p| {
            Box::pin(async move {
                Ok(p.page_errors()
                    .into_iter()
                    .map(PageError)
                    .collect::<Vec<_>>())
            })
        })
    }

    /// The next download this page finished (finished ones queue up), within `timeout`
    /// milliseconds (default 30 s).
    #[pyo3(signature = (*, timeout=None))]
    fn wait_for_download<'py>(
        &self,
        py: Python<'py>,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let timeout = Duration::from_millis(timeout.unwrap_or(30_000.0).max(0.0) as u64);
        let lease = self.lease.clone();
        future_into_py(py, async move {
            // Hold no page lock while waiting, or close() would wait for the download too.
            let downloads =
                with_page(&lease, |p| Box::pin(async move { Ok(p.downloads()) })).await?;
            let inner = downloads.next(timeout).await.map_err(error)?;
            Ok(Download { inner })
        })
    }

    fn close<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let lease = self.lease.clone();
        future_into_py(py, async move {
            let page = lease.write().await.take();
            if let Some(p) = page {
                let _ = p.close().await;
            }
            Ok(None::<()>)
        })
    }

    /// CSS, `text=`, or `role=` selector.
    fn locator(&self, selector: String) -> Locator {
        self.top().locator(selector)
    }

    #[pyo3(signature = (text, *, exact=None))]
    fn get_by_text(&self, text: &str, exact: Option<bool>) -> Locator {
        self.top().get_by_text(text, exact)
    }

    #[pyo3(signature = (role, *, name=None, exact=None))]
    fn get_by_role(&self, role: &str, name: Option<&str>, exact: Option<bool>) -> Locator {
        self.top().get_by_role(role, name, exact)
    }

    #[pyo3(signature = (text, *, exact=None))]
    fn get_by_label(&self, text: &str, exact: Option<bool>) -> Locator {
        self.top().get_by_label(text, exact)
    }

    #[pyo3(signature = (text, *, exact=None))]
    fn get_by_placeholder(&self, text: &str, exact: Option<bool>) -> Locator {
        self.top().get_by_placeholder(text, exact)
    }

    fn get_by_test_id(&self, test_id: &str) -> Locator {
        self.top().get_by_test_id(test_id)
    }

    fn frame_locator(&self, selector: String) -> FrameLocator {
        self.top().frame_locator(selector)
    }

    /// Console messages and uncaught errors from now on: subscribed before this returns, so
    /// nothing logged after the call is missed.
    #[pyo3(name = "_logs")]
    fn logs(&self) -> PyResult<LogStream> {
        let page = self.lease.try_read().map_err(|_| closed())?;
        let p = page.as_ref().ok_or_else(closed)?;
        Ok(LogStream(Arc::new(Mutex::new(p.subscribe_logs()))))
    }

    /// Starts handing this page's requests over; resolves once interception is on, so a `goto`
    /// after it is covered.
    #[pyo3(name = "_intercept")]
    fn intercept<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        run(py, &self.lease, |p| {
            Box::pin(async move { Ok(RouteStream(Arc::new(Mutex::new(p.intercept().await?)))) })
        })
    }
}

impl Page {
    fn top(&self) -> FrameLocator {
        FrameLocator {
            lease: self.lease.clone(),
            frames: Vec::new(),
        }
    }
}

/// Lazy, like Playwright's: every action runs the query again.
#[pyclass(frozen, module = "fluxwright._native")]
struct Locator {
    lease: LeaseSlot,
    selector: fluxwright::Selector,
}

#[pymethods]
impl Locator {
    fn click<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let sel = self.selector.clone();
        run(py, &self.lease, move |p| {
            Box::pin(async move { p.click(&sel).await.map(none) })
        })
    }

    fn fill<'py>(&self, py: Python<'py>, value: String) -> PyResult<Bound<'py, PyAny>> {
        let sel = self.selector.clone();
        run(py, &self.lease, move |p| {
            Box::pin(async move { p.fill(&sel, &value).await.map(none) })
        })
    }

    /// Waits until visible.
    fn wait_for<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let sel = self.selector.clone();
        run(py, &self.lease, move |p| {
            Box::pin(async move { p.wait_for_selector(&sel).await.map(none) })
        })
    }

    fn text_content<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let sel = self.selector.clone();
        run(py, &self.lease, move |p| {
            Box::pin(async move { p.text_content(&sel).await })
        })
    }

    /// PNG of the element, once it is visible and still; with `path`, also written there.
    #[pyo3(signature = (*, path=None))]
    fn screenshot<'py>(
        &self,
        py: Python<'py>,
        path: Option<PathBuf>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let sel = self.selector.clone();
        let lease = self.lease.clone();
        future_into_py(py, async move {
            let png = with_page(&lease, move |p| {
                Box::pin(async move { p.element_screenshot(&sel).await })
            })
            .await?;
            if let Some(path) = path {
                save(&path, &png)?;
            }
            Ok(png)
        })
    }

    /// `{x, y, width, height}` relative to the viewport, without scrolling; `None` when the
    /// element is not visible.
    fn bounding_box<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let sel = self.selector.clone();
        run(py, &self.lease, move |p| {
            Box::pin(async move {
                let b = p.bounding_box(&sel).await?;
                Ok(b.map(|b| Json(serde_json::json!({ "x": b.x, "y": b.y, "width": b.width, "height": b.height }))))
            })
        })
    }

    /// Calls the JavaScript function `expression` with the element and `arg` (`null` for
    /// Python's `None`, as in Playwright) in the page's own world, and returns its result.
    #[pyo3(signature = (expression, arg=None))]
    fn evaluate<'py>(
        &self,
        py: Python<'py>,
        expression: String,
        arg: Option<Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let arg = Some(arg.map(|a| to_json(&a)).transpose()?.unwrap_or_default());
        let sel = self.selector.clone();
        run(py, &self.lease, move |p| {
            Box::pin(async move { p.evaluate_on(&sel, &expression, arg).await.map(Json) })
        })
    }

    /// The `index`th match (0-based; negative counts from the end).
    fn nth(&self, index: i32) -> Locator {
        self.then(format!("nth={index}"))
    }

    #[getter]
    fn first(&self) -> Locator {
        self.nth(0)
    }

    #[getter]
    fn last(&self) -> Locator {
        self.nth(-1)
    }

    /// Keeps matches containing `has_text` (case-insensitive substring), as in Playwright.
    #[pyo3(signature = (*, has_text=None))]
    fn filter(&self, has_text: Option<&str>) -> Locator {
        match has_text {
            Some(text) => self.then(fluxwright::Selector::has_text(text)),
            None => Locator {
                lease: self.lease.clone(),
                selector: self.selector.clone(),
            },
        }
    }

    /// Searches inside this locator's matches.
    fn locator(&self, selector: String) -> Locator {
        self.then(selector)
    }

    #[pyo3(signature = (text, *, exact=None))]
    fn get_by_text(&self, text: &str, exact: Option<bool>) -> Locator {
        self.then(fluxwright::Selector::text(text, exact.unwrap_or(false)))
    }

    #[pyo3(signature = (role, *, name=None, exact=None))]
    fn get_by_role(&self, role: &str, name: Option<&str>, exact: Option<bool>) -> Locator {
        self.then(fluxwright::Selector::role(
            role,
            name,
            exact.unwrap_or(false),
        ))
    }

    #[pyo3(signature = (text, *, exact=None))]
    fn get_by_label(&self, text: &str, exact: Option<bool>) -> Locator {
        self.then(fluxwright::Selector::label(text, exact.unwrap_or(false)))
    }

    #[pyo3(signature = (text, *, exact=None))]
    fn get_by_placeholder(&self, text: &str, exact: Option<bool>) -> Locator {
        self.then(fluxwright::Selector::placeholder(
            text,
            exact.unwrap_or(false),
        ))
    }

    fn get_by_test_id(&self, test_id: &str) -> Locator {
        self.then(fluxwright::Selector::test_id(test_id))
    }

    fn __repr__(&self) -> String {
        format!("<Locator selector={:?}>", self.selector.to_string())
    }
}

impl Locator {
    fn then(&self, part: String) -> Locator {
        Locator {
            lease: self.lease.clone(),
            selector: self.selector.then(&part),
        }
    }
}

/// An iframe, same- or cross-origin, to find elements in.
#[pyclass(frozen, module = "fluxwright._native")]
struct FrameLocator {
    lease: LeaseSlot,
    frames: Vec<String>,
}

#[pymethods]
impl FrameLocator {
    fn locator(&self, selector: String) -> Locator {
        self.find(selector)
    }

    #[pyo3(signature = (text, *, exact=None))]
    fn get_by_text(&self, text: &str, exact: Option<bool>) -> Locator {
        self.find(fluxwright::Selector::text(text, exact.unwrap_or(false)))
    }

    #[pyo3(signature = (role, *, name=None, exact=None))]
    fn get_by_role(&self, role: &str, name: Option<&str>, exact: Option<bool>) -> Locator {
        self.find(fluxwright::Selector::role(
            role,
            name,
            exact.unwrap_or(false),
        ))
    }

    /// `<label>`, `aria-labelledby` or `aria-label`; case-insensitive substring unless `exact`.
    #[pyo3(signature = (text, *, exact=None))]
    fn get_by_label(&self, text: &str, exact: Option<bool>) -> Locator {
        self.find(fluxwright::Selector::label(text, exact.unwrap_or(false)))
    }

    #[pyo3(signature = (text, *, exact=None))]
    fn get_by_placeholder(&self, text: &str, exact: Option<bool>) -> Locator {
        self.find(fluxwright::Selector::placeholder(
            text,
            exact.unwrap_or(false),
        ))
    }

    /// `data-testid`, exact.
    fn get_by_test_id(&self, test_id: &str) -> Locator {
        self.find(fluxwright::Selector::test_id(test_id))
    }

    /// A nested iframe inside this one.
    fn frame_locator(&self, selector: String) -> FrameLocator {
        let mut frames = self.frames.clone();
        frames.push(selector);
        FrameLocator {
            lease: self.lease.clone(),
            frames,
        }
    }
}

impl FrameLocator {
    fn find(&self, query: String) -> Locator {
        Locator {
            lease: self.lease.clone(),
            selector: fluxwright::Selector {
                frames: self.frames.clone(),
                query,
            },
        }
    }
}

/// A paused request; `fluxwright.async_api` wraps it in Playwright's `Route` and `Request`.
#[pyclass(frozen, module = "fluxwright._native")]
struct Route {
    request: Arc<Mutex<Option<fluxwright::InterceptedRequest>>>,
    #[pyo3(get)]
    url: String,
    #[pyo3(get)]
    method: String,
    #[pyo3(get)]
    headers: HashMap<String, String>,
    #[pyo3(get)]
    post_data: Option<String>,
    #[pyo3(get)]
    resource_type: String,
}

impl Route {
    fn new(req: fluxwright::InterceptedRequest) -> Self {
        Route {
            url: req.url.clone(),
            method: req.method.clone(),
            headers: req.headers.iter().cloned().collect(),
            post_data: req.post_data.clone(),
            resource_type: req.resource_type.clone(),
            request: Arc::new(Mutex::new(Some(req))),
        }
    }

    /// Takes the request to answer it; a second answer fails, as in Playwright.
    async fn take(
        slot: &Mutex<Option<fluxwright::InterceptedRequest>>,
    ) -> PyResult<fluxwright::InterceptedRequest> {
        slot.lock()
            .await
            .take()
            .ok_or_else(|| raise("Error", "route is already handled".into()))
    }
}

#[pymethods]
impl Route {
    #[pyo3(signature = (*, status=None, headers=None, body=None))]
    fn fulfill<'py>(
        &self,
        py: Python<'py>,
        status: Option<i64>,
        headers: Option<HashMap<String, String>>,
        body: Option<Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        if let Some(s) = status.filter(|s| !(100..=599).contains(s)) {
            return Err(PyValueError::new_err(format!(
                "status must be between 100 and 599, not {s}"
            )));
        }
        let fulfill = fluxwright::Fulfill {
            status: status.map(|s| s as u16),
            headers: headers.unwrap_or_default().into_iter().collect(),
            body: body.map(|b| bytes(&b)).transpose()?.unwrap_or_default(),
        };
        let slot = self.request.clone();
        future_into_py(py, async move {
            Route::take(&slot)
                .await?
                .fulfill(fulfill)
                .await
                .map(none)
                .map_err(error)
        })
    }

    /// Sends the request on, changed if asked; `headers` replaces all of them.
    #[pyo3(name = "continue_", signature = (*, url=None, method=None, headers=None, post_data=None))]
    fn continue_request<'py>(
        &self,
        py: Python<'py>,
        url: Option<String>,
        method: Option<String>,
        headers: Option<HashMap<String, String>>,
        post_data: Option<Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let overrides = fluxwright::Overrides {
            url,
            method,
            headers: headers.map(|h| h.into_iter().collect()),
            post_data: post_data.map(|b| bytes(&b)).transpose()?,
        };
        let slot = self.request.clone();
        future_into_py(py, async move {
            Route::take(&slot)
                .await?
                .continue_with(overrides)
                .await
                .map(none)
                .map_err(error)
        })
    }

    /// `error_code` as in Playwright: `failed` (default), `aborted`, `blockedbyclient`, ...
    #[pyo3(signature = (error_code=None))]
    fn abort<'py>(
        &self,
        py: Python<'py>,
        error_code: Option<String>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let code = error_code.unwrap_or_else(|| "failed".into());
        let slot = self.request.clone();
        future_into_py(py, async move {
            Route::take(&slot)
                .await?
                .abort(&code)
                .await
                .map(none)
                .map_err(error)
        })
    }
}

/// Requests `page.route` intercepted, in order.
#[pyclass(frozen, module = "fluxwright._native")]
struct RouteStream(Arc<Mutex<UnboundedReceiver<fluxwright::InterceptedRequest>>>);

#[pymethods]
impl RouteStream {
    /// The next request, or `None` once the page closed.
    fn next<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let rx = self.0.clone();
        future_into_py(py, async move {
            Ok(rx.lock().await.recv().await.map(Route::new))
        })
    }
}

/// Console messages and uncaught errors, as they happen.
#[pyclass(frozen, module = "fluxwright._native")]
struct LogStream(Arc<Mutex<UnboundedReceiver<fluxwright::LogEntry>>>);

#[pymethods]
impl LogStream {
    /// The next `ConsoleMessage` or `fluxwright.Error`, or `None` once the page closed.
    fn next<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let rx = self.0.clone();
        future_into_py(py, async move { Ok(rx.lock().await.recv().await.map(Log)) })
    }
}

struct Log(fluxwright::LogEntry);

impl<'py> IntoPyObject<'py> for Log {
    type Target = PyAny;
    type Output = Bound<'py, PyAny>;
    type Error = PyErr;

    fn into_pyobject(self, py: Python<'py>) -> PyResult<Self::Output> {
        match self.0 {
            fluxwright::LogEntry::Console(m) => {
                Ok(Bound::new(py, ConsoleMessage::from(m))?.into_any())
            }
            fluxwright::LogEntry::Error(e) => PageError(e).into_pyobject(py),
        }
    }
}

/// An exception nothing caught, as Playwright hands it over: a `fluxwright.Error` with `name`,
/// `message` and `stack`, given rather than raised.
struct PageError(fluxwright::PageError);

impl<'py> IntoPyObject<'py> for PageError {
    type Target = PyAny;
    type Output = Bound<'py, PyAny>;
    type Error = PyErr;

    fn into_pyobject(self, py: Python<'py>) -> PyResult<Self::Output> {
        let e = self.0;
        py.import("fluxwright._errors")?
            .getattr("Error")?
            .call1((e.message, e.name, e.stack))
    }
}

#[pyclass(frozen, module = "fluxwright._native")]
struct ConsoleMessage {
    /// `log`, `error`, `warning`, `info`, `debug`, ...
    #[pyo3(get, name = "type")]
    kind: String,
    #[pyo3(get)]
    text: String,
}

#[pymethods]
impl ConsoleMessage {
    fn __repr__(&self) -> String {
        format!("<ConsoleMessage type={:?} text={:?}>", self.kind, self.text)
    }
}

impl From<fluxwright::ConsoleMessage> for ConsoleMessage {
    fn from(m: fluxwright::ConsoleMessage) -> Self {
        ConsoleMessage {
            kind: m.kind,
            text: m.text,
        }
    }
}

/// A file the page downloaded; deleted when the page closes, so `save_as` it first.
#[pyclass(frozen, module = "fluxwright._native")]
struct Download {
    inner: fluxwright::Download,
}

#[pymethods]
impl Download {
    #[getter]
    fn url(&self) -> String {
        self.inner.url.clone()
    }

    /// From `Content-Disposition` or the URL.
    #[getter]
    fn suggested_filename(&self) -> String {
        self.inner.suggested_filename.clone()
    }

    /// The temporary file.
    fn path<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let path = self.inner.path.clone();
        future_into_py(py, async move { Ok(path) })
    }

    /// Copies the file to `path`, creating missing folders.
    fn save_as<'py>(&self, py: Python<'py>, path: PathBuf) -> PyResult<Bound<'py, PyAny>> {
        let download = self.inner.clone();
        future_into_py(py, async move { Ok(download.save_as(path).map(none)?) })
    }

    /// Always `None`: only finished downloads are handed over.
    fn failure<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        future_into_py(py, async move { Ok(None::<String>) })
    }

    fn __repr__(&self) -> String {
        format!(
            "<Download url={:?} suggested_filename={:?}>",
            self.inner.url, self.inner.suggested_filename
        )
    }
}

#[pymodule]
fn _native(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<Engine>()?;
    m.add_class::<Lease>()?;
    m.add_class::<Page>()?;
    m.add_class::<Locator>()?;
    m.add_class::<FrameLocator>()?;
    m.add_class::<Route>()?;
    m.add_class::<RouteStream>()?;
    m.add_class::<LogStream>()?;
    m.add_class::<ConsoleMessage>()?;
    m.add_class::<Download>()?;
    Ok(())
}
