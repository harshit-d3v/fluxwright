use serde::Serialize;

#[derive(Debug, Clone, Default, Serialize)]
pub struct MetricsSnapshot {
    pub browsers: usize,
    pub contexts: usize,
    pub pages: usize,
    pub active_leases: usize,
    pub queued_requests: usize,
    pub browser_reuse_rate: f64,
    pub crash_count: u64,
    pub recycle_count: u64,
    pub engine_rss_bytes: u64,
    pub browser_tree_rss_bytes: u64,
    /// Browser process-tree RSS divided by active leases. An estimate.
    pub memory_per_job_bytes_estimate: u64,
}

#[derive(Debug, Clone, Default)]
pub struct Counters {
    pub acquires: u64,
    pub acquires_reused_browser: u64,
    pub crash_count: u64,
    pub recycle_count: u64,
}

impl Counters {
    pub fn reuse_rate(&self) -> f64 {
        if self.acquires == 0 {
            0.0
        } else {
            self.acquires_reused_browser as f64 / self.acquires as f64
        }
    }
}
