use std::sync::{Arc, Mutex};
use std::time::Duration;

use fluxwright_core::{Engine, JobOptions, ResourcePreset, ResourceType};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

async fn spawn_asset_server() -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let seen_c = seen.clone();

    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let seen = seen_c.clone();
            tokio::spawn(async move {
                let mut buf = Vec::with_capacity(2048);
                let mut tmp = [0u8; 1024];
                loop {
                    let n = match sock.read(&mut tmp).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => n,
                    };
                    buf.extend_from_slice(&tmp[..n]);
                    if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.len() > 16384 {
                        break;
                    }
                }
                let req = String::from_utf8_lossy(&buf);
                let path = req
                    .lines()
                    .next()
                    .and_then(|l| l.split_whitespace().nth(1))
                    .unwrap_or("/")
                    .to_string();
                seen.lock().unwrap().push(path.clone());
                let (ctype, body): (&str, Vec<u8>) = match path.as_str() {
                    "/" => (
                        "text/html",
                        b"<!doctype html><head><link rel=\"stylesheet\" href=\"/style.css\"></head>\
                          <body><img src=\"/img.png\"><script src=\"/app.js\"></script>\
                          <script src=\"/tracker.js\"></script>\
                          <style>@font-face{font-family:F;src:url(/font.woff2)}body{font-family:F}</style>\
                          <p>ok</p></body>"
                            .to_vec(),
                    ),
                    "/style.css" => ("text/css", b"p { color: rgb(1, 2, 3); }".to_vec()),
                    "/app.js" => ("application/javascript", b"window.__appLoaded = true;".to_vec()),
                    "/tracker.js" => (
                        "application/javascript",
                        b"window.__trackerLoaded = true;".to_vec(),
                    ),
                    "/img.png" => ("image/png", vec![0; 16]),
                    "/font.woff2" => ("font/woff2", vec![0; 16]),
                    _ => ("text/plain", b"ok".to_vec()),
                };
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(&body).await;
                let _ = sock.shutdown().await;
            });
        }
    });

    (format!("http://{addr}"), seen)
}

#[tokio::test(flavor = "multi_thread")]
async fn launch_custom_args_and_ignore_default_args() {
    let err = match Engine::builder().channel("bogus-browser").build().await {
        Ok(_) => panic!("expected bogus channel to fail eagerly"),
        Err(e) => e,
    };
    assert!(
        err.to_string().contains("unsupported browser channel"),
        "{err}"
    );

    let no_sandbox = std::env::var("FLUXWRIGHT_NO_SANDBOX").is_ok();
    let engine = Engine::builder()
        .no_sandbox(no_sandbox)
        .max_browsers(1)
        .args(["--user-agent=FluxwrightArgTest/1.0"])
        .ignore_default_args_list(["--mute-audio"])
        .build()
        .await
        .unwrap();

    let page = engine.acquire().await.unwrap();
    let ua = page.evaluate("navigator.userAgent").await.unwrap();
    assert_eq!(ua, json!("FluxwrightArgTest/1.0"));
    page.close().await.unwrap();
    engine.shutdown(Duration::from_secs(5)).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn prewarm_contexts_serves_warm_isolated_pages() {
    let (base_url, _) = spawn_asset_server().await;
    let no_sandbox = std::env::var("FLUXWRIGHT_NO_SANDBOX").is_ok();
    let engine = Engine::builder()
        .no_sandbox(no_sandbox)
        .min_browsers(1)
        .max_browsers(1)
        .max_contexts_per_browser(8)
        .prewarm_contexts(2)
        .build()
        .await
        .unwrap();

    let warmed = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if engine.metrics().await.warm_contexts >= 2 {
                break true;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap_or(false);
    assert!(warmed, "expected at least 2 pre-warmed contexts");

    let p1 = engine.acquire().await.unwrap();
    p1.goto(&base_url).await.unwrap();
    p1.evaluate("document.cookie = 'leak=1; Path=/'; localStorage.setItem('leak_key', 'yes');")
        .await
        .unwrap();
    assert!(engine.metrics().await.warm_context_hits >= 1);
    p1.close().await.unwrap();

    let p2 = engine.acquire().await.unwrap();
    p2.goto(&base_url).await.unwrap();
    let leaked = p2
        .evaluate("[document.cookie, localStorage.getItem('leak_key')]")
        .await
        .unwrap();
    assert_eq!(leaked, json!(["", null]));
    assert!(engine.metrics().await.warm_context_hits >= 2);
    p2.close().await.unwrap();

    engine.shutdown(Duration::from_secs(5)).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn resource_presets_and_block_resources() {
    let (base_url, seen) = spawn_asset_server().await;
    let no_sandbox = std::env::var("FLUXWRIGHT_NO_SANDBOX").is_ok();
    let engine = Engine::builder()
        .no_sandbox(no_sandbox)
        .max_browsers(1)
        .build()
        .await
        .unwrap();

    // Scraping: blocks images, fonts, media; keeps CSS and scripts.
    seen.lock().unwrap().clear();
    let p_scraping = engine
        .acquire_job(&JobOptions::default().resource_preset(ResourcePreset::Scraping))
        .await
        .unwrap();
    p_scraping.goto(&base_url).await.unwrap();
    assert_eq!(
        p_scraping.evaluate("window.__appLoaded === true").await.unwrap(),
        json!(true)
    );
    {
        let reqs = seen.lock().unwrap();
        assert!(!reqs.iter().any(|p| p == "/img.png"), "{reqs:?}");
        assert!(!reqs.iter().any(|p| p == "/font.woff2"), "{reqs:?}");
        assert!(reqs.iter().any(|p| p == "/style.css"), "{reqs:?}");
        assert!(reqs.iter().any(|p| p == "/app.js"), "{reqs:?}");
    }
    p_scraping.close().await.unwrap();

    // Minimal: also blocks stylesheets.
    seen.lock().unwrap().clear();
    let p_minimal = engine
        .acquire_job(&JobOptions::default().resource_preset(ResourcePreset::Minimal))
        .await
        .unwrap();
    p_minimal.goto(&base_url).await.unwrap();
    assert_eq!(
        p_minimal.evaluate("window.__appLoaded === true").await.unwrap(),
        json!(true)
    );
    {
        let reqs = seen.lock().unwrap();
        assert!(!reqs.iter().any(|p| p == "/img.png"), "{reqs:?}");
        assert!(!reqs.iter().any(|p| p == "/style.css"), "{reqs:?}");
        assert!(reqs.iter().any(|p| p == "/app.js"), "{reqs:?}");
    }
    p_minimal.close().await.unwrap();

    // Explicit block_resources([Script]) + block_url_patterns(["*tracker.js"]).
    seen.lock().unwrap().clear();
    let p_custom = engine
        .acquire_job(
            &JobOptions::default()
                .block_resources([ResourceType::Script])
                .block_url_patterns(["*tracker.js"]),
        )
        .await
        .unwrap();
    p_custom.goto(&base_url).await.unwrap();
    assert_eq!(
        p_custom
            .evaluate("[window.__appLoaded === true, window.__trackerLoaded === true]")
            .await
            .unwrap(),
        json!([false, false])
    );
    {
        let reqs = seen.lock().unwrap();
        assert!(!reqs.iter().any(|p| p == "/app.js"), "{reqs:?}");
        assert!(!reqs.iter().any(|p| p == "/tracker.js"), "{reqs:?}");
    }
    p_custom.close().await.unwrap();

    engine.shutdown(Duration::from_secs(5)).await.unwrap();
}
