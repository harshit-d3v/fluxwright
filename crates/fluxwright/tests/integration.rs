use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use fluxwright::{BrowserEngine, ColorScheme, JobOptions, Proxy, QueueFullMode, StorageState, WaitUntil};
use serde_json::json;

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

/// Playwright's newContext options: the page and its HTTP requests both see them, and the next
/// job on the same browser gets Chrome's defaults back.
#[tokio::test(flavor = "multi_thread")]
async fn emulation_and_permissions() {
    let srv = benchmark_server::spawn("127.0.0.1:0").await.unwrap();
    let eng = engine(1, 2).await;
    let opts = JobOptions::default()
        .user_agent("FluxTest/1.0")
        .locale("de-DE")
        .timezone("Asia/Tokyo")
        .geolocation(35.68, 139.69)
        .permissions(["geolocation"])
        .viewport(500, 400)
        .device_scale_factor(2.0)
        .color_scheme(ColorScheme::Dark);
    let page = eng.acquire_job(&opts).await.unwrap();
    page.goto(&format!("{}/headers", srv.base_url)).await.unwrap();
    let sent = page.evaluate("document.body.innerText").await.unwrap();
    let sent = sent.as_str().unwrap();
    assert!(sent.starts_with("FluxTest/1.0\nde-DE"), "request headers: {sent:?}");
    let seen = page
        .evaluate(
            r#"(async () => {
                const pos = await new Promise((ok, err) => navigator.geolocation.getCurrentPosition(ok, err));
                return [navigator.userAgent, navigator.language, Intl.DateTimeFormat().resolvedOptions().timeZone,
                    pos.coords.latitude, pos.coords.longitude, innerWidth, innerHeight, devicePixelRatio,
                    matchMedia('(prefers-color-scheme: dark)').matches].join('|');
            })()"#,
        )
        .await
        .unwrap();
    assert_eq!(seen, json!("FluxTest/1.0|de-DE|Asia/Tokyo|35.68|139.69|500|400|2|true"));
    // A popup is a new page in the same context, with a session of its own.
    let popup = page
        .evaluate(
            r#"(async () => {
                const w = window.open(location.href);
                for (let i = 0; i < 250 && w.document.readyState !== 'complete'; i++) await new Promise(r => setTimeout(r, 20));
                return [w.navigator.userAgent, w.innerWidth, w.Intl.DateTimeFormat().resolvedOptions().timeZone].join('|');
            })()"#,
        )
        .await
        .unwrap();
    assert_eq!(popup, json!("FluxTest/1.0|500|Asia/Tokyo"));
    drop(page);

    let plain = eng.acquire().await.unwrap();
    plain.goto(&format!("{}/headers", srv.base_url)).await.unwrap();
    assert_ne!(plain.evaluate("navigator.userAgent").await.unwrap(), json!("FluxTest/1.0"));
    let unknown = eng.acquire_job(&JobOptions::default().permissions(["teleport"])).await;
    assert!(unknown.is_err(), "an unknown permission must fail the job, not be ignored");
}

/// An iframe from another site runs in its own process and DevTools session, which gets the
/// same emulation when it attaches.
#[tokio::test(flavor = "multi_thread")]
async fn emulation_reaches_cross_site_iframes() {
    use axum::{response::Html, routing::get, Router};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let top = format!(r#"<!doctype html><iframe src="http://localhost:{port}/who"></iframe>"#);
    let who = r#"<!doctype html><script>document.write('<p id=who>' + [navigator.userAgent, navigator.language,
        Intl.DateTimeFormat().resolvedOptions().timeZone, matchMedia('(prefers-color-scheme: dark)').matches]
        .join('|') + '</p>')</script>"#;
    let app = Router::new()
        .route("/", get(move || async move { Html(top) }))
        .route("/who", get(move || async move { Html(who) }));
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let eng = engine(1, 1).await;
    let opts = JobOptions::default()
        .user_agent("FluxTest/1.0")
        .locale("de-DE")
        .timezone("Asia/Tokyo")
        .color_scheme(ColorScheme::Dark);
    let page = eng.acquire_job(&opts).await.unwrap();
    page.goto(&format!("http://127.0.0.1:{port}/")).await.unwrap();
    let seen = page.frame_locator("iframe").locator("#who").text_content().await.unwrap();
    assert_eq!(seen.as_deref(), Some("FluxTest/1.0|de-DE|Asia/Tokyo|true"));
}

