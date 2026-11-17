use axum::body::{to_bytes, Body};
use axum::http::Request;
use tower::ServiceExt;

use super::*;

const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// A browser that never finishes starting.
struct NeverStarts;

impl fluxwright::BrowserSource for NeverStarts {
    fn launch(
        &self,
        _: &fluxwright::LaunchOptions,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = fluxwright_cdp::Result<Arc<fluxwright_cdp::CdpBrowser>>> + Send + '_>,
    > {
        Box::pin(std::future::pending())
    }
}

/// No Chrome needed: work stays queued until cancelled or timed out.
async fn idle_engine() -> BrowserEngine {
    BrowserEngine::builder()
        .source(Arc::new(NeverStarts))
        .acquire_timeout(Duration::from_secs(300))
        .build()
        .await
        .unwrap()
}

async fn server(capacity: usize) -> HttpServer {
    let engine = idle_engine().await;
    let mut config = ServerConfig::new(TOKEN.into()).unwrap();
    config.max_jobs = capacity;
    HttpServer::new(engine, config).unwrap()
}

async fn call(server: &HttpServer, method: &str, path: &str, body: impl Into<Body>) -> Response {
    server
        .router()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(body.into())
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn body(response: Response) -> Value {
    let bytes = to_bytes(response.into_body(), MAX_RESULT_BYTES * 2)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

async fn submit_job(server: &HttpServer) -> Job {
    let response = call(
        server,
        "POST",
        "/v1/jobs",
        r#"{"steps":[{"action":"title"}]}"#,
    )
    .await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let location = response.headers()[header::LOCATION]
        .to_str()
        .unwrap()
        .to_owned();
    let job: Job = serde_json::from_value(body(response).await).unwrap();
    assert_eq!(location, format!("/v1/jobs/{}", job.id));
    assert_eq!(job.status, JobStatus::Queued);
    job
}

#[test]
fn validates_secrets_without_printing_them() {
    for token in [
        "short".into(),
        "a".repeat(257),
        format!("{}\n", "a".repeat(32)),
        "é".repeat(32),
    ] {
        assert!(matches!(
            ServerConfig::new(token),
            Err(ConfigError::InvalidToken)
        ));
    }
    assert!(same_token(TOKEN.as_bytes(), TOKEN.as_bytes()));
    assert!(!same_token(TOKEN.as_bytes(), b"bad"));
    assert!(!same_token(
        TOKEN.as_bytes(),
        "f".repeat(TOKEN.len()).as_bytes()
    ));
    assert_eq!(bounded_error(&"🦀".repeat(3000)).chars().count(), 2048);
}

#[tokio::test]
async fn rejects_invalid_capacity() {
    for max_jobs in [0, 4097] {
        let engine = idle_engine().await;
        let mut config = ServerConfig::new(TOKEN.into()).unwrap();
        config.max_jobs = max_jobs;
        assert!(matches!(
            HttpServer::new(engine, config),
            Err(ConfigError::InvalidCapacity)
        ));
    }
}

#[tokio::test]
async fn requires_auth_before_parsing_jobs_and_protects_results_and_stats() {
    let server = server(2).await;
    for (method, path) in [
        ("POST", "/v1/jobs"),
        ("GET", "/v1/jobs/invalid"),
        ("DELETE", "/v1/jobs/invalid"),
        ("GET", "/v1/stats"),
    ] {
        let response = server
            .router()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .body(Body::from("not JSON"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(response.headers()[header::WWW_AUTHENTICATE], "Bearer");
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        assert_eq!(
            response.headers()[header::X_CONTENT_TYPE_OPTIONS],
            "nosniff"
        );
        assert!(!response
            .headers()
            .contains_key(header::ACCESS_CONTROL_ALLOW_ORIGIN));
        assert_eq!(body(response).await["error"]["code"], "unauthorized");
    }
    let response = server
        .router()
        .oneshot(
            Request::builder()
                .uri("/healthz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(body(response).await, json!({ "status": "ok" }));
    let response = server
        .router()
        .oneshot(
            Request::builder()
                .uri("/v1/stats")
                .header(header::AUTHORIZATION, "Bearer wrong-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        call(&server, "GET", "/v1/stats", Body::empty())
            .await
            .status(),
        StatusCode::OK
    );
    // The scheme is case-insensitive.
    for scheme in ["bearer", "BEARER"] {
        let response = server
            .router()
            .oneshot(
                Request::builder()
                    .uri("/v1/stats")
                    .header(header::AUTHORIZATION, format!("{scheme} {TOKEN}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{scheme}");
    }
    server.shutdown(Duration::ZERO).await.unwrap();
}

#[tokio::test]
async fn rejects_invalid_and_oversized_requests_before_scheduling() {
    let server = server(2).await;
    for (payload, status) in [
        (r#"{"steps":[]}"#, StatusCode::BAD_REQUEST),
        (
            r#"{"steps":[{"action":"goto","url":"file:///secret"}]}"#,
            StatusCode::BAD_REQUEST,
        ),
        (
            r#"{"steps":[{"action":"title"}],"unknown":true}"#,
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        ("not JSON", StatusCode::BAD_REQUEST),
    ] {
        let response = call(&server, "POST", "/v1/jobs", payload).await;
        assert_eq!(response.status(), status);
        assert_eq!(body(response).await["error"]["code"], "invalid_request");
    }
    let response = call(
        &server,
        "POST",
        "/v1/jobs",
        " ".repeat(MAX_REQUEST_BYTES + 1),
    )
    .await;
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert!(server.inner.registry.lock().unwrap().jobs.is_empty());
    assert_eq!(
        call(&server, "GET", "/v1/jobs/not-a-uuid", Body::empty())
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        call(
            &server,
            "GET",
            &format!("/v1/jobs/{}", Uuid::new_v4()),
            Body::empty()
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
    server.shutdown(Duration::ZERO).await.unwrap();
}

#[tokio::test]
async fn bounds_admission_and_cancels_without_overwriting_status() {
    let server = server(1).await;
    let job = submit_job(&server).await;
    let path = format!("/v1/jobs/{}", job.id);
    let response = call(
        &server,
        "POST",
        "/v1/jobs",
        r#"{"steps":[{"action":"title"}]}"#,
    )
    .await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(response.headers()[header::RETRY_AFTER], "1");
    assert_eq!(body(response).await["error"]["code"], "capacity_exceeded");

    for _ in 0..2 {
        let response = call(&server, "DELETE", &path, Body::empty()).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body(response).await["status"], "cancelled");
    }
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if server
                .inner
                .registry
                .lock()
                .unwrap()
                .find_mut(job.id)
                .unwrap()
                .abort
                .is_none()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        body(call(&server, "GET", &path, Body::empty()).await).await["status"],
        "cancelled"
    );
    let next = submit_job(&server).await;
    assert_ne!(next.id, job.id);
    assert_eq!(
        call(&server, "GET", &path, Body::empty()).await.status(),
        StatusCode::NOT_FOUND
    );
    server.shutdown(Duration::ZERO).await.unwrap();
    assert_eq!(
        call(
            &server,
            "POST",
            "/v1/jobs",
            r#"{"steps":[{"action":"title"}]}"#
        )
        .await
        .status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
}

#[tokio::test]
async fn queue_time_counts_towards_the_total_deadline() {
    let server = server(1).await;
    let response = call(
        &server,
        "POST",
        "/v1/jobs",
        r#"{"steps":[{"action":"title"}],"timeout_ms":10,"retries":3}"#,
    )
    .await;
    let submitted: Job = serde_json::from_value(body(response).await).unwrap();
    let job = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let response = call(
                &server,
                "GET",
                &format!("/v1/jobs/{}", submitted.id),
                Body::empty(),
            )
            .await;
            let job: Job = serde_json::from_value(body(response).await).unwrap();
            if job.status.terminal() {
                break job;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(job.status, JobStatus::Failed);
    assert_eq!(job.attempts, 0);
    assert!(job.started_at.is_none());
    assert!(job.finished_at.is_some());
    assert!(job.error.unwrap().contains("timed out"));
    assert_eq!(
        call(
            &server,
            "DELETE",
            &format!("/v1/jobs/{}", job.id),
            Body::empty()
        )
        .await
        .status(),
        StatusCode::CONFLICT
    );
    server.shutdown(Duration::ZERO).await.unwrap();
}

#[tokio::test]
async fn discarding_the_server_cancels_its_workers() {
    let server = server(1).await;
    let job = submit_job(&server).await;
    let weak = Arc::downgrade(&server.inner);
    let abort = server
        .inner
        .registry
        .lock()
        .unwrap()
        .find_mut(job.id)
        .unwrap()
        .abort
        .clone()
        .unwrap();
    drop(server);
    assert!(weak.upgrade().is_none());
    tokio::time::timeout(Duration::from_secs(2), async {
        while !abort.is_finished() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn serves_prometheus_metrics_with_auth() {
    let server = server(4).await;
    // Without token: 401
    let response = server
        .router()
        .oneshot(
            Request::builder()
                .uri("/metrics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    // With bearer token: 200 OK
    let response = call(&server, "GET", "/metrics", Body::empty()).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_TYPE],
        "text/plain; version=0.0.4; charset=utf-8"
    );
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    assert!(text.contains("# HELP fluxwright_browsers"));
    assert!(text.contains("fluxwright_browsers 0"));
    assert!(text.contains("# HELP fluxwright_http_jobs_total"));
    assert!(text.contains("fluxwright_http_jobs_capacity 4"));
    server.shutdown(Duration::ZERO).await.unwrap();
}

#[tokio::test]
async fn extracts_w3c_traceparent_and_populates_job() {
    let server = server(2).await;
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
    let outbound_tp = response.headers().get("traceparent").unwrap().to_str().unwrap();
    assert!(outbound_tp.starts_with("00-4bf92f3577b34da6a3ce929d0e0e4736-"));

    let job: Job = serde_json::from_value(body(response).await).unwrap();
    assert_eq!(job.trace_id.as_deref(), Some("4bf92f3577b34da6a3ce929d0e0e4736"));

    // When no traceparent is supplied, a 32-hex trace ID is automatically generated
    let auto_job = submit_job(&server).await;
    assert!(auto_job.trace_id.is_some());
    assert_eq!(auto_job.trace_id.as_ref().unwrap().len(), 32);

    server.shutdown(Duration::ZERO).await.unwrap();
}

#[tokio::test]
async fn serves_dashboard_html_and_sse_events() {
    let server = server(2).await;

    // Dashboard HTML is available at /dashboard and / without credentials
    for path in ["/dashboard", "/"] {
        let response = server
            .router()
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        let html = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(html.contains("Fluxwright Fleet Dashboard"));
        assert!(html.contains("Skip to main content"));
        assert!(html.contains("Recent HTTP Jobs"));
    }

    // /v1/events requires authentication
    let unauth = server
        .router()
        .oneshot(
            Request::builder()
                .uri("/v1/events")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unauth.status(), StatusCode::UNAUTHORIZED);

    // /v1/events with the bearer token returns an SSE stream
    let sse_resp = call(&server, "GET", "/v1/events", Body::empty()).await;
    assert_eq!(sse_resp.status(), StatusCode::OK);
    assert_eq!(
        sse_resp.headers()[header::CONTENT_TYPE],
        "text/event-stream"
    );

    // Shutting down ends the stream: a graceful HTTP shutdown waits for every open response,
    // so an endless one kept `fluxwright serve` from ever exiting.
    server.shutdown(Duration::ZERO).await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), to_bytes(sse_resp.into_body(), 1024 * 1024))
        .await
        .expect("the event stream did not end after shutdown")
        .unwrap();
}

#[tokio::test]
async fn job_serializes_and_deserializes_failure_evidence_and_aliases() {
    let mut evidence = FailureEvidence::new();
    evidence.console.push(ConsoleEntry::new("log", "page initialised"));
    evidence.console.push(ConsoleEntry::new("error", "uncaught reference error"));
    evidence.network.push(NetworkEntry {
        url: "https://example.com/api/data".into(),
        method: "GET".into(),
        status: Some(200),
        resource_type: Some("fetch".into()),
        failure: None,
    });
    evidence.network.push(NetworkEntry {
        url: "https://example.com/missing".into(),
        method: "POST".into(),
        status: Some(404),
        resource_type: Some("xhr".into()),
        failure: Some("not found".into()),
    });
    evidence.screenshot = Some("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=".into());

    let job = Job {
        id: Uuid::new_v4(),
        status: JobStatus::Failed,
        submitted_at: 1000,
        started_at: Some(1005),
        finished_at: Some(1050),
        attempts: 1,
        step_index: Some(2),
        results: None,
        error: Some("step failed".into()),
        trace_id: Some("0123456789abcdef0123456789abcdef".into()),
        failure_evidence: Some(evidence.clone()),
    };

    assert_eq!(job.evidence(), Some(&evidence));

    // Serialize to JSON and verify structure
    let val = serde_json::to_value(&job).unwrap();
    assert_eq!(val["status"], "failed");
    assert_eq!(val["failure_evidence"]["console"].as_array().unwrap().len(), 2);
    assert_eq!(val["failure_evidence"]["console"][0]["kind"], "log");
    assert_eq!(val["failure_evidence"]["console"][0]["text"], "page initialised");
    assert_eq!(val["failure_evidence"]["network"].as_array().unwrap().len(), 2);
    assert_eq!(val["failure_evidence"]["network"][0]["status"], 200);
    assert_eq!(val["failure_evidence"]["network"][1]["failure"], "not found");
    assert!(val["failure_evidence"]["screenshot"].as_str().unwrap().starts_with("iVBOR"));

    // Deserialize into Job using canonical fields
    let deserialized: Job = serde_json::from_value(val).unwrap();
    assert_eq!(deserialized.id, job.id);
    assert_eq!(deserialized.failure_evidence, Some(evidence.clone()));
    assert_eq!(deserialized.evidence().unwrap().console.len(), 2);
    assert_eq!(deserialized.evidence().unwrap().network.len(), 2);

    let canonical_json = serde_json::json!({
        "id": job.id,
        "status": "failed",
        "submitted_at": 1000,
        "attempts": 1,
        "failure_evidence": {
            "console": [
                { "kind": "warn", "text": "canonical warning" }
            ],
            "network": [
                { "url": "https://example.com/test", "method": "GET" }
            ],
            "screenshot": "iVBORcanonical"
        }
    });
    let canonical_job: Job = serde_json::from_value(canonical_json).unwrap();
    let ev = canonical_job.evidence().unwrap();
    assert_eq!(ev.console[0].kind, "warn");
    assert_eq!(ev.console[0].text, "canonical warning");
    assert_eq!(ev.network[0].url, "https://example.com/test");
    assert_eq!(ev.screenshot.as_deref(), Some("iVBORcanonical"));
}

#[test]
fn failure_evidence_enforces_bounds_and_limits_size() {
    let mut evidence = FailureEvidence::new();
    // Oversized console text (multi-byte UTF-8) and excess entries
    evidence
        .console
        .push(ConsoleEntry::new("log", "🦀".repeat(MAX_CONSOLE_TEXT_BYTES)));
    for i in 0..(MAX_CONSOLE_ENTRIES + 50) {
        evidence.console.push(ConsoleEntry::new(
            "log",
            format!("entry {i} {}", "x".repeat(MAX_CONSOLE_TEXT_BYTES + 500)),
        ));
    }
    // Oversized URLs and excess entries
    for i in 0..(MAX_NETWORK_ENTRIES + 50) {
        evidence.network.push(NetworkEntry {
            url: format!("https://example.com/{i}/{}", "🦀".repeat(MAX_NETWORK_URL_BYTES)),
            method: "GET".into(),
            status: Some(200),
            resource_type: Some("fetch".into()),
            failure: Some("f".repeat(1000)),
        });
    }
    // Excessively large screenshot exceeding MAX_RESULT_BYTES
    evidence.screenshot = Some("s".repeat(MAX_RESULT_BYTES + 1000));

    evidence.enforce_bounds();

    assert!(evidence.console.len() <= MAX_CONSOLE_ENTRIES);
    assert!(evidence.network.len() <= MAX_NETWORK_ENTRIES);
    for entry in &evidence.console {
        assert!(entry.text.len() <= MAX_CONSOLE_TEXT_BYTES);
    }
    for entry in &evidence.network {
        assert!(entry.url.len() <= MAX_NETWORK_URL_BYTES);
        if let Some(f) = &entry.failure {
            assert!(f.len() <= 512);
        }
    }
    // Huge screenshot is dropped to protect memory bounds
    assert!(evidence.screenshot.is_none());

    let bytes = serde_json::to_vec(&evidence).unwrap();
    assert!(bytes.len() <= MAX_RESULT_BYTES);
}

#[test]
fn empty_failure_evidence_is_omitted_from_json() {
    let job = Job {
        id: Uuid::new_v4(),
        status: JobStatus::Queued,
        submitted_at: 1000,
        started_at: None,
        finished_at: None,
        attempts: 0,
        step_index: None,
        results: None,
        error: None,
        trace_id: None,
        failure_evidence: None,
    };
    let json = serde_json::to_value(&job).unwrap();
    assert!(json.get("failure_evidence").is_none());
    assert!(json.get("evidence").is_none());

    let empty_evidence = FailureEvidence::new();
    assert!(empty_evidence.is_empty());
    let _empty_job = Job {
        failure_evidence: Some(empty_evidence),
        ..job
    };
    let empty_val = serde_json::to_value(&_empty_job).unwrap();
    assert!(empty_val.get("failure_evidence").is_none());
    assert!(empty_val.get("evidence").is_none());
}

/// The token never travels in a URL, where proxy logs, history and Referer headers keep it.
#[tokio::test]
async fn query_tokens_are_refused_everywhere() {
    let srv = server(2).await;
    for (method, path) in [("GET", "/v1/events"), ("GET", "/metrics"), ("GET", "/v1/stats"), ("POST", "/v1/jobs")] {
        let response = srv
            .router()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(format!("{path}?token={TOKEN}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"steps":[{"action":"title"}]}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{method} {path}");
    }
    srv.shutdown(Duration::ZERO).await.unwrap();
}

fn counts(server: &HttpServer) -> (u64, u64, u64) {
    let reg = server.inner.registry.lock().unwrap();
    (reg.succeeded_jobs, reg.failed_jobs, reg.cancelled_jobs)
}

#[tokio::test]
async fn a_worker_panic_is_recorded_as_a_failure() {
    let server = server(2).await;
    let job = submit_job(&server).await;
    let panicked = tokio::spawn(async { panic!("boom") }).await.unwrap_err();
    finish_job(&server.inner, job.id, Err(panicked));
    let job: Job = serde_json::from_value(
        body(
            call(
                &server,
                "GET",
                &format!("/v1/jobs/{}", job.id),
                Body::empty(),
            )
            .await,
        )
        .await,
    )
    .unwrap();
    assert_eq!(job.status, JobStatus::Failed);
    assert_eq!(job.error.as_deref(), Some("job worker panicked"));
    assert!(job.finished_at.is_some());
    assert_eq!(counts(&server), (0, 1, 0));
    server.shutdown(Duration::ZERO).await.unwrap();
}

/// A job cancelled as its result arrives is counted once, as cancelled.
#[tokio::test]
async fn cancel_and_completion_count_each_job_once() {
    let server = server(2).await;
    let job = submit_job(&server).await;
    let path = format!("/v1/jobs/{}", job.id);
    for _ in 0..2 {
        assert_eq!(
            call(&server, "DELETE", &path, Body::empty()).await.status(),
            StatusCode::OK
        );
    }
    finish_job(&server.inner, job.id, Ok(Ok(vec![json!("late")])));
    assert_eq!(
        body(call(&server, "GET", &path, Body::empty()).await).await["status"],
        "cancelled"
    );
    assert_eq!(counts(&server), (0, 0, 1));

    // And a finished job is counted once, however often its outcome is reported.
    let done = submit_job(&server).await;
    finish_job(&server.inner, done.id, Ok(Ok(vec![])));
    finish_job(&server.inner, done.id, Ok(Ok(vec![])));
    assert_eq!(
        call(
            &server,
            "DELETE",
            &format!("/v1/jobs/{}", done.id),
            Body::empty()
        )
        .await
        .status(),
        StatusCode::CONFLICT
    );
    assert_eq!(counts(&server), (1, 0, 1));
    server.shutdown(Duration::ZERO).await.unwrap();
}

/// Room is made from the oldest finished jobs only; running and queued ones stay, wherever
/// they sit in the list.
#[test]
fn eviction_keeps_active_jobs() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();
    let record = |status| {
        let active = !matches!(
            status,
            JobStatus::Succeeded | JobStatus::Failed | JobStatus::Cancelled
        );
        Record {
            job: Job {
                id: Uuid::new_v4(),
                status,
                submitted_at: 0,
                started_at: None,
                finished_at: None,
                attempts: 0,
                step_index: None,
                results: None,
                error: None,
                trace_id: None,
                failure_evidence: None,
            },
            request: JobRequest {
                steps: vec![Step::Title {}],
                timeout_ms: 60_000,
                retries: 0,
                priority: JobPriority::Normal,
            },
            abort: active.then(|| tokio::spawn(std::future::pending::<()>()).abort_handle()),
            worker: None,
            lease_until: None,
        }
    };
    let mut registry = Registry {
        accepting: true,
        jobs: VecDeque::from([
            record(JobStatus::Running),
            record(JobStatus::Succeeded),
            record(JobStatus::Queued),
            record(JobStatus::Failed),
            record(JobStatus::Running),
        ]),
        succeeded_jobs: 0,
        failed_jobs: 0,
        cancelled_jobs: 0,
    };
    assert!(registry.make_room(5));
    let left: Vec<JobStatus> = registry.jobs.iter().map(|r| r.job.status).collect();
    assert_eq!(
        left,
        [
            JobStatus::Running,
            JobStatus::Queued,
            JobStatus::Failed,
            JobStatus::Running
        ]
    );
    assert!(registry.make_room(4));
    let left: Vec<JobStatus> = registry.jobs.iter().map(|r| r.job.status).collect();
    assert_eq!(
        left,
        [JobStatus::Running, JobStatus::Queued, JobStatus::Running]
    );
    // Only active jobs left: no room, and none of them evicted.
    assert!(!registry.make_room(3));
    assert_eq!(registry.jobs.len(), 3);
}

/// Jobs still open when the server stops are queued again, under their ids, by the next one. A
/// job the client cancelled is over and stays over.
#[tokio::test]
async fn unfinished_jobs_run_again_after_a_restart() {
    let path = std::env::temp_dir().join(format!("fluxwright-server-journal-{}.jsonl", Uuid::new_v4()));
    let start = || async {
        let mut config = ServerConfig::new(TOKEN.into()).unwrap();
        config.max_jobs = 4;
        config.journal = Some(path.clone());
        HttpServer::new(idle_engine().await, config).unwrap()
    };
    let server = start().await;
    let kept = submit_job(&server).await;
    let cancelled = submit_job(&server).await;
    let response = call(&server, "DELETE", &format!("/v1/jobs/{}", cancelled.id), Body::empty()).await;
    assert_eq!(response.status(), StatusCode::OK);
    server.shutdown(Duration::ZERO).await.unwrap();
    drop(server);

    let server = start().await;
    let ids: Vec<Uuid> = server.inner.registry.lock().unwrap().jobs.iter().map(|r| r.job.id).collect();
    assert_eq!(ids, [kept.id]);
    let response = call(&server, "GET", &format!("/v1/jobs/{}", kept.id), Body::empty()).await;
    assert_eq!(body(response).await["status"], "queued");
    server.shutdown(Duration::ZERO).await.unwrap();
    drop(server);
    let _ = std::fs::remove_file(&path);
}

async fn submit_priority(server: &HttpServer, priority: &str, retries: u32) -> Job {
    let payload = serde_json::json!({
        "priority": priority,
        "retries": retries,
        "steps": [{ "action": "title" }],
    });
    let response = call(server, "POST", "/v1/jobs", payload.to_string()).await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    serde_json::from_value(body(response).await).unwrap()
}

#[tokio::test]
async fn coordinator_leases_by_priority_and_age_heartbeats_and_cancels() {
    let mut config = ServerConfig::new(TOKEN.into()).unwrap();
    config.max_jobs = 8;
    let server = HttpServer::coordinator(config).unwrap();

    assert_eq!(call(&server, "GET", "/v1/stats", Body::empty()).await.status(), StatusCode::OK);
    assert_eq!(call(&server, "GET", "/metrics", Body::empty()).await.status(), StatusCode::OK);

    let low = submit_priority(&server, "low", 0).await;
    let norm = submit_priority(&server, "normal", 0).await;
    let high1 = submit_priority(&server, "high", 0).await;
    let high2 = submit_priority(&server, "high", 0).await;

    let res = call(&server, "POST", "/v1/workers/worker-1/lease", r#"{"max_jobs":2}"#).await;
    assert_eq!(res.status(), StatusCode::OK);
    let b1 = body(res).await;
    let jobs1 = b1["jobs"].as_array().unwrap();
    assert_eq!(jobs1.len(), 2);
    assert_eq!(jobs1[0]["id"], high1.id.to_string());
    assert_eq!(jobs1[1]["id"], high2.id.to_string());
    assert_eq!(jobs1[0]["attempts"], 1);
    assert_eq!(jobs1[0]["trace_id"], high1.trace_id.as_deref().unwrap());

    let res = call(&server, "POST", "/v1/workers/worker-2/lease", r#"{"max_jobs":5}"#).await;
    assert_eq!(res.status(), StatusCode::OK);
    let b2 = body(res).await;
    let jobs2 = b2["jobs"].as_array().unwrap();
    assert_eq!(jobs2.len(), 2);
    assert_eq!(jobs2[0]["id"], norm.id.to_string());
    assert_eq!(jobs2[1]["id"], low.id.to_string());

    let empty = body(call(&server, "POST", "/v1/workers/worker-2/lease", r#"{"max_jobs":5}"#).await).await;
    assert!(empty["jobs"].as_array().unwrap().is_empty());

    let hb1 = serde_json::json!({
        "jobs": [{ "id": high1.id, "step_index": 2, "attempts": 1 }]
    });
    let hb_res = call(&server, "POST", "/v1/workers/worker-1/heartbeat", hb1.to_string()).await;
    assert_eq!(hb_res.status(), StatusCode::OK);
    assert_eq!(body(hb_res).await["cancelled"], serde_json::json!([]));

    let status = body(call(&server, "GET", &format!("/v1/jobs/{}", high1.id), Body::empty()).await).await;
    assert_eq!(status["status"], "running");
    assert_eq!(status["step_index"], 2);

    assert_eq!(
        call(&server, "DELETE", &format!("/v1/jobs/{}", high1.id), Body::empty())
            .await
            .status(),
        StatusCode::OK
    );

    let hb2 = serde_json::json!({
        "jobs": [
            { "id": high1.id, "step_index": 2, "attempts": 1 },
            { "id": high2.id, "step_index": 0, "attempts": 1 }
        ]
    });
    let hb2_body = body(call(&server, "POST", "/v1/workers/worker-1/heartbeat", hb2.to_string()).await).await;
    assert_eq!(hb2_body["cancelled"], serde_json::json!([high1.id]));

    let wrong = serde_json::json!({
        "worker": "worker-1",
        "status": "succeeded",
        "attempts": 1,
        "step_index": 0,
        "results": ["Example"]
    });
    assert_eq!(
        call(&server, "POST", &format!("/v1/jobs/{}/result", norm.id), wrong.to_string())
            .await
            .status(),
        StatusCode::CONFLICT
    );

    let ok = serde_json::json!({
        "worker": "worker-2",
        "status": "succeeded",
        "attempts": 1,
        "step_index": 0,
        "results": ["Example"]
    });
    let res = call(&server, "POST", &format!("/v1/jobs/{}/result", low.id), ok.to_string()).await;
    assert_eq!(res.status(), StatusCode::OK);
    let done = body(res).await;
    assert_eq!(done["status"], "succeeded");
    assert_eq!(done["results"], serde_json::json!(["Example"]));

    server.shutdown(Duration::ZERO).await.unwrap();
}

#[tokio::test]
async fn coordinator_expires_leases_requeues_retries_and_rejects_stale_worker_with_409() {
    let mut config = ServerConfig::new(TOKEN.into()).unwrap();
    config.max_jobs = 8;
    let server = HttpServer::coordinator(config).unwrap();
    server.set_lease_timeout(Duration::from_millis(60));

    let retryable = submit_priority(&server, "normal", 1).await;
    let once = submit_priority(&server, "normal", 0).await;

    let res = call(&server, "POST", "/v1/workers/worker-dead/lease", r#"{"max_jobs":2}"#).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(body(res).await["jobs"].as_array().unwrap().len(), 2);

    tokio::time::sleep(Duration::from_millis(140)).await;

    let once_state = body(call(&server, "GET", &format!("/v1/jobs/{}", once.id), Body::empty()).await).await;
    assert_eq!(once_state["status"], "failed");
    assert_eq!(once_state["error"], "worker lost");

    let retry_state = body(call(&server, "GET", &format!("/v1/jobs/{}", retryable.id), Body::empty()).await).await;
    assert_eq!(retry_state["status"], "queued");

    let stale = serde_json::json!({
        "worker": "worker-dead",
        "status": "succeeded",
        "attempts": 1,
        "step_index": 0,
        "results": ["late"]
    });
    assert_eq!(
        call(&server, "POST", &format!("/v1/jobs/{}/result", retryable.id), stale.to_string())
            .await
            .status(),
        StatusCode::CONFLICT
    );

    let res2 = body(call(&server, "POST", "/v1/workers/worker-alive/lease", r#"{"max_jobs":2}"#).await).await;
    let jobs2 = res2["jobs"].as_array().unwrap();
    assert_eq!(jobs2.len(), 1);
    assert_eq!(jobs2[0]["id"], retryable.id.to_string());
    assert_eq!(jobs2[0]["attempts"], 2);

    assert_eq!(
        call(&server, "POST", &format!("/v1/jobs/{}/result", retryable.id), stale.to_string())
            .await
            .status(),
        StatusCode::CONFLICT
    );

    let failed = serde_json::json!({
        "worker": "worker-alive",
        "status": "failed",
        "attempts": 2,
        "step_index": 0,
        "error": "boom",
        "failure_evidence": {
            "console": [{ "kind": "error", "text": "boom" }],
            "network": []
        }
    });
    let res3 = call(&server, "POST", &format!("/v1/jobs/{}/result", retryable.id), failed.to_string()).await;
    assert_eq!(res3.status(), StatusCode::OK);
    let done = body(res3).await;
    assert_eq!(done["status"], "failed");
    assert_eq!(done["attempts"], 2);
    assert_eq!(done["failure_evidence"]["console"][0]["text"], "boom");

    assert!(matches!(
        cluster::CoordinatorEndpoint::parse("https://coordinator:7331"),
        Err(ConfigError::InvalidCoordinator(_))
    ));
    assert!(matches!(
        cluster::CoordinatorEndpoint::parse("http://example.com?bad"),
        Err(ConfigError::InvalidCoordinator(_))
    ));
    assert!(cluster::CoordinatorEndpoint::parse("http://[::1]:7331/sub/").is_ok());
    assert!(cluster::CoordinatorEndpoint::parse("http://127.0.0.1:7331").unwrap().is_loopback());
    assert!(cluster::CoordinatorEndpoint::parse("http://[::1]:7331").unwrap().is_loopback());
    assert!(!cluster::CoordinatorEndpoint::parse("http://10.0.1.10:7331").unwrap().is_loopback());

    server.shutdown(Duration::ZERO).await.unwrap();
}

#[tokio::test]
async fn standalone_server_does_not_expose_worker_cluster_routes() {
    let server = server(4).await;
    let job = submit_job(&server).await;

    for (path, payload) in [
        ("/v1/workers/w1/lease", r#"{"max_jobs":1}"#),
        ("/v1/workers/w1/heartbeat", r#"{"jobs":[]}"#),
        ("/v1/workers/w1/release", r#"{"jobs":[]}"#),
    ] {
        assert_eq!(
            call(&server, "POST", path, payload).await.status(),
            StatusCode::NOT_FOUND
        );
    }
    assert_eq!(
        call(
            &server,
            "POST",
            &format!("/v1/jobs/{}/result", job.id),
            r#"{"worker":"w1","status":"succeeded","attempts":1,"step_index":0,"results":[]}"#,
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );

    server.shutdown(Duration::ZERO).await.unwrap();
}

#[tokio::test]
async fn coordinator_lease_caps_batch_bytes_without_burning_remaining_jobs() {
    let mut config = ServerConfig::new(TOKEN.into()).unwrap();
    config.max_jobs = 64;
    let server = HttpServer::coordinator(config).unwrap();

    // 45 jobs at ~55 KiB each (~2.47 MiB total) exceed MAX_LEASE_RESPONSE_BYTES (2 MiB).
    let pad = "a".repeat(55_000);
    let expr = format!("/*{pad}*/'ok'");
    let mut submitted = Vec::with_capacity(45);
    for _ in 0..45 {
        let payload = serde_json::json!({
            "steps": [{ "action": "evaluate", "expression": expr }]
        });
        let res = call(&server, "POST", "/v1/jobs", payload.to_string()).await;
        assert_eq!(res.status(), StatusCode::ACCEPTED);
        submitted.push(serde_json::from_value::<Job>(body(res).await).unwrap().id);
    }

    let res1 = call(&server, "POST", "/v1/workers/w1/lease", r#"{"max_jobs":64}"#).await;
    assert_eq!(res1.status(), StatusCode::OK);
    let b1 = body(res1).await;
    let jobs1 = b1["jobs"].as_array().unwrap();
    assert!(
        jobs1.len() > 1 && jobs1.len() < 45,
        "expected first lease batch to split cleanly at byte limit, got {}",
        jobs1.len()
    );

    // Remaining jobs stayed Queued (never marked Running or burned).
    let next_id = submitted[jobs1.len()];
    let state = body(call(&server, "GET", &format!("/v1/jobs/{next_id}"), Body::empty()).await).await;
    assert_eq!(state["status"], "queued");
    assert_eq!(state["attempts"], 0);

    let res2 = call(&server, "POST", "/v1/workers/w2/lease", r#"{"max_jobs":64}"#).await;
    assert_eq!(res2.status(), StatusCode::OK);
    let jobs2 = body(res2).await["jobs"].as_array().unwrap().len();
    assert_eq!(jobs1.len() + jobs2, 45);

    server.shutdown(Duration::ZERO).await.unwrap();
}

#[tokio::test]
async fn worker_heartbeats_independently_and_stops_cleanly_when_cancelled() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let heartbeats = Arc::new(AtomicUsize::new(0));
    let hb_count = heartbeats.clone();
    let job_id = Uuid::new_v4();

    let server_task = tokio::spawn(async move {
        let mut leased = false;
        while let Ok((mut stream, _)) = listener.accept().await {
            let mut buf = vec![0u8; 8192];
            let n = stream.read(&mut buf).await.unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]);
            let resp_body = if req.contains("/lease") && !leased {
                leased = true;
                serde_json::json!({
                    "jobs": [{
                        "id": job_id,
                        "request": { "steps": [{ "action": "title" }] },
                        "attempts": 1,
                        "trace_id": null
                    }]
                })
                .to_string()
            } else if req.contains("/lease") {
                r#"{"jobs":[]}"#.to_string()
            } else if req.contains("/heartbeat") {
                let c = hb_count.fetch_add(1, Ordering::SeqCst) + 1;
                if c >= 2 {
                    serde_json::json!({ "cancelled": [job_id] }).to_string()
                } else {
                    r#"{"cancelled":[]}"#.to_string()
                }
            } else {
                r#"{"error":"unexpected"}"#.to_string()
            };
            let reply = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                resp_body.len(),
                resp_body
            );
            let _ = stream.write_all(reply.as_bytes()).await;
        }
    });

    let engine = idle_engine().await;
    let worker = cluster::spawn_worker_with(
        engine.clone(),
        format!("http://{addr}"),
        TOKEN.into(),
        Some("w-hb".into()),
        1,
        Duration::from_millis(20),
        Duration::from_millis(30),
    )
    .unwrap();

    tokio::time::timeout(Duration::from_secs(5), async {
        while heartbeats.load(Ordering::SeqCst) < 2 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("heartbeat task ran at least twice");

    // Once cancelled via heartbeat, shutdown drains and finishes promptly.
    worker.shutdown(Duration::from_secs(2)).await;
    server_task.abort();
    engine.shutdown(Duration::ZERO).await.unwrap();
}

#[tokio::test]
async fn coordinator_release_requeues_without_counting_attempt() {
    let mut config = ServerConfig::new(TOKEN.into()).unwrap();
    config.max_jobs = 8;
    let server = HttpServer::coordinator(config).unwrap();

    // Even with retries: 0, releasing returns the job cleanly with attempts == 0.
    let job_a = submit_priority(&server, "normal", 0).await;
    let job_b = submit_priority(&server, "normal", 0).await;

    let l1 = body(call(&server, "POST", "/v1/workers/w1/lease", r#"{"max_jobs":1}"#).await).await;
    assert_eq!(l1["jobs"][0]["id"], job_a.id.to_string());
    let l2 = body(call(&server, "POST", "/v1/workers/w2/lease", r#"{"max_jobs":1}"#).await).await;
    assert_eq!(l2["jobs"][0]["id"], job_b.id.to_string());

    // Passing another worker's job ID only releases w1's own job.
    let rel_payload = serde_json::json!({ "jobs": [job_a.id, job_b.id] });
    let rel = call(&server, "POST", "/v1/workers/w1/release", rel_payload.to_string()).await;
    assert_eq!(rel.status(), StatusCode::OK);
    assert_eq!(body(rel).await["released"], serde_json::json!([job_a.id]));

    let state_a = body(call(&server, "GET", &format!("/v1/jobs/{}", job_a.id), Body::empty()).await).await;
    assert_eq!(state_a["status"], "queued");
    assert_eq!(state_a["attempts"], 0);
    assert_eq!(state_a["started_at"], serde_json::Value::Null);

    // w2's job stays running under w2.
    let state_b = body(call(&server, "GET", &format!("/v1/jobs/{}", job_b.id), Body::empty()).await).await;
    assert_eq!(state_b["status"], "running");
    assert_eq!(state_b["attempts"], 1);

    server.shutdown(Duration::ZERO).await.unwrap();
}
