//! The native half of the `fluxwright` Python package: the engine, pages and locators, with
//! awaitable methods. `fluxwright/async_api.py` adds what needs Python (routes, events), and
//! `fluxwright/sync_api.py` the blocking API.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyCFunction};
use pyo3::IntoPyObjectExt;
use serde::Deserialize;
use tokio::runtime::Runtime;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::{Mutex, RwLock};

/// Read-locked by every page call, so calls run side by side (a download wait and the click
/// that starts it); write-locked by `close`, which raises `closing` first so in-flight calls
/// unblock immediately instead of holding the lock until timeout.
struct PageSlot {
    page: RwLock<Option<fluxwright::PageLease>>,
    closing: tokio::sync::watch::Sender<bool>,
}

type LeaseSlot = Arc<PageSlot>;

type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// What an awaitable that answers nothing resolves with: `None` (a bare `()` would arrive as an
/// empty tuple).
type Nothing = Option<()>;

fn none<T>(_: T) -> Nothing {
    None
}

/// `fluxwright.Error` and `fluxwright.TimeoutError` as lazy `PyErr`s: making one needs no
/// Python, so futures make them on Tokio's workers (where waiting for the GIL would hold up
/// other tasks), and Python builds the exception when it raises it.
mod py_errors {
    pyo3::import_exception!(fluxwright._errors, Error);
    pyo3::import_exception!(fluxwright._errors, TimeoutError);
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
    if timeout {
        py_errors::TimeoutError::new_err(e.to_string())
    } else {
        py_errors::Error::new_err(e.to_string())
    }
}

fn closed() -> PyErr {
    py_errors::Error::new_err("page closed")
}

/// Runs `f` against the page; fails with "page closed" after `close()`, and cancels immediately
/// when `close()` is called while running.
async fn with_page<T, F>(lease: &LeaseSlot, f: F) -> PyResult<T>
where
    F: for<'a> FnOnce(&'a fluxwright::PageLease) -> BoxFuture<'a, fluxwright::Result<T>>,
{
    with_page_in(lease, None, f).await
}

/// Playwright's `timeout` in milliseconds: 0 means no timeout, and anything that is not a time
/// (negative, NaN) leaves the default in place.
fn call_timeout(ms: Option<f64>) -> Option<Duration> {
    match ms {
        Some(ms) if ms == 0.0 => Some(Duration::from_secs(365 * 24 * 60 * 60)),
        Some(ms) if ms > 0.0 && ms.is_finite() => Some(Duration::from_secs_f64(ms / 1000.0)),
        _ => None,
    }
}

/// `with_page` with the call's own timeout in place of the page's and the engine's.
async fn with_page_in<T, F>(lease: &LeaseSlot, timeout: Option<Duration>, f: F) -> PyResult<T>
where
    F: for<'a> FnOnce(&'a fluxwright::PageLease) -> BoxFuture<'a, fluxwright::Result<T>>,
{
    let mut closing = lease.closing.subscribe();
    let guard = lease.page.read().await;
    let Some(p) = guard.as_ref() else {
        return Err(closed());
    };
    tokio::select! {
        r = fluxwright::with_timeout(timeout, f(p)) => r.map_err(error),
        _ = async { let _ = closing.wait_for(|c| *c).await; } => Err(closed()),
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
    run_in(py, lease, None, f)
}

/// `run` with the call's `timeout` option (milliseconds).
fn run_in<'py, T, F>(
    py: Python<'py>,
    lease: &LeaseSlot,
    timeout: Option<f64>,
    f: F,
) -> PyResult<Bound<'py, PyAny>>
where
    T: for<'a> IntoPyObject<'a> + Send + 'static,
    F: for<'a> FnOnce(&'a fluxwright::PageLease) -> BoxFuture<'a, fluxwright::Result<T>>
        + Send
        + 'static,
{
    let lease = lease.clone();
    let timeout = call_timeout(timeout);
    future_into_py(py, async move { with_page_in(&lease, timeout, f).await })
}

static RT: LazyLock<Runtime> = LazyLock::new(|| Runtime::new().expect("tokio"));

