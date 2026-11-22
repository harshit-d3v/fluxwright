//! Adaptive AIMD concurrency controller driven by browser tree memory pressure and failures.

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Configures optional AIMD adaptive concurrency limiting for [`crate::Engine`].
#[derive(Debug, Clone)]
pub struct AdaptiveConfig {
    pub min_concurrency: usize,
    /// `0` means cap automatically at `max_browsers * max_contexts_per_browser`.
    pub max_concurrency: usize,
    /// Initial concurrency limit (`None` starts at half of `max_concurrency`, clamped >= `min`).
    pub initial_concurrency: Option<usize>,
    /// Grow ceiling when queue demand exists and RSS / memory_ceiling is below this ratio (default `0.80`).
    pub target_memory_utilization: f64,
    /// Shrink limit multiplicatively when RSS / memory_ceiling exceeds this ratio (default `0.90`).
    pub high_memory_utilization: f64,
    /// Minimum time between consecutive adjustments (default `2s`).
    pub step_interval: Duration,
}

impl Default for AdaptiveConfig {
    fn default() -> Self {
        Self {
            min_concurrency: 1,
            max_concurrency: 0,
            initial_concurrency: None,
            target_memory_utilization: 0.80,
            high_memory_utilization: 0.90,
            step_interval: Duration::from_secs(2),
        }
    }
}

struct AdaptiveState {
    min: usize,
    max: usize,
    limit: usize,
    target_util: f64,
    high_util: f64,
    step_interval: Duration,
    last_change: Option<Instant>,
}

pub(crate) struct AdaptiveController {
    state: Mutex<AdaptiveState>,
}

impl AdaptiveController {
    pub(crate) fn new(cfg: AdaptiveConfig, hard_max: usize) -> Self {
        let min = cfg.min_concurrency.max(1);
        let max_raw = if cfg.max_concurrency == 0 {
            hard_max
        } else {
            cfg.max_concurrency.min(hard_max)
        };
        let max = max_raw.max(min);
        let init = cfg
            .initial_concurrency
            .unwrap_or_else(|| (max / 2).max(min))
            .clamp(min, max);
        let target_util = cfg.target_memory_utilization.clamp(0.30, 0.95);
        let high_util = cfg.high_memory_utilization.clamp(target_util, 0.98);
        let step_interval = cfg.step_interval.max(Duration::from_millis(50));
        Self {
            state: Mutex::new(AdaptiveState {
                min,
                max,
                limit: init,
                target_util,
                high_util,
                step_interval,
                last_change: None,
            }),
        }
    }

    pub(crate) fn limit(&self) -> usize {
        self.state.lock().unwrap().limit
    }

    pub(crate) fn observe_rss(&self, tree_rss_bytes: u64, ceiling_bytes: u64) {
        self.observe_rss_at(tree_rss_bytes, ceiling_bytes, Instant::now());
    }

    pub(crate) fn on_outcome(
        &self,
        completed: bool,
        has_queue_demand: bool,
        tree_rss_bytes: u64,
        ceiling_bytes: u64,
    ) {
        self.on_outcome_at(
            completed,
            has_queue_demand,
            tree_rss_bytes,
            ceiling_bytes,
            Instant::now(),
        );
    }

    fn observe_rss_at(&self, tree_rss_bytes: u64, ceiling_bytes: u64, now: Instant) {
        if ceiling_bytes == 0 {
            return;
        }
        let util = tree_rss_bytes as f64 / ceiling_bytes as f64;
        let mut st = self.state.lock().unwrap();
        if util >= st.high_util {
            let interval = if util >= 0.95 {
                st.step_interval.min(Duration::from_millis(500))
            } else {
                st.step_interval
            };
            if st.last_change.is_none_or(|t| now.duration_since(t) >= interval) {
                let next = ((st.limit as f64 * 0.75).floor() as usize).max(st.min);
                if next != st.limit {
                    st.limit = next;
                    st.last_change = Some(now);
                }
            }
        }
    }

    fn on_outcome_at(
        &self,
        completed: bool,
        has_queue_demand: bool,
        tree_rss_bytes: u64,
        ceiling_bytes: u64,
        now: Instant,
    ) {
        let mut st = self.state.lock().unwrap();
        if st
            .last_change
            .is_some_and(|t| now.duration_since(t) < st.step_interval)
        {
            return;
        }
        if !completed {
            let next = ((st.limit as f64 * 0.75).floor() as usize).max(st.min);
            if next != st.limit {
                st.limit = next;
                st.last_change = Some(now);
            }
            return;
        }
        let mem_ok = ceiling_bytes == 0
            || (tree_rss_bytes as f64 / ceiling_bytes as f64) < st.target_util;
        if has_queue_demand && mem_ok {
            let next = (st.limit + 1).min(st.max);
            if next != st.limit {
                st.limit = next;
                st.last_change = Some(now);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aimd_scales_up_under_low_rss_and_backs_off_under_pressure_or_failure() {
        let ctrl = AdaptiveController::new(
            AdaptiveConfig {
                min_concurrency: 2,
                max_concurrency: 16,
                initial_concurrency: Some(8),
                target_memory_utilization: 0.80,
                high_memory_utilization: 0.90,
                step_interval: Duration::from_secs(2),
            },
            16,
        );
        assert_eq!(ctrl.limit(), 8);

        let t0 = Instant::now();
        // Low RSS + queue demand -> additive increase (+1).
        ctrl.on_outcome_at(true, true, 500, 1000, t0);
        assert_eq!(ctrl.limit(), 9);

        // Cooldown gate blocks immediate bump.
        ctrl.on_outcome_at(true, true, 500, 1000, t0 + Duration::from_millis(500));
        assert_eq!(ctrl.limit(), 9);

        // Between target (80%) and high (90%) holds steady.
        ctrl.on_outcome_at(true, true, 850, 1000, t0 + Duration::from_secs(3));
        assert_eq!(ctrl.limit(), 9);

        // Above high memory watermark -> multiplicative decrease (*0.75 = 6).
        ctrl.observe_rss_at(920, 1000, t0 + Duration::from_secs(6));
        assert_eq!(ctrl.limit(), 6);

        // Failure -> multiplicative decrease (*0.75 = 4), clamped >= min_concurrency (2).
        ctrl.on_outcome_at(false, true, 500, 1000, t0 + Duration::from_secs(9));
        assert_eq!(ctrl.limit(), 4);
        ctrl.on_outcome_at(false, true, 500, 1000, t0 + Duration::from_secs(12));
        assert_eq!(ctrl.limit(), 3);
        ctrl.on_outcome_at(false, true, 500, 1000, t0 + Duration::from_secs(15));
        ctrl.on_outcome_at(false, true, 500, 1000, t0 + Duration::from_secs(18));
        assert_eq!(ctrl.limit(), 2);
    }
}
