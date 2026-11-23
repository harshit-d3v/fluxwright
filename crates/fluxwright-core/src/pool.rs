use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use fluxwright_cdp::{
    http_client::HttpContextDefaults, normalize_headers, ApiRequestOptions, ApiResponse,
    AriaSnapshot, BoundingBox, CdpBrowser, CdpPage, CdpSession, ClockInstallOptions, ClockTicks,
    ClockTime, ConsoleMessage, ContextId, CookieFilter, Download, DownloadQueue, FrameInfo, Fulfill,
    HarNotFound, HarRecorder, HarRouteOptions, HarRouter, InterceptedRequest, LogEntry,
    MarkdownOptions, Overrides, PageError, PageLog, PdfOptions, Proxy, RecordHarOptions,
    ResourceType, RouteFetchHandle, RouteFetchOptions, RouteOptions, ScreenshotOptions, Selector,
    SnapshotOptions, WebSocketEvent, Worker, WorkerEvent,
};
use tokio::sync::{Mutex, Notify};
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::adaptive::{AdaptiveConfig, AdaptiveController};
use crate::config::{EngineConfig, JobOptions, Priority, QueueFullMode};
use crate::error::{Error, Result};
use crate::expect::resolve_url;
use crate::hosts::{HostLimits, HostSlot};
use crate::metrics::{Counters, MetricsSnapshot};
use crate::process::{current_process_rss_bytes, process_tree_memory_map};
use crate::profiles::{validate_profile_name, ProfileStore};
use crate::proxies::{ProxyLease, ProxyPool};
use crate::source::BrowserSource;
use crate::tracing_rec::{ContextTracing, Tracing};
use crate::video::{RecordVideoOptions, Video, VideoRecorder};

#[derive(Clone)]
pub struct Engine {
    inner: Arc<Inner>,
}

struct Inner {
    config: EngineConfig,
    source: Arc<dyn BrowserSource>,
    profiles: Option<Arc<ProfileStore>>,
    adaptive: Option<AdaptiveController>,
    state: Mutex<State>,
    admit: Notify,
    queue_space: Notify,
    /// When the fleet's memory was last walked (never: `None`), and each browser root's footprint.
    rss_cache: Mutex<(Option<Instant>, HashMap<u32, u64>)>,
    rss_refreshing: AtomicBool,
    grants: Arc<tokio::sync::Semaphore>,
    hosts: HostLimits,
}

struct State {
    browsers: Vec<Slot>,
    queue: VecDeque<Waiter>,
    mixed_priorities: bool,
    counters: Counters,
    launching: usize,
    shutting_down: bool,
}

impl State {
    fn push_waiter(&mut self, waiter: Waiter) {
        if waiter.priority != Priority::Normal {
            self.mixed_priorities = true;
        }
        self.queue.push_back(waiter);
    }

    /// Browsers that count toward `max_browsers`: those that can still take a lease. A draining
    /// or dead one is on its way out, so its replacement may start at once; counting it stopped
    /// the whole pool (with one browser) until its last job ended. The memory ceiling still
    /// gates every new launch.
    fn serving(&self) -> usize {
        self.browsers
            .iter()
            .filter(|s| !s.crashed && !s.draining && !s.browser.connection_dead())
            .count()
    }
}

