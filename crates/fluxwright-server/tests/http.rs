//! Browser-backed contract tests. Require Chrome, like the existing Rust integration suite.

use std::time::Duration;

use axum::body::{to_bytes, Body};
use axum::http::{header, Request, StatusCode};
use base64::Engine;
use fluxwright::BrowserEngine;
use fluxwright_server::{HttpServer, Job, JobStatus, ServerConfig, MAX_RESULT_BYTES};
use serde_json::{json, Value};
use tower::ServiceExt;

const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

async fn request(
    server: &HttpServer,
    method: &str,
    path: &str,
    payload: Value,
) -> (StatusCode, Value) {
    let response = server
        .router()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(payload.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), MAX_RESULT_BYTES * 2)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

async fn submit(server: &HttpServer, payload: Value) -> Job {
    let (status, body) = request(server, "POST", "/v1/jobs", payload).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    serde_json::from_value(body).unwrap()
}

async fn finish(server: &HttpServer, job: Job) -> Job {
    tokio::time::timeout(Duration::from_secs(90), async {
        loop {
            let (status, body) =
                request(server, "GET", &format!("/v1/jobs/{}", job.id), Value::Null).await;
            assert_eq!(status, StatusCode::OK);
            let job: Job = serde_json::from_value(body).unwrap();
            if matches!(
                job.status,
                JobStatus::Succeeded | JobStatus::Failed | JobStatus::Cancelled
            ) {
                return job;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("job finished")
}

/// A Chrome to connect to over CDP, as a remote browser is.
async fn debuggable_chrome() -> benchmark_server::DebuggableChrome {
    benchmark_server::DebuggableChrome::start(&fluxwright::find_browser(true).unwrap()).await
}

async fn server() -> HttpServer {
    let engine = BrowserEngine::builder()
        .max_browsers(1)
        .max_contexts_per_browser(1)
        .memory_ceiling_mb(16_384)
        .acquire_timeout(Duration::from_secs(60))
        .build()
        .await
        .unwrap();
    HttpServer::new(engine, ServerConfig::new(TOKEN.into()).unwrap()).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn executes_steps_returns_results_and_isolates_jobs() {
    let fixture = benchmark_server::spawn("127.0.0.1:0").await.unwrap();
    let server = server().await;
    let submitted = submit(
        &server,
        json!({ "steps": [
        { "action": "goto", "url": format!("{}/forms", fixture.base_url) },
        { "action": "fill", "selector": "#name", "value": "Ada" },
        { "action": "click", "selector": "#go" },
        { "action": "evaluate", "expression": "document.querySelector('#name').value" },
        { "action": "title" },
        { "action": "screenshot" },
        { "action": "screenshot", "type": "jpeg", "quality": 60 },
        { "action": "screenshot", "type": "webp", "quality": 80 },
        { "action": "pdf", "format": "A4", "print_background": true }
    ] }),
    )
    .await;
    let job = finish(&server, submitted).await;
    assert_eq!(job.status, JobStatus::Succeeded, "{:?}", job.error);
    assert_eq!(job.attempts, 1);
    let results = job.results.unwrap();
    assert_eq!(results[3], "Ada-ok");
    assert_eq!(results[4], "forms");
    assert_eq!(results[5]["mime_type"], "image/png");
    assert!(results[5]["base64"].as_str().unwrap().starts_with("iVBOR"));
    use base64::Engine;
    assert_eq!(results[6]["mime_type"], "image/jpeg");
    let jpg = base64::engine::general_purpose::STANDARD
        .decode(results[6]["base64"].as_str().unwrap())
        .unwrap();
    assert_eq!(jpg[..3], [0xFF, 0xD8, 0xFF]);
    assert_eq!(results[7]["mime_type"], "image/webp");
    let webp = base64::engine::general_purpose::STANDARD
        .decode(results[7]["base64"].as_str().unwrap())
        .unwrap();
    assert!(webp.len() >= 12 && &webp[..4] == b"RIFF" && &webp[8..12] == b"WEBP");
    assert_eq!(results[8]["mime_type"], "application/pdf");
    let pdf = base64::engine::general_purpose::STANDARD
        .decode(results[8]["base64"].as_str().unwrap())
        .unwrap();
    assert!(pdf.starts_with(b"%PDF-"));

    let set_cookie = submit(
        &server,
        json!({ "steps": [
        { "action": "goto", "url": format!("{}/set-cookie", fixture.base_url) },
        { "action": "evaluate", "expression": "document.cookie" }
    ] }),
    )
    .await;
    assert_eq!(
        finish(&server, set_cookie).await.results.unwrap()[1],
        "fw=secret"
    );
    let read_cookie = submit(
        &server,
        json!({ "steps": [
        { "action": "goto", "url": format!("{}/show-cookie", fixture.base_url) },
        { "action": "text_content", "selector": "#c" }
    ] }),
    )
    .await;
    assert_eq!(finish(&server, read_cookie).await.results.unwrap()[1], "");

    let failed = submit(
        &server,
        json!({ "steps": [
        { "action": "evaluate", "expression": "(() => { throw new Error('fixture failure') })()" }
    ] }),
    )
    .await;
    let failed = finish(&server, failed).await;
    assert_eq!(failed.status, JobStatus::Failed);
    assert_eq!(failed.step_index, Some(0));
    assert!(failed.error.unwrap().contains("fixture failure"));
    let large = submit(
        &server,
        json!({ "steps": [
        { "action": "evaluate", "expression": "'x'.repeat(1048576)" }
    ] }),
    )
    .await;
    let large = finish(&server, large).await;
    assert_eq!(large.status, JobStatus::Failed);
    assert!(large.error.unwrap().contains("results exceed"));
    server.shutdown(Duration::from_secs(5)).await.unwrap();
    fixture.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn cancels_running_jobs_and_returns_the_only_fleet_slot() {
    let server = server().await;
    let waiting = submit(
        &server,
        json!({ "steps": [
        { "action": "wait_for_selector", "selector": "#never-created" }
    ] }),
    )
    .await;
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let (_, body) = request(
                &server,
                "GET",
                &format!("/v1/jobs/{}", waiting.id),
                Value::Null,
            )
            .await;
            assert_ne!(body["status"], "failed", "{body}");
            if body["status"] == "running" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let next = submit(&server, json!({ "steps": [{ "action": "title" }] })).await;
    let (status, body) = request(
        &server,
        "DELETE",
        &format!("/v1/jobs/{}", waiting.id),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "cancelled");
    let next = finish(&server, next).await;
    assert_eq!(next.status, JobStatus::Succeeded, "{:?}", next.error);
    let (_, cancelled) = request(
        &server,
        "GET",
        &format!("/v1/jobs/{}", waiting.id),
        Value::Null,
    )
    .await;
    assert_eq!(cancelled["status"], "cancelled");
    server.shutdown(Duration::from_secs(5)).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn observability_metrics_and_traceparent_propagation() {
    let server = server().await;
    let tp = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
    let response = server
        .router()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/jobs")
                .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                .header(header::CONTENT_TYPE, "application/json")
                .header("traceparent", tp)
                .body(Body::from(r#"{"steps":[{"action":"title"}]}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let outbound_tp = response
        .headers()
        .get("traceparent")
        .unwrap()
        .to_str()
        .unwrap();
    assert!(outbound_tp.starts_with("00-4bf92f3577b34da6a3ce929d0e0e4736-"));

    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let job: Job = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        job.trace_id.as_deref(),
        Some("4bf92f3577b34da6a3ce929d0e0e4736")
    );

    let finished = finish(&server, job).await;
    assert_eq!(finished.status, JobStatus::Succeeded);

    // Scrape /metrics and verify that succeeded count reflects real browser run
    let resp = server
        .router()
        .oneshot(
            Request::builder()
                .uri("/metrics")
                .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let metrics_bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
    let metrics_text = String::from_utf8(metrics_bytes.to_vec()).unwrap();
    assert!(metrics_text.contains("fluxwright_http_jobs_total{status=\"succeeded\"} 1"));

    server.shutdown(Duration::from_secs(5)).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn succeeded_jobs_and_retried_jobs_omit_failure_evidence() {
    let fixture = benchmark_server::spawn("127.0.0.1:0").await.unwrap();
    let server = server().await;

    let submitted = submit(
        &server,
        json!({
            "steps": [
                { "action": "goto", "url": format!("{}/forms", fixture.base_url) },
                { "action": "evaluate", "expression": "console.log('diagnostic log alpha'); console.warn('diagnostic warn beta'); 42" },
                { "action": "title" }
            ]
        }),
    )
    .await;

    let job: Job = finish(&server, submitted).await;
    assert_eq!(job.status, JobStatus::Succeeded, "{:?}", job.error);
    assert!(job.failure_evidence.is_none());
    server.shutdown(Duration::from_secs(5)).await.unwrap();

    // Now test a job that fails attempt 1 (populating failure_evidence) and succeeds on attempt 2:
    let chrome = debuggable_chrome().await;
    let ws_url = chrome.ws_url.clone();

    let rest = ws_url.strip_prefix("ws://").unwrap();
    let (upstream_addr, ws_path) = rest.split_once('/').unwrap();
    let upstream_addr = upstream_addr.to_string();
    let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_ws_url = format!("ws://{}/{}", proxy_listener.local_addr().unwrap(), ws_path);
    let drop_first = std::sync::Arc::new(tokio::sync::Notify::new());
    let drop_first_proxy = drop_first.clone();

    let proxy_task = tokio::spawn(async move {
        let mut conn_index = 0usize;
        while let Ok((mut client, _)) = proxy_listener.accept().await {
            conn_index += 1;
            let addr = upstream_addr.clone();
            let drop_notify = drop_first_proxy.clone();
            tokio::spawn(async move {
                if let Ok(mut upstream) = tokio::net::TcpStream::connect(&addr).await {
                    if conn_index == 1 {
                        tokio::select! {
                            _ = tokio::io::copy_bidirectional(&mut client, &mut upstream) => {}
                            _ = drop_notify.notified() => {}
                        }
                    } else {
                        let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
                    }
                }
            });
        }
    });

    let attempt_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let attempt_count_route = attempt_count.clone();
    let drop_first_route = drop_first.clone();
    let flaky_app = axum::Router::new().route(
        "/flaky",
        axum::routing::get(move || {
            let count = attempt_count_route.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            let drop_notify = drop_first_route.clone();
            async move {
                if count == 1 {
                    drop_notify.notify_waiters();
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
                axum::response::Html("<!doctype html><title>recovered</title><p>ok</p>")
            }
        }),
    );
    let flaky_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let flaky_url = format!("http://{}/flaky", flaky_listener.local_addr().unwrap());
    let flaky_task = tokio::spawn(async move {
        let _ = axum::serve(flaky_listener, flaky_app).await;
    });

    let remote_eng = BrowserEngine::connect_over_cdp(&proxy_ws_url).await.unwrap();
    let retry_server =
        HttpServer::new(remote_eng, ServerConfig::new(TOKEN.into()).unwrap()).unwrap();

    let retried_sub = submit(
        &retry_server,
        json!({
            "retries": 1,
            "steps": [
                { "action": "goto", "url": format!("{}/forms", fixture.base_url) },
                { "action": "evaluate", "expression": "console.error('attempt 1 error'); 1" },
                { "action": "goto", "url": flaky_url },
                { "action": "title" }
            ]
        }),
    )
    .await;

    let retried_job = finish(&retry_server, retried_sub).await;
    assert_eq!(retried_job.status, JobStatus::Succeeded, "{:?}", retried_job.error);
    assert_eq!(retried_job.attempts, 2);
    assert_eq!(retried_job.results.as_ref().unwrap()[3], "recovered");
    assert!(
        retried_job.failure_evidence.is_none(),
        "succeeded retry job must not retain failure_evidence from attempt 1: {:?}",
        retried_job.failure_evidence
    );

    retry_server.shutdown(Duration::from_secs(5)).await.unwrap();
    drop(chrome);
    proxy_task.abort();
    flaky_task.abort();
    fixture.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn captures_failure_screenshot_and_logs_on_failure() {
    let fixture = benchmark_server::spawn("127.0.0.1:0").await.unwrap();
    let server = server().await;

    let submitted = submit(
        &server,
        json!({
            "steps": [
                { "action": "goto", "url": format!("{}/forms", fixture.base_url) },
                { "action": "evaluate", "expression": "console.error('critical failure diagnostic'); 1" },
                { "action": "evaluate", "expression": "(() => { throw new Error('intentional failure in test') })()" }
            ]
        }),
    )
    .await;

    let job = finish(&server, submitted).await;
    assert_eq!(job.status, JobStatus::Failed);
    assert!(job.error.as_ref().unwrap().contains("intentional failure in test"));

    // Verify failure evidence contains console logs, network entries, and failure screenshot
    let evidence = job.evidence().expect("failure evidence should be attached to failed job");
    assert!(
        evidence
            .console
            .iter()
            .any(|c| c.text.contains("critical failure diagnostic")),
        "console should capture error before failure: {:?}",
        evidence.console
    );
    assert!(
        evidence
            .network
            .iter()
            .any(|n| n.url.contains("/forms")),
        "network should record requests prior to failure: {:?}",
        evidence.network
    );

    // Verify screenshot is captured as base64 PNG
    let screenshot_b64 = evidence.screenshot.as_ref().expect("failure screenshot must be present");
    assert!(
        screenshot_b64.starts_with("iVBOR"),
        "screenshot must start with PNG base64 magic bytes"
    );
    let png_bytes = base64::engine::general_purpose::STANDARD
        .decode(screenshot_b64)
        .expect("screenshot must be valid base64");
    assert_eq!(
        &png_bytes[0..8],
        b"\x89PNG\r\n\x1a\n",
        "screenshot must decode to a valid PNG header"
    );

    server.shutdown(Duration::from_secs(5)).await.unwrap();
    fixture.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn captures_failure_screenshot_on_step_timeout() {
    let fixture = benchmark_server::spawn("127.0.0.1:0").await.unwrap();
    let server = server().await;

    let submitted = submit(
        &server,
        json!({
            "timeout_ms": 1500,
            "steps": [
                { "action": "goto", "url": format!("{}/forms", fixture.base_url) },
                { "action": "wait_for_selector", "selector": "#never-appears-in-dom" }
            ]
        }),
    )
    .await;

    let job = finish(&server, submitted).await;
    assert_eq!(job.status, JobStatus::Failed);
    assert!(
        job.error.as_ref().unwrap().contains("timed out"),
        "error should indicate timeout: {:?}",
        job.error
    );

    // Verify failure evidence captured screenshot on timeout
    let evidence = job.evidence().expect("failure evidence should be present on timeout");
    let screenshot_b64 = evidence.screenshot.as_ref().expect("screenshot must be captured on timeout");
    assert!(
        screenshot_b64.starts_with("iVBOR"),
        "timeout screenshot must be base64 PNG"
    );
    let png_bytes = base64::engine::general_purpose::STANDARD
        .decode(screenshot_b64)
        .expect("must decode base64");
    assert_eq!(&png_bytes[0..8], b"\x89PNG\r\n\x1a\n");

    server.shutdown(Duration::from_secs(5)).await.unwrap();
    fixture.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn executes_job_via_remote_cdp_engine() {
    let chrome = debuggable_chrome().await;
    let remote_eng = BrowserEngine::connect_over_cdp(&chrome.ws_url).await.unwrap();
    let server = HttpServer::new(remote_eng, ServerConfig::new(TOKEN.into()).unwrap()).unwrap();

    let submitted = submit(
        &server,
        json!({
            "steps": [
                { "action": "title" },
                { "action": "evaluate", "expression": "'remote-ok'" }
            ]
        }),
    )
    .await;

    let job = finish(&server, submitted).await;
    assert_eq!(job.status, JobStatus::Succeeded, "{:?}", job.error);
    let results = job.results.unwrap();
    assert_eq!(results[1], "remote-ok");

    server.shutdown(Duration::from_secs(5)).await.unwrap();
    drop(chrome);
}

#[tokio::test(flavor = "multi_thread")]
async fn worker_pulls_and_executes_jobs_from_coordinator() {
    let fixture = benchmark_server::spawn("127.0.0.1:0").await.unwrap();
    let coordinator = HttpServer::coordinator(ServerConfig::new(TOKEN.into()).unwrap()).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let coord_addr = listener.local_addr().unwrap();
    let coord_router = coordinator.router();
    let coord_task = tokio::spawn(async move {
        let _ = axum::serve(listener, coord_router).await;
    });

    let engine = BrowserEngine::builder()
        .max_browsers(1)
        .max_contexts_per_browser(2)
        .memory_ceiling_mb(16_384)
        .acquire_timeout(Duration::from_secs(60))
        .build()
        .await
        .unwrap();
    let worker = fluxwright_server::spawn_worker(
        engine.clone(),
        format!("http://{coord_addr}"),
        TOKEN.into(),
        Some("http-test-worker".into()),
        2,
    )
    .unwrap();

    let ok_sub = submit(
        &coordinator,
        json!({ "steps": [
            { "action": "goto", "url": format!("{}/forms", fixture.base_url) },
            { "action": "fill", "selector": "#name", "value": "Cluster" },
            { "action": "click", "selector": "#go" },
            { "action": "title" }
        ] }),
    )
    .await;

    let fail_sub = submit(
        &coordinator,
        json!({
            "timeout_ms": 3000,
            "steps": [
                { "action": "goto", "url": format!("{}/forms", fixture.base_url) },
                { "action": "wait_for_selector", "selector": "#missing-cluster-el" }
            ]
        }),
    )
    .await;

    let ok_job = finish(&coordinator, ok_sub).await;
    assert_eq!(ok_job.status, JobStatus::Succeeded, "{:?}", ok_job.error);
    assert_eq!(ok_job.results.unwrap()[3], "forms");

    let fail_job = finish(&coordinator, fail_sub).await;
    assert_eq!(fail_job.status, JobStatus::Failed);
    let shot = fail_job
        .evidence()
        .and_then(|e| e.screenshot.as_ref())
        .expect("cluster failure evidence includes screenshot");
    assert!(shot.starts_with("iVBOR"));

    // Verify graceful worker shutdown lets an already-leased job finish before exiting.
    let drain_sub = submit(
        &coordinator,
        json!({ "steps": [
            { "action": "evaluate", "expression": "new Promise(r => setTimeout(() => r('drained-ok'), 250))" }
        ] }),
    )
    .await;
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let (status, body) =
                request(&coordinator, "GET", &format!("/v1/jobs/{}", drain_sub.id), Value::Null).await;
            assert_eq!(status, StatusCode::OK);
            let j: Job = serde_json::from_value(body).unwrap();
            if j.status != JobStatus::Queued {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("worker leased drain job");

    worker.shutdown(Duration::from_secs(10)).await;
    let drain_job = finish(&coordinator, drain_sub).await;
    assert_eq!(drain_job.status, JobStatus::Succeeded, "{:?}", drain_job.error);
    assert_eq!(drain_job.results.unwrap()[0], "drained-ok");

    // Verify a job still running past shutdown grace is released back cleanly without losing its
    // only attempt, then picked up and finished by a replacement worker.
    let worker2 = fluxwright_server::spawn_worker(
        engine.clone(),
        format!("http://{coord_addr}"),
        TOKEN.into(),
        Some("http-test-worker-2".into()),
        1,
    )
    .unwrap();
    let long_sub = submit(
        &coordinator,
        json!({
            "retries": 0,
            "steps": [
                { "action": "evaluate", "expression": "new Promise(r => setTimeout(() => r('released-ok'), 800))" }
            ]
        }),
    )
    .await;
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let (status, body) =
                request(&coordinator, "GET", &format!("/v1/jobs/{}", long_sub.id), Value::Null).await;
            assert_eq!(status, StatusCode::OK);
            let j: Job = serde_json::from_value(body).unwrap();
            if j.status == JobStatus::Running {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("worker2 leased long job");

    worker2.shutdown(Duration::from_millis(40)).await;
    let (_, after_rel) =
        request(&coordinator, "GET", &format!("/v1/jobs/{}", long_sub.id), Value::Null).await;
    assert_eq!(after_rel["status"], "queued", "{after_rel}");
    assert_eq!(after_rel["attempts"], 0, "{after_rel}");

    let worker3 = fluxwright_server::spawn_worker(
        engine.clone(),
        format!("http://{coord_addr}"),
        TOKEN.into(),
        Some("http-test-worker-3".into()),
        1,
    )
    .unwrap();
    let rel_job = finish(&coordinator, long_sub).await;
    assert_eq!(rel_job.status, JobStatus::Succeeded, "{:?}", rel_job.error);
    assert_eq!(rel_job.results.unwrap()[0], "released-ok");
    worker3.abort();

    coord_task.abort();
    let _ = engine.shutdown(Duration::from_secs(5)).await;
    coordinator.shutdown(Duration::ZERO).await.unwrap();
    fixture.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn executes_rich_input_and_form_steps() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let html = r#"<!doctype html>
        <input id="inp" value="old" onkeydown="if(event.key==='Enter') window.__enter=true">
        <button id="dbl" ondblclick="this.dataset.dbl='1'">Dbl</button>
        <input id="chk" type="checkbox">
        <select id="sel">
          <option value="a">Alpha</option>
          <option value="b">Beta</option>
        </select>"#;
    let app = axum::Router::new().route("/", axum::routing::get(move || async move { axum::response::Html(html) }));
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let server = server().await;
    let sub = submit(
        &server,
        json!({ "steps": [
            { "action": "goto", "url": format!("http://127.0.0.1:{port}/") },
            { "action": "clear", "selector": "#inp" },
            { "action": "type", "selector": "#inp", "text": "new", "delay_ms": 2 },
            { "action": "press", "selector": "#inp", "key": "Enter" },
            { "action": "dblclick", "selector": "#dbl" },
            { "action": "check", "selector": "#chk" },
            { "action": "select_option", "selector": "#sel", "values": { "label": "Beta" } },
            { "action": "evaluate", "expression": "[inp.value, !!window.__enter, dbl.dataset.dbl, chk.checked, sel.value]" }
        ] }),
    )
    .await;

    let job = finish(&server, sub).await;
    assert_eq!(job.status, JobStatus::Succeeded, "{:?}", job.error);
    let results = job.results.unwrap();
    assert_eq!(results[6], json!(["b"]));
    assert_eq!(results[7], json!(["new", true, "1", true, "b"]));
    server.shutdown(Duration::from_secs(5)).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn executes_locator_read_and_state_steps() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let html = r#"<!doctype html>
        <ul><li>One</li><li>Two</li></ul>
        <div id="box"><b>Hi</b></div>
        <input id="inp" value="val" data-k="v">
        <input id="dis" disabled>
        <input id="chk" type="checkbox" checked>
        <p id="hid" style="display:none">Secret</p>"#;
    let app = axum::Router::new().route("/", axum::routing::get(move || async move { axum::response::Html(html) }));
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let server = server().await;
    let sub = submit(
        &server,
        json!({ "steps": [
            { "action": "goto", "url": format!("http://127.0.0.1:{port}/") },
            { "action": "count", "selector": "li" },
            { "action": "all_text_contents", "selector": "li" },
            { "action": "all_inner_texts", "selector": "li" },
            { "action": "inner_text", "selector": "#box" },
            { "action": "inner_html", "selector": "#box" },
            { "action": "input_value", "selector": "#inp" },
            { "action": "get_attribute", "selector": "#inp", "name": "data-k" },
            { "action": "is_visible", "selector": "#box" },
            { "action": "is_hidden", "selector": "#hid" },
            { "action": "is_enabled", "selector": "#inp" },
            { "action": "is_disabled", "selector": "#dis" },
            { "action": "is_checked", "selector": "#chk" },
            { "action": "is_editable", "selector": "#inp" }
        ] }),
    )
    .await;

    let job = finish(&server, sub).await;
    assert_eq!(job.status, JobStatus::Succeeded, "{:?}", job.error);
    let results = job.results.unwrap();
    assert_eq!(results[1], json!(2));
    assert_eq!(results[2], json!(["One", "Two"]));
    assert_eq!(results[3], json!(["One", "Two"]));
    assert_eq!(results[4], json!("Hi"));
    assert_eq!(results[5], json!("<b>Hi</b>"));
    assert_eq!(results[6], json!("val"));
    assert_eq!(results[7], json!("v"));
    assert_eq!(results[8], json!(true));
    assert_eq!(results[9], json!(true));
    assert_eq!(results[10], json!(true));
    assert_eq!(results[11], json!(true));
    assert_eq!(results[12], json!(true));
    assert_eq!(results[13], json!(true));

    // strict: true rejects multiple matches.
    let bad = submit(
        &server,
        json!({ "steps": [
            { "action": "goto", "url": format!("http://127.0.0.1:{port}/") },
            { "action": "inner_text", "selector": "li", "strict": true }
        ] }),
    )
    .await;
    let bad_job = finish(&server, bad).await;
    assert_eq!(bad_job.status, JobStatus::Failed);
    assert!(bad_job.error.unwrap_or_default().contains("strict mode violation"));

    server.shutdown(Duration::from_secs(5)).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn executes_navigation_and_wait_steps() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    let hits = Arc::new(AtomicUsize::new(0));
    let hits_a = Arc::clone(&hits);
    let app = axum::Router::new()
        .route(
            "/a",
            axum::routing::get(move || {
                let n = hits_a.fetch_add(1, Ordering::SeqCst) + 1;
                async move { axum::response::Html(format!("<title>S-{n}</title><p id='p'>A</p>")) }
            }),
        )
        .route(
            "/b",
            axum::routing::get(|| async {
                axum::response::Html("<title>SB</title><script>setTimeout(() => { window.__ready = 9; }, 60);</script>")
            }),
        )
        .route(
            "/delayed",
            axum::routing::get(|| async {
                axum::response::Html("<script>setTimeout(() => { location.href = '/done'; }, 80);</script>")
            }),
        )
        .route(
            "/done",
            axum::routing::get(|| async { axum::response::Html("<title>SDone</title>") }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let base = format!("http://127.0.0.1:{port}");

    let server = server().await;
    let sub = submit(
        &server,
        json!({ "steps": [
            { "action": "goto", "url": format!("{base}/a") },
            { "action": "url" },
            { "action": "reload" },
            { "action": "title" },
            { "action": "goto", "url": format!("{base}/b") },
            { "action": "wait_for_function", "expression": "window.__ready", "polling_ms": 20 },
            { "action": "go_back" },
            { "action": "url" },
            { "action": "go_forward" },
            { "action": "url" },
            { "action": "wait_for_load_state", "state": "load" },
            { "action": "goto", "url": format!("{base}/delayed") },
            { "action": "wait_for_url", "url": "**/done" },
            { "action": "url" },
            { "action": "set_content", "html": "<h1 id='sc'>Inline</h1>" },
            { "action": "inner_text", "selector": "#sc" }
        ] }),
    )
    .await;

    let job = finish(&server, sub).await;
    assert_eq!(job.status, JobStatus::Succeeded, "{:?}", job.error);
    let results = job.results.unwrap();
    assert_eq!(results[1], json!(format!("{base}/a")));
    assert_eq!(results[3], json!("S-2"));
    assert_eq!(results[5], json!(9));
    assert_eq!(results[6], json!(true));
    assert_eq!(results[7], json!(format!("{base}/a")));
    assert_eq!(results[8], json!(true));
    assert_eq!(results[9], json!(format!("{base}/b")));
    assert_eq!(results[13], json!(format!("{base}/done")));
    assert_eq!(results[15], json!("Inline"));

    server.shutdown(Duration::from_secs(5)).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn executes_init_script_and_tag_steps() {
    let app = axum::Router::new()
        .route(
            "/",
            axum::routing::get(|| async {
                axum::response::Html("<h1 id='h'></h1><script>document.getElementById('h').textContent = String(window.__early)</script>")
            }),
        )
        .route(
            "/u.js",
            axum::routing::get(|| async {
                ([(axum::http::header::CONTENT_TYPE, "application/javascript")], "window.__u = 'js-ok';")
            }),
        )
        .route(
            "/u.css",
            axum::routing::get(|| async {
                ([(axum::http::header::CONTENT_TYPE, "text/css")], "#h { padding-left: 19px; }")
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let base = format!("http://127.0.0.1:{port}");

    let server = server().await;
    let sub = submit(
        &server,
        json!({ "steps": [
            { "action": "add_init_script", "script": "window.__early = 99;" },
            { "action": "goto", "url": format!("{base}/") },
            { "action": "inner_text", "selector": "#h" },
            { "action": "add_script_tag", "content": "window.__c = 5;" },
            { "action": "add_script_tag", "url": "/u.js" },
            { "action": "add_style_tag", "content": "#h { color: rgb(0, 128, 0); }" },
            { "action": "add_style_tag", "url": "/u.css" },
            { "action": "evaluate", "expression": "[window.__c, window.__u, getComputedStyle(document.getElementById('h')).color, getComputedStyle(document.getElementById('h')).paddingLeft]" }
        ] }),
    )
    .await;

    let job = finish(&server, sub).await;
    assert_eq!(job.status, JobStatus::Succeeded, "{:?}", job.error);
    let results = job.results.unwrap();
    assert_eq!(results[2], json!("99"));
    assert_eq!(results[7], json!([5, "js-ok", "rgb(0, 128, 0)", "19px"]));

    server.shutdown(Duration::from_secs(5)).await.unwrap();
}

async fn connect_cdp_ws(
    url: &str,
    bearer: Option<&str>,
) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let mut req = url.into_client_request().unwrap();
    if let Some(tok) = bearer {
        req.headers_mut().insert(
            header::AUTHORIZATION,
            header::HeaderValue::from_str(&format!("Bearer {tok}")).unwrap(),
        );
    }
    let (ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    ws
}

async fn cdp_rpc(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    id: u64,
    method: &str,
    params: Value,
    session_id: Option<&str>,
) -> Value {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    let mut payload = json!({ "id": id, "method": method, "params": params });
    if let Some(sid) = session_id {
        payload["sessionId"] = json!(sid);
    }
    ws.send(Message::Text(payload.to_string().into()))
        .await
        .unwrap();

    tokio::time::timeout(Duration::from_secs(30), async {
        while let Some(msg) = ws.next().await {
            let msg = msg.unwrap();
            if let Message::Text(text) = msg {
                let v: Value = serde_json::from_str(&text).unwrap();
                if v.get("id").and_then(Value::as_u64) == Some(id) {
                    return v;
                }
            }
        }
        panic!("WebSocket closed waiting for RPC id {id} ({method})");
    })
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn cdp_gateway_serves_json_version_and_connect_over_cdp() {
    let fixture = benchmark_server::spawn("127.0.0.1:0").await.unwrap();
    let engine = BrowserEngine::builder()
        .max_browsers(1)
        .max_contexts_per_browser(2)
        .memory_ceiling_mb(16_384)
        .acquire_timeout(Duration::from_secs(60))
        .build()
        .await
        .unwrap();
    let mut cfg = ServerConfig::new(TOKEN.into()).unwrap();
    cfg.allow_query_token = true;
    let server = HttpServer::new(engine.clone(), cfg).unwrap();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let srv_handle = tokio::spawn({
        let router = server.router();
        async move { axum::serve(listener, router).await }
    });

    // Verify GET /json/version discovery over router with Bearer auth.
    let ver_resp = server
        .router()
        .oneshot(
            Request::builder()
                .uri("/json/version")
                .header(header::HOST, addr.to_string())
                .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(ver_resp.status(), StatusCode::OK);
    let ver_json: Value =
        serde_json::from_slice(&to_bytes(ver_resp.into_body(), 16_384).await.unwrap()).unwrap();
    assert_eq!(ver_json["webSocketDebuggerUrl"], format!("ws://{addr}/cdp"));

    // Connect Fluxwright's own `connect_over_cdp` client through `/cdp?token=...`.
    let client_engine =
        BrowserEngine::connect_over_cdp(format!("ws://{addr}/cdp?token={TOKEN}"))
            .await
            .unwrap();

    let metrics_resp = server
        .router()
        .oneshot(
            Request::builder()
                .uri("/metrics")
                .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let metrics_txt =
        String::from_utf8(to_bytes(metrics_resp.into_body(), 65_536).await.unwrap().to_vec())
            .unwrap();
    assert!(metrics_txt.contains("fluxwright_cdp_sessions_active 1\n"));
    assert!(metrics_txt.contains("fluxwright_cdp_sessions_total 1\n"));

    let page = client_engine.acquire().await.unwrap();
    page.goto(&format!("{}/forms", fixture.base_url)).await.unwrap();
    assert_eq!(page.title().await.unwrap(), "forms");
    page.fill("#name", "Gateway").await.unwrap();
    page.click("#go").await.unwrap();
    assert_eq!(
        page.evaluate("document.querySelector('#name').value")
            .await
            .unwrap(),
        json!("Gateway-ok")
    );
    drop(page);

    client_engine.shutdown(Duration::from_secs(5)).await.unwrap();

    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if engine.metrics().await.active_leases == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("lease released after client disconnect");

    server.shutdown(Duration::from_secs(5)).await.unwrap();
    srv_handle.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn cdp_gateway_isolates_concurrent_clients_and_blocks_escape() {
    let fixture = benchmark_server::spawn("127.0.0.1:0").await.unwrap();
    let engine = BrowserEngine::builder()
        .max_browsers(1)
        .max_contexts_per_browser(2)
        .memory_ceiling_mb(16_384)
        .acquire_timeout(Duration::from_secs(60))
        .build()
        .await
        .unwrap();
    let server = HttpServer::new(engine.clone(), ServerConfig::new(TOKEN.into()).unwrap()).unwrap();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let srv_handle = tokio::spawn({
        let router = server.router();
        async move { axum::serve(listener, router).await }
    });

    let ws_url = format!("ws://{addr}/cdp");
    let mut ws1 = connect_cdp_ws(&ws_url, Some(TOKEN)).await;
    let mut ws2 = connect_cdp_ws(&ws_url, Some(TOKEN)).await;

    // Client 1 creates a page and sets a cookie on its isolated context.
    let c1_target = cdp_rpc(&mut ws1, 1, "Target.createTarget", json!({ "url": "about:blank" }), None)
        .await["result"]["targetId"]
        .as_str()
        .unwrap()
        .to_string();
    let s1 = cdp_rpc(&mut ws1, 2, "Target.attachToTarget", json!({ "targetId": c1_target, "flatten": true }), None)
        .await["result"]["sessionId"]
        .as_str()
        .unwrap()
        .to_string();
    cdp_rpc(&mut ws1, 3, "Page.enable", json!({}), Some(&s1)).await;
    cdp_rpc(&mut ws1, 4, "Page.navigate", json!({ "url": format!("{}/set-cookie", fixture.base_url) }), Some(&s1)).await;

    let cookie1 = tokio::time::timeout(Duration::from_secs(10), async {
        for seq in 5.. {
            let res = cdp_rpc(
                &mut ws1,
                seq,
                "Runtime.evaluate",
                json!({ "expression": "document.cookie", "returnByValue": true }),
                Some(&s1),
            )
            .await;
            let val = res["result"]["result"]["value"].as_str().unwrap_or("");
            if val.contains("fw=secret") {
                return val.to_string();
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        unreachable!()
    })
    .await
    .expect("client 1 cookie set");
    assert!(cookie1.contains("fw=secret"));

    // Client 2 creates its own page on the same browser and verifies cookie + target isolation.
    let c2_target = cdp_rpc(&mut ws2, 1, "Target.createTarget", json!({ "url": "about:blank" }), None)
        .await["result"]["targetId"]
        .as_str()
        .unwrap()
        .to_string();
    let s2 = cdp_rpc(&mut ws2, 2, "Target.attachToTarget", json!({ "targetId": c2_target, "flatten": true }), None)
        .await["result"]["sessionId"]
        .as_str()
        .unwrap()
        .to_string();
    cdp_rpc(&mut ws2, 3, "Page.enable", json!({}), Some(&s2)).await;
    cdp_rpc(&mut ws2, 4, "Page.navigate", json!({ "url": format!("{}/", fixture.base_url) }), Some(&s2)).await;

    tokio::time::timeout(Duration::from_secs(10), async {
        for seq in 5.. {
            let res = cdp_rpc(
                &mut ws2,
                seq,
                "Runtime.evaluate",
                json!({ "expression": "document.title + '|' + document.cookie", "returnByValue": true }),
                Some(&s2),
            )
            .await;
            let val = res["result"]["result"]["value"].as_str().unwrap_or("");
            if val.starts_with("home|") {
                assert!(!val.contains("fw=secret"), "cookie leaked across CDP leases: {val}");
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("client 2 navigation completed");

    // Client 2 cannot see or attach to Client 1's target, nor run arbitrary Browser.* commands.
    let targets2 = cdp_rpc(&mut ws2, 100, "Target.getTargets", json!({}), None).await;
    let infos = targets2["result"]["targetInfos"].as_array().unwrap();
    assert!(!infos.iter().any(|t| t["targetId"].as_str() == Some(c1_target.as_str())));

    let hijack = cdp_rpc(
        &mut ws2,
        101,
        "Target.attachToTarget",
        json!({ "targetId": c1_target, "flatten": true }),
        None,
    )
    .await;
    assert_eq!(hijack["error"]["code"], -32000);

    let forbidden = cdp_rpc(&mut ws2, 102, "Browser.crash", json!({}), None).await;
    assert_eq!(forbidden["error"]["code"], -32601);

    // Client 1's Browser.close only ends Client 1's lease; Client 2 stays healthy on the same browser.
    let closed1 = cdp_rpc(&mut ws1, 103, "Browser.close", json!({}), None).await;
    assert_eq!(closed1["result"], json!({}));
    drop(ws1);

    let still_alive = cdp_rpc(
        &mut ws2,
        104,
        "Runtime.evaluate",
        json!({ "expression": "1 + 1", "returnByValue": true }),
        Some(&s2),
    )
    .await;
    assert_eq!(still_alive["result"]["result"]["value"], 2);
    assert_eq!(engine.metrics().await.browsers, 1);

    drop(ws2);
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if engine.metrics().await.active_leases == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("all leases released after client disconnect");

    server.shutdown(Duration::from_secs(5)).await.unwrap();
    srv_handle.abort();
}
