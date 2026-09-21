use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use fluxwright_cdp::{CdpBrowser, CdpPage, ContextId, ResourceType};
use tokio::sync::{Mutex, Notify};
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::config::{EngineConfig, JobOptions, Priority, QueueFullMode};
use crate::error::{Error, Result};
use crate::metrics::{Counters, MetricsSnapshot};
use crate::process::{current_process_rss_bytes, process_tree_rss_map, process_trees_rss_bytes};
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
    shutdown: Mutex<bool>,
    rss_cache: Mutex<(Instant, u64)>,
    grants: Arc<tokio::sync::Semaphore>,
}

struct State {
    browsers: Vec<Slot>,
    queue: VecDeque<Waiter>,
    counters: Counters,
    launching: usize,
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
    queued_at: Instant,
    tx: tokio::sync::oneshot::Sender<Result<PageLease>>,
}

pub struct PageLease {
    engine: Arc<Inner>,
    browser: Arc<CdpBrowser>,
    context: ContextId,
    page: CdpPage,
    lease_id: Uuid,
    released: bool,
}

impl Engine {
    pub fn new(config: EngineConfig, source: Arc<dyn BrowserSource>) -> Self {
        let inner = Arc::new(Inner {
            config,
            source,
            state: Mutex::new(State {
                browsers: Vec::new(),
                queue: VecDeque::new(),
                counters: Counters::default(),
                launching: 0,
            }),
            admit: Notify::new(),
            shutdown: Mutex::new(false),
            rss_cache: Mutex::new((Instant::now(), 0)),
            grants: Arc::new(tokio::sync::Semaphore::new(24)),
        });
        let pump = inner.clone();
        tokio::spawn(async move {
            loop {
                if *pump.shutdown.lock().await {
                    break;
                }
                let notified = pump.admit.notified();
                tokio::pin!(notified);
                let progress = Inner::dispatch(pump.clone()).await.unwrap_or(false);
                if !progress {
                    notified.await;
                }
            }
        });
        Self { inner }
    }

    pub fn builder() -> EngineBuilder {
        EngineBuilder {
            config: EngineConfig::default(),
        }
    }

    pub async fn acquire(&self) -> Result<PageLease> {
        self.acquire_with(Priority::Normal, self.inner.config.acquire_timeout)
            .await
    }