/// Log in once, start the next job from the saved state, and the job after that from nothing.
#[tokio::test(flavor = "multi_thread")]
async fn storage_state_round_trip() {
    let srv = benchmark_server::spawn("127.0.0.1:0").await.unwrap();
    let eng = engine(1, 2).await;
    let first = eng.acquire().await.unwrap();
    first.goto(&format!("{}/set-cookie", srv.base_url)).await.unwrap();
    first.evaluate("localStorage.setItem('token', 'abc')").await.unwrap();
    let state = first.storage_state().await.unwrap();
    drop(first);
    assert!(state.cookies.iter().any(|c| c.name == "fw" && c.value == "secret"), "{state:?}");
    assert_eq!(state.origins.len(), 1);
    assert_eq!(state.origins[0].origin, srv.base_url);

    // Through JSON, the way a saved file goes.
    let state: StorageState = serde_json::from_str(&serde_json::to_string(&state).unwrap()).unwrap();
    let restored = eng.acquire_job(&JobOptions::default().storage_state(state)).await.unwrap();
    restored.goto(&format!("{}/show-cookie", srv.base_url)).await.unwrap();
    let both = "[document.cookie, localStorage.getItem('token')].join('|')";
    assert_eq!(restored.evaluate(both).await.unwrap(), json!("fw=secret|abc"));
    // The page's own change survives a reload: the saved value is restored only once.
    restored.evaluate("localStorage.removeItem('token')").await.unwrap();
    restored.goto(&format!("{}/show-cookie", srv.base_url)).await.unwrap();
    assert_eq!(restored.evaluate("localStorage.getItem('token')").await.unwrap(), json!(null));
    drop(restored);

    let fresh = eng.acquire().await.unwrap();
    fresh.goto(&format!("{}/show-cookie", srv.base_url)).await.unwrap();
    assert_eq!(fresh.evaluate(both).await.unwrap(), json!("|"));
}

/// Node's garbage collector drops a page that was never closed on its own thread, outside any
/// runtime. That must not panic, and the slot must still come back.
#[test]
fn lease_dropped_outside_runtime_frees_its_slot() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    let eng = rt
        .block_on(
            BrowserEngine::builder()
                .max_browsers(1)
                .max_contexts_per_browser(1)
                .acquire_timeout(Duration::from_secs(20))
                .build(),
        )
        .expect("engine");
    let lease = rt.block_on(eng.acquire()).expect("first lease");
    drop(lease);
    let again = rt.block_on(eng.acquire());
    assert!(again.is_ok(), "the dropped lease kept its slot: {:?}", again.err());
    rt.block_on(async move {
        drop(again);
        eng.shutdown(Duration::from_secs(5)).await.unwrap();
    });
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
        .acquire_timeout(Duration::from_secs(60))
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