fn remove_dir_if_exists(dir: &std::path::Path) {
    if matches!(dir.try_exists(), Ok(true)) {
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// An engine dropped without `shutdown()` kills its browsers: their own tasks keep them alive
/// until Chrome exits.
impl Drop for Inner {
    fn drop(&mut self) {
        if let Ok(st) = self.state.try_lock() {
            for s in &st.browsers {
                for w in &s.warm {
                    remove_dir_if_exists(&w.download_dir);
                }
                s.browser.start_kill();
            }
        }
    }
}

struct WarmContext {
    context: ContextId,
    download_dir: std::path::PathBuf,
    blank_page: CdpPage,
}

struct Slot {
    browser: Arc<CdpBrowser>,
    active: usize,
    warm: VecDeque<WarmContext>,
    warming: usize,
    jobs_served: u64,
    started: Instant,
    last_used: Instant,
    draining: bool,
    crashed: bool,
}

struct Waiter {
    priority: Priority,
    proxy: Option<Proxy>,
    eager_page: bool,
    queued_at: Instant,
    tx: tokio::sync::oneshot::Sender<Result<Arc<ContextGuard>>>,
}

struct ContextGuard {
    engine: Arc<Inner>,
    browser: Arc<CdpBrowser>,
    context: ContextId,
    download_dir: std::path::PathBuf,
    warm_page: std::sync::Mutex<Option<CdpPage>>,
    http_defaults: std::sync::Mutex<HttpContextDefaults>,
    har_recorders: std::sync::Mutex<Vec<HarRecorder>>,
    har_route_tasks: std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>,
    route_entries: Arc<std::sync::Mutex<Vec<Arc<RouteEntry>>>>,
    route_start: tokio::sync::Mutex<bool>,
    route_tasks: std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>,
    record_video: std::sync::Mutex<Option<RecordVideoOptions>>,
    video_recorders: std::sync::Mutex<Vec<VideoRecorder>>,
    proxy_lease: std::sync::Mutex<Option<ProxyLease>>,
    profile_target: std::sync::Mutex<Option<(Arc<ProfileStore>, String)>>,
    tracing: Arc<ContextTracing>,
    runtime: tokio::runtime::Handle,
    failed: AtomicBool,
    disposed: AtomicBool,
}

impl ContextGuard {
    fn is_disposed(&self) -> bool {
        self.disposed.load(Ordering::SeqCst)
    }

    fn release_proxy(&self, completed: bool) {
        if let Some(lease) = self.proxy_lease.lock().unwrap().take() {
            lease.report_outcome(completed);
        }
    }

    async fn invalidate_warm_page(&self) {
        let page = self.warm_page.lock().unwrap().take();
        if let Some(p) = page {
            let _ = self.browser.close_target(&p.target_id).await;
        }
    }

    async fn dispose(&self, completed: bool) {
        if self.disposed.swap(true, Ordering::SeqCst) {
            return;
        }
        self.tracing.shutdown();
        self.release_proxy(completed);
        let _ = self.warm_page.lock().unwrap().take();
        for handle in self.har_route_tasks.lock().unwrap().drain(..) {
            handle.abort();
        }
        for handle in self.route_tasks.lock().unwrap().drain(..) {
            handle.abort();
        }
        let recorders = std::mem::take(&mut *self.har_recorders.lock().unwrap());
        for rec in recorders {
            let _ = rec.finish().await;
        }
        let video_recorders = std::mem::take(&mut *self.video_recorders.lock().unwrap());
        for rec in video_recorders {
            let _ = rec.finish().await;
        }
        let _ = self.browser.dispose_context(&self.context).await;
        if matches!(self.download_dir.try_exists(), Ok(true)) {
            let dir = self.download_dir.clone();
            let _ = tokio::task::spawn_blocking(move || std::fs::remove_dir_all(dir)).await;
        }
        self.engine.release_slot(&self.browser, completed).await;
    }

    fn dispose_in_background(self: &Arc<Self>, completed: bool) {
        if self.disposed.swap(true, Ordering::SeqCst) {
            return;
        }
        self.tracing.shutdown();
        remove_dir_if_exists(&self.download_dir);
        self.release_proxy(completed);
        let _ = self.warm_page.lock().unwrap().take();
        for handle in self.har_route_tasks.lock().unwrap().drain(..) {
            handle.abort();
        }
        for handle in self.route_tasks.lock().unwrap().drain(..) {
            handle.abort();
        }
        let recorders = std::mem::take(&mut *self.har_recorders.lock().unwrap());
        let video_recorders = std::mem::take(&mut *self.video_recorders.lock().unwrap());
        let guard = Arc::clone(self);
        self.runtime.spawn(async move {
            for rec in recorders {
                let _ = rec.finish().await;
            }
            for rec in video_recorders {
                let _ = rec.finish().await;
            }
            let _ = guard.browser.dispose_context(&guard.context).await;
            guard.engine.release_slot(&guard.browser, completed).await;
        });
    }
}

impl Drop for ContextGuard {
    fn drop(&mut self) {
        if self.disposed.swap(true, Ordering::SeqCst) {
            return;
        }
        self.tracing.shutdown();
        remove_dir_if_exists(&self.download_dir);
        let completed = !self.failed.load(Ordering::SeqCst);
        self.release_proxy(completed);
        let _ = self.warm_page.lock().unwrap().take();
        for handle in self.har_route_tasks.lock().unwrap().drain(..) {
            handle.abort();
        }
        for handle in self.route_tasks.lock().unwrap().drain(..) {
            handle.abort();
        }
        let recorders = std::mem::take(&mut *self.har_recorders.lock().unwrap());
        let video_recorders = std::mem::take(&mut *self.video_recorders.lock().unwrap());
        let browser = self.browser.clone();
        let ctx = self.context.clone();
        let engine = self.engine.clone();
        self.runtime.spawn(async move {
            for rec in recorders {
                let _ = rec.finish().await;
            }
            for rec in video_recorders {
                let _ = rec.finish().await;
            }
            let _ = browser.dispose_context(&ctx).await;
            engine.release_slot(&browser, completed).await;
        });
    }
}

/// HTTP client sharing cookies and defaults (`baseURL`, `extraHTTPHeaders`, `httpCredentials`,
/// `proxy`) with its browser context.
#[derive(Clone)]
pub struct ApiRequestContext {
    guard: Arc<ContextGuard>,
    default_timeout: Duration,
}

impl ApiRequestContext {
    pub async fn fetch(&self, url: &str, opts: &ApiRequestOptions) -> Result<ApiResponse> {
        if self.guard.is_disposed() {
            return Err(Error::Other("APIRequestContext is disposed".into()));
        }
        let defaults = self.guard.http_defaults.lock().unwrap().clone();
        let mut effective = opts.clone();
        if effective.timeout.is_none() {
            effective.timeout = Some(self.default_timeout);
        }
        Ok(self
            .guard
            .browser
            .context_api_fetch(&self.guard.context, &defaults, url, &effective)
            .await?)
    }

    async fn with_method(
        &self,
        method: &str,
        url: &str,
        opts: &ApiRequestOptions,
    ) -> Result<ApiResponse> {
        let mut o = opts.clone();
        o.method = Some(method.to_string());
        self.fetch(url, &o).await
    }

    pub async fn get(&self, url: &str, opts: &ApiRequestOptions) -> Result<ApiResponse> {
        self.with_method("GET", url, opts).await
    }

    pub async fn post(&self, url: &str, opts: &ApiRequestOptions) -> Result<ApiResponse> {
        self.with_method("POST", url, opts).await
    }

    pub async fn put(&self, url: &str, opts: &ApiRequestOptions) -> Result<ApiResponse> {
        self.with_method("PUT", url, opts).await
    }

    pub async fn patch(&self, url: &str, opts: &ApiRequestOptions) -> Result<ApiResponse> {
        self.with_method("PATCH", url, opts).await
    }

    pub async fn delete(&self, url: &str, opts: &ApiRequestOptions) -> Result<ApiResponse> {
        self.with_method("DELETE", url, opts).await
    }

    pub async fn head(&self, url: &str, opts: &ApiRequestOptions) -> Result<ApiResponse> {
        self.with_method("HEAD", url, opts).await
    }

    pub async fn storage_state(&self) -> Result<fluxwright_cdp::StorageState> {
        if self.guard.is_disposed() {
            return Err(Error::Other("APIRequestContext is disposed".into()));
        }
        let cookies = self
            .guard
            .browser
            .context_cookies(&self.guard.context, self.default_timeout)
            .await?;
        Ok(fluxwright_cdp::StorageState {
            cookies,
            origins: Vec::new(),
        })
    }
}

/// An isolated browser context leased from the pool; may hold multiple pages and popups.
pub struct ContextLease {
    guard: Arc<ContextGuard>,
    base_url: Option<String>,
    action_timeout: Option<Duration>,
    navigation_timeout: Option<Duration>,
    binding_listeners: Listeners<fluxwright_cdp::BindingCall>,
    context_bindings_installed: Arc<tokio::sync::OnceCell<()>>,
    binding_handlers: Arc<std::sync::Mutex<HashMap<String, BindingHandler>>>,
    binding_dispatch_started: AtomicBool,
}

pub struct PageLease {
    engine: Arc<Inner>,
    context: Arc<ContextGuard>,
    owns_context: bool,
    page: CdpPage,
    lease_id: Uuid,
    released: bool,
    /// Where `Drop` runs its cleanup: a lease can be dropped outside any runtime (Node's garbage
    /// collector does this), and `tokio::spawn` panics there.
    runtime: tokio::runtime::Handle,
    log: Arc<std::sync::Mutex<PageLog>>,
    log_task: tokio::task::JoinHandle<()>,
    popup_task: tokio::task::JoinHandle<()>,
    downloads: Downloads,
    /// Who answers this page's dialogs; with nobody, a dialog is dismissed as before.
    dialog_listeners: Listeners<fluxwright_cdp::Dialog>,
    /// Who receives intercepted file choosers on this page.
    file_chooser_listeners: Listeners<fluxwright_cdp::FileChooser>,
    /// Invocations of exposed JS->host bindings on this page.
    binding_listeners: Listeners<fluxwright_cdp::BindingCall>,
    binding_handlers: Arc<std::sync::Mutex<HashMap<String, BindingHandler>>>,
    binding_dispatch_started: AtomicBool,
    /// Popups opened from this page (`window.open` / `target=_blank`).
    popup_listeners: Listeners<PageLease>,
    /// `set_default_timeout` and `set_default_navigation_timeout`, when called.
    default_timeouts: std::sync::Mutex<(Option<Duration>, Option<Duration>)>,
    /// This page's place on the host it navigated to, when hosts are limited.
    host_slot: HostSlot,
    /// Base URL for relative `goto` calls.
    base_url: std::sync::Mutex<Option<String>>,
    har_recorders: std::sync::Mutex<Vec<HarRecorder>>,
    har_route_tasks: std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>,
    route_entries: Arc<std::sync::Mutex<Vec<Arc<RouteEntry>>>>,
    route_start: tokio::sync::Mutex<bool>,
    route_tasks: std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>,
    video_recorder: std::sync::Mutex<Option<VideoRecorder>>,
    video: Option<Video>,
}

tokio::task_local! {
    /// The timeout of the call in progress: Playwright's per-call `timeout` option.
    static CALL_TIMEOUT: Duration;
}

/// Runs `call` (one or more page calls) with `timeout` in place of the page's and the engine's
/// timeouts, as Playwright's per-call `timeout` option does. `None` changes nothing.
pub async fn with_timeout<F: std::future::Future>(timeout: Option<Duration>, call: F) -> F::Output {
    match timeout {
        Some(t) => CALL_TIMEOUT.scope(t, call).await,
        None => call.await,
    }
}

type Listeners<T> = Arc<std::sync::Mutex<Vec<tokio::sync::mpsc::UnboundedSender<T>>>>;
type BindingHandler = Arc<
    dyn Fn(Vec<serde_json::Value>) -> std::pin::Pin<Box<dyn std::future::Future<Output = std::result::Result<serde_json::Value, String>> + Send>>
        + Send
        + Sync,
>;

fn deliver_newest<T>(item: T, listeners: &Listeners<T>) {
    let mut item = Some(item);
    let mut listeners = listeners.lock().unwrap();
    while let (Some(tx), Some(v)) = (listeners.last(), item.take()) {
        match tx.send(v) {
            Ok(()) => break,
            Err(back) => {
                item = Some(back.0);
                listeners.pop();
            }
        }
    }
}

/// Routes events (dialogs, file choosers) to the newest listener still subscribed; an
/// unhandled event is dropped cleanly. Ends with the context.
fn route_newest<T: Send + 'static>(
    mut rx: tokio::sync::mpsc::UnboundedReceiver<T>,
    listeners: Listeners<T>,
) {
    tokio::spawn(async move {
        while let Some(item) = rx.recv().await {
            deliver_newest(item, &listeners);
        }
    });
}

fn spawn_page_event_router(
    mut dialogs_rx: tokio::sync::mpsc::UnboundedReceiver<fluxwright_cdp::Dialog>,
    dialog_listeners: Listeners<fluxwright_cdp::Dialog>,
    mut file_choosers_rx: tokio::sync::mpsc::UnboundedReceiver<fluxwright_cdp::FileChooser>,
    file_chooser_listeners: Listeners<fluxwright_cdp::FileChooser>,
    mut bindings_rx: tokio::sync::mpsc::UnboundedReceiver<fluxwright_cdp::BindingCall>,
    binding_listeners: Listeners<fluxwright_cdp::BindingCall>,
) {
    tokio::spawn(async move {
        let mut d_open = true;
        let mut f_open = true;
        let mut b_open = true;
        while d_open || f_open || b_open {
            tokio::select! {
                res = dialogs_rx.recv(), if d_open => match res {
                    Some(item) => deliver_newest(item, &dialog_listeners),
                    None => d_open = false,
                },
                res = file_choosers_rx.recv(), if f_open => match res {
                    Some(item) => deliver_newest(item, &file_chooser_listeners),
                    None => f_open = false,
                },
                res = bindings_rx.recv(), if b_open => match res {
                    Some(item) => deliver_newest(item, &binding_listeners),
                    None => b_open = false,
                },
            }
        }
    });
}

impl Engine {
    pub fn new(config: EngineConfig, source: Arc<dyn BrowserSource>) -> Self {
        let profiles = config
            .profiles_dir
            .as_ref()
            .map(|d| Arc::new(ProfileStore::new(d)));
        let hard_max = config
            .max_browsers
            .saturating_mul(config.max_contexts_per_browser)
            .max(1);
        let adaptive = config
            .adaptive_concurrency
            .clone()
            .map(|cfg| AdaptiveController::new(cfg, hard_max));
        let inner = Arc::new(Inner {
            hosts: HostLimits::new(config.max_concurrency_per_host, config.min_delay_per_host),
            config,
            source,
            profiles,
            adaptive,
            state: Mutex::new(State {
                browsers: Vec::new(),
                queue: VecDeque::new(),
                mixed_priorities: false,
                counters: Counters::default(),
                launching: 0,
                shutting_down: false,
            }),
            admit: Notify::new(),
            queue_space: Notify::new(),
            rss_cache: Mutex::new((None, HashMap::new())),
            rss_refreshing: AtomicBool::new(false),
            grants: Arc::new(tokio::sync::Semaphore::new(hard_max.clamp(24, 128))),
        });
        // Weak: an engine dropped without shutdown() must free its browsers (see Inner's Drop),
        // and this loop holding it would keep it alive forever.
        let pump = Arc::downgrade(&inner);
        tokio::spawn(async move {
            loop {
                let Some(inner) = pump.upgrade() else {
                    break;
                };
                if inner.state.lock().await.shutting_down {
                    break;
                }
                let notified = inner.admit.notified();
                tokio::pin!(notified);
                // notify_waiters() stores no permit, and Notified only listens
                // once polled: without enable() every wakeup raised while
                // dispatch runs is lost and the queue stalls until timeout.
                notified.as_mut().enable();
                let progress = Inner::dispatch(inner.clone()).await.unwrap_or(false);
                if !progress {
                    // Also wakes regularly without traffic: idle browsers close on time,
                    // RSS ceiling re-checks after cache expiry, and a dropped engine is noticed.
                    let _ = tokio::time::timeout(Duration::from_millis(500), notified).await;
                }
            }
        });
        Self { inner }
    }

    pub fn builder() -> EngineBuilder {
        EngineBuilder {
            config: EngineConfig::default(),
            source: Arc::new(crate::source::LocalChromium),
        }
    }

    /// Connects to a remote browser over CDP rather than launching local Chrome.
    pub async fn connect_over_cdp(endpoint_url: impl Into<String>) -> Result<Self> {
        Self::builder()
            .connect_over_cdp(endpoint_url)
            .build()
            .await
    }

    pub async fn acquire(&self) -> Result<PageLease> {
        self.acquire_with(Priority::Normal, self.inner.config.acquire_timeout)
            .await
    }

    pub async fn acquire_with(&self, priority: Priority, timeout: Duration) -> Result<PageLease> {
        let guard = self.acquire_context_guard(priority, None, timeout, true).await?;
        let ctx = ContextLease {
            guard,
            base_url: None,
            action_timeout: None,
            navigation_timeout: None,
            binding_listeners: Listeners::default(),
            context_bindings_installed: Arc::default(),
            binding_handlers: Default::default(),
            binding_dispatch_started: AtomicBool::new(false),
        };
        let mut page = match ctx.new_page().await {
            Ok(p) => p,
            Err(e) => {
                ctx.dispose_failed().await;
                return Err(e);
            }
        };
        page.owns_context = true;
        Ok(page)
    }

    pub async fn acquire_context(&self) -> Result<ContextLease> {
        self.acquire_context_with(&JobOptions::default()).await
    }

    /// Named profile store when `EngineConfig::profiles_dir` is configured.
    pub fn profiles(&self) -> Option<&ProfileStore> {
        self.inner.profiles.as_deref()
    }

    /// Configured outbound proxy pool, if any.
    pub fn proxy_pool(&self) -> Option<&ProxyPool> {
        self.inner.config.proxy_pool.as_ref()
    }

    /// Acquires an isolated browser context configured with `opts`; open tabs via [`ContextLease::new_page`].
    pub async fn acquire_context_with(&self, opts: &JobOptions) -> Result<ContextLease> {
        self.acquire_context_inner(opts, false).await
    }

    async fn acquire_context_inner(&self, opts: &JobOptions, eager_page: bool) -> Result<ContextLease> {
        let loaded_profile_state = if let Some(ref profile_name) = opts.profile {
            validate_profile_name(profile_name)?;
            let Some(store) = self.inner.profiles.as_ref() else {
                return Err(Error::Other("profiles_dir is not configured".into()));
            };
            if opts.storage_state.is_none() {
                let store = Arc::clone(store);
                let name = profile_name.clone();
                tokio::task::spawn_blocking(move || store.load(&name))
                    .await
                    .map_err(|e| Error::Other(e.to_string()))??
            } else {
                None
            }
        } else {
            None
        };

        let (effective_proxy, proxy_lease) = match (&opts.proxy, &self.inner.config.proxy_pool) {
            (Some(p), _) => (Some(p.clone()), None),
            (None, Some(pool)) => {
                let lease = pool.acquire(opts.proxy_session.as_deref());
                let proxy = lease.proxy().clone();
                (Some(proxy), Some(lease))
            }
            (None, None) => (None, None),
        };

        if let Some(ref rv) = opts.record_video {
            rv.validate()?;
            let dir = rv.dir.clone();
            tokio::task::spawn_blocking(move || std::fs::create_dir_all(&dir))
                .await
                .map_err(|e| Error::Other(e.to_string()))?
                .map_err(|e| Error::Other(format!("record_video create_dir_all: {e}")))?;
        }
        let timeout_budget = opts.timeout.unwrap_or(self.inner.config.acquire_timeout);
        let guard = self
            .acquire_context_guard(
                opts.priority,
                effective_proxy.clone(),
                timeout_budget,
                eager_page,
            )
            .await?;
        if proxy_lease.is_some() {
            *guard.proxy_lease.lock().unwrap() = proxy_lease;
        }
        if let (Some(profile_name), Some(store)) = (&opts.profile, &self.inner.profiles) {
            if opts.persist_profile {
                *guard.profile_target.lock().unwrap() =
                    Some((Arc::clone(store), profile_name.clone()));
            }
        }
        let effective_storage_state = opts
            .storage_state
            .as_ref()
            .or(loaded_profile_state.as_ref());
        if effective_storage_state.is_some()
            || opts.record_har.is_some()
            || opts.record_video.is_some()
            || opts.emulation.javascript_enabled == Some(false)
            || opts.emulation.bypass_csp
        {
            guard.invalidate_warm_page().await;
        }
        if let Some(ref rv) = opts.record_video {
            *guard.record_video.lock().unwrap() = Some(rv.clone());
        }
        if opts.base_url.is_some()
            || !opts.emulation.extra_http_headers.is_empty()
            || opts.emulation.user_agent.is_some()
            || opts.http_credentials.is_some()
            || effective_proxy.is_some()
        {
            *guard.http_defaults.lock().unwrap() = HttpContextDefaults {
                base_url: opts.base_url.clone(),
                extra_http_headers: opts.emulation.extra_http_headers.clone(),
                user_agent: opts.emulation.user_agent.clone(),
                http_credentials: opts.http_credentials.clone(),
                proxy: effective_proxy,
            };
        }
        let has_blocking = opts.block_images
            || opts.block_fonts
            || opts.block_media
            || opts.block_stylesheets
            || opts.block_scripts
            || !opts.block_url_patterns.is_empty();
        let has_setup = has_blocking
            || opts.http_credentials.is_some()
            || !opts.permissions.is_empty()
            || effective_storage_state.is_some()
            || !opts.emulation.is_empty();
        if has_setup {
            let setup_res = async {
                if has_blocking {
                    apply_context_blocking(&guard, opts).await?;
                }
                let timeout = Duration::from_secs(5);
                if let Some(creds) = &opts.http_credentials {
                    guard
                        .browser
                        .context_set_http_credentials(&guard.context, creds, timeout)
                        .await?;
                }
                if !opts.permissions.is_empty() {
                    guard
                        .browser
                        .context_grant_permissions(&guard.context, &opts.permissions, timeout)
                        .await?;
                }
                if let Some(state) = effective_storage_state {
                    guard
                        .browser
                        .context_set_storage_state(&guard.context, state, timeout)
                        .await?;
                }
                if !opts.emulation.is_empty() {
                    guard
                        .browser
                        .context_emulate(&guard.context, &opts.emulation, timeout)
                        .await?;
                }
                Ok::<_, Error>(())
            }
            .await;
            if let Err(e) = setup_res {
                guard.dispose(false).await;
                return Err(e);
            }
        }
        if let Some(ref har_opts) = opts.record_har {
            let rec = guard
                .browser
                .start_har_recording(&guard.context, None, har_opts.clone())
                .await;
            guard.har_recorders.lock().unwrap().push(rec);
        }
        Ok(ContextLease {
            guard,
            base_url: opts.base_url.clone(),
            action_timeout: None,
            navigation_timeout: None,
            binding_listeners: Listeners::default(),
            context_bindings_installed: Arc::default(),
            binding_handlers: Default::default(),
            binding_dispatch_started: AtomicBool::new(false),
        })
    }

    /// A lease set up for `opts`: priority, proxy, resource blocking, permissions, saved
    /// storage and emulation.
    pub async fn acquire_job(&self, opts: &JobOptions) -> Result<PageLease> {
        let eager_page = opts.profile.is_none()
            && opts.storage_state.is_none()
            && opts.record_har.is_none()
            && opts.record_video.is_none()
            && opts.emulation.javascript_enabled != Some(false)
            && !opts.emulation.bypass_csp;
        let ctx = self.acquire_context_inner(opts, eager_page).await?;
        let mut page = match ctx.new_page().await {
            Ok(p) => p,
            Err(e) => {
                ctx.dispose_failed().await;
                return Err(e);
            }
        };
        page.owns_context = true;
        Ok(page)
    }

    async fn acquire_context_guard(
        &self,
        priority: Priority,
        proxy: Option<Proxy>,
        timeout: Duration,
        eager_page: bool,
    ) -> Result<Arc<ContextGuard>> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        {
            let mut st = self.inner.state.lock().await;
            if st.shutting_down {
                return Err(Error::ShuttingDown);
            }
            if st.queue.len() >= self.inner.config.queue_capacity {
                let dropped = drop_abandoned(&mut st.queue);
                if st.queue.is_empty() {
                    st.mixed_priorities = false;
                }
                notify_n(&self.inner.queue_space, dropped);
            }
            if st.queue.len() >= self.inner.config.queue_capacity {
                match self.inner.config.queue_full_mode {
                    QueueFullMode::Error => return Err(Error::QueueFull),
                    QueueFullMode::Wait => {
                        drop(st);
                        let start = Instant::now();
                        loop {
                            if start.elapsed() > timeout {
                                return Err(Error::AcquireTimeout);
                            }
                            let notified = self.inner.queue_space.notified();
                            tokio::pin!(notified);
                            notified.as_mut().enable();
                            {
                                // Checked and joined under one lock: every waiter woken at once
                                // used to see the same free place and join, past the capacity.
                                let mut st = self.inner.state.lock().await;
                                if st.shutting_down {
                                    return Err(Error::ShuttingDown);
                                }
                                let dropped = drop_abandoned(&mut st.queue);
                                if st.queue.is_empty() {
                                    st.mixed_priorities = false;
                                }
                                notify_n(&self.inner.queue_space, dropped);
                                if st.queue.len() < self.inner.config.queue_capacity {
                                    st.push_waiter(Waiter {
                                        priority,
                                        proxy,
                                        eager_page,
                                        queued_at: Instant::now(),
                                        tx,
                                    });
                                    break;
                                }
                            }
                            tokio::time::timeout(
                                timeout.saturating_sub(start.elapsed()),
                                notified,
                            )
                            .await
                            .map_err(|_| Error::AcquireTimeout)?;
                        }
                    }
                }
            } else {
                st.push_waiter(Waiter {
                    priority,
                    proxy,
                    eager_page,
                    queued_at: Instant::now(),
                    tx,
                });
            }
        }
        self.pump();
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(res)) => res,
            Ok(Err(_)) => Err(Error::AcquireTimeout),
            Err(_) => Err(Error::AcquireTimeout),
        }
    }

    fn pump(&self) {
        self.inner.admit.notify_one();
    }

    pub async fn run<F, Fut, T>(&self, opts: JobOptions, f: F) -> Result<T>
    where
        F: Fn(PageLease) -> Fut + Clone,
        Fut: std::future::Future<Output = Result<T>>,
        T: Send,
    {
        let timeout = opts.timeout.unwrap_or(self.inner.config.job_timeout);
        let retries = opts.retries;
        let mut attempt = 0;
        loop {
            let result = tokio::time::timeout(timeout, async {
                let page = self.acquire_job(&opts).await?;
                f.clone()(page).await
            })
            .await
            .unwrap_or(Err(Error::JobTimeout));
            match result {
                Ok(v) => return Ok(v),
                // Only what is_retryable allows (a browser that died). A job that timed out is
                // not run again: it may already have clicked or submitted something.
                Err(e) if e.is_retryable() && attempt < retries => {
                    warn!(error = %e, attempt, "retryable job failure");
                    attempt += 1;
                }
                Err(e) => return Err(e),
            }
        }
    }

    pub async fn metrics(&self) -> MetricsSnapshot {
        let (browsers, contexts, queued, warm_contexts, reuse, warm_hits, crash, recycle, pids) = {
            let st = self.inner.state.lock().await;
            let pids: Vec<u32> = st
                .browsers
                .iter()
                .filter(|s| !s.crashed && s.browser.pid > 0)
                .map(|s| s.browser.pid)
                .collect();
            (
                st.browsers.iter().filter(|s| !s.crashed).count(),
                st.browsers.iter().map(|s| s.active).sum::<usize>(),
                st.queue.len(),
                st.browsers.iter().map(|s| s.warm.len()).sum::<usize>(),
                st.counters.reuse_rate(),
                st.counters.warm_context_hits,
                st.counters.crash_count,
                st.counters.recycle_count,
                pids,
            )
        };
        let tree = self.inner.recent_tree_rss(pids).await;
        let engine_rss = tokio::task::spawn_blocking(current_process_rss_bytes)
            .await
            .unwrap_or(0);
        let leases = contexts;
        let hard_max = self
            .inner
            .config
            .max_browsers
            .saturating_mul(self.inner.config.max_contexts_per_browser);
        let effective_concurrency_limit = self
            .inner
            .adaptive
            .as_ref()
            .map_or(hard_max, |a| a.limit());
        MetricsSnapshot {
            browsers,
            contexts,
            pages: contexts,
            active_leases: leases,
            queued_requests: queued,
            warm_contexts,
            warm_context_hits: warm_hits,
            browser_reuse_rate: reuse,
            crash_count: crash,
            recycle_count: recycle,
            engine_rss_bytes: engine_rss,
            browser_tree_rss_bytes: tree,
            memory_per_job_bytes_estimate: if leases == 0 { 0 } else { tree / leases as u64 },
            effective_concurrency_limit,
        }
    }

    pub async fn browsers(&self) -> Vec<BrowserInfo> {
        let (rows, pids) = {
            let st = self.inner.state.lock().await;
            let rows: Vec<_> = st
                .browsers
                .iter()
                .map(|s| {
                    (
                        s.browser.id.0.to_string(),
                        s.browser.pid,
                        s.started.elapsed().as_secs(),
                        s.jobs_served,
                        s.active,
                        s.draining,
                        !s.crashed && !s.browser.connection_dead(),
                    )
                })
                .collect();
            let pids: Vec<u32> = rows.iter().filter(|r| r.1 > 0).map(|r| r.1).collect();
            (rows, pids)
        };
        let rss = if pids.is_empty() {
            HashMap::new()
        } else {
            tokio::task::spawn_blocking(move || process_tree_memory_map(&pids))
                .await
                .unwrap_or_default()
        };
        rows.into_iter()
            .map(|r| BrowserInfo {
                id: r.0,
                pid: r.1,
                age_secs: r.2,
                jobs_served: r.3,
                active_leases: r.4,
                rss_bytes: rss.get(&r.1).copied().unwrap_or(0),
                draining: r.5,
                healthy: r.6,
            })
            .collect()
    }

    pub async fn shutdown(&self, grace: Duration) -> Result<()> {
        let start = Instant::now();
        let browsers = loop {
            let browsers = {
                let mut st = self.inner.state.lock().await;
                st.shutting_down = true;
                while let Some(w) = st.queue.pop_front() {
                    let _ = w.tx.send(Err(Error::ShuttingDown));
                }
                // A browser still starting counts: it would otherwise come up after this returns.
                let idle = st.launching == 0 && st.browsers.iter().all(|s| s.active == 0);
                if idle || start.elapsed() > grace {
                    Some(std::mem::take(&mut st.browsers))
                } else {
                    None
                }
            };
            self.inner.admit.notify_one();
            self.inner.queue_space.notify_waiters();
            if let Some(browsers) = browsers {
                break browsers;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        let mut closing = tokio::task::JoinSet::new();
        for mut s in browsers {
            let warm: Vec<WarmContext> = s.warm.drain(..).collect();
            closing.spawn(async move {
                for w in warm {
                    remove_dir_if_exists(&w.download_dir);
                    let _ = s.browser.dispose_context(&w.context).await;
                }
                let _ = s.browser.close().await;
            });
        }
        while closing.join_next().await.is_some() {}
        Ok(())
    }

    pub fn config(&self) -> &EngineConfig {
        &self.inner.config
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct BrowserInfo {
    pub id: String,
    pub pid: u32,
    pub age_secs: u64,
    pub jobs_served: u64,
    pub active_leases: usize,
    pub rss_bytes: u64,
    pub draining: bool,
    pub healthy: bool,
}

enum Reservation {
    Warm(Arc<CdpBrowser>, WarmContext),
    Reuse(Arc<CdpBrowser>),
    Launch,
}

impl State {
    fn next_reusable_browser(&self, max_contexts: usize) -> Option<usize> {
        self.browsers
            .iter()
            .enumerate()
            .filter(|(_, s)| {
                !s.crashed
                    && !s.draining
                    && !s.browser.connection_dead()
                    && s.active < max_contexts
            })
            .min_by_key(|(_, s)| s.active)
            .map(|(i, _)| i)
    }

    fn next_warm_browser(&self, max_contexts: usize) -> Option<usize> {
        self.browsers
            .iter()
            .enumerate()
            .filter(|(_, s)| {
                !s.crashed
                    && !s.draining
                    && !s.browser.connection_dead()
                    && s.active < max_contexts
                    && !s.warm.is_empty()
            })
            .min_by_key(|(_, s)| s.active)
            .map(|(i, _)| i)
    }

    fn can_launch_for_queue(&self, config: &EngineConfig) -> bool {
        // Each in-flight launch will take 1 queued waiter immediately and open
        // max_contexts_per_browser - 1 remaining slots for additional waiters.
        let pending_slots =
            self.launching * config.max_contexts_per_browser.saturating_sub(1);
        self.serving() + self.launching < config.max_browsers
            && self.queue.len() > pending_slots
    }

    fn can_dispatch(&self, config: &EngineConfig) -> bool {
        !self.queue.is_empty()
            && (self.next_reusable_browser(config.max_contexts_per_browser).is_some()
                || self.can_launch_for_queue(config))
    }
}

impl Inner {
    async fn dispatch(self: Arc<Self>) -> Result<bool> {
        self.reap().await;
        self.recycle().await;
        let (can_dispatch_now, mut abandoned_total) = {
            let mut st = self.state.lock().await;
            if st.shutting_down {
                return Ok(false);
            }
            let dropped = if st.queue.is_empty() {
                0
            } else {
                let d = drop_abandoned(&mut st.queue);
                if st.queue.is_empty() {
                    st.mixed_priorities = false;
                }
                d
            };
            notify_n(&self.queue_space, dropped);
            (st.can_dispatch(&self.config), 0usize)
        };
        if !can_dispatch_now {
            self.maybe_prewarm().await;
            return Ok(false);
        }
        // Over the memory ceiling no new browser starts; one already running still takes work.
        let ceiling = self.config.memory_ceiling_mb * 1024 * 1024;
        let tree = if ceiling > 0 || self.adaptive.is_some() {
            self.browser_tree_rss().await
        } else {
            0
        };
        if let Some(ref adaptive) = self.adaptive {
            adaptive.observe_rss(tree, ceiling);
        }
        let over_ceiling = ceiling > 0 && {
            if tree > ceiling {
                debug!(tree, ceiling, "rss ceiling hit: no new launches");
            }
            tree > ceiling
        };
        let mut excess_warm: Vec<(Arc<CdpBrowser>, WarmContext)> = Vec::new();
        let mut batch: Vec<(Waiter, Reservation, tokio::sync::OwnedSemaphorePermit)> = Vec::new();
        {
            let mut st = self.state.lock().await;
            if st.shutting_down {
                return Ok(false);
            }
            loop {
                if let Some(ref adaptive) = self.adaptive {
                    let in_flight: usize =
                        st.browsers.iter().map(|s| s.active).sum::<usize>() + st.launching;
                    if in_flight >= adaptive.limit() {
                        break;
                    }
                }
                let idx = st.next_reusable_browser(self.config.max_contexts_per_browser);
                if idx.is_none() && (over_ceiling || !st.can_launch_for_queue(&self.config)) {
                    break;
                }
                if st.queue.is_empty() {
                    break;
                }
                let Ok(permit) = self.grants.clone().try_acquire_owned() else {
                    break;
                };
                let state = &mut *st;
                let (waiter, picked_abandoned) =
                    pick_waiter(&mut state.queue, &mut state.mixed_priorities);
                abandoned_total += picked_abandoned;
                let Some(waiter) = waiter else {
                    break;
                };
                let reservation = if waiter.proxy.is_none() {
                    if let Some(wi) = st.next_warm_browser(self.config.max_contexts_per_browser) {
                        let warm = st.browsers[wi].warm.pop_front().unwrap();
                        st.browsers[wi].active += 1;
                        st.counters.acquires += 1;
                        st.counters.acquires_reused_browser += 1;
                        st.counters.warm_context_hits += 1;
                        Reservation::Warm(st.browsers[wi].browser.clone(), warm)
                    } else if let Some(i) = idx {
                        st.browsers[i].active += 1;
                        st.counters.acquires += 1;
                        st.counters.acquires_reused_browser += 1;
                        Reservation::Reuse(st.browsers[i].browser.clone())
                    } else {
                        st.launching += 1;
                        Reservation::Launch
                    }
                } else if let Some(i) = idx {
                    if st.browsers[i].active + st.browsers[i].warm.len()
                        >= self.config.max_contexts_per_browser
                    {
                        if let Some(evicted) = st.browsers[i].warm.pop_back() {
                            excess_warm.push((st.browsers[i].browser.clone(), evicted));
                        }
                    }
                    st.browsers[i].active += 1;
                    st.counters.acquires += 1;
                    st.counters.acquires_reused_browser += 1;
                    Reservation::Reuse(st.browsers[i].browser.clone())
                } else {
                    st.launching += 1;
                    Reservation::Launch
                };
                batch.push((waiter, reservation, permit));
            }
        }
        notify_n(&self.queue_space, abandoned_total + batch.len());
        if batch.is_empty() {
            self.maybe_prewarm().await;
            return Ok(false);
        }
        for (browser, evicted) in excess_warm {
            tokio::spawn(async move {
                let dir = evicted.download_dir;
                let _ = browser.dispose_context(&evicted.context).await;
                if matches!(dir.try_exists(), Ok(true)) {
                    tokio::task::spawn_blocking(move || {
                        let _ = std::fs::remove_dir_all(dir);
                    });
                }
            });
        }
        for (waiter, reservation, permit) in batch {
            let inner = self.clone();
            tokio::spawn(async move {
                let res = inner
                    .prepare_context(waiter.proxy.as_ref(), reservation, waiter.eager_page)
                    .await;
                let _ = waiter.tx.send(res);
                drop(permit);
                inner.admit.notify_one();
            });
        }
        Ok(true)
    }

    /// Last measured process-tree RSS of all browsers. Never waits for the walk: a stale
    /// value starts one background refresh. The walk takes ~30 ms idle and ~200 ms under
    /// load, and every acquire used to queue behind it.
    async fn browser_tree_rss(self: &Arc<Self>) -> u64 {
        let (at, tree) = {
            let cache = self.rss_cache.lock().await;
            (cache.0, cache.1.values().copied().sum())
        };
        if at.is_none_or(|t| t.elapsed() >= Duration::from_millis(500))
            && !self.rss_refreshing.swap(true, Ordering::SeqCst)
        {
            let me = self.clone();
            tokio::spawn(async move {
                let pids: Vec<u32> = {
                    let st = me.state.lock().await;
                    st.browsers
                        .iter()
                        .filter(|s| !s.crashed && s.browser.pid > 0)
                        .map(|s| s.browser.pid)
                        .collect()
                };
                let by_pid = if pids.is_empty() {
                    HashMap::new()
                } else {
                    tokio::task::spawn_blocking(move || process_tree_memory_map(&pids))
                        .await
                        .unwrap_or_default()
                };
                *me.rss_cache.lock().await = (Some(Instant::now()), by_pid);
                me.rss_refreshing.store(false, Ordering::SeqCst);
                me.admit.notify_one();
            });
        }
        tree
    }

    /// The fleet's memory for metrics: the last walk while it is under a second old, else a new
    /// one. Every dashboard tab, scrape and stats call used to walk the whole process table.
    async fn recent_tree_rss(&self, pids: Vec<u32>) -> u64 {
        if pids.is_empty() {
            return 0;
        }
        {
            let cache = self.rss_cache.lock().await;
            if cache.0.is_some_and(|t| t.elapsed() < Duration::from_secs(1))
                && pids.iter().all(|p| cache.1.contains_key(p))
            {
                return pids.iter().map(|p| cache.1.get(p).copied().unwrap_or(0)).sum();
            }
        }
        let by_pid = tokio::task::spawn_blocking(move || process_tree_memory_map(&pids))
            .await
            .unwrap_or_default();
        let total = by_pid.values().copied().sum();
        *self.rss_cache.lock().await = (Some(Instant::now()), by_pid);
        total
    }

    async fn maybe_prewarm(self: &Arc<Self>) {
        let target_per_browser = self.config.prewarm_contexts;
        if target_per_browser == 0 {
            return;
        }
        let ceiling = self.config.memory_ceiling_mb * 1024 * 1024;
        if ceiling > 0 {
            let tree: u64 = self.rss_cache.lock().await.1.values().copied().sum();
            if tree > ceiling {
                return;
            }
        }
        let mut to_spawn: Vec<(Arc<CdpBrowser>, tokio::sync::OwnedSemaphorePermit)> = Vec::new();
        {
            let mut st = self.state.lock().await;
            if st.shutting_down || !st.queue.is_empty() {
                return;
            }
            for s in st.browsers.iter_mut() {
                if s.crashed || s.draining || s.browser.connection_dead() {
                    continue;
                }
                let max_warm = target_per_browser
                    .min(self.config.max_contexts_per_browser.saturating_sub(s.active));
                let have = s.warm.len() + s.warming;
                for _ in have..max_warm {
                    let Ok(permit) = self.grants.clone().try_acquire_owned() else {
                        break;
                    };
                    s.warming += 1;
                    to_spawn.push((s.browser.clone(), permit));
                }
            }
        }
        let nav_timeout = self.config.navigation_timeout;
        for (browser, permit) in to_spawn {
            let inner = self.clone();
            tokio::spawn(async move {
                let _permit = permit;
                let warm_res = async {
                    let ctx = browser.create_context(None).await?;
                    let download_dir = std::env::temp_dir()
                        .join(format!("fluxwright-{}-dl-{}", std::process::id(), ctx.0));
                    let (_, page_res) = tokio::join!(
                        browser.context_allow_downloads(&ctx, &download_dir, Duration::from_secs(5)),
                        browser.create_page(&ctx, nav_timeout),
                    );
                    match page_res {
                        Ok(blank_page) => Ok::<_, fluxwright_cdp::Error>(WarmContext {
                            context: ctx,
                            download_dir,
                            blank_page,
                        }),
                        Err(e) => {
                            let _ = browser.dispose_context(&ctx).await;
                            remove_dir_if_exists(&download_dir);
                            Err(e)
                        }
                    }
                }
                .await;

                let discarded: Option<WarmContext> = {
                    let mut st = inner.state.lock().await;
                    let shutting_down = st.shutting_down;
                    let max_per_browser = inner.config.max_contexts_per_browser;
                    match st.browsers.iter_mut().find(|s| s.browser.id == browser.id) {
                        Some(s) => {
                            s.warming = s.warming.saturating_sub(1);
                            match warm_res {
                                Ok(w)
                                    if !shutting_down
                                        && !s.crashed
                                        && !s.draining
                                        && !s.browser.connection_dead()
                                        && s.warm.len() < target_per_browser
                                        && s.active + s.warm.len() < max_per_browser =>
                                {
                                    s.warm.push_back(w);
                                    None
                                }
                                Ok(w) => Some(w),
                                Err(_) => None,
                            }
                        }
                        None => warm_res.ok(),
                    }
                };
                if let Some(w) = discarded {
                    let _ = browser.dispose_context(&w.context).await;
                    remove_dir_if_exists(&w.download_dir);
                }
                inner.admit.notify_one();
            });
        }
    }

    async fn prepare_context(
        self: &Arc<Self>,
        proxy: Option<&Proxy>,
        reservation: Reservation,
        eager_page: bool,
    ) -> Result<Arc<ContextGuard>> {
        let (browser, warm_page, existing_ctx) = match reservation {
            Reservation::Warm(b, w) => {
                if self.state.lock().await.shutting_down {
                    let _ = b.dispose_context(&w.context).await;
                    remove_dir_if_exists(&w.download_dir);
                    self.release_slot(&b, false).await;
                    return Err(Error::ShuttingDown);
                }
                if !b.connection_dead() && !w.blank_page.is_closed() {
                    (b, Some(w.blank_page), Some((w.context, w.download_dir)))
                } else {
                    let _ = b.dispose_context(&w.context).await;
                    remove_dir_if_exists(&w.download_dir);
                    (b, None, None)
                }
            }
            Reservation::Reuse(b) => {
                if self.state.lock().await.shutting_down {
                    self.release_slot(&b, false).await;
                    return Err(Error::ShuttingDown);
                }
                (b, None, None)
            }
            Reservation::Launch => {
                let launched = self.source.launch(&self.config.launch).await;
                let outcome = {
                    let mut st = self.state.lock().await;
                    st.launching = st.launching.saturating_sub(1);
                    match launched {
                        Ok(browser) if st.shutting_down => Ok(Err(browser)),
                        Ok(browser) => {
                            st.browsers.push(Slot {
                                browser: browser.clone(),
                                active: 1,
                                warm: VecDeque::new(),
                                warming: 0,
                                jobs_served: 0,
                                started: Instant::now(),
                                last_used: Instant::now(),
                                draining: false,
                                crashed: false,
                            });
                            st.counters.acquires += 1;
                            Ok(Ok(browser))
                        }
                        Err(e) => Err(e),
                    }
                };
                match outcome {
                    Ok(Ok(b)) => (b, None, None),
                    Ok(Err(b)) => {
                        let _ = b.close().await;
                        return Err(Error::ShuttingDown);
                    }
                    Err(e) => return Err(e.into()),
                }
            }
        };

        let (ctx, download_dir, warm_page) = match existing_ctx {
            Some((ctx, dir)) => (ctx, dir, warm_page),
            None => {
                let ctx = match browser.create_context(proxy).await {
                    Ok(c) => c,
                    Err(e) => {
                        self.release_slot(&browser, false).await;
                        return Err(e.into());
                    }
                };

                // Named like profile folders, so the sweep after a crash finds it: `fluxwright-<pid>-…`.
                let download_dir = std::env::temp_dir()
                    .join(format!("fluxwright-{}-dl-{}", std::process::id(), ctx.0));
                if eager_page {
                    let (_, page_res) = tokio::join!(
                        browser.context_allow_downloads(&ctx, &download_dir, Duration::from_secs(5)),
                        browser.create_page(&ctx, self.config.navigation_timeout),
                    );
                    match page_res {
                        Ok(page) => (ctx, download_dir, Some(page)),
                        Err(e) => {
                            let _ = browser.dispose_context(&ctx).await;
                            remove_dir_if_exists(&download_dir);
                            self.release_slot(&browser, false).await;
                            return Err(e.into());
                        }
                    }
                } else {
                    // Best effort: a Chrome that refuses only loses downloads.
                    let _ = browser
                        .context_allow_downloads(&ctx, &download_dir, Duration::from_secs(5))
                        .await;
                    (ctx, download_dir, None)
                }
            }
        };

        info!(browser = %browser.id.0, context = %ctx.0, warm = warm_page.is_some(), "context lease granted");

        Ok(Arc::new(ContextGuard {
            engine: self.clone(),
            browser,
            context: ctx,
            download_dir,
            warm_page: std::sync::Mutex::new(warm_page),
            http_defaults: std::sync::Mutex::new(HttpContextDefaults::default()),
            har_recorders: Default::default(),
            har_route_tasks: Default::default(),
            route_entries: Default::default(),
            route_start: tokio::sync::Mutex::new(false),
            route_tasks: Default::default(),
            record_video: Default::default(),
            video_recorders: Default::default(),
            proxy_lease: Default::default(),
            profile_target: Default::default(),
            tracing: ContextTracing::new(),
            runtime: tokio::runtime::Handle::current(),
            failed: AtomicBool::new(false),
            disposed: AtomicBool::new(false),
        }))
    }

    async fn release_slot(self: &Arc<Self>, browser: &CdpBrowser, completed: bool) {
        let has_queue_demand = {
            let mut st = self.state.lock().await;
            if let Some(s) = st.browsers.iter_mut().find(|s| s.browser.id == browser.id) {
                s.active = s.active.saturating_sub(1);
                s.last_used = Instant::now();
                if completed {
                    s.jobs_served += 1;
                }
                if s.crashed || browser.connection_dead() {
                    s.crashed = true;
                }
            }
            !st.queue.is_empty()
        };
        if let Some(ref adaptive) = self.adaptive {
            let ceiling = self.config.memory_ceiling_mb * 1024 * 1024;
            let tree: u64 = self.rss_cache.lock().await.1.values().copied().sum();
            adaptive.on_outcome(completed, has_queue_demand, tree, ceiling);
        }
        self.admit.notify_one();
    }

    async fn reap(&self) {
        let mut st = self.state.lock().await;
        let mut i = 0;
        while i < st.browsers.len() {
            let dead = st.browsers[i].browser.connection_dead() || st.browsers[i].crashed;
            if dead && st.browsers[i].active == 0 {
                let mut slot = st.browsers.remove(i);
                for w in slot.warm.drain(..) {
                    remove_dir_if_exists(&w.download_dir);
                }
                st.counters.crash_count += 1;
                warn!(pid = slot.browser.pid, "removing dead browser");
                // Not awaited: dispatch serves every waiter, and a close can take seconds.
                tokio::spawn(async move {
                    let _ = slot.browser.close().await;
                });
                continue;
            }
            i += 1;
        }
    }

    async fn recycle(self: &Arc<Self>) {
        let now = Instant::now();
        let rss_limit = self.config.recycle_rss_mb;
        let rss_by_pid = if rss_limit.is_some() {
            let _ = self.browser_tree_rss().await;
            self.rss_cache.lock().await.1.clone()
        } else {
            HashMap::new()
        };
        let mut to_close = Vec::new();
        let mut to_dispose_warm: Vec<(Arc<CdpBrowser>, WarmContext)> = Vec::new();
        let mut bump = 0u64;
        {
            let mut st = self.state.lock().await;
            // The idle rule never takes the pool below min_browsers.
            let mut idle_closable = st.serving().saturating_sub(self.config.min_browsers);
            for s in st.browsers.iter_mut() {
                if s.draining || s.crashed {
                    if s.draining && s.active == 0 {
                        for w in s.warm.drain(..) {
                            to_dispose_warm.push((s.browser.clone(), w));
                        }
                        to_close.push(s.browser.clone());
                    }
                    continue;
                }
                let by_jobs = self
                    .config
                    .recycle_after_jobs
                    .is_some_and(|n| s.jobs_served >= n);
                let by_age = self
                    .config
                    .recycle_after
                    .is_some_and(|d| now.duration_since(s.started) >= d);
                let by_rss = s.browser.pid > 0
                    && rss_limit.is_some_and(|mb| {
                        rss_by_pid.get(&s.browser.pid).copied().unwrap_or(0) / (1024 * 1024) >= mb
                    });
                let idle = idle_closable > 0
                    && self.config.idle_timeout.is_some_and(|d| {
                        s.active == 0 && now.duration_since(s.last_used) >= d
                    });
                if idle {
                    idle_closable -= 1;
                }
                if by_jobs || by_age || by_rss || idle {
                    info!(
                        pid = s.browser.pid,
                        jobs = s.jobs_served,
                        by_jobs,
                        by_age,
                        by_rss,
                        idle,
                        "draining browser for recycle"
                    );
                    s.draining = true;
                    bump += 1;
                    for w in s.warm.drain(..) {
                        to_dispose_warm.push((s.browser.clone(), w));
                    }
                    if s.active == 0 {
                        to_close.push(s.browser.clone());
                    }
                }
            }
            st.counters.recycle_count += bump;
            st.browsers.retain(|s| {
                !to_close
                    .iter()
                    .any(|b| b.id == s.browser.id && s.active == 0 && s.draining)
            });
        }
        for (browser, w) in to_dispose_warm {
            tokio::spawn(async move {
                let _ = browser.dispose_context(&w.context).await;
                if matches!(w.download_dir.try_exists(), Ok(true)) {
                    let dir = w.download_dir;
                    tokio::task::spawn_blocking(move || {
                        let _ = std::fs::remove_dir_all(dir);
                    });
                }
            });
        }
        // Not awaited: dispatch serves every waiter, and a close can take seconds.
        for b in to_close {
            tokio::spawn(async move {
                let _ = b.close().await;
            });
        }
    }
}

fn notify_n(notify: &Notify, n: usize) {
    for _ in 0..n {
        notify.notify_one();
    }
}

fn pick_waiter(queue: &mut VecDeque<Waiter>, mixed_priorities: &mut bool) -> (Option<Waiter>, usize) {
    let mut abandoned = 0usize;
    while let Some(front) = queue.front() {
        if front.tx.is_closed() {
            queue.pop_front();
            abandoned += 1;
        } else {
            break;
        }
    }
    let picked = if !*mixed_priorities {
        queue.pop_front()
    } else {
        let mut best: Option<(usize, Priority, Instant)> = None;
        for (i, w) in queue.iter().enumerate() {
            if w.tx.is_closed() {
                continue;
            }
            match best {
                None => best = Some((i, w.priority, w.queued_at)),
                Some((_, bp, bt)) => {
                    if w.priority > bp || (w.priority == bp && w.queued_at < bt) {
                        best = Some((i, w.priority, w.queued_at));
                    }
                }
            }
        }
        match best {
            Some((idx, _, _)) => queue.remove(idx),
            None => {
                abandoned += queue.len();
                queue.clear();
                None
            }
        }
    };
    if queue.is_empty() {
        *mixed_priorities = false;
    }
    (picked, abandoned)
}

/// Waiters whose caller timed out or gave up (the receiver is gone). Left queued, they count
/// against `queue_capacity` and get a page built that nobody takes.
fn drop_abandoned(queue: &mut VecDeque<Waiter>) -> usize {
    let before = queue.len();
    queue.retain(|w| !w.tx.is_closed());
    before - queue.len()
}

async fn apply_context_blocking(guard: &ContextGuard, opts: &JobOptions) -> Result<()> {
    let mut types = Vec::new();
    if opts.block_images {
        types.push(ResourceType::Image);
    }
    if opts.block_fonts {
        types.push(ResourceType::Font);
    }
    if opts.block_media {
        types.push(ResourceType::Media);
    }
    if opts.block_stylesheets {
        types.push(ResourceType::Stylesheet);
    }
    if opts.block_scripts {
        types.push(ResourceType::Script);
    }
    if !types.is_empty() {
        guard
            .browser
            .context_block_resource_types(&guard.context, &types, Duration::from_secs(5))
            .await?;
    }
    if !opts.block_url_patterns.is_empty() {
        guard
            .browser
            .context_set_blocked_urls(&guard.context, &opts.block_url_patterns, Duration::from_secs(5))
            .await?;
    }
    Ok(())
}

fn build_page_lease(
    engine: Arc<Inner>,
    context: Arc<ContextGuard>,
    owns_context: bool,
    page: CdpPage,
    base_url: Option<String>,
    default_timeouts: (Option<Duration>, Option<Duration>),
) -> std::pin::Pin<Box<dyn std::future::Future<Output = PageLease> + Send>> {
    Box::pin(async move {
        let lease_id = Uuid::new_v4();
        let (log, downloads, log_task) = page.collect_logs(&context.download_dir);
        if !context.tracing.track_page_if_inactive(lease_id, &page, &log) {
            context
                .tracing
                .register_page(lease_id, page.clone(), log.clone())
                .await;
        }
        let dialog_listeners = Listeners::default();
        let file_chooser_listeners = Listeners::default();
        let binding_listeners = Listeners::default();
        let popup_listeners: Listeners<PageLease> = Listeners::default();
        let (dialogs_rx, file_choosers_rx, bindings_rx, mut popups_rx) =
            page.install_event_channels().await;
        spawn_page_event_router(
            dialogs_rx,
            dialog_listeners.clone(),
            file_choosers_rx,
            file_chooser_listeners.clone(),
            bindings_rx,
            binding_listeners.clone(),
        );
        let popup_task = tokio::spawn({
            let popup_listeners = popup_listeners.clone();
            let context = context.clone();
            let engine = engine.clone();
            let base_url = base_url.clone();
            async move {
                while let Some(raw_popup) = popups_rx.recv().await {
                    let has_listener = {
                        let mut g = popup_listeners.lock().unwrap();
                        g.retain(|tx| !tx.is_closed());
                        !g.is_empty()
                    };
                    if !has_listener {
                        if context.tracing.is_active() {
                            let popup_lease_id = Uuid::new_v4();
                            let (log, _downloads, log_task) =
                                raw_popup.collect_logs(&context.download_dir);
                            context
                                .tracing
                                .register_popup(popup_lease_id, raw_popup, log, log_task)
                                .await;
                        } else {
                            drop(raw_popup);
                        }
                        continue;
                    }
                    let lease = build_page_lease(
                        engine.clone(),
                        context.clone(),
                        false,
                        raw_popup,
                        base_url.clone(),
                        default_timeouts,
                    )
                    .await;
                    let mut g = popup_listeners.lock().unwrap();
                    g.retain(|tx| !tx.is_closed());
                    if let Some(tx) = g.last() {
                        let _ = tx.send(lease);
                    }
                }
            }
        });

        let record_video_opts = context.record_video.lock().unwrap().clone();
        let (video_recorder, video) = match record_video_opts {
            Some(rv) => match page.new_cdp_session(Duration::from_secs(5)).await {
                Ok(sess) => match VideoRecorder::start(sess, rv).await {
                    Ok((rec, vid)) => {
                        context.video_recorders.lock().unwrap().push(rec.clone());
                        (Some(rec), Some(vid))
                    }
                    Err(err) => {
                        warn!(error = %err, "failed to start video recorder");
                        (None, None)
                    }
                },
                Err(err) => {
                    warn!(error = %err, "failed to attach CDP session for video recording");
                    (None, None)
                }
            },
            None => (None, None),
        };

        PageLease {
            engine,
            context,
            owns_context,
            page,
            lease_id,
            released: false,
            runtime: tokio::runtime::Handle::current(),
            log,
            log_task,
            popup_task,
            downloads: Downloads(Arc::new(Mutex::new(downloads))),
            dialog_listeners,
            file_chooser_listeners,
            binding_listeners,
            binding_handlers: Default::default(),
            binding_dispatch_started: AtomicBool::new(false),
            popup_listeners,
            default_timeouts: std::sync::Mutex::new(default_timeouts),
            host_slot: HostSlot::default(),
            base_url: std::sync::Mutex::new(base_url),
            har_recorders: Default::default(),
            har_route_tasks: Default::default(),
            route_entries: Default::default(),
            route_start: tokio::sync::Mutex::new(false),
            route_tasks: Default::default(),
            video_recorder: std::sync::Mutex::new(video_recorder),
            video,
        }
    })
}

fn spawn_har_route_handler(
    router: Arc<HarRouter>,
    url_filter: Option<String>,
    not_found: HarNotFound,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<InterceptedRequest>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(req) = rx.recv().await {
            let matches = url_filter
                .as_deref()
                .is_none_or(|pat| fluxwright_cdp::url_glob_matches(pat, &req.url));
            if !matches {
                let _ = req.continue_with(Default::default()).await;
                continue;
            }
            if let Some(fulfill) = router.match_request(
                &req.url,
                &req.method,
                &req.headers,
                req.post_data.as_deref(),
            ) {
                let _ = req.fulfill(fulfill).await;
            } else if not_found == HarNotFound::Fallback {
                let _ = req.continue_with(Default::default()).await;
            } else {
                let _ = req.abort("failed").await;
            }
        }
    })
}

fn spawn_binding_dispatcher(
    started: &AtomicBool,
    handlers: Arc<std::sync::Mutex<HashMap<String, BindingHandler>>>,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<fluxwright_cdp::BindingCall>,
) {
    if started
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
    {
        tokio::spawn(async move {
            while let Some(call) = rx.recv().await {
                let h = handlers.lock().unwrap().get(&call.name).cloned();
                tokio::spawn(async move {
                    match h {
                        Some(handler) => match handler(call.args.clone()).await {
                            Ok(val) => {
                                let _ = call.resolve(&val).await;
                            }
                            Err(msg) => {
                                let _ = call.reject(&msg).await;
                            }
                        },
                        None => {
                            let _ = call
                                .reject(&format!("Function \"{}\" is not registered", call.name))
                                .await;
                        }
                    }
                });
            }
        });
    }
}

impl ContextLease {
    fn timeout(&self) -> Duration {
        self.action_timeout
            .unwrap_or(self.guard.engine.config.action_timeout)
    }

    pub fn is_closed(&self) -> bool {
        self.guard.is_disposed() || self.guard.browser.connection_dead()
    }

    pub fn cdp_connection(&self) -> Arc<fluxwright_cdp::Connection> {
        self.guard.browser.connection()
    }

    pub fn context_id(&self) -> &str {
        &self.guard.context.0
    }

    pub async fn discard_warm_page(&self) {
        self.guard.invalidate_warm_page().await;
    }

    pub fn mark_external_context(&self, ctx_id: &str) {
        self.guard.browser.mark_external_context(ctx_id);
    }

    pub fn unmark_external_context(&self, ctx_id: &str) {
        self.guard.browser.unmark_external_context(ctx_id);
    }

    pub fn mark_external_session(&self, session_id: &str) {
        self.guard.browser.mark_external_session(session_id);
    }

    pub fn unmark_external_session(&self, session_id: &str) {
        self.guard.browser.unmark_external_session(session_id);
    }

    /// Opens a new tab in this context sharing cookies, routes, emulation, and storage.
    pub async fn new_page(&self) -> Result<PageLease> {
        if self.is_closed() {
            return Err(Error::Other("browser context is closed".into()));
        }
        let warm = self
            .guard
            .warm_page
            .lock()
            .unwrap()
            .take()
            .filter(|p| !p.is_closed());
        let page = match warm {
            Some(p) => p,
            None => {
                self.guard
                    .browser
                    .create_page(
                        &self.guard.context,
                        self.navigation_timeout
                            .or(self.action_timeout)
                            .unwrap_or(self.guard.engine.config.navigation_timeout),
                    )
                    .await?
            }
        };
        Ok(build_page_lease(
            self.guard.engine.clone(),
            self.guard.clone(),
            false,
            page,
            self.base_url.clone(),
            (self.action_timeout, self.navigation_timeout),
        )
        .await)
    }

    pub async fn cookies(&self) -> Result<Vec<fluxwright_cdp::Cookie>> {
        if self.is_closed() {
            return Err(Error::Other("browser context is closed".into()));
        }
        Ok(self
            .guard
            .browser
            .context_cookies(&self.guard.context, self.timeout())
            .await?)
    }

    pub async fn cookies_for_urls(&self, urls: &[String]) -> Result<Vec<fluxwright_cdp::Cookie>> {
        if self.is_closed() {
            return Err(Error::Other("browser context is closed".into()));
        }
        Ok(self
            .guard
            .browser
            .context_cookies_for_urls(&self.guard.context, urls, self.timeout())
            .await?)
    }

    pub async fn add_cookies(&self, cookies: &[fluxwright_cdp::Cookie]) -> Result<()> {
        if self.is_closed() {
            return Err(Error::Other("browser context is closed".into()));
        }
        Ok(self
            .guard
            .browser
            .context_add_cookies(&self.guard.context, cookies, self.timeout())
            .await?)
    }

    pub async fn clear_cookies(&self) -> Result<()> {
        if self.is_closed() {
            return Err(Error::Other("browser context is closed".into()));
        }
        Ok(self
            .guard
            .browser
            .context_clear_cookies(&self.guard.context, self.timeout())
            .await?)
    }

    pub async fn clear_cookies_filtered(&self, filter: &CookieFilter) -> Result<()> {
        if self.is_closed() {
            return Err(Error::Other("browser context is closed".into()));
        }
        Ok(self
            .guard
            .browser
            .context_clear_cookies_filtered(&self.guard.context, filter, self.timeout())
            .await?)
    }

    pub async fn replace_cookies(&self, cookies: &[fluxwright_cdp::Cookie]) -> Result<()> {
        if self.is_closed() {
            return Err(Error::Other("browser context is closed".into()));
        }
        Ok(self
            .guard
            .browser
            .context_replace_cookies(&self.guard.context, cookies, self.timeout())
            .await?)
    }

    pub fn request(&self) -> ApiRequestContext {
        ApiRequestContext {
            guard: self.guard.clone(),
            default_timeout: self.timeout(),
        }
    }

    pub async fn add_init_script(&self, source: &str) -> Result<()> {
        if self.is_closed() {
            return Err(Error::Other("browser context is closed".into()));
        }
        self.guard.invalidate_warm_page().await;
        Ok(self
            .guard
            .browser
            .context_add_init_script(&self.guard.context, source, self.timeout())
            .await?)
    }

    pub fn subscribe_bindings(
        &self,
    ) -> tokio::sync::mpsc::UnboundedReceiver<fluxwright_cdp::BindingCall> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        self.binding_listeners.lock().unwrap().push(tx);
        rx
    }

    pub async fn expose_binding(&self, name: &str) -> Result<()> {
        if self.is_closed() {
            return Err(Error::Other("browser context is closed".into()));
        }
        self.context_bindings_installed
            .get_or_init(|| async {
                route_newest(
                    self.guard.browser.context_bindings(&self.guard.context).await,
                    self.binding_listeners.clone(),
                );
            })
            .await;
        Ok(self
            .guard
            .browser
            .context_expose_binding(&self.guard.context, name, self.timeout())
            .await?)
    }

    pub async fn expose_function<F, Fut>(&self, name: &str, handler: F) -> Result<()>
    where
        F: Fn(Vec<serde_json::Value>) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = std::result::Result<serde_json::Value, String>>
            + Send
            + 'static,
    {
        if !self.binding_dispatch_started.load(Ordering::SeqCst) {
            spawn_binding_dispatcher(
                &self.binding_dispatch_started,
                self.binding_handlers.clone(),
                self.subscribe_bindings(),
            );
        }
        let handler: BindingHandler = Arc::new(move |args| Box::pin(handler(args)));
        {
            let mut handlers = self.binding_handlers.lock().unwrap();
            if handlers.contains_key(name) {
                return Err(Error::Other(format!(
                    "Function \"{name}\" has been already registered"
                )));
            }
            handlers.insert(name.to_string(), handler);
        }
        if let Err(err) = self.expose_binding(name).await {
            self.binding_handlers.lock().unwrap().remove(name);
            return Err(err);
        }
        Ok(())
    }

    pub async fn intercept(
        &self,
    ) -> Result<tokio::sync::mpsc::UnboundedReceiver<InterceptedRequest>> {
        if self.is_closed() {
            return Err(Error::Other("browser context is closed".into()));
        }
        Ok(self
            .guard
            .browser
            .context_intercept(&self.guard.context, self.timeout())
            .await?)
    }

    /// Registers a context-wide route handler (`url` glob or predicate), running newest-first.
    pub async fn route<F, Fut>(&self, pattern: impl Into<UrlPattern>, handler: F) -> Result<()>
    where
        F: Fn(Route) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Result<()>> + Send + 'static,
    {
        self.route_with(pattern, handler, RouteOptions::default()).await
    }

    /// Registers a context-wide route handler with [`RouteOptions`] (such as `times`).
    pub async fn route_with<F, Fut>(
        &self,
        pattern: impl Into<UrlPattern>,
        handler: F,
        options: RouteOptions,
    ) -> Result<()>
    where
        F: Fn(Route) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Result<()>> + Send + 'static,
    {
        if self.is_closed() {
            return Err(Error::Other("browser context is closed".into()));
        }
        {
            let mut started = self.guard.route_start.lock().await;
            if !*started {
                let rx = self.intercept().await?;
                let task = spawn_route_dispatcher(
                    None,
                    self.guard.route_entries.clone(),
                    rx,
                );
                self.guard.route_tasks.lock().unwrap().push(task);
                *started = true;
            }
        }
        push_route_entry(&self.guard.route_entries, pattern.into(), handler, options);
        Ok(())
    }

    /// Removes context-wide handlers registered for the matching glob `pattern`.
    pub async fn unroute(&self, pattern: &str) {
        remove_route_entries(&self.guard.route_entries, pattern);
    }

    pub async fn new_cdp_session(&self, page: &PageLease) -> Result<CdpSession> {
        if self.is_closed() {
            return Err(Error::Other("browser context is closed".into()));
        }
        if page.page.context_id.0 != self.guard.context.0 {
            return Err(Error::Other("page does not belong to this context".into()));
        }
        page.new_cdp_session().await
    }

    pub async fn start_har_recording(&self, opts: RecordHarOptions) -> Result<()> {
        if self.is_closed() {
            return Err(Error::Other("browser context is closed".into()));
        }
        let rec = self
            .guard
            .browser
            .start_har_recording(&self.guard.context, None, opts)
            .await;
        self.guard.har_recorders.lock().unwrap().push(rec);
        Ok(())
    }

    pub async fn route_from_har(
        &self,
        path: impl AsRef<std::path::Path>,
        opts: &HarRouteOptions,
    ) -> Result<()> {
        if self.is_closed() {
            return Err(Error::Other("browser context is closed".into()));
        }
        if opts.update {
            return self
                .start_har_recording(RecordHarOptions {
                    path: path.as_ref().to_path_buf(),
                    omit_content: false,
                    content: Some(opts.update_content),
                    mode: opts.update_mode,
                    url_filter: opts.url.clone(),
                })
                .await;
        }
        let router = Arc::new(HarRouter::from_file(path)?);
        let rx = self.intercept().await?;
        let task = spawn_har_route_handler(router, opts.url.clone(), opts.not_found, rx);
        self.guard.har_route_tasks.lock().unwrap().push(task);
        Ok(())
    }

    pub fn tracing(&self) -> Tracing {
        Tracing::new(Arc::clone(&self.guard.tracing))
    }

    pub async fn close(&self) -> Result<()> {
        self.guard.dispose(true).await;
        Ok(())
    }

    async fn dispose_failed(&self) {
        self.guard.dispose(false).await;
    }
}

