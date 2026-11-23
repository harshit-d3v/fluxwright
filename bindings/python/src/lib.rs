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

use pyo3::exceptions::{PyRuntimeError, PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyBool, PyBytes, PyCFunction, PyDict, PyString};
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

fn page_sel(selector: String, strict: Option<bool>) -> fluxwright::Selector {
    fluxwright::Selector::new(selector).strict(strict.unwrap_or(false))
}

enum TextOrRegex {
    Text(String),
    Regex(String, String),
}

fn parse_text_or_regex(obj: &Bound<'_, PyAny>, what: &str) -> PyResult<TextOrRegex> {
    if let Ok(s) = obj.extract::<String>() {
        return Ok(TextOrRegex::Text(s));
    }
    let pattern_ty = obj.py().import("re")?.getattr("Pattern")?;
    if obj.is_instance(&pattern_ty)? {
        let pattern: String = obj.getattr("pattern")?.extract()?;
        let flags_int: u32 = obj.getattr("flags")?.extract()?;
        let mut flags = String::new();
        if flags_int & 2 != 0 {
            flags.push('i');
        }
        if flags_int & 8 != 0 {
            flags.push('m');
        }
        if flags_int & 16 != 0 {
            flags.push('s');
        }
        return Ok(TextOrRegex::Regex(pattern, flags));
    }
    Err(PyTypeError::new_err(format!("{what} must be a str or re.Pattern")))
}

#[allow(clippy::too_many_arguments)]
fn parse_role_options(
    checked: Option<bool>,
    disabled: Option<bool>,
    exact: Option<bool>,
    expanded: Option<bool>,
    include_hidden: Option<bool>,
    level: Option<u32>,
    name: Option<&Bound<'_, PyAny>>,
    pressed: Option<bool>,
    selected: Option<bool>,
) -> PyResult<fluxwright::RoleOptions> {
    let (name_str, name_regex) = match name {
        None => (None, None),
        Some(v) => match parse_text_or_regex(v, "name")? {
            TextOrRegex::Text(s) => (Some(s), None),
            TextOrRegex::Regex(p, f) => (None, Some((p, f))),
        },
    };
    Ok(fluxwright::RoleOptions {
        name: name_str,
        exact: exact.unwrap_or(false),
        name_regex,
        checked,
        disabled,
        expanded,
        include_hidden: include_hidden.unwrap_or(false),
        level,
        pressed,
        selected,
    })
}

fn apply_filter(
    mut sel: fluxwright::Selector,
    has: Option<PyRef<'_, Locator>>,
    has_not: Option<PyRef<'_, Locator>>,
    has_text: Option<&Bound<'_, PyAny>>,
    has_not_text: Option<&Bound<'_, PyAny>>,
) -> PyResult<fluxwright::Selector> {
    if let Some(t) = has_text {
        let part = match parse_text_or_regex(t, "has_text")? {
            TextOrRegex::Text(s) => fluxwright::Selector::has_text(&s),
            TextOrRegex::Regex(p, f) => fluxwright::Selector::has_text_regex(&p, &f),
        };
        sel = sel.then(&part);
    }
    if let Some(t) = has_not_text {
        let part = match parse_text_or_regex(t, "has_not_text")? {
            TextOrRegex::Text(s) => fluxwright::Selector::has_not_text(&s),
            TextOrRegex::Regex(p, f) => fluxwright::Selector::has_not_text_regex(&p, &f),
        };
        sel = sel.then(&part);
    }
    if let Some(loc) = has {
        if sel.frames != loc.selector.frames {
            return Err(PyValueError::new_err("has locator belongs to a different frame"));
        }
        sel = sel.has(&loc.selector);
    }
    if let Some(loc) = has_not {
        if sel.frames != loc.selector.frames {
            return Err(PyValueError::new_err("has_not locator belongs to a different frame"));
        }
        sel = sel.has_not(&loc.selector);
    }
    Ok(sel)
}

/// Playwright's `state`: attached, detached, visible (the default) or hidden.
fn wait_state(state: Option<String>) -> PyResult<fluxwright::WaitState> {
    match state {
        Some(s) => s
            .parse::<fluxwright::WaitState>()
            .map_err(|e| PyValueError::new_err(e.to_string())),
        None => Ok(Default::default()),
    }
}

/// Playwright's `timeout` in milliseconds: 0 means no timeout, and anything that is not a time
/// (negative, NaN) leaves the default in place.
fn call_timeout(ms: Option<f64>) -> Option<Duration> {
    const YEAR: Duration = Duration::from_secs(365 * 24 * 60 * 60);
    match ms {
        Some(ms) if ms == 0.0 => Some(YEAR),
        Some(ms) if ms > 0.0 && ms.is_finite() => {
            Some(Duration::try_from_secs_f64(ms / 1000.0).unwrap_or(YEAR).min(YEAR))
        }
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

fn parse_api_timeout(timeout: Option<f64>) -> Option<Duration> {
    timeout.map(|ms| {
        if ms <= 0.0 {
            Duration::ZERO
        } else {
            Duration::from_millis(ms as u64)
        }
    })
}

fn build_api_options(
    method: Option<String>,
    headers: Option<HashMap<String, String>>,
    params: Option<Vec<(String, String)>>,
    data: Option<&Bound<'_, PyAny>>,
    fail_on_status_code: bool,
    max_redirects: Option<usize>,
    timeout: Option<f64>,
) -> PyResult<fluxwright::ApiRequestOptions> {
    Ok(fluxwright::ApiRequestOptions {
        method,
        headers: headers.unwrap_or_default().into_iter().collect(),
        params: params.unwrap_or_default(),
        data: data.map(bytes).transpose()?,
        fail_on_status_code,
        max_redirects,
        timeout: parse_api_timeout(timeout),
    })
}

#[pyclass(frozen, module = "fluxwright._native")]
struct ApiResponse {
    #[pyo3(get)]
    url: String,
    #[pyo3(get)]
    status: u16,
    #[pyo3(get)]
    status_text: String,
    #[pyo3(get)]
    headers: Vec<(String, String)>,
    body: std::sync::Mutex<Option<Vec<u8>>>,
}

impl ApiResponse {
    fn from_core(resp: fluxwright::ApiResponse) -> Self {
        Self {
            url: resp.url,
            status: resp.status,
            status_text: resp.status_text,
            headers: resp.headers,
            body: std::sync::Mutex::new(Some(resp.body)),
        }
    }
}

#[pymethods]
impl ApiResponse {
    #[getter]
    fn body<'py>(&self, py: Python<'py>) -> PyResult<Py<PyBytes>> {
        let g = self.body.lock().unwrap();
        let data = g
            .as_deref()
            .ok_or_else(|| py_errors::Error::new_err("Response has been disposed"))?;
        Ok(PyBytes::new(py, data).unbind())
    }

    fn dispose(&self) {
        *self.body.lock().unwrap() = None;
    }
}

/// `str` or `bytes`.
fn bytes(value: &Bound<'_, PyAny>) -> PyResult<Vec<u8>> {
    match value.cast::<PyBytes>() {
        Ok(b) => Ok(b.as_bytes().to_vec()),
        Err(_) => Ok(value.extract::<String>()?.into_bytes()),
    }
}

#[derive(Deserialize)]
struct Position {
    x: f64,
    y: f64,
}

fn opt_position(position: Option<&Bound<'_, PyAny>>) -> PyResult<Option<(f64, f64)>> {
    position
        .map(|p| options::<Position>(p, "position").map(|p| (p.x, p.y)))
        .transpose()
}

fn opt_delay(ms: Option<f64>) -> PyResult<Option<Duration>> {
    match ms {
        None => Ok(None),
        Some(ms) if ms == 0.0 => Ok(None),
        Some(ms) if ms > 0.0 && ms.is_finite() => Ok(Some(
            Duration::try_from_secs_f64(ms / 1000.0)
                .map_err(|_| PyValueError::new_err("delay out of range"))?,
        )),
        Some(ms) => Err(PyValueError::new_err(format!("delay must be >= 0, got {ms}"))),
    }
}

fn parse_button(button: Option<&str>) -> PyResult<fluxwright::MouseButton> {
    match button {
        None => Ok(fluxwright::MouseButton::Left),
        Some(b) => b
            .parse()
            .map_err(|e: fluxwright_cdp::Error| PyValueError::new_err(e.to_string())),
    }
}

fn to_modifiers(modifiers: Option<Vec<String>>) -> PyResult<u8> {
    match modifiers {
        None => Ok(0),
        Some(m) => fluxwright::parse_modifiers(&m).map_err(|e| PyValueError::new_err(e.to_string())),
    }
}

fn click_options(
    button: Option<&str>,
    click_count: u32,
    delay: Option<f64>,
    position: Option<&Bound<'_, PyAny>>,
    modifiers: Option<Vec<String>>,
    force: Option<bool>,
    trial: Option<bool>,
) -> PyResult<fluxwright::ClickOptions> {
    Ok(fluxwright::ClickOptions {
        button: parse_button(button)?,
        click_count,
        delay: opt_delay(delay)?,
        position: opt_position(position)?,
        modifiers: to_modifiers(modifiers)?,
        force: force.unwrap_or(false),
        trial: trial.unwrap_or(false),
    })
}

fn hover_options(
    position: Option<&Bound<'_, PyAny>>,
    modifiers: Option<Vec<String>>,
    force: Option<bool>,
    trial: Option<bool>,
) -> PyResult<fluxwright::HoverOptions> {
    Ok(fluxwright::HoverOptions {
        position: opt_position(position)?,
        modifiers: to_modifiers(modifiers)?,
        force: force.unwrap_or(false),
        trial: trial.unwrap_or(false),
    })
}

fn check_options(
    position: Option<&Bound<'_, PyAny>>,
    force: Option<bool>,
    trial: Option<bool>,
) -> PyResult<fluxwright::CheckOptions> {
    Ok(fluxwright::CheckOptions {
        position: opt_position(position)?,
        force: force.unwrap_or(false),
        trial: trial.unwrap_or(false),
    })
}

fn extract_index(v: &Bound<'_, PyAny>) -> PyResult<usize> {
    if v.is_instance_of::<PyBool>() {
        return Err(PyValueError::new_err("select_option index must be an int"));
    }
    v.extract::<usize>()
}

fn parse_select_dict(d: &Bound<'_, PyDict>) -> PyResult<fluxwright::SelectEntry> {
    if d.get_item("element")?.is_some_and(|v| !v.is_none()) {
        return Err(PyValueError::new_err(
            "select_option: element is not supported; pass value, label or index",
        ));
    }
    if let Some(v) = d.get_item("value")?.filter(|v| !v.is_none()) {
        return Ok(fluxwright::SelectEntry::Value(v.extract()?));
    }
    if let Some(l) = d.get_item("label")?.filter(|v| !v.is_none()) {
        return Ok(fluxwright::SelectEntry::Label(l.extract()?));
    }
    if let Some(i) = d.get_item("index")?.filter(|v| !v.is_none()) {
        return Ok(fluxwright::SelectEntry::Index(extract_index(&i)?));
    }
    Err(PyValueError::new_err(
        "select_option item must specify value, label or index",
    ))
}

fn parse_select_entries(
    value: Option<&Bound<'_, PyAny>>,
    index: Option<&Bound<'_, PyAny>>,
    label: Option<&Bound<'_, PyAny>>,
    element: Option<&Bound<'_, PyAny>>,
) -> PyResult<Vec<fluxwright::SelectEntry>> {
    if element.is_some_and(|e| !e.is_none()) {
        return Err(PyValueError::new_err(
            "select_option: element is not supported; pass value, label or index",
        ));
    }
    let mut out = Vec::new();
    if let Some(v) = value.filter(|v| !v.is_none()) {
        if let Ok(s) = v.cast::<PyString>() {
            out.push(fluxwright::SelectEntry::Value(s.extract()?));
        } else if let Ok(d) = v.cast::<PyDict>() {
            out.push(parse_select_dict(d)?);
        } else {
            for item in v.try_iter()? {
                let item = item?;
                if let Ok(d) = item.cast::<PyDict>() {
                    out.push(parse_select_dict(d)?);
                } else {
                    out.push(fluxwright::SelectEntry::Value(item.extract()?));
                }
            }
        }
    }
    if let Some(l) = label.filter(|l| !l.is_none()) {
        if let Ok(s) = l.cast::<PyString>() {
            out.push(fluxwright::SelectEntry::Label(s.extract()?));
        } else {
            for item in l.try_iter()? {
                out.push(fluxwright::SelectEntry::Label(item?.extract()?));
            }
        }
    }
    if let Some(idx) = index.filter(|i| !i.is_none()) {
        if !idx.is_instance_of::<PyBool>() {
            if let Ok(i) = idx.extract::<usize>() {
                out.push(fluxwright::SelectEntry::Index(i));
                return Ok(out);
            }
        }
        if idx.is_instance_of::<PyBool>() || idx.extract::<isize>().is_ok() {
            return Err(PyValueError::new_err(
                "select_option index must be a non-negative int",
            ));
        }
        for item in idx.try_iter()? {
            out.push(fluxwright::SelectEntry::Index(extract_index(&item?)?));
        }
    }
    Ok(out)
}

fn parse_file_payload(d: &Bound<'_, PyDict>) -> PyResult<fluxwright::FilePayload> {
    let name: String = d
        .get_item("name")?
        .ok_or_else(|| PyValueError::new_err("FilePayload missing 'name'"))?
        .extract()?;
    let mime_type: String = match d.get_item("mimeType")? {
        Some(m) if !m.is_none() => m.extract()?,
        _ => match d.get_item("mime_type")? {
            Some(m) if !m.is_none() => m.extract()?,
            _ => String::new(),
        },
    };
    let buf_any = d
        .get_item("buffer")?
        .ok_or_else(|| PyValueError::new_err("FilePayload missing 'buffer'"))?;
    let buffer = bytes(&buf_any)?;
    Ok(fluxwright::FilePayload {
        name,
        mime_type,
        buffer,
    })
}

fn is_path_like(v: &Bound<'_, PyAny>) -> PyResult<bool> {
    Ok(v.is_instance_of::<PyString>() || v.hasattr("__fspath__")?)
}

fn parse_file_input(files: &Bound<'_, PyAny>) -> PyResult<fluxwright::FileInput> {
    if is_path_like(files)? {
        return Ok(fluxwright::FileInput::Paths(vec![files.extract::<PathBuf>()?]));
    }
    if let Ok(d) = files.cast::<PyDict>() {
        return Ok(fluxwright::FileInput::Payloads(vec![parse_file_payload(d)?]));
    }
    let mut paths = Vec::new();
    let mut payloads = Vec::new();
    for item in files.try_iter()? {
        let item = item?;
        if let Ok(d) = item.cast::<PyDict>() {
            if !paths.is_empty() {
                return Err(PyValueError::new_err(
                    "set_input_files cannot mix file paths and FilePayload dicts",
                ));
            }
            payloads.push(parse_file_payload(d)?);
        } else {
            if !payloads.is_empty() {
                return Err(PyValueError::new_err(
                    "set_input_files cannot mix file paths and FilePayload dicts",
                ));
            }
            paths.push(item.extract::<PathBuf>()?);
        }
    }
    if !payloads.is_empty() {
        Ok(fluxwright::FileInput::Payloads(payloads))
    } else {
        Ok(fluxwright::FileInput::Paths(paths))
    }
}

#[allow(clippy::too_many_arguments)]
fn do_click<'py>(
    py: Python<'py>,
    lease: &LeaseSlot,
    sel: fluxwright::Selector,
    button: Option<&str>,
    click_count: u32,
    delay: Option<f64>,
    position: Option<&Bound<'_, PyAny>>,
    modifiers: Option<Vec<String>>,
    force: Option<bool>,
    trial: Option<bool>,
    dbl: bool,
    timeout: Option<f64>,
) -> PyResult<Bound<'py, PyAny>> {
    let opts = click_options(button, click_count, delay, position, modifiers, force, trial)?;
    run_in(py, lease, timeout, move |p| {
        Box::pin(async move {
            if dbl {
                p.dblclick(&sel, &opts).await.map(none)
            } else {
                p.click_with(&sel, &opts).await.map(none)
            }
        })
    })
}

