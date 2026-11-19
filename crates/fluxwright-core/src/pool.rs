use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use fluxwright_cdp::{
    BoundingBox, CdpBrowser, CdpPage, ConsoleMessage, ContextId, Download, DownloadQueue,
    FrameInfo, InterceptedRequest, LogEntry, PageError, PageLog, PdfOptions, Proxy, ResourceType,
    ScreenshotOptions, Selector,
};
use tokio::sync::{Mutex, Notify};
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::config::{EngineConfig, JobOptions, Priority, QueueFullMode};
use crate::error::{Error, Result};
use crate::hosts::{HostLimits, HostSlot};
use crate::metrics::{Counters, MetricsSnapshot};
use crate::process::{current_process_rss_bytes, process_tree_memory_map};
use crate::source::BrowserSource;

#[derive(Clone)]
pub struct Engine {
    inner: Arc<Inner>,
}

struct Inner {
    config: EngineConfig,
    source: Arc<dyn BrowserSource>,
    state: Mutex<State>,
    admit: Notify,
    /// When the fleet's memory was last walked (never: `None`), and each browser root's footprint.
    rss_cache: Mutex<(Option<Instant>, HashMap<u32, u64>)>,
    rss_refreshing: AtomicBool,
    grants: Arc<tokio::sync::Semaphore>,
    hosts: HostLimits,
}

struct State {
    browsers: Vec<Slot>,
    queue: VecDeque<Waiter>,
    counters: Counters,
    launching: usize,
    shutting_down: bool,
}

impl State {
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

/// An engine dropped without `shutdown()` kills its browsers: their own tasks keep them alive
/// until Chrome exits.
impl Drop for Inner {
    fn drop(&mut self) {
        if let Ok(st) = self.state.try_lock() {
            for s in &st.browsers {
                s.browser.start_kill();
            }
        }
    }
}

struct Slot {
    browser: Arc<CdpBrowser>,
    active: usize,
    jobs_served: u64,
    started: Instant,
    last_used: Instant,
    draining: bool,
    crashed: bool,
}

struct Waiter {
    priority: Priority,
    proxy: Option<Proxy>,
    queued_at: Instant,
    tx: tokio::sync::oneshot::Sender<Result<Arc<ContextGuard>>>,
}

struct ContextGuard {
    engine: Arc<Inner>,
    browser: Arc<CdpBrowser>,
    context: ContextId,
    download_dir: std::path::PathBuf,
    runtime: tokio::runtime::Handle,
    disposed: AtomicBool,
}

impl ContextGuard {
    fn is_disposed(&self) -> bool {
        self.disposed.load(Ordering::SeqCst)
    }

    async fn dispose(&self, completed: bool) {
        let _ = std::fs::remove_dir_all(&self.download_dir);
        if self.disposed.swap(true, Ordering::SeqCst) {
            return;
        }
        let _ = self.browser.dispose_context(&self.context).await;
        self.engine.release_slot(&self.browser, completed).await;
    }

    fn dispose_in_background(self: &Arc<Self>, completed: bool) {
        let _ = std::fs::remove_dir_all(&self.download_dir);
        if self.disposed.swap(true, Ordering::SeqCst) {
            return;
        }
        let guard = Arc::clone(self);
        self.runtime.spawn(async move {
            let _ = guard.browser.dispose_context(&guard.context).await;
            guard.engine.release_slot(&guard.browser, completed).await;
        });
    }
}

impl Drop for ContextGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.download_dir);
        if self.disposed.swap(true, Ordering::SeqCst) {
            return;
        }
        let browser = self.browser.clone();
        let ctx = self.context.clone();
        let engine = self.engine.clone();
        self.runtime.spawn(async move {
            let _ = browser.dispose_context(&ctx).await;
            engine.release_slot(&browser, true).await;
        });
    }
}