    pub async fn acquire_with(&self, priority: Priority, timeout: Duration) -> Result<PageLease> {
        if *self.inner.shutdown.lock().await {
            return Err(Error::ShuttingDown);
        }
        let (tx, rx) = tokio::sync::oneshot::channel();
        {
            let mut st = self.inner.state.lock().await;
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
                            {
                                let st = self.inner.state.lock().await;
                                if st.queue.len() < self.inner.config.queue_capacity {
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
                        let mut st = self.inner.state.lock().await;
                        st.queue.push_back(Waiter {
                            priority,
                            queued_at: Instant::now(),
                            tx,
                        });
                    }
                }
            } else {
                st.queue.push_back(Waiter {
                    priority,
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
                let page = self.acquire_with(opts.priority, self.inner.config.acquire_timeout).await?;
                apply_blocking(&page, &opts).await?;
                f.clone()(page).await
            })
            .await
            .map_err(|_| Error::JobTimeout);
            match result {
                Ok(Ok(v)) => return Ok(v),
                Ok(Err(e)) if e.is_retryable() && attempt < retries => {
                    warn!(error = %e, attempt, "retryable job failure");
                    attempt += 1;
                    continue;
                }
                Ok(Err(e)) => return Err(e),
                Err(_e) if attempt < retries => {
                    attempt += 1;
                    warn!(attempt, "job timeout, retrying");
                    continue;
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
                .filter(|s| !s.crashed)
                .map(|s| s.browser.pid)
                .collect();
            (
                pids.len(),
                st.browsers.iter().map(|s| s.active).sum::<usize>(),
                st.queue.len(),
                st.counters.reuse_rate(),
                st.counters.crash_count,
                st.counters.recycle_count,
                pids,
            )
        };
        let tree = tokio::task::spawn_blocking(move || process_trees_rss_bytes(&pids))
            .await
            .unwrap_or(0);
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
            let pids: Vec<u32> = rows.iter().map(|r| r.1).collect();
            (rows, pids)
        };
        let rss = tokio::task::spawn_blocking(move || process_tree_rss_map(&pids))
            .await
            .unwrap_or_default();
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
        *self.inner.shutdown.lock().await = true;
        let start = Instant::now();
        loop {
            {
                let mut st = self.inner.state.lock().await;
                while let Some(w) = st.queue.pop_front() {
                    let _ = w.tx.send(Err(Error::ShuttingDown));
                }
                let idle = st.browsers.iter().all(|s| s.active == 0);
                if idle || start.elapsed() > grace {
                    for s in st.browsers.drain(..) {
                        let _ = s.browser.close().await;
                    }
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        self.inner.admit.notify_waiters();
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

impl Inner {
    async fn dispatch(self: Arc<Self>) -> Result<bool> {
        self.reap().await;
        self.recycle().await;
        {
            let st = self.state.lock().await;
            let healthy = st
                .browsers
                .iter()
                .filter(|s| !s.crashed && !s.draining && !s.browser.connection_dead());
            let free: usize = healthy
                .map(|s| {
                    self.config
                        .max_contexts_per_browser
                        .saturating_sub(s.active)
                })
                .sum();
            let used = st.browsers.iter().filter(|s| !s.crashed).count() + st.launching;
            let launch_room = self.config.max_browsers.saturating_sub(used);
            if free + launch_room == 0 {
                return Ok(false);
            }
        }
        let permit = match self.grants.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => return Ok(false),
        };
        let next = {
            let mut st = self.state.lock().await;
            pick_waiter(&mut st.queue)
        };
        let Some(waiter) = next else {
            drop(permit);
            return Ok(false);
        };
        let inner = self.clone();
        tokio::spawn(async move {
            let _permit = permit;
            match inner.grant_lease().await {
                Ok(lease) => {
                    let _ = waiter.tx.send(Ok(lease));
                }
                Err(Error::NoBrowser) => {
                    let mut st = inner.state.lock().await;
                    st.queue.push_front(waiter);
                }
                Err(e) => {
                    let _ = waiter.tx.send(Err(e));
                }
            }
            inner.admit.notify_waiters();
        });
        Ok(true)
    }

    async fn browser_tree_rss(&self) -> u64 {
        let mut cache = self.rss_cache.lock().await;
        if cache.0.elapsed() < Duration::from_millis(500) {
            return cache.1;
        }
        let pids: Vec<u32> = {
            let st = self.state.lock().await;
            st.browsers
                .iter()
                .filter(|s| !s.crashed)
                .map(|s| s.browser.pid)
                .collect()
        };
        let tree = tokio::task::spawn_blocking(move || process_trees_rss_bytes(&pids))
            .await
            .unwrap_or(0);
        *cache = (Instant::now(), tree);
        tree
    }

    async fn grant_lease(self: &Arc<Self>) -> Result<PageLease> {
        if *self.shutdown.lock().await {
            return Err(Error::ShuttingDown);
        }
        let ceiling = self.config.memory_ceiling_mb * 1024 * 1024;
        let tree = self.browser_tree_rss().await;
        if tree > ceiling && ceiling > 0 {
            debug!(tree, ceiling, "rss ceiling hit");
            return Err(Error::NoBrowser);
        }

        let chosen = {
            let mut st = self.state.lock().await;
            let idx = st
                .browsers
                .iter()
                .enumerate()
                .filter(|(_, s)| {
                    !s.crashed
                        && !s.draining
                        && !s.browser.connection_dead()
                        && s.active < self.config.max_contexts_per_browser
                })
                .min_by_key(|(_, s)| s.active)
                .map(|(i, _)| i);
            if let Some(i) = idx {
                st.browsers[i].active += 1;
                st.counters.acquires += 1;
                st.counters.acquires_reused_browser += 1;
                Some(st.browsers[i].browser.clone())
            } else {
                let can_launch = st.browsers.iter().filter(|s| !s.crashed).count() + st.launching
                    < self.config.max_browsers;
                if can_launch {
                    st.launching += 1;
                    None
                } else {
                    return Err(Error::NoBrowser);
                }
            }
        };

        let browser = if let Some(b) = chosen {
            b
        } else {
            let launched = self.source.launch(&self.config.launch).await;
            {
                let mut st = self.state.lock().await;
                st.launching = st.launching.saturating_sub(1);
            }
            let browser = launched?;
            let mut st = self.state.lock().await;
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
            browser
        };

        let ctx = match browser.create_context().await {
            Ok(c) => c,
            Err(e) => {
                self.release_slot(&browser, false).await;
                return Err(e.into());
            }
        };
        let page = match browser
            .create_page(&ctx, self.config.navigation_timeout)
            .await
        {
            Ok(p) => p,
            Err(e) => {
                let _ = browser.dispose_context(&ctx).await;
                self.release_slot(&browser, false).await;
                return Err(e.into());
            }
        };

        info!(
            browser = %browser.id.0,
            context = %ctx.0,
            "lease granted"
        );

        Ok(PageLease {
            engine: self.clone(),
            browser,
            context: ctx,
            page,
            lease_id: Uuid::new_v4(),
            released: false,
        })
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
                drop(st);
                warn!(pid = slot.browser.pid, "removing dead browser");
                let _ = slot.browser.close().await;
                st = self.state.lock().await;
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
                    .filter(|s| !s.crashed)
                    .map(|s| s.browser.pid)
                    .collect()
            };
            tokio::task::spawn_blocking(move || process_tree_rss_map(&pids))
                .await
                .unwrap_or_default()
        } else {
            std::collections::HashMap::new()
        };
        let mut to_close = Vec::new();
        let mut bump = 0u64;
        {
            let mut st = self.state.lock().await;
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
                let by_rss = rss_limit.is_some_and(|mb| {
                    rss_by_pid.get(&s.browser.pid).copied().unwrap_or(0) / (1024 * 1024) >= mb
                });
                let idle = self.config.idle_timeout.is_some_and(|d| {
                    s.active == 0 && now.duration_since(s.last_used) >= d
                });
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
        for b in to_close {
            let _ = b.close().await;
        }
    }
}

fn pick_waiter(queue: &mut VecDeque<Waiter>) -> Option<Waiter> {
    let best_pri = queue.iter().map(|w| w.priority).max()?;
    let idx = queue
        .iter()
        .enumerate()
        .filter(|(_, w)| w.priority == best_pri)
        .min_by_key(|(_, w)| w.queued_at)
        .map(|(i, _)| i)?;
    queue.remove(idx)
}

async fn apply_blocking(page: &PageLease, opts: &JobOptions) -> Result<()> {
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
        page.page
            .block_resource_types(&types, Duration::from_secs(5))
            .await?;
    }
    if !opts.block_url_patterns.is_empty() {
        page.page
            .set_blocked_urls(&opts.block_url_patterns, Duration::from_secs(5))
            .await?;
    }
    Ok(())
}

impl PageLease {
    pub fn browser_pid(&self) -> u32 {
        self.browser.pid
    }

    pub fn lease_id(&self) -> Uuid {
        self.lease_id
    }

    pub async fn goto(&self, url: &str) -> Result<()> {
        self.page
            .goto(url, self.engine.config.navigation_timeout)
            .await?;
        Ok(())
    }

    pub async fn title(&self) -> Result<String> {
        Ok(self.page.title(self.engine.config.action_timeout).await?)
    }

    pub async fn content(&self) -> Result<String> {
        Ok(self.page.content(self.engine.config.action_timeout).await?)
    }

    pub async fn evaluate(&self, expression: &str) -> Result<serde_json::Value> {
        Ok(self
            .page
            .evaluate(expression, self.engine.config.action_timeout)
            .await?)
    }

    pub async fn screenshot(&self) -> Result<Vec<u8>> {
        Ok(self
            .page
            .screenshot(self.engine.config.action_timeout)
            .await?)
    }

    pub async fn click(&self, selector: &str) -> Result<()> {
        self.page
            .click(selector, self.engine.config.action_timeout)
            .await?;
        Ok(())
    }

    pub async fn fill(&self, selector: &str, value: &str) -> Result<()> {
        self.page
            .fill(selector, value, self.engine.config.action_timeout)
            .await?;
        Ok(())
    }

    pub async fn wait_for_selector(&self, selector: &str) -> Result<()> {
        self.page
            .wait_for_selector(selector, self.engine.config.action_timeout)
            .await?;
        Ok(())
    }

    pub fn locator(&self, selector: impl Into<String>) -> Locator<'_> {
        Locator {
            lease: self,
            selector: selector.into(),
        }
    }

    pub async fn close(mut self) -> Result<()> {
        self.release(true).await;
        Ok(())
    }

    async fn release(&mut self, completed: bool) {
        if self.released {
            return;
        }
        self.released = true;
        let _ = self.browser.dispose_context(&self.context).await;
        self.engine.release_slot(&self.browser, completed).await;
    }
}

impl Drop for PageLease {
    fn drop(&mut self) {
        if self.released {
            return;
        }
        self.released = true;
        let browser = self.browser.clone();
        let ctx = self.context.clone();
        let engine = self.engine.clone();
        tokio::spawn(async move {
            let _ = browser.dispose_context(&ctx).await;
            engine.release_slot(&browser, true).await;
        });
    }
}

pub struct Locator<'a> {
    lease: &'a PageLease,
    selector: String,
}

impl Locator<'_> {
    pub async fn click(&self) -> Result<()> {
        self.lease.click(&self.selector).await
    }

    pub async fn fill(&self, value: &str) -> Result<()> {
        self.lease.fill(&self.selector, value).await
    }

    pub async fn wait(&self) -> Result<()> {
        self.lease.wait_for_selector(&self.selector).await
    }
}

pub struct EngineBuilder {
    config: EngineConfig,
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
    pub fn max_pages_per_context(mut self, n: usize) -> Self {
        self.config.max_pages_per_context = n;
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
    pub fn no_sandbox(mut self, v: bool) -> Self {
        self.config.launch.no_sandbox = v;
        self
    }
    pub fn headless(mut self, v: bool) -> Self {
        self.config.launch.headless = v;
        self
    }
    pub fn source(self, _source: Arc<dyn BrowserSource>) -> EngineBuilderWithSource {
        EngineBuilderWithSource {
            config: self.config,
            source: _source,
        }
    }
    pub async fn build(self) -> Result<Engine> {
        EngineBuilderWithSource {
            config: self.config,
            source: Arc::new(crate::source::LocalChromium),
        }
        .build()
        .await
    }
}

pub struct EngineBuilderWithSource {
    config: EngineConfig,
    source: Arc<dyn BrowserSource>,
}

impl EngineBuilderWithSource {
    pub async fn build(self) -> Result<Engine> {
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