#[allow(clippy::too_many_arguments)]
fn do_hover_or_tap<'py>(
    py: Python<'py>,
    lease: &LeaseSlot,
    sel: fluxwright::Selector,
    position: Option<&Bound<'_, PyAny>>,
    modifiers: Option<Vec<String>>,
    force: Option<bool>,
    trial: Option<bool>,
    tap: bool,
    timeout: Option<f64>,
) -> PyResult<Bound<'py, PyAny>> {
    let opts = hover_options(position, modifiers, force, trial)?;
    run_in(py, lease, timeout, move |p| {
        Box::pin(async move {
            if tap {
                p.tap(&sel, &opts).await.map(none)
            } else {
                p.hover(&sel, &opts).await.map(none)
            }
        })
    })
}

#[allow(clippy::too_many_arguments)]
fn do_check<'py>(
    py: Python<'py>,
    lease: &LeaseSlot,
    sel: fluxwright::Selector,
    checked: bool,
    position: Option<&Bound<'_, PyAny>>,
    force: Option<bool>,
    trial: Option<bool>,
    timeout: Option<f64>,
) -> PyResult<Bound<'py, PyAny>> {
    let opts = check_options(position, force, trial)?;
    run_in(py, lease, timeout, move |p| {
        Box::pin(async move { p.set_checked(&sel, checked, &opts).await.map(none) })
    })
}

fn do_drag<'py>(
    py: Python<'py>,
    lease: &LeaseSlot,
    src: fluxwright::Selector,
    dst: fluxwright::Selector,
    force: Option<bool>,
    timeout: Option<f64>,
) -> PyResult<Bound<'py, PyAny>> {
    let force = force.unwrap_or(false);
    run_in(py, lease, timeout, move |p| {
        Box::pin(async move { p.drag_and_drop(src, dst, force).await.map(none) })
    })
}

fn do_press<'py>(
    py: Python<'py>,
    lease: &LeaseSlot,
    sel: fluxwright::Selector,
    key: String,
    delay: Option<f64>,
    timeout: Option<f64>,
) -> PyResult<Bound<'py, PyAny>> {
    let delay = opt_delay(delay)?;
    run_in(py, lease, timeout, move |p| {
        Box::pin(async move { p.press(&sel, &key, delay).await.map(none) })
    })
}

fn do_type<'py>(
    py: Python<'py>,
    lease: &LeaseSlot,
    sel: fluxwright::Selector,
    text: String,
    delay: Option<f64>,
    timeout: Option<f64>,
) -> PyResult<Bound<'py, PyAny>> {
    let delay = opt_delay(delay)?;
    run_in(py, lease, timeout, move |p| {
        Box::pin(async move { p.press_sequentially(&sel, &text, delay).await.map(none) })
    })
}

#[allow(clippy::too_many_arguments)]
fn do_select_option<'py>(
    py: Python<'py>,
    lease: &LeaseSlot,
    sel: fluxwright::Selector,
    value: Option<&Bound<'_, PyAny>>,
    index: Option<&Bound<'_, PyAny>>,
    label: Option<&Bound<'_, PyAny>>,
    element: Option<&Bound<'_, PyAny>>,
    force: Option<bool>,
    timeout: Option<f64>,
) -> PyResult<Bound<'py, PyAny>> {
    let entries = parse_select_entries(value, index, label, element)?;
    let force = force.unwrap_or(false);
    run_in(py, lease, timeout, move |p| {
        Box::pin(async move { p.select_option(&sel, &entries, force).await })
    })
}

fn do_set_input_files<'py>(
    py: Python<'py>,
    lease: &LeaseSlot,
    sel: fluxwright::Selector,
    files: &Bound<'_, PyAny>,
    timeout: Option<f64>,
) -> PyResult<Bound<'py, PyAny>> {
    let input = parse_file_input(files)?;
    run_in(py, lease, timeout, move |p| {
        Box::pin(async move { p.set_input_files(&sel, &input).await.map(none) })
    })
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
#[serde(deny_unknown_fields)]
struct Credentials {
    username: String,
    password: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CookieInput {
    name: String,
    value: String,
    url: Option<String>,
    domain: Option<String>,
    path: Option<String>,
    expires: Option<f64>,
    #[serde(alias = "http_only")]
    http_only: Option<bool>,
    secure: Option<bool>,
    #[serde(alias = "same_site")]
    same_site: Option<String>,
    #[serde(alias = "partition_key")]
    partition_key: Option<String>,
    #[serde(rename = "_crHasCrossSiteAncestor")]
    cross_site_ancestor: Option<bool>,
}

fn parse_cookies(cookies: Vec<Bound<'_, PyAny>>) -> PyResult<Vec<fluxwright::Cookie>> {
    let mut parsed = Vec::with_capacity(cookies.len());
    for c in cookies {
        let item: CookieInput = options(&c, "cookies")?;
        if item.domain.is_some() == item.url.is_some() {
            return Err(PyValueError::new_err(
                "add_cookies: each cookie needs url or domain, not both",
            ));
        }
        let domain = item.domain.or(item.url).expect("checked above");
        parsed.push(fluxwright::Cookie {
            name: item.name,
            value: item.value,
            domain,
            path: item.path.unwrap_or_else(|| "/".into()),
            expires: item.expires.unwrap_or(-1.0),
            http_only: item.http_only.unwrap_or(false),
            secure: item.secure.unwrap_or(false),
            same_site: item.same_site.unwrap_or_else(|| "Lax".into()),
            partition_key: item.partition_key,
            cross_site_ancestor: item.cross_site_ancestor,
        });
    }
    Ok(parsed)
}

fn eval_script(expression: &str, arg: Option<Bound<'_, PyAny>>) -> PyResult<String> {
    let arg = arg.map(|a| to_json(&a)).transpose()?;
    Ok(page_function(expression, arg.as_ref()))
}

async fn save_storage_json(json: serde_json::Value, path: Option<PathBuf>) -> PyResult<Json> {
    if let Some(path) = path {
        let bytes = serde_json::to_vec_pretty(&json)
            .map_err(|e| PyValueError::new_err(e.to_string()))?;
        RT.spawn_blocking(move || save_private(&path, &bytes))
            .await
            .map_err(|e| PyRuntimeError::new_err(e.to_string()))??;
    }
    Ok(Json(json))
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

fn extract_mask_selectors(
    mask: Option<&Bound<'_, PyAny>>,
    expected_lease: &LeaseSlot,
) -> PyResult<Vec<String>> {
    let Some(val) = mask else {
        return Ok(Vec::new());
    };
    if val.is_none() {
        return Ok(Vec::new());
    }
    if val.is_instance_of::<PyString>() || val.is_instance_of::<PyBytes>() {
        return Err(PyTypeError::new_err(
            "screenshot: mask must be a sequence of Locator or str",
        ));
    }
    let iter = val
        .try_iter()
        .map_err(|_| PyTypeError::new_err("screenshot: mask must be a sequence of Locator or str"))?;
    let mut out = Vec::new();
    for item in iter {
        let item = item?;
        if let Ok(s) = item.extract::<String>() {
            if s.trim().is_empty() {
                return Err(PyValueError::new_err(
                    "screenshot: mask selector must not be empty",
                ));
            }
            out.push(s);
            continue;
        }
        let mut cur = item.clone();
        let mut found: Option<(LeaseSlot, fluxwright::Selector)> = None;
        for _ in 0..3 {
            if let Ok(loc) = cur.extract::<PyRef<'_, Locator>>() {
                found = Some((loc.lease.clone(), loc.selector.clone()));
                break;
            }
            match cur.getattr("_impl") {
                Ok(next) => cur = next,
                Err(_) => break,
            }
        }
        let Some((loc_lease, sel)) = found else {
            return Err(PyTypeError::new_err(
                "screenshot: mask entries must be Locator or str",
            ));
        };
        if !Arc::ptr_eq(&loc_lease, expected_lease) {
            return Err(PyValueError::new_err(
                "screenshot: mask locator must belong to the same page",
            ));
        }
        if !sel.frames.is_empty() {
            return Err(PyValueError::new_err(
                "screenshot: mask locators inside frame_locator are not supported",
            ));
        }
        out.push(sel.query);
    }
    Ok(out)
}

/// The screenshot type as Playwright picks it: `type`, else the extension of `path`, else PNG.
#[allow(clippy::too_many_arguments)]
fn shot_options(
    lease: &LeaseSlot,
    path: Option<&Path>,
    kind: Option<&str>,
    quality: Option<u32>,
    omit_background: Option<bool>,
    annotate: Option<bool>,
    mask: Option<&Bound<'_, PyAny>>,
    mask_color: Option<String>,
    animations: Option<&str>,
    caret: Option<&str>,
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
    let animations = match animations {
        None => None,
        Some("allow") => Some(fluxwright::ScreenshotAnimations::Allow),
        Some("disabled") => Some(fluxwright::ScreenshotAnimations::Disabled),
        Some(other) => {
            return Err(PyValueError::new_err(format!(
                "screenshot: animations must be 'allow' or 'disabled', not {other:?}"
            )));
        }
    };
    let caret = match caret {
        None => None,
        Some("hide") => Some(fluxwright::ScreenshotCaret::Hide),
        Some("initial") => Some(fluxwright::ScreenshotCaret::Initial),
        Some(other) => {
            return Err(PyValueError::new_err(format!(
                "screenshot: caret must be 'hide' or 'initial', not {other:?}"
            )));
        }
    };
    let mask = extract_mask_selectors(mask, lease)?;
    let opts = fluxwright::ScreenshotOptions {
        format,
        // Over 255 becomes 255, which the engine rejects as over 100.
        quality: quality.map(|q| q.min(255) as u8),
        omit_background: omit_background.unwrap_or(false),
        annotate: annotate.unwrap_or(false),
        mask,
        mask_color,
        animations,
        caret,
        ..Default::default()
    };
    opts.validate()
        .map_err(|e| PyValueError::new_err(e.to_string()))?;
    Ok(opts)
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

fn build_record_har_options(
    path: PathBuf,
    omit_content: Option<bool>,
    content: Option<String>,
    mode: Option<String>,
    url_filter: Option<String>,
) -> PyResult<fluxwright::RecordHarOptions> {
    let content = match content.as_deref() {
        None => None,
        Some("embed") => Some(fluxwright::HarContentPolicy::Embed),
        Some("omit") => Some(fluxwright::HarContentPolicy::Omit),
        Some(other) => {
            return Err(PyValueError::new_err(format!(
                "record_har_content must be 'embed' or 'omit', not {other:?}"
            )))
        }
    };
    let mode = match mode.as_deref() {
        None | Some("full") => fluxwright::HarMode::Full,
        Some("minimal") => fluxwright::HarMode::Minimal,
        Some(other) => {
            return Err(PyValueError::new_err(format!(
                "record_har_mode must be 'full' or 'minimal', not {other:?}"
            )))
        }
    };
    Ok(fluxwright::RecordHarOptions {
        path,
        omit_content: omit_content.unwrap_or(false),
        content,
        mode,
        url_filter,
    })
}

enum PyIgnoreDefaultArgs {
    All(bool),
    Some(Vec<String>),
}

fn parse_ignore_default_args(val: Bound<'_, PyAny>) -> PyResult<PyIgnoreDefaultArgs> {
    if val.is_instance_of::<PyBool>() {
        Ok(PyIgnoreDefaultArgs::All(val.extract::<bool>()?))
    } else if val.is_instance_of::<PyString>() {
        Err(PyTypeError::new_err(
            "ignore_default_args must be a bool or list[str], not str",
        ))
    } else if let Ok(list) = val.extract::<Vec<String>>() {
        Ok(PyIgnoreDefaultArgs::Some(list))
    } else {
        Err(PyTypeError::new_err(
            "ignore_default_args must be a bool or list[str]",
        ))
    }
}

#[allow(clippy::too_many_arguments)]
fn parse_job_options<'py>(
    proxy: Option<Bound<'py, PyAny>>,
    user_agent: Option<String>,
    locale: Option<String>,
    timezone_id: Option<String>,
    geolocation: Option<Bound<'py, PyAny>>,
    permissions: Option<Vec<String>>,
    viewport: Option<Bound<'py, PyAny>>,
    device_scale_factor: Option<f64>,
    color_scheme: Option<String>,
    is_mobile: Option<bool>,
    has_touch: Option<bool>,
    extra_http_headers: Option<HashMap<String, String>>,
    ignore_https_errors: Option<bool>,
    java_script_enabled: Option<bool>,
    offline: Option<bool>,
    base_url: Option<String>,
    http_credentials: Option<Bound<'py, PyAny>>,
    bypass_csp: Option<bool>,
    storage_state: Option<Bound<'py, PyAny>>,
    record_har_path: Option<PathBuf>,
    record_har_omit_content: Option<bool>,
    record_har_content: Option<String>,
    record_har_mode: Option<String>,
    record_har_url_filter: Option<String>,
    resource_preset: Option<String>,
    block_resources: Option<Vec<String>>,
    block_url_patterns: Option<Vec<String>>,
) -> PyResult<fluxwright::JobOptions> {
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
    let http_credentials: Option<Credentials> = http_credentials
        .map(|c| options(&c, "http_credentials"))
        .transpose()?;
    let record_har = match record_har_path {
        Some(path) => Some(build_record_har_options(
            path,
            record_har_omit_content,
            record_har_content,
            record_har_mode,
            record_har_url_filter,
        )?),
        None => None,
    };
    let mut opts = fluxwright::JobOptions {
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
            is_mobile: is_mobile.unwrap_or(false),
            has_touch: has_touch.unwrap_or(false),
            extra_http_headers: extra_http_headers.unwrap_or_default().into_iter().collect(),
            ignore_https_errors: ignore_https_errors.unwrap_or(false),
            javascript_enabled: java_script_enabled,
            offline: offline.unwrap_or(false),
            bypass_csp: bypass_csp.unwrap_or(false),
        },
        permissions: permissions.unwrap_or_default(),
        storage_state: storage_state.map(|s| storage(&s)).transpose()?,
        base_url,
        http_credentials: http_credentials.map(|c| (c.username, c.password)),
        record_har,
        ..Default::default()
    };
    if let Some(ref preset) = resource_preset {
        let p = fluxwright::ResourcePreset::parse(preset).ok_or_else(|| {
            PyValueError::new_err(format!(
                "resource_preset must be 'scraping', 'minimal', or 'no-media', not {preset:?}"
            ))
        })?;
        opts = opts.resource_preset(p);
    }
    if let Some(resources) = block_resources {
        let mut parsed = Vec::with_capacity(resources.len());
        for r in resources {
            parsed.push(fluxwright::ResourceType::parse(&r).ok_or_else(|| {
                PyValueError::new_err(format!(
                    "block_resources entry must be image, font, media, stylesheet or script, not {r:?}"
                ))
            })?);
        }
        opts = opts.block_resources(parsed);
    }
    if let Some(patterns) = block_url_patterns {
        opts = opts.block_url_patterns(patterns);
    }
    Ok(opts)
}

