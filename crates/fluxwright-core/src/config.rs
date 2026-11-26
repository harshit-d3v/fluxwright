use std::path::PathBuf;
use std::time::Duration;

use fluxwright_cdp::{
    ColorScheme, Emulation, Geolocation, LaunchOptions, Proxy, RecordHarOptions, ResourceType,
    StorageState,
};

use crate::adaptive::AdaptiveConfig;
use crate::proxies::ProxyPool;
use crate::video::RecordVideoOptions;

/// Curated URL wildcard patterns matching common Consent Management Platform (CMP) / cookie-banner scripts.
pub const COOKIE_BANNER_URL_PATTERNS: &[&str] = &[
    "*cdn.cookielaw.org/*",
    "*geolocation.onetrust.com/*",
    "*consent.cookiebot.com/*",
    "*consentcdn.cookiebot.com/*",
    "*consent.trustarc.com/*",
    "*sdk.privacy-center.org/*",
    "*cdn.privacy-mgmt.com/*",
    "*app.usercentrics.eu/*",
    "*cmp.osano.com/*",
    "*cdn.iubenda.com/cs/*",
    "*consentmanager.net/*",
    "*quantcast.mgr.consensu.org/*",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourcePreset {
    /// Block images, fonts, and media; keep HTML, CSS, and scripts intact.
    Scraping,
    /// Block images, fonts, media, and stylesheets; keep HTML and scripts intact.
    Minimal,
    /// Block images and media; keep fonts, stylesheets, and scripts intact.
    NoMedia,
}

impl ResourcePreset {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "scraping" => Some(Self::Scraping),
            "minimal" => Some(Self::Minimal),
            "no-media" | "no_media" => Some(Self::NoMedia),
            _ => None,
        }
    }

    pub fn apply(self, opts: &mut JobOptions) {
        match self {
            Self::Scraping => {
                opts.block_images = true;
                opts.block_fonts = true;
                opts.block_media = true;
            }
            Self::Minimal => {
                opts.block_images = true;
                opts.block_fonts = true;
                opts.block_media = true;
                opts.block_stylesheets = true;
            }
            Self::NoMedia => {
                opts.block_images = true;
                opts.block_media = true;
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum Priority {
    Low = 0,
    #[default]
    Normal = 1,
    High = 2,
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
    /// Pre-warmed blank context + `about:blank` page slots kept ready per running browser.
    pub prewarm_contexts: usize,
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
    /// Pages that may be on one host at once, across the fleet. `None` (and 0): no limit.
    pub max_concurrency_per_host: Option<usize>,
    /// The least time between the starts of two `goto`s to one host. `None`: no delay.
    pub min_delay_per_host: Option<Duration>,
    /// Optional directory for named persistent browser profiles (`storageState`).
    pub profiles_dir: Option<PathBuf>,
    /// Optional pool of outbound proxies used when `JobOptions::proxy` is unset.
    pub proxy_pool: Option<ProxyPool>,
    /// Optional AIMD adaptive concurrency limiter driven by memory pressure and job failures.
    pub adaptive_concurrency: Option<AdaptiveConfig>,
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
    let cgroup = sys.cgroup_limits().map(|c| (c.total_memory, c.rss));
    (cpus, free_mb(sys.available_memory(), cgroup))
}

/// Memory free now (MB): the host's available memory, or less inside a container: its limit
/// minus its anonymous memory (`cgroup` is that pair, in bytes). Page cache counts as free, as it
/// does on the host; sysinfo's own cgroup "free" subtracts memory.current, which includes cache,
/// so a container that had read 2.5 GB of files looked nearly full.
fn free_mb(host_available: u64, cgroup: Option<(u64, u64)>) -> u64 {
    let free = cgroup.map_or(host_available, |(limit, anon)| host_available.min(limit.saturating_sub(anon)));
    free / (1024 * 1024)
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
            prewarm_contexts: 0,
            memory_ceiling_mb,
            idle_timeout: Some(Duration::from_secs(300)),
            recycle_after_jobs: Some(100),
            recycle_after: Some(Duration::from_secs(30 * 60)),
            recycle_rss_mb: None,
            queue_capacity: 256,
            queue_full_mode: QueueFullMode::Error,
            launch: LaunchOptions {
                no_sandbox,
                low_memory: false,
                ..LaunchOptions::default()
            },
            acquire_timeout: Duration::from_secs(30),
            navigation_timeout: Duration::from_secs(30),
            action_timeout: Duration::from_secs(10),
            job_timeout: Duration::from_secs(60),
            max_concurrency_per_host: None,
            min_delay_per_host: None,
            profiles_dir: None,
            proxy_pool: None,
            adaptive_concurrency: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct JobOptions {
    pub priority: Priority,
    pub timeout: Option<Duration>,
    /// Runs after a browser died mid-job (see `Error::is_retryable`). A job that timed out is
    /// not run again: it may already have clicked or submitted something.
    pub retries: u32,
    pub block_images: bool,
    pub block_fonts: bool,
    pub block_media: bool,
    pub block_stylesheets: bool,
    pub block_scripts: bool,
    pub block_url_patterns: Vec<String>,
    /// Block curated Consent Management Platform (CMP) / cookie-banner URL patterns.
    pub block_cookie_banners: bool,
    /// Proxy for this job's browser context. Other jobs on the same browser are unaffected.
    pub proxy: Option<Proxy>,
    /// User agent, locale, timezone, geolocation, viewport, scale, color scheme.
    pub emulation: Emulation,
    /// Playwright names: `geolocation`, `notifications`, `clipboard-read`, ...
    pub permissions: Vec<String>,
    /// Cookies and localStorage to start from, e.g. saved by `PageLease::storage_state`.
    pub storage_state: Option<StorageState>,
    /// Base URL for relative `goto` calls.
    pub base_url: Option<String>,
    /// HTTP Basic/Digest authentication credentials.
    pub http_credentials: Option<(String, String)>,
    /// HAR 1.2 recording options for this browser context.
    pub record_har: Option<RecordHarOptions>,
    /// Video recording options for pages in this browser context.
    pub record_video: Option<RecordVideoOptions>,
    /// Named persistent profile (loads `storageState` on acquire, saves on clean completion).
    pub profile: Option<String>,
    /// Persist updated `storageState` back to `profile` on clean completion (default `true`).
    pub persist_profile: bool,
    /// Sticky session affinity key for `EngineConfig::proxy_pool`.
    pub proxy_session: Option<String>,
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
            block_stylesheets: false,
            block_scripts: false,
            block_url_patterns: Vec::new(),
            block_cookie_banners: false,
            proxy: None,
            emulation: Emulation::default(),
            permissions: Vec::new(),
            storage_state: None,
            base_url: None,
            http_credentials: None,
            record_har: None,
            record_video: None,
            profile: None,
            persist_profile: true,
            proxy_session: None,
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

    pub fn record_har(mut self, opts: RecordHarOptions) -> Self {
        self.record_har = Some(opts);
        self
    }

    pub fn record_video(mut self, opts: RecordVideoOptions) -> Self {
        self.record_video = Some(opts);
        self
    }

    pub fn block_images(mut self, v: bool) -> Self {
        self.block_images = v;
        self
    }

    pub fn block_fonts(mut self, v: bool) -> Self {
        self.block_fonts = v;
        self
    }

    pub fn block_media(mut self, v: bool) -> Self {
        self.block_media = v;
        self
    }

    pub fn block_stylesheets(mut self, v: bool) -> Self {
        self.block_stylesheets = v;
        self
    }

    pub fn block_scripts(mut self, v: bool) -> Self {
        self.block_scripts = v;
        self
    }

    pub fn block_resources<I>(mut self, types: I) -> Self
    where
        I: IntoIterator<Item = ResourceType>,
    {
        for t in types {
            match t {
                ResourceType::Image => self.block_images = true,
                ResourceType::Font => self.block_fonts = true,
                ResourceType::Media => self.block_media = true,
                ResourceType::Stylesheet => self.block_stylesheets = true,
                ResourceType::Script => self.block_scripts = true,
            }
        }
        self
    }

    pub fn block_url_patterns<I, S>(mut self, patterns: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.block_url_patterns
            .extend(patterns.into_iter().map(Into::into));
        self
    }

    pub fn block_cookie_banners(mut self, block: bool) -> Self {
        self.block_cookie_banners = block;
        self
    }

    pub fn resource_preset(mut self, preset: ResourcePreset) -> Self {
        preset.apply(&mut self);
        self
    }

    pub fn profile(mut self, name: impl Into<String>) -> Self {
        self.profile = Some(name.into());
        self
    }

    pub fn persist_profile(mut self, persist: bool) -> Self {
        self.persist_profile = persist;
        self
    }

    pub fn proxy_session(mut self, session: impl Into<String>) -> Self {
        self.proxy_session = Some(session.into());
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_preset_parses_and_applies() {
        assert_eq!(ResourcePreset::parse("scraping"), Some(ResourcePreset::Scraping));
        assert_eq!(ResourcePreset::parse("MINIMAL"), Some(ResourcePreset::Minimal));
        assert_eq!(ResourcePreset::parse("no-media"), Some(ResourcePreset::NoMedia));
        assert_eq!(ResourcePreset::parse("no_media"), Some(ResourcePreset::NoMedia));
        assert_eq!(ResourcePreset::parse("unknown"), None);

        let s = JobOptions::default().resource_preset(ResourcePreset::Scraping);
        assert!(s.block_images && s.block_fonts && s.block_media);
        assert!(!s.block_stylesheets && !s.block_scripts);

        let m = JobOptions::default().resource_preset(ResourcePreset::Minimal);
        assert!(m.block_images && m.block_fonts && m.block_media && m.block_stylesheets);
        assert!(!m.block_scripts);

        let r = JobOptions::default()
            .block_resources([ResourceType::Script, ResourceType::Image])
            .block_url_patterns(["**/*.png", "**/analytics*"]);
        assert!(r.block_scripts && r.block_images);
        assert!(!r.block_fonts);
        assert_eq!(r.block_url_patterns, vec!["**/*.png", "**/analytics*"]);
        assert!(!r.block_cookie_banners);

        let cb = JobOptions::default().block_cookie_banners(true);
        assert!(cb.block_cookie_banners);
        assert_eq!(COOKIE_BANNER_URL_PATTERNS.len(), 12);
        assert!(COOKIE_BANNER_URL_PATTERNS.contains(&"*cdn.cookielaw.org/*"));
        assert!(COOKIE_BANNER_URL_PATTERNS.contains(&"*quantcast.mgr.consensu.org/*"));
    }

    #[test]
    fn auto_tune_sizes_the_fleet_to_the_machine() {
        assert_eq!(auto_tune(16, 10_000), (4, 7_500));
        assert_eq!(auto_tune(4, 2_048), (1, 1_536));
        assert_eq!(auto_tune(1, 0), (1, 8_192), "memory unknown: the old fixed budget");
        assert_eq!(auto_tune(64, 1), (16, 1), "never 0 MB, which would mean no ceiling");
        let config = EngineConfig::default();
        assert!(config.max_browsers >= 1 && config.memory_ceiling_mb >= 1);
        assert!(!config.launch.low_memory);
        assert!(config.recycle_rss_mb.is_none());
    }

    #[test]
    fn containers_count_page_cache_as_free() {
        const MB: u64 = 1024 * 1024;
        // A 4 GB container using 512 MB itself, on a host with 16 GB available.
        assert_eq!(free_mb(16_384 * MB, Some((4_096 * MB, 512 * MB))), 3_584);
        // The host has less available than the container's limit.
        assert_eq!(free_mb(2_048 * MB, Some((4_096 * MB, 512 * MB))), 2_048);
        assert_eq!(free_mb(8_192 * MB, None), 8_192);
    }
}