impl PageLease {
    async fn traced<F, P, T>(
        &self,
        class_name: &str,
        method_name: &str,
        api_name: &str,
        params: P,
        fut: F,
    ) -> Result<T>
    where
        F: std::future::Future<Output = Result<T>>,
        P: FnOnce() -> serde_json::Value,
    {
        if !self.context.tracing.is_active() {
            return fut.await;
        }
        self.context
            .tracing
            .trace_action(
                &self.page.target_id.0,
                class_name,
                method_name,
                api_name,
                params,
                fut,
            )
            .await
    }

    /// For click, fill, waits, screenshots and the like: the call's own timeout, else this
    /// page's default, else the engine's.
    fn action_timeout(&self) -> Duration {
        CALL_TIMEOUT
            .try_with(|t| *t)
            .ok()
            .or(self.default_timeouts.lock().unwrap().0)
            .unwrap_or(self.engine.config.action_timeout)
    }

    /// For `goto`. As in Playwright, the page's default timeout covers navigation too unless
    /// a navigation default was set.
    pub fn navigation_timeout(&self) -> Duration {
        let (action, navigation) = *self.default_timeouts.lock().unwrap();
        CALL_TIMEOUT
            .try_with(|t| *t)
            .ok()
            .or(navigation)
            .or(action)
            .unwrap_or(self.engine.config.navigation_timeout)
    }