/// The engine: a pool of Chrome processes that pages are leased from.
#[pyclass(frozen, module = "fluxwright._native")]
struct Engine {
    inner: fluxwright::BrowserEngine,
}

#[pymethods]
impl Engine {
    #[staticmethod]
    #[pyo3(signature = (*, max_browsers=None, executable_path=None, headless=None, queue_timeout=None,
        memory_ceiling_mb=None, max_concurrency_per_host=None, min_delay_per_host=None,
        args=None, ignore_default_args=None, channel=None, prewarm_contexts=None))]
    #[allow(clippy::too_many_arguments)]
    fn launch<'py>(
        py: Python<'py>,
        max_browsers: Option<usize>,
        executable_path: Option<PathBuf>,
        headless: Option<bool>,
        queue_timeout: Option<f64>,
        memory_ceiling_mb: Option<u64>,
        max_concurrency_per_host: Option<usize>,
        min_delay_per_host: Option<f64>,
        args: Option<Vec<String>>,
        ignore_default_args: Option<Bound<'py, PyAny>>,
        channel: Option<String>,
        prewarm_contexts: Option<usize>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let parsed_ignore = ignore_default_args
            .map(parse_ignore_default_args)
            .transpose()?;
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
            if let Some(n) = max_concurrency_per_host {
                b = b.max_concurrency_per_host(n);
            }
            // Milliseconds, as Playwright's timeouts; a negative or NaN delay is no delay.
            if let Some(ms) = min_delay_per_host.filter(|ms| *ms > 0.0 && ms.is_finite()) {
                b = b.min_delay_per_host(Duration::from_secs_f64(ms / 1000.0));
            }
            if let Some(exe) = executable_path {
                b = b.chrome(exe);
            }
            if let Some(h) = headless {
                b = b.headless(h);
            }
            if let Some(a) = args {
                b = b.args(a);
            }
            if let Some(ig) = parsed_ignore {
                match ig {
                    PyIgnoreDefaultArgs::All(all) => b = b.ignore_default_args(all),
                    PyIgnoreDefaultArgs::Some(list) => b = b.ignore_default_args_list(list),
                }
            }
            if let Some(ch) = channel {
                b = b.channel(ch);
            }
            if let Some(n) = prewarm_contexts {
                b = b.prewarm_contexts(n);
            }
            Ok(Engine {
                inner: b.build().await.map_err(error)?,
            })
        })
    }

    #[staticmethod]
    fn connect_over_cdp(py: Python<'_>, endpoint_url: String) -> PyResult<Bound<'_, PyAny>> {
        future_into_py(py, async move {
            let inner = fluxwright::BrowserEngine::builder()
                .connect_over_cdp(endpoint_url)
                .max_contexts_per_browser(8)
                .queue_capacity(usize::MAX)
                .acquire_timeout(queue_wait(None))
                .build()
                .await
                .map_err(error)?;
            Ok(Engine { inner })
        })
    }

    /// A fresh browser context; every option applies to this page only.
    #[allow(clippy::too_many_arguments)]
    #[pyo3(signature = (*, proxy=None, user_agent=None, locale=None, timezone_id=None, geolocation=None,
                        permissions=None, viewport=None, device_scale_factor=None, color_scheme=None,
                        is_mobile=None, has_touch=None, extra_http_headers=None, ignore_https_errors=None,
                        java_script_enabled=None, offline=None, base_url=None, http_credentials=None,
                        bypass_csp=None, storage_state=None,
                        record_har_path=None, record_har_omit_content=None, record_har_content=None,
                        record_har_mode=None, record_har_url_filter=None,
                        resource_preset=None, block_resources=None, block_url_patterns=None))]
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
        is_mobile: Option<bool>,
        has_touch: Option<bool>,
        extra_http_headers: Option<HashMap<String, String>>,
        ignore_https_errors: Option<bool>,
        java_script_enabled: Option<bool>,
        offline: Option<bool>,
        base_url: Option<String>,
        http_credentials: Option<Bound<'py, PyAny>>,
        bypass_csp: Option<bool>,
        storage_state: Option<Bound<'py, PyAny>>,
        record_har_path: Option<PathBuf>,
        record_har_omit_content: Option<bool>,
        record_har_content: Option<String>,
        record_har_mode: Option<String>,
        record_har_url_filter: Option<String>,
        resource_preset: Option<String>,
        block_resources: Option<Vec<String>>,
        block_url_patterns: Option<Vec<String>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let opts = parse_job_options(
            proxy,
            user_agent,
            locale,
            timezone_id,
            geolocation,
            permissions,
            viewport,
            device_scale_factor,
            color_scheme,
            is_mobile,
            has_touch,
            extra_http_headers,
            ignore_https_errors,
            java_script_enabled,
            offline,
            base_url,
            http_credentials,
            bypass_csp,
            storage_state,
            record_har_path,
            record_har_omit_content,
            record_har_content,
            record_har_mode,
            record_har_url_filter,
            resource_preset,
            block_resources,
            block_url_patterns,
        )?;
        let engine = self.inner.clone();
        future_into_py(py, async move {
            let lease = engine.acquire_job(&opts).await.map_err(error)?;
            Ok(Lease(std::sync::Mutex::new(Some(lease))))
        })
    }

    #[allow(clippy::too_many_arguments)]
    #[pyo3(signature = (*, proxy=None, user_agent=None, locale=None, timezone_id=None, geolocation=None,
                        permissions=None, viewport=None, device_scale_factor=None, color_scheme=None,
                        is_mobile=None, has_touch=None, extra_http_headers=None, ignore_https_errors=None,
                        java_script_enabled=None, offline=None, base_url=None, http_credentials=None,
                        bypass_csp=None, storage_state=None,
                        record_har_path=None, record_har_omit_content=None, record_har_content=None,
                        record_har_mode=None, record_har_url_filter=None,
                        resource_preset=None, block_resources=None, block_url_patterns=None))]
    fn new_context<'py>(
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
        is_mobile: Option<bool>,
        has_touch: Option<bool>,
        extra_http_headers: Option<HashMap<String, String>>,
        ignore_https_errors: Option<bool>,
        java_script_enabled: Option<bool>,
        offline: Option<bool>,
        base_url: Option<String>,
        http_credentials: Option<Bound<'py, PyAny>>,
        bypass_csp: Option<bool>,
        storage_state: Option<Bound<'py, PyAny>>,
        record_har_path: Option<PathBuf>,
        record_har_omit_content: Option<bool>,
        record_har_content: Option<String>,
        record_har_mode: Option<String>,
        record_har_url_filter: Option<String>,
        resource_preset: Option<String>,
        block_resources: Option<Vec<String>>,
        block_url_patterns: Option<Vec<String>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let opts = parse_job_options(
            proxy,
            user_agent,
            locale,
            timezone_id,
            geolocation,
            permissions,
            viewport,
            device_scale_factor,
            color_scheme,
            is_mobile,
            has_touch,
            extra_http_headers,
            ignore_https_errors,
            java_script_enabled,
            offline,
            base_url,
            http_credentials,
            bypass_csp,
            storage_state,
            record_har_path,
            record_har_omit_content,
            record_har_content,
            record_har_mode,
            record_har_url_filter,
            resource_preset,
            block_resources,
            block_url_patterns,
        )?;
        let engine = self.inner.clone();
        future_into_py(py, async move {
            let ctx = engine.acquire_context_with(&opts).await.map_err(error)?;
            Ok(Context {
                inner: Arc::new(ctx),
            })
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

#[pyclass(frozen, module = "fluxwright._native")]
struct Context {
    inner: Arc<fluxwright::ContextLease>,
}

#[pymethods]
impl Context {
    fn new_page<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let ctx = self.inner.clone();
        future_into_py(py, async move {
            let lease = ctx.new_page().await.map_err(error)?;
            Ok(Lease(std::sync::Mutex::new(Some(lease))))
        })
    }

    #[pyo3(signature = (urls=None))]
    fn cookies<'py>(
        &self,
        py: Python<'py>,
        urls: Option<Vec<String>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let ctx = self.inner.clone();
        future_into_py(py, async move {
            let cookies = match urls {
                Some(u) if !u.is_empty() => ctx.cookies_for_urls(&u).await.map_err(error)?,
                _ => ctx.cookies().await.map_err(error)?,
            };
            let json =
                serde_json::to_value(&cookies).map_err(|e| PyValueError::new_err(e.to_string()))?;
            Ok(Json(json))
        })
    }

    fn add_cookies<'py>(
        &self,
        py: Python<'py>,
        cookies: Vec<Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let parsed = parse_cookies(cookies)?;
        let ctx = self.inner.clone();
        future_into_py(py, async move {
            ctx.add_cookies(&parsed).await.map(none).map_err(error)
        })
    }

    fn clear_cookies<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let ctx = self.inner.clone();
        future_into_py(py, async move {
            ctx.clear_cookies().await.map(none).map_err(error)
        })
    }

    fn replace_cookies<'py>(
        &self,
        py: Python<'py>,
        cookies: Vec<Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let parsed = parse_cookies(cookies)?;
        let ctx = self.inner.clone();
        future_into_py(py, async move {
            ctx.replace_cookies(&parsed).await.map(none).map_err(error)
        })
    }

    #[allow(clippy::too_many_arguments)]
    #[pyo3(signature = (url, *, method=None, headers=None, params=None, data=None, fail_on_status_code=false, max_redirects=None, timeout=None))]
    fn api_fetch<'py>(
        &self,
        py: Python<'py>,
        url: String,
        method: Option<String>,
        headers: Option<HashMap<String, String>>,
        params: Option<Vec<(String, String)>>,
        data: Option<Bound<'py, PyAny>>,
        fail_on_status_code: bool,
        max_redirects: Option<usize>,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let opts = build_api_options(
            method,
            headers,
            params,
            data.as_ref(),
            fail_on_status_code,
            max_redirects,
            timeout,
        )?;
        let ctx = self.inner.clone();
        future_into_py(py, async move {
            let resp = ctx.request().fetch(&url, &opts).await.map_err(error)?;
            Ok(ApiResponse::from_core(resp))
        })
    }

    fn add_init_script<'py>(&self, py: Python<'py>, source: String) -> PyResult<Bound<'py, PyAny>> {
        let ctx = self.inner.clone();
        future_into_py(py, async move {
            ctx.add_init_script(&source).await.map(none).map_err(error)
        })
    }

    fn expose_binding<'py>(&self, py: Python<'py>, name: String) -> PyResult<Bound<'py, PyAny>> {
        let ctx = self.inner.clone();
        future_into_py(py, async move {
            ctx.expose_binding(&name).await.map(none).map_err(error)
        })
    }

    fn binding_calls(&self) -> BindingStream {
        BindingStream(Arc::new(Mutex::new(self.inner.subscribe_bindings())))
    }

    fn intercept<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let ctx = self.inner.clone();
        future_into_py(py, async move {
            let rx = ctx.intercept().await.map_err(error)?;
            Ok(RouteStream(Arc::new(Mutex::new(rx))))
        })
    }

    #[pyo3(signature = (*, path=None, indexed_db=false))]
    fn storage_state<'py>(
        &self,
        py: Python<'py>,
        path: Option<PathBuf>,
        indexed_db: bool,
    ) -> PyResult<Bound<'py, PyAny>> {
        let _ = indexed_db;
        let ctx = self.inner.clone();
        future_into_py(py, async move {
            let cookies = ctx.cookies().await.map_err(error)?;
            let json = serde_json::json!({ "cookies": cookies, "origins": [] });
            save_storage_json(json, path).await
        })
    }

    #[pyo3(name = "_start_har_recording", signature = (path, *, omit_content=None, content=None, mode=None, url_filter=None))]
    fn start_har_recording<'py>(
        &self,
        py: Python<'py>,
        path: PathBuf,
        omit_content: Option<bool>,
        content: Option<String>,
        mode: Option<String>,
        url_filter: Option<String>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let opts = build_record_har_options(path, omit_content, content, mode, url_filter)?;
        let ctx = self.inner.clone();
        future_into_py(py, async move {
            ctx.start_har_recording(opts).await.map(none).map_err(error)
        })
    }

    fn is_closed(&self) -> bool {
        self.inner.is_closed()
    }

    fn close<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let ctx = self.inner.clone();
        future_into_py(py, async move { ctx.close().await.map(none).map_err(error) })
    }
}

#[pyclass(frozen, module = "fluxwright._native")]
struct FrameInfo {
    #[pyo3(get)]
    id: String,
    #[pyo3(get)]
    parent_id: Option<String>,
    #[pyo3(get)]
    name: String,
    #[pyo3(get)]
    url: String,
}

impl From<fluxwright::FrameInfo> for FrameInfo {
    fn from(f: fluxwright::FrameInfo) -> Self {
        FrameInfo {
            id: f.id,
            parent_id: f.parent_id,
            name: f.name,
            url: f.url,
        }
    }
}

/// A leased page on its way to becoming a `Page`.
#[pyclass(frozen, module = "fluxwright._native")]
struct Lease(std::sync::Mutex<Option<fluxwright::PageLease>>);

/// A browser tab in a fresh context. Subclassed by `fluxwright.async_api.Page`.
#[pyclass(frozen, subclass, module = "fluxwright._native")]
struct Page {
    lease: LeaseSlot,
    url: tokio::sync::watch::Receiver<String>,
    target_id: String,
    opener_target_id: Option<String>,
}

fn parse_wait_until(wait_until: Option<&str>) -> PyResult<fluxwright::WaitUntil> {
    match wait_until {
        None | Some("load") => Ok(fluxwright::WaitUntil::Load),
        Some("domcontentloaded") => Ok(fluxwright::WaitUntil::DomContentLoaded),
        Some("networkidle") => Ok(fluxwright::WaitUntil::NetworkIdle),
        Some("commit") => Ok(fluxwright::WaitUntil::Commit),
        Some(other) => Err(PyValueError::new_err(format!(
            "wait_until must be load, domcontentloaded, networkidle or commit, not {other:?}"
        ))),
    }
}

