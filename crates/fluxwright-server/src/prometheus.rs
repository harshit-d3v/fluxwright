//! Prometheus text exposition format generator for Fluxwright fleet and server metrics.

use fluxwright::MetricsSnapshot;

/// Combined metrics snapshot of the underlying fleet and the HTTP server registry.
#[derive(Debug, Clone, Default)]
pub struct ServerMetrics {
    pub fleet: MetricsSnapshot,
    pub queued_jobs: usize,
    pub running_jobs: usize,
    pub succeeded_jobs: u64,
    pub failed_jobs: u64,
    pub cancelled_jobs: u64,
    pub max_jobs: usize,
    pub online_workers: usize,
    pub draining_workers: usize,
    pub stale_workers: usize,
    pub expired_leases: u64,
    pub active_cdp_sessions: usize,
    pub total_cdp_sessions: u64,
    pub fastpath_hits: u64,
    pub fastpath_fallbacks: u64,
}

impl ServerMetrics {
    /// Formats all metrics using the official Prometheus exposition text format (v0.0.4).
    pub fn to_prometheus_text(&self) -> String {
        let mut out = String::with_capacity(2048);

        // Fleet metrics
        write_metric(
            &mut out,
            "fluxwright_browsers",
            "Number of active browsers in the fleet.",
            "gauge",
            self.fleet.browsers,
        );
        write_metric(
            &mut out,
            "fluxwright_contexts",
            "Number of active browser contexts in the fleet.",
            "gauge",
            self.fleet.contexts,
        );
        write_metric(
            &mut out,
            "fluxwright_pages",
            "Number of open browser pages in the fleet.",
            "gauge",
            self.fleet.pages,
        );
        write_metric(
            &mut out,
            "fluxwright_active_leases",
            "Number of browser leases currently running jobs.",
            "gauge",
            self.fleet.active_leases,
        );
        write_metric(
            &mut out,
            "fluxwright_queued_requests",
            "Number of requests waiting in the engine pool for an available browser slot.",
            "gauge",
            self.fleet.queued_requests,
        );
        write_metric(
            &mut out,
            "fluxwright_warm_contexts",
            "Number of pre-warmed blank contexts ready in the fleet.",
            "gauge",
            self.fleet.warm_contexts,
        );
        write_metric(
            &mut out,
            "fluxwright_warm_context_hits_total",
            "Total number of leases fulfilled from a pre-warmed context.",
            "counter",
            self.fleet.warm_context_hits,
        );
        write_float_metric(
            &mut out,
            "fluxwright_browser_reuse_rate",
            "Ratio of browser acquisitions that reused an existing browser (0.0 to 1.0).",
            "gauge",
            self.fleet.browser_reuse_rate,
        );
        write_metric(
            &mut out,
            "fluxwright_crash_count_total",
            "Total count of browser crashes detected and recovered.",
            "counter",
            self.fleet.crash_count,
        );
        write_metric(
            &mut out,
            "fluxwright_recycle_count_total",
            "Total count of browser recycles performed by the engine.",
            "counter",
            self.fleet.recycle_count,
        );
        write_metric(
            &mut out,
            "fluxwright_engine_rss_bytes",
            "Resident set size of the engine process in bytes.",
            "gauge",
            self.fleet.engine_rss_bytes,
        );
        write_metric(
            &mut out,
            "fluxwright_browser_tree_rss_bytes",
            "Total resident memory of all managed browser process trees in bytes.",
            "gauge",
            self.fleet.browser_tree_rss_bytes,
        );
        write_metric(
            &mut out,
            "fluxwright_memory_per_job_bytes_estimate",
            "Estimated browser process-tree RSS divided by active leases in bytes.",
            "gauge",
            self.fleet.memory_per_job_bytes_estimate,
        );
        write_metric(
            &mut out,
            "fluxwright_effective_concurrency_limit",
            "Active fleet concurrency admission limit (reflects adaptive concurrency when enabled).",
            "gauge",
            self.fleet.effective_concurrency_limit,
        );

        // HTTP Server metrics
        out.push_str("# HELP fluxwright_http_jobs_total Total number of HTTP jobs completed by final status.\n");
        out.push_str("# TYPE fluxwright_http_jobs_total counter\n");
        out.push_str(&format!(
            "fluxwright_http_jobs_total{{status=\"succeeded\"}} {}\n",
            self.succeeded_jobs
        ));
        out.push_str(&format!(
            "fluxwright_http_jobs_total{{status=\"failed\"}} {}\n",
            self.failed_jobs
        ));
        out.push_str(&format!(
            "fluxwright_http_jobs_total{{status=\"cancelled\"}} {}\n",
            self.cancelled_jobs
        ));

        out.push_str("# HELP fluxwright_http_jobs_active Current number of active HTTP jobs.\n");
        out.push_str("# TYPE fluxwright_http_jobs_active gauge\n");
        out.push_str(&format!(
            "fluxwright_http_jobs_active{{status=\"queued\"}} {}\n",
            self.queued_jobs
        ));
        out.push_str(&format!(
            "fluxwright_http_jobs_active{{status=\"running\"}} {}\n",
            self.running_jobs
        ));

        write_metric(
            &mut out,
            "fluxwright_http_jobs_capacity",
            "Configured maximum job capacity (active and retained completed jobs combined).",
            "gauge",
            self.max_jobs,
        );

        out.push_str("# HELP fluxwright_cluster_workers Registered coordinator workers by status.\n");
        out.push_str("# TYPE fluxwright_cluster_workers gauge\n");
        out.push_str(&format!(
            "fluxwright_cluster_workers{{status=\"online\"}} {}\n",
            self.online_workers
        ));
        out.push_str(&format!(
            "fluxwright_cluster_workers{{status=\"draining\"}} {}\n",
            self.draining_workers
        ));
        out.push_str(&format!(
            "fluxwright_cluster_workers{{status=\"stale\"}} {}\n",
            self.stale_workers
        ));

        write_metric(
            &mut out,
            "fluxwright_cluster_lease_expirations_total",
            "Total number of worker leases expired by the coordinator.",
            "counter",
            self.expired_leases,
        );

        write_metric(
            &mut out,
            "fluxwright_cdp_sessions_active",
            "Number of currently connected pooled CDP WebSocket gateway sessions.",
            "gauge",
            self.active_cdp_sessions,
        );

        write_metric(
            &mut out,
            "fluxwright_cdp_sessions_total",
            "Total number of pooled CDP WebSocket gateway sessions accepted.",
            "counter",
            self.total_cdp_sessions,
        );

        write_metric(
            &mut out,
            "fluxwright_fastpath_hits_total",
            "Total number of scrape/map requests served by the HTTP fast path.",
            "counter",
            self.fastpath_hits,
        );

        write_metric(
            &mut out,
            "fluxwright_fastpath_fallbacks_total",
            "Total number of scrape/map requests that escalated from HTTP fast path to Chromium.",
            "counter",
            self.fastpath_fallbacks,
        );

        out
    }
}