/// Results being handed to Python right now, on Tokio's blocking threads.
static HANDING_OVER: AtomicUsize = AtomicUsize::new(0);

/// Set first thing by `wait_for_hand_overs`: a hand-over that starts later never attaches.
static EXITING: AtomicBool = AtomicBool::new(false);

/// `fut` as an asyncio future on the running loop; cancelling that future drops `fut`.
///
/// Counts its hand-overs for `wait_for_hand_overs`: a hand-over releases the GIL midway
/// (`call_soon_threadsafe` writes to the loop's wake-up socket), so the script can finish in
/// that gap, and if the interpreter is shutting down when the thread takes the GIL back, Python
/// 3.10-3.13 end the thread and the process crashes (SIGSEGV) while Rust unwinds it. One that
/// starts after that wait drops its result instead: the sync API's loop keeps futures pending
/// past exit, and a page freed during shutdown ends its event streams then. The abi3 build
/// can't ask whether Python is finalizing (PyO3 checks only in 3.13+ builds).
fn future_into_py<'py, T, F>(py: Python<'py>, fut: F) -> PyResult<Bound<'py, PyAny>>
where
    F: Future<Output = PyResult<T>> + Send + 'static,
    T: for<'a> IntoPyObject<'a> + Send + 'static,
{
    // Its result could never be handed over: fail now rather than leave the caller waiting for
    // good (an atexit handler registered before fluxwright was imported runs after its own).
    if EXITING.load(Ordering::SeqCst) {
        return Err(py_errors::Error::new_err("fluxwright stopped at interpreter exit"));
    }
    let event_loop = py.import("asyncio")?.call_method0("get_running_loop")?;
    let future = event_loop.call_method0("create_future")?;
    let task = RT.spawn(fut);
    let abort = task.abort_handle();
    let on_done = PyCFunction::new_closure(py, None, None, move |args, _| -> PyResult<()> {
        if args.get_item(0)?.call_method0("cancelled")?.is_truthy()? {
            abort.abort();
        }
        Ok(())
    })?;
    future.call_method1("add_done_callback", (on_done,))?;
    let (event_loop, target) = (event_loop.unbind(), future.clone().unbind());
    RT.spawn(async move {
        let result = match task.await {
            Ok(result) => result,
            Err(e) if e.is_panic() => Err(PyRuntimeError::new_err("the Rust future panicked")),
            Err(_) => return, // cancelled from Python
        };
        // Not on a Tokio worker: waiting there for the GIL would hold up its other tasks.
        RT.spawn_blocking(move || {
            HANDING_OVER.fetch_add(1, Ordering::SeqCst);
            // Counted before the check, so the exit wait either waits for this one or this one
            // sees EXITING. Skipped, the Python objects are released later, without the GIL.
            if !EXITING.load(Ordering::SeqCst) {
                Python::attach(move |py| {
                    let (method, value) = match result.and_then(|v| v.into_bound_py_any(py)) {
                        Ok(v) => ("set_result", v),
                        Err(e) => ("set_exception", e.into_value(py).into_bound(py).into_any()),
                    };
                    // Fails when the loop is closed: then nothing waits for the result.
                    let _ = wrap_pyfunction!(hand_over, py).and_then(|f| {
                        event_loop.call_method1(py, "call_soon_threadsafe", (f, target.bind(py), method, value))
                    });
                });
            }
            HANDING_OVER.fetch_sub(1, Ordering::SeqCst);
        });
    });
    Ok(future)
}

/// Sets `future`'s result or exception (`method`), unless it was cancelled meanwhile.
#[pyfunction]
fn hand_over(future: &Bound<'_, PyAny>, method: &str, value: &Bound<'_, PyAny>) -> PyResult<()> {
    if !future.call_method0("done")?.is_truthy()? {
        future.call_method1(method, (value,))?;
    }
    Ok(())
}