fn parse_load_state(state: Option<&str>) -> PyResult<fluxwright::WaitUntil> {
    match state {
        None | Some("load") => Ok(fluxwright::WaitUntil::Load),
        Some("domcontentloaded") => Ok(fluxwright::WaitUntil::DomContentLoaded),
        Some("networkidle") => Ok(fluxwright::WaitUntil::NetworkIdle),
        Some(other) => Err(PyValueError::new_err(format!(
            "state must be load, domcontentloaded or networkidle, not {other:?}"
        ))),
    }
}

fn to_clock_time(time_ms: Option<f64>, time_iso: Option<String>) -> PyResult<fluxwright::ClockTime> {
    match (time_ms, time_iso) {
        (Some(ms), None) => Ok(fluxwright::ClockTime::Ms(ms)),
        (None, Some(iso)) => Ok(fluxwright::ClockTime::Iso(iso)),
        _ => Err(PyValueError::new_err(
            "clock time must specify either time_ms or time_iso",
        )),
    }
}

fn to_clock_ticks(
    ticks_ms: Option<f64>,
    ticks_text: Option<String>,
) -> PyResult<fluxwright::ClockTicks> {
    match (ticks_ms, ticks_text) {
        (Some(ms), None) => Ok(fluxwright::ClockTicks::Ms(ms)),
        (None, Some(text)) => Ok(fluxwright::ClockTicks::Text(text)),
        _ => Err(PyValueError::new_err(
            "clock ticks must specify either ticks_ms or ticks_text",
        )),
    }
}

#[pymethods]
impl Page {
    #[new]
    fn new(lease: &Bound<'_, Lease>) -> PyResult<Self> {
        let lease = lease.get().0.lock().unwrap().take();
        let lease = lease.ok_or_else(|| PyValueError::new_err("this lease already has a page"))?;
        let target_id = lease.target_id().to_owned();
        let opener_target_id = lease.opener_target_id().map(str::to_owned);
        let url = lease.subscribe_url();
        let (closing, _) = tokio::sync::watch::channel(false);
        Ok(Page {
            lease: Arc::new(PageSlot {
                page: RwLock::new(Some(lease)),
                closing,
            }),
            url,
            target_id,
            opener_target_id,
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
        let wait = parse_wait_until(wait_until.as_deref())?;
        run_in(py, &self.lease, timeout, move |p| {
            Box::pin(async move { p.goto_with(&url, wait).await.map(none) })
        })
    }

    #[getter]
    fn url(&self) -> String {
        self.url.borrow().clone()
    }

    #[pyo3(signature = (*, wait_until=None, timeout=None))]
    fn reload<'py>(
        &self,
        py: Python<'py>,
        wait_until: Option<String>,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let wait = parse_wait_until(wait_until.as_deref())?;
        run_in(py, &self.lease, timeout, move |p| {
            Box::pin(async move { p.reload_with(wait).await.map(none) })
        })
    }

    #[pyo3(signature = (*, wait_until=None, timeout=None))]
    fn go_back<'py>(
        &self,
        py: Python<'py>,
        wait_until: Option<String>,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let wait = parse_wait_until(wait_until.as_deref())?;
        run_in(py, &self.lease, timeout, move |p| {
            Box::pin(async move {
                p.go_back_with(wait).await?;
                Ok(None::<()>)
            })
        })
    }