    /// The timeout for this page's calls from now on, navigation included (Playwright's
    /// `page.setDefaultTimeout`).
    pub fn set_default_timeout(&self, timeout: Duration) {
        self.default_timeouts.lock().unwrap().0 = Some(timeout);
    }

    /// The timeout for this page's `goto` from now on (`page.setDefaultNavigationTimeout`).
    pub fn set_default_navigation_timeout(&self, timeout: Duration) {
        self.default_timeouts.lock().unwrap().1 = Some(timeout);
    }

    pub fn browser_pid(&self) -> u32 {
        self.context.browser.pid
    }

    /// WebSocket endpoint URL if connected over CDP or local WebSocket transport.
    pub fn ws_endpoint(&self) -> Option<&str> {
        self.context.browser.ws_endpoint()
    }

    /// Returns `true` if connected to a remote browser rather than a locally spawned OS child process.
    pub fn is_remote(&self) -> bool {
        self.context.browser.is_remote()
    }

    pub fn lease_id(&self) -> Uuid {
        self.lease_id
    }

    /// Recorded video handle when `JobOptions::record_video` was configured on this context.
    pub fn video(&self) -> Option<Video> {
        self.video.clone()
    }

    pub fn is_closed(&self) -> bool {
        self.released || self.context.is_disposed() || self.page.is_closed()
    }

    pub fn target_id(&self) -> &str {
        self.page.target_id()
    }

    pub fn cdp_connection(&self) -> Arc<fluxwright_cdp::Connection> {
        self.context.browser.connection()
    }

    pub fn opener_target_id(&self) -> Option<&str> {
        self.page.opener_id()
    }

    pub async fn bring_to_front(&self) -> Result<()> {
        Ok(self.page.bring_to_front(self.action_timeout()).await?)
    }

    pub async fn new_cdp_session(&self) -> Result<CdpSession> {
        if self.is_closed() {
            return Err(Error::Other("page is closed".into()));
        }
        Ok(self.page.new_cdp_session(self.action_timeout()).await?)
    }

    pub async fn new_frame_cdp_session(&self, frame_id: &str) -> Result<CdpSession> {
        if self.is_closed() {
            return Err(Error::Other("page is closed".into()));
        }
        Ok(self
            .page
            .new_frame_cdp_session(frame_id, self.action_timeout())
            .await?)
    }

