//! 1,000 jobs through a small pool. Prints a metrics snapshot at the end.
//! Engine RSS should stay roughly flat (Chromium RSS will not).

use std::time::Duration;

use fluxwright::{BrowserEngine, JobOptions, QueueFullMode};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("fluxwright=info".parse()?),
        )
        .init();

    let server = benchmark_server::spawn("127.0.0.1:0").await?;
    let base = server.base_url.clone();

    let engine = BrowserEngine::builder()
        .max_browsers(3)
        .max_contexts_per_browser(8)
        .memory_ceiling_mb(8192)
        .recycle_after_jobs(200)
        .queue_capacity(1024)
        .queue_full_mode(QueueFullMode::Wait)
        .acquire_timeout(Duration::from_secs(180))
        .build()
        .await?;

    let mut joins = Vec::new();
    for i in 0..1000 {
        let engine = engine.clone();
        let url = format!("{base}/");
        joins.push(tokio::spawn(async move {
            engine
                .run(JobOptions::default().timeout(Duration::from_secs(180)), {
                    let url = url.clone();
                    move |page| {
                        let url = url.clone();
                        async move {
                            page.goto(&url).await?;
                            let _ = page.title().await?;
                            let _ = i;
                            Ok(())
                        }
                    }
                })
                .await
        }));
    }
    let mut ok = 0u32;
    let mut err = 0u32;
    for j in joins {
        match j.await {
            Ok(Ok(())) => ok += 1,
            _ => err += 1,
        }
    }
    let snap = engine.metrics().await;
    println!(
        "jobs_ok={ok} jobs_err={err} browsers={} leases={} reuse={:.3} crashes={} recycles={} engine_rss_mb={} browser_tree_rss_mb={} mem_per_job_est_mb={}",
        snap.browsers,
        snap.active_leases,
        snap.browser_reuse_rate,
        snap.crash_count,
        snap.recycle_count,
        snap.engine_rss_bytes / (1024 * 1024),
        snap.browser_tree_rss_bytes / (1024 * 1024),
        snap.memory_per_job_bytes_estimate / (1024 * 1024),
    );
    engine.shutdown(Duration::from_secs(10)).await?;
    Ok(())
}
