use std::time::Duration;

use fluxwright_cdp::{ColorScheme, Emulation, Geolocation, LaunchOptions, Proxy, StorageState};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Priority {
    Low = 0,
    Normal = 1,
    High = 2,
}

impl Default for Priority {
    fn default() -> Self {
        Self::Normal
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueFullMode {
    /// `acquire` returns `QueueFull` when the waiter queue is at capacity.
    Error,
    /// `acquire` parks until it can enter the queue or take a lease.
    Wait,
}

#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub min_browsers: usize,
    pub max_browsers: usize,
    pub max_contexts_per_browser: usize,
    pub max_pages_per_context: usize,
    pub memory_ceiling_mb: u64,
    pub idle_timeout: Option<Duration>,
    pub recycle_after_jobs: Option<u64>,
    pub recycle_after: Option<Duration>,
    pub recycle_rss_mb: Option<u64>,
    pub queue_capacity: usize,
    pub queue_full_mode: QueueFullMode,
    pub launch: LaunchOptions,
    pub acquire_timeout: Duration,
    pub navigation_timeout: Duration,
    pub action_timeout: Duration,
    pub job_timeout: Duration,
}

/// Browsers and the fleet's memory budget (MB) for a machine with `cpus` CPUs and
/// `available_mb` of memory free: a browser per 4 CPUs, so 8 pages each makes about 2 pages
/// per CPU, and 75% of the free memory. The budget is what keeps the fleet from overcommitting:
/// a lease waits while the measured fleet is over it (one Chrome is about 50 MB and a light page
/// 25 MB more), so more browsers than a small budget can fill cost nothing.
fn auto_tune(cpus: usize, available_mb: u64) -> (usize, u64) {
    let browsers = cpus.div_ceil(4).max(1);
    // 0 means memory unknown: keep the old fixed budget. Never 0 MB, which means no ceiling.
    let budget = if available_mb == 0 { 8192 } else { (available_mb * 3 / 4).max(1) };
    (browsers, budget)
}

/// CPUs this process may use, and memory (MB) free now within a container's limit.
fn this_machine() -> (usize, u64) {
    let cpus = std::thread::available_parallelism().map_or(1, |n| n.get());
    let mut sys = sysinfo::System::new();
    sys.refresh_memory();
    let mut free = sys.available_memory();
    if let Some(cgroup) = sys.cgroup_limits() {
        free = free.min(cgroup.free_memory);
    }
    (cpus, free / (1024 * 1024))
}

impl Default for EngineConfig {
    /// Sized to this machine when called (see [`auto_tune`]); the rest are fixed defaults.
    fn default() -> Self {
        let no_sandbox = std::env::var("FLUXWRIGHT_NO_SANDBOX")
            .ok()
            .filter(|v| v != "0" && !v.eq_ignore_ascii_case("false"))
            .is_some();
        let (cpus, available_mb) = this_machine();
        let (max_browsers, memory_ceiling_mb) = auto_tune(cpus, available_mb);
        Self {
            min_browsers: 0,
            max_browsers,
            max_contexts_per_browser: 8,
            max_pages_per_context: 1,
            memory_ceiling_mb,
            idle_timeout: Some(Duration::from_secs(300)),
            recycle_after_jobs: Some(100),
            recycle_after: Some(Duration::from_secs(30 * 60)),
            recycle_rss_mb: None,
            queue_capacity: 256,
            queue_full_mode: QueueFullMode::Error,
            launch: LaunchOptions {
                no_sandbox,
                ..LaunchOptions::default()
            },
            acquire_timeout: Duration::from_secs(30),
            navigation_timeout: Duration::from_secs(30),
            action_timeout: Duration::from_secs(10),
            job_timeout: Duration::from_secs(60),
        }
    }
}

#[derive(Debug, Clone)]
pub struct JobOptions {
    pub priority: Priority,
    pub timeout: Option<Duration>,
    pub retries: u32,
    pub block_images: bool,
    pub block_fonts: bool,
    pub block_media: bool,
    pub block_url_patterns: Vec<String>,
    /// Proxy for this job's browser context. Other jobs on the same browser are unaffected.
    pub proxy: Option<Proxy>,
    /// User agent, locale, timezone, geolocation, viewport, scale, color scheme.
    pub emulation: Emulation,
    /// Playwright names: `geolocation`, `notifications`, `clipboard-read`, ...
    pub permissions: Vec<String>,
    /// Cookies and localStorage to start from, e.g. saved by `PageLease::storage_state`.
    pub storage_state: Option<StorageState>,
}

impl Default for JobOptions {
    fn default() -> Self {
        Self {
            priority: Priority::Normal,
            timeout: None,
            retries: 1,
            block_images: false,
            block_fonts: false,
            block_media: false,
            block_url_patterns: Vec::new(),
            proxy: None,
            emulation: Emulation::default(),
            permissions: Vec::new(),
            storage_state: None,
        }
    }
}

impl JobOptions {
    pub fn priority(mut self, p: Priority) -> Self {
        self.priority = p;
        self
    }

    pub fn timeout(mut self, t: Duration) -> Self {
        self.timeout = Some(t);
        self
    }

    pub fn retries(mut self, n: u32) -> Self {
        self.retries = n;
        self
    }

    pub fn proxy(mut self, proxy: Proxy) -> Self {
        self.proxy = Some(proxy);
        self
    }

    pub fn user_agent(mut self, ua: impl Into<String>) -> Self {
        self.emulation.user_agent = Some(ua.into());
        self
    }

    /// BCP 47, e.g. `de-DE`.
    pub fn locale(mut self, locale: impl Into<String>) -> Self {
        self.emulation.locale = Some(locale.into());
        self
    }

    /// IANA, e.g. `Asia/Tokyo`.
    pub fn timezone(mut self, id: impl Into<String>) -> Self {
        self.emulation.timezone_id = Some(id.into());
        self
    }

    /// Also needs `permissions(["geolocation"])`, as in Playwright.
    pub fn geolocation(mut self, latitude: f64, longitude: f64) -> Self {
        self.emulation.geolocation = Some(Geolocation { latitude, longitude, accuracy: 0.0 });
        self
    }

    pub fn viewport(mut self, width: u32, height: u32) -> Self {
        self.emulation.viewport = Some((width, height));
        self
    }

    pub fn device_scale_factor(mut self, factor: f64) -> Self {
        self.emulation.device_scale_factor = Some(factor);
        self
    }

    pub fn color_scheme(mut self, scheme: ColorScheme) -> Self {
        self.emulation.color_scheme = Some(scheme);
        self
    }

    pub fn permissions<I, S>(mut self, names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.permissions = names.into_iter().map(Into::into).collect();
        self
    }

    pub fn storage_state(mut self, state: StorageState) -> Self {
        self.storage_state = Some(state);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_tune_sizes_the_fleet_to_the_machine() {
        assert_eq!(auto_tune(16, 10_000), (4, 7_500));
        assert_eq!(auto_tune(4, 2_048), (1, 1_536));
        assert_eq!(auto_tune(1, 0), (1, 8_192), "memory unknown: the old fixed budget");
        assert_eq!(auto_tune(64, 1), (16, 1), "never 0 MB, which would mean no ceiling");
        let config = EngineConfig::default();
        assert!(config.max_browsers >= 1 && config.memory_ceiling_mb >= 1);
    }
}
