//! Chrome's memory with 1, 2, 4 and 8 pages open on one browser, then with a second browser:
//! the numbers behind auto-tuning. `cargo run --example calibrate_memory -- <url>`; without a
//! URL it serves the small local page the benchmarks use.

use std::time::Duration;

use fluxwright::BrowserEngine;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let server = benchmark_server::spawn("127.0.0.1:0").await?;
    let url = std::env::args().nth(1).unwrap_or_else(|| format!("{}/", server.base_url));
    let mb = |b: u64| b / (1024 * 1024);

    let engine = BrowserEngine::builder()
        .min_browsers(1)
        .max_browsers(2)
        .max_contexts_per_browser(8)
        .memory_ceiling_mb(1 << 20)
        .acquire_timeout(Duration::from_secs(60))
        .build()
        .await?;
    tokio::time::sleep(Duration::from_secs(3)).await;
    let m = engine.metrics().await;
    println!("browsers {} pages 0: {} MB", m.browsers, mb(m.browser_tree_rss_bytes));

    let mut leases = Vec::new();
    for target in [1usize, 2, 4, 8] {
        while leases.len() < target {
            let lease = engine.acquire().await?;
            lease.goto(&url).await?;
            leases.push(lease);
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
        let m = engine.metrics().await;
        println!("browsers {} pages {target}: {} MB", m.browsers, mb(m.browser_tree_rss_bytes));
    }
    // The first browser is full: one more page starts the second browser.
    let lease = engine.acquire().await?;
    lease.goto(&url).await?;
    leases.push(lease);
    tokio::time::sleep(Duration::from_secs(3)).await;
    for b in engine.browsers().await {
        println!("  browser {} leases {}: {} MB", b.pid, b.active_leases, mb(b.rss_bytes));
    }
    for lease in leases {
        lease.close().await?;
    }
    engine.shutdown(Duration::from_secs(10)).await?;
    Ok(())
}