/// Runs at exit (`atexit`, while the interpreter is still whole): stops hand-overs that have
/// not started, then lets the ones in progress finish, for at most 2 s.
#[pyfunction]
fn wait_for_hand_overs(py: Python<'_>) {
    EXITING.store(true, Ordering::SeqCst);
    py.detach(|| {
        let give_up = Instant::now() + Duration::from_secs(2);
        while HANDING_OVER.load(Ordering::SeqCst) > 0 && Instant::now() < give_up {
            std::thread::sleep(Duration::from_millis(1));
        }
    });
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

/// `save` on a blocking thread, handing `data` back: a Tokio worker held by a large write delays
/// every page's browser traffic.
async fn save_blocking(path: PathBuf, data: Vec<u8>) -> PyResult<Vec<u8>> {
    RT.spawn_blocking(move || save(&path, &data).map(|()| data))
        .await
        .map_err(|e| PyRuntimeError::new_err(e.to_string()))?
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ClipRect {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

/// A Playwright length: a number of pixels, or a string with a unit.
#[derive(Deserialize)]
#[serde(untagged)]
enum Length {
    Pixels(f64),
    Text(String),
}

impl Length {
    fn text(self) -> String {
        match self {
            Length::Pixels(n) => n.to_string(),
            Length::Text(s) => s,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Margin {
    top: Option<Length>,
    right: Option<Length>,
    bottom: Option<Length>,
    left: Option<Length>,
}

fn length(value: &Bound<'_, PyAny>, what: &str) -> PyResult<String> {
    options::<Length>(value, what).map(Length::text)
}

/// The screenshot type as Playwright picks it: `type`, else the extension of `path`, else PNG.
fn shot_options(
    path: Option<&Path>,
    kind: Option<&str>,
    quality: Option<u32>,
    omit_background: Option<bool>,
) -> PyResult<fluxwright::ScreenshotOptions> {
    let kind = match (kind, path) {
        (Some(k), _) => Some(k),
        (None, Some(p)) => {
            let ext = p.extension().and_then(|e| e.to_str()).unwrap_or_default().to_ascii_lowercase();
            Some(match ext.as_str() {
                "png" => "png",
                "jpg" | "jpeg" => "jpeg",
                _ => {
                    return Err(PyValueError::new_err(format!(
                        "screenshot: {}: only .png, .jpg and .jpeg are supported",
                        p.display()
                    )))
                }
            })
        }
        (None, None) => None,
    };
    let format = match kind {
        None | Some("png") => fluxwright::ImageFormat::Png,
        Some("jpeg") => fluxwright::ImageFormat::Jpeg,
        Some(k) => return Err(PyValueError::new_err(format!("screenshot: type {k:?} is not png or jpeg"))),
    };
    if format == fluxwright::ImageFormat::Png && quality.is_some() {
        return Err(PyValueError::new_err("screenshot: quality is for JPEG only"));
    }
    Ok(fluxwright::ScreenshotOptions {
        format,
        // Over 255 becomes 255, which the engine rejects as over 100.
        quality: quality.map(|q| q.min(255) as u8),
        omit_background: omit_background.unwrap_or(false),
        ..Default::default()
    })
}

/// Saved storage: what `storage_state()` returned, or the path of the JSON file it saved.
fn storage(value: &Bound<'_, PyAny>) -> PyResult<fluxwright::StorageState> {
    match value.extract::<PathBuf>() {
        Ok(path) => value
            .py()
            .detach(|| std::fs::read_to_string(&path).map(|s| serde_json::from_str::<fluxwright::StorageState>(&s)))?
            .map_err(|e| PyValueError::new_err(format!("storage_state {}: {e}", path.display()))),
        Err(_) => options(value, "storage_state"),
    }
}

/// `queue_timeout` (milliseconds) as the engine's acquire timeout, which also covers starting a
/// browser; `None` or 0 waits as long as the queue takes.
fn queue_wait(ms: Option<f64>) -> Duration {
    match ms {
        Some(ms) if ms > 0.0 => Duration::from_millis(ms as u64),
        // ponytail: a year stands in for "no limit", so deadline arithmetic never overflows.
        _ => Duration::from_secs(365 * 24 * 60 * 60),
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
    #[pyo3(signature = (*, max_browsers=None, executable_path=None, headless=None, queue_timeout=None, memory_ceiling_mb=None))]
    fn launch(
        py: Python<'_>,
        max_browsers: Option<usize>,
        executable_path: Option<PathBuf>,
        headless: Option<bool>,
        queue_timeout: Option<f64>,
        memory_ceiling_mb: Option<u64>,
    ) -> PyResult<Bound<'_, PyAny>> {
        future_into_py(py, async move {
            // new_page waits its turn however long the queue is, so starting every job at once
            // works; the engine's own default (256 waiting, then an error) is for a server
            // shedding load.
            let mut b = fluxwright::BrowserEngine::builder()
                .max_contexts_per_browser(8)
                .queue_capacity(usize::MAX)
                .acquire_timeout(queue_wait(queue_timeout));
            if let Some(n) = max_browsers {
                b = b.max_browsers(n);
            }
            if let Some(mb) = memory_ceiling_mb {
                b = b.memory_ceiling_mb(mb);
            }
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
        let (closing, _) = tokio::sync::watch::channel(false);
        Ok(Page {
            lease: Arc::new(PageSlot {
                page: RwLock::new(Some(lease)),
                closing,
            }),
        })
    }

    #[pyo3(signature = (url, *, wait_until=None, timeout=None))]
    fn goto<'py>(
        &self,
        py: Python<'py>,
        url: String,
        wait_until: Option<String>,
        timeout: Option<f64>,
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
        run_in(py, &self.lease, timeout, move |p| {
            Box::pin(async move { p.goto_with(&url, wait).await.map(none) })
        })
    }

    fn title<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        run(py, &self.lease, |p| Box::pin(p.title()))
    }

    fn content<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        run(py, &self.lease, |p| Box::pin(p.content()))
    }

    #[pyo3(signature = (selector, *, timeout=None))]
    fn click<'py>(
        &self,
        py: Python<'py>,
        selector: String,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        run_in(py, &self.lease, timeout, move |p| {
            Box::pin(async move { p.click(selector.as_str()).await.map(none) })
        })
    }

    #[pyo3(signature = (selector, value, *, timeout=None))]
    fn fill<'py>(
        &self,
        py: Python<'py>,
        selector: String,
        value: String,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        run_in(py, &self.lease, timeout, move |p| {
            Box::pin(async move { p.fill(selector.as_str(), &value).await.map(none) })
        })
    }

    #[pyo3(signature = (selector, *, timeout=None))]
    fn wait_for_selector<'py>(
        &self,
        py: Python<'py>,
        selector: String,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        run_in(py, &self.lease, timeout, move |p| {
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

    /// PNG or JPEG bytes; with `path`, also written there (its extension picks the type when
    /// `type` is not given). `clip` is `{x, y, width, height}` relative to the viewport, or to
    /// the page with `full_page`.
    #[pyo3(signature = (*, path=None, full_page=None, r#type=None, quality=None, clip=None, omit_background=None,
        timeout=None))]
    #[allow(clippy::too_many_arguments)]
    fn screenshot<'py>(
        &self,
        py: Python<'py>,
        path: Option<PathBuf>,
        full_page: Option<bool>,
        r#type: Option<String>,
        quality: Option<u32>,
        clip: Option<Bound<'py, PyAny>>,
        omit_background: Option<bool>,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let timeout = call_timeout(timeout);
        let mut shot = shot_options(path.as_deref(), r#type.as_deref(), quality, omit_background)?;
        shot.full_page = full_page.unwrap_or(false);
        if let Some(clip) = clip {
            let c: ClipRect = options(&clip, "clip")?;
            shot.clip = Some(fluxwright::Clip { x: c.x, y: c.y, width: c.width, height: c.height });
        }
        let lease = self.lease.clone();
        future_into_py(py, async move {
            let image = with_page_in(&lease, timeout, move |p| {
                Box::pin(async move { p.screenshot_with(&shot).await })
            })
            .await?;
            match path {
                Some(path) => save_blocking(path, image).await,
                None => Ok(image),
            }
        })
    }

    /// The page printed to PDF, as Playwright's `page.pdf` (headless only); with `path`, also
    /// written there. Lengths (`width`, `height`, the `margin` sides) are numbers of pixels or
    /// strings with `px`, `in`, `cm` or `mm`.
    #[pyo3(signature = (*, path=None, format=None, width=None, height=None, margin=None, landscape=None,
        print_background=None, scale=None, page_ranges=None, prefer_css_page_size=None,
        display_header_footer=None, header_template=None, footer_template=None, outline=None, tagged=None))]
    #[allow(clippy::too_many_arguments)]
    fn pdf<'py>(
        &self,
        py: Python<'py>,
        path: Option<PathBuf>,
        format: Option<String>,
        width: Option<Bound<'py, PyAny>>,
        height: Option<Bound<'py, PyAny>>,
        margin: Option<Bound<'py, PyAny>>,
        landscape: Option<bool>,
        print_background: Option<bool>,
        scale: Option<f64>,
        page_ranges: Option<String>,
        prefer_css_page_size: Option<bool>,
        display_header_footer: Option<bool>,
        header_template: Option<String>,
        footer_template: Option<String>,
        outline: Option<bool>,
        tagged: Option<bool>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let margin = match margin {
            Some(m) => {
                let m: Margin = options(&m, "margin")?;
                fluxwright::PdfMargin {
                    top: m.top.map(Length::text),
                    right: m.right.map(Length::text),
                    bottom: m.bottom.map(Length::text),
                    left: m.left.map(Length::text),
                }
            }
            None => Default::default(),
        };
        let pdf_options = fluxwright::PdfOptions {
            format,
            width: width.map(|w| length(&w, "width")).transpose()?,
            height: height.map(|h| length(&h, "height")).transpose()?,
            margin,
            landscape: landscape.unwrap_or(false),
            print_background: print_background.unwrap_or(false),
            scale,
            page_ranges,
            prefer_css_page_size: prefer_css_page_size.unwrap_or(false),
            display_header_footer: display_header_footer.unwrap_or(false),
            header_template,
            footer_template,
            outline: outline.unwrap_or(false),
            tagged: tagged.unwrap_or(false),
        };
        let lease = self.lease.clone();
        future_into_py(py, async move {
            let pdf = with_page(&lease, move |p| Box::pin(async move { p.pdf(&pdf_options).await })).await?;
            match path {
                Some(path) => save_blocking(path, pdf).await,
                None => Ok(pdf),
            }
        })
    }

    /// The timeout in milliseconds for this page's calls from now on, navigation included; 0
    /// for none. A call's own `timeout` wins.
    fn set_default_timeout(&self, timeout: f64) {
        self.set_timeouts(timeout, false);
    }

    /// The timeout in milliseconds for this page's `goto` from now on; 0 for none.
    fn set_default_navigation_timeout(&self, timeout: f64) {
        self.set_timeouts(timeout, true);
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
                let bytes = serde_json::to_vec_pretty(&json)
                    .map_err(|e| PyValueError::new_err(e.to_string()))?;
                RT.spawn_blocking(move || save_private(&path, &bytes))
                    .await
                    .map_err(|e| PyRuntimeError::new_err(e.to_string()))??;
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
        // 0 waits as long as it takes, as in Playwright (it used to fail at once).
        let timeout = queue_wait(Some(timeout.unwrap_or(30_000.0)));
        let lease = self.lease.clone();
        future_into_py(py, async move {
            // Hold no page lock while waiting, or close() would wait for the download too.
            let mut closing = lease.closing.subscribe();
            let downloads =
                with_page(&lease, |p| Box::pin(async move { Ok(p.downloads()) })).await?;
            let inner = tokio::select! {
                r = downloads.next(timeout) => r.map_err(error)?,
                _ = async { let _ = closing.wait_for(|c| *c).await; } => return Err(closed()),
            };
            Ok(Download { inner })
        })
    }

    /// Drops downloads that already finished, so the next `wait_for_download` waits for a new
    /// one: `expect_download` starts with it.
    #[pyo3(name = "_clear_downloads")]
    fn clear_downloads<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let lease = self.lease.clone();
        future_into_py(py, async move {
            // As in wait_for_download: no page lock while waiting for the downloads.
            let downloads =
                with_page(&lease, |p| Box::pin(async move { Ok(p.downloads()) })).await?;
            downloads.clear().await;
            Ok(None::<()>)
        })
    }

    fn close<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        // Calls still running unblock at once (see `with_page`), so the write lock comes right away.
        self.lease.closing.send_replace(true);
        let lease = self.lease.clone();
        future_into_py(py, async move {
            let page = lease.page.write().await.take();
            if let Some(p) = page {
                let _ = p.close().await;
            }
            Ok(None::<()>)
        })
    }

    /// Whether `close()` was called (or is under way).
    fn is_closed(&self) -> bool {
        *self.lease.closing.borrow()
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
        self.now(|p| LogStream(Arc::new(Mutex::new(p.subscribe_logs()))))
    }

    /// Network events from now on: subscribed before this returns, so a navigation right after
    /// it is seen whole.
    #[pyo3(name = "_network")]
    fn network(&self) -> PyResult<NetworkStream> {
        self.now(|p| NetworkStream(Arc::new(Mutex::new(p.subscribe_network()))))
    }

    /// Dialogs from now on, for the caller to answer instead of the engine dismissing them.
    #[pyo3(name = "_dialogs")]
    fn dialogs(&self) -> PyResult<DialogStream> {
        self.now(|p| DialogStream(Arc::new(Mutex::new(p.subscribe_dialogs()))))
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

    fn now<T>(&self, f: impl FnOnce(&fluxwright::PageLease) -> T) -> PyResult<T> {
        if *self.lease.closing.borrow() {
            return Err(closed());
        }
        let page = self.lease.page.try_read().map_err(|_| closed())?;
        let p = page.as_ref().ok_or_else(closed)?;
        Ok(f(p))
    }

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
    #[pyo3(signature = (*, timeout=None))]
    fn click<'py>(&self, py: Python<'py>, timeout: Option<f64>) -> PyResult<Bound<'py, PyAny>> {
        let sel = self.selector.clone();
        run_in(py, &self.lease, timeout, move |p| {
            Box::pin(async move { p.click(&sel).await.map(none) })
        })
    }

    #[pyo3(signature = (value, *, timeout=None))]
    fn fill<'py>(
        &self,
        py: Python<'py>,
        value: String,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let sel = self.selector.clone();
        run_in(py, &self.lease, timeout, move |p| {
            Box::pin(async move { p.fill(&sel, &value).await.map(none) })
        })
    }

    /// Waits until visible.
    #[pyo3(signature = (*, timeout=None))]
    fn wait_for<'py>(&self, py: Python<'py>, timeout: Option<f64>) -> PyResult<Bound<'py, PyAny>> {
        let sel = self.selector.clone();
        run_in(py, &self.lease, timeout, move |p| {
            Box::pin(async move { p.wait_for_selector(&sel).await.map(none) })
        })
    }

    #[pyo3(signature = (*, timeout=None))]
    fn text_content<'py>(&self, py: Python<'py>, timeout: Option<f64>) -> PyResult<Bound<'py, PyAny>> {
        let sel = self.selector.clone();
        run_in(py, &self.lease, timeout, move |p| {
            Box::pin(async move { p.text_content(&sel).await })
        })
    }

    /// PNG or JPEG of the element, once it is visible and still; with `path`, also written
    /// there (its extension picks the type when `type` is not given).
    #[pyo3(signature = (*, path=None, r#type=None, quality=None, omit_background=None, timeout=None))]
    fn screenshot<'py>(
        &self,
        py: Python<'py>,
        path: Option<PathBuf>,
        r#type: Option<String>,
        quality: Option<u32>,
        omit_background: Option<bool>,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let timeout = call_timeout(timeout);
        let shot = shot_options(path.as_deref(), r#type.as_deref(), quality, omit_background)?;
        let sel = self.selector.clone();
        let lease = self.lease.clone();
        future_into_py(py, async move {
            let image = with_page_in(&lease, timeout, move |p| {
                Box::pin(async move { p.element_screenshot_with(&sel, &shot).await })
            })
            .await?;
            match path {
                Some(path) => save_blocking(path, image).await,
                None => Ok(image),
            }
        })
    }

    /// `{x, y, width, height}` relative to the viewport, without scrolling; `None` when the
    /// element is not visible.
    #[pyo3(signature = (*, timeout=None))]
    fn bounding_box<'py>(&self, py: Python<'py>, timeout: Option<f64>) -> PyResult<Bound<'py, PyAny>> {
        let sel = self.selector.clone();
        run_in(py, &self.lease, timeout, move |p| {
            Box::pin(async move {
                let b = p.bounding_box(&sel).await?;
                Ok(b.map(|b| Json(serde_json::json!({ "x": b.x, "y": b.y, "width": b.width, "height": b.height }))))
            })
        })
    }

    /// Calls the JavaScript function `expression` with the element and `arg` (`null` for
    /// Python's `None`, as in Playwright) in the page's own world, and returns its result.
    #[pyo3(signature = (expression, arg=None, *, timeout=None))]
    fn evaluate<'py>(
        &self,
        py: Python<'py>,
        expression: String,
        arg: Option<Bound<'py, PyAny>>,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let arg = Some(arg.map(|a| to_json(&a)).transpose()?.unwrap_or_default());
        let sel = self.selector.clone();
        run_in(py, &self.lease, timeout, move |p| {
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
            .ok_or_else(|| py_errors::Error::new_err("route is already handled"))
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

/// A request, as network events hand it over; `fluxwright.async_api` wraps it in `Request`.
#[pyclass(frozen, module = "fluxwright._native")]
struct NetworkRequest {
    /// Chrome's request id: the same for every event of one request.
    #[pyo3(get)]
    id: String,
    #[pyo3(get)]
    url: String,
    #[pyo3(get)]
    method: String,
    /// Names lower-cased, as Playwright reports them.
    #[pyo3(get)]
    headers: HashMap<String, String>,
    #[pyo3(get)]
    post_data: Option<String>,
    #[pyo3(get)]
    resource_type: String,
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

/// A response; `fluxwright.async_api` wraps it in `Response`.
#[pyclass(frozen, module = "fluxwright._native")]
struct NetworkResponse {
    inner: fluxwright::NetResponse,
}

#[pymethods]
impl NetworkResponse {
    #[getter]
    fn url(&self) -> String {
        self.inner.url.clone()
    }

    #[getter]
    fn status(&self) -> u16 {
        self.inner.status
    }

    #[getter]
    fn status_text(&self) -> String {
        self.inner.status_text.clone()
    }

    #[getter]
    fn headers(&self) -> HashMap<String, String> {
        self.inner.headers.iter().cloned().collect()
    }

    #[getter]
    fn request(&self) -> NetworkRequest {
        self.inner.request.clone().into()
    }

    /// The body, once the response finished loading, within `timeout` milliseconds (default
    /// 30 s).
    #[pyo3(signature = (*, timeout=None))]
    fn body<'py>(&self, py: Python<'py>, timeout: Option<f64>) -> PyResult<Bound<'py, PyAny>> {
        let inner = self.inner.clone();
        // 0 waits as long as it takes, as in Playwright (it used to fail at once).
        let timeout = queue_wait(Some(timeout.unwrap_or(30_000.0)));
        future_into_py(py, async move { inner.body(timeout).await.map_err(error) })
    }
}

/// One network event: `request`, `response`, `requestfinished` or `requestfailed`.
#[pyclass(frozen, module = "fluxwright._native")]
struct NetworkEvent {
    #[pyo3(get)]
    event: String,
    request: fluxwright::NetRequest,
    response: Option<fluxwright::NetResponse>,
    /// Why a failed request failed (`net::ERR_FAILED`, ...).
    #[pyo3(get)]
    failure: Option<String>,
}

#[pymethods]
impl NetworkEvent {
    #[getter]
    fn request(&self) -> NetworkRequest {
        self.request.clone().into()
    }

    #[getter]
    fn response(&self) -> Option<NetworkResponse> {
        self.response.clone().map(|inner| NetworkResponse { inner })
    }
}

impl From<fluxwright::NetEvent> for NetworkEvent {
    fn from(e: fluxwright::NetEvent) -> Self {
        let (event, request, response, failure) = match e {
            fluxwright::NetEvent::Request(r) => ("request", r, None, None),
            fluxwright::NetEvent::Response(r) => ("response", r.request.clone(), Some(r), None),
            fluxwright::NetEvent::Finished(r) => ("requestfinished", r, None, None),
            fluxwright::NetEvent::Failed(r, why) => ("requestfailed", r, None, Some(why)),
        };
        NetworkEvent {
            event: event.into(),
            request,
            response,
            failure,
        }
    }
}

/// A page's network events, in order.
#[pyclass(frozen, module = "fluxwright._native")]
struct NetworkStream(Arc<Mutex<UnboundedReceiver<fluxwright::NetEvent>>>);

#[pymethods]
impl NetworkStream {
    /// The next event, or `None` once the page closed.
    fn next<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let rx = self.0.clone();
        future_into_py(py, async move {
            Ok(rx.lock().await.recv().await.map(NetworkEvent::from))
        })
    }
}