#[tokio::test(flavor = "multi_thread")]
async fn page_actions() {
    let eng = BrowserEngine::builder()
        .max_browsers(1)
        .action_timeout(Duration::from_secs(2))
        .build()
        .await
        .unwrap();
    let page = eng.acquire().await.unwrap();
    // No '#' or '%' in the markup: it goes into a data: URL unencoded.
    page.goto(concat!(
        "data:text/html,<title>start</title><body style='margin:0'>",
        "<div style='height:3000px'></div>",
        "<button id='far' onclick=\"document.title='far'\">far</button>",
        "<button id='dis' disabled>disabled</button>",
        "<div style='height:500px'></div>"
    ))
    .await
    .unwrap();

    page.click("#far").await.unwrap();
    assert_eq!(page.title().await.unwrap(), "far", "below-the-fold click missed");

    page.wait_for_selector("#dis")
        .await
        .expect("visible but disabled satisfies wait_for_selector");

    let err = page.wait_for_selector("#nope").await.unwrap_err().to_string();
    assert!(err.contains("no element matches selector"), "{err}");
    let err = page.click("button[").await.unwrap_err().to_string();
    assert!(err.contains("invalid selector"), "{err}");

    let err = page.evaluate("null.x").await.unwrap_err().to_string();
    assert!(err.contains("TypeError") && !err.contains("exceptionId"), "{err}");

    page.set_viewport_size(400, 300).await.unwrap();
    assert_eq!(page.evaluate("innerWidth").await.unwrap(), 400);
    let png = page.screenshot_full_page().await.unwrap();
    let height = u32::from_be_bytes(png[20..24].try_into().unwrap()); // IHDR height
    assert!(height > 3000, "full-page screenshot is {height}px tall");

    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port(); // listener dropped: port now refuses
    let err = page
        .goto(&format!("http://127.0.0.1:{port}/"))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("net::ERR_"), "{err}");

    drop(page);
    eng.shutdown(Duration::from_secs(5)).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn goto_wait_until() {
    let srv = benchmark_server::spawn("127.0.0.1:0").await.unwrap();
    let eng = BrowserEngine::builder()
        .max_browsers(1)
        .navigation_timeout(Duration::from_secs(3))
        .build()
        .await
        .unwrap();
    let page = eng.acquire().await.unwrap();
    // The image takes 30 s: DOMContentLoaded fires at once, load does not.
    let url = format!("{}/slow-subresource", srv.base_url);

    let t = std::time::Instant::now();
    page.goto_with(&url, WaitUntil::DomContentLoaded).await.unwrap();
    assert!(t.elapsed() < Duration::from_secs(2), "domcontentloaded took {:?}", t.elapsed());
    assert_eq!(page.title().await.unwrap(), "dcl");

    let err = page.goto(&url).await.unwrap_err().to_string();
    assert!(err.contains("timed out"), "load must wait for the image: {err}");

    page.goto_with(&url, WaitUntil::Commit).await.unwrap();

    let t = std::time::Instant::now();
    page.goto_with(&format!("{}/", srv.base_url), WaitUntil::NetworkIdle)
        .await
        .unwrap();
    assert!(t.elapsed() >= Duration::from_millis(500), "networkidle returned after {:?}", t.elapsed());
    assert_eq!(page.title().await.unwrap(), "home");

    drop(page);
    eng.shutdown(Duration::from_secs(5)).await.unwrap();
}

/// getByRole / getByText, and elements inside a same-origin and a cross-origin iframe.
#[tokio::test(flavor = "multi_thread")]
async fn selectors_and_frames() {
    use axum::{response::Html, routing::get, Router};

    // The cross-origin frame is on `localhost`, the page on 127.0.0.1: a different site, so
    // Chrome runs it in its own process (an OOPIF) with its own CDP session.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let top = format!(
        r#"<!doctype html><title>top</title>
        <h1>Welcome back</h1>
        <p>Hello <b>World</b></p>
        <button onclick="document.title='draft'">Save draft</button>
        <button onclick="document.title='saved'">Save</button>
        <button style="display:none">Ghost</button>
        <label for="email">Email</label><input id="email">
        <div style="height:1500px"></div>
        <iframe id="same" src="/inner"></iframe>
        <iframe id="cross" src="http://localhost:{port}/inner"></iframe>"#
    );
    const INNER: &str = r#"<!doctype html><title>inner</title>
        <input aria-label="Code"><p id="out">idle</p>
        <button onclick="document.getElementById('out').textContent =
            'clicked ' + document.querySelector('input').value">Inner</button>"#;
    let app = Router::new()
        .route("/", get(move || async move { Html(top) }))
        .route("/inner", get(|| async { Html(INNER) }));
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let eng = BrowserEngine::builder()
        .max_browsers(1)
        .action_timeout(Duration::from_secs(2))
        .build()
        .await
        .unwrap();
    let page = eng.acquire().await.unwrap();
    page.goto(&format!("http://127.0.0.1:{port}/")).await.unwrap();

    // Name is a case-insensitive substring by default, so "Save" finds "Save draft" first.
    page.get_by_role("button", Some("save"), false).click().await.unwrap();
    assert_eq!(page.title().await.unwrap(), "draft");
    page.get_by_role("button", Some("Save"), true).click().await.unwrap();
    assert_eq!(page.title().await.unwrap(), "saved");

    let heading = page.get_by_role("heading", Some("welcome"), false);
    assert_eq!(heading.text_content().await.unwrap().as_deref(), Some("Welcome back"));
    // The deepest element wins: <b>, not <p>.
    assert_eq!(page.get_by_text("world", false).text_content().await.unwrap().as_deref(), Some("World"));
    assert!(page.get_by_text("hello world", true).wait().await.is_err(), "exact is case-sensitive");

    page.get_by_role("textbox", Some("Email"), false).fill("a@b.co").await.unwrap();
    assert_eq!(page.evaluate("document.querySelector('#email').value").await.unwrap(), "a@b.co");

    let err = page.get_by_role("button", Some("Ghost"), false).click().await.unwrap_err().to_string();
    assert!(err.contains("no element matches"), "hidden elements never match a role: {err}");

    for frame in ["#same", "#cross"] {
        let f = page.frame_locator(frame);
        f.get_by_role("textbox", Some("code"), false).fill("42").await.unwrap();
        f.get_by_role("button", Some("Inner"), true).click().await.unwrap();
        let out = f.locator("#out").text_content().await.unwrap();
        assert_eq!(out.as_deref(), Some("clicked 42"), "in {frame}");
    }

    drop(page);
    eng.shutdown(Duration::from_secs(5)).await.unwrap();
}

