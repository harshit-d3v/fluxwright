use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use fluxwright::{BrowserEngine, JobOptions, QueueFullMode};

fn kill_pid(pid: u32) {
    if cfg!(windows) {
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .status();
    } else {
        let _ = Command::new("kill")
            .args(["-9", &pid.to_string()])
            .status();
    }
}

async fn engine(max_browsers: usize, max_ctx: usize) -> BrowserEngine {
    BrowserEngine::builder()
        .max_browsers(max_browsers)
        .max_contexts_per_browser(max_ctx)
        .memory_ceiling_mb(16_384)
        .queue_capacity(64)
        .acquire_timeout(Duration::from_secs(60))
        .build()
        .await
        .expect("engine")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cookie_set_in_one_job_not_visible_in_next() {
    let srv = benchmark_server::spawn("127.0.0.1:0").await.unwrap();
    let eng = engine(1, 2).await;
    let set = format!("{}/set-cookie", srv.base_url);
    let show = format!("{}/show-cookie", srv.base_url);

    eng.run(JobOptions::default(), {
        let set = set.clone();
        move |page| {
            let set = set.clone();
            async move {
                page.goto(&set).await?;
                Ok(())
            }
        }
    })
    .await
    .unwrap();

    let cookie = eng
        .run(JobOptions::default(), {
            let show = show.clone();
            move |page| {
                let show = show.clone();
                async move {
                    page.goto(&show).await?;
                    page.evaluate("document.cookie").await
                }
            }
        })
        .await
        .unwrap();
    let cookie = cookie.as_str().unwrap_or("");
    assert!(
        !cookie.contains("secret"),
        "cookie leaked across jobs: {cookie}"
    );
    eng.shutdown(Duration::from_secs(5)).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn queue_full_error_mode() {
    let _srv = benchmark_server::spawn("127.0.0.1:0").await.unwrap();
    let eng = BrowserEngine::builder()
        .max_browsers(1)
        .max_contexts_per_browser(1)
        .queue_capacity(1)
        .queue_full_mode(QueueFullMode::Error)
        .acquire_timeout(Duration::from_secs(5))
        .build()
        .await
        .unwrap();

    let a = eng.acquire().await.expect("first lease");
    let eng2 = eng.clone();
    let queued = tokio::spawn(async move { eng2.acquire().await });
    let start = std::time::Instant::now();
    loop {
        if eng.metrics().await.queued_requests >= 1 {
            break;
        }
        if start.elapsed() > Duration::from_secs(10) {
            panic!("second acquire never entered the queue");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let third = eng.acquire().await;
    assert!(matches!(third, Err(fluxwright::Error::QueueFull)));
    drop(a);
    let _ = queued.await;
    eng.shutdown(Duration::from_secs(5)).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn queue_full_wait_mode() {
    let srv = benchmark_server::spawn("127.0.0.1:0").await.unwrap();
    let base = srv.base_url.clone();
    let eng = BrowserEngine::builder()
        .max_browsers(1)
        .max_contexts_per_browser(1)
        .queue_capacity(1)
        .queue_full_mode(QueueFullMode::Wait)
        .acquire_timeout(Duration::from_secs(30))
        .build()
        .await
        .unwrap();

    let held = eng.acquire().await.unwrap();
    let eng2 = eng.clone();
    let waiter = tokio::spawn(async move { eng2.acquire().await });
    tokio::time::sleep(Duration::from_millis(150)).await;
    drop(held);
    let page = waiter.await.unwrap().expect("waited for slot");
    page.goto(&format!("{base}/")).await.unwrap();
    drop(page);
    eng.shutdown(Duration::from_secs(5)).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn recycle_by_job_count() {
    let srv = benchmark_server::spawn("127.0.0.1:0").await.unwrap();
    let url = format!("{}/", srv.base_url);
    let eng = BrowserEngine::builder()
        .max_browsers(1)
        .max_contexts_per_browser(1)
        .recycle_after_jobs(1)
        .build()
        .await
        .unwrap();

    for _ in 0..2 {
        let url = url.clone();
        eng.run(JobOptions::default(), move |page| {
            let url = url.clone();
            async move {
                page.goto(&url).await?;
                Ok(())
            }
        })
        .await
        .unwrap();
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    let snap = eng.metrics().await;
    assert!(
        snap.recycle_count >= 1,
        "expected recycle, got {}",
        snap.recycle_count
    );
    eng.shutdown(Duration::from_secs(5)).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn recycle_by_rss_threshold() {
    let srv = benchmark_server::spawn("127.0.0.1:0").await.unwrap();
    let url = format!("{}/", srv.base_url);
    let eng = BrowserEngine::builder()
        .max_browsers(1)
        .max_contexts_per_browser(1)
        .recycle_rss_mb(1)
        .recycle_after_jobs(u64::MAX)
        .build()
        .await
        .unwrap();

    eng.run(JobOptions::default(), {
        let url = url.clone();
        move |page| {
            let url = url.clone();
            async move {
                page.goto(&url).await?;
                Ok(())
            }
        }
    })
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(800)).await;
    let snap = eng.metrics().await;
    assert!(
        snap.recycle_count >= 1,
        "expected rss recycle, got {}",
        snap.recycle_count
    );
    eng.shutdown(Duration::from_secs(5)).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn shutdown_with_leases_in_flight() {
    let srv = benchmark_server::spawn("127.0.0.1:0").await.unwrap();
    let url = format!("{}/", srv.base_url);
    let eng = engine(1, 2).await;
    let page = eng.acquire().await.unwrap();
    page.goto(&url).await.unwrap();
    let eng2 = eng.clone();
    let stop = tokio::spawn(async move {
        eng2.shutdown(Duration::from_secs(8)).await
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    drop(page);
    stop.await.unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn kill_browser_mid_job_retries() {
    let srv = benchmark_server::spawn("127.0.0.1:0").await.unwrap();
    let slow = format!("{}/slow", srv.base_url);
    let home = format!("{}/", srv.base_url);
    let eng = BrowserEngine::builder()
        .max_browsers(2)
        .max_contexts_per_browser(2)
        .acquire_timeout(Duration::from_secs(45))
        .build()
        .await
        .unwrap();

    let first = Arc::new(AtomicBool::new(true));
    let result = eng
        .run(JobOptions::default().retries(2), {
            let first = first.clone();
            let slow = slow.clone();
            let home = home.clone();
            move |page| {
                let first = first.clone();
                let slow = slow.clone();
                let home = home.clone();
                async move {
                    if first.swap(false, Ordering::SeqCst) {
                        let pid = page.browser_pid();
                        tokio::spawn(async move {
                            tokio::time::sleep(Duration::from_millis(400)).await;
                            kill_pid(pid);
                        });
                        page.goto(&slow).await?;
                    } else {
                        page.goto(&home).await?;
                    }
                    Ok(())
                }
            }
        })
        .await;
    assert!(result.is_ok(), "{result:?}");
    eng.shutdown(Duration::from_secs(5)).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn two_hundred_leases_five_browsers_no_deadlock() {
    let srv = benchmark_server::spawn("127.0.0.1:0").await.unwrap();
    let url = format!("{}/", srv.base_url);
    let eng = BrowserEngine::builder()
        .max_browsers(5)
        .max_contexts_per_browser(40)
        .queue_capacity(256)
        .recycle_after_jobs(u64::MAX)
        .memory_ceiling_mb(32_768)
        .acquire_timeout(Duration::from_secs(180))
        .build()
        .await
        .unwrap();

    let mut joins = Vec::new();
    for _ in 0..200 {
        let eng = eng.clone();
        let url = url.clone();
        joins.push(tokio::spawn(async move {
            let page = eng.acquire().await?;
            page.goto(&url).await?;
            page.title().await?;
            drop(page);
            Ok::<_, fluxwright::Error>(())
        }));
    }
    for j in joins {
        j.await.unwrap().expect("lease");
    }
    eng.shutdown(Duration::from_secs(15)).await.unwrap();
}