fn write_metric<T: std::fmt::Display>(
    out: &mut String,
    name: &str,
    help: &str,
    metric_type: &str,
    value: T,
) {
    out.push_str(&format!("# HELP {name} {help}\n"));
    out.push_str(&format!("# TYPE {name} {metric_type}\n"));
    out.push_str(&format!("{name} {value}\n"));
}

fn write_float_metric(out: &mut String, name: &str, help: &str, metric_type: &str, value: f64) {
    out.push_str(&format!("# HELP {name} {help}\n"));
    out.push_str(&format!("# TYPE {name} {metric_type}\n"));
    // Format float with up to 4 decimal places, avoiding scientific notation for reuse rate
    out.push_str(&format!("{name} {:.4}\n", value));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_prometheus_text_validly() {
        let metrics = ServerMetrics {
            fleet: MetricsSnapshot {
                browsers: 2,
                contexts: 4,
                pages: 3,
                active_leases: 1,
                queued_requests: 0,
                warm_contexts: 0,
                warm_context_hits: 0,
                browser_reuse_rate: 0.75,
                crash_count: 0,
                recycle_count: 1,
                engine_rss_bytes: 52_428_800,
                browser_tree_rss_bytes: 314_572_800,
                memory_per_job_bytes_estimate: 157_286_400,
                effective_concurrency_limit: 16,
            },
            queued_jobs: 1,
            running_jobs: 1,
            succeeded_jobs: 10,
            failed_jobs: 2,
            cancelled_jobs: 1,
            max_jobs: 128,
            online_workers: 2,
            draining_workers: 1,
            stale_workers: 0,
            expired_leases: 3,
            active_cdp_sessions: 2,
            total_cdp_sessions: 7,
            fastpath_hits: 15,
            fastpath_fallbacks: 3,
        };

        let text = metrics.to_prometheus_text();
        assert!(text.contains("# HELP fluxwright_browsers Number of active browsers in the fleet."));
        assert!(text.contains("# TYPE fluxwright_browsers gauge\nfluxwright_browsers 2\n"));
        assert!(text.contains("fluxwright_browser_reuse_rate 0.7500\n"));
        assert!(text.contains("fluxwright_http_jobs_total{status=\"succeeded\"} 10\n"));
        assert!(text.contains("fluxwright_http_jobs_total{status=\"failed\"} 2\n"));
        assert!(text.contains("fluxwright_http_jobs_total{status=\"cancelled\"} 1\n"));
        assert!(text.contains("fluxwright_http_jobs_active{status=\"queued\"} 1\n"));
        assert!(text.contains("fluxwright_http_jobs_active{status=\"running\"} 1\n"));
        assert!(text.contains("fluxwright_http_jobs_capacity 128\n"));
        assert!(text.contains("fluxwright_cluster_workers{status=\"online\"} 2\n"));
        assert!(text.contains("fluxwright_cluster_workers{status=\"draining\"} 1\n"));
        assert!(text.contains("fluxwright_cluster_workers{status=\"stale\"} 0\n"));
        assert!(text.contains("fluxwright_cluster_lease_expirations_total 3\n"));
        assert!(text.contains("fluxwright_cdp_sessions_active 2\n"));
        assert!(text.contains("fluxwright_cdp_sessions_total 7\n"));
        assert!(text.contains("fluxwright_fastpath_hits_total 15\n"));
        assert!(text.contains("fluxwright_fastpath_fallbacks_total 3\n"));
        assert!(text.contains("fluxwright_effective_concurrency_limit 16\n"));
    }
}