/// block_images must hold in cross-origin iframes (their own CDP sessions) and on every job
/// on a browser, and leave no per-job task behind.
#[tokio::test(flavor = "multi_thread")]
async fn blocking_covers_cross_origin_frames() {
    use axum::{http::header, response::Html, routing::get, Router};
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};

    let hits = Arc::new(AtomicUsize::new(0));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    // Each document marks itself done once its image loads or fails.
    const IMG: &str = r#"<img src="/img.png" onload="document.body.dataset.done=1" onerror="document.body.dataset.done=1">"#;
    let top = format!(r#"<!doctype html><body>{IMG}<iframe src="http://localhost:{port}/frame"></iframe>"#);
    let counter = hits.clone();
    let app = Router::new()
        .route("/", get(move || async move { Html(top) }))
        .route("/frame", get(|| async { Html(format!("<!doctype html><body>{IMG}")) }))
        .route(
            "/img.png",
            get(move || {
                counter.fetch_add(1, SeqCst);
                async { ([(header::CONTENT_TYPE, "image/png")], "not really a png") }
            }),
        );
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let eng = BrowserEngine::builder().max_browsers(1).build().await.unwrap();
    let url = format!("http://127.0.0.1:{port}/");
    let load = |opts: JobOptions| {
        let url = url.clone();
        eng.run(opts, move |page| {
            let url = url.clone();
            async move {
                page.goto(&url).await?;
                page.wait_for_selector("body[data-done]").await?;
                page.frame_locator("iframe").locator("body[data-done]").wait().await?;
                Ok(())
            }
        })
    };

    for _ in 0..3 {
        let opts = JobOptions { block_images: true, ..JobOptions::default() };
        load(opts).await.unwrap();
    }
    assert_eq!(hits.load(SeqCst), 0, "images fetched despite block_images");

    load(JobOptions::default()).await.unwrap();
    assert_eq!(hits.load(SeqCst), 2, "control run: page and cross-origin frame each fetch the image");
    eng.shutdown(Duration::from_secs(5)).await.unwrap();
}