    #[pyo3(signature = (*, wait_until=None, timeout=None))]
    fn go_forward<'py>(
        &self,
        py: Python<'py>,
        wait_until: Option<String>,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let wait = parse_wait_until(wait_until.as_deref())?;
        run_in(py, &self.lease, timeout, move |p| {
            Box::pin(async move {
                p.go_forward_with(wait).await?;
                Ok(None::<()>)
            })
        })
    }

    #[pyo3(signature = (html, *, wait_until=None, timeout=None))]
    fn set_content<'py>(
        &self,
        py: Python<'py>,
        html: String,
        wait_until: Option<String>,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let wait = parse_wait_until(wait_until.as_deref())?;
        run_in(py, &self.lease, timeout, move |p| {
            Box::pin(async move { p.set_content_with(&html, wait).await.map(none) })
        })
    }

    #[pyo3(signature = (state=None, *, timeout=None))]
    fn wait_for_load_state<'py>(
        &self,
        py: Python<'py>,
        state: Option<String>,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let state = parse_load_state(state.as_deref())?;
        run_in(py, &self.lease, timeout, move |p| {
            Box::pin(async move { p.wait_for_load_state(state).await.map(none) })
        })
    }

    #[pyo3(signature = (expression, arg=None, *, timeout=None, polling=None))]
    fn wait_for_function<'py>(
        &self,
        py: Python<'py>,
        expression: String,
        arg: Option<Bound<'py, PyAny>>,
        timeout: Option<f64>,
        polling: Option<&Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let arg = arg.map(|a| to_json(&a)).transpose()?;
        let script = page_function(&expression, arg.as_ref());
        let polling = match polling {
            None => fluxwright::WaitPolling::Raf,
            Some(v) => {
                if v.is_instance_of::<PyBool>() {
                    return Err(PyValueError::new_err("polling must be 'raf' or a positive number"));
                }
                if let Ok(s) = v.extract::<String>() {
                    if s == "raf" {
                        fluxwright::WaitPolling::Raf
                    } else {
                        return Err(PyValueError::new_err(format!(
                            "polling must be 'raf' or a positive number, not {s:?}"
                        )));
                    }
                } else if let Ok(ms) = v.extract::<f64>() {
                    if ms > 0.0 && ms.is_finite() {
                        fluxwright::WaitPolling::Interval(Duration::from_secs_f64(ms / 1000.0))
                    } else {
                        return Err(PyValueError::new_err("polling must be 'raf' or a positive number"));
                    }
                } else {
                    return Err(PyValueError::new_err("polling must be 'raf' or a positive number"));
                }
            }
        };
        run_in(py, &self.lease, timeout, move |p| {
            Box::pin(async move { p.wait_for_function_with(&script, polling).await.map(Json) })
        })
    }

    fn _navigation_timeout(&self) -> f64 {
        let Ok(page) = self.lease.page.try_read() else {
            return 30_000.0;
        };
        page.as_ref()
            .map(|p| p.navigation_timeout().as_millis() as f64)
            .unwrap_or(30_000.0)
    }

    fn _base_url(&self) -> Option<String> {
        let Ok(page) = self.lease.page.try_read() else {
            return None;
        };
        page.as_ref().and_then(|p| p.base_url())
    }

    #[pyo3(signature = (after_url, *, timeout=None))]
    fn _wait_for_url_change<'py>(
        &self,
        py: Python<'py>,
        after_url: String,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        run_in(py, &self.lease, timeout, move |p| {
            Box::pin(async move { p.wait_for_url_change(&after_url).await })
        })
    }

    fn title<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        run(py, &self.lease, |p| Box::pin(p.title()))
    }

    fn content<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        run(py, &self.lease, |p| Box::pin(p.content()))
    }

    #[pyo3(signature = (*, interactive_only=None, compact=None, max_depth=None, max_nodes=None, diff=None))]
    fn aria_snapshot<'py>(
        &self,
        py: Python<'py>,
        interactive_only: Option<bool>,
        compact: Option<bool>,
        max_depth: Option<usize>,
        max_nodes: Option<usize>,
        diff: Option<bool>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let mut opts = fluxwright::SnapshotOptions {
            interactive_only: interactive_only.unwrap_or(false),
            compact: compact.unwrap_or(true),
            max_depth,
            diff: diff.unwrap_or(false),
            ..Default::default()
        };
        if let Some(limit) = max_nodes {
            opts.max_nodes = limit;
        }
        run(py, &self.lease, move |p| {
            Box::pin(async move {
                let snap = p.aria_snapshot_with(&opts).await?;
                Ok(Json(serde_json::to_value(snap).unwrap_or_default()))
            })
        })
    }

    #[pyo3(signature = (*, selector=None, main_content_only=None, include_links=None, include_images=None))]
    fn markdown<'py>(
        &self,
        py: Python<'py>,
        selector: Option<String>,
        main_content_only: Option<bool>,
        include_links: Option<bool>,
        include_images: Option<bool>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let opts = fluxwright::MarkdownOptions {
            selector,
            main_content_only: main_content_only.unwrap_or(false),
            include_links: include_links.unwrap_or(true),
            include_images: include_images.unwrap_or(false),
        };
        run(py, &self.lease, move |p| {
            Box::pin(async move { p.markdown_with(&opts).await })
        })
    }

    #[pyo3(signature = (
        selector,
        *,
        button=None,
        click_count=None,
        delay=None,
        position=None,
        modifiers=None,
        force=None,
        trial=None,
        strict=None,
        timeout=None
    ))]
    #[allow(clippy::too_many_arguments)]
    fn click<'py>(
        &self,
        py: Python<'py>,
        selector: String,
        button: Option<&str>,
        click_count: Option<u32>,
        delay: Option<f64>,
        position: Option<&Bound<'_, PyAny>>,
        modifiers: Option<Vec<String>>,
        force: Option<bool>,
        trial: Option<bool>,
        strict: Option<bool>,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        do_click(
            py,
            &self.lease,
            page_sel(selector, strict),
            button,
            click_count.unwrap_or(1),
            delay,
            position,
            modifiers,
            force,
            trial,
            false,
            timeout,
        )
    }

    #[pyo3(signature = (
        selector,
        *,
        button=None,
        delay=None,
        position=None,
        modifiers=None,
        force=None,
        trial=None,
        strict=None,
        timeout=None
    ))]
    #[allow(clippy::too_many_arguments)]
    fn dblclick<'py>(
        &self,
        py: Python<'py>,
        selector: String,
        button: Option<&str>,
        delay: Option<f64>,
        position: Option<&Bound<'_, PyAny>>,
        modifiers: Option<Vec<String>>,
        force: Option<bool>,
        trial: Option<bool>,
        strict: Option<bool>,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        do_click(
            py,
            &self.lease,
            page_sel(selector, strict),
            button,
            2,
            delay,
            position,
            modifiers,
            force,
            trial,
            true,
            timeout,
        )
    }

    #[pyo3(signature = (selector, *, position=None, modifiers=None, force=None, trial=None, strict=None, timeout=None))]
    #[allow(clippy::too_many_arguments)]
    fn hover<'py>(
        &self,
        py: Python<'py>,
        selector: String,
        position: Option<&Bound<'_, PyAny>>,
        modifiers: Option<Vec<String>>,
        force: Option<bool>,
        trial: Option<bool>,
        strict: Option<bool>,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        do_hover_or_tap(
            py,
            &self.lease,
            page_sel(selector, strict),
            position,
            modifiers,
            force,
            trial,
            false,
            timeout,
        )
    }

    #[pyo3(signature = (selector, *, position=None, modifiers=None, force=None, trial=None, strict=None, timeout=None))]
    #[allow(clippy::too_many_arguments)]
    fn tap<'py>(
        &self,
        py: Python<'py>,
        selector: String,
        position: Option<&Bound<'_, PyAny>>,
        modifiers: Option<Vec<String>>,
        force: Option<bool>,
        trial: Option<bool>,
        strict: Option<bool>,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        do_hover_or_tap(
            py,
            &self.lease,
            page_sel(selector, strict),
            position,
            modifiers,
            force,
            trial,
            true,
            timeout,
        )
    }

    #[pyo3(signature = (selector, *, strict=None, timeout=None))]
    fn focus<'py>(
        &self,
        py: Python<'py>,
        selector: String,
        strict: Option<bool>,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let sel = page_sel(selector, strict);
        run_in(py, &self.lease, timeout, move |p| {
            Box::pin(async move { p.focus(&sel).await.map(none) })
        })
    }

    #[pyo3(signature = (selector, *, strict=None, timeout=None))]
    fn blur<'py>(
        &self,
        py: Python<'py>,
        selector: String,
        strict: Option<bool>,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let sel = page_sel(selector, strict);
        run_in(py, &self.lease, timeout, move |p| {
            Box::pin(async move { p.blur(&sel).await.map(none) })
        })
    }

    #[pyo3(signature = (selector, *, strict=None, timeout=None))]
    fn scroll_into_view_if_needed<'py>(
        &self,
        py: Python<'py>,
        selector: String,
        strict: Option<bool>,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let sel = page_sel(selector, strict);
        run_in(py, &self.lease, timeout, move |p| {
            Box::pin(async move { p.scroll_into_view_if_needed(&sel).await.map(none) })
        })
    }

    #[pyo3(signature = (source, target, *, force=None, strict=None, timeout=None))]
    fn drag_and_drop<'py>(
        &self,
        py: Python<'py>,
        source: String,
        target: String,
        force: Option<bool>,
        strict: Option<bool>,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        do_drag(py, &self.lease, page_sel(source, strict), page_sel(target, strict), force, timeout)
    }

    #[pyo3(signature = (selector, value, *, strict=None, timeout=None))]
    fn fill<'py>(
        &self,
        py: Python<'py>,
        selector: String,
        value: String,
        strict: Option<bool>,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let sel = page_sel(selector, strict);
        run_in(py, &self.lease, timeout, move |p| {
            Box::pin(async move { p.fill(&sel, &value).await.map(none) })
        })
    }

    #[pyo3(signature = (selector, *, force=None, strict=None, timeout=None))]
    fn clear<'py>(
        &self,
        py: Python<'py>,
        selector: String,
        force: Option<bool>,
        strict: Option<bool>,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let force = force.unwrap_or(false);
        let sel = page_sel(selector, strict);
        run_in(py, &self.lease, timeout, move |p| {
            Box::pin(async move { p.clear(&sel, force).await.map(none) })
        })
    }

    #[pyo3(signature = (selector, key, *, delay=None, strict=None, timeout=None))]
    fn press<'py>(
        &self,
        py: Python<'py>,
        selector: String,
        key: String,
        delay: Option<f64>,
        strict: Option<bool>,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        do_press(py, &self.lease, page_sel(selector, strict), key, delay, timeout)
    }

    #[pyo3(signature = (selector, text, *, delay=None, strict=None, timeout=None))]
    fn press_sequentially<'py>(
        &self,
        py: Python<'py>,
        selector: String,
        text: String,
        delay: Option<f64>,
        strict: Option<bool>,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        do_type(py, &self.lease, page_sel(selector, strict), text, delay, timeout)
    }

    #[pyo3(name = "type", signature = (selector, text, *, delay=None, strict=None, timeout=None))]
    fn r#type<'py>(
        &self,
        py: Python<'py>,
        selector: String,
        text: String,
        delay: Option<f64>,
        strict: Option<bool>,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        do_type(py, &self.lease, page_sel(selector, strict), text, delay, timeout)
    }

    #[pyo3(signature = (selector, *, position=None, force=None, trial=None, strict=None, timeout=None))]
    #[allow(clippy::too_many_arguments)]
    fn check<'py>(
        &self,
        py: Python<'py>,
        selector: String,
        position: Option<&Bound<'_, PyAny>>,
        force: Option<bool>,
        trial: Option<bool>,
        strict: Option<bool>,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        do_check(py, &self.lease, page_sel(selector, strict), true, position, force, trial, timeout)
    }

    #[pyo3(signature = (selector, *, position=None, force=None, trial=None, strict=None, timeout=None))]
    #[allow(clippy::too_many_arguments)]
    fn uncheck<'py>(
        &self,
        py: Python<'py>,
        selector: String,
        position: Option<&Bound<'_, PyAny>>,
        force: Option<bool>,
        trial: Option<bool>,
        strict: Option<bool>,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        do_check(py, &self.lease, page_sel(selector, strict), false, position, force, trial, timeout)
    }

    #[pyo3(signature = (selector, checked, *, position=None, force=None, trial=None, strict=None, timeout=None))]
    #[allow(clippy::too_many_arguments)]
    fn set_checked<'py>(
        &self,
        py: Python<'py>,
        selector: String,
        checked: bool,
        position: Option<&Bound<'_, PyAny>>,
        force: Option<bool>,
        trial: Option<bool>,
        strict: Option<bool>,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        do_check(py, &self.lease, page_sel(selector, strict), checked, position, force, trial, timeout)
    }

    #[pyo3(signature = (selector, value=None, *, index=None, label=None, element=None, force=None, strict=None, timeout=None))]
    #[allow(clippy::too_many_arguments)]
    fn select_option<'py>(
        &self,
        py: Python<'py>,
        selector: String,
        value: Option<&Bound<'_, PyAny>>,
        index: Option<&Bound<'_, PyAny>>,
        label: Option<&Bound<'_, PyAny>>,
        element: Option<&Bound<'_, PyAny>>,
        force: Option<bool>,
        strict: Option<bool>,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        do_select_option(
            py,
            &self.lease,
            page_sel(selector, strict),
            value,
            index,
            label,
            element,
            force,
            timeout,
        )
    }

    #[pyo3(signature = (selector, files, *, strict=None, timeout=None))]
    fn set_input_files<'py>(
        &self,
        py: Python<'py>,
        selector: String,
        files: &Bound<'_, PyAny>,
        strict: Option<bool>,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        do_set_input_files(py, &self.lease, page_sel(selector, strict), files, timeout)
    }

    #[pyo3(name = "_mouse_move", signature = (x, y, *, steps=None))]
    fn mouse_move<'py>(
        &self,
        py: Python<'py>,
        x: f64,
        y: f64,
        steps: Option<u32>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let steps = steps.unwrap_or(1);
        run(py, &self.lease, move |p| {
            Box::pin(async move { p.mouse_move(x, y, steps).await.map(none) })
        })
    }

    #[pyo3(name = "_mouse_down", signature = (*, button=None, click_count=None))]
    fn mouse_down<'py>(
        &self,
        py: Python<'py>,
        button: Option<&str>,
        click_count: Option<u32>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let button = parse_button(button)?;
        let click_count = click_count.unwrap_or(1);
        run(py, &self.lease, move |p| {
            Box::pin(async move { p.mouse_down(button, click_count).await.map(none) })
        })
    }

    #[pyo3(name = "_mouse_up", signature = (*, button=None, click_count=None))]
    fn mouse_up<'py>(
        &self,
        py: Python<'py>,
        button: Option<&str>,
        click_count: Option<u32>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let button = parse_button(button)?;
        let click_count = click_count.unwrap_or(1);
        run(py, &self.lease, move |p| {
            Box::pin(async move { p.mouse_up(button, click_count).await.map(none) })
        })
    }

    #[pyo3(name = "_mouse_click", signature = (x, y, *, delay=None, button=None, click_count=None))]
    fn mouse_click<'py>(
        &self,
        py: Python<'py>,
        x: f64,
        y: f64,
        delay: Option<f64>,
        button: Option<&str>,
        click_count: Option<u32>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let button = parse_button(button)?;
        let click_count = click_count.unwrap_or(1);
        let delay = opt_delay(delay)?;
        run(py, &self.lease, move |p| {
            Box::pin(async move { p.mouse_click(x, y, button, click_count, delay).await.map(none) })
        })
    }

    #[pyo3(name = "_mouse_dblclick", signature = (x, y, *, delay=None, button=None))]
    fn mouse_dblclick<'py>(
        &self,
        py: Python<'py>,
        x: f64,
        y: f64,
        delay: Option<f64>,
        button: Option<&str>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let button = parse_button(button)?;
        let delay = opt_delay(delay)?;
        run(py, &self.lease, move |p| {
            Box::pin(async move { p.mouse_dblclick(x, y, button, delay).await.map(none) })
        })
    }

    #[pyo3(name = "_mouse_wheel")]
    fn mouse_wheel<'py>(
        &self,
        py: Python<'py>,
        delta_x: f64,
        delta_y: f64,
    ) -> PyResult<Bound<'py, PyAny>> {
        run(py, &self.lease, move |p| {
            Box::pin(async move { p.mouse_wheel(delta_x, delta_y).await.map(none) })
        })
    }

    #[pyo3(name = "_keyboard_down")]
    fn keyboard_down<'py>(&self, py: Python<'py>, key: String) -> PyResult<Bound<'py, PyAny>> {
        run(py, &self.lease, move |p| {
            Box::pin(async move { p.keyboard_down(&key).await.map(none) })
        })
    }

    #[pyo3(name = "_keyboard_up")]
    fn keyboard_up<'py>(&self, py: Python<'py>, key: String) -> PyResult<Bound<'py, PyAny>> {
        run(py, &self.lease, move |p| {
            Box::pin(async move { p.keyboard_up(&key).await.map(none) })
        })
    }

    #[pyo3(name = "_keyboard_press", signature = (key, *, delay=None))]
    fn keyboard_press<'py>(
        &self,
        py: Python<'py>,
        key: String,
        delay: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let delay = opt_delay(delay)?;
        run(py, &self.lease, move |p| {
            Box::pin(async move { p.keyboard_press(&key, delay).await.map(none) })
        })
    }

    #[pyo3(name = "_keyboard_type", signature = (text, *, delay=None))]
    fn keyboard_type<'py>(
        &self,
        py: Python<'py>,
        text: String,
        delay: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let delay = opt_delay(delay)?;
        run(py, &self.lease, move |p| {
            Box::pin(async move { p.keyboard_type(&text, delay).await.map(none) })
        })
    }

    #[pyo3(name = "_keyboard_insert_text")]
    fn keyboard_insert_text<'py>(
        &self,
        py: Python<'py>,
        text: String,
    ) -> PyResult<Bound<'py, PyAny>> {
        run(py, &self.lease, move |p| {
            Box::pin(async move { p.keyboard_insert_text(&text).await.map(none) })
        })
    }

    #[pyo3(name = "_touchscreen_tap")]
    fn touchscreen_tap<'py>(&self, py: Python<'py>, x: f64, y: f64) -> PyResult<Bound<'py, PyAny>> {
        run(py, &self.lease, move |p| {
            Box::pin(async move { p.touchscreen_tap(x, y).await.map(none) })
        })
    }

    #[pyo3(signature = (selector, *, state=None, strict=None, timeout=None))]
    fn wait_for_selector<'py>(
        &self,
        py: Python<'py>,
        selector: String,
        state: Option<String>,
        strict: Option<bool>,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let state = wait_state(state)?;
        let sel = page_sel(selector, strict);
        run_in(py, &self.lease, timeout, move |p| {
            Box::pin(async move { p.wait_for_selector_state(&sel, state).await.map(none) })
        })
    }

    #[pyo3(signature = (selector, *, strict=None, timeout=None))]
    fn text_content<'py>(&self, py: Python<'py>, selector: String, strict: Option<bool>, timeout: Option<f64>) -> PyResult<Bound<'py, PyAny>> {
        let sel = page_sel(selector, strict);
        run_in(py, &self.lease, timeout, move |p| Box::pin(async move { p.text_content(&sel).await }))
    }

    #[pyo3(signature = (selector, *, strict=None, timeout=None))]
    fn inner_text<'py>(&self, py: Python<'py>, selector: String, strict: Option<bool>, timeout: Option<f64>) -> PyResult<Bound<'py, PyAny>> {
        let sel = page_sel(selector, strict);
        run_in(py, &self.lease, timeout, move |p| Box::pin(async move { p.inner_text(&sel).await }))
    }

    #[pyo3(signature = (selector, *, strict=None, timeout=None))]
    fn inner_html<'py>(&self, py: Python<'py>, selector: String, strict: Option<bool>, timeout: Option<f64>) -> PyResult<Bound<'py, PyAny>> {
        let sel = page_sel(selector, strict);
        run_in(py, &self.lease, timeout, move |p| Box::pin(async move { p.inner_html(&sel).await }))
    }

    #[pyo3(signature = (selector, *, strict=None, timeout=None))]
    fn input_value<'py>(&self, py: Python<'py>, selector: String, strict: Option<bool>, timeout: Option<f64>) -> PyResult<Bound<'py, PyAny>> {
        let sel = page_sel(selector, strict);
        run_in(py, &self.lease, timeout, move |p| Box::pin(async move { p.input_value(&sel).await }))
    }

    #[pyo3(signature = (selector, name, *, strict=None, timeout=None))]
    fn get_attribute<'py>(&self, py: Python<'py>, selector: String, name: String, strict: Option<bool>, timeout: Option<f64>) -> PyResult<Bound<'py, PyAny>> {
        let sel = page_sel(selector, strict);
        run_in(py, &self.lease, timeout, move |p| Box::pin(async move { p.get_attribute(&sel, &name).await }))
    }

    #[pyo3(signature = (selector, *, strict=None, timeout=None))]
    fn is_visible<'py>(&self, py: Python<'py>, selector: String, strict: Option<bool>, timeout: Option<f64>) -> PyResult<Bound<'py, PyAny>> {
        let sel = page_sel(selector, strict);
        run_in(py, &self.lease, timeout, move |p| Box::pin(async move { p.is_visible(&sel).await }))
    }

    #[pyo3(signature = (selector, *, strict=None, timeout=None))]
    fn is_hidden<'py>(&self, py: Python<'py>, selector: String, strict: Option<bool>, timeout: Option<f64>) -> PyResult<Bound<'py, PyAny>> {
        let sel = page_sel(selector, strict);
        run_in(py, &self.lease, timeout, move |p| Box::pin(async move { p.is_hidden(&sel).await }))
    }

    #[pyo3(signature = (selector, *, strict=None, timeout=None))]
    fn is_enabled<'py>(&self, py: Python<'py>, selector: String, strict: Option<bool>, timeout: Option<f64>) -> PyResult<Bound<'py, PyAny>> {
        let sel = page_sel(selector, strict);
        run_in(py, &self.lease, timeout, move |p| Box::pin(async move { p.is_enabled(&sel).await }))
    }

    #[pyo3(signature = (selector, *, strict=None, timeout=None))]
    fn is_disabled<'py>(&self, py: Python<'py>, selector: String, strict: Option<bool>, timeout: Option<f64>) -> PyResult<Bound<'py, PyAny>> {
        let sel = page_sel(selector, strict);
        run_in(py, &self.lease, timeout, move |p| Box::pin(async move { p.is_disabled(&sel).await }))
    }

    #[pyo3(signature = (selector, *, strict=None, timeout=None))]
    fn is_checked<'py>(&self, py: Python<'py>, selector: String, strict: Option<bool>, timeout: Option<f64>) -> PyResult<Bound<'py, PyAny>> {
        let sel = page_sel(selector, strict);
        run_in(py, &self.lease, timeout, move |p| Box::pin(async move { p.is_checked(&sel).await }))
    }

    #[pyo3(signature = (selector, *, strict=None, timeout=None))]
    fn is_editable<'py>(&self, py: Python<'py>, selector: String, strict: Option<bool>, timeout: Option<f64>) -> PyResult<Bound<'py, PyAny>> {
        let sel = page_sel(selector, strict);
        run_in(py, &self.lease, timeout, move |p| Box::pin(async move { p.is_editable(&sel).await }))
    }

    #[pyo3(signature = (expression, arg=None))]
    fn evaluate<'py>(
        &self,
        py: Python<'py>,
        expression: String,
        arg: Option<Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let script = eval_script(&expression, arg)?;
        run(py, &self.lease, move |p| {
            Box::pin(async move { p.evaluate(&script).await.map(Json) })
        })
    }

    /// PNG or JPEG bytes; with `path`, also written there (its extension picks the type when
    /// `type` is not given). `clip` is `{x, y, width, height}` relative to the viewport, or to
    /// the page with `full_page`.
    #[pyo3(signature = (*, path=None, full_page=None, r#type=None, quality=None, clip=None, omit_background=None,
        annotate=None, mask=None, mask_color=None, animations=None, caret=None, timeout=None))]
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
        annotate: Option<bool>,
        mask: Option<Bound<'py, PyAny>>,
        mask_color: Option<String>,
        animations: Option<String>,
        caret: Option<String>,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let timeout = call_timeout(timeout);
        let mut shot = shot_options(
            &self.lease,
            path.as_deref(),
            r#type.as_deref(),
            quality,
            omit_background,
            annotate,
            mask.as_ref(),
            mask_color,
            animations.as_deref(),
            caret.as_deref(),
        )?;
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

    /// Every cookie in this page's context, plus localStorage (and optionally IndexedDB) of the current origin, in
    /// Playwright's format; with `path`, also saved as JSON for `new_page(storage_state=path)`.
    #[pyo3(signature = (*, path=None, indexed_db=false))]
    fn storage_state<'py>(
        &self,
        py: Python<'py>,
        path: Option<PathBuf>,
        indexed_db: bool,
    ) -> PyResult<Bound<'py, PyAny>> {
        let lease = self.lease.clone();
        future_into_py(py, async move {
            let state = with_page(&lease, move |p| Box::pin(p.storage_state_with(indexed_db))).await?;
            let json =
                serde_json::to_value(&state).map_err(|e| PyValueError::new_err(e.to_string()))?;
            save_storage_json(json, path).await
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
            || self
                .lease
                .page
                .try_read()
                .map_or(true, |g| g.as_ref().is_none_or(|p| p.is_closed()))
    }

    /// CSS, `text=`, `xpath=`, or `role=` selector.
    #[pyo3(signature = (selector, *, has=None, has_not=None, has_text=None, has_not_text=None))]
    fn locator(
        &self,
        selector: String,
        has: Option<PyRef<'_, Locator>>,
        has_not: Option<PyRef<'_, Locator>>,
        has_text: Option<&Bound<'_, PyAny>>,
        has_not_text: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Locator> {
        self.top().locator(selector, has, has_not, has_text, has_not_text)
    }

    #[pyo3(signature = (text, *, exact=None))]
    fn get_by_text(&self, text: &str, exact: Option<bool>) -> Locator {
        self.top().get_by_text(text, exact)
    }

    #[pyo3(signature = (role, *, checked=None, disabled=None, exact=None, expanded=None, include_hidden=None, level=None, name=None, pressed=None, selected=None))]
    #[allow(clippy::too_many_arguments)]
    fn get_by_role(
        &self,
        role: &str,
        checked: Option<bool>,
        disabled: Option<bool>,
        exact: Option<bool>,
        expanded: Option<bool>,
        include_hidden: Option<bool>,
        level: Option<u32>,
        name: Option<&Bound<'_, PyAny>>,
        pressed: Option<bool>,
        selected: Option<bool>,
    ) -> PyResult<Locator> {
        self.top().get_by_role(
            role,
            checked,
            disabled,
            exact,
            expanded,
            include_hidden,
            level,
            name,
            pressed,
            selected,
        )
    }

    #[pyo3(signature = (text, *, exact=None))]
    fn get_by_label(&self, text: &str, exact: Option<bool>) -> Locator {
        self.top().get_by_label(text, exact)
    }

    #[pyo3(signature = (text, *, exact=None))]
    fn get_by_placeholder(&self, text: &str, exact: Option<bool>) -> Locator {
        self.top().get_by_placeholder(text, exact)
    }

    #[pyo3(signature = (text, *, exact=None))]
    fn get_by_alt_text(&self, text: &str, exact: Option<bool>) -> Locator {
        self.top().get_by_alt_text(text, exact)
    }

    #[pyo3(signature = (text, *, exact=None))]
    fn get_by_title(&self, text: &str, exact: Option<bool>) -> Locator {
        self.top().get_by_title(text, exact)
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

    #[pyo3(name = "_file_choosers")]
    fn file_choosers(&self) -> PyResult<FileChooserStream> {
        self.now(|p| FileChooserStream(Arc::new(Mutex::new(p.subscribe_file_choosers()))))
    }

    /// Starts handing this page's requests over; resolves once interception is on, so a `goto`
    /// after it is covered.
    #[pyo3(name = "_intercept")]
    fn intercept<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        run(py, &self.lease, |p| {
            Box::pin(async move { Ok(RouteStream(Arc::new(Mutex::new(p.intercept().await?)))) })
        })
    }

    #[pyo3(name = "_emulate_media", signature = (*, media=None, color_scheme=None))]
    fn emulate_media<'py>(
        &self,
        py: Python<'py>,
        media: Option<String>,
        color_scheme: Option<String>,
    ) -> PyResult<Bound<'py, PyAny>> {
        run(py, &self.lease, move |p| {
            Box::pin(async move {
                p.emulate_media(media.as_deref(), color_scheme.as_deref())
                    .await
                    .map(none)
            })
        })
    }

    #[pyo3(name = "_add_cookies")]
    fn add_cookies<'py>(
        &self,
        py: Python<'py>,
        cookies: Vec<Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let parsed = parse_cookies(cookies)?;
        run(py, &self.lease, move |p| {
            Box::pin(async move { p.add_cookies(&parsed).await.map(none) })
        })
    }

    #[pyo3(name = "_clear_cookies")]
    fn clear_cookies<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        run(py, &self.lease, |p| {
            Box::pin(async move { p.clear_cookies().await.map(none) })
        })
    }

    #[allow(clippy::too_many_arguments)]
    #[pyo3(name = "_api_fetch", signature = (url, *, method=None, headers=None, params=None, data=None, fail_on_status_code=false, max_redirects=None, timeout=None))]
    fn api_fetch<'py>(
        &self,
        py: Python<'py>,
        url: String,
        method: Option<String>,
        headers: Option<HashMap<String, String>>,
        params: Option<Vec<(String, String)>>,
        data: Option<Bound<'py, PyAny>>,
        fail_on_status_code: bool,
        max_redirects: Option<usize>,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let opts = build_api_options(
            method,
            headers,
            params,
            data.as_ref(),
            fail_on_status_code,
            max_redirects,
            timeout,
        )?;
        run(py, &self.lease, move |p| {
            Box::pin(async move {
                let resp = p.request().fetch(&url, &opts).await?;
                Ok(ApiResponse::from_core(resp))
            })
        })
    }

    #[pyo3(name = "_add_init_script")]
    fn add_init_script<'py>(&self, py: Python<'py>, source: String) -> PyResult<Bound<'py, PyAny>> {
        run(py, &self.lease, move |p| {
            Box::pin(async move { p.add_init_script(&source).await.map(none) })
        })
    }

    #[pyo3(name = "_clock_install", signature = (*, time_ms=None, time_iso=None))]
    fn clock_install<'py>(
        &self,
        py: Python<'py>,
        time_ms: Option<f64>,
        time_iso: Option<String>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let time = if time_ms.is_none() && time_iso.is_none() {
            None
        } else {
            Some(to_clock_time(time_ms, time_iso)?)
        };
        let opts = fluxwright::ClockInstallOptions { time };
        run(py, &self.lease, move |p| {
            Box::pin(async move { p.clock().install_with(&opts).await.map(none) })
        })
    }

    #[pyo3(name = "_clock_pause_at", signature = (*, time_ms=None, time_iso=None))]
    fn clock_pause_at<'py>(
        &self,
        py: Python<'py>,
        time_ms: Option<f64>,
        time_iso: Option<String>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let time = to_clock_time(time_ms, time_iso)?;
        run(py, &self.lease, move |p| {
            Box::pin(async move { p.clock().pause_at(time).await.map(none) })
        })
    }

    #[pyo3(name = "_clock_set_fixed_time", signature = (*, time_ms=None, time_iso=None))]
    fn clock_set_fixed_time<'py>(
        &self,
        py: Python<'py>,
        time_ms: Option<f64>,
        time_iso: Option<String>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let time = to_clock_time(time_ms, time_iso)?;
        run(py, &self.lease, move |p| {
            Box::pin(async move { p.clock().set_fixed_time(time).await.map(none) })
        })
    }

    #[pyo3(name = "_clock_set_system_time", signature = (*, time_ms=None, time_iso=None))]
    fn clock_set_system_time<'py>(
        &self,
        py: Python<'py>,
        time_ms: Option<f64>,
        time_iso: Option<String>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let time = to_clock_time(time_ms, time_iso)?;
        run(py, &self.lease, move |p| {
            Box::pin(async move { p.clock().set_system_time(time).await.map(none) })
        })
    }

    #[pyo3(name = "_clock_fast_forward", signature = (*, ticks_ms=None, ticks_text=None))]
    fn clock_fast_forward<'py>(
        &self,
        py: Python<'py>,
        ticks_ms: Option<f64>,
        ticks_text: Option<String>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let ticks = to_clock_ticks(ticks_ms, ticks_text)?;
        run(py, &self.lease, move |p| {
            Box::pin(async move { p.clock().fast_forward(ticks).await.map(none) })
        })
    }

    #[pyo3(name = "_clock_run_for", signature = (*, ticks_ms=None, ticks_text=None))]
    fn clock_run_for<'py>(
        &self,
        py: Python<'py>,
        ticks_ms: Option<f64>,
        ticks_text: Option<String>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let ticks = to_clock_ticks(ticks_ms, ticks_text)?;
        run(py, &self.lease, move |p| {
            Box::pin(async move { p.clock().run_for(ticks).await.map(none) })
        })
    }

    #[pyo3(name = "_clock_resume")]
    fn clock_resume<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        run(py, &self.lease, |p| {
            Box::pin(async move { p.clock().resume().await.map(none) })
        })
    }

    #[pyo3(name = "_add_script_tag", signature = (*, url=None, content=None, script_type=None))]
    fn add_script_tag<'py>(
        &self,
        py: Python<'py>,
        url: Option<String>,
        content: Option<String>,
        script_type: Option<String>,
    ) -> PyResult<Bound<'py, PyAny>> {
        run(py, &self.lease, move |p| {
            Box::pin(async move {
                p.add_script_tag(&fluxwright::ScriptTagOptions {
                    url,
                    content,
                    script_type,
                })
                .await
                .map(none)
            })
        })
    }

    #[pyo3(name = "_add_style_tag", signature = (*, url=None, content=None))]
    fn add_style_tag<'py>(
        &self,
        py: Python<'py>,
        url: Option<String>,
        content: Option<String>,
    ) -> PyResult<Bound<'py, PyAny>> {
        run(py, &self.lease, move |p| {
            Box::pin(async move {
                p.add_style_tag(&fluxwright::StyleTagOptions { url, content })
                    .await
                    .map(none)
            })
        })
    }

    #[pyo3(name = "_binding_calls")]
    fn binding_calls(&self) -> PyResult<BindingStream> {
        self.now(|p| BindingStream(Arc::new(Mutex::new(p.subscribe_bindings()))))
    }

    #[pyo3(name = "_expose_binding")]
    fn expose_binding<'py>(&self, py: Python<'py>, name: String) -> PyResult<Bound<'py, PyAny>> {
        run(py, &self.lease, move |p| {
            Box::pin(async move { p.expose_binding(&name).await.map(none) })
        })
    }

    fn bring_to_front<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        run(py, &self.lease, |p| {
            Box::pin(async move { p.bring_to_front().await.map(none) })
        })
    }

    #[pyo3(name = "_target_id")]
    fn target_id(&self) -> String {
        self.target_id.clone()
    }

    #[pyo3(name = "_opener_target_id")]
    fn opener_target_id(&self) -> Option<String> {
        self.opener_target_id.clone()
    }

    #[pyo3(name = "_popups")]
    fn popups(&self) -> PyResult<PopupStream> {
        self.now(|p| PopupStream(Arc::new(Mutex::new(p.subscribe_popups()))))
    }

    #[pyo3(name = "_main_frame_id")]
    fn main_frame_id(&self) -> String {
        self.target_id.clone()
    }

    #[pyo3(name = "_frames")]
    fn frames(&self) -> Vec<FrameInfo> {
        if *self.lease.closing.borrow() {
            return Vec::new();
        }
        let Ok(page) = self.lease.page.try_read() else {
            return Vec::new();
        };
        let Some(p) = page.as_ref().filter(|p| !p.is_closed()) else {
            return Vec::new();
        };
        p.frame_infos().into_iter().map(FrameInfo::from).collect()
    }

    #[pyo3(name = "_frame_goto", signature = (frame_id, url, *, wait_until=None, timeout=None))]
    fn frame_goto<'py>(
        &self,
        py: Python<'py>,
        frame_id: String,
        url: String,
        wait_until: Option<String>,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let wait = parse_wait_until(wait_until.as_deref())?;
        run_in(py, &self.lease, timeout, move |p| {
            Box::pin(async move { p.frame_goto_with(&frame_id, &url, wait).await.map(none) })
        })
    }

    #[pyo3(name = "_frame_evaluate", signature = (frame_id, expression, arg=None))]
    fn frame_evaluate<'py>(
        &self,
        py: Python<'py>,
        frame_id: String,
        expression: String,
        arg: Option<Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let script = eval_script(&expression, arg)?;
        run(py, &self.lease, move |p| {
            Box::pin(async move { p.frame_evaluate(&frame_id, &script).await.map(Json) })
        })
    }

    #[pyo3(name = "_frame_by_id")]
    fn frame_by_id(&self, frame_id: String) -> FrameLocator {
        FrameLocator {
            lease: self.lease.clone(),
            frames: vec![fluxwright::Selector::frame_id_hop(&frame_id)],
        }
    }

    #[pyo3(name = "_new_cdp_session", signature = (frame_id=None))]
    fn new_cdp_session<'py>(
        &self,
        py: Python<'py>,
        frame_id: Option<String>,
    ) -> PyResult<Bound<'py, PyAny>> {
        run(py, &self.lease, move |p| {
            let default_timeout = p.navigation_timeout();
            Box::pin(async move {
                let inner = match frame_id.as_deref() {
                    Some(fid) => p.new_frame_cdp_session(fid).await?,
                    None => p.new_cdp_session().await?,
                };
                Ok(CdpSession {
                    inner,
                    default_timeout,
                })
            })
        })
    }

    #[pyo3(name = "_websockets")]
    fn websockets(&self) -> PyResult<WebSocketStream> {
        self.now(|p| WebSocketStream(Arc::new(Mutex::new(p.subscribe_websockets()))))
    }

    #[pyo3(name = "_workers")]
    fn workers(&self) -> Vec<WorkerInfo> {
        if *self.lease.closing.borrow() {
            return Vec::new();
        }
        let Ok(page) = self.lease.page.try_read() else {
            return Vec::new();
        };
        let Some(p) = page.as_ref().filter(|p| !p.is_closed()) else {
            return Vec::new();
        };
        p.workers()
            .into_iter()
            .map(|w| WorkerInfo {
                id: w.target_id().to_owned(),
                url: w.url().to_owned(),
            })
            .collect()
    }

    #[pyo3(name = "_worker_evaluate", signature = (worker_id, expression, arg=None))]
    fn worker_evaluate<'py>(
        &self,
        py: Python<'py>,
        worker_id: String,
        expression: String,
        arg: Option<Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let script = eval_script(&expression, arg)?;
        run(py, &self.lease, move |p| {
            Box::pin(async move {
                let w = p
                    .worker(&worker_id)
                    .ok_or_else(|| fluxwright::Error::Other("worker is closed".into()))?;
                Ok(Json(w.evaluate(&script, p.navigation_timeout()).await?))
            })
        })
    }

    #[pyo3(name = "_worker_events")]
    fn worker_events(&self) -> PyResult<WorkerStream> {
        self.now(|p| WorkerStream(Arc::new(Mutex::new(p.subscribe_workers()))))
    }

    #[pyo3(name = "_start_har_recording", signature = (path, *, omit_content=None, content=None, mode=None, url_filter=None))]
    fn start_har_recording<'py>(
        &self,
        py: Python<'py>,
        path: PathBuf,
        omit_content: Option<bool>,
        content: Option<String>,
        mode: Option<String>,
        url_filter: Option<String>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let opts = build_record_har_options(path, omit_content, content, mode, url_filter)?;
        run(py, &self.lease, move |p| {
            Box::pin(async move { p.start_har_recording(opts).await.map(none) })
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
    #[pyo3(signature = (
        *,
        button=None,
        click_count=None,
        delay=None,
        position=None,
        modifiers=None,
        force=None,
        trial=None,
        timeout=None
    ))]
    #[allow(clippy::too_many_arguments)]
    fn click<'py>(
        &self,
        py: Python<'py>,
        button: Option<&str>,
        click_count: Option<u32>,
        delay: Option<f64>,
        position: Option<&Bound<'_, PyAny>>,
        modifiers: Option<Vec<String>>,
        force: Option<bool>,
        trial: Option<bool>,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        do_click(
            py,
            &self.lease,
            self.selector.clone(),
            button,
            click_count.unwrap_or(1),
            delay,
            position,
            modifiers,
            force,
            trial,
            false,
            timeout,
        )
    }

    #[pyo3(signature = (
        *,
        button=None,
        delay=None,
        position=None,
        modifiers=None,
        force=None,
        trial=None,
        timeout=None
    ))]
    #[allow(clippy::too_many_arguments)]
    fn dblclick<'py>(
        &self,
        py: Python<'py>,
        button: Option<&str>,
        delay: Option<f64>,
        position: Option<&Bound<'_, PyAny>>,
        modifiers: Option<Vec<String>>,
        force: Option<bool>,
        trial: Option<bool>,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        do_click(
            py,
            &self.lease,
            self.selector.clone(),
            button,
            2,
            delay,
            position,
            modifiers,
            force,
            trial,
            true,
            timeout,
        )
    }

    #[pyo3(signature = (*, position=None, modifiers=None, force=None, trial=None, timeout=None))]
    fn hover<'py>(
        &self,
        py: Python<'py>,
        position: Option<&Bound<'_, PyAny>>,
        modifiers: Option<Vec<String>>,
        force: Option<bool>,
        trial: Option<bool>,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        do_hover_or_tap(
            py,
            &self.lease,
            self.selector.clone(),
            position,
            modifiers,
            force,
            trial,
            false,
            timeout,
        )
    }

    #[pyo3(signature = (*, position=None, modifiers=None, force=None, trial=None, timeout=None))]
    fn tap<'py>(
        &self,
        py: Python<'py>,
        position: Option<&Bound<'_, PyAny>>,
        modifiers: Option<Vec<String>>,
        force: Option<bool>,
        trial: Option<bool>,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        do_hover_or_tap(
            py,
            &self.lease,
            self.selector.clone(),
            position,
            modifiers,
            force,
            trial,
            true,
            timeout,
        )
    }

    #[pyo3(signature = (*, timeout=None))]
    fn focus<'py>(&self, py: Python<'py>, timeout: Option<f64>) -> PyResult<Bound<'py, PyAny>> {
        let sel = self.selector.clone();
        run_in(py, &self.lease, timeout, move |p| {
            Box::pin(async move { p.focus(&sel).await.map(none) })
        })
    }

    #[pyo3(signature = (*, timeout=None))]
    fn blur<'py>(&self, py: Python<'py>, timeout: Option<f64>) -> PyResult<Bound<'py, PyAny>> {
        let sel = self.selector.clone();
        run_in(py, &self.lease, timeout, move |p| {
            Box::pin(async move { p.blur(&sel).await.map(none) })
        })
    }

    #[pyo3(signature = (*, timeout=None))]
    fn scroll_into_view_if_needed<'py>(
        &self,
        py: Python<'py>,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let sel = self.selector.clone();
        run_in(py, &self.lease, timeout, move |p| {
            Box::pin(async move { p.scroll_into_view_if_needed(&sel).await.map(none) })
        })
    }

    #[pyo3(signature = (target, *, force=None, timeout=None))]
    fn drag_to<'py>(
        &self,
        py: Python<'py>,
        target: &Locator,
        force: Option<bool>,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        if !Arc::ptr_eq(&self.lease, &target.lease) {
            return Err(PyValueError::new_err(
                "drag_to target locator must belong to the same page",
            ));
        }
        do_drag(
            py,
            &self.lease,
            self.selector.clone(),
            target.selector.clone(),
            force,
            timeout,
        )
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

    #[pyo3(signature = (*, force=None, timeout=None))]
    fn clear<'py>(
        &self,
        py: Python<'py>,
        force: Option<bool>,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let sel = self.selector.clone();
        let force = force.unwrap_or(false);
        run_in(py, &self.lease, timeout, move |p| {
            Box::pin(async move { p.clear(&sel, force).await.map(none) })
        })
    }

    #[pyo3(signature = (key, *, delay=None, timeout=None))]
    fn press<'py>(
        &self,
        py: Python<'py>,
        key: String,
        delay: Option<f64>,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        do_press(py, &self.lease, self.selector.clone(), key, delay, timeout)
    }

    #[pyo3(signature = (text, *, delay=None, timeout=None))]
    fn press_sequentially<'py>(
        &self,
        py: Python<'py>,
        text: String,
        delay: Option<f64>,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        do_type(py, &self.lease, self.selector.clone(), text, delay, timeout)
    }

    #[pyo3(name = "type", signature = (text, *, delay=None, timeout=None))]
    fn r#type<'py>(
        &self,
        py: Python<'py>,
        text: String,
        delay: Option<f64>,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        do_type(py, &self.lease, self.selector.clone(), text, delay, timeout)
    }

    #[pyo3(signature = (*, position=None, force=None, trial=None, timeout=None))]
    fn check<'py>(
        &self,
        py: Python<'py>,
        position: Option<&Bound<'_, PyAny>>,
        force: Option<bool>,
        trial: Option<bool>,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        do_check(
            py,
            &self.lease,
            self.selector.clone(),
            true,
            position,
            force,
            trial,
            timeout,
        )
    }

    #[pyo3(signature = (*, position=None, force=None, trial=None, timeout=None))]
    fn uncheck<'py>(
        &self,
        py: Python<'py>,
        position: Option<&Bound<'_, PyAny>>,
        force: Option<bool>,
        trial: Option<bool>,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        do_check(
            py,
            &self.lease,
            self.selector.clone(),
            false,
            position,
            force,
            trial,
            timeout,
        )
    }

    #[pyo3(signature = (checked, *, position=None, force=None, trial=None, timeout=None))]
    fn set_checked<'py>(
        &self,
        py: Python<'py>,
        checked: bool,
        position: Option<&Bound<'_, PyAny>>,
        force: Option<bool>,
        trial: Option<bool>,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        do_check(
            py,
            &self.lease,
            self.selector.clone(),
            checked,
            position,
            force,
            trial,
            timeout,
        )
    }

    #[pyo3(signature = (value=None, *, index=None, label=None, element=None, force=None, timeout=None))]
    #[allow(clippy::too_many_arguments)]
    fn select_option<'py>(
        &self,
        py: Python<'py>,
        value: Option<&Bound<'_, PyAny>>,
        index: Option<&Bound<'_, PyAny>>,
        label: Option<&Bound<'_, PyAny>>,
        element: Option<&Bound<'_, PyAny>>,
        force: Option<bool>,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        do_select_option(
            py,
            &self.lease,
            self.selector.clone(),
            value,
            index,
            label,
            element,
            force,
            timeout,
        )
    }

    #[pyo3(signature = (files, *, timeout=None))]
    fn set_input_files<'py>(
        &self,
        py: Python<'py>,
        files: &Bound<'_, PyAny>,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        do_set_input_files(py, &self.lease, self.selector.clone(), files, timeout)
    }

    /// Waits until visible.
    #[pyo3(signature = (*, state=None, timeout=None))]
    fn wait_for<'py>(
        &self,
        py: Python<'py>,
        state: Option<String>,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let sel = self.selector.clone();
        let state = wait_state(state)?;
        run_in(py, &self.lease, timeout, move |p| {
            Box::pin(async move { p.wait_for_selector_state(&sel, state).await.map(none) })
        })
    }

    #[pyo3(signature = (*, timeout=None))]
    fn text_content<'py>(&self, py: Python<'py>, timeout: Option<f64>) -> PyResult<Bound<'py, PyAny>> {
        let sel = self.selector.clone();
        run_in(py, &self.lease, timeout, move |p| {
            Box::pin(async move { p.text_content(&sel).await })
        })
    }

    #[pyo3(signature = (*, timeout=None))]
    fn inner_text<'py>(&self, py: Python<'py>, timeout: Option<f64>) -> PyResult<Bound<'py, PyAny>> {
        let sel = self.selector.clone();
        run_in(py, &self.lease, timeout, move |p| Box::pin(async move { p.inner_text(&sel).await }))
    }

    #[pyo3(signature = (*, timeout=None))]
    fn inner_html<'py>(&self, py: Python<'py>, timeout: Option<f64>) -> PyResult<Bound<'py, PyAny>> {
        let sel = self.selector.clone();
        run_in(py, &self.lease, timeout, move |p| Box::pin(async move { p.inner_html(&sel).await }))
    }

    #[pyo3(signature = (*, timeout=None))]
    fn input_value<'py>(&self, py: Python<'py>, timeout: Option<f64>) -> PyResult<Bound<'py, PyAny>> {
        let sel = self.selector.clone();
        run_in(py, &self.lease, timeout, move |p| Box::pin(async move { p.input_value(&sel).await }))
    }

    #[pyo3(signature = (name, *, timeout=None))]
    fn get_attribute<'py>(&self, py: Python<'py>, name: String, timeout: Option<f64>) -> PyResult<Bound<'py, PyAny>> {
        let sel = self.selector.clone();
        run_in(py, &self.lease, timeout, move |p| Box::pin(async move { p.get_attribute(&sel, &name).await }))
    }

    fn count<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let sel = self.selector.clone();
        run(py, &self.lease, move |p| Box::pin(async move { p.count(&sel).await }))
    }

    fn all<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let lease = self.lease.clone();
        let base = self.selector.clone();
        future_into_py(py, async move {
            let n = with_page(&lease, {
                let sel = base.clone();
                move |p| Box::pin(async move { p.count(&sel).await })
            })
            .await?;
            Ok((0..n)
                .map(|i| Locator {
                    lease: lease.clone(),
                    selector: base.then(&format!("nth={i}")),
                })
                .collect::<Vec<_>>())
        })
    }

    fn all_text_contents<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let sel = self.selector.clone();
        run(py, &self.lease, move |p| Box::pin(async move { p.all_text_contents(&sel).await }))
    }

    fn all_inner_texts<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let sel = self.selector.clone();
        run(py, &self.lease, move |p| Box::pin(async move { p.all_inner_texts(&sel).await }))
    }

    #[pyo3(signature = (*, timeout=None))]
    fn is_visible<'py>(&self, py: Python<'py>, timeout: Option<f64>) -> PyResult<Bound<'py, PyAny>> {
        let sel = self.selector.clone();
        run_in(py, &self.lease, timeout, move |p| Box::pin(async move { p.is_visible(&sel).await }))
    }

    #[pyo3(signature = (*, timeout=None))]
    fn is_hidden<'py>(&self, py: Python<'py>, timeout: Option<f64>) -> PyResult<Bound<'py, PyAny>> {
        let sel = self.selector.clone();
        run_in(py, &self.lease, timeout, move |p| Box::pin(async move { p.is_hidden(&sel).await }))
    }

    #[pyo3(signature = (*, timeout=None))]
    fn is_enabled<'py>(&self, py: Python<'py>, timeout: Option<f64>) -> PyResult<Bound<'py, PyAny>> {
        let sel = self.selector.clone();
        run_in(py, &self.lease, timeout, move |p| Box::pin(async move { p.is_enabled(&sel).await }))
    }

    #[pyo3(signature = (*, timeout=None))]
    fn is_disabled<'py>(&self, py: Python<'py>, timeout: Option<f64>) -> PyResult<Bound<'py, PyAny>> {
        let sel = self.selector.clone();
        run_in(py, &self.lease, timeout, move |p| Box::pin(async move { p.is_disabled(&sel).await }))
    }

    #[pyo3(signature = (*, timeout=None))]
    fn is_checked<'py>(&self, py: Python<'py>, timeout: Option<f64>) -> PyResult<Bound<'py, PyAny>> {
        let sel = self.selector.clone();
        run_in(py, &self.lease, timeout, move |p| Box::pin(async move { p.is_checked(&sel).await }))
    }

    #[pyo3(signature = (*, timeout=None))]
    fn is_editable<'py>(&self, py: Python<'py>, timeout: Option<f64>) -> PyResult<Bound<'py, PyAny>> {
        let sel = self.selector.clone();
        run_in(py, &self.lease, timeout, move |p| Box::pin(async move { p.is_editable(&sel).await }))
    }

    /// PNG or JPEG of the element, once it is visible and still; with `path`, also written
    /// there (its extension picks the type when `type` is not given).
    #[pyo3(signature = (*, path=None, r#type=None, quality=None, omit_background=None,
        annotate=None, mask=None, mask_color=None, animations=None, caret=None, timeout=None))]
    #[allow(clippy::too_many_arguments)]
    fn screenshot<'py>(
        &self,
        py: Python<'py>,
        path: Option<PathBuf>,
        r#type: Option<String>,
        quality: Option<u32>,
        omit_background: Option<bool>,
        annotate: Option<bool>,
        mask: Option<Bound<'py, PyAny>>,
        mask_color: Option<String>,
        animations: Option<String>,
        caret: Option<String>,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let timeout = call_timeout(timeout);
        let shot = shot_options(
            &self.lease,
            path.as_deref(),
            r#type.as_deref(),
            quality,
            omit_background,
            annotate,
            mask.as_ref(),
            mask_color,
            animations.as_deref(),
            caret.as_deref(),
        )?;
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
    #[pyo3(signature = (*, has=None, has_not=None, has_text=None, has_not_text=None))]
    fn filter(
        &self,
        has: Option<PyRef<'_, Locator>>,
        has_not: Option<PyRef<'_, Locator>>,
        has_text: Option<&Bound<'_, PyAny>>,
        has_not_text: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Locator> {
        Ok(Locator {
            lease: self.lease.clone(),
            selector: apply_filter(self.selector.clone(), has, has_not, has_text, has_not_text)?,
        })
    }

    fn and_(&self, locator: &Locator) -> PyResult<Locator> {
        if self.selector.frames != locator.selector.frames {
            return Err(PyValueError::new_err("locator combinator belongs to a different frame"));
        }
        Ok(Locator {
            lease: self.lease.clone(),
            selector: self.selector.and(&locator.selector),
        })
    }

    fn or_(&self, locator: &Locator) -> PyResult<Locator> {
        if self.selector.frames != locator.selector.frames {
            return Err(PyValueError::new_err("locator combinator belongs to a different frame"));
        }
        Ok(Locator {
            lease: self.lease.clone(),
            selector: self.selector.or(&locator.selector),
        })
    }

    /// Searches inside this locator's matches.
    #[pyo3(signature = (selector, *, has=None, has_not=None, has_text=None, has_not_text=None))]
    fn locator(
        &self,
        selector: String,
        has: Option<PyRef<'_, Locator>>,
        has_not: Option<PyRef<'_, Locator>>,
        has_text: Option<&Bound<'_, PyAny>>,
        has_not_text: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Locator> {
        Ok(Locator {
            lease: self.lease.clone(),
            selector: apply_filter(self.selector.then(&selector), has, has_not, has_text, has_not_text)?,
        })
    }

    #[pyo3(signature = (text, *, exact=None))]
    fn get_by_text(&self, text: &str, exact: Option<bool>) -> Locator {
        self.then(fluxwright::Selector::text(text, exact.unwrap_or(false)))
    }

    #[pyo3(signature = (role, *, checked=None, disabled=None, exact=None, expanded=None, include_hidden=None, level=None, name=None, pressed=None, selected=None))]
    #[allow(clippy::too_many_arguments)]
    fn get_by_role(
        &self,
        role: &str,
        checked: Option<bool>,
        disabled: Option<bool>,
        exact: Option<bool>,
        expanded: Option<bool>,
        include_hidden: Option<bool>,
        level: Option<u32>,
        name: Option<&Bound<'_, PyAny>>,
        pressed: Option<bool>,
        selected: Option<bool>,
    ) -> PyResult<Locator> {
        let opts = parse_role_options(checked, disabled, exact, expanded, include_hidden, level, name, pressed, selected)?;
        Ok(self.then(fluxwright::Selector::role_with(role, &opts)))
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

    #[pyo3(signature = (text, *, exact=None))]
    fn get_by_alt_text(&self, text: &str, exact: Option<bool>) -> Locator {
        self.then(fluxwright::Selector::alt_text(text, exact.unwrap_or(false)))
    }

    #[pyo3(signature = (text, *, exact=None))]
    fn get_by_title(&self, text: &str, exact: Option<bool>) -> Locator {
        self.then(fluxwright::Selector::title(text, exact.unwrap_or(false)))
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
    #[pyo3(signature = (selector, *, has=None, has_not=None, has_text=None, has_not_text=None))]
    fn locator(
        &self,
        selector: String,
        has: Option<PyRef<'_, Locator>>,
        has_not: Option<PyRef<'_, Locator>>,
        has_text: Option<&Bound<'_, PyAny>>,
        has_not_text: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Locator> {
        let loc = self.find(selector);
        Ok(Locator {
            lease: loc.lease,
            selector: apply_filter(loc.selector, has, has_not, has_text, has_not_text)?,
        })
    }

    #[pyo3(signature = (text, *, exact=None))]
    fn get_by_text(&self, text: &str, exact: Option<bool>) -> Locator {
        self.find(fluxwright::Selector::text(text, exact.unwrap_or(false)))
    }

    #[pyo3(signature = (role, *, checked=None, disabled=None, exact=None, expanded=None, include_hidden=None, level=None, name=None, pressed=None, selected=None))]
    #[allow(clippy::too_many_arguments)]
    fn get_by_role(
        &self,
        role: &str,
        checked: Option<bool>,
        disabled: Option<bool>,
        exact: Option<bool>,
        expanded: Option<bool>,
        include_hidden: Option<bool>,
        level: Option<u32>,
        name: Option<&Bound<'_, PyAny>>,
        pressed: Option<bool>,
        selected: Option<bool>,
    ) -> PyResult<Locator> {
        let opts = parse_role_options(checked, disabled, exact, expanded, include_hidden, level, name, pressed, selected)?;
        Ok(self.find(fluxwright::Selector::role_with(role, &opts)))
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

    #[pyo3(signature = (text, *, exact=None))]
    fn get_by_alt_text(&self, text: &str, exact: Option<bool>) -> Locator {
        self.find(fluxwright::Selector::alt_text(text, exact.unwrap_or(false)))
    }

    #[pyo3(signature = (text, *, exact=None))]
    fn get_by_title(&self, text: &str, exact: Option<bool>) -> Locator {
        self.find(fluxwright::Selector::title(text, exact.unwrap_or(false)))
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
                strict: true,
            },
        }
    }
}

/// A paused request; `fluxwright.async_api` wraps it in Playwright's `Route` and `Request`.
#[pyclass(frozen, module = "fluxwright._native")]
struct Route {
    request: Arc<Mutex<Option<fluxwright::InterceptedRequest>>>,
    fetch_handle: fluxwright::RouteFetchHandle,
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
        let fetch_handle = req.fetch_handle();
        Route {
            url: req.url.clone(),
            method: req.method.clone(),
            headers: req.headers.iter().cloned().collect(),
            post_data: req.post_data.clone(),
            resource_type: req.resource_type.clone(),
            fetch_handle,
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
    #[pyo3(signature = (*, url=None, method=None, headers=None, post_data=None, max_redirects=None, timeout=None))]
    fn fetch<'py>(
        &self,
        py: Python<'py>,
        url: Option<String>,
        method: Option<String>,
        headers: Option<HashMap<String, String>>,
        post_data: Option<Bound<'py, PyAny>>,
        max_redirects: Option<usize>,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let opts = fluxwright::RouteFetchOptions {
            url,
            method,
            headers: headers.map(|h| h.into_iter().collect()),
            post_data: post_data.map(|b| bytes(&b)).transpose()?,
            max_redirects,
            timeout: parse_api_timeout(timeout),
        };
        let slot = self.request.clone();
        let handle = self.fetch_handle.clone();
        future_into_py(py, async move {
            if slot.lock().await.is_none() {
                return Err(py_errors::Error::new_err("route is already handled"));
            }
            let resp = handle.fetch(&opts).await.map_err(error)?;
            Ok(ApiResponse::from_core(resp))
        })
    }

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

/// A file chooser waiting for files; `fluxwright.async_api` wraps it in `FileChooser`.
#[pyclass(frozen, module = "fluxwright._native")]
struct FileChooser {
    inner: fluxwright::FileChooser,
}

impl FileChooser {
    fn new(inner: fluxwright::FileChooser) -> Self {
        Self { inner }
    }
}

#[pymethods]
impl FileChooser {
    fn is_multiple(&self) -> bool {
        self.inner.is_multiple
    }

    #[pyo3(signature = (files, *, timeout=None))]
    fn set_files<'py>(
        &self,
        py: Python<'py>,
        files: &Bound<'_, PyAny>,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let fc = self.inner.clone();
        let input = parse_file_input(files)?;
        let timeout = call_timeout(timeout).unwrap_or(Duration::from_secs(30));
        future_into_py(py, async move {
            fc.set_files(&input, timeout).await.map(none).map_err(error)
        })
    }
}

/// A page's file choosers, in order.
#[pyclass(frozen, module = "fluxwright._native")]
struct FileChooserStream(Arc<Mutex<UnboundedReceiver<fluxwright::FileChooser>>>);

#[pymethods]
impl FileChooserStream {
    /// The next file chooser, or `None` once the page closed.
    fn next<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let rx = self.0.clone();
        future_into_py(py, async move {
            Ok(rx.lock().await.recv().await.map(FileChooser::new))
        })
    }
}

/// An exposed-binding invocation waiting for its Python callback's outcome.
#[pyclass(frozen, module = "fluxwright._native")]
struct BindingCall {
    inner: Arc<Mutex<Option<fluxwright::BindingCall>>>,
    name: String,
    args_json: String,
}

impl BindingCall {
    fn new(call: fluxwright::BindingCall) -> Self {
        let name = call.name.clone();
        let args_json = serde_json::to_string(&call.args).unwrap_or_else(|_| "[]".into());
        Self {
            inner: Arc::new(Mutex::new(Some(call))),
            name,
            args_json,
        }
    }
}

#[pymethods]
impl BindingCall {
    #[getter]
    fn name(&self) -> &str {
        &self.name
    }

    #[getter]
    fn args_json(&self) -> &str {
        &self.args_json
    }

    fn resolve<'py>(&self, py: Python<'py>, value_json: String) -> PyResult<Bound<'py, PyAny>> {
        let value: serde_json::Value = serde_json::from_str(&value_json)
            .map_err(|e| PyValueError::new_err(format!("resolve binding: invalid JSON: {e}")))?;
        let inner = self.inner.clone();
        future_into_py(py, async move {
            let Some(call) = inner.lock().await.take() else {
                return Ok(none(()));
            };
            call.resolve(&value).await.map(none).map_err(error)
        })
    }

    fn reject<'py>(&self, py: Python<'py>, message: String) -> PyResult<Bound<'py, PyAny>> {
        let inner = self.inner.clone();
        future_into_py(py, async move {
            let Some(call) = inner.lock().await.take() else {
                return Ok(none(()));
            };
            call.reject(&message).await.map(none).map_err(error)
        })
    }
}