/// An isolated browser context leased from the pool; may hold multiple pages and popups.
pub struct ContextLease {
    guard: Arc<ContextGuard>,
    base_url: Option<String>,
    action_timeout: Option<Duration>,
    navigation_timeout: Option<Duration>,
    binding_listeners: Listeners<fluxwright_cdp::BindingCall>,
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

/// Routes events (dialogs, file choosers) to the newest listener still subscribed; an
/// unhandled event is dropped cleanly. Ends with the context.
fn route_newest<T: Send + 'static>(
    mut rx: tokio::sync::mpsc::UnboundedReceiver<T>,
    listeners: Listeners<T>,
) {
    tokio::spawn(async move {
        while let Some(item) = rx.recv().await {
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
    });
}

impl Engine {
    pub fn new(config: EngineConfig, source: Arc<dyn BrowserSource>) -> Self {
        let inner = Arc::new(Inner {
            hosts: HostLimits::new(config.max_concurrency_per_host, config.min_delay_per_host),
            config,
            source,
            state: Mutex::new(State {
                browsers: Vec::new(),
                queue: VecDeque::new(),
                counters: Counters::default(),
                launching: 0,
                shutting_down: false,
            }),
            admit: Notify::new(),
            rss_cache: Mutex::new((None, HashMap::new())),
            rss_refreshing: AtomicBool::new(false),
            grants: Arc::new(tokio::sync::Semaphore::new(24)),
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
        let guard = self.acquire_context_guard(priority, None, timeout).await?;
        let ctx = ContextLease {
            guard,
            base_url: None,
            action_timeout: None,
            navigation_timeout: None,
            binding_listeners: Listeners::default(),
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

    /// Acquires an isolated browser context configured with `opts`; open tabs via [`ContextLease::new_page`].
    pub async fn acquire_context_with(&self, opts: &JobOptions) -> Result<ContextLease> {
        let timeout_budget = opts.timeout.unwrap_or(self.inner.config.acquire_timeout);
        let guard = self
            .acquire_context_guard(opts.priority, opts.proxy.clone(), timeout_budget)
            .await?;
        let setup_res = async {
            apply_context_blocking(&guard, opts).await?;
            let timeout = Duration::from_secs(5);
            if let Some(creds) = &opts.http_credentials {
                guard
                    .browser
                    .context_set_http_credentials(&guard.context, creds, timeout)
                    .await?;
            }
            guard
                .browser
                .context_grant_permissions(&guard.context, &opts.permissions, timeout)
                .await?;
            if let Some(state) = &opts.storage_state {
                guard
                    .browser
                    .context_set_storage_state(&guard.context, state, timeout)
                    .await?;
            }
            guard
                .browser
                .context_emulate(&guard.context, &opts.emulation, timeout)
                .await?;
            Ok::<_, Error>(())
        }
        .await;
        if let Err(e) = setup_res {
            guard.dispose(false).await;
            return Err(e);
        }
        let binding_listeners = Listeners::default();
        route_newest(
            guard.browser.context_bindings(&guard.context).await,
            binding_listeners.clone(),
        );
        Ok(ContextLease {
            guard,
            base_url: opts.base_url.clone(),
            action_timeout: None,
            navigation_timeout: None,
            binding_listeners,
            binding_handlers: Default::default(),
            binding_dispatch_started: AtomicBool::new(false),
        })
    }

    /// A lease set up for `opts`: priority, proxy, resource blocking, permissions, saved
    /// storage and emulation.
    pub async fn acquire_job(&self, opts: &JobOptions) -> Result<PageLease> {
        let ctx = self.acquire_context_with(opts).await?;
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
    ) -> Result<Arc<ContextGuard>> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        {
            let mut st = self.inner.state.lock().await;
            if st.shutting_down {
                return Err(Error::ShuttingDown);
            }
            drop_abandoned(&mut st.queue);
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
                            let notified = self.inner.admit.notified();
                            tokio::pin!(notified);
                            notified.as_mut().enable();
                            {
                                // Checked and joined under one lock: every waiter woken at once
                                // used to see the same free place and join, past the capacity.
                                let mut st = self.inner.state.lock().await;
                                if st.shutting_down {
                                    return Err(Error::ShuttingDown);
                                }
                                drop_abandoned(&mut st.queue);
                                if st.queue.len() < self.inner.config.queue_capacity {
                                    st.queue.push_back(Waiter {
                                        priority,
                                        proxy,
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
                st.queue.push_back(Waiter {
                    priority,
                    proxy,
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
        self.inner.admit.notify_waiters();
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
        let (browsers, contexts, queued, reuse, crash, recycle, pids) = {
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
                st.counters.reuse_rate(),
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
        MetricsSnapshot {
            browsers,
            contexts,
            pages: contexts,
            active_leases: leases,
            queued_requests: queued,
            browser_reuse_rate: reuse,
            crash_count: crash,
            recycle_count: recycle,
            engine_rss_bytes: engine_rss,
            browser_tree_rss_bytes: tree,
            memory_per_job_bytes_estimate: if leases == 0 { 0 } else { tree / leases as u64 },
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
            self.inner.admit.notify_waiters();
            if let Some(browsers) = browsers {
                break browsers;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        let mut closing = tokio::task::JoinSet::new();
        for s in browsers {
            closing.spawn(async move {
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
        {
            let mut st = self.state.lock().await;
            if st.shutting_down {
                return Ok(false);
            }
            drop_abandoned(&mut st.queue);
            if !st.can_dispatch(&self.config) {
                return Ok(false);
            }
        }
        // Over the memory ceiling no new browser starts; one already running still takes work.
        let ceiling = self.config.memory_ceiling_mb * 1024 * 1024;
        let over_ceiling = ceiling > 0 && {
            let tree = self.browser_tree_rss().await;
            if tree > ceiling {
                debug!(tree, ceiling, "rss ceiling hit: no new launches");
            }
            tree > ceiling
        };
        let permit = match self.grants.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => return Ok(false),
        };
        let (waiter, reservation) = {
            let mut st = self.state.lock().await;
            if st.shutting_down {
                return Ok(false);
            }
            drop_abandoned(&mut st.queue);
            let idx = st.next_reusable_browser(self.config.max_contexts_per_browser);
            if idx.is_none() && (over_ceiling || !st.can_launch_for_queue(&self.config)) {
                return Ok(false);
            }
            let Some(waiter) = pick_waiter(&mut st.queue) else {
                return Ok(false);
            };
            let reservation = if let Some(i) = idx {
                st.browsers[i].active += 1;
                st.counters.acquires += 1;
                st.counters.acquires_reused_browser += 1;
                Reservation::Reuse(st.browsers[i].browser.clone())
            } else {
                st.launching += 1;
                Reservation::Launch
            };
            (waiter, reservation)
        };
        let inner = self.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let res = inner.prepare_context(waiter.proxy.as_ref(), reservation).await;
            let _ = waiter.tx.send(res);
            inner.admit.notify_waiters();
        });
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
                me.admit.notify_waiters();
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

    async fn prepare_context(
        self: &Arc<Self>,
        proxy: Option<&Proxy>,
        reservation: Reservation,
    ) -> Result<Arc<ContextGuard>> {
        let browser = match reservation {
            Reservation::Reuse(b) => {
                if self.state.lock().await.shutting_down {
                    self.release_slot(&b, false).await;
                    return Err(Error::ShuttingDown);
                }
                b
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
                    Ok(Ok(b)) => b,
                    Ok(Err(b)) => {
                        let _ = b.close().await;
                        return Err(Error::ShuttingDown);
                    }
                    Err(e) => return Err(e.into()),
                }
            }
        };

        let ctx = match browser.create_context(proxy).await {
            Ok(c) => c,
            Err(e) => {
                self.release_slot(&browser, false).await;
                return Err(e.into());
            }
        };

        info!(browser = %browser.id.0, context = %ctx.0, "context lease granted");

        // Named like profile folders, so the sweep after a crash finds it: `fluxwright-<pid>-…`.
        let download_dir = std::env::temp_dir().join(format!("fluxwright-{}-dl-{}", std::process::id(), ctx.0));
        // Best effort: a Chrome that refuses only loses downloads.
        let _ = browser
            .context_allow_downloads(&ctx, &download_dir, Duration::from_secs(5))
            .await;

        Ok(Arc::new(ContextGuard {
            engine: self.clone(),
            browser,
            context: ctx,
            download_dir,
            runtime: tokio::runtime::Handle::current(),
            disposed: AtomicBool::new(false),
        }))
    }

    async fn release_slot(self: &Arc<Self>, browser: &CdpBrowser, completed: bool) {
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
        drop(st);
        self.admit.notify_waiters();
    }

    async fn reap(&self) {
        let mut st = self.state.lock().await;
        let mut i = 0;
        while i < st.browsers.len() {
            let dead = st.browsers[i].browser.connection_dead() || st.browsers[i].crashed;
            if dead && st.browsers[i].active == 0 {
                let slot = st.browsers.remove(i);
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

    async fn recycle(&self) {
        let now = Instant::now();
        let rss_limit = self.config.recycle_rss_mb;
        let rss_by_pid = if rss_limit.is_some() {
            let pids: Vec<u32> = {
                let st = self.state.lock().await;
                st.browsers
                    .iter()
                    .filter(|s| !s.crashed && s.browser.pid > 0)
                    .map(|s| s.browser.pid)
                    .collect()
            };
            if pids.is_empty() {
                HashMap::new()
            } else {
                let cached = {
                    let cache = self.rss_cache.lock().await;
                    if cache.0.is_some_and(|t| t.elapsed() < Duration::from_millis(500))
                        && pids.iter().all(|p| cache.1.contains_key(p))
                    {
                        Some(cache.1.clone())
                    } else {
                        None
                    }
                };
                match cached {
                    Some(map) => map,
                    None => {
                        let map =
                            tokio::task::spawn_blocking(move || process_tree_memory_map(&pids))
                                .await
                                .unwrap_or_default();
                        *self.rss_cache.lock().await = (Some(Instant::now()), map.clone());
                        map
                    }
                }
            }
        } else {
            HashMap::new()
        };
        let mut to_close = Vec::new();
        let mut bump = 0u64;
        {
            let mut st = self.state.lock().await;
            // The idle rule never takes the pool below min_browsers.
            let mut idle_closable = st.serving().saturating_sub(self.config.min_browsers);
            for s in st.browsers.iter_mut() {
                if s.draining || s.crashed {
                    if s.draining && s.active == 0 {
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
        // Not awaited: dispatch serves every waiter, and a close can take seconds.
        for b in to_close {
            tokio::spawn(async move {
                let _ = b.close().await;
            });
        }
    }
}

fn pick_waiter(queue: &mut VecDeque<Waiter>) -> Option<Waiter> {
    drop_abandoned(queue);
    let best_pri = queue.iter().map(|w| w.priority).max()?;
    let idx = queue
        .iter()
        .enumerate()
        .filter(|(_, w)| w.priority == best_pri)
        .min_by_key(|(_, w)| w.queued_at)
        .map(|(i, _)| i)?;
    queue.remove(idx)
}

/// Waiters whose caller timed out or gave up (the receiver is gone). Left queued, they count
/// against `queue_capacity` and get a page built that nobody takes.
fn drop_abandoned(queue: &mut VecDeque<Waiter>) {
    queue.retain(|w| !w.tx.is_closed());
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
        let (log, downloads, log_task) = page.collect_logs(&context.download_dir);
        let dialog_listeners = Listeners::default();
        route_newest(page.dialogs().await, dialog_listeners.clone());
        let file_chooser_listeners = Listeners::default();
        route_newest(page.file_choosers().await, file_chooser_listeners.clone());
        let binding_listeners = Listeners::default();
        route_newest(page.bindings().await, binding_listeners.clone());

        let popup_listeners: Listeners<PageLease> = Listeners::default();
        let mut popups_rx = page.popups().await;
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
                        drop(raw_popup);
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

        PageLease {
            engine,
            context,
            owns_context,
            page,
            lease_id: Uuid::new_v4(),
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
        self.guard.is_disposed()
    }

    /// Opens a new tab in this context sharing cookies, routes, emulation, and storage.
    pub async fn new_page(&self) -> Result<PageLease> {
        if self.is_closed() {
            return Err(Error::Other("browser context is closed".into()));
        }
        let page = self
            .guard
            .browser
            .create_page(
                &self.guard.context,
                self.navigation_timeout
                    .or(self.action_timeout)
                    .unwrap_or(self.guard.engine.config.navigation_timeout),
            )
            .await?;
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

    pub async fn add_init_script(&self, source: &str) -> Result<()> {
        if self.is_closed() {
            return Err(Error::Other("browser context is closed".into()));
        }
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

    pub async fn close(&self) -> Result<()> {
        self.guard.dispose(true).await;
        Ok(())
    }

    async fn dispose_failed(&self) {
        self.guard.dispose(false).await;
    }
}

impl PageLease {
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

    pub fn is_closed(&self) -> bool {
        self.released || self.context.is_disposed() || self.page.is_closed()
    }

    pub fn target_id(&self) -> &str {
        self.page.target_id()
    }

    pub fn opener_target_id(&self) -> Option<&str> {
        self.page.opener_id()
    }

    pub async fn bring_to_front(&self) -> Result<()> {
        Ok(self.page.bring_to_front(self.action_timeout()).await?)
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
        self.page
            .frame_goto(frame_id, &url, wait_until, self.navigation_timeout())
            .await?;
        Ok(())
    }

    pub async fn frame_evaluate(&self, frame_id: &str, expression: &str) -> Result<serde_json::Value> {
        Ok(self
            .page
            .frame_evaluate(frame_id, expression, self.action_timeout())
            .await?)
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
        Ok(self.page.storage_state(self.action_timeout()).await?)
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
        self.page
            .goto(&url, wait_until, self.navigation_timeout())
            .await?;
        Ok(())
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
        self.page.reload(wait_until, self.navigation_timeout()).await?;
        Ok(())
    }

    pub async fn go_back(&self) -> Result<bool> {
        self.go_back_with(fluxwright_cdp::WaitUntil::Load).await
    }

    pub async fn go_back_with(&self, wait_until: fluxwright_cdp::WaitUntil) -> Result<bool> {
        Ok(self.page.go_back(wait_until, self.navigation_timeout()).await?)
    }

    pub async fn go_forward(&self) -> Result<bool> {
        self.go_forward_with(fluxwright_cdp::WaitUntil::Load).await
    }

    pub async fn go_forward_with(&self, wait_until: fluxwright_cdp::WaitUntil) -> Result<bool> {
        Ok(self.page.go_forward(wait_until, self.navigation_timeout()).await?)
    }

    pub async fn set_content(&self, html: &str) -> Result<()> {
        self.set_content_with(html, fluxwright_cdp::WaitUntil::Load).await
    }

    pub async fn set_content_with(&self, html: &str, wait_until: fluxwright_cdp::WaitUntil) -> Result<()> {
        self.page
            .set_content(html, wait_until, self.navigation_timeout())
            .await?;
        Ok(())
    }

    pub async fn wait_for_load_state(&self, state: fluxwright_cdp::WaitUntil) -> Result<()> {
        self.page
            .wait_for_load_state(state, self.navigation_timeout())
            .await?;
        Ok(())
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
        Ok(self
            .page
            .wait_for_function(expression, polling, self.action_timeout())
            .await?)
    }

    pub async fn title(&self) -> Result<String> {
        Ok(self.page.title(self.action_timeout()).await?)
    }

    pub async fn content(&self) -> Result<String> {
        Ok(self.page.content(self.action_timeout()).await?)
    }

    pub async fn evaluate(&self, expression: &str) -> Result<serde_json::Value> {
        Ok(self
            .page
            .evaluate(expression, self.action_timeout())
            .await?)
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
        Ok(self
            .page
            .screenshot(options, self.action_timeout())
            .await?)
    }

    /// The page printed to PDF (headless Chrome only).
    pub async fn pdf(&self, options: &PdfOptions) -> Result<Vec<u8>> {
        self.pdf_within(options, fluxwright_cdp::MAX_PDF_BYTES).await
    }

    /// `pdf`, failing without reading on once the PDF is over `max_bytes`: for a caller that
    /// must bound what one job can make it hold.
    pub async fn pdf_within(&self, options: &PdfOptions, max_bytes: usize) -> Result<Vec<u8>> {
        Ok(self
            .page
            .pdf(options, max_bytes, self.action_timeout())
            .await?)
    }

    /// CSS-pixel viewport for this lease's page. Leases are fresh contexts, so it never leaks.
    pub async fn set_viewport_size(&self, width: u32, height: u32) -> Result<()> {
        Ok(self
            .page
            .set_viewport(width, height, self.action_timeout())
            .await?)
    }

    /// Selectors: CSS, `text=`, `role=` (see [`Selector`]), or a [`Selector`] with frames.
    pub async fn click(&self, selector: impl Into<Selector>) -> Result<()> {
        self.page
            .click(&selector.into(), self.action_timeout())
            .await?;
        Ok(())
    }

    pub async fn click_with(
        &self,
        selector: impl Into<Selector>,
        options: &fluxwright_cdp::ClickOptions,
    ) -> Result<()> {
        self.page
            .click_with(&selector.into(), options, self.action_timeout())
            .await?;
        Ok(())
    }

    pub async fn dblclick(
        &self,
        selector: impl Into<Selector>,
        options: &fluxwright_cdp::ClickOptions,
    ) -> Result<()> {
        self.page
            .dblclick(&selector.into(), options, self.action_timeout())
            .await?;
        Ok(())
    }

    pub async fn hover(
        &self,
        selector: impl Into<Selector>,
        options: &fluxwright_cdp::HoverOptions,
    ) -> Result<()> {
        self.page
            .hover(&selector.into(), options, self.action_timeout())
            .await?;
        Ok(())
    }

    pub async fn focus(&self, selector: impl Into<Selector>) -> Result<()> {
        self.page.focus(&selector.into(), self.action_timeout()).await?;
        Ok(())
    }

    pub async fn blur(&self, selector: impl Into<Selector>) -> Result<()> {
        self.page.blur(&selector.into(), self.action_timeout()).await?;
        Ok(())
    }

    pub async fn scroll_into_view_if_needed(&self, selector: impl Into<Selector>) -> Result<()> {
        self.page
            .scroll_into_view_if_needed(&selector.into(), self.action_timeout())
            .await?;
        Ok(())
    }

    pub async fn drag_and_drop(
        &self,
        source: impl Into<Selector>,
        target: impl Into<Selector>,
        force: bool,
    ) -> Result<()> {
        self.page
            .drag_and_drop(&source.into(), &target.into(), force, self.action_timeout())
            .await?;
        Ok(())
    }

    pub async fn fill(&self, selector: impl Into<Selector>, value: &str) -> Result<()> {
        self.page
            .fill(&selector.into(), value, self.action_timeout())
            .await?;
        Ok(())
    }

    pub async fn clear(&self, selector: impl Into<Selector>, force: bool) -> Result<()> {
        self.page
            .fill_with(&selector.into(), "", force, self.action_timeout())
            .await?;
        Ok(())
    }

    pub async fn press(
        &self,
        selector: impl Into<Selector>,
        key: &str,
        delay: Option<Duration>,
    ) -> Result<()> {
        self.page
            .press(&selector.into(), key, delay, self.action_timeout())
            .await?;
        Ok(())
    }

    pub async fn press_sequentially(
        &self,
        selector: impl Into<Selector>,
        text: &str,
        delay: Option<Duration>,
    ) -> Result<()> {
        self.page
            .press_sequentially(&selector.into(), text, delay, self.action_timeout())
            .await?;
        Ok(())
    }

    pub async fn set_checked(
        &self,
        selector: impl Into<Selector>,
        checked: bool,
        options: &fluxwright_cdp::CheckOptions,
    ) -> Result<()> {
        self.page
            .set_checked(&selector.into(), checked, options, self.action_timeout())
            .await?;
        Ok(())
    }

    pub async fn check(
        &self,
        selector: impl Into<Selector>,
        options: &fluxwright_cdp::CheckOptions,
    ) -> Result<()> {
        self.set_checked(selector, true, options).await
    }

    pub async fn uncheck(
        &self,
        selector: impl Into<Selector>,
        options: &fluxwright_cdp::CheckOptions,
    ) -> Result<()> {
        self.set_checked(selector, false, options).await
    }

    pub async fn select_option(
        &self,
        selector: impl Into<Selector>,
        items: &[fluxwright_cdp::SelectEntry],
        force: bool,
    ) -> Result<Vec<String>> {
        Ok(self
            .page
            .select_option(&selector.into(), items, force, self.action_timeout())
            .await?)
    }

    pub async fn set_input_files(
        &self,
        selector: impl Into<Selector>,
        files: &fluxwright_cdp::FileInput,
    ) -> Result<()> {
        self.page
            .set_input_files(&selector.into(), files, self.action_timeout())
            .await?;
        Ok(())
    }

    pub async fn tap(
        &self,
        selector: impl Into<Selector>,
        options: &fluxwright_cdp::HoverOptions,
    ) -> Result<()> {
        self.page
            .tap(&selector.into(), options, self.action_timeout())
            .await?;
        Ok(())
    }

    pub async fn mouse_move(&self, x: f64, y: f64, steps: u32) -> Result<()> {
        Ok(self.page.mouse_move(x, y, steps, self.action_timeout()).await?)
    }

    pub async fn mouse_down(
        &self,
        button: fluxwright_cdp::MouseButton,
        click_count: u32,
    ) -> Result<()> {
        Ok(self
            .page
            .mouse_down(button, click_count, self.action_timeout())
            .await?)
    }

    pub async fn mouse_up(
        &self,
        button: fluxwright_cdp::MouseButton,
        click_count: u32,
    ) -> Result<()> {
        Ok(self
            .page
            .mouse_up(button, click_count, self.action_timeout())
            .await?)
    }

    pub async fn mouse_click(
        &self,
        x: f64,
        y: f64,
        button: fluxwright_cdp::MouseButton,
        click_count: u32,
        delay: Option<Duration>,
    ) -> Result<()> {
        Ok(self
            .page
            .mouse_click(x, y, button, click_count, delay, self.action_timeout())
            .await?)
    }

    pub async fn mouse_dblclick(
        &self,
        x: f64,
        y: f64,
        button: fluxwright_cdp::MouseButton,
        delay: Option<Duration>,
    ) -> Result<()> {
        Ok(self
            .page
            .mouse_dblclick(x, y, button, delay, self.action_timeout())
            .await?)
    }

    pub async fn mouse_wheel(&self, delta_x: f64, delta_y: f64) -> Result<()> {
        Ok(self
            .page
            .mouse_wheel(delta_x, delta_y, self.action_timeout())
            .await?)
    }

    pub async fn keyboard_down(&self, key: &str) -> Result<()> {
        Ok(self.page.keyboard_down(key, self.action_timeout()).await?)
    }

    pub async fn keyboard_up(&self, key: &str) -> Result<()> {
        Ok(self.page.keyboard_up(key, self.action_timeout()).await?)
    }

    pub async fn keyboard_press(&self, key: &str, delay: Option<Duration>) -> Result<()> {
        Ok(self
            .page
            .keyboard_press(key, delay, self.action_timeout())
            .await?)
    }

    pub async fn keyboard_type(&self, text: &str, delay: Option<Duration>) -> Result<()> {
        Ok(self
            .page
            .keyboard_type(text, delay, self.action_timeout())
            .await?)
    }

    pub async fn keyboard_insert_text(&self, text: &str) -> Result<()> {
        Ok(self
            .page
            .keyboard_insert_text(text, self.action_timeout())
            .await?)
    }

    pub async fn touchscreen_tap(&self, x: f64, y: f64) -> Result<()> {
        Ok(self
            .page
            .touchscreen_tap(x, y, self.action_timeout())
            .await?)
    }

    pub async fn wait_for_selector(&self, selector: impl Into<Selector>) -> Result<()> {
        self.page
            .wait_for_selector(&selector.into(), self.action_timeout())
            .await?;
        Ok(())
    }

    /// Waits for the selector to be attached, detached, visible or hidden.
    pub async fn wait_for_selector_state(
        &self,
        selector: impl Into<Selector>,
        state: fluxwright_cdp::WaitState,
    ) -> Result<()> {
        Ok(self
            .page
            .wait_for_selector_state(&selector.into(), state, self.action_timeout())
            .await?)
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

    /// Hands every request of this page, its popups and iframes to the returned channel to
    /// fulfill, change or abort (Playwright's `page.route`). Call it before navigating. A request
    /// dropped unanswered continues; blocked resource types stay blocked.
    pub async fn intercept(&self) -> Result<tokio::sync::mpsc::UnboundedReceiver<InterceptedRequest>> {
        Ok(self.page.intercept(self.action_timeout()).await?)
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
        Ok(self
            .page
            .evaluate_on(&selector.into(), function, arg, self.action_timeout())
            .await?)
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
        Ok(self
            .page
            .element_screenshot(&selector.into(), options, self.action_timeout())
            .await?)
    }

    /// Enters an iframe (same- or cross-origin) by CSS selector.
    pub fn frame_locator(&self, iframe: &str) -> FrameLocator<'_> {
        FrameLocator {
            lease: self,
            frames: vec![iframe.to_string()],
        }
    }

    pub async fn close(mut self) -> Result<()> {
        self.release(true).await;
        Ok(())
    }

    async fn release(&mut self, completed: bool) {
        self.log_task.abort();
        self.popup_task.abort();
        if self.released {
            return;
        }
        self.released = true;
        if self.owns_context {
            self.context.dispose(completed).await;
        } else if !self.context.is_disposed() {
            let _ = self.context.browser.close_target(&self.page.target_id).await;
        }
    }
}

impl Drop for PageLease {
    fn drop(&mut self) {
        self.log_task.abort();
        self.popup_task.abort();
        if self.released {
            return;
        }
        self.released = true;
        if self.owns_context {
            self.context.dispose_in_background(true);
        } else if !self.context.is_disposed() {
            let browser = self.context.browser.clone();
            let target_id = self.page.target_id.clone();
            self.runtime.spawn(async move {
                let _ = browser.close_target(&target_id).await;
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

    /// PNG of the element, once it is visible and still.
    pub async fn screenshot(&self) -> Result<Vec<u8>> {
        self.lease.element_screenshot(&self.selector).await
    }

    /// PNG or JPEG of the element, once it is visible and still.
    pub async fn screenshot_with(&self, options: &ScreenshotOptions) -> Result<Vec<u8>> {
        self.lease.element_screenshot_with(&self.selector, options).await
    }

    pub async fn bounding_box(&self) -> Result<Option<BoundingBox>> {
        self.lease.bounding_box(&self.selector).await
    }

    /// `function` is JavaScript source taking the element and `arg`, e.g. `(el, n) => el.value.length > n`.
    pub async fn evaluate(&self, function: &str, arg: Option<serde_json::Value>) -> Result<serde_json::Value> {
        self.lease.evaluate_on(&self.selector, function, arg).await
    }

    pub async fn click(&self) -> Result<()> {
        self.lease.click(&self.selector).await
    }

    pub async fn click_with(&self, options: &fluxwright_cdp::ClickOptions) -> Result<()> {
        self.lease.click_with(&self.selector, options).await
    }

    pub async fn dblclick(&self) -> Result<()> {
        self.lease
            .dblclick(&self.selector, &fluxwright_cdp::ClickOptions::default())
            .await
    }

    pub async fn dblclick_with(&self, options: &fluxwright_cdp::ClickOptions) -> Result<()> {
        self.lease.dblclick(&self.selector, options).await
    }

    pub async fn hover(&self) -> Result<()> {
        self.lease
            .hover(&self.selector, &fluxwright_cdp::HoverOptions::default())
            .await
    }

    pub async fn hover_with(&self, options: &fluxwright_cdp::HoverOptions) -> Result<()> {
        self.lease.hover(&self.selector, options).await
    }

    pub async fn focus(&self) -> Result<()> {
        self.lease.focus(&self.selector).await
    }

    pub async fn blur(&self) -> Result<()> {
        self.lease.blur(&self.selector).await
    }

    pub async fn scroll_into_view_if_needed(&self) -> Result<()> {
        self.lease.scroll_into_view_if_needed(&self.selector).await
    }

    pub async fn drag_to(&self, target: &Locator<'_>) -> Result<()> {
        self.lease
            .drag_and_drop(&self.selector, &target.selector, false)
            .await
    }

    pub async fn drag_to_with(&self, target: &Locator<'_>, force: bool) -> Result<()> {
        self.lease
            .drag_and_drop(&self.selector, &target.selector, force)
            .await
    }

    pub async fn fill(&self, value: &str) -> Result<()> {
        self.lease.fill(&self.selector, value).await
    }

    pub async fn clear(&self, force: bool) -> Result<()> {
        self.lease.clear(&self.selector, force).await
    }

    pub async fn press(&self, key: &str, delay: Option<Duration>) -> Result<()> {
        self.lease.press(&self.selector, key, delay).await
    }

    pub async fn press_sequentially(&self, text: &str, delay: Option<Duration>) -> Result<()> {
        self.lease
            .press_sequentially(&self.selector, text, delay)
            .await
    }

    pub async fn check(&self) -> Result<()> {
        self.lease
            .check(&self.selector, &fluxwright_cdp::CheckOptions::default())
            .await
    }

    pub async fn check_with(&self, options: &fluxwright_cdp::CheckOptions) -> Result<()> {
        self.lease.check(&self.selector, options).await
    }

    pub async fn uncheck(&self) -> Result<()> {
        self.lease
            .uncheck(&self.selector, &fluxwright_cdp::CheckOptions::default())
            .await
    }

    pub async fn uncheck_with(&self, options: &fluxwright_cdp::CheckOptions) -> Result<()> {
        self.lease.uncheck(&self.selector, options).await
    }

    pub async fn set_checked(
        &self,
        checked: bool,
        options: &fluxwright_cdp::CheckOptions,
    ) -> Result<()> {
        self.lease.set_checked(&self.selector, checked, options).await
    }

    pub async fn select_option(
        &self,
        items: &[fluxwright_cdp::SelectEntry],
        force: bool,
    ) -> Result<Vec<String>> {
        self.lease.select_option(&self.selector, items, force).await
    }

    pub async fn set_input_files(&self, files: &fluxwright_cdp::FileInput) -> Result<()> {
        self.lease.set_input_files(&self.selector, files).await
    }

    pub async fn tap(&self) -> Result<()> {
        self.lease
            .tap(&self.selector, &fluxwright_cdp::HoverOptions::default())
            .await
    }

    pub async fn tap_with(&self, options: &fluxwright_cdp::HoverOptions) -> Result<()> {
        self.lease.tap(&self.selector, options).await
    }

    /// Waits until visible.
    pub async fn wait(&self) -> Result<()> {
        self.lease.wait_for_selector(&self.selector).await
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
        let engine = Engine::new(self.config, self.source);
        let n = engine.config().min_browsers;
        for _ in 0..n {
            let b = engine.inner.source.launch(&engine.config().launch).await?;
            let mut st = engine.inner.state.lock().await;
            st.browsers.push(Slot {
                browser: b,
                active: 0,
                jobs_served: 0,
                started: Instant::now(),
                last_used: Instant::now(),
                draining: false,
                crashed: false,
            });
        }
        Ok(engine)
    }
}

fn resolve_url(base: Option<&str>, url: &str) -> String {
    let Some(base) = base.filter(|_| !url.split(['/', '?', '#']).next().unwrap_or("").contains(':')) else {
        return url.to_string();
    };
    if url.starts_with("//") {
        if let Some((scheme, _)) = base.split_once("://") {
            return format!("{scheme}:{url}");
        }
    }
    if url.starts_with('/') {
        if let Some((scheme, rest)) = base.split_once("://") {
            let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
            return format!("{scheme}://{authority}{url}");
        }
    }
    let dir = match base.find("://").map(|i| i + 3) {
        Some(after_scheme) => match base[after_scheme..].rfind('/') {
            Some(slash) => &base[..=after_scheme + slash],
            None => return format!("{base}/{}", url.trim_start_matches("./")),
        },
        None => base,
    };
    format!("{dir}{}", url.trim_start_matches("./"))
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
}
