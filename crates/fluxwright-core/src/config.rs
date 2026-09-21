use std::time::Duration;

use fluxwright_cdp::LaunchOptions;

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

impl Default for EngineConfig {
    fn default() -> Self {
        let no_sandbox = std::env::var("FLUXWRIGHT_NO_SANDBOX")
            .ok()
            .filter(|v| v != "0" && !v.eq_ignore_ascii_case("false"))
            .is_some();
        Self {
            min_browsers: 0,
            max_browsers: 4,
            max_contexts_per_browser: 8,
            max_pages_per_context: 1,
            memory_ceiling_mb: 8192,
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
}