/// Exposed-binding calls on this page, in order.
#[pyclass(frozen, module = "fluxwright._native")]
struct BindingStream(Arc<Mutex<UnboundedReceiver<fluxwright::BindingCall>>>);

#[pymethods]
impl BindingStream {
    /// The next binding call, or `None` once the page closed.
    fn next<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let rx = self.0.clone();
        future_into_py(py, async move {
            Ok(rx.lock().await.recv().await.map(BindingCall::new))
        })
    }
}

/// Popups opened from this page, in order.
#[pyclass(frozen, module = "fluxwright._native")]
struct PopupStream(Arc<Mutex<UnboundedReceiver<fluxwright::PageLease>>>);

#[pymethods]
impl PopupStream {
    /// The next popup lease, or `None` once the page closed.
    fn next<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let rx = self.0.clone();
        future_into_py(py, async move {
            Ok(rx
                .lock()
                .await
                .recv()
                .await
                .map(|lease| Lease(std::sync::Mutex::new(Some(lease)))))
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

#[pyclass(frozen, module = "fluxwright._native")]
struct CdpSessionEvent {
    #[pyo3(get)]
    method: String,
    #[pyo3(get)]
    params_json: String,
}

#[pyclass(frozen, module = "fluxwright._native")]
struct CdpEventStream(Arc<Mutex<UnboundedReceiver<fluxwright::CdpSessionEvent>>>);

#[pymethods]
impl CdpEventStream {
    fn next<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let rx = self.0.clone();
        future_into_py(py, async move {
            Ok(rx.lock().await.recv().await.map(|ev| CdpSessionEvent {
                method: ev.method,
                params_json: ev.params.to_string(),
            }))
        })
    }
}

#[pyclass(frozen, module = "fluxwright._native")]
struct CdpSession {
    inner: fluxwright::CdpSession,
    default_timeout: Duration,
}

#[pymethods]
impl CdpSession {
    #[pyo3(signature = (method, params=None))]
    fn send<'py>(
        &self,
        py: Python<'py>,
        method: String,
        params: Option<Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let params = match params {
            Some(p) => to_json(&p)?,
            None => serde_json::json!({}),
        };
        let session = self.inner.clone();
        let timeout = self.default_timeout;
        future_into_py(py, async move {
            session
                .send(&method, params, timeout)
                .await
                .map(Json)
                .map_err(error)
        })
    }

    #[pyo3(name = "_events")]
    fn events(&self) -> CdpEventStream {
        CdpEventStream(Arc::new(Mutex::new(self.inner.subscribe_events())))
    }

    fn detach<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let session = self.inner.clone();
        future_into_py(py, async move {
            session
                .detach(Duration::from_secs(5))
                .await
                .map(none)
                .map_err(error)
        })
    }
}