/// An unhandled dialog used to block every call on the page until it timed out.
#[tokio::test(flavor = "multi_thread")]
async fn dialogs_do_not_block() {
    let eng = BrowserEngine::builder()
        .max_browsers(1)
        .action_timeout(Duration::from_secs(3))
        .navigation_timeout(Duration::from_secs(5))
        .build()
        .await
        .unwrap();
    let page = eng.acquire().await.unwrap();
    page.goto("data:text/html,<title>d</title><body style='height:400px'>x</body>").await.unwrap();

    assert_eq!(page.evaluate("alert('hi'); 42").await.unwrap(), 42);
    assert_eq!(page.evaluate("confirm('sure?')").await.unwrap(), false, "confirm is dismissed");
    assert_eq!(page.evaluate("prompt('name?')").await.unwrap(), serde_json::Value::Null);

    // beforeunload is accepted, so a "leave this page?" handler cannot pin the page. Chrome
    // only asks after a user gesture, hence the click.
    page.evaluate("window.onbeforeunload = e => { e.preventDefault(); e.returnValue = ''; }; 1")
        .await
        .unwrap();
    page.click("body").await.unwrap();
    page.goto("about:blank").await.unwrap();

    drop(page);
    eng.shutdown(Duration::from_secs(5)).await.unwrap();
}

/// Selector code runs in an isolated world, so a page that patches DOM APIs (anti-bot
/// scripts do) cannot break clicks, fills or waits.
#[tokio::test(flavor = "multi_thread")]
async fn page_scripts_cannot_break_selectors() {
    let eng = BrowserEngine::builder()
        .max_browsers(1)
        .action_timeout(Duration::from_secs(3))
        .build()
        .await
        .unwrap();
    let page = eng.acquire().await.unwrap();
    page.goto(concat!(
        "data:text/html,<title>t</title>",
        "<button onclick=\"document.title='clicked'\">Go</button><input aria-label='Name'>",
        "<script>document.querySelector = () => null; document.querySelectorAll = () => [];",
        "Element.prototype.getBoundingClientRect = () => new DOMRect(0, 0, 0, 0);</script>"
    ))
    .await
    .unwrap();

    page.get_by_role("button", Some("Go"), true).click().await.unwrap();
    assert_eq!(page.title().await.unwrap(), "clicked");
    page.fill("input", "ok").await.unwrap();
    assert_eq!(page.get_by_role("textbox", Some("Name"), true).text_content().await.unwrap().as_deref(), Some(""));
    // evaluate still runs in the page's own world, where the patch is visible.
    assert_eq!(page.evaluate("document.querySelector('input')").await.unwrap(), serde_json::Value::Null);

    drop(page);
    eng.shutdown(Duration::from_secs(5)).await.unwrap();
}

/// A proxy per job, with credentials, next to direct jobs on the same browser.
#[tokio::test(flavor = "multi_thread")]
async fn proxy_per_job() {
    use axum::http::{header, HeaderMap, StatusCode};
    use axum::response::{Html, IntoResponse};
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};

    // A forward proxy that wants user:pass and answers every request itself, so a host
    // that does not exist (fluxwright.invalid) loads only through it.
    // A fresh password each run, so no credential is written in the code.
    use base64::Engine;
    use std::hash::{BuildHasher, Hasher};
    let pass = format!("{:016x}", std::collections::hash_map::RandomState::new().build_hasher().finish());
    let expected = format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("user:{pass}"))
    );
    let challenges = Arc::new(AtomicUsize::new(0));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = format!("http://{}", listener.local_addr().unwrap());
    let seen = challenges.clone();
    let app = axum::Router::new().fallback(move |headers: HeaderMap| {
        let seen = seen.clone();
        let expected = expected.clone();
        async move {
            if headers
                .get(header::PROXY_AUTHORIZATION)
                .is_some_and(|v| v.as_bytes() == expected.as_bytes())
            {
                Html("<title>via proxy</title>").into_response()
            } else {
                seen.fetch_add(1, SeqCst);
                (StatusCode::PROXY_AUTHENTICATION_REQUIRED, [(header::PROXY_AUTHENTICATE, "Basic realm=\"fw\"")])
                    .into_response()
            }
        }
    });
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let eng = BrowserEngine::builder()
        .max_browsers(1)
        .navigation_timeout(Duration::from_secs(10))
        .build()
        .await
        .unwrap();
    let title = |opts: JobOptions| {
        eng.run(opts.retries(0), |page| async move {
            page.goto("http://fluxwright.invalid/").await?;
            page.title().await
        })
    };

    let good = JobOptions::default().proxy(Proxy::new(&proxy).auth("user", &pass));
    assert_eq!(title(good).await.unwrap(), "via proxy");

    let direct = title(JobOptions::default()).await;
    assert!(!matches!(direct.as_deref(), Ok("via proxy")), "a job without a proxy used it: {direct:?}");

    // Wrong credentials: answered once, then cancelled, instead of a 407 loop. (Chrome's own
    // requests, such as the favicon, meet the proxy without Fetch and give up by themselves.)
    challenges.store(0, SeqCst);
    let start = std::time::Instant::now();
    let bad = JobOptions::default().proxy(Proxy::new(&proxy).auth("user", format!("{pass}-wrong")));
    let bad = title(bad).await;
    assert!(!matches!(bad.as_deref(), Ok("via proxy")), "{bad:?}");
    assert!(start.elapsed() < Duration::from_secs(5), "wrong credentials took {:?}", start.elapsed());
    assert!(challenges.load(SeqCst) < 10, "{} proxy challenges: auth is looping", challenges.load(SeqCst));

    eng.shutdown(Duration::from_secs(5)).await.unwrap();
}