    pub fn subscribe_popups(&self) -> tokio::sync::mpsc::UnboundedReceiver<PageLease> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        self.popup_listeners.lock().unwrap().push(tx);
        rx
    }

    pub fn frame_infos(&self) -> Vec<FrameInfo> {
        self.page.frames()
    }

    pub fn main_frame_id(&self) -> &str {
        self.page.main_frame_id()
    }

    pub fn frames(&self) -> Vec<Frame<'_>> {
        self.page
            .frames()
            .into_iter()
            .map(|info| Frame { lease: self, info })
            .collect()
    }

    pub fn main_frame(&self) -> Frame<'_> {
        let id = self.main_frame_id();
        let info = self
            .frame_infos()
            .into_iter()
            .find(|f| f.id == id)
            .unwrap_or_else(|| FrameInfo {
                id: id.to_owned(),
                parent_id: None,
                name: String::new(),
                url: self.url(),
            });
        Frame { lease: self, info }
    }

    pub async fn frame_goto(&self, frame_id: &str, url: &str) -> Result<()> {
        self.frame_goto_with(frame_id, url, fluxwright_cdp::WaitUntil::Load).await
    }

    pub async fn frame_goto_with(
        &self,
        frame_id: &str,
        url: &str,
        wait_until: fluxwright_cdp::WaitUntil,
    ) -> Result<()> {
        let base = self.base_url.lock().unwrap().clone();
        let url = resolve_url(base.as_deref(), url);
        self.engine.hosts.enter(&url, &self.host_slot).await;
        self.traced(
            "Frame",
            "goto",
            "frame.goto",
            || serde_json::json!({ "frameId": frame_id, "url": url }),
            async {
                self.page
                    .frame_goto(frame_id, &url, wait_until, self.navigation_timeout())
                    .await?;
                Ok(())
            },
        )
        .await
    }

    pub async fn frame_evaluate(&self, frame_id: &str, expression: &str) -> Result<serde_json::Value> {
        self.traced(
            "Frame",
            "evaluate",
            "frame.evaluate",
            || serde_json::json!({ "frameId": frame_id }),
            async {
                Ok(self
                    .page
                    .frame_evaluate(frame_id, expression, self.action_timeout())
                    .await?)
            },
        )
        .await
    }

    pub fn frame_locator_by_id(&self, frame_id: &str) -> FrameLocator<'_> {
        FrameLocator {
            lease: self,
            frames: vec![Selector::frame_id_hop(frame_id)],
        }
    }

    /// Every cookie in this page's context plus localStorage of the current origin, in
    /// Playwright's `storageState` format. Start a later job from it with
    /// `JobOptions::storage_state` to skip logging in again.
    pub async fn storage_state(&self) -> Result<fluxwright_cdp::StorageState> {
        self.storage_state_with(false).await
    }

    pub async fn storage_state_with(
        &self,
        include_indexed_db: bool,
    ) -> Result<fluxwright_cdp::StorageState> {
        Ok(self
            .page
            .storage_state_with(include_indexed_db, self.action_timeout())
            .await?)
    }

    pub fn request(&self) -> ApiRequestContext {
        ApiRequestContext {
            guard: self.context.clone(),
            default_timeout: self.action_timeout(),
        }
    }

    pub async fn add_cookies(&self, cookies: &[fluxwright_cdp::Cookie]) -> Result<()> {
        Ok(self.page.add_cookies(cookies, self.action_timeout()).await?)
    }

    pub async fn clear_cookies(&self) -> Result<()> {
        Ok(self.page.clear_cookies(self.action_timeout()).await?)
    }

    pub async fn emulate_media(&self, media: Option<&str>, color_scheme: Option<&str>) -> Result<()> {
        Ok(self
            .page
            .emulate_media(media, color_scheme, self.action_timeout())
            .await?)
    }

    /// Navigates and waits for the `load` event.
    pub async fn goto(&self, url: &str) -> Result<()> {
        self.goto_with(url, fluxwright_cdp::WaitUntil::Load).await
    }

    pub async fn goto_with(&self, url: &str, wait_until: fluxwright_cdp::WaitUntil) -> Result<()> {
        let base = self.base_url.lock().unwrap().clone();
        let url = resolve_url(base.as_deref(), url);
        self.engine.hosts.enter(&url, &self.host_slot).await;
        self.traced(
            "Page",
            "goto",
            "page.goto",
            || serde_json::json!({ "url": url }),
            async {
                self.page
                    .goto(&url, wait_until, self.navigation_timeout())
                    .await?;
                Ok(())
            },
        )
        .await
    }

    pub fn url(&self) -> String {
        self.page.url()
    }

    pub fn subscribe_url(&self) -> tokio::sync::watch::Receiver<String> {
        self.page.subscribe_url()
    }

    pub fn base_url(&self) -> Option<String> {
        self.base_url.lock().unwrap().clone()
    }

    pub async fn reload(&self) -> Result<()> {
        self.reload_with(fluxwright_cdp::WaitUntil::Load).await
    }

    pub async fn reload_with(&self, wait_until: fluxwright_cdp::WaitUntil) -> Result<()> {
        self.traced(
            "Page",
            "reload",
            "page.reload",
            || serde_json::json!({}),
            async {
                self.page.reload(wait_until, self.navigation_timeout()).await?;
                Ok(())
            },
        )
        .await
    }

    pub async fn go_back(&self) -> Result<bool> {
        self.go_back_with(fluxwright_cdp::WaitUntil::Load).await
    }

    pub async fn go_back_with(&self, wait_until: fluxwright_cdp::WaitUntil) -> Result<bool> {
        self.traced(
            "Page",
            "goBack",
            "page.goBack",
            || serde_json::json!({}),
            async {
                Ok(self
                    .page
                    .go_back(wait_until, self.navigation_timeout())
                    .await?)
            },
        )
        .await
    }

    pub async fn go_forward(&self) -> Result<bool> {
        self.go_forward_with(fluxwright_cdp::WaitUntil::Load).await
    }

    pub async fn go_forward_with(&self, wait_until: fluxwright_cdp::WaitUntil) -> Result<bool> {
        self.traced(
            "Page",
            "goForward",
            "page.goForward",
            || serde_json::json!({}),
            async {
                Ok(self
                    .page
                    .go_forward(wait_until, self.navigation_timeout())
                    .await?)
            },
        )
        .await
    }

    pub async fn set_content(&self, html: &str) -> Result<()> {
        self.set_content_with(html, fluxwright_cdp::WaitUntil::Load).await
    }

    pub async fn set_content_with(&self, html: &str, wait_until: fluxwright_cdp::WaitUntil) -> Result<()> {
        self.traced(
            "Page",
            "setContent",
            "page.setContent",
            || serde_json::json!({ "length": html.len() }),
            async {
                self.page
                    .set_content(html, wait_until, self.navigation_timeout())
                    .await?;
                Ok(())
            },
        )
        .await
    }

    pub async fn wait_for_load_state(&self, state: fluxwright_cdp::WaitUntil) -> Result<()> {
        self.traced(
            "Page",
            "waitForLoadState",
            "page.waitForLoadState",
            || serde_json::json!({}),
            async {
                self.page
                    .wait_for_load_state(state, self.navigation_timeout())
                    .await?;
                Ok(())
            },
        )
        .await
    }

    pub async fn wait_for_url(&self, pattern: &str, wait_until: fluxwright_cdp::WaitUntil) -> Result<()> {
        let base = self.base_url();
        let resolved = if pattern.starts_with('/')
            || (!pattern.starts_with('*')
                && !pattern.split(['/', '?', '#']).next().unwrap_or("").contains(':')
                && !pattern.contains('{'))
        {
            resolve_url(base.as_deref(), pattern)
        } else {
            pattern.to_string()
        };
        self.wait_for_url_matching(|u| fluxwright_cdp::url_glob_matches(&resolved, u), wait_until)
            .await
    }

    pub async fn wait_for_url_matching<F>(
        &self,
        predicate: F,
        wait_until: fluxwright_cdp::WaitUntil,
    ) -> Result<()>
    where
        F: FnMut(&str) -> bool,
    {
        self.page
            .wait_for_url(predicate, wait_until, self.navigation_timeout())
            .await?;
        Ok(())
    }

    pub async fn wait_for_url_change(&self, after_url: &str) -> Result<String> {
        Ok(self
            .page
            .wait_for_url_change(after_url, self.navigation_timeout())
            .await?)
    }

    pub async fn wait_for_function(&self, expression: &str) -> Result<serde_json::Value> {
        self.wait_for_function_with(expression, fluxwright_cdp::WaitPolling::Raf)
            .await
    }

    pub async fn wait_for_function_with(
        &self,
        expression: &str,
        polling: fluxwright_cdp::WaitPolling,
    ) -> Result<serde_json::Value> {
        self.traced(
            "Page",
            "waitForFunction",
            "page.waitForFunction",
            || serde_json::json!({ "expression": expression }),
            async {
                Ok(self
                    .page
                    .wait_for_function(expression, polling, self.action_timeout())
                    .await?)
            },
        )
        .await
    }

    pub async fn title(&self) -> Result<String> {
        Ok(self.page.title(self.action_timeout()).await?)
    }

    pub async fn content(&self) -> Result<String> {
        Ok(self.page.content(self.action_timeout()).await?)
    }

    /// Captures a compact accessibility tree snapshot of the page and caches `@eN` element refs.
    pub async fn aria_snapshot(&self) -> Result<AriaSnapshot> {
        self.aria_snapshot_with(&SnapshotOptions::default()).await
    }

    pub async fn aria_snapshot_with(&self, options: &SnapshotOptions) -> Result<AriaSnapshot> {
        Ok(self
            .page
            .aria_snapshot(options, self.action_timeout())
            .await?)
    }

    /// Alias for [`Self::aria_snapshot`].
    pub async fn snapshot(&self) -> Result<AriaSnapshot> {
        self.aria_snapshot().await
    }

    /// Alias for [`Self::aria_snapshot_with`].
    pub async fn snapshot_with(&self, options: &SnapshotOptions) -> Result<AriaSnapshot> {
        self.aria_snapshot_with(options).await
    }

    /// Serializes the visible DOM (or a sub-tree matching `selector`) cleanly to Markdown.
    pub async fn markdown(&self) -> Result<String> {
        self.markdown_with(&MarkdownOptions::default()).await
    }

    pub async fn markdown_with(&self, options: &MarkdownOptions) -> Result<String> {
        Ok(self.page.markdown(options, self.action_timeout()).await?)
    }

    pub async fn evaluate(&self, expression: &str) -> Result<serde_json::Value> {
        self.traced(
            "Page",
            "evaluate",
            "page.evaluate",
            || serde_json::json!({ "expression": expression }),
            async {
                Ok(self
                    .page
                    .evaluate(expression, self.action_timeout())
                    .await?)
            },
        )
        .await
    }

    pub async fn screenshot(&self) -> Result<Vec<u8>> {
        self.screenshot_with(&ScreenshotOptions::default()).await
    }

    /// PNG of the whole scrollable page, not just the viewport.
    pub async fn screenshot_full_page(&self) -> Result<Vec<u8>> {
        self.screenshot_with(&ScreenshotOptions { full_page: true, ..Default::default() })
            .await
    }

    /// PNG or JPEG, of the viewport, the whole page or a clip.
    pub async fn screenshot_with(&self, options: &ScreenshotOptions) -> Result<Vec<u8>> {
        self.traced(
            "Page",
            "screenshot",
            "page.screenshot",
            || serde_json::json!({ "fullPage": options.full_page }),
            async {
                Ok(self
                    .page
                    .screenshot(options, self.action_timeout())
                    .await?)
            },
        )
        .await
    }

    /// The page printed to PDF (headless Chrome only).
    pub async fn pdf(&self, options: &PdfOptions) -> Result<Vec<u8>> {
        self.pdf_within(options, fluxwright_cdp::MAX_PDF_BYTES).await
    }

    /// `pdf`, failing without reading on once the PDF is over `max_bytes`: for a caller that
    /// must bound what one job can make it hold.
    pub async fn pdf_within(&self, options: &PdfOptions, max_bytes: usize) -> Result<Vec<u8>> {
        self.traced(
            "Page",
            "pdf",
            "page.pdf",
            || serde_json::json!({}),
            async {
                Ok(self
                    .page
                    .pdf(options, max_bytes, self.action_timeout())
                    .await?)
            },
        )
        .await
    }

    /// CSS-pixel viewport for this lease's page. Leases are fresh contexts, so it never leaks.
    pub async fn set_viewport_size(&self, width: u32, height: u32) -> Result<()> {
        self.traced(
            "Page",
            "setViewportSize",
            "page.setViewportSize",
            || serde_json::json!({ "width": width, "height": height }),
            async {
                Ok(self
                    .page
                    .set_viewport(width, height, self.action_timeout())
                    .await?)
            },
        )
        .await
    }

    /// Selectors: CSS, `text=`, `role=` (see [`Selector`]), or a [`Selector`] with frames.
    pub async fn click(&self, selector: impl Into<Selector>) -> Result<()> {
        let sel = selector.into();
        self.traced(
            "Page",
            "click",
            "page.click",
            || serde_json::json!({ "selector": sel.query }),
            async {
                self.page.click(&sel, self.action_timeout()).await?;
                Ok(())
            },
        )
        .await
    }

    pub async fn click_with(
        &self,
        selector: impl Into<Selector>,
        options: &fluxwright_cdp::ClickOptions,
    ) -> Result<()> {
        let sel = selector.into();
        self.traced(
            "Page",
            "click",
            "page.click",
            || serde_json::json!({ "selector": sel.query }),
            async {
                self.page
                    .click_with(&sel, options, self.action_timeout())
                    .await?;
                Ok(())
            },
        )
        .await
    }

    pub async fn dblclick(
        &self,
        selector: impl Into<Selector>,
        options: &fluxwright_cdp::ClickOptions,
    ) -> Result<()> {
        let sel = selector.into();
        self.traced(
            "Page",
            "dblclick",
            "page.dblclick",
            || serde_json::json!({ "selector": sel.query }),
            async {
                self.page
                    .dblclick(&sel, options, self.action_timeout())
                    .await?;
                Ok(())
            },
        )
        .await
    }

    pub async fn hover(
        &self,
        selector: impl Into<Selector>,
        options: &fluxwright_cdp::HoverOptions,
    ) -> Result<()> {
        let sel = selector.into();
        self.traced(
            "Page",
            "hover",
            "page.hover",
            || serde_json::json!({ "selector": sel.query }),
            async {
                self.page
                    .hover(&sel, options, self.action_timeout())
                    .await?;
                Ok(())
            },
        )
        .await
    }

    pub async fn focus(&self, selector: impl Into<Selector>) -> Result<()> {
        let sel = selector.into();
        self.traced(
            "Page",
            "focus",
            "page.focus",
            || serde_json::json!({ "selector": sel.query }),
            async {
                self.page.focus(&sel, self.action_timeout()).await?;
                Ok(())
            },
        )
        .await
    }

    pub async fn blur(&self, selector: impl Into<Selector>) -> Result<()> {
        let sel = selector.into();
        self.traced(
            "Page",
            "blur",
            "page.blur",
            || serde_json::json!({ "selector": sel.query }),
            async {
                self.page.blur(&sel, self.action_timeout()).await?;
                Ok(())
            },
        )
        .await
    }

    pub async fn scroll_into_view_if_needed(&self, selector: impl Into<Selector>) -> Result<()> {
        let sel = selector.into();
        self.traced(
            "Page",
            "scrollIntoViewIfNeeded",
            "page.scrollIntoViewIfNeeded",
            || serde_json::json!({ "selector": sel.query }),
            async {
                self.page
                    .scroll_into_view_if_needed(&sel, self.action_timeout())
                    .await?;
                Ok(())
            },
        )
        .await
    }

    pub async fn drag_and_drop(
        &self,
        source: impl Into<Selector>,
        target: impl Into<Selector>,
        force: bool,
    ) -> Result<()> {
        let src = source.into();
        let dst = target.into();
        self.traced(
            "Page",
            "dragAndDrop",
            "page.dragAndDrop",
            || serde_json::json!({ "source": src.query, "target": dst.query }),
            async {
                self.page
                    .drag_and_drop(&src, &dst, force, self.action_timeout())
                    .await?;
                Ok(())
            },
        )
        .await
    }

    pub async fn fill(&self, selector: impl Into<Selector>, value: &str) -> Result<()> {
        let sel = selector.into();
        self.traced(
            "Page",
            "fill",
            "page.fill",
            || serde_json::json!({ "selector": sel.query, "value": value }),
            async {
                self.page.fill(&sel, value, self.action_timeout()).await?;
                Ok(())
            },
        )
        .await
    }

    pub async fn clear(&self, selector: impl Into<Selector>, force: bool) -> Result<()> {
        let sel = selector.into();
        self.traced(
            "Page",
            "clear",
            "page.clear",
            || serde_json::json!({ "selector": sel.query }),
            async {
                self.page
                    .fill_with(&sel, "", force, self.action_timeout())
                    .await?;
                Ok(())
            },
        )
        .await
    }

    pub async fn press(
        &self,
        selector: impl Into<Selector>,
        key: &str,
        delay: Option<Duration>,
    ) -> Result<()> {
        let sel = selector.into();
        self.traced(
            "Page",
            "press",
            "page.press",
            || serde_json::json!({ "selector": sel.query, "key": key }),
            async {
                self.page
                    .press(&sel, key, delay, self.action_timeout())
                    .await?;
                Ok(())
            },
        )
        .await
    }

    pub async fn press_sequentially(
        &self,
        selector: impl Into<Selector>,
        text: &str,
        delay: Option<Duration>,
    ) -> Result<()> {
        let sel = selector.into();
        self.traced(
            "Page",
            "pressSequentially",
            "page.pressSequentially",
            || serde_json::json!({ "selector": sel.query, "text": text }),
            async {
                self.page
                    .press_sequentially(&sel, text, delay, self.action_timeout())
                    .await?;
                Ok(())
            },
        )
        .await
    }

    async fn set_checked_as(
        &self,
        method: &str,
        api: &str,
        selector: Selector,
        checked: bool,
        options: &fluxwright_cdp::CheckOptions,
    ) -> Result<()> {
        self.traced(
            "Page",
            method,
            api,
            || serde_json::json!({ "selector": selector.query, "checked": checked }),
            async {
                self.page
                    .set_checked(&selector, checked, options, self.action_timeout())
                    .await?;
                Ok(())
            },
        )
        .await
    }

    pub async fn set_checked(
        &self,
        selector: impl Into<Selector>,
        checked: bool,
        options: &fluxwright_cdp::CheckOptions,
    ) -> Result<()> {
        self.set_checked_as("setChecked", "page.setChecked", selector.into(), checked, options)
            .await
    }

    pub async fn check(
        &self,
        selector: impl Into<Selector>,
        options: &fluxwright_cdp::CheckOptions,
    ) -> Result<()> {
        self.set_checked_as("check", "page.check", selector.into(), true, options)
            .await
    }

    pub async fn uncheck(
        &self,
        selector: impl Into<Selector>,
        options: &fluxwright_cdp::CheckOptions,
    ) -> Result<()> {
        self.set_checked_as("uncheck", "page.uncheck", selector.into(), false, options)
            .await
    }

    pub async fn select_option(
        &self,
        selector: impl Into<Selector>,
        items: &[fluxwright_cdp::SelectEntry],
        force: bool,
    ) -> Result<Vec<String>> {
        let sel = selector.into();
        self.traced(
            "Page",
            "selectOption",
            "page.selectOption",
            || serde_json::json!({ "selector": sel.query }),
            async {
                Ok(self
                    .page
                    .select_option(&sel, items, force, self.action_timeout())
                    .await?)
            },
        )
        .await
    }

    pub async fn set_input_files(
        &self,
        selector: impl Into<Selector>,
        files: &fluxwright_cdp::FileInput,
    ) -> Result<()> {
        let sel = selector.into();
        self.traced(
            "Page",
            "setInputFiles",
            "page.setInputFiles",
            || serde_json::json!({ "selector": sel.query }),
            async {
                self.page
                    .set_input_files(&sel, files, self.action_timeout())
                    .await?;
                Ok(())
            },
        )
        .await
    }

    pub async fn tap(
        &self,
        selector: impl Into<Selector>,
        options: &fluxwright_cdp::HoverOptions,
    ) -> Result<()> {
        let sel = selector.into();
        self.traced(
            "Page",
            "tap",
            "page.tap",
            || serde_json::json!({ "selector": sel.query }),
            async {
                self.page
                    .tap(&sel, options, self.action_timeout())
                    .await?;
                Ok(())
            },
        )
        .await
    }

    pub async fn mouse_move(&self, x: f64, y: f64, steps: u32) -> Result<()> {
        self.traced(
            "Mouse",
            "move",
            "mouse.move",
            || serde_json::json!({ "x": x, "y": y }),
            async { Ok(self.page.mouse_move(x, y, steps, self.action_timeout()).await?) },
        )
        .await
    }

    pub async fn mouse_down(
        &self,
        button: fluxwright_cdp::MouseButton,
        click_count: u32,
    ) -> Result<()> {
        self.traced(
            "Mouse",
            "down",
            "mouse.down",
            || serde_json::json!({}),
            async {
                Ok(self
                    .page
                    .mouse_down(button, click_count, self.action_timeout())
                    .await?)
            },
        )
        .await
    }

    pub async fn mouse_up(
        &self,
        button: fluxwright_cdp::MouseButton,
        click_count: u32,
    ) -> Result<()> {
        self.traced(
            "Mouse",
            "up",
            "mouse.up",
            || serde_json::json!({}),
            async {
                Ok(self
                    .page
                    .mouse_up(button, click_count, self.action_timeout())
                    .await?)
            },
        )
        .await
    }

    pub async fn mouse_click(
        &self,
        x: f64,
        y: f64,
        button: fluxwright_cdp::MouseButton,
        click_count: u32,
        delay: Option<Duration>,
    ) -> Result<()> {
        self.traced(
            "Mouse",
            "click",
            "mouse.click",
            || serde_json::json!({ "x": x, "y": y }),
            async {
                Ok(self
                    .page
                    .mouse_click(x, y, button, click_count, delay, self.action_timeout())
                    .await?)
            },
        )
        .await
    }

    pub async fn mouse_dblclick(
        &self,
        x: f64,
        y: f64,
        button: fluxwright_cdp::MouseButton,
        delay: Option<Duration>,
    ) -> Result<()> {
        self.traced(
            "Mouse",
            "dblclick",
            "mouse.dblclick",
            || serde_json::json!({ "x": x, "y": y }),
            async {
                Ok(self
                    .page
                    .mouse_dblclick(x, y, button, delay, self.action_timeout())
                    .await?)
            },
        )
        .await
    }

    pub async fn mouse_wheel(&self, delta_x: f64, delta_y: f64) -> Result<()> {
        self.traced(
            "Mouse",
            "wheel",
            "mouse.wheel",
            || serde_json::json!({ "deltaX": delta_x, "deltaY": delta_y }),
            async {
                Ok(self
                    .page
                    .mouse_wheel(delta_x, delta_y, self.action_timeout())
                    .await?)
            },
        )
        .await
    }

    pub async fn keyboard_down(&self, key: &str) -> Result<()> {
        self.traced(
            "Keyboard",
            "down",
            "keyboard.down",
            || serde_json::json!({ "key": key }),
            async { Ok(self.page.keyboard_down(key, self.action_timeout()).await?) },
        )
        .await
    }

    pub async fn keyboard_up(&self, key: &str) -> Result<()> {
        self.traced(
            "Keyboard",
            "up",
            "keyboard.up",
            || serde_json::json!({ "key": key }),
            async { Ok(self.page.keyboard_up(key, self.action_timeout()).await?) },
        )
        .await
    }

    pub async fn keyboard_press(&self, key: &str, delay: Option<Duration>) -> Result<()> {
        self.traced(
            "Keyboard",
            "press",
            "keyboard.press",
            || serde_json::json!({ "key": key }),
            async {
                Ok(self
                    .page
                    .keyboard_press(key, delay, self.action_timeout())
                    .await?)
            },
        )
        .await
    }

    pub async fn keyboard_type(&self, text: &str, delay: Option<Duration>) -> Result<()> {
        self.traced(
            "Keyboard",
            "type",
            "keyboard.type",
            || serde_json::json!({ "text": text }),
            async {
                Ok(self
                    .page
                    .keyboard_type(text, delay, self.action_timeout())
                    .await?)
            },
        )
        .await
    }

    pub async fn keyboard_insert_text(&self, text: &str) -> Result<()> {
        self.traced(
            "Keyboard",
            "insertText",
            "keyboard.insertText",
            || serde_json::json!({ "text": text }),
            async {
                Ok(self
                    .page
                    .keyboard_insert_text(text, self.action_timeout())
                    .await?)
            },
        )
        .await
    }

    pub async fn touchscreen_tap(&self, x: f64, y: f64) -> Result<()> {
        self.traced(
            "Touchscreen",
            "tap",
            "touchscreen.tap",
            || serde_json::json!({ "x": x, "y": y }),
            async {
                Ok(self
                    .page
                    .touchscreen_tap(x, y, self.action_timeout())
                    .await?)
            },
        )
        .await
    }

    pub async fn wait_for_selector(&self, selector: impl Into<Selector>) -> Result<()> {
        let sel = selector.into();
        self.traced(
            "Page",
            "waitForSelector",
            "page.waitForSelector",
            || serde_json::json!({ "selector": sel.query }),
            async {
                self.page
                    .wait_for_selector(&sel, self.action_timeout())
                    .await?;
                Ok(())
            },
        )
        .await
    }

    /// Waits for the selector to be attached, detached, visible or hidden.
    pub async fn wait_for_selector_state(
        &self,
        selector: impl Into<Selector>,
        state: fluxwright_cdp::WaitState,
    ) -> Result<()> {
        let sel = selector.into();
        self.traced(
            "Page",
            "waitForSelector",
            "page.waitForSelector",
            || serde_json::json!({ "selector": sel.query }),
            async {
                Ok(self
                    .page
                    .wait_for_selector_state(&sel, state, self.action_timeout())
                    .await?)
            },
        )
        .await
    }

    /// `textContent` of the first match, once it exists.
    pub async fn text_content(&self, selector: impl Into<Selector>) -> Result<Option<String>> {
        Ok(self
            .page
            .text_content(&selector.into(), self.action_timeout())
            .await?)
    }

    pub async fn inner_text(&self, selector: impl Into<Selector>) -> Result<String> {
        Ok(self
            .page
            .inner_text(&selector.into(), self.action_timeout())
            .await?)
    }

    pub async fn inner_html(&self, selector: impl Into<Selector>) -> Result<String> {
        Ok(self
            .page
            .inner_html(&selector.into(), self.action_timeout())
            .await?)
    }

    pub async fn input_value(&self, selector: impl Into<Selector>) -> Result<String> {
        Ok(self
            .page
            .input_value(&selector.into(), self.action_timeout())
            .await?)
    }

    pub async fn get_attribute(&self, selector: impl Into<Selector>, name: &str) -> Result<Option<String>> {
        Ok(self
            .page
            .get_attribute(&selector.into(), name, self.action_timeout())
            .await?)
    }

    pub async fn count(&self, selector: impl Into<Selector>) -> Result<usize> {
        Ok(self.page.count(&selector.into(), self.action_timeout()).await?)
    }

    pub async fn all_text_contents(&self, selector: impl Into<Selector>) -> Result<Vec<String>> {
        Ok(self
            .page
            .all_text_contents(&selector.into(), self.action_timeout())
            .await?)
    }

    pub async fn all_inner_texts(&self, selector: impl Into<Selector>) -> Result<Vec<String>> {
        Ok(self
            .page
            .all_inner_texts(&selector.into(), self.action_timeout())
            .await?)
    }

    pub async fn is_visible(&self, selector: impl Into<Selector>) -> Result<bool> {
        Ok(self
            .page
            .is_visible(&selector.into(), self.action_timeout())
            .await?)
    }

    pub async fn is_hidden(&self, selector: impl Into<Selector>) -> Result<bool> {
        Ok(self
            .page
            .is_hidden(&selector.into(), self.action_timeout())
            .await?)
    }

    pub async fn is_enabled(&self, selector: impl Into<Selector>) -> Result<bool> {
        Ok(self
            .page
            .is_enabled(&selector.into(), self.action_timeout())
            .await?)
    }

    pub async fn is_disabled(&self, selector: impl Into<Selector>) -> Result<bool> {
        Ok(self
            .page
            .is_disabled(&selector.into(), self.action_timeout())
            .await?)
    }

    pub async fn is_checked(&self, selector: impl Into<Selector>) -> Result<bool> {
        Ok(self
            .page
            .is_checked(&selector.into(), self.action_timeout())
            .await?)
    }

    pub async fn is_editable(&self, selector: impl Into<Selector>) -> Result<bool> {
        Ok(self
            .page
            .is_editable(&selector.into(), self.action_timeout())
            .await?)
    }

    pub fn locator(&self, selector: impl Into<String>) -> Locator<'_> {
        Locator {
            lease: self,
            selector: Selector::new(selector.into()).strict(true),
        }
    }

    /// Like Playwright's `getByText`: case-insensitive substring, or exact.
    pub fn get_by_text(&self, text: &str, exact: bool) -> Locator<'_> {
        self.locator(Selector::text(text, exact))
    }

    /// Like Playwright's `getByRole`: implicit and explicit ARIA roles, accessible name as a
    /// case-insensitive substring, or exact. Hidden elements never match.
    pub fn get_by_role(&self, role: &str, name: Option<&str>, exact: bool) -> Locator<'_> {
        self.locator(Selector::role(role, name, exact))
    }

    pub fn get_by_role_with(&self, role: &str, opts: &fluxwright_cdp::RoleOptions) -> Locator<'_> {
        self.locator(Selector::role_with(role, opts))
    }

    /// Like Playwright's `getByLabel`: the text of a `<label>`, `aria-labelledby` or `aria-label`.
    pub fn get_by_label(&self, text: &str, exact: bool) -> Locator<'_> {
        self.locator(Selector::label(text, exact))
    }

    pub fn get_by_placeholder(&self, text: &str, exact: bool) -> Locator<'_> {
        self.locator(Selector::placeholder(text, exact))
    }

    pub fn get_by_alt_text(&self, text: &str, exact: bool) -> Locator<'_> {
        self.locator(Selector::alt_text(text, exact))
    }

    pub fn get_by_title(&self, text: &str, exact: bool) -> Locator<'_> {
        self.locator(Selector::title(text, exact))
    }

    /// `data-testid`, exact.
    pub fn get_by_test_id(&self, id: &str) -> Locator<'_> {
        self.locator(Selector::test_id(id))
    }

    /// Fake clock controls for this page (`Date`, `performance.now`, `setTimeout`, `setInterval`, `requestAnimationFrame`).
    pub fn clock(&self) -> Clock<'_> {
        Clock { lease: self }
    }

    /// Hands every request of this page, its popups and iframes to the returned channel to
    /// fulfill, change or abort (Playwright's `page.route`). Call it before navigating. A request
    /// dropped unanswered continues; blocked resource types stay blocked.
    pub async fn intercept(&self) -> Result<tokio::sync::mpsc::UnboundedReceiver<InterceptedRequest>> {
        Ok(self.page.intercept(self.action_timeout()).await?)
    }

    /// Registers a Playwright-style route handler on this page (`page.route`). Newest handler runs first.
    pub async fn route<F, Fut>(&self, pattern: impl Into<UrlPattern>, handler: F) -> Result<()>
    where
        F: Fn(Route) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Result<()>> + Send + 'static,
    {
        self.route_with(pattern, handler, RouteOptions::default()).await
    }

    /// Registers a route handler with [`RouteOptions`] (such as `times`).
    pub async fn route_with<F, Fut>(
        &self,
        pattern: impl Into<UrlPattern>,
        handler: F,
        options: RouteOptions,
    ) -> Result<()>
    where
        F: Fn(Route) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Result<()>> + Send + 'static,
    {
        if self.is_closed() {
            return Err(Error::Other("page is closed".into()));
        }
        {
            let mut started = self.route_start.lock().await;
            if !*started {
                let rx = self.intercept().await?;
                let task = spawn_route_dispatcher(
                    Some(self.route_entries.clone()),
                    self.context.route_entries.clone(),
                    rx,
                );
                self.route_tasks.lock().unwrap().push(task);
                *started = true;
            }
        }
        push_route_entry(&self.route_entries, pattern.into(), handler, options);
        Ok(())
    }

    /// Removes page-level handlers registered for the matching glob `pattern`.
    pub async fn unroute(&self, pattern: &str) {
        remove_route_entries(&self.route_entries, pattern);
    }

    /// The next download this page, its popups or iframes finished, oldest first, waiting up to
    /// `timeout`. Downloads that finished before the call are returned too, so starting the
    /// download first is fine. The file is deleted when the page closes: `save_as` it first.
    pub async fn wait_for_download(&self, timeout: Duration) -> Result<Download> {
        self.downloads.next(timeout).await
    }

    /// A handle on this page's finished downloads that outlives borrows of the lease, for
    /// callers that must not hold the page while they wait.
    pub fn downloads(&self) -> Downloads {
        self.downloads.clone()
    }

    /// Every console message and uncaught error from now on, as they happen.
    /// Network events of this page, its popups and iframes from now on: requests, responses,
    /// finished and failed requests.
    pub fn subscribe_network(&self) -> tokio::sync::mpsc::UnboundedReceiver<fluxwright_cdp::NetEvent> {
        self.log.lock().unwrap().subscribe_network()
    }

    /// WebSocket creation, frame, error, and close events from this page and its frames.
    pub fn subscribe_websockets(&self) -> tokio::sync::mpsc::UnboundedReceiver<WebSocketEvent> {
        self.log.lock().unwrap().subscribe_websockets()
    }

    /// Dedicated Web Workers currently active on this page.
    pub fn workers(&self) -> Vec<Worker> {
        self.page.workers()
    }

    /// Looks up a dedicated Web Worker on this page by its CDP target ID.
    pub fn worker(&self, target_id: &str) -> Option<Worker> {
        self.page.worker(target_id)
    }

    /// Lifecycle and console events for dedicated Web Workers on this page.
    pub fn subscribe_workers(&self) -> tokio::sync::mpsc::UnboundedReceiver<WorkerEvent> {
        self.page.subscribe_workers()
    }

    /// Starts recording HTTP traffic on this page (and its frames) to `opts.path`, flushed on close.
    pub async fn start_har_recording(&self, opts: RecordHarOptions) -> Result<()> {
        if self.is_closed() {
            return Err(Error::Other("page is closed".into()));
        }
        let rec = self
            .context
            .browser
            .start_har_recording(
                &self.context.context,
                Some(self.page.target_id.0.clone()),
                opts,
            )
            .await;
        self.har_recorders.lock().unwrap().push(rec);
        Ok(())
    }

    /// Serves matching requests on this page from a HAR 1.2 file (or records/updates it when `opts.update` is true).
    pub async fn route_from_har(
        &self,
        path: impl AsRef<std::path::Path>,
        opts: &HarRouteOptions,
    ) -> Result<()> {
        if self.is_closed() {
            return Err(Error::Other("page is closed".into()));
        }
        if opts.update {
            return self
                .start_har_recording(RecordHarOptions {
                    path: path.as_ref().to_path_buf(),
                    omit_content: false,
                    content: Some(opts.update_content),
                    mode: opts.update_mode,
                    url_filter: opts.url.clone(),
                })
                .await;
        }
        let router = Arc::new(HarRouter::from_file(path)?);
        let rx = self.intercept().await?;
        let task = spawn_har_route_handler(router, opts.url.clone(), opts.not_found, rx);
        self.har_route_tasks.lock().unwrap().push(task);
        Ok(())
    }

    /// Dialogs of this page, its popups and iframes from now on, for the job to answer instead
    /// of the engine dismissing them. The newest subscriber gets each one.
    pub fn subscribe_dialogs(&self) -> tokio::sync::mpsc::UnboundedReceiver<fluxwright_cdp::Dialog> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        self.dialog_listeners.lock().unwrap().push(tx);
        rx
    }

    /// File chooser dialogs intercepted on this page and its frames from now on.
    pub fn subscribe_file_choosers(
        &self,
    ) -> tokio::sync::mpsc::UnboundedReceiver<fluxwright_cdp::FileChooser> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        self.file_chooser_listeners.lock().unwrap().push(tx);
        rx
    }

    /// Invocations of exposed bindings on this page and its frames from now on.
    pub fn subscribe_bindings(
        &self,
    ) -> tokio::sync::mpsc::UnboundedReceiver<fluxwright_cdp::BindingCall> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        self.binding_listeners.lock().unwrap().push(tx);
        rx
    }

    pub async fn add_init_script(&self, source: &str) -> Result<()> {
        Ok(self.page.add_init_script(source, self.action_timeout()).await?)
    }

    pub async fn add_init_script_path(&self, path: impl AsRef<std::path::Path>) -> Result<()> {
        let p = path.as_ref();
        let source = tokio::fs::read_to_string(p)
            .await
            .map_err(|e| Error::Other(format!("add_init_script {}: {e}", p.display())))?;
        self.add_init_script(&source).await
    }

    pub async fn expose_binding(&self, name: &str) -> Result<()> {
        Ok(self.page.expose_binding(name, self.action_timeout()).await?)
    }

    pub async fn expose_function<F, Fut>(&self, name: &str, handler: F) -> Result<()>
    where
        F: Fn(Vec<serde_json::Value>) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = std::result::Result<serde_json::Value, String>>
            + Send
            + 'static,
    {
        if !self.binding_dispatch_started.load(Ordering::SeqCst) {
            spawn_binding_dispatcher(
                &self.binding_dispatch_started,
                self.binding_handlers.clone(),
                self.subscribe_bindings(),
            );
        }
        let handler: BindingHandler = Arc::new(move |args| Box::pin(handler(args)));
        {
            let mut handlers = self.binding_handlers.lock().unwrap();
            if handlers.contains_key(name) {
                return Err(Error::Other(format!(
                    "Function \"{name}\" has been already registered"
                )));
            }
            handlers.insert(name.to_string(), handler);
        }
        if let Err(err) = self.expose_binding(name).await {
            self.binding_handlers.lock().unwrap().remove(name);
            return Err(err);
        }
        Ok(())
    }

    pub async fn add_script_tag(&self, opts: &fluxwright_cdp::ScriptTagOptions) -> Result<()> {
        Ok(self.page.add_script_tag(opts, self.action_timeout()).await?)
    }

    pub async fn add_script_tag_path(
        &self,
        path: impl AsRef<std::path::Path>,
        script_type: Option<&str>,
    ) -> Result<()> {
        let p = path.as_ref();
        let mut content = tokio::fs::read_to_string(p)
            .await
            .map_err(|e| Error::Other(format!("add_script_tag {}: {e}", p.display())))?;
        content.push_str(&format!("\n//# sourceURL={}", p.display()));
        self.add_script_tag(&fluxwright_cdp::ScriptTagOptions {
            url: None,
            content: Some(content),
            script_type: script_type.map(str::to_owned),
        })
        .await
    }

    pub async fn add_style_tag(&self, opts: &fluxwright_cdp::StyleTagOptions) -> Result<()> {
        Ok(self.page.add_style_tag(opts, self.action_timeout()).await?)
    }

    pub async fn add_style_tag_path(&self, path: impl AsRef<std::path::Path>) -> Result<()> {
        let p = path.as_ref();
        let mut content = tokio::fs::read_to_string(p)
            .await
            .map_err(|e| Error::Other(format!("add_style_tag {}: {e}", p.display())))?;
        content.push_str(&format!("\n/*# sourceURL={} */", p.display()));
        self.add_style_tag(&fluxwright_cdp::StyleTagOptions {
            url: None,
            content: Some(content),
        })
        .await
    }

    pub fn subscribe_logs(&self) -> tokio::sync::mpsc::UnboundedReceiver<LogEntry> {
        self.log.lock().unwrap().subscribe()
    }

    /// Console messages so far from this page, its popups and its iframes (the last 1000).
    pub fn console_messages(&self) -> Vec<ConsoleMessage> {
        self.log.lock().unwrap().console.iter().cloned().collect()
    }

    /// Exceptions nothing caught so far (the last 1000). Fail a job on them with
    /// `assert!(page.page_errors().is_empty())`.
    pub fn page_errors(&self) -> Vec<PageError> {
        self.log.lock().unwrap().errors.iter().cloned().collect()
    }

    /// The first match's box relative to the viewport, without scrolling; `None` when hidden.
    pub async fn bounding_box(&self, selector: impl Into<Selector>) -> Result<Option<BoundingBox>> {
        Ok(self.page.bounding_box(&selector.into(), self.action_timeout()).await?)
    }

    /// Calls `function` (JavaScript source taking the element and `arg`) on the first match.
    pub async fn evaluate_on(&self, selector: impl Into<Selector>, function: &str, arg: Option<serde_json::Value>) -> Result<serde_json::Value> {
        let sel = selector.into();
        self.traced(
            "Page",
            "evalOnSelector",
            "page.evalOnSelector",
            || serde_json::json!({ "selector": sel.to_string(), "expression": function }),
            async {
                Ok(self
                    .page
                    .evaluate_on(&sel, function, arg, self.action_timeout())
                    .await?)
            },
        )
        .await
    }

    /// PNG of the first match, once it is visible and still.
    pub async fn element_screenshot(&self, selector: impl Into<Selector>) -> Result<Vec<u8>> {
        self.element_screenshot_with(selector, &ScreenshotOptions::default())
            .await
    }

    /// PNG or JPEG of the first match, once it is visible and still.
    pub async fn element_screenshot_with(
        &self,
        selector: impl Into<Selector>,
        options: &ScreenshotOptions,
    ) -> Result<Vec<u8>> {
        let sel = selector.into();
        self.traced(
            "Page",
            "screenshot",
            "page.screenshot",
            || serde_json::json!({ "selector": sel.to_string() }),
            async {
                Ok(self
                    .page
                    .element_screenshot(&sel, options, self.action_timeout())
                    .await?)
            },
        )
        .await
    }

    /// Enters an iframe (same- or cross-origin) by CSS selector.
    pub fn frame_locator(&self, iframe: &str) -> FrameLocator<'_> {
        FrameLocator {
            lease: self,
            frames: vec![iframe.to_string()],
        }
    }

    /// Persists current `storageState` to the job's named profile once, if configured.
    pub async fn save_profile(&self) -> Result<()> {
        let Some((store, name)) = self.context.profile_target.lock().unwrap().take() else {
            return Ok(());
        };
        if self.is_closed() {
            return Ok(());
        }
        let state = self.storage_state().await?;
        tokio::task::spawn_blocking(move || store.save(&name, &state))
            .await
            .map_err(|e| Error::Other(e.to_string()))??;
        Ok(())
    }

    pub async fn close(mut self) -> Result<()> {
        let save_res = self.save_profile().await;
        self.release(save_res.is_ok()).await;
        save_res
    }

    /// Releases the page and reports `completed = false` without saving any profile.
    pub async fn discard(mut self) {
        self.mark_failed();
        self.release(false).await;
    }

    /// Marks the context lease as failed so drop-based cleanup records proxy and adaptive backoff.
    pub fn mark_failed(&self) {
        self.context.profile_target.lock().unwrap().take();
        self.context.failed.store(true, Ordering::SeqCst);
    }

    async fn release(&mut self, completed: bool) {
        self.log_task.abort();
        self.popup_task.abort();
        for handle in self.har_route_tasks.lock().unwrap().drain(..) {
            handle.abort();
        }
        for handle in self.route_tasks.lock().unwrap().drain(..) {
            handle.abort();
        }
        if self.released {
            return;
        }
        self.released = true;
        self.context.tracing.unregister_page(self.lease_id);
        let recorders = std::mem::take(&mut *self.har_recorders.lock().unwrap());
        for rec in recorders {
            let _ = rec.finish().await;
        }
        let video_rec = self.video_recorder.lock().unwrap().take();
        if let Some(rec) = video_rec {
            let _ = rec.finish().await;
            self.context
                .video_recorders
                .lock()
                .unwrap()
                .retain(|r| !r.is_finished());
        }
        if self.owns_context {
            self.context.dispose(completed).await;
        } else if !self.context.is_disposed() {
            let flushers: Vec<_> = self
                .context
                .har_recorders
                .lock()
                .unwrap()
                .iter()
                .map(|r| r.flusher())
                .collect();
            for f in flushers {
                f.flush_session(Some(self.page.session_id.0.clone())).await;
            }
            let _ = self.context.browser.close_target(&self.page.target_id).await;
        }
    }
}