#[pyclass(frozen, module = "fluxwright._native")]
struct WebSocketEvent {
    #[pyo3(get)]
    event: String,
    #[pyo3(get)]
    request_id: String,
    #[pyo3(get)]
    url: Option<String>,
    #[pyo3(get)]
    payload_text: Option<String>,
    payload_bytes: Option<Vec<u8>>,
    #[pyo3(get)]
    error_message: Option<String>,
}

#[pymethods]
impl WebSocketEvent {
    #[getter]
    fn payload_bytes<'py>(&self, py: Python<'py>) -> Option<Py<PyBytes>> {
        self.payload_bytes
            .as_deref()
            .map(|b| PyBytes::new(py, b).unbind())
    }
}

impl From<fluxwright::WebSocketEvent> for WebSocketEvent {
    fn from(ev: fluxwright::WebSocketEvent) -> Self {
        match ev {
            fluxwright::WebSocketEvent::Created { request_id, url } => WebSocketEvent {
                event: "created".into(),
                request_id,
                url: Some(url),
                payload_text: None,
                payload_bytes: None,
                error_message: None,
            },
            fluxwright::WebSocketEvent::FrameSent {
                request_id,
                payload,
                is_text,
                ..
            } => WebSocketEvent {
                event: "framesent".into(),
                request_id,
                url: None,
                payload_text: is_text.then(|| String::from_utf8_lossy(&payload).into_owned()),
                payload_bytes: (!is_text).then_some(payload),
                error_message: None,
            },
            fluxwright::WebSocketEvent::FrameReceived {
                request_id,
                payload,
                is_text,
                ..
            } => WebSocketEvent {
                event: "framereceived".into(),
                request_id,
                url: None,
                payload_text: is_text.then(|| String::from_utf8_lossy(&payload).into_owned()),
                payload_bytes: (!is_text).then_some(payload),
                error_message: None,
            },
            fluxwright::WebSocketEvent::Error {
                request_id,
                error_message,
            } => WebSocketEvent {
                event: "socketerror".into(),
                request_id,
                url: None,
                payload_text: None,
                payload_bytes: None,
                error_message: Some(error_message),
            },
            fluxwright::WebSocketEvent::Closed { request_id } => WebSocketEvent {
                event: "close".into(),
                request_id,
                url: None,
                payload_text: None,
                payload_bytes: None,
                error_message: None,
            },
        }
    }
}