/// Review findings on PR #2: each used to misclick or fill silently.
#[tokio::test(flavor = "multi_thread")]
async fn actionability_edge_cases() {
    let eng = BrowserEngine::builder()
        .max_browsers(1)
        .action_timeout(Duration::from_secs(2))
        .build()
        .await
        .unwrap();
    let page = eng.acquire().await.unwrap();
    page.goto(concat!(
        "data:text/html,<title>t</title>",
        "<input type=button value=Save onclick=\"document.title='input'\">",
        "<span onclick=\"document.title='span'\">Save</span>",
        "<button onclick=\"document.title='img'\" style='padding:8px'><img alt=Upload></button>",
        "<fieldset disabled><button>Locked</button></fieldset>",
        "<div aria-disabled=true><button>Aria</button></div>",
        "<div id=ce contenteditable>old</div><h1>Head</h1>",
        "<input id=gone onfocus=\"this.replaceWith(document.createElement('input'))\">"
    ))
    .await
    .unwrap();

    // text= finds matches in DOM order, input buttons included.
    page.get_by_text("save", false).click().await.unwrap();
    assert_eq!(page.title().await.unwrap(), "input");
    // A button named by its image's alt text.
    page.get_by_role("button", Some("Upload"), true).click().await.unwrap();
    assert_eq!(page.title().await.unwrap(), "img");
    // Disabled through <fieldset disabled> or an ancestor's aria-disabled.
    for name in ["Locked", "Aria"] {
        let err = page.get_by_role("button", Some(name), true).click().await.unwrap_err().to_string();
        assert!(err.contains("disabled"), "{name}: {err}");
    }
    // fill replaces contenteditable content, refuses non-editable elements, and fails when
    // the page swaps the element out on focus instead of typing into nothing.
    page.fill("#ce", "new").await.unwrap();
    assert_eq!(page.evaluate("document.querySelector('#ce').textContent").await.unwrap(), "new");
    let err = page.fill("h1", "x").await.unwrap_err().to_string();
    assert!(err.contains("not an <input>"), "{err}");
    let err = page.fill("#gone", "x").await.unwrap_err().to_string();
    assert!(err.contains("removed after the click"), "{err}");

    // An overlay in the parent page covers the iframe: the click must not report success.
    page.goto(concat!(
        "data:text/html,<iframe id=f srcdoc='<button>In</button>'></iframe>",
        "<div style='position:fixed;inset:0;background:rgba(0,0,0,.1)'></div>"
    ))
    .await
    .unwrap();
    let err = page.frame_locator("#f").get_by_role("button", Some("In"), true).click().await.unwrap_err().to_string();
    assert!(err.contains("obscured by <div>"), "{err}");

    drop(page);
    eng.shutdown(Duration::from_secs(5)).await.unwrap();
}