impl Drop for PageLease {
    fn drop(&mut self) {
        self.log_task.abort();
        self.popup_task.abort();
        for handle in self.har_route_tasks.lock().unwrap().drain(..) {
            handle.abort();
        }
        for handle in self.route_tasks.lock().unwrap().drain(..) {
            handle.abort();
        }
        if self.released {
            return;
        }
        self.released = true;
        self.context.tracing.unregister_page(self.lease_id);
        let completed = !self.context.failed.load(Ordering::SeqCst);
        let recorders = std::mem::take(&mut *self.har_recorders.lock().unwrap());
        let video_rec = self.video_recorder.lock().unwrap().take();
        if self.owns_context && recorders.is_empty() && video_rec.is_none() {
            self.context.dispose_in_background(completed);
        } else {
            let owns = self.owns_context;
            let ctx = Arc::clone(&self.context);
            let target_id = self.page.target_id.clone();
            let sid = self.page.session_id.0.clone();
            let ctx_flushers: Vec<_> = if !owns {
                ctx.har_recorders
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|r| r.flusher())
                    .collect()
            } else {
                Vec::new()
            };
            self.runtime.spawn(async move {
                for rec in recorders {
                    let _ = rec.finish().await;
                }
                if let Some(rec) = video_rec {
                    let _ = rec.finish().await;
                    ctx.video_recorders
                        .lock()
                        .unwrap()
                        .retain(|r| !r.is_finished());
                }
                if owns {
                    ctx.dispose(completed).await;
                } else if !ctx.is_disposed() {
                    for f in ctx_flushers {
                        f.flush_session(Some(sid.clone())).await;
                    }
                    let _ = ctx.browser.close_target(&target_id).await;
                }
            });
        }
    }
}

/// A page's finished downloads, oldest first.
#[derive(Clone)]
pub struct Downloads(Arc<Mutex<DownloadQueue>>);

impl Downloads {
    /// The next finished download, waiting up to `timeout`.
    pub async fn next(&self, timeout: Duration) -> Result<Download> {
        let mut queue = self.0.lock().await;
        match tokio::time::timeout(timeout, queue.recv()).await {
            Ok(Some(Ok(download))) => Ok(download),
            Ok(Some(Err(reason))) => Err(Error::Other(reason)),
            Ok(None) => Err(Error::Other("page closed".into())),
            Err(_) => Err(Error::Other(format!("no download finished within {} ms", timeout.as_millis()))),
        }
    }

    /// Drops downloads that already finished, so the next [`next`](Self::next) waits for a new
    /// one (Python's `expect_download` block). Their files stay until the page closes. A wait
    /// already running holds the queue for its whole timeout and takes the old ones itself, so
    /// then nothing is dropped rather than waiting for it.
    pub async fn clear(&self) {
        if let Ok(mut queue) = self.0.try_lock() {
            while queue.try_recv().is_ok() {}
        }
    }
}

pub struct Locator<'a> {
    lease: &'a PageLease,
    selector: Selector,
}

impl<'a> Locator<'a> {
    pub(crate) fn lease(&self) -> &'a PageLease {
        self.lease
    }

    pub fn selector(&self) -> &Selector {
        &self.selector
    }

    fn then(&self, part: &str) -> Locator<'a> {
        Locator { lease: self.lease, selector: self.selector.then(part) }
    }

    /// The `n`th match (0-based; negative counts from the end).
    pub fn nth(&self, n: i32) -> Locator<'a> {
        self.then(&format!("nth={n}"))
    }

    pub fn first(&self) -> Locator<'a> {
        self.nth(0)
    }

    pub fn last(&self) -> Locator<'a> {
        self.nth(-1)
    }

    /// Like Playwright's `filter({ hasText })`: keeps matches containing `text`, case-insensitive.
    pub fn filter_has_text(&self, text: &str) -> Locator<'a> {
        self.then(&Selector::has_text(text))
    }

    pub fn filter_has_text_regex(&self, pattern: &str, flags: &str) -> Locator<'a> {
        self.then(&Selector::has_text_regex(pattern, flags))
    }

    pub fn filter_has_not_text(&self, text: &str) -> Locator<'a> {
        self.then(&Selector::has_not_text(text))
    }

    pub fn filter_has_not_text_regex(&self, pattern: &str, flags: &str) -> Locator<'a> {
        self.then(&Selector::has_not_text_regex(pattern, flags))
    }

    pub fn filter_has(&self, inner: &Locator<'_>) -> Locator<'a> {
        Locator { lease: self.lease, selector: self.selector.has(&inner.selector) }
    }

    pub fn filter_has_not(&self, inner: &Locator<'_>) -> Locator<'a> {
        Locator { lease: self.lease, selector: self.selector.has_not(&inner.selector) }
    }

    pub fn and(&self, other: &Locator<'_>) -> Locator<'a> {
        Locator { lease: self.lease, selector: self.selector.and(&other.selector) }
    }

    pub fn or(&self, other: &Locator<'_>) -> Locator<'a> {
        Locator { lease: self.lease, selector: self.selector.or(&other.selector) }
    }

    /// Searches inside this locator's matches.
    pub fn locator(&self, query: &str) -> Locator<'a> {
        self.then(query)
    }

    pub fn get_by_text(&self, text: &str, exact: bool) -> Locator<'a> {
        self.then(&Selector::text(text, exact))
    }

    pub fn get_by_role(&self, role: &str, name: Option<&str>, exact: bool) -> Locator<'a> {
        self.then(&Selector::role(role, name, exact))
    }

    pub fn get_by_role_with(&self, role: &str, opts: &fluxwright_cdp::RoleOptions) -> Locator<'a> {
        self.then(&Selector::role_with(role, opts))
    }

    pub fn get_by_label(&self, text: &str, exact: bool) -> Locator<'a> {
        self.then(&Selector::label(text, exact))
    }

    pub fn get_by_placeholder(&self, text: &str, exact: bool) -> Locator<'a> {
        self.then(&Selector::placeholder(text, exact))
    }

    pub fn get_by_alt_text(&self, text: &str, exact: bool) -> Locator<'a> {
        self.then(&Selector::alt_text(text, exact))
    }

    pub fn get_by_title(&self, text: &str, exact: bool) -> Locator<'a> {
        self.then(&Selector::title(text, exact))
    }

    pub fn get_by_test_id(&self, id: &str) -> Locator<'a> {
        self.then(&Selector::test_id(id))
    }

    async fn traced<F, P, T>(
        &self,
        method_name: &str,
        api_name: &str,
        params: P,
        fut: F,
    ) -> Result<T>
    where
        F: std::future::Future<Output = Result<T>>,
        P: FnOnce() -> serde_json::Value,
    {
        if !self.lease.context.tracing.is_active() {
            return fut.await;
        }
        self.lease
            .context
            .tracing
            .trace_action(
                &self.lease.page.target_id.0,
                "Locator",
                method_name,
                api_name,
                params,
                fut,
            )
            .await
    }

    /// PNG of the element, once it is visible and still.
    pub async fn screenshot(&self) -> Result<Vec<u8>> {
        self.screenshot_with(&ScreenshotOptions::default()).await
    }

    /// PNG or JPEG of the element, once it is visible and still.
    pub async fn screenshot_with(&self, options: &ScreenshotOptions) -> Result<Vec<u8>> {
        self.traced(
            "screenshot",
            "locator.screenshot",
            || serde_json::json!({ "selector": self.selector.to_string() }),
            async {
                Ok(self
                    .lease
                    .page
                    .element_screenshot(&self.selector, options, self.lease.action_timeout())
                    .await?)
            },
        )
        .await
    }

    pub async fn bounding_box(&self) -> Result<Option<BoundingBox>> {
        self.lease.bounding_box(&self.selector).await
    }

    /// `function` is JavaScript source taking the element and `arg`, e.g. `(el, n) => el.value.length > n`.
    pub async fn evaluate(&self, function: &str, arg: Option<serde_json::Value>) -> Result<serde_json::Value> {
        self.traced(
            "evaluate",
            "locator.evaluate",
            || serde_json::json!({ "selector": self.selector.to_string(), "expression": function }),
            async {
                Ok(self
                    .lease
                    .page
                    .evaluate_on(&self.selector, function, arg, self.lease.action_timeout())
                    .await?)
            },
        )
        .await
    }

    pub async fn click(&self) -> Result<()> {
        self.traced(
            "click",
            "locator.click",
            || serde_json::json!({ "selector": self.selector.to_string() }),
            async {
                self.lease
                    .page
                    .click(&self.selector, self.lease.action_timeout())
                    .await?;
                Ok(())
            },
        )
        .await
    }

    pub async fn click_with(&self, options: &fluxwright_cdp::ClickOptions) -> Result<()> {
        self.traced(
            "click",
            "locator.click",
            || serde_json::json!({ "selector": self.selector.to_string() }),
            async {
                self.lease
                    .page
                    .click_with(&self.selector, options, self.lease.action_timeout())
                    .await?;
                Ok(())
            },
        )
        .await
    }

    pub async fn dblclick(&self) -> Result<()> {
        self.dblclick_with(&fluxwright_cdp::ClickOptions::default())
            .await
    }

    pub async fn dblclick_with(&self, options: &fluxwright_cdp::ClickOptions) -> Result<()> {
        self.traced(
            "dblclick",
            "locator.dblclick",
            || serde_json::json!({ "selector": self.selector.to_string() }),
            async {
                self.lease
                    .page
                    .dblclick(&self.selector, options, self.lease.action_timeout())
                    .await?;
                Ok(())
            },
        )
        .await
    }

    pub async fn hover(&self) -> Result<()> {
        self.hover_with(&fluxwright_cdp::HoverOptions::default())
            .await
    }

    pub async fn hover_with(&self, options: &fluxwright_cdp::HoverOptions) -> Result<()> {
        self.traced(
            "hover",
            "locator.hover",
            || serde_json::json!({ "selector": self.selector.to_string() }),
            async {
                self.lease
                    .page
                    .hover(&self.selector, options, self.lease.action_timeout())
                    .await?;
                Ok(())
            },
        )
        .await
    }

    pub async fn focus(&self) -> Result<()> {
        self.traced(
            "focus",
            "locator.focus",
            || serde_json::json!({ "selector": self.selector.to_string() }),
            async {
                self.lease
                    .page
                    .focus(&self.selector, self.lease.action_timeout())
                    .await?;
                Ok(())
            },
        )
        .await
    }

    pub async fn blur(&self) -> Result<()> {
        self.traced(
            "blur",
            "locator.blur",
            || serde_json::json!({ "selector": self.selector.to_string() }),
            async {
                self.lease
                    .page
                    .blur(&self.selector, self.lease.action_timeout())
                    .await?;
                Ok(())
            },
        )
        .await
    }

    pub async fn scroll_into_view_if_needed(&self) -> Result<()> {
        self.traced(
            "scrollIntoViewIfNeeded",
            "locator.scrollIntoViewIfNeeded",
            || serde_json::json!({ "selector": self.selector.to_string() }),
            async {
                self.lease
                    .page
                    .scroll_into_view_if_needed(&self.selector, self.lease.action_timeout())
                    .await?;
                Ok(())
            },
        )
        .await
    }

    pub async fn drag_to(&self, target: &Locator<'_>) -> Result<()> {
        self.drag_to_with(target, false).await
    }

    pub async fn drag_to_with(&self, target: &Locator<'_>, force: bool) -> Result<()> {
        self.traced(
            "dragTo",
            "locator.dragTo",
            || {
                serde_json::json!({
                    "selector": self.selector.to_string(),
                    "target": target.selector.to_string(),
                })
            },
            async {
                self.lease
                    .page
                    .drag_and_drop(
                        &self.selector,
                        &target.selector,
                        force,
                        self.lease.action_timeout(),
                    )
                    .await?;
                Ok(())
            },
        )
        .await
    }

    pub async fn fill(&self, value: &str) -> Result<()> {
        self.traced(
            "fill",
            "locator.fill",
            || serde_json::json!({ "selector": self.selector.to_string(), "value": value }),
            async {
                self.lease
                    .page
                    .fill(&self.selector, value, self.lease.action_timeout())
                    .await?;
                Ok(())
            },
        )
        .await
    }

    pub async fn clear(&self, force: bool) -> Result<()> {
        self.traced(
            "clear",
            "locator.clear",
            || serde_json::json!({ "selector": self.selector.to_string() }),
            async {
                self.lease
                    .page
                    .fill_with(&self.selector, "", force, self.lease.action_timeout())
                    .await?;
                Ok(())
            },
        )
        .await
    }

    pub async fn press(&self, key: &str, delay: Option<Duration>) -> Result<()> {
        self.traced(
            "press",
            "locator.press",
            || serde_json::json!({ "selector": self.selector.to_string(), "key": key }),
            async {
                self.lease
                    .page
                    .press(&self.selector, key, delay, self.lease.action_timeout())
                    .await?;
                Ok(())
            },
        )
        .await
    }

    pub async fn press_sequentially(&self, text: &str, delay: Option<Duration>) -> Result<()> {
        self.traced(
            "pressSequentially",
            "locator.pressSequentially",
            || serde_json::json!({ "selector": self.selector.to_string(), "text": text }),
            async {
                self.lease
                    .page
                    .press_sequentially(&self.selector, text, delay, self.lease.action_timeout())
                    .await?;
                Ok(())
            },
        )
        .await
    }

    pub async fn check(&self) -> Result<()> {
        self.check_with(&fluxwright_cdp::CheckOptions::default())
            .await
    }

    pub async fn check_with(&self, options: &fluxwright_cdp::CheckOptions) -> Result<()> {
        self.traced(
            "check",
            "locator.check",
            || serde_json::json!({ "selector": self.selector.to_string(), "checked": true }),
            async {
                self.lease
                    .page
                    .set_checked(&self.selector, true, options, self.lease.action_timeout())
                    .await?;
                Ok(())
            },
        )
        .await
    }

    pub async fn uncheck(&self) -> Result<()> {
        self.uncheck_with(&fluxwright_cdp::CheckOptions::default())
            .await
    }

    pub async fn uncheck_with(&self, options: &fluxwright_cdp::CheckOptions) -> Result<()> {
        self.traced(
            "uncheck",
            "locator.uncheck",
            || serde_json::json!({ "selector": self.selector.to_string(), "checked": false }),
            async {
                self.lease
                    .page
                    .set_checked(&self.selector, false, options, self.lease.action_timeout())
                    .await?;
                Ok(())
            },
        )
        .await
    }

    pub async fn set_checked(
        &self,
        checked: bool,
        options: &fluxwright_cdp::CheckOptions,
    ) -> Result<()> {
        self.traced(
            "setChecked",
            "locator.setChecked",
            || serde_json::json!({ "selector": self.selector.to_string(), "checked": checked }),
            async {
                self.lease
                    .page
                    .set_checked(&self.selector, checked, options, self.lease.action_timeout())
                    .await?;
                Ok(())
            },
        )
        .await
    }

    pub async fn select_option(
        &self,
        items: &[fluxwright_cdp::SelectEntry],
        force: bool,
    ) -> Result<Vec<String>> {
        self.traced(
            "selectOption",
            "locator.selectOption",
            || serde_json::json!({ "selector": self.selector.to_string() }),
            async {
                Ok(self
                    .lease
                    .page
                    .select_option(&self.selector, items, force, self.lease.action_timeout())
                    .await?)
            },
        )
        .await
    }

    pub async fn set_input_files(&self, files: &fluxwright_cdp::FileInput) -> Result<()> {
        self.traced(
            "setInputFiles",
            "locator.setInputFiles",
            || serde_json::json!({ "selector": self.selector.to_string() }),
            async {
                self.lease
                    .page
                    .set_input_files(&self.selector, files, self.lease.action_timeout())
                    .await?;
                Ok(())
            },
        )
        .await
    }

    pub async fn tap(&self) -> Result<()> {
        self.tap_with(&fluxwright_cdp::HoverOptions::default())
            .await
    }

    pub async fn tap_with(&self, options: &fluxwright_cdp::HoverOptions) -> Result<()> {
        self.traced(
            "tap",
            "locator.tap",
            || serde_json::json!({ "selector": self.selector.to_string() }),
            async {
                self.lease
                    .page
                    .tap(&self.selector, options, self.lease.action_timeout())
                    .await?;
                Ok(())
            },
        )
        .await
    }

    /// Waits until visible.
    pub async fn wait(&self) -> Result<()> {
        self.traced(
            "waitFor",
            "locator.waitFor",
            || serde_json::json!({ "selector": self.selector.to_string() }),
            async {
                self.lease
                    .page
                    .wait_for_selector(&self.selector, self.lease.action_timeout())
                    .await?;
                Ok(())
            },
        )
        .await
    }

    pub async fn text_content(&self) -> Result<Option<String>> {
        self.lease.text_content(&self.selector).await
    }

    pub async fn inner_text(&self) -> Result<String> {
        self.lease.inner_text(&self.selector).await
    }

    pub async fn inner_html(&self) -> Result<String> {
        self.lease.inner_html(&self.selector).await
    }

    pub async fn input_value(&self) -> Result<String> {
        self.lease.input_value(&self.selector).await
    }

    pub async fn get_attribute(&self, name: &str) -> Result<Option<String>> {
        self.lease.get_attribute(&self.selector, name).await
    }

    pub async fn count(&self) -> Result<usize> {
        self.lease.count(&self.selector).await
    }

    pub async fn all(&self) -> Result<Vec<Locator<'a>>> {
        let n = self.count().await?;
        Ok((0..n).map(|i| self.nth(i as i32)).collect())
    }

    pub async fn all_text_contents(&self) -> Result<Vec<String>> {
        self.lease.all_text_contents(&self.selector).await
    }

    pub async fn all_inner_texts(&self) -> Result<Vec<String>> {
        self.lease.all_inner_texts(&self.selector).await
    }

    pub async fn is_visible(&self) -> Result<bool> {
        self.lease.is_visible(&self.selector).await
    }

    pub async fn is_hidden(&self) -> Result<bool> {
        self.lease.is_hidden(&self.selector).await
    }

    pub async fn is_enabled(&self) -> Result<bool> {
        self.lease.is_enabled(&self.selector).await
    }

    pub async fn is_disabled(&self) -> Result<bool> {
        self.lease.is_disabled(&self.selector).await
    }

    pub async fn is_checked(&self) -> Result<bool> {
        self.lease.is_checked(&self.selector).await
    }

    pub async fn is_editable(&self) -> Result<bool> {
        self.lease.is_editable(&self.selector).await
    }
}

