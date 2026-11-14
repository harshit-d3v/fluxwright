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
        { "action": "screenshot" }
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