/// Chrome sends each CDP message as one WebSocket frame; tungstenite's default 16 MiB frame
/// cap made a big screenshot kill the connection, and with it every job on that browser.
#[tokio::test(flavor = "multi_thread")]
async fn screenshot_larger_than_16_mib() {
    let eng = BrowserEngine::builder().max_browsers(1).build().await.unwrap();
    let page = eng.acquire().await.unwrap();
    page.goto("about:blank").await.unwrap();
    // Random pixels do not compress: 1280x8000 comes out around 30 MB of PNG, 40 MB as base64.
    page.evaluate(
        r#"(() => {
            const c = document.createElement('canvas');
            c.width = 1280; c.height = 8000; c.style.display = 'block';
            const ctx = c.getContext('2d'), img = ctx.createImageData(1280, 8000);
            for (let i = 0; i < img.data.length; i += 65536)
                crypto.getRandomValues(img.data.subarray(i, i + 65536));
            ctx.putImageData(img, 0, 0);
            document.body.style.margin = '0';
            document.body.appendChild(c);
        })()"#,
    )
    .await
    .unwrap();
    let png = page.screenshot_full_page().await.unwrap();
    assert!(png.len() > 16 << 20, "only {} bytes; the test page did not get big enough", png.len());
    assert_eq!(page.title().await.unwrap(), "", "connection still usable afterwards");
    drop(page);
    eng.shutdown(Duration::from_secs(5)).await.unwrap();
}

/// A crashed or hard-killed engine must take Chrome with it (job object on Windows;
/// PR_SET_PDEATHSIG and the DevTools pipe on Linux; the pipe on macOS), and its temp
/// profile must be swept afterwards.
#[tokio::test(flavor = "multi_thread")]
async fn chrome_dies_with_engine() {
    use std::io::{BufRead, BufReader};

    if std::env::var_os("FLUXWRIGHT_TEST_CHILD").is_some() {
        // Child: start Chrome, report its pid, wait to be killed.
        let eng = BrowserEngine::builder().max_browsers(1).build().await.unwrap();
        let page = eng.acquire().await.unwrap();
        println!("BROWSER_PID {}", page.browser_pid());
        tokio::time::sleep(Duration::from_secs(120)).await;
        return;
    }
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["chrome_dies_with_engine", "--exact", "--nocapture", "--test-threads=1"])
        .env("FLUXWRIGHT_TEST_CHILD", "1")
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let engine_pid = child.id();
    let browser_pid = BufReader::new(child.stdout.take().unwrap())
        .lines()
        .map_while(|l| l.ok())
        // libtest may print "test chrome_dies_with_engine ... " on the same line first.
        .find_map(|l| l.split("BROWSER_PID ").nth(1).and_then(|p| p.trim().parse::<u32>().ok()))
        .expect("child never reported a browser pid");
    child.kill().unwrap(); // TerminateProcess / SIGKILL: no destructor or kill_on_drop runs
    child.wait().unwrap();

    let alive = |pid: u32| {
        let mut sys = sysinfo::System::new();
        let pid = sysinfo::Pid::from_u32(pid);
        sys.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
        sys.process(pid).is_some()
    };
    let start = std::time::Instant::now();
    while alive(browser_pid) {
        assert!(start.elapsed() < Duration::from_secs(10), "chrome {browser_pid} outlived its engine");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let prefix = format!("fluxwright-{engine_pid}-");
    let leftovers = || {
        std::fs::read_dir(std::env::temp_dir())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with(&prefix))
            .count()
    };
    let start = std::time::Instant::now();
    loop {
        fluxwright::sweep_stale_profiles(); // retried: Windows holds file locks briefly after exit
        if leftovers() == 0 {
            break;
        }
        assert!(start.elapsed() < Duration::from_secs(10), "profile of dead engine not swept");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn two_hundred_leases_five_browsers_no_deadlock() {
    let srv = benchmark_server::spawn("127.0.0.1:0").await.unwrap();
    let url = format!("{}/", srv.base_url);
    // 50 slots for 200 leases: most leases queue, which is what exercises the wakeup path.
    // 200 tabs loading at once pushed single navigations near the 30 s timeout on Windows.
    let eng = BrowserEngine::builder()
        .max_browsers(5)
        .max_contexts_per_browser(10)
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