#[pyclass(frozen, module = "fluxwright._native")]
struct WebSocketStream(Arc<Mutex<UnboundedReceiver<fluxwright::WebSocketEvent>>>);

#[pymethods]
impl WebSocketStream {
    fn next<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let rx = self.0.clone();
        future_into_py(py, async move {
            Ok(rx.lock().await.recv().await.map(WebSocketEvent::from))
        })
    }
}

#[pyclass(frozen, module = "fluxwright._native")]
struct WorkerInfo {
    #[pyo3(get)]
    id: String,
    #[pyo3(get)]
    url: String,
}

#[pyclass(frozen, module = "fluxwright._native")]
struct WorkerEvent {
    #[pyo3(get)]
    event: String,
    #[pyo3(get)]
    id: String,
    #[pyo3(get)]
    url: Option<String>,
    message: Option<fluxwright::ConsoleMessage>,
}

#[pymethods]
impl WorkerEvent {
    #[getter]
    fn message(&self) -> Option<ConsoleMessage> {
        self.message.clone().map(ConsoleMessage::from)
    }
}

impl From<fluxwright::WorkerEvent> for WorkerEvent {
    fn from(ev: fluxwright::WorkerEvent) -> Self {
        match ev {
            fluxwright::WorkerEvent::Created(w) => WorkerEvent {
                event: "created".into(),
                id: w.target_id().to_owned(),
                url: Some(w.url().to_owned()),
                message: None,
            },
            fluxwright::WorkerEvent::Closed { target_id, url } => WorkerEvent {
                event: "close".into(),
                id: target_id,
                url: Some(url),
                message: None,
            },
            fluxwright::WorkerEvent::Console { target_id, message } => WorkerEvent {
                event: "console".into(),
                id: target_id,
                url: None,
                message: Some(message),
            },
        }
    }
}

#[pyclass(frozen, module = "fluxwright._native")]
struct WorkerStream(Arc<Mutex<UnboundedReceiver<fluxwright::WorkerEvent>>>);

#[pymethods]
impl WorkerStream {
    fn next<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let rx = self.0.clone();
        future_into_py(py, async move {
            Ok(rx.lock().await.recv().await.map(WorkerEvent::from))
        })
    }
}

#[pymodule]
fn _native(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<ApiResponse>()?;
    m.add_class::<Engine>()?;
    m.add_class::<Context>()?;
    m.add_class::<FrameInfo>()?;
    m.add_class::<CdpSessionEvent>()?;
    m.add_class::<CdpEventStream>()?;
    m.add_class::<CdpSession>()?;
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
    m.add_class::<FileChooser>()?;
    m.add_class::<FileChooserStream>()?;
    m.add_class::<BindingCall>()?;
    m.add_class::<BindingStream>()?;
    m.add_class::<PopupStream>()?;
    m.add_class::<WebSocketEvent>()?;
    m.add_class::<WebSocketStream>()?;
    m.add_class::<WorkerInfo>()?;
    m.add_class::<WorkerEvent>()?;
    m.add_class::<WorkerStream>()?;
    m.py().import("atexit")?.call_method1("register", (wrap_pyfunction!(wait_for_hand_overs, m)?,))?;
    Ok(())
}