/// A dialog waiting for an answer; `fluxwright.async_api` wraps it in `Dialog`. One dropped
/// unanswered is dismissed (`beforeunload` accepted).
#[pyclass(frozen, module = "fluxwright._native")]
struct Dialog {
    inner: Arc<Mutex<Option<fluxwright::Dialog>>>,
    /// `alert`, `confirm`, `prompt` or `beforeunload`.
    #[pyo3(get, name = "type")]
    kind: String,
    #[pyo3(get)]
    message: String,
    #[pyo3(get)]
    default_value: String,
}

impl Dialog {
    fn new(d: fluxwright::Dialog) -> Self {
        Dialog {
            kind: d.kind.clone(),
            message: d.message.clone(),
            default_value: d.default_value.clone(),
            inner: Arc::new(Mutex::new(Some(d))),
        }
    }

    /// Takes the dialog to answer it; a second answer fails, as in Playwright.
    async fn take(slot: &Mutex<Option<fluxwright::Dialog>>) -> PyResult<fluxwright::Dialog> {
        slot.lock()
            .await
            .take()
            .ok_or_else(|| py_errors::Error::new_err("dialog is already handled"))
    }
}

#[pymethods]
impl Dialog {
    /// `prompt_text` is what a `prompt` returns.
    #[pyo3(signature = (prompt_text=None))]
    fn accept<'py>(
        &self,
        py: Python<'py>,
        prompt_text: Option<String>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let slot = self.inner.clone();
        future_into_py(py, async move {
            Dialog::take(&slot)
                .await?
                .accept(prompt_text)
                .await
                .map(none)
                .map_err(error)
        })
    }

    fn dismiss<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let slot = self.inner.clone();
        future_into_py(py, async move {
            Dialog::take(&slot)
                .await?
                .dismiss()
                .await
                .map(none)
                .map_err(error)
        })
    }
}

/// A page's dialogs, in order.
#[pyclass(frozen, module = "fluxwright._native")]
struct DialogStream(Arc<Mutex<UnboundedReceiver<fluxwright::Dialog>>>);

#[pymethods]
impl DialogStream {
    /// The next dialog, or `None` once the page closed.
    fn next<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let rx = self.0.clone();
        future_into_py(py, async move {
            Ok(rx.lock().await.recv().await.map(Dialog::new))
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
        py.get_type::<py_errors::Error>()
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
        future_into_py(py, async move {
            // On a blocking thread: copying a large file held a Tokio worker for seconds.
            RT.spawn_blocking(move || download.save_as(path))
                .await
                .map_err(|e| PyRuntimeError::new_err(e.to_string()))??;
            Ok(None::<()>)
        })
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
    m.add_class::<NetworkRequest>()?;
    m.add_class::<NetworkResponse>()?;
    m.add_class::<NetworkEvent>()?;
    m.add_class::<NetworkStream>()?;
    m.add_class::<Dialog>()?;
    m.add_class::<DialogStream>()?;
    m.py().import("atexit")?.call_method1("register", (wrap_pyfunction!(wait_for_hand_overs, m)?,))?;
    Ok(())
}