/// An iframe (or a chain of nested ones) to find elements in. Cross-origin frames work too.
pub struct FrameLocator<'a> {
    lease: &'a PageLease,
    frames: Vec<String>,
}

impl<'a> FrameLocator<'a> {
    pub fn locator(&self, query: impl Into<String>) -> Locator<'a> {
        Locator {
            lease: self.lease,
            selector: Selector {
                frames: self.frames.clone(),
                query: query.into(),
                strict: true,
            },
        }
    }

    pub fn get_by_text(&self, text: &str, exact: bool) -> Locator<'a> {
        self.locator(Selector::text(text, exact))
    }

    pub fn get_by_role(&self, role: &str, name: Option<&str>, exact: bool) -> Locator<'a> {
        self.locator(Selector::role(role, name, exact))
    }

    pub fn get_by_role_with(&self, role: &str, opts: &fluxwright_cdp::RoleOptions) -> Locator<'a> {
        self.locator(Selector::role_with(role, opts))
    }

    pub fn get_by_label(&self, text: &str, exact: bool) -> Locator<'a> {
        self.locator(Selector::label(text, exact))
    }

    pub fn get_by_placeholder(&self, text: &str, exact: bool) -> Locator<'a> {
        self.locator(Selector::placeholder(text, exact))
    }

    pub fn get_by_alt_text(&self, text: &str, exact: bool) -> Locator<'a> {
        self.locator(Selector::alt_text(text, exact))
    }

    pub fn get_by_title(&self, text: &str, exact: bool) -> Locator<'a> {
        self.locator(Selector::title(text, exact))
    }

    pub fn get_by_test_id(&self, id: &str) -> Locator<'a> {
        self.locator(Selector::test_id(id))
    }

    /// A nested iframe inside this one.
    pub fn frame_locator(&self, iframe: &str) -> FrameLocator<'a> {
        let mut frames = self.frames.clone();
        frames.push(iframe.to_string());
        FrameLocator {
            lease: self.lease,
            frames,
        }
    }
}

/// An imperative handle to a main frame or iframe on a page.
pub struct Frame<'a> {
    lease: &'a PageLease,
    info: FrameInfo,
}

impl<'a> Frame<'a> {
    pub fn page(&self) -> &'a PageLease {
        self.lease
    }

    pub fn id(&self) -> &str {
        &self.info.id
    }

    pub fn url(&self) -> &str {
        &self.info.url
    }

    pub fn name(&self) -> &str {
        &self.info.name
    }

    pub fn is_detached(&self) -> bool {
        !self.lease.frame_infos().iter().any(|f| f.id == self.info.id)
    }

    pub fn parent_frame(&self) -> Option<Frame<'a>> {
        let parent_id = self.info.parent_id.as_deref()?;
        self.lease
            .frame_infos()
            .into_iter()
            .find(|f| f.id == parent_id)
            .map(|info| Frame { lease: self.lease, info })
    }

    pub fn child_frames(&self) -> Vec<Frame<'a>> {
        self.lease
            .frame_infos()
            .into_iter()
            .filter(|f| f.parent_id.as_deref() == Some(self.info.id.as_str()))
            .map(|info| Frame { lease: self.lease, info })
            .collect()
    }

    pub async fn goto(&self, url: &str) -> Result<()> {
        self.lease.frame_goto(&self.info.id, url).await
    }

    pub async fn goto_with(&self, url: &str, wait_until: fluxwright_cdp::WaitUntil) -> Result<()> {
        self.lease.frame_goto_with(&self.info.id, url, wait_until).await
    }

    pub async fn evaluate(&self, expression: &str) -> Result<serde_json::Value> {
        self.lease.frame_evaluate(&self.info.id, expression).await
    }

    pub async fn title(&self) -> Result<String> {
        Ok(self.evaluate("document.title").await?.as_str().unwrap_or("").to_owned())
    }

    pub async fn content(&self) -> Result<String> {
        Ok(self
            .evaluate("document.documentElement ? document.documentElement.outerHTML : ''")
            .await?
            .as_str()
            .unwrap_or("")
            .to_owned())
    }

    fn root(&self) -> FrameLocator<'a> {
        self.lease.frame_locator_by_id(&self.info.id)
    }

    pub fn locator(&self, query: impl Into<String>) -> Locator<'a> {
        self.root().locator(query)
    }

    pub fn get_by_text(&self, text: &str, exact: bool) -> Locator<'a> {
        self.root().get_by_text(text, exact)
    }

    pub fn get_by_role(&self, role: &str, name: Option<&str>, exact: bool) -> Locator<'a> {
        self.root().get_by_role(role, name, exact)
    }

    pub fn get_by_role_with(&self, role: &str, opts: &fluxwright_cdp::RoleOptions) -> Locator<'a> {
        self.root().get_by_role_with(role, opts)
    }

    pub fn get_by_label(&self, text: &str, exact: bool) -> Locator<'a> {
        self.root().get_by_label(text, exact)
    }

    pub fn get_by_placeholder(&self, text: &str, exact: bool) -> Locator<'a> {
        self.root().get_by_placeholder(text, exact)
    }

    pub fn get_by_alt_text(&self, text: &str, exact: bool) -> Locator<'a> {
        self.root().get_by_alt_text(text, exact)
    }

    pub fn get_by_title(&self, text: &str, exact: bool) -> Locator<'a> {
        self.root().get_by_title(text, exact)
    }

    pub fn get_by_test_id(&self, id: &str) -> Locator<'a> {
        self.root().get_by_test_id(id)
    }

    pub fn frame_locator(&self, iframe: &str) -> FrameLocator<'a> {
        self.root().frame_locator(iframe)
    }

    pub async fn new_cdp_session(&self) -> Result<CdpSession> {
        self.lease.new_frame_cdp_session(&self.info.id).await
    }
}

pub struct EngineBuilder {
    config: EngineConfig,
    source: Arc<dyn BrowserSource>,
}

impl Default for EngineBuilder {
    fn default() -> Self {
        Engine::builder()
    }
}

impl EngineBuilder {
    pub fn max_browsers(mut self, n: usize) -> Self {
        self.config.max_browsers = n;
        self
    }
    pub fn min_browsers(mut self, n: usize) -> Self {
        self.config.min_browsers = n;
        self
    }
    pub fn max_contexts_per_browser(mut self, n: usize) -> Self {
        self.config.max_contexts_per_browser = n;
        self
    }
    pub fn memory_ceiling_mb(mut self, n: u64) -> Self {
        self.config.memory_ceiling_mb = n;
        self
    }
    pub fn queue_capacity(mut self, n: usize) -> Self {
        self.config.queue_capacity = n;
        self
    }
    pub fn queue_full_mode(mut self, m: QueueFullMode) -> Self {
        self.config.queue_full_mode = m;
        self
    }
    pub fn recycle_after_jobs(mut self, n: u64) -> Self {
        self.config.recycle_after_jobs = Some(n);
        self
    }
    pub fn recycle_after(mut self, d: Duration) -> Self {
        self.config.recycle_after = Some(d);
        self
    }
    pub fn recycle_rss_mb(mut self, n: u64) -> Self {
        self.config.recycle_rss_mb = Some(n);
        self
    }
    pub fn idle_timeout(mut self, d: Duration) -> Self {
        self.config.idle_timeout = Some(d);
        self
    }
    pub fn acquire_timeout(mut self, d: Duration) -> Self {
        self.config.acquire_timeout = d;
        self
    }
    /// At most `n` pages on one host at once, across the fleet; a `goto` to a full host waits.
    pub fn max_concurrency_per_host(mut self, n: usize) -> Self {
        self.config.max_concurrency_per_host = Some(n);
        self
    }
    /// At least `d` between the starts of two `goto`s to one host.
    pub fn min_delay_per_host(mut self, d: Duration) -> Self {
        self.config.min_delay_per_host = Some(d);
        self
    }
    pub fn navigation_timeout(mut self, d: Duration) -> Self {
        self.config.navigation_timeout = d;
        self
    }
    /// Timeout for click, fill, evaluate, wait_for_selector, screenshot.
    pub fn action_timeout(mut self, d: Duration) -> Self {
        self.config.action_timeout = d;
        self
    }
    pub fn no_sandbox(mut self, v: bool) -> Self {
        self.config.launch.no_sandbox = v;
        self
    }
    pub fn headless(mut self, v: bool) -> Self {
        self.config.launch.headless = v;
        self
    }
    /// Browser binary. Default: see `find_browser` (chrome-headless-shell when headless and
    /// installed, else Chrome).
    pub fn chrome(mut self, path: impl Into<std::path::PathBuf>) -> Self {
        self.config.launch.executable = Some(path.into());
        self
    }
    /// Extra CLI flags passed to Chromium on launch.
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.config
            .launch
            .extra_args
            .extend(args.into_iter().map(Into::into));
        self
    }
    /// Skip all Fluxwright default Chromium CLI flags when `all` is true.
    pub fn ignore_default_args(mut self, all: bool) -> Self {
        self.config.launch.ignore_default_args = if all {
            fluxwright_cdp::IgnoreDefaultArgs::All
        } else {
            fluxwright_cdp::IgnoreDefaultArgs::None
        };
        self
    }
    /// Skip specific default Chromium CLI flags by flag name (with or without `=...`).
    pub fn ignore_default_args_list<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.config.launch.ignore_default_args = fluxwright_cdp::IgnoreDefaultArgs::Some(
            args.into_iter().map(Into::into).collect(),
        );
        self
    }
    /// Browser channel (`chrome`, `chrome-beta`, `chrome-dev`, `chrome-canary`, `msedge`, ...).
    pub fn channel(mut self, channel: impl Into<String>) -> Self {
        self.config.launch.channel = Some(channel.into());
        self
    }
    /// Pre-warmed blank context + `about:blank` page slots kept ready per running browser.
    pub fn prewarm_contexts(mut self, n: usize) -> Self {
        self.config.prewarm_contexts = n;
        self
    }
    /// Directory used to persist named browser profiles (`JobOptions::profile`).
    pub fn profiles_dir(mut self, dir: impl Into<std::path::PathBuf>) -> Self {
        self.config.profiles_dir = Some(dir.into());
        self
    }
    /// Outbound proxy pool used when `JobOptions::proxy` is unset.
    pub fn proxy_pool(mut self, pool: ProxyPool) -> Self {
        self.config.proxy_pool = Some(pool);
        self
    }
    /// Enables AIMD adaptive concurrency limiting driven by memory pressure and outcomes.
    pub fn adaptive_concurrency(mut self, cfg: AdaptiveConfig) -> Self {
        self.config.adaptive_concurrency = Some(cfg);
        self
    }
    pub fn source(mut self, source: Arc<dyn BrowserSource>) -> Self {
        self.source = source;
        self
    }
    /// Connects to a remote browser over CDP (e.g. `ws://...` or `http://...`) with one
    /// connection: concurrency comes from `max_contexts_per_browser`. Several connections to
    /// one browser each attach to every page, and each answers dialogs on pages it does not
    /// own, before the job's own handler can. Set `max_browsers` after this to override.
    pub fn connect_over_cdp(mut self, endpoint_url: impl Into<String>) -> Self {
        self.config.min_browsers = 1;
        self.config.max_browsers = 1;
        self.source = Arc::new(crate::source::RemoteCdp::new(endpoint_url));
        self
    }
    pub async fn build(self) -> Result<Engine> {
        // 0 of either means no lease can ever be granted, and every acquire waits it out.
        if self.config.max_browsers == 0 || self.config.max_contexts_per_browser == 0 {
            return Err(Error::Other(
                "max_browsers and max_contexts_per_browser must be at least 1".into(),
            ));
        }
        if self.config.launch.executable.is_none() {
            if let Some(ref ch) = self.config.launch.channel {
                fluxwright_cdp::find_channel(ch)?;
            }
        }
        let engine = Engine::new(self.config, self.source);
        let n = engine.config().min_browsers;
        for _ in 0..n {
            let b = engine.inner.source.launch(&engine.config().launch).await?;
            let mut st = engine.inner.state.lock().await;
            st.browsers.push(Slot {
                browser: b,
                active: 0,
                warm: VecDeque::new(),
                warming: 0,
                jobs_served: 0,
                started: Instant::now(),
                last_used: Instant::now(),
                draining: false,
                crashed: false,
            });
        }
        engine.inner.maybe_prewarm().await;
        Ok(engine)
    }
}

/// Playwright-style `page.clock` controller for deterministic time and timer testing.
pub struct Clock<'a> {
    lease: &'a PageLease,
}

impl<'a> Clock<'a> {
    /// Installs fake clocks at the current real wall-clock time.
    pub async fn install(&self) -> Result<()> {
        self.install_with(&ClockInstallOptions::default()).await
    }

    /// Installs fake clocks with explicit [`ClockInstallOptions`].
    pub async fn install_with(&self, options: &ClockInstallOptions) -> Result<()> {
        let timeout = self.lease.action_timeout();
        self.lease
            .traced("Clock", "install", "clock.install", || serde_json::json!({}), async {
                Ok(self.lease.page.clock_install(options, timeout).await?)
            })
            .await
    }

    /// Jumps to `time` (if ahead) and pauses time progression.
    pub async fn pause_at(&self, time: impl Into<ClockTime>) -> Result<()> {
        let time = time.into();
        let timeout = self.lease.action_timeout();
        self.lease
            .traced("Clock", "pauseAt", "clock.pause_at", || serde_json::json!({}), async {
                Ok(self.lease.page.clock_pause_at(&time, timeout).await?)
            })
            .await
    }

    /// Freezes `Date.now()` / `new Date()` to `time` while timers continue running.
    pub async fn set_fixed_time(&self, time: impl Into<ClockTime>) -> Result<()> {
        let time = time.into();
        let timeout = self.lease.action_timeout();
        self.lease
            .traced(
                "Clock",
                "setFixedTime",
                "clock.set_fixed_time",
                || serde_json::json!({}),
                async { Ok(self.lease.page.clock_set_fixed_time(&time, timeout).await?) },
            )
            .await
    }

    /// Sets the system clock to `time` without freezing `Date` or firing timers.
    pub async fn set_system_time(&self, time: impl Into<ClockTime>) -> Result<()> {
        let time = time.into();
        let timeout = self.lease.action_timeout();
        self.lease
            .traced(
                "Clock",
                "setSystemTime",
                "clock.set_system_time",
                || serde_json::json!({}),
                async { Ok(self.lease.page.clock_set_system_time(&time, timeout).await?) },
            )
            .await
    }

    /// Jumps simulated time forward by `ticks`, firing due timers at most once.
    pub async fn fast_forward(&self, ticks: impl Into<ClockTicks>) -> Result<()> {
        let ticks = ticks.into();
        let timeout = self.lease.action_timeout();
        self.lease
            .traced(
                "Clock",
                "fastForward",
                "clock.fast_forward",
                || serde_json::json!({}),
                async { Ok(self.lease.page.clock_fast_forward(&ticks, timeout).await?) },
            )
            .await
    }

    /// Advances simulated time by `ticks`, executing due timers and intervals in order.
    pub async fn run_for(&self, ticks: impl Into<ClockTicks>) -> Result<()> {
        let ticks = ticks.into();
        let timeout = self.lease.action_timeout();
        self.lease
            .traced("Clock", "runFor", "clock.run_for", || serde_json::json!({}), async {
                Ok(self.lease.page.clock_run_for(&ticks, timeout).await?)
            })
            .await
    }

    /// Resumes natural wall-clock progression from the current clock state.
    pub async fn resume(&self) -> Result<()> {
        let timeout = self.lease.action_timeout();
        self.lease
            .traced("Clock", "resume", "clock.resume", || serde_json::json!({}), async {
                Ok(self.lease.page.clock_resume(timeout).await?)
            })
            .await
    }
}

/// URL matcher for `page.route` and `context.route`.
#[derive(Clone)]
pub enum UrlPattern {
    Glob(String),
    Predicate(Arc<dyn Fn(&str) -> bool + Send + Sync>),
}

impl UrlPattern {
    pub fn matches(&self, url: &str) -> bool {
        match self {
            Self::Glob(pat) => fluxwright_cdp::url_glob_matches(pat, url),
            Self::Predicate(pred) => pred(url),
        }
    }

    fn glob_key(&self) -> Option<&str> {
        match self {
            Self::Glob(pat) => Some(pat.as_str()),
            Self::Predicate(_) => None,
        }
    }
}

impl From<&str> for UrlPattern {
    fn from(s: &str) -> Self {
        Self::Glob(s.to_owned())
    }
}

impl From<String> for UrlPattern {
    fn from(s: String) -> Self {
        Self::Glob(s)
    }
}

/// Request snapshot visible to a Rust `page.route` / `context.route` handler, reflecting any
/// prior `route.fallback(overrides)`.
#[derive(Debug, Clone)]
pub struct RouteRequest {
    pub url: String,
    pub method: String,
    pub headers: Vec<(String, String)>,
    pub post_data: Option<String>,
    pub resource_type: String,
}

impl RouteRequest {
    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn method(&self) -> &str {
        &self.method
    }

    pub fn headers(&self) -> &[(String, String)] {
        &self.headers
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        let lower = name.to_ascii_lowercase();
        self.headers
            .iter()
            .rev()
            .find(|(k, _)| k.to_ascii_lowercase() == lower)
            .map(|(_, v)| v.as_str())
    }

    pub fn post_data(&self) -> Option<&str> {
        self.post_data.as_deref()
    }

    pub fn resource_type(&self) -> &str {
        &self.resource_type
    }
}

enum RouteOutcome {
    Pending,
    Answered,
    FellBack(Overrides),
}

struct RouteInner {
    raw: Arc<Mutex<Option<InterceptedRequest>>>,
    fetch_handle: RouteFetchHandle,
    accumulated: Overrides,
    request: RouteRequest,
    outcome: Mutex<RouteOutcome>,
    returned: AtomicBool,
}

/// A Playwright-style route handle passed to `page.route` / `context.route` handlers.
#[derive(Clone)]
pub struct Route {
    inner: Arc<RouteInner>,
}

impl Route {
    pub fn request(&self) -> &RouteRequest {
        &self.inner.request
    }

    /// Performs the HTTP request without fulfilling it, inheriting accumulated fallback overrides.
    pub async fn fetch(&self, opts: &RouteFetchOptions) -> Result<ApiResponse> {
        if self.inner.returned.load(Ordering::SeqCst) {
            return Err(Error::Other("route: handler already returned".into()));
        }
        {
            let outcome = self.inner.outcome.lock().await;
            if !matches!(*outcome, RouteOutcome::Pending) {
                return Err(Error::Other("route is already handled".into()));
            }
        }
        if self.inner.raw.lock().await.is_none() {
            return Err(Error::Other("route is already handled".into()));
        }
        Ok(self.inner.fetch_handle.fetch(opts).await?)
    }

    /// Fulfills the intercepted request with a synthetic response.
    pub async fn fulfill(&self, fulfill: Fulfill) -> Result<()> {
        let req = self.take_for_answer().await?;
        Ok(req.fulfill(fulfill).await?)
    }

    /// Continues the intercepted request with optional overrides merged onto fallback overrides.
    pub async fn continue_with(&self, overrides: Overrides) -> Result<()> {
        let req = self.take_for_answer().await?;
        let mut merged = self.inner.accumulated.clone();
        merged.merge(overrides);
        Ok(req.continue_with(merged).await?)
    }

    /// Fails the intercepted request with a Playwright error code (`failed`, `aborted`, ...).
    pub async fn abort(&self, error_code: &str) -> Result<()> {
        let req = self.take_for_answer().await?;
        Ok(req.abort(error_code).await?)
    }

    /// Hands the request to the next matching route handler, applying optional `overrides`.
    pub async fn fallback(&self, overrides: Overrides) -> Result<()> {
        if self.inner.returned.load(Ordering::SeqCst) {
            return Err(Error::Other("route: handler already returned".into()));
        }
        let mut outcome = self.inner.outcome.lock().await;
        if !matches!(*outcome, RouteOutcome::Pending) {
            return Err(Error::Other("route is already handled".into()));
        }
        *outcome = RouteOutcome::FellBack(overrides);
        Ok(())
    }

    async fn take_for_answer(&self) -> Result<InterceptedRequest> {
        if self.inner.returned.load(Ordering::SeqCst) {
            return Err(Error::Other("route: handler already returned".into()));
        }
        let mut outcome = self.inner.outcome.lock().await;
        if !matches!(*outcome, RouteOutcome::Pending) {
            return Err(Error::Other("route is already handled".into()));
        }
        let req = self
            .inner
            .raw
            .lock()
            .await
            .take()
            .ok_or_else(|| Error::Other("route is already handled".into()))?;
        *outcome = RouteOutcome::Answered;
        Ok(req)
    }
}

type RouteHandlerFn = Arc<
    dyn Fn(Route) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send>>
        + Send
        + Sync,
>;

struct RouteEntry {
    id: u64,
    pattern: UrlPattern,
    handler: RouteHandlerFn,
    remaining: std::sync::Mutex<Option<usize>>,
    removed: AtomicBool,
}

static NEXT_ROUTE_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn push_route_entry<F, Fut>(
    list: &std::sync::Mutex<Vec<Arc<RouteEntry>>>,
    pattern: UrlPattern,
    handler: F,
    options: RouteOptions,
) where
    F: Fn(Route) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = Result<()>> + Send + 'static,
{
    if options.times == Some(0) {
        return;
    }
    let id = NEXT_ROUTE_ID.fetch_add(1, Ordering::Relaxed);
    let entry = Arc::new(RouteEntry {
        id,
        pattern,
        handler: Arc::new(move |r| Box::pin(handler(r))),
        remaining: std::sync::Mutex::new(options.times),
        removed: AtomicBool::new(false),
    });
    list.lock().unwrap().push(entry);
}

fn remove_route_entries(list: &std::sync::Mutex<Vec<Arc<RouteEntry>>>, pattern: &str) {
    let mut guard = list.lock().unwrap();
    guard.retain(|entry| {
        if entry.pattern.glob_key() == Some(pattern) {
            entry.removed.store(true, Ordering::SeqCst);
            false
        } else {
            true
        }
    });
}

fn spawn_route_dispatcher(
    page_routes: Option<Arc<std::sync::Mutex<Vec<Arc<RouteEntry>>>>>,
    ctx_routes: Arc<std::sync::Mutex<Vec<Arc<RouteEntry>>>>,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<InterceptedRequest>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(req) = rx.recv().await {
            let page_ref = page_routes.clone();
            let ctx_ref = ctx_routes.clone();
            tokio::spawn(async move {
                dispatch_intercepted_request(page_ref, ctx_ref, req).await;
            });
        }
    })
}

async fn dispatch_intercepted_request(
    page_routes: Option<Arc<std::sync::Mutex<Vec<Arc<RouteEntry>>>>>,
    ctx_routes: Arc<std::sync::Mutex<Vec<Arc<RouteEntry>>>>,
    req: InterceptedRequest,
) {
    let mut accumulated = Overrides::default();
    let mut current = RouteRequest {
        url: req.url.clone(),
        method: req.method.clone(),
        headers: req.headers.clone(),
        post_data: req.post_data.clone(),
        resource_type: req.resource_type.clone(),
    };
    let base_fetch = req.fetch_handle();
    let raw = Arc::new(Mutex::new(Some(req)));

    let mut groups: Vec<(Vec<Arc<RouteEntry>>, Arc<std::sync::Mutex<Vec<Arc<RouteEntry>>>>)> =
        Vec::with_capacity(2);
    if let Some(pr) = page_routes {
        let snapshot = pr.lock().unwrap().clone();
        groups.push((snapshot, pr));
    }
    let ctx_snapshot = ctx_routes.lock().unwrap().clone();
    groups.push((ctx_snapshot, ctx_routes));

    'dispatch: for (entries, owner_list) in groups {
        for entry in entries.into_iter().rev() {
            if entry.removed.load(Ordering::SeqCst) {
                continue;
            }
            if !entry.pattern.matches(&current.url) {
                continue;
            }
            let exhausted_now = {
                let mut rem = entry.remaining.lock().unwrap();
                match *rem {
                    Some(0) => continue,
                    Some(n) => {
                        let next = n - 1;
                        *rem = Some(next);
                        next == 0
                    }
                    None => false,
                }
            };
            if exhausted_now {
                entry.removed.store(true, Ordering::SeqCst);
                owner_list.lock().unwrap().retain(|e| e.id != entry.id);
            }

            let route = Route {
                inner: Arc::new(RouteInner {
                    raw: raw.clone(),
                    fetch_handle: base_fetch.with_overrides(&accumulated),
                    accumulated: accumulated.clone(),
                    request: current.clone(),
                    outcome: Mutex::new(RouteOutcome::Pending),
                    returned: AtomicBool::new(false),
                }),
            };

            let res = (entry.handler)(route.clone()).await;
            route.inner.returned.store(true, Ordering::SeqCst);
            if let Err(err) = res {
                warn!(error = %err, "route handler failed; continuing request");
                break 'dispatch;
            }

            let outcome = std::mem::replace(
                &mut *route.inner.outcome.lock().await,
                RouteOutcome::Pending,
            );
            match outcome {
                RouteOutcome::Answered => return,
                RouteOutcome::FellBack(overrides) => {
                    if let Some(ref url) = overrides.url {
                        current.url = url.clone();
                    }
                    if let Some(ref method) = overrides.method {
                        current.method = method.clone();
                    }
                    if let Some(ref headers) = overrides.headers {
                        current.headers = normalize_headers(headers.clone());
                    }
                    if let Some(ref post_data) = overrides.post_data {
                        current.post_data = Some(String::from_utf8_lossy(post_data).into_owned());
                    }
                    accumulated.merge(overrides);
                    continue;
                }
                RouteOutcome::Pending => break 'dispatch,
            }
        }
    }

    let maybe_req = raw.lock().await.take();
    if let Some(req) = maybe_req {
        let _ = req.continue_with(accumulated).await;
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use fluxwright_cdp::{CdpBrowser, Error as CdpError, LaunchOptions, Result as CdpResult};
    use tokio::sync::{mpsc, Notify};

    use super::*;

    struct HoldSource {
        calls: Arc<AtomicUsize>,
        started: mpsc::UnboundedSender<()>,
        release: Arc<Notify>,
    }

    impl BrowserSource for HoldSource {
        fn launch(
            &self,
            _opts: &LaunchOptions,
        ) -> Pin<Box<dyn Future<Output = CdpResult<Arc<CdpBrowser>>> + Send + '_>> {
            let calls = self.calls.clone();
            let started = self.started.clone();
            let release = self.release.clone();
            Box::pin(async move {
                calls.fetch_add(1, Ordering::SeqCst);
                let _ = started.send(());
                release.notified().await;
                Err(CdpError::Launch("mock stop".into()))
            })
        }
    }

    #[tokio::test]
    async fn cold_start_throttles_launches_by_context_capacity() {
        let calls = Arc::new(AtomicUsize::new(0));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let release = Arc::new(Notify::new());
        let eng = Engine::builder()
            .min_browsers(0)
            .max_browsers(4)
            .max_contexts_per_browser(8)
            .memory_ceiling_mb(0)
            .source(Arc::new(HoldSource {
                calls: calls.clone(),
                started: tx,
                release: release.clone(),
            }))
            .build()
            .await
            .unwrap();

        let waiters: Vec<_> = (0..4)
            .map(|_| {
                let eng = eng.clone();
                tokio::spawn(async move { eng.acquire().await })
            })
            .collect();

        rx.recv().await.unwrap();
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        {
            let st = eng.inner.state.lock().await;
            assert_eq!(st.launching, 1);
            assert_eq!(st.queue.len(), 3);
        }

        eng.shutdown(Duration::from_millis(10)).await.unwrap();
        release.notify_waiters();
        let mut results = Vec::new();
        for h in waiters {
            results.push(h.await.unwrap());
        }
        assert!(results.iter().any(|r| matches!(r, Err(Error::ShuttingDown))));
    }

    #[tokio::test]
    async fn cold_start_scales_when_queue_exceeds_in_flight_slots() {
        let calls = Arc::new(AtomicUsize::new(0));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let release = Arc::new(Notify::new());
        let eng = Engine::builder()
            .min_browsers(0)
            .max_browsers(4)
            .max_contexts_per_browser(8)
            .memory_ceiling_mb(0)
            .source(Arc::new(HoldSource {
                calls: calls.clone(),
                started: tx,
                release: release.clone(),
            }))
            .build()
            .await
            .unwrap();

        let waiters: Vec<_> = (0..10)
            .map(|_| {
                let eng = eng.clone();
                tokio::spawn(async move { eng.acquire().await })
            })
            .collect();

        rx.recv().await.unwrap();
        rx.recv().await.unwrap();
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        {
            let st = eng.inner.state.lock().await;
            assert_eq!(st.launching, 2);
            assert_eq!(st.queue.len(), 8);
        }

        eng.shutdown(Duration::from_millis(10)).await.unwrap();
        release.notify_waiters();
        for h in waiters {
            let _ = h.await;
        }
    }

    #[tokio::test]
    async fn memory_ceiling_blocks_dispatch_without_popping_or_launching() {
        let calls = Arc::new(AtomicUsize::new(0));
        let (tx, _rx) = mpsc::unbounded_channel();
        let release = Arc::new(Notify::new());
        let eng = Engine::builder()
            .min_browsers(0)
            .max_browsers(4)
            .max_contexts_per_browser(8)
            .memory_ceiling_mb(1)
            .source(Arc::new(HoldSource {
                calls: calls.clone(),
                started: tx,
                release: release.clone(),
            }))
            .build()
            .await
            .unwrap();
        *eng.inner.rss_cache.lock().await = (
            Some(Instant::now()),
            HashMap::from([(1, 100 * 1024 * 1024)]),
        );

        let waiters: Vec<_> = (0..8)
            .map(|_| {
                let eng = eng.clone();
                tokio::spawn(async move { eng.acquire().await })
            })
            .collect();

        tokio::time::sleep(Duration::from_millis(60)).await;
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(eng.inner.state.lock().await.queue.len(), 8);

        eng.shutdown(Duration::from_millis(10)).await.unwrap();
        for h in waiters {
            assert!(matches!(h.await.unwrap(), Err(Error::ShuttingDown)));
        }
    }

    /// Over the ceiling a running browser with room is still used; only launches stop.
    #[tokio::test]
    async fn memory_ceiling_still_reuses_running_browsers() {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;

        // A stand-in remote browser: answers every command with an empty result.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (seen_tx, mut seen) = mpsc::unbounded_channel::<String>();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            while let Some(Ok(Message::Text(text))) = ws.next().await {
                let msg: serde_json::Value = serde_json::from_str(&text).unwrap();
                let _ = seen_tx.send(msg["method"].as_str().unwrap_or("").to_string());
                let reply = serde_json::json!({ "id": msg["id"], "result": {} }).to_string();
                if ws.send(Message::Text(reply.into())).await.is_err() {
                    break;
                }
            }
        });
        let browser = CdpBrowser::connect(&format!("ws://{addr}")).await.unwrap();

        let calls = Arc::new(AtomicUsize::new(0));
        let (tx, _rx) = mpsc::unbounded_channel();
        let eng = Engine::builder()
            .min_browsers(0)
            .max_browsers(4)
            .max_contexts_per_browser(8)
            .memory_ceiling_mb(1)
            .source(Arc::new(HoldSource {
                calls: calls.clone(),
                started: tx,
                release: Arc::new(Notify::new()),
            }))
            .build()
            .await
            .unwrap();
        *eng.inner.rss_cache.lock().await = (
            Some(Instant::now()),
            HashMap::from([(1, 100 * 1024 * 1024)]),
        );
        eng.inner.state.lock().await.browsers.push(Slot {
            browser,
            active: 0,
            warm: VecDeque::new(),
            warming: 0,
            jobs_served: 0,
            started: Instant::now(),
            last_used: Instant::now(),
            draining: false,
            crashed: false,
        });

        // The stand-in cannot make a context, so the lease fails: what counts is that it was
        // tried on the running browser instead of waiting in the queue.
        // Under the 500 ms RSS cache expiry: the old code also got through once the cache refreshed.
        let res = tokio::time::timeout(Duration::from_millis(400), eng.acquire())
            .await
            .expect("the request waited instead of reusing the running browser");
        assert!(res.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let mut methods = Vec::new();
        while let Ok(m) = seen.try_recv() {
            methods.push(m);
        }
        assert!(
            methods.iter().any(|m| m == "Target.createBrowserContext"),
            "{methods:?}"
        );
        eng.shutdown(Duration::from_millis(10)).await.unwrap();
    }

    #[tokio::test]
    async fn shutdown_rejects_late_acquire_and_discards_late_launch() {
        let calls = Arc::new(AtomicUsize::new(0));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let release = Arc::new(Notify::new());
        let eng = Engine::builder()
            .min_browsers(0)
            .max_browsers(2)
            .max_contexts_per_browser(4)
            .memory_ceiling_mb(0)
            .source(Arc::new(HoldSource {
                calls: calls.clone(),
                started: tx,
                release: release.clone(),
            }))
            .build()
            .await
            .unwrap();

        let inflight = tokio::spawn({
            let eng = eng.clone();
            async move { eng.acquire().await }
        });
        rx.recv().await.unwrap();

        eng.shutdown(Duration::from_millis(20)).await.unwrap();
        assert!(matches!(eng.acquire().await, Err(Error::ShuttingDown)));

        release.notify_waiters();
        let _ = inflight.await;
        assert!(eng.inner.state.lock().await.browsers.is_empty());
        assert_eq!(eng.inner.state.lock().await.launching, 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn batch_dispatch_admits_waiters_and_wakes_queue_waiters() {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;

        struct MockWsSource(std::net::SocketAddr);
        impl BrowserSource for MockWsSource {
            fn launch(
                &self,
                _opts: &LaunchOptions,
            ) -> Pin<Box<dyn Future<Output = CdpResult<Arc<CdpBrowser>>> + Send + '_>> {
                let addr = self.0;
                Box::pin(async move { CdpBrowser::connect(&format!("ws://{addr}")).await })
            }
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seq = Arc::new(AtomicUsize::new(1));
        let srv = tokio::spawn({
            let seq = seq.clone();
            async move {
                while let Ok((stream, _)) = listener.accept().await {
                    let seq = seq.clone();
                    tokio::spawn(async move {
                        let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else {
                            return;
                        };
                        while let Some(Ok(Message::Text(text))) = ws.next().await {
                            let Ok(msg) = serde_json::from_str::<serde_json::Value>(&text) else {
                                break;
                            };
                            let result = match msg["method"].as_str() {
                                Some("Target.createBrowserContext") => {
                                    let n = seq.fetch_add(1, Ordering::SeqCst);
                                    serde_json::json!({ "browserContextId": format!("ctx-{n}") })
                                }
                                _ => serde_json::json!({}),
                            };
                            let reply =
                                serde_json::json!({ "id": msg["id"], "result": result }).to_string();
                            if ws.send(Message::Text(reply.into())).await.is_err() {
                                break;
                            }
                        }
                    });
                }
            }
        });

        let eng = Engine::builder()
            .min_browsers(0)
            .max_browsers(2)
            .max_contexts_per_browser(2)
            .memory_ceiling_mb(0)
            .queue_capacity(2)
            .queue_full_mode(QueueFullMode::Wait)
            .acquire_timeout(Duration::from_secs(5))
            .source(Arc::new(MockWsSource(addr)))
            .build()
            .await
            .unwrap();

        let jobs: Vec<_> = (0..8)
            .map(|_| {
                let eng = eng.clone();
                tokio::spawn(async move {
                    let ctx = eng.acquire_context().await.unwrap();
                    ctx.close().await.unwrap();
                })
            })
            .collect();

        for j in jobs {
            tokio::time::timeout(Duration::from_secs(5), j)
                .await
                .expect("waiter timed out under QueueFullMode::Wait")
                .unwrap();
        }
        eng.shutdown(Duration::from_secs(2)).await.unwrap();
        srv.abort();
    }

    #[test]
    fn resolve_url_handles_relative_and_absolute_paths() {
        assert_eq!(resolve_url(None, "/a"), "/a");
        assert_eq!(resolve_url(Some("http://127.0.0.1:8080/sub/page.html"), "/a"), "http://127.0.0.1:8080/a");
        assert_eq!(resolve_url(Some("http://127.0.0.1:8080/sub/page.html"), "b.html"), "http://127.0.0.1:8080/sub/b.html");
        assert_eq!(resolve_url(Some("http://127.0.0.1:8080"), "b.html"), "http://127.0.0.1:8080/b.html");
        assert_eq!(resolve_url(Some("http://127.0.0.1:8080/"), "//cdn.test/x.js"), "http://cdn.test/x.js");
        assert_eq!(resolve_url(Some("http://127.0.0.1:8080/"), "https://other.test/y"), "https://other.test/y");
        assert_eq!(resolve_url(Some("http://127.0.0.1:8080/"), "about:blank"), "about:blank");
        assert_eq!(
            resolve_url(Some("http://127.0.0.1:8080"), "/login?next=http://example.com"),
            "http://127.0.0.1:8080/login?next=http://example.com"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn context_lease_is_closed_when_browser_connection_dies() {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;

        struct MockWsSource(std::net::SocketAddr);
        impl BrowserSource for MockWsSource {
            fn launch(
                &self,
                _opts: &LaunchOptions,
            ) -> Pin<Box<dyn Future<Output = CdpResult<Arc<CdpBrowser>>> + Send + '_>> {
                let addr = self.0;
                Box::pin(async move { CdpBrowser::connect(&format!("ws://{addr}")).await })
            }
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let drop_ws = Arc::new(Notify::new());
        let srv = tokio::spawn({
            let drop_ws = drop_ws.clone();
            async move {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else {
                    return;
                };
                loop {
                    tokio::select! {
                        _ = drop_ws.notified() => {
                            let _ = ws.close(None).await;
                            break;
                        }
                        frame = ws.next() => {
                            let Some(Ok(Message::Text(text))) = frame else {
                                break;
                            };
                            let Ok(msg) = serde_json::from_str::<serde_json::Value>(&text) else {
                                break;
                            };
                            let result = match msg["method"].as_str() {
                                Some("Target.createBrowserContext") => {
                                    serde_json::json!({ "browserContextId": "ctx-dead-check" })
                                }
                                _ => serde_json::json!({}),
                            };
                            let reply =
                                serde_json::json!({ "id": msg["id"], "result": result }).to_string();
                            if ws.send(Message::Text(reply.into())).await.is_err() {
                                break;
                            }
                        }
                    }
                }
            }
        });

        let eng = Engine::builder()
            .min_browsers(0)
            .max_browsers(1)
            .max_contexts_per_browser(2)
            .memory_ceiling_mb(0)
            .acquire_timeout(Duration::from_secs(5))
            .source(Arc::new(MockWsSource(addr)))
            .build()
            .await
            .unwrap();

        let ctx = eng.acquire_context().await.unwrap();
        assert!(!ctx.is_closed());

        drop_ws.notify_waiters();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !ctx.is_closed() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            ctx.is_closed(),
            "ContextLease::is_closed() must become true once browser connection dies"
        );

        drop(ctx);
        eng.shutdown(Duration::from_secs(2)).await.unwrap();
        srv.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn context_guard_dispose_skips_absent_dir_and_removes_existing_dir() {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;

        struct MockWsSource(std::net::SocketAddr);
        impl BrowserSource for MockWsSource {
            fn launch(
                &self,
                _opts: &LaunchOptions,
            ) -> Pin<Box<dyn Future<Output = CdpResult<Arc<CdpBrowser>>> + Send + '_>> {
                let addr = self.0;
                Box::pin(async move { CdpBrowser::connect(&format!("ws://{addr}")).await })
            }
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seq = Arc::new(AtomicUsize::new(1));
        let srv = tokio::spawn({
            let seq = seq.clone();
            async move {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else {
                    return;
                };
                while let Some(Ok(Message::Text(text))) = ws.next().await {
                    let Ok(msg) = serde_json::from_str::<serde_json::Value>(&text) else {
                        break;
                    };
                    let result = match msg["method"].as_str() {
                        Some("Target.createBrowserContext") => {
                            let n = seq.fetch_add(1, Ordering::SeqCst);
                            serde_json::json!({ "browserContextId": format!("ctx-dl-{n}") })
                        }
                        _ => serde_json::json!({}),
                    };
                    let reply = serde_json::json!({ "id": msg["id"], "result": result }).to_string();
                    if ws.send(Message::Text(reply.into())).await.is_err() {
                        break;
                    }
                }
            }
        });

        let eng = Engine::builder()
            .min_browsers(0)
            .max_browsers(1)
            .max_contexts_per_browser(2)
            .memory_ceiling_mb(0)
            .acquire_timeout(Duration::from_secs(5))
            .source(Arc::new(MockWsSource(addr)))
            .build()
            .await
            .unwrap();

        let ctx1 = eng.acquire_context().await.unwrap();
        let absent_dir = ctx1.guard.download_dir.clone();
        assert!(!absent_dir.exists());
        ctx1.close().await.unwrap();
        assert!(!absent_dir.exists());

        let ctx2 = eng.acquire_context().await.unwrap();
        let existing_dir = ctx2.guard.download_dir.clone();
        std::fs::create_dir_all(&existing_dir).unwrap();
        std::fs::write(existing_dir.join("payload.bin"), b"abc").unwrap();
        assert!(existing_dir.exists());
        ctx2.close().await.unwrap();

        let deadline = Instant::now() + Duration::from_secs(2);
        while existing_dir.exists() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            !existing_dir.exists(),
            "existing download_dir should be removed in background: {}",
            existing_dir.display()
        );

        eng.shutdown(Duration::from_secs(2)).await.unwrap();
        srv.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn inactive_tracing_tracks_open_pages_for_later_start() {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;

        struct MockPageWsSource(std::net::SocketAddr);
        impl BrowserSource for MockPageWsSource {
            fn launch(
                &self,
                _opts: &LaunchOptions,
            ) -> Pin<Box<dyn Future<Output = CdpResult<Arc<CdpBrowser>>> + Send + '_>> {
                let addr = self.0;
                Box::pin(async move { CdpBrowser::connect(&format!("ws://{addr}")).await })
            }
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let srv = tokio::spawn(async move {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else {
                return;
            };
            let mut target_seq = 0usize;
            while let Some(Ok(Message::Text(text))) = ws.next().await {
                let Ok(msg) = serde_json::from_str::<serde_json::Value>(&text) else {
                    break;
                };
                if msg["method"].as_str() == Some("Target.createTarget") {
                    target_seq += 1;
                    let tid = format!("t-{target_seq}");
                    let sid = format!("s-{target_seq}");
                    let ctx_id = msg["params"]["browserContextId"].clone();
                    let reply = serde_json::json!({
                        "id": msg["id"],
                        "result": { "targetId": tid }
                    })
                    .to_string();
                    if ws.send(Message::Text(reply.into())).await.is_err() {
                        break;
                    }
                    let attached = serde_json::json!({
                        "method": "Target.attachedToTarget",
                        "params": {
                            "sessionId": sid,
                            "waitingForDebugger": false,
                            "targetInfo": {
                                "targetId": tid,
                                "type": "page",
                                "url": "about:blank",
                                "browserContextId": ctx_id
                            }
                        }
                    })
                    .to_string();
                    if ws.send(Message::Text(attached.into())).await.is_err() {
                        break;
                    }
                    continue;
                }
                let result = match msg["method"].as_str() {
                    Some("Target.createBrowserContext") => {
                        serde_json::json!({ "browserContextId": "ctx-trace" })
                    }
                    _ => serde_json::json!({}),
                };
                let reply = serde_json::json!({ "id": msg["id"], "result": result }).to_string();
                if ws.send(Message::Text(reply.into())).await.is_err() {
                    break;
                }
            }
        });

        let eng = Engine::builder()
            .min_browsers(0)
            .max_browsers(1)
            .max_contexts_per_browser(2)
            .memory_ceiling_mb(0)
            .acquire_timeout(Duration::from_secs(5))
            .source(Arc::new(MockPageWsSource(addr)))
            .build()
            .await
            .unwrap();

        let ctx = eng.acquire_context().await.unwrap();
        assert!(!ctx.guard.tracing.is_active());
        let page = ctx.new_page().await.unwrap();
        assert!(!ctx.guard.tracing.is_active());
        assert_eq!(ctx.guard.tracing.tracked_target_ids(), vec![page.target_id()]);

        // Starting tracing after page creation must still capture the open page.
        ctx.tracing()
            .start(crate::TracingStartOptions::default())
            .await
            .unwrap();
        assert!(ctx.guard.tracing.is_active());
        ctx.tracing().stop(None).await.unwrap();

        drop(page);
        assert!(ctx.guard.tracing.tracked_target_ids().is_empty());

        ctx.close().await.unwrap();
        eng.shutdown(Duration::from_secs(2)).await.unwrap();
        srv.abort();
    }
}
