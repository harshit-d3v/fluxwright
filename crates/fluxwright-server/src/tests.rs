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
        assert!(html.contains("Cluster Workers"));
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
                resource_preset: None,
                block_resources: Vec::new(),
                block_url_patterns: Vec::new(),
                block_cookie_banners: false,
                profile: None,
                persist_profile: true,
                proxy_session: None,
                webhook_url: None,
                webhook_headers: HashMap::new(),
            },
            trace_ctx: JobTraceContext::new(),
            submitted_ns: 0,
            started_ns: None,
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
        workers: HashMap::new(),
        last_leased_per_host: HashMap::new(),
        succeeded_jobs: 0,
        failed_jobs: 0,
        cancelled_jobs: 0,
        expired_leases: 0,
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

    assert_eq!(
        cluster::CoordinatorEndpoint::parse("https://coordinator:7331")
            .unwrap()
            .host,
        "coordinator"
    );
    assert!(matches!(
        cluster::CoordinatorEndpoint::parse("ftp://coordinator:7331"),
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
        call(&server, "GET", "/v1/workers", Body::empty()).await.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(&server, "POST", "/v1/workers/w1/drain", Body::empty())
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(&server, "DELETE", "/v1/workers/w1", Body::empty())
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
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

#[tokio::test]
async fn rejects_invalid_navigation_and_wait_steps() {
    let server = server(2).await;
    for payload in [
        r#"{"steps":[{"action":"wait_for_url","url":"   "}]}"#,
        r#"{"steps":[{"action":"wait_for_function","expression":"  "}]}"#,
        r#"{"steps":[{"action":"wait_for_function","expression":"true","polling_ms":0}]}"#,
        r#"{"steps":[{"action":"wait_for_function","expression":"true","polling_ms":60001}]}"#,
        r#"{"steps":[{"action":"url","extra":true}]}"#,
    ] {
        let response = call(&server, "POST", "/v1/jobs", payload).await;
        assert!(
            matches!(response.status(), StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY),
            "expected 400/422 for {payload}, got {}",
            response.status()
        );
    }
    server.shutdown(Duration::ZERO).await.unwrap();
}

#[tokio::test]
async fn rejects_invalid_init_script_and_tag_steps() {
    let server = server(2).await;
    for payload in [
        r#"{"steps":[{"action":"add_init_script","script":"  "}]}"#,
        r#"{"steps":[{"action":"add_script_tag"}]}"#,
        r#"{"steps":[{"action":"add_script_tag","url":"https://example.com/a.js","content":"1"}]}"#,
        r#"{"steps":[{"action":"add_script_tag","url":"file:///etc/passwd"}]}"#,
        r#"{"steps":[{"action":"add_script_tag","url":"//169.254.169.254/a.js"}]}"#,
        r#"{"steps":[{"action":"add_script_tag","url":"javascript:alert(1)"}]}"#,
        r#"{"steps":[{"action":"add_style_tag","url":""}]}"#,
        r#"{"steps":[{"action":"add_style_tag","content":"a{}","path":"/tmp/a.css"}]}"#,
    ] {
        let response = call(&server, "POST", "/v1/jobs", payload).await;
        assert!(
            matches!(response.status(), StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY),
            "expected 400/422 for {payload}, got {}",
            response.status()
        );
    }
    server.shutdown(Duration::ZERO).await.unwrap();
}

#[tokio::test]
async fn validates_and_applies_resource_presets_and_block_rules() {
    let req: JobRequest = serde_json::from_str(
        r#"{
            "steps": [{ "action": "title" }],
            "resource_preset": "scraping",
            "block_resources": ["stylesheet"],
            "block_url_patterns": ["**/tracker.js"],
            "block_cookie_banners": true
        }"#,
    )
    .unwrap();
    assert!(req.validate().is_ok());
    let opts = req.options();
    assert!(opts.block_images && opts.block_fonts && opts.block_media);
    assert!(opts.block_stylesheets);
    assert!(!opts.block_scripts);
    assert!(opts.block_cookie_banners);
    assert_eq!(opts.block_url_patterns, vec!["**/tracker.js"]);

    let no_media: JobRequest = serde_json::from_str(
        r#"{"steps":[{"action":"title"}],"resource_preset":"no-media"}"#,
    )
    .unwrap();
    let nm_opts = no_media.options();
    assert!(nm_opts.block_images && nm_opts.block_media);
    assert!(!nm_opts.block_fonts && !nm_opts.block_stylesheets);

    let server = server(2).await;
    let ok = call(
        &server,
        "POST",
        "/v1/jobs",
        r#"{"steps":[{"action":"title"}],"resource_preset":"minimal","block_url_patterns":["**/*.png"]}"#,
    )
    .await;
    assert_eq!(ok.status(), StatusCode::ACCEPTED);

    let too_many_patterns = serde_json::json!({
        "steps": [{ "action": "title" }],
        "block_url_patterns": vec!["*.js"; 65],
    })
    .to_string();
    for bad in [
        r#"{"steps":[{"action":"title"}],"resource_preset":"unknown"}"#.to_string(),
        r#"{"steps":[{"action":"title"}],"block_resources":["unknown"]}"#.to_string(),
        r#"{"steps":[{"action":"title"}],"block_url_patterns":[""]}"#.to_string(),
        too_many_patterns,
    ] {
        let resp = call(&server, "POST", "/v1/jobs", bad.clone()).await;
        assert!(
            matches!(resp.status(), StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY),
            "expected 400/422 for {bad}, got {}",
            resp.status()
        );
    }
    server.shutdown(Duration::ZERO).await.unwrap();
}

#[tokio::test]
async fn coordinator_tracks_workers_supports_drain_and_delete_and_exposes_dashboard_and_metrics() {
    let mut config = ServerConfig::new(TOKEN.into()).unwrap();
    config.max_jobs = 8;
    let server = HttpServer::coordinator(config).unwrap();

    let job1 = submit_priority(&server, "normal", 0).await;
    let job2 = submit_priority(&server, "normal", 0).await;

    let lease_res = call(&server, "POST", "/v1/workers/w-alpha/lease", r#"{"max_jobs":1}"#).await;
    assert_eq!(lease_res.status(), StatusCode::OK);
    assert_eq!(body(lease_res).await["jobs"][0]["id"], job1.id.to_string());

    let workers = body(call(&server, "GET", "/v1/workers", Body::empty()).await).await;
    assert_eq!(workers["summary"]["total"], 1);
    assert_eq!(workers["summary"]["online"], 1);
    assert_eq!(workers["summary"]["capacity"], 1);
    assert_eq!(workers["summary"]["active_jobs"], 1);
    assert_eq!(workers["workers"][0]["worker"], "w-alpha");
    assert_eq!(workers["workers"][0]["status"], "online");
    assert_eq!(workers["workers"][0]["active_jobs"], 1);

    let snap = server.inner.dashboard_snapshot().await;
    assert_eq!(snap["mode"], "coordinator");
    assert_eq!(snap["workers"][0]["worker"], "w-alpha");
    let j1_snap = snap["jobs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|j| j["id"] == job1.id.to_string())
        .unwrap();
    assert_eq!(j1_snap["worker"], "w-alpha");

    // Draining worker keeps active leases allowed to finish, blocks removing while busy, and skips new leases.
    let drain_res = call(&server, "POST", "/v1/workers/w-alpha/drain", Body::empty()).await;
    assert_eq!(drain_res.status(), StatusCode::OK);
    let drained = body(drain_res).await;
    assert_eq!(drained["status"], "draining");
    assert_eq!(drained["draining"], true);

    assert_eq!(
        call(&server, "DELETE", "/v1/workers/w-alpha", Body::empty())
            .await
            .status(),
        StatusCode::CONFLICT
    );

    let next_lease = body(call(&server, "POST", "/v1/workers/w-alpha/lease", r#"{"max_jobs":2}"#).await).await;
    assert!(next_lease["jobs"].as_array().unwrap().is_empty());

    let ok = serde_json::json!({
        "worker": "w-alpha",
        "status": "succeeded",
        "attempts": 1,
        "step_index": 0,
        "results": ["done"]
    });
    assert_eq!(
        call(&server, "POST", &format!("/v1/jobs/{}/result", job1.id), ok.to_string())
            .await
            .status(),
        StatusCode::OK
    );

    let after = body(call(&server, "GET", "/v1/workers", Body::empty()).await).await;
    assert_eq!(after["workers"][0]["active_jobs"], 0);
    assert_eq!(after["workers"][0]["succeeded_jobs"], 1);
    assert_eq!(after["summary"]["draining"], 1);
    assert_eq!(after["summary"]["capacity"], 0);

    let metrics = server.inner.prometheus_text().await;
    assert!(metrics.contains("fluxwright_cluster_workers{status=\"draining\"} 1\n"));
    assert!(metrics.contains("fluxwright_cluster_lease_expirations_total 0\n"));

    assert_eq!(
        call(&server, "DELETE", "/v1/workers/w-alpha", Body::empty())
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        call(&server, "POST", "/v1/workers/missing/drain", Body::empty())
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(&server, "DELETE", "/v1/workers/missing", Body::empty())
            .await
            .status(),
        StatusCode::NOT_FOUND
    );

    // Remaining queued job is still available for another online worker.
    let w2 = body(call(&server, "POST", "/v1/workers/w-beta/lease", r#"{"max_jobs":1}"#).await).await;
    assert_eq!(w2["jobs"][0]["id"], job2.id.to_string());

    server.shutdown(Duration::ZERO).await.unwrap();
}

#[tokio::test]
async fn coordinator_background_reaper_expires_overdue_leases_proactively() {
    let mut config = ServerConfig::new(TOKEN.into()).unwrap();
    config.max_jobs = 8;
    let server = HttpServer::coordinator(config).unwrap();
    server.set_lease_timeout(Duration::from_millis(50));

    let job = submit_priority(&server, "normal", 0).await;
    let lease_res = call(&server, "POST", "/v1/workers/w-reap/lease", r#"{"max_jobs":1}"#).await;
    assert_eq!(lease_res.status(), StatusCode::OK);
    assert_eq!(body(lease_res).await["jobs"].as_array().unwrap().len(), 1);

    // Inspect registry mutex directly (bypassing `with_registry`) to verify background reaper acted.
    let reaped = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let done = {
                let reg = server.inner.registry.lock().unwrap();
                reg.expired_leases == 1
                    && reg.failed_jobs == 1
                    && reg.jobs.iter().any(|r| {
                        r.job.id == job.id && r.job.status == JobStatus::Failed
                    })
                    && reg
                        .workers
                        .get("w-reap")
                        .is_some_and(|w| w.expired_leases == 1)
            };
            if done {
                break true;
            }
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
    })
    .await
    .unwrap_or(false);
    assert!(reaped, "background reaper did not expire overdue lease proactively");

    server.shutdown(Duration::ZERO).await.unwrap();
}

#[tokio::test]
async fn live_endpoints_enforce_auth_validate_ids_and_serve_accessible_viewer() {
    let srv = server(2).await;
    let fake_id = Uuid::new_v4();

    // Missing Bearer token and ?token= query param are rejected on all live/pause endpoints.
    for (method, path) in [
        ("GET", "/v1/live"),
        ("POST", "/v1/live/job-1/ticket"),
        ("GET", "/v1/live/job-1/ws"),
        ("POST", &format!("/v1/jobs/{fake_id}/pause")),
        ("POST", &format!("/v1/jobs/{fake_id}/resume")),
    ] {
        let unauth = srv
            .router()
            .oneshot(Request::builder().method(method).uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(unauth.status(), StatusCode::UNAUTHORIZED, "{method} {path}");

        let query_tok = srv
            .router()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(format!("{path}?token={TOKEN}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(query_tok.status(), StatusCode::UNAUTHORIZED, "{method} {path}?token=");
    }

    // Authenticated validation & 404 checks.
    assert_eq!(body(call(&srv, "GET", "/v1/live", Body::empty()).await).await["sessions"], json!([]));
    assert_eq!(call(&srv, "POST", "/v1/live/missing-id/ticket", Body::empty()).await.status(), StatusCode::NOT_FOUND);
    assert_eq!(call(&srv, "POST", "/v1/jobs/not-a-uuid/pause", Body::empty()).await.status(), StatusCode::BAD_REQUEST);
    assert_eq!(call(&srv, "POST", &format!("/v1/jobs/{fake_id}/pause"), Body::empty()).await.status(), StatusCode::NOT_FOUND);
    assert_eq!(call(&srv, "POST", &format!("/v1/jobs/{fake_id}/resume"), Body::empty()).await.status(), StatusCode::NOT_FOUND);

    // Dashboard markup includes native modal live viewer and compliant scroll containment.
    let dash = srv
        .router()
        .oneshot(Request::builder().uri("/dashboard").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(dash.status(), StatusCode::OK);
    let html = String::from_utf8(to_bytes(dash.into_body(), 1024 * 1024).await.unwrap().to_vec()).unwrap();
    assert!(html.contains("Live Browser Sessions"));
    assert!(html.contains(r#"id="live-dialog""#));
    assert!(html.contains("overscroll-behavior: contain"));
    assert!(!html.contains(r#"role="dialog""#));
    assert!(!html.contains("aria-modal"));

    srv.shutdown(Duration::ZERO).await.unwrap();
}

#[tokio::test]
async fn profile_proxy_endpoints_enforce_auth_validation_and_adaptive_metric() {
    use fluxwright::{AdaptiveConfig, ProxyPool};

    let srv = server(2).await;
    for (method, path) in [
        ("GET", "/v1/profiles"),
        ("DELETE", "/v1/profiles/acct"),
        ("GET", "/v1/proxies"),
    ] {
        let unauth = srv
            .router()
            .oneshot(Request::builder().method(method).uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(unauth.status(), StatusCode::UNAUTHORIZED, "{method} {path}");

        let query_tok = srv
            .router()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(format!("{path}?token={TOKEN}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(query_tok.status(), StatusCode::UNAUTHORIZED, "{method} {path}?token=");
    }

    // Unconfigured profiles/proxies return 404; bad profile name returns 400.
    assert_eq!(call(&srv, "GET", "/v1/profiles", Body::empty()).await.status(), StatusCode::NOT_FOUND);
    assert_eq!(call(&srv, "DELETE", "/v1/profiles/acct", Body::empty()).await.status(), StatusCode::NOT_FOUND);
    assert_eq!(call(&srv, "GET", "/v1/proxies", Body::empty()).await.status(), StatusCode::NOT_FOUND);
    assert_eq!(call(&srv, "DELETE", "/v1/profiles/bad.name", Body::empty()).await.status(), StatusCode::BAD_REQUEST);
    srv.shutdown(Duration::ZERO).await.unwrap();

    let dir = std::env::temp_dir().join(format!("fluxwright-srv-prof-{}", Uuid::new_v4()));
    let proxy_pool = ProxyPool::from_urls(["http://u:secret@127.0.0.1:8080"]).unwrap();
    let engine = BrowserEngine::builder()
        .source(Arc::new(NeverStarts))
        .profiles_dir(&dir)
        .proxy_pool(proxy_pool)
        .adaptive_concurrency(AdaptiveConfig {
            min_concurrency: 1,
            max_concurrency: 4,
            initial_concurrency: Some(2),
            ..Default::default()
        })
        .build()
        .await
        .unwrap();
    let mut cfg = ServerConfig::new(TOKEN.into()).unwrap();
    cfg.max_jobs = 8;
    let configured = HttpServer::new(engine, cfg).unwrap();

    let list_prof = call(&configured, "GET", "/v1/profiles", Body::empty()).await;
    assert_eq!(list_prof.status(), StatusCode::OK);
    assert_eq!(body(list_prof).await["profiles"], json!([]));

    assert_eq!(
        call(&configured, "DELETE", "/v1/profiles/missing", Body::empty())
            .await
            .status(),
        StatusCode::NOT_FOUND
    );

    let list_prox = call(&configured, "GET", "/v1/proxies", Body::empty()).await;
    assert_eq!(list_prox.status(), StatusCode::OK);
    let prox_bytes = to_bytes(list_prox.into_body(), MAX_RESULT_BYTES).await.unwrap();
    let prox_text = String::from_utf8(prox_bytes.to_vec()).unwrap();
    assert!(!prox_text.contains("secret"));
    let prox_json: Value = serde_json::from_str(&prox_text).unwrap();
    assert_eq!(prox_json["proxies"][0]["server"], "http://127.0.0.1:8080");
    assert_eq!(prox_json["proxies"][0]["healthy"], true);

    let metrics_resp = call(&configured, "GET", "/metrics", Body::empty()).await;
    assert_eq!(metrics_resp.status(), StatusCode::OK);
    let metrics_text = String::from_utf8(
        to_bytes(metrics_resp.into_body(), MAX_RESULT_BYTES)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(
        metrics_text.contains("fluxwright_effective_concurrency_limit 2\n"),
        "{metrics_text}"
    );

    configured.shutdown(Duration::ZERO).await.unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn cluster_worker_token_isolates_worker_and_client_routes() {
    const WORKER_TOKEN: &str = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";

    let config = ServerConfig::new(TOKEN.into())
        .unwrap()
        .with_worker_token(WORKER_TOKEN.into())
        .unwrap();
    let coord = HttpServer::coordinator(config).unwrap();

    let call_with = |tok: &str, method: &str, path: &str, payload: &str| {
        let router = coord.router();
        let req = Request::builder()
            .method(method)
            .uri(path)
            .header(header::AUTHORIZATION, format!("Bearer {tok}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(payload.to_string()))
            .unwrap();
        async move { router.oneshot(req).await.unwrap() }
    };

    // Submit a job with client token -> 202.
    let sub = call_with(
        TOKEN,
        "POST",
        "/v1/jobs",
        r#"{"steps":[{"action":"title"}]}"#,
    )
    .await;
    assert_eq!(sub.status(), StatusCode::ACCEPTED);
    let job: Job = serde_json::from_value(body(sub).await).unwrap();

    // Worker token cannot hit client or operator endpoints -> 401.
    for (m, p, pl) in [
        ("GET", &format!("/v1/jobs/{}", job.id) as &str, ""),
        ("POST", "/v1/jobs", r#"{"steps":[{"action":"title"}]}"#),
        ("GET", "/v1/workers", ""),
    ] {
        assert_eq!(call_with(WORKER_TOKEN, m, p, pl).await.status(), StatusCode::UNAUTHORIZED, "{m} {p}");
    }

    // Client token cannot call worker pull/result routes when worker_token is configured -> 401.
    assert_eq!(
        call_with(TOKEN, "POST", "/v1/workers/w1/lease", r#"{"max_jobs":1}"#)
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );

    // Worker token leases, heartbeats, and reports result -> 200.
    let lease = call_with(
        WORKER_TOKEN,
        "POST",
        "/v1/workers/w1/lease",
        r#"{"max_jobs":1}"#,
    )
    .await;
    assert_eq!(lease.status(), StatusCode::OK);
    let leased = body(lease).await;
    assert_eq!(leased["jobs"].as_array().unwrap().len(), 1);

    let hb = call_with(
        WORKER_TOKEN,
        "POST",
        "/v1/workers/w1/heartbeat",
        &json!({ "jobs": [{ "id": job.id, "step_index": 0, "attempts": 1 }] }).to_string(),
    )
    .await;
    assert_eq!(hb.status(), StatusCode::OK);

    let res = call_with(
        WORKER_TOKEN,
        "POST",
        &format!("/v1/jobs/{}/result", job.id),
        &json!({
            "worker": "w1",
            "status": "succeeded",
            "attempts": 1,
            "step_index": 0,
            "results": ["Example"]
        })
        .to_string(),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);

    // Client token reads the completed job and operator worker summary -> 200.
    let status = call_with(TOKEN, "GET", &format!("/v1/jobs/{}", job.id), "").await;
    assert_eq!(status.status(), StatusCode::OK);
    assert_eq!(body(status).await["status"], "succeeded");
    assert_eq!(call_with(TOKEN, "GET", "/v1/workers", "").await.status(), StatusCode::OK);

    coord.shutdown(Duration::ZERO).await.unwrap();
}

#[tokio::test]
async fn coordinator_rejects_pause_for_human_and_preserves_worker_capacity() {
    let mut config = ServerConfig::new(TOKEN.into()).unwrap();
    config.max_jobs = 16;
    let coord = HttpServer::coordinator(config).unwrap();

    // Coordinator must reject pause_for_human upfront with HTTP 400.
    let bad = call(
        &coord,
        "POST",
        "/v1/jobs",
        r#"{"steps":[{"action":"pause_for_human","message":"wait"}]}"#,
    )
    .await;
    assert_eq!(bad.status(), StatusCode::BAD_REQUEST);
    let bad_body = body(bad).await;
    assert_eq!(bad_body["error"]["code"], "invalid_request");

    // Submit 2 jobs and lease them with capacity=4, then poll again with want=2, capacity=4.
    let _j1 = submit_priority(&coord, "normal", 0).await;
    let _j2 = submit_priority(&coord, "normal", 0).await;

    let l1 = call(
        &coord,
        "POST",
        "/v1/workers/w-cap/lease",
        r#"{"max_jobs":4,"capacity":4}"#,
    )
    .await;
    assert_eq!(l1.status(), StatusCode::OK);
    assert_eq!(body(l1).await["jobs"].as_array().unwrap().len(), 2);

    // Subsequent poll with smaller `max_jobs` (free slots) must keep reported capacity=4.
    let l2 = call(
        &coord,
        "POST",
        "/v1/workers/w-cap/lease",
        r#"{"max_jobs":2,"capacity":4}"#,
    )
    .await;
    assert_eq!(l2.status(), StatusCode::OK);

    // Legacy worker without `capacity` field also does not shrink its registered capacity.
    let _ = call(
        &coord,
        "POST",
        "/v1/workers/w-legacy/lease",
        r#"{"max_jobs":3}"#,
    )
    .await;
    let _ = call(
        &coord,
        "POST",
        "/v1/workers/w-legacy/lease",
        r#"{"max_jobs":1}"#,
    )
    .await;

    let workers = body(call(&coord, "GET", "/v1/workers", Body::empty()).await).await;
    let list = workers["workers"].as_array().unwrap();
    let w_cap = list.iter().find(|w| w["worker"] == "w-cap").unwrap();
    assert_eq!(w_cap["max_jobs"], 4);
    assert_eq!(w_cap["active_jobs"], 2);
    let w_legacy = list.iter().find(|w| w["worker"] == "w-legacy").unwrap();
    assert_eq!(w_legacy["max_jobs"], 3);
    assert_eq!(workers["summary"]["capacity"], 7);

    coord.shutdown(Duration::ZERO).await.unwrap();
}

#[test]
fn remaining_retries_accounts_for_prior_cluster_attempts() {
    assert_eq!(remaining_retries(2, 0), 2);
    assert_eq!(remaining_retries(2, 1), 2);
    assert_eq!(remaining_retries(2, 2), 1);
    assert_eq!(remaining_retries(2, 3), 0);
    assert_eq!(remaining_retries(1, 2), 0);
    assert_eq!(remaining_retries(0, 2), 0);
}

#[tokio::test]
async fn stats_and_dashboard_snapshot_expose_memory_ceiling() {
    let engine = BrowserEngine::builder()
        .source(Arc::new(NeverStarts))
        .memory_ceiling_mb(2048)
        .build()
        .await
        .unwrap();
    let srv = HttpServer::new(engine, ServerConfig::new(TOKEN.into()).unwrap()).unwrap();

    let stats_val = body(call(&srv, "GET", "/v1/stats", Body::empty()).await).await;
    assert_eq!(stats_val["memory_ceiling_mb"], 2048);
    assert_eq!(stats_val["memory_ceiling_bytes"], 2048u64 * 1024 * 1024);

    let snap = srv.inner.dashboard_snapshot().await;
    assert_eq!(snap["server"]["memory_ceiling_mb"], 2048);
    assert_eq!(snap["server"]["memory_ceiling_bytes"], 2048u64 * 1024 * 1024);

    srv.shutdown(Duration::ZERO).await.unwrap();
}

#[test]
fn job_target_host_normalizes_first_goto_url() {
    let req_with_url = |steps: Value| -> JobRequest {
        serde_json::from_value(json!({ "steps": steps })).unwrap()
    };

    assert_eq!(
        job_target_host(&req_with_url(json!([
            { "action": "add_init_script", "script": "window.x = 1" },
            { "action": "goto", "url": "https://User:Pass@Sub.Example.COM.:8443/path?a=1#frag" },
            { "action": "goto", "url": "https://second.example.com/" }
        ])))
        .as_deref(),
        Some("sub.example.com")
    );
    assert_eq!(
        job_target_host(&req_with_url(json!([
            { "action": "goto", "url": "http://[::1]:8080/page" }
        ])))
        .as_deref(),
        Some("::1")
    );
    assert_eq!(
        job_target_host(&req_with_url(json!([
            { "action": "goto", "url": "about:blank" }
        ]))),
        None
    );
    assert_eq!(
        job_target_host(&req_with_url(json!([{ "action": "title" }]))),
        None
    );
}

#[tokio::test]
async fn rejects_both_journal_and_store_on_server_start() {
    let mut cfg = ServerConfig::new(TOKEN.into()).unwrap();
    cfg.journal = Some(std::env::temp_dir().join("conflict.jsonl"));
    cfg.store = Some(std::env::temp_dir().join("conflict.sqlite"));
    assert!(matches!(
        HttpServer::coordinator(cfg),
        Err(ConfigError::ConflictingStore)
    ));
}

#[tokio::test]
async fn coordinator_sqlite_store_preserves_finished_results_and_replays_unfinished_jobs() {
    let db_path =
        std::env::temp_dir().join(format!("fluxwright-coord-store-{}.sqlite", Uuid::new_v4()));
    let _cleanup = || {
        let _ = std::fs::remove_file(&db_path);
        for suffix in ["-wal", "-shm"] {
            let mut s = db_path.as_os_str().to_os_string();
            s.push(suffix);
            let _ = std::fs::remove_file(std::path::Path::new(&s));
        }
    };

    let start_coord = |max_jobs: usize, ttl: Duration| {
        let mut cfg = ServerConfig::new(TOKEN.into()).unwrap();
        cfg.max_jobs = max_jobs;
        cfg.store = Some(db_path.clone());
        cfg.result_ttl = ttl;
        HttpServer::coordinator(cfg).unwrap()
    };

    let coord1 = start_coord(8, Duration::from_secs(3600));
    let j_ok = submit_priority(&coord1, "normal", 0).await;
    let j_fail = submit_priority(&coord1, "normal", 0).await;
    let j_cancel = submit_priority(&coord1, "normal", 0).await;
    let j_running = submit_priority(&coord1, "normal", 0).await;
    let j_queued = submit_priority(&coord1, "normal", 0).await;

    assert_eq!(
        call(
            &coord1,
            "DELETE",
            &format!("/v1/jobs/{}", j_cancel.id),
            Body::empty()
        )
        .await
        .status(),
        StatusCode::OK
    );
    tokio::time::sleep(Duration::from_millis(5)).await;

    // Lease 3 jobs -> j_ok, j_fail, j_running.
    let lease = body(
        call(
            &coord1,
            "POST",
            "/v1/workers/w1/lease",
            r#"{"max_jobs":3}"#,
        )
        .await,
    )
    .await;
    assert_eq!(lease["jobs"].as_array().unwrap().len(), 3);

    let ok_res = call(
        &coord1,
        "POST",
        &format!("/v1/jobs/{}/result", j_ok.id),
        json!({
            "worker": "w1",
            "status": "succeeded",
            "attempts": 1,
            "step_index": 0,
            "results": ["Recovered Title"]
        })
        .to_string(),
    )
    .await;
    assert_eq!(ok_res.status(), StatusCode::OK);
    tokio::time::sleep(Duration::from_millis(5)).await;

    let fail_res = call(
        &coord1,
        "POST",
        &format!("/v1/jobs/{}/result", j_fail.id),
        json!({
            "worker": "w1",
            "status": "failed",
            "attempts": 1,
            "step_index": 0,
            "error": "boom-error",
            "failure_evidence": {
                "console": [{ "kind": "error", "text": "console-err" }],
                "network": [],
                "screenshot": "iVBORmock"
            }
        })
        .to_string(),
    )
    .await;
    assert_eq!(fail_res.status(), StatusCode::OK);

    // Stop coord1 while j_running is still leased and j_queued is still queued.
    coord1.shutdown(Duration::ZERO).await.unwrap();
    drop(coord1);

    // Restart with max_jobs = 5: 2 pending + all 3 finished fit.
    let coord2 = start_coord(5, Duration::from_secs(3600));

    let got_ok = body(
        call(
            &coord2,
            "GET",
            &format!("/v1/jobs/{}", j_ok.id),
            Body::empty(),
        )
        .await,
    )
    .await;
    assert_eq!(got_ok["status"], "succeeded");
    assert_eq!(got_ok["results"], json!(["Recovered Title"]));

    let got_fail = body(
        call(
            &coord2,
            "GET",
            &format!("/v1/jobs/{}", j_fail.id),
            Body::empty(),
        )
        .await,
    )
    .await;
    assert_eq!(got_fail["status"], "failed");
    assert_eq!(got_fail["error"], "boom-error");
    assert_eq!(got_fail["failure_evidence"]["screenshot"], "iVBORmock");

    let got_cancel = body(
        call(
            &coord2,
            "GET",
            &format!("/v1/jobs/{}", j_cancel.id),
            Body::empty(),
        )
        .await,
    )
    .await;
    assert_eq!(got_cancel["status"], "cancelled");

    // Both unfinished jobs are queued again with no worker assigned.
    for pending_id in [j_running.id, j_queued.id] {
        let st = body(
            call(
                &coord2,
                "GET",
                &format!("/v1/jobs/{pending_id}"),
                Body::empty(),
            )
            .await,
        )
        .await;
        assert_eq!(st["status"], "queued");
        assert_eq!(st["attempts"], 0);
    }

    // A worker can lease both unfinished jobs cleanly.
    let leased2 = body(
        call(
            &coord2,
            "POST",
            "/v1/workers/w2/lease",
            r#"{"max_jobs":5}"#,
        )
        .await,
    )
    .await;
    let leased_ids: Vec<String> = leased2["jobs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|j| j["id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        leased_ids,
        vec![j_running.id.to_string(), j_queued.id.to_string()]
    );
    coord2.shutdown(Duration::ZERO).await.unwrap();
    drop(coord2);

    // Restart with max_jobs = 3 -> 2 pending + 1 newest finished (j_fail).
    let coord_bounded = start_coord(3, Duration::from_secs(3600));
    assert_eq!(
        call(
            &coord_bounded,
            "GET",
            &format!("/v1/jobs/{}", j_fail.id),
            Body::empty(),
        )
        .await
        .status(),
        StatusCode::OK
    );
    assert_eq!(
        call(
            &coord_bounded,
            "GET",
            &format!("/v1/jobs/{}", j_cancel.id),
            Body::empty(),
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
    coord_bounded.shutdown(Duration::ZERO).await.unwrap();
    drop(coord_bounded);

    // Restart with TTL = 0 -> all terminal jobs purged, pending jobs preserved.
    let coord3 = start_coord(8, Duration::ZERO);
    for done_id in [j_ok.id, j_fail.id, j_cancel.id] {
        assert_eq!(
            call(
                &coord3,
                "GET",
                &format!("/v1/jobs/{done_id}"),
                Body::empty(),
            )
            .await
            .status(),
            StatusCode::NOT_FOUND
        );
    }
    for pending_id in [j_running.id, j_queued.id] {
        assert_eq!(
            call(
                &coord3,
                "GET",
                &format!("/v1/jobs/{pending_id}"),
                Body::empty(),
            )
            .await
            .status(),
            StatusCode::OK
        );
    }
    coord3.shutdown(Duration::ZERO).await.unwrap();
    drop(coord3);
    _cleanup();
}

async fn submit_url(server: &HttpServer, url: &str, priority: &str) -> Job {
    let payload = json!({
        "priority": priority,
        "retries": 1,
        "steps": [{ "action": "goto", "url": url }, { "action": "title" }],
    });
    let response = call(server, "POST", "/v1/jobs", payload.to_string()).await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    serde_json::from_value(body(response).await).unwrap()
}

#[tokio::test]
async fn coordinator_enforces_global_per_host_concurrency_without_hol_blocking() {
    let mut cfg = ServerConfig::new(TOKEN.into()).unwrap();
    cfg.max_jobs = 16;
    cfg.max_concurrency_per_host = Some(1);
    let coord = HttpServer::coordinator(cfg).unwrap();

    // High-priority A1 & A2, followed by Low-priority B1.
    let a1 = submit_url(&coord, "https://host-a.example/1", "high").await;
    let a2 = submit_url(&coord, "https://HOST-A.example:443/2", "high").await;
    let b1 = submit_url(&coord, "https://host-b.example/1", "low").await;

    // Worker 1 asks for up to 3 jobs: gets A1 and B1 (skips A2 without HOL blocking).
    let l1 = body(
        call(
            &coord,
            "POST",
            "/v1/workers/w1/lease",
            r#"{"max_jobs":3}"#,
        )
        .await,
    )
    .await;
    let ids1: Vec<String> = l1["jobs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|j| j["id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(ids1, vec![a1.id.to_string(), b1.id.to_string()]);

    // Worker 2 cannot lease A2 while A1 is still running on worker 1.
    let l2 = body(
        call(
            &coord,
            "POST",
            "/v1/workers/w2/lease",
            r#"{"max_jobs":3}"#,
        )
        .await,
    )
    .await;
    assert!(l2["jobs"].as_array().unwrap().is_empty());

    // Once worker 1 finishes A1, worker 2 can immediately lease A2.
    let res = call(
        &coord,
        "POST",
        &format!("/v1/jobs/{}/result", a1.id),
        json!({
            "worker": "w1",
            "status": "succeeded",
            "attempts": 1,
            "step_index": 1,
            "results": [null, "ok"]
        })
        .to_string(),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);

    let l3 = body(
        call(
            &coord,
            "POST",
            "/v1/workers/w2/lease",
            r#"{"max_jobs":1}"#,
        )
        .await,
    )
    .await;
    assert_eq!(l3["jobs"][0]["id"], a2.id.to_string());

    // Releasing A2 drops running count so another worker can lease it right away.
    assert_eq!(
        call(
            &coord,
            "POST",
            "/v1/workers/w2/release",
            json!({ "jobs": [a2.id] }).to_string(),
        )
        .await
        .status(),
        StatusCode::OK
    );
    coord.set_lease_timeout(Duration::from_millis(50));
    let l4 = body(
        call(
            &coord,
            "POST",
            "/v1/workers/w3/lease",
            r#"{"max_jobs":1}"#,
        )
        .await,
    )
    .await;
    assert_eq!(l4["jobs"][0]["id"], a2.id.to_string());

    // Lease expiration also frees the per-host slot so another worker can run A2.
    tokio::time::sleep(Duration::from_millis(120)).await;
    let l5 = body(
        call(
            &coord,
            "POST",
            "/v1/workers/w4/lease",
            r#"{"max_jobs":1}"#,
        )
        .await,
    )
    .await;
    assert_eq!(l5["jobs"][0]["id"], a2.id.to_string());

    coord.shutdown(Duration::ZERO).await.unwrap();
}

#[tokio::test]
async fn coordinator_enforces_global_per_host_delay_across_workers_without_hol_blocking() {
    let mut cfg = ServerConfig::new(TOKEN.into()).unwrap();
    cfg.max_jobs = 16;
    cfg.min_delay_per_host = Some(Duration::from_millis(120));
    let coord = HttpServer::coordinator(cfg).unwrap();

    let a1 = submit_url(&coord, "https://rate.example/1", "normal").await;
    let a2 = submit_url(&coord, "https://rate.example/2", "normal").await;
    let b1 = submit_url(&coord, "https://other.example/1", "normal").await;

    // Even in one batch with uncapped concurrency, only A1 and B1 lease immediately; A2 waits.
    let l1 = body(
        call(
            &coord,
            "POST",
            "/v1/workers/w1/lease",
            r#"{"max_jobs":3}"#,
        )
        .await,
    )
    .await;
    let ids1: Vec<String> = l1["jobs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|j| j["id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(ids1, vec![a1.id.to_string(), b1.id.to_string()]);

    // Finish A1 quickly; A2 still must wait for min_delay_per_host.
    assert_eq!(
        call(
            &coord,
            "POST",
            &format!("/v1/jobs/{}/result", a1.id),
            json!({
                "worker": "w1",
                "status": "succeeded",
                "attempts": 1,
                "step_index": 1,
                "results": [null, "ok"]
            })
            .to_string(),
        )
        .await
        .status(),
        StatusCode::OK
    );

    let l_early = body(
        call(
            &coord,
            "POST",
            "/v1/workers/w2/lease",
            r#"{"max_jobs":2}"#,
        )
        .await,
    )
    .await;
    assert!(l_early["jobs"].as_array().unwrap().is_empty());

    // After delay passes, worker 2 leases A2.
    tokio::time::sleep(Duration::from_millis(150)).await;
    let l_after = body(
        call(
            &coord,
            "POST",
            "/v1/workers/w2/lease",
            r#"{"max_jobs":2}"#,
        )
        .await,
    )
    .await;
    assert_eq!(l_after["jobs"].as_array().unwrap().len(), 1);
    assert_eq!(l_after["jobs"][0]["id"], a2.id.to_string());

    coord.shutdown(Duration::ZERO).await.unwrap();
}

#[test]
fn http_post_parses_urls_validates_headers_and_blocks_ssrf_ips() {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

    let ep = http_post::HttpEndpoint::parse("https://Hooks.Example.COM:8443/v1/hook?token=abc#frag", true)
        .unwrap();
    assert!(ep.tls);
    assert_eq!(ep.host, "hooks.example.com");
    assert_eq!(ep.port, 8443);
    assert_eq!(ep.host_header, "Hooks.Example.COM:8443");
    assert_eq!(ep.path_and_query, "/v1/hook?token=abc");

    let q_root = http_post::HttpEndpoint::parse("http://127.0.0.1:9000?a=1", true).unwrap();
    assert_eq!(q_root.path_and_query, "/?a=1");
    assert!(q_root.is_loopback());

    let v6 = http_post::HttpEndpoint::parse("https://[::1]:443/otel/", false).unwrap();
    assert_eq!(v6.host, "::1");
    assert!(v6.is_loopback());
    assert_eq!(v6.base_path(), "/otel");

    for bad in [
        "",
        "ftp://example.com/hook",
        "https://user:pass@example.com/hook",
        "https://example.com\\@169.254.169.254/",
        "https://example.com:0/hook",
        "https://exa\nmple.com/hook",
        "https://[::1/hook",
    ] {
        assert!(http_post::HttpEndpoint::parse(bad, true).is_err(), "{bad}");
    }

    // Header hygiene checks
    assert!(http_post::validate_header_name("X-Api-Key").is_ok());
    assert!(http_post::validate_header_name("Bad Header").is_err());
    assert!(http_post::validate_header_name("Bad:Name").is_err());
    assert!(http_post::validate_header_value("ok-value").is_ok());
    assert!(http_post::validate_header_value("bad\r\nInjected: 1").is_err());
    assert!(http_post::validate_header_value("bad\nline").is_err());
    for reserved in [
        "Host",
        "content-length",
        "Connection",
        "Transfer-Encoding",
        "X-Fluxwright-Signature",
        "x-fluxwright-event",
    ] {
        assert!(http_post::is_reserved_header(reserved), "{reserved}");
    }
    assert!(!http_post::is_reserved_header("X-Custom-Token"));

    // SSRF IP and mixed DNS resolution checks
    for blocked in [
        IpAddr::V4(Ipv4Addr::new(169, 254, 169, 254)),
        IpAddr::V6("fe80::1".parse::<Ipv6Addr>().unwrap()),
        IpAddr::V6("fd00:ec2::254".parse::<Ipv6Addr>().unwrap()),
        IpAddr::V6("::ffff:169.254.169.254".parse::<Ipv6Addr>().unwrap()),
    ] {
        assert!(http_post::ip_allowed(blocked).is_err(), "{blocked}");
        // Mixed DNS resolution containing even one blocked IP must be rejected entirely.
        let mixed = vec![
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34)), 443),
            SocketAddr::new(blocked, 443),
        ];
        assert!(http_post::validate_resolved_addrs(mixed.clone(), true).is_err());
        assert!(http_post::validate_resolved_addrs(mixed, false).is_ok());
    }
    for allowed in [
        IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
        IpAddr::V6("::1".parse::<Ipv6Addr>().unwrap()),
        IpAddr::V4(Ipv4Addr::new(10, 0, 0, 5)),
        IpAddr::V4(Ipv4Addr::new(192, 168, 1, 10)),
        IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)),
    ] {
        assert!(http_post::ip_allowed(allowed).is_ok(), "{allowed}");
    }
}

#[tokio::test]
async fn webhook_validation_rejects_ssrf_and_reserved_headers_and_verifies_hmac() {
    let srv = server(4).await;

    // SSRF and malformed webhook URLs rejected with 400.
    for bad_url in [
        "file:///etc/passwd",
        "javascript:alert(1)",
        "http://169.254.169.254/latest/meta-data",
        "http://2852039166/hook",
        "http://metadata.google.internal/hook",
        "http://[::ffff:169.254.169.254]/hook",
        "http://[fe80::1]/hook",
        "http://user:pass@example.com/hook",
    ] {
        let payload = json!({
            "steps": [{ "action": "title" }],
            "webhook_url": bad_url,
        });
        let resp = call(&srv, "POST", "/v1/jobs", payload.to_string()).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{bad_url}");
    }

    // Reserved, CR/LF, duplicate-case, and >16 webhook_headers rejected.
    let mut too_many = serde_json::Map::new();
    for i in 0..17 {
        too_many.insert(format!("X-H-{i}"), json!("v"));
    }
    for bad_headers in [
        json!({ "Host": "evil.example" }),
        json!({ "Content-Length": "999" }),
        json!({ "Connection": "keep-alive" }),
        json!({ "Transfer-Encoding": "chunked" }),
        json!({ "X-Fluxwright-Signature": "sha256=deadbeef" }),
        json!({ "x-fluxwright-event": "fake" }),
        json!({ "X-Test": "line1\r\nX-Evil: 1" }),
        json!({ "X-Test": "val", "x-test": "dup" }),
        Value::Object(too_many),
    ] {
        let payload = json!({
            "steps": [{ "action": "title" }],
            "webhook_url": "http://127.0.0.1:9999/hook",
            "webhook_headers": bad_headers,
        });
        let resp = call(&srv, "POST", "/v1/jobs", payload.to_string()).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{bad_headers}");
    }

    srv.shutdown(Duration::ZERO).await.unwrap();

    // allow_any_url permits 169.254.x.x webhook URLs (while still rejecting non-http schemes).
    let engine = idle_engine().await;
    let mut cfg = ServerConfig::new(TOKEN.into()).unwrap();
    cfg.allow_any_url = true;
    let any_srv = HttpServer::new(engine, cfg).unwrap();
    let ok_any = call(
        &any_srv,
        "POST",
        "/v1/jobs",
        json!({
            "steps": [{ "action": "title" }],
            "webhook_url": "http://169.254.169.254/hook",
        })
        .to_string(),
    )
    .await;
    assert_eq!(ok_any.status(), StatusCode::ACCEPTED);
    let still_bad = call(
        &any_srv,
        "POST",
        "/v1/jobs",
        json!({
            "steps": [{ "action": "title" }],
            "webhook_url": "file:///etc/passwd",
        })
        .to_string(),
    )
    .await;
    assert_eq!(still_bad.status(), StatusCode::BAD_REQUEST);
    any_srv.shutdown(Duration::ZERO).await.unwrap();

    // HMAC-SHA256 verification against ring::hmac::verify
    let secret = b"whsec_super_secret";
    let body = br#"{"event":"job.completed","job":{"id":"123"}}"#;
    let ts = 1_793_664_000u64;
    let sig_header = webhook::sign_payload(secret, ts, body);
    let hex_sig = sig_header.strip_prefix("sha256=").expect("sha256= prefix");
    assert_eq!(hex_sig.len(), 64);
    let sig_bytes: Vec<u8> = (0..hex_sig.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex_sig[i..i + 2], 16).unwrap())
        .collect();
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, secret);
    let signed_msg = format!("{ts}.{}", std::str::from_utf8(body).unwrap());
    assert!(ring::hmac::verify(&key, signed_msg.as_bytes(), &sig_bytes).is_ok());
    assert!(ring::hmac::verify(&key, b"tampered", &sig_bytes).is_err());

    // Config validation rejects empty webhook secrets.
    assert!(matches!(
        ServerConfig::new(TOKEN.into()).unwrap().with_webhook_secret(String::new()),
        Err(ConfigError::InvalidWebhookSecret(_))
    ));
}

#[derive(Clone)]
struct CapturedWebhook {
    uri: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

async fn spawn_webhook_receiver(fail_first_n: usize) -> (String, Arc<Mutex<Vec<CapturedWebhook>>>, tokio::task::JoinHandle<()>) {
    use axum::body::Bytes;
    use axum::http::{HeaderMap, Uri};
    use std::sync::atomic::{AtomicUsize, Ordering};

    let captured = Arc::new(Mutex::new(Vec::new()));
    let attempts = Arc::new(AtomicUsize::new(0));
    let app = axum::Router::new().route(
        "/hook",
        axum::routing::post({
            let captured = captured.clone();
            let attempts = attempts.clone();
            move |uri: Uri, headers: HeaderMap, body: Bytes| async move {
                let n = attempts.fetch_add(1, Ordering::SeqCst);
                let mut hdrs = HashMap::new();
                for (k, v) in &headers {
                    if let Ok(val) = v.to_str() {
                        hdrs.insert(k.as_str().to_ascii_lowercase(), val.to_string());
                    }
                }
                captured.lock().unwrap().push(CapturedWebhook {
                    uri: uri.to_string(),
                    headers: hdrs,
                    body: body.to_vec(),
                });
                if n < fail_first_n {
                    StatusCode::INTERNAL_SERVER_ERROR
                } else {
                    StatusCode::OK
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}/hook?src=fw"), captured, handle)
}

fn verify_webhook_signature(secret: &[u8], cap: &CapturedWebhook) {
    let ts = cap
        .headers
        .get("x-fluxwright-timestamp")
        .expect("missing X-Fluxwright-Timestamp");
    let sig = cap
        .headers
        .get("x-fluxwright-signature")
        .expect("missing X-Fluxwright-Signature");
    let hex_sig = sig.strip_prefix("sha256=").expect("sha256= prefix");
    let sig_bytes: Vec<u8> = (0..hex_sig.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex_sig[i..i + 2], 16).unwrap())
        .collect();
    let mut msg = format!("{ts}.").into_bytes();
    msg.extend_from_slice(&cap.body);
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, secret);
    ring::hmac::verify(&key, &msg, &sig_bytes).expect("HMAC signature mismatch");
}

#[tokio::test]
async fn webhook_delivers_signed_payload_retries_500_to_200_and_hides_secrets() {
    let secret = "whsec_standalone_secret";
    let (hook_url, captured, hook_task) = spawn_webhook_receiver(1).await;

    let engine = idle_engine().await;
    let config = ServerConfig::new(TOKEN.into())
        .unwrap()
        .with_webhook_secret(secret.into())
        .unwrap();
    let srv = HttpServer::new(engine, config).unwrap();

    // 1. Submit job with secret webhook header + query string on webhook_url.
    let sub_resp = call(
        &srv,
        "POST",
        "/v1/jobs",
        json!({
            "steps": [{ "action": "title" }],
            "webhook_url": hook_url,
            "webhook_headers": { "Authorization": "Bearer hook-secret-xyz", "X-Env": "ci" },
        })
        .to_string(),
    )
    .await;
    assert_eq!(sub_resp.status(), StatusCode::ACCEPTED);
    let sub_bytes = to_bytes(sub_resp.into_body(), MAX_RESULT_BYTES).await.unwrap();
    let sub_text = String::from_utf8(sub_bytes.to_vec()).unwrap();
    assert!(!sub_text.contains("hook-secret-xyz"));
    assert!(!sub_text.contains(&hook_url));
    let job1: Job = serde_json::from_str(&sub_text).unwrap();

    let get_resp = call(&srv, "GET", &format!("/v1/jobs/{}", job1.id), Body::empty()).await;
    assert_eq!(get_resp.status(), StatusCode::OK);
    let get_text = String::from_utf8(
        to_bytes(get_resp.into_body(), MAX_RESULT_BYTES)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(!get_text.contains("hook-secret-xyz"));

    // Finish job1: first attempt hits 500, second attempt after 500ms backoff succeeds 200.
    finish_job(&srv.inner, job1.id, Ok(Ok(vec![json!("Example Title")])));
    tokio::time::timeout(Duration::from_secs(5), async {
        while captured.lock().unwrap().len() < 2 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("webhook retried from 500 -> 200");

    {
        let caps = captured.lock().unwrap();
        assert_eq!(caps.len(), 2);
        let ok_cap = &caps[1];
        assert_eq!(ok_cap.uri, "/hook?src=fw");
        assert_eq!(ok_cap.headers.get("content-type").map(String::as_str), Some("application/json"));
        assert_eq!(ok_cap.headers.get("x-fluxwright-event").map(String::as_str), Some("job.completed"));
        assert_eq!(
            ok_cap.headers.get("x-fluxwright-job-id").map(String::as_str),
            Some(job1.id.to_string().as_str())
        );
        assert_eq!(ok_cap.headers.get("authorization").map(String::as_str), Some("Bearer hook-secret-xyz"));
        assert_eq!(ok_cap.headers.get("x-env").map(String::as_str), Some("ci"));
        verify_webhook_signature(secret.as_bytes(), ok_cap);
        let payload: Value = serde_json::from_slice(&ok_cap.body).unwrap();
        assert_eq!(payload["event"], "job.completed");
        assert_eq!(payload["job"]["id"], job1.id.to_string());
        assert_eq!(payload["job"]["status"], "succeeded");
        assert_eq!(payload["job"]["results"], json!(["Example Title"]));
    }

    // 2. Cancelled job also dispatches webhook.
    captured.lock().unwrap().clear();
    let sub2 = call(
        &srv,
        "POST",
        "/v1/jobs",
        json!({
            "steps": [{ "action": "title" }],
            "webhook_url": hook_url,
        })
        .to_string(),
    )
    .await;
    assert_eq!(sub2.status(), StatusCode::ACCEPTED);
    let job2: Job = serde_json::from_value(body(sub2).await).unwrap();
    assert_eq!(
        call(&srv, "DELETE", &format!("/v1/jobs/{}", job2.id), Body::empty())
            .await
            .status(),
        StatusCode::OK
    );

    tokio::time::timeout(Duration::from_secs(3), async {
        while captured.lock().unwrap().is_empty() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("cancelled job webhook delivered");
    let cancel_cap = &captured.lock().unwrap()[0];
    verify_webhook_signature(secret.as_bytes(), cancel_cap);
    let cancel_payload: Value = serde_json::from_slice(&cancel_cap.body).unwrap();
    assert_eq!(cancel_payload["event"], "job.completed");
    assert_eq!(cancel_payload["job"]["id"], job2.id.to_string());
    assert_eq!(cancel_payload["job"]["status"], "cancelled");

    srv.shutdown(Duration::ZERO).await.unwrap();
    hook_task.abort();
}

#[tokio::test]
async fn coordinator_dispatches_signed_webhooks_on_result_cancel_and_lease_expiry_and_strips_worker_secrets() {
    let secret = "whsec_coord_secret";
    let (hook_url, captured, hook_task) = spawn_webhook_receiver(0).await;

    let mut config = ServerConfig::new(TOKEN.into())
        .unwrap()
        .with_webhook_secret(secret.into())
        .unwrap();
    config.max_jobs = 8;
    let coord = HttpServer::coordinator(config).unwrap();
    coord.set_lease_timeout(Duration::from_millis(50));

    let submit_with_hook = || async {
        let resp = call(
            &coord,
            "POST",
            "/v1/jobs",
            json!({
                "steps": [{ "action": "title" }],
                "webhook_url": hook_url,
                "webhook_headers": { "X-Secret": "top-secret" },
            })
            .to_string(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
        serde_json::from_value::<Job>(body(resp).await).unwrap()
    };

    let j_ok = submit_with_hook().await;
    let j_lost = submit_with_hook().await;
    let j_cancel = submit_with_hook().await;

    // Leasing to workers must strip webhook_url and webhook_headers secrets.
    let leased = body(
        call(&coord, "POST", "/v1/workers/w1/lease", r#"{"max_jobs":2}"#).await,
    )
    .await;
    let leased_text = leased.to_string();
    assert!(!leased_text.contains("top-secret"));
    assert!(!leased_text.contains(&hook_url));
    assert!(leased["jobs"][0]["request"]["webhook_url"].is_null());

    // 1. Worker posts succeeded result -> webhook delivered.
    assert_eq!(
        call(
            &coord,
            "POST",
            &format!("/v1/jobs/{}/result", j_ok.id),
            json!({
                "worker": "w1",
                "status": "succeeded",
                "attempts": 1,
                "step_index": 0,
                "results": ["Cluster OK"],
            })
            .to_string(),
        )
        .await
        .status(),
        StatusCode::OK
    );

    // 2. Cancel j_cancel -> webhook delivered.
    assert_eq!(
        call(&coord, "DELETE", &format!("/v1/jobs/{}", j_cancel.id), Body::empty())
            .await
            .status(),
        StatusCode::OK
    );

    // 3. Lease on j_lost expires -> "worker lost" failure -> webhook delivered.
    tokio::time::sleep(Duration::from_millis(120)).await;
    let _ = call(&coord, "GET", &format!("/v1/jobs/{}", j_lost.id), Body::empty()).await;

    tokio::time::timeout(Duration::from_secs(5), async {
        while captured.lock().unwrap().len() < 3 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("all 3 coordinator terminal webhooks delivered");

    let caps = captured.lock().unwrap();
    let mut statuses_by_id = HashMap::new();
    for cap in caps.iter() {
        verify_webhook_signature(secret.as_bytes(), cap);
        assert_eq!(cap.headers.get("x-secret").map(String::as_str), Some("top-secret"));
        let v: Value = serde_json::from_slice(&cap.body).unwrap();
        assert_eq!(v["event"], "job.completed");
        statuses_by_id.insert(
            v["job"]["id"].as_str().unwrap().to_string(),
            v["job"]["status"].as_str().unwrap().to_string(),
        );
    }
    assert_eq!(statuses_by_id.get(&j_ok.id.to_string()).map(String::as_str), Some("succeeded"));
    assert_eq!(statuses_by_id.get(&j_cancel.id.to_string()).map(String::as_str), Some("cancelled"));
    assert_eq!(statuses_by_id.get(&j_lost.id.to_string()).map(String::as_str), Some("failed"));

    coord.shutdown(Duration::ZERO).await.unwrap();
    hook_task.abort();
}

#[tokio::test]
async fn otlp_exporter_emits_resource_spans_with_traceparent_and_attributes() {
    use axum::body::Bytes;

    let batches: Arc<Mutex<Vec<(String, Value)>>> = Arc::new(Mutex::new(Vec::new()));
    let app = axum::Router::new().route(
        "/collector/v1/traces",
        axum::routing::post({
            let batches = batches.clone();
            move |headers: axum::http::HeaderMap, body: Bytes| async move {
                let ct = headers
                    .get(header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_string();
                let val: Value = serde_json::from_slice(&body).unwrap();
                batches.lock().unwrap().push((ct, val));
                StatusCode::OK
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let collector_task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    assert!(matches!(
        ServerConfig::new(TOKEN.into())
            .unwrap()
            .with_otlp_endpoint("ftp://bad".into()),
        Err(ConfigError::InvalidOtlpEndpoint(_))
    ));
    assert_eq!(
        otlp::parse_otlp_endpoint("http://127.0.0.1:4318")
            .unwrap()
            .1,
        "/v1/traces"
    );
    assert_eq!(
        otlp::parse_otlp_endpoint("http://127.0.0.1:4318/collector/v1/traces")
            .unwrap()
            .1,
        "/collector/v1/traces"
    );

    let config = ServerConfig::new(TOKEN.into())
        .unwrap()
        .with_otlp_endpoint(format!("http://{addr}/collector"))
        .unwrap()
        .with_otlp_service_name("fluxwright-test-svc".into());
    let coord = HttpServer::coordinator(config).unwrap();

    let tp = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
    let sub_resp = coord
        .router()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/jobs")
                .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                .header(header::CONTENT_TYPE, "application/json")
                .header("traceparent", tp)
                .body(Body::from(
                    r#"{"steps":[{"action":"title"},{"action":"url"}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(sub_resp.status(), StatusCode::ACCEPTED);
    let job: Job = serde_json::from_value(body(sub_resp).await).unwrap();

    let _ = call(&coord, "POST", "/v1/workers/w1/lease", r#"{"max_jobs":1}"#).await;
    assert_eq!(
        call(
            &coord,
            "POST",
            &format!("/v1/jobs/{}/result", job.id),
            json!({
                "worker": "w1",
                "status": "succeeded",
                "attempts": 2,
                "step_index": 1,
                "results": ["t", "u"],
            })
            .to_string(),
        )
        .await
        .status(),
        StatusCode::OK
    );

    // Graceful shutdown flushes buffered OTLP spans without waiting the full 2s timer.
    coord.shutdown(Duration::from_secs(2)).await.unwrap();

    let recorded = batches.lock().unwrap();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].0, "application/json");
    let rs = &recorded[0].1["resourceSpans"][0];
    assert_eq!(rs["resource"]["attributes"][0]["key"], "service.name");
    assert_eq!(
        rs["resource"]["attributes"][0]["value"]["stringValue"],
        "fluxwright-test-svc"
    );
    let span = &rs["scopeSpans"][0]["spans"][0];
    assert_eq!(span["traceId"], "4bf92f3577b34da6a3ce929d0e0e4736");
    assert_eq!(span["parentSpanId"], "00f067aa0ba902b7");
    assert_eq!(span["spanId"].as_str().unwrap().len(), 16);
    assert_eq!(span["status"]["code"], 1);
    let start_ns: u128 = span["startTimeUnixNano"].as_str().unwrap().parse().unwrap();
    let end_ns: u128 = span["endTimeUnixNano"].as_str().unwrap().parse().unwrap();
    assert!(start_ns > 0 && end_ns >= start_ns);

    let attrs = span["attributes"].as_array().unwrap();
    let find_attr = |k: &str| {
        attrs
            .iter()
            .find(|a| a["key"] == k)
            .unwrap()["value"]
            .clone()
    };
    assert_eq!(find_attr("job.id")["stringValue"], job.id.to_string());
    assert_eq!(find_attr("job.status")["stringValue"], "succeeded");
    assert_eq!(find_attr("job.steps")["intValue"], "2");
    assert_eq!(find_attr("job.attempts")["intValue"], "2");

    collector_task.abort();
}

#[tokio::test]
async fn otlp_exporter_batches_at_32_and_drops_cleanly_when_bounded_channel_full() {
    use axum::body::Bytes;

    let total_spans = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let app = axum::Router::new().route(
        "/v1/traces",
        axum::routing::post({
            let total_spans = total_spans.clone();
            move |body: Bytes| async move {
                let val: Value = serde_json::from_slice(&body).unwrap();
                let count = val["resourceSpans"][0]["scopeSpans"][0]["spans"]
                    .as_array()
                    .map_or(0, Vec::len);
                total_spans.fetch_add(count, std::sync::atomic::Ordering::SeqCst);
                StatusCode::OK
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let collector_task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let exporter =
        otlp::OtlpExporter::new(&format!("http://{addr}"), "batch-svc".into()).unwrap();
    for _ in 0..32 {
        exporter.try_record(otlp::OtlpSpanData {
            trace_ctx: JobTraceContext::new(),
            job_id: Uuid::new_v4(),
            status: JobStatus::Succeeded,
            steps: 1,
            attempts: 1,
            start_time_unix_nano: 1_000,
            end_time_unix_nano: 2_000,
            error: None,
        });
    }

    // 32 spans must flush immediately without waiting for the 2s periodic flush.
    tokio::time::timeout(Duration::from_millis(800), async {
        while total_spans.load(std::sync::atomic::Ordering::SeqCst) < 32 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("32-span OTLP batch flushed promptly");

    // Verify bounded channel (512) never blocks even when hammered synchronously without draining.
    let (tx, _rx) = tokio::sync::mpsc::channel::<u32>(512);
    for i in 0..600 {
        let _ = tx.try_send(i);
    }

    collector_task.abort();
}

async fn spawn_mock_cdp_server() -> (
    std::net::SocketAddr,
    Arc<Mutex<Vec<String>>>,
    tokio::task::JoinHandle<()>,
) {
    use axum::extract::ws::{Message as AxumMsg, WebSocketUpgrade};
    use futures_util::{SinkExt, StreamExt};

    let recorded_cmds: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let app = axum::Router::new().route(
        "/",
        axum::routing::get({
            let recorded_cmds = recorded_cmds.clone();
            move |ws: WebSocketUpgrade| {
                let recorded_cmds = recorded_cmds.clone();
                async move {
                    ws.on_upgrade(move |mut socket| async move {
                        while let Some(Ok(AxumMsg::Text(txt))) = socket.next().await {
                            let Ok(v) = serde_json::from_str::<Value>(txt.as_str()) else {
                                continue;
                            };
                            let Some(id) = v.get("id").and_then(Value::as_u64) else {
                                continue;
                            };
                            let method = v["method"].as_str().unwrap_or("");
                            recorded_cmds.lock().unwrap().push(method.to_string());
                            let result = match method {
                                "Target.getTargetInfo" => {
                                    json!({ "targetInfo": { "url": "https://example.com" } })
                                }
                                "Target.attachToTarget" => json!({ "sessionId": "obs-1" }),
                                "Page.getLayoutMetrics" => json!({
                                    "cssLayoutViewport": { "clientWidth": 1280.0, "clientHeight": 800.0 }
                                }),
                                "Page.captureScreenshot" => json!({ "data": "AA==" }),
                                _ => json!({}),
                            };
                            let reply = json!({ "id": id, "result": result });
                            if socket
                                .send(AxumMsg::Text(reply.to_string().into()))
                                .await
                                .is_err()
                            {
                                break;
                            }
                            if method == "Page.startScreencast" {
                                let frame = json!({
                                    "method": "Page.screencastFrame",
                                    "sessionId": "obs-1",
                                    "params": {
                                        "data": "/9j/4AAQ",
                                        "sessionId": 1,
                                        "metadata": {
                                            "deviceWidth": 1280,
                                            "deviceHeight": 800,
                                            "timestamp": 1.0
                                        }
                                    }
                                });
                                let _ = socket.send(AxumMsg::Text(frame.to_string().into())).await;
                            }
                        }
                    })
                }
            }
        }),
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (addr, recorded_cmds, handle)
}

#[tokio::test]
async fn embed_live_endpoints_validate_auth_ttl_clamping_capacity_csp_and_html_isolation() {
    assert!(matches!(
        ServerConfig::new(TOKEN.into())
            .unwrap()
            .with_embed_frame_ancestors("https://ok.test; script-src 'unsafe-inline'".into()),
        Err(ConfigError::InvalidFrameAncestors(_))
    ));
    assert!(matches!(
        ServerConfig::new(TOKEN.into())
            .unwrap()
            .with_embed_frame_ancestors("   ".into()),
        Err(ConfigError::InvalidFrameAncestors(_))
    ));

    let engine = idle_engine().await;
    let config = ServerConfig::new(TOKEN.into())
        .unwrap()
        .with_embed_frame_ancestors("'self' https://app.example.com".into())
        .unwrap();
    let srv = HttpServer::new(engine, config).unwrap();

    // POST /v1/live/{id}/embed requires Bearer auth (never ?token=).
    let unauth = srv
        .router()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/live/job-1/embed")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unauth.status(), StatusCode::UNAUTHORIZED);

    let query_tok = srv
        .router()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/v1/live/job-1/embed?token={TOKEN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(query_tok.status(), StatusCode::UNAUTHORIZED);

    assert_eq!(
        call(&srv, "POST", "/v1/live/missing/embed", Body::empty())
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        srv.router()
            .oneshot(
                Request::builder()
                    .uri("/embed/live/nonexistent-token-0123456789abcdef0123456789abcdef")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );

    // Register a live session against a mock CDP connection.
    let (cdp_addr, _cmds, cdp_task) = spawn_mock_cdp_server().await;
    let conn = fluxwright_cdp::Connection::connect(&format!("ws://{cdp_addr}"))
        .await
        .unwrap();
    let (pause_tx, _pause_rx) = tokio::sync::watch::channel(false);
    let session_id = Uuid::new_v4().to_string();
    let guard = srv.inner.live.register_job(
        session_id.clone(),
        conn,
        "target-1".into(),
        "https://example.com".into(),
        now_ms(),
        pause_tx,
        Arc::new(Mutex::new(None)),
    );

    // Default body (empty) -> mode=read_only, ttl=300; ttl clamping 10..=3600.
    let def_resp = call(
        &srv,
        "POST",
        &format!("/v1/live/{session_id}/embed"),
        Body::empty(),
    )
    .await;
    assert_eq!(def_resp.status(), StatusCode::OK);
    let def_json = body(def_resp).await;
    assert_eq!(def_json["mode"], "read_only");
    assert_eq!(def_json["expires_in_secs"], 300);
    let ro_url = def_json["url"].as_str().unwrap().to_string();
    let ro_ws = def_json["ws_url"].as_str().unwrap().to_string();
    assert!(ro_url.starts_with("/embed/live/"));
    assert!(ro_ws.starts_with(&format!("/v1/live/{session_id}/ws?embed=")));

    let low_ttl = body(
        call(
            &srv,
            "POST",
            &format!("/v1/live/{session_id}/embed"),
            r#"{"ttl_secs":1}"#,
        )
        .await,
    )
    .await;
    assert_eq!(low_ttl["expires_in_secs"], 10);

    let high_ttl = body(
        call(
            &srv,
            "POST",
            &format!("/v1/live/{session_id}/embed"),
            r#"{"mode":"interactive","ttl_secs":99999}"#,
        )
        .await,
    )
    .await;
    assert_eq!(high_ttl["mode"], "interactive");
    assert_eq!(high_ttl["expires_in_secs"], 3600);
    let int_url = high_ttl["url"].as_str().unwrap().to_string();

    // Invalid mode or unknown fields rejected.
    assert_eq!(
        call(
            &srv,
            "POST",
            &format!("/v1/live/{session_id}/embed"),
            r#"{"mode":"admin"}"#,
        )
        .await
        .status(),
        StatusCode::UNPROCESSABLE_ENTITY
    );
    // Oversized embed request bodies (> 4096 bytes) rejected.
    assert_eq!(
        call(
            &srv,
            "POST",
            &format!("/v1/live/{session_id}/embed"),
            vec![b'a'; 5000],
        )
        .await
        .status(),
        StatusCode::PAYLOAD_TOO_LARGE
    );

    // Unauthenticated GET /embed/live/{token} serves minimal isolated HTML with strict CSP.
    let ro_html_resp = srv
        .router()
        .oneshot(Request::builder().uri(&ro_url).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(ro_html_resp.status(), StatusCode::OK);
    let csp = ro_html_resp.headers()["content-security-policy"]
        .to_str()
        .unwrap()
        .to_string();
    assert!(csp.contains("frame-ancestors 'self' https://app.example.com"), "{csp}");
    assert!(csp.contains("script-src 'self' 'sha256-"), "{csp}");
    assert_eq!(ro_html_resp.headers()["referrer-policy"], "no-referrer");
    assert_eq!(ro_html_resp.headers()[header::CACHE_CONTROL], "no-store");
    let ro_html = String::from_utf8(
        to_bytes(ro_html_resp.into_body(), MAX_RESULT_BYTES)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(ro_html.contains("data-mode=\"read_only\""));
    assert!(ro_html.contains("<span class=\"pill pill-readonly\">"));
    assert!(!ro_html.contains("fluxwright_token"));
    assert!(!ro_html.contains("/dashboard"));
    assert!(!ro_html.contains("<form"));
    assert!(!ro_html.contains("id=\"live-url-form\""));
    assert!(!ro_html.contains("id=\"btn-live-pause\""));
    assert!(!ro_html.contains("id=\"btn-live-esc\""));
    assert!(!ro_html.contains("id=\"btn-live-banner-resume\""));

    let int_html_resp = srv
        .router()
        .oneshot(Request::builder().uri(&int_url).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(int_html_resp.status(), StatusCode::OK);
    let int_html = String::from_utf8(
        to_bytes(int_html_resp.into_body(), MAX_RESULT_BYTES)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(int_html.contains("data-mode=\"interactive\""));
    assert!(!int_html.contains("<span class=\"pill pill-readonly\">"));
    assert!(int_html.contains("id=\"live-url-form\""));
    assert!(int_html.contains("id=\"btn-live-esc\""));
    assert!(int_html.contains("id=\"btn-live-banner-resume\""));
    assert!(!int_html.contains("fluxwright_token"));
    assert!(!int_html.contains("/dashboard"));

    // Capacity bound: 256 embed tokens max (3 already created, 253 more succeed, 257th returns 429).
    for _ in 0..253 {
        let r = call(
            &srv,
            "POST",
            &format!("/v1/live/{session_id}/embed"),
            Body::empty(),
        )
        .await;
        assert_eq!(r.status(), StatusCode::OK);
    }
    let overflow = call(
        &srv,
        "POST",
        &format!("/v1/live/{session_id}/embed"),
        Body::empty(),
    )
    .await;
    assert_eq!(overflow.status(), StatusCode::TOO_MANY_REQUESTS);

    drop(guard);
    // Closing the live session immediately revokes embed tokens.
    let after_close = srv
        .router()
        .oneshot(Request::builder().uri(&ro_url).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(after_close.status(), StatusCode::UNAUTHORIZED);

    srv.shutdown(Duration::ZERO).await.unwrap();
    cdp_task.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn embed_live_ws_enforces_read_only_vs_interactive_ttl_expiry_and_session_revocation() {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message as WsMsg;

    let engine = idle_engine().await;
    let srv = HttpServer::new(engine, ServerConfig::new(TOKEN.into()).unwrap()).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let srv_handle = tokio::spawn({
        let router = srv.router();
        async move {
            let _ = axum::serve(listener, router).await;
        }
    });

    let (cdp_addr, recorded_cmds, cdp_task) = spawn_mock_cdp_server().await;
    let conn = fluxwright_cdp::Connection::connect(&format!("ws://{cdp_addr}"))
        .await
        .unwrap();
    let (pause_tx, _pause_rx) = tokio::sync::watch::channel(false);
    let session_id = Uuid::new_v4().to_string();
    let guard = srv.inner.live.register_job(
        session_id.clone(),
        conn,
        "target-1".into(),
        "https://example.com".into(),
        now_ms(),
        pause_tx.clone(),
        Arc::new(Mutex::new(None)),
    );

    // 1. Read-only embed token rejects ALL mouse/key/wheel/text/navigate/pause/resume frames.
    let ro_resp = body(
        call(
            &srv,
            "POST",
            &format!("/v1/live/{session_id}/embed"),
            r#"{"mode":"read_only","ttl_secs":300}"#,
        )
        .await,
    )
    .await;
    let ro_ws_path = ro_resp["ws_url"].as_str().unwrap();
    let ro_token = ro_resp["url"]
        .as_str()
        .unwrap()
        .strip_prefix("/embed/live/")
        .unwrap()
        .to_string();

    // Binding check: embed token cannot be used on a different live session ID.
    let wrong_session_err = tokio_tungstenite::connect_async(&format!(
        "ws://{addr}/v1/live/other-session/ws?embed={ro_token}"
    ))
    .await
    .unwrap_err();
    assert!(matches!(
        wrong_session_err,
        tokio_tungstenite::tungstenite::Error::Http(ref r) if r.status() == StatusCode::UNAUTHORIZED
    ));

    let (mut ro_ws, _) =
        tokio_tungstenite::connect_async(&format!("ws://{addr}{ro_ws_path}"))
            .await
            .unwrap();

    // Wait for status + frame.
    let mut got_status = false;
    let mut got_frame = false;
    tokio::time::timeout(Duration::from_secs(5), async {
        while !(got_status && got_frame) {
            if let Some(Ok(WsMsg::Text(txt))) = ro_ws.next().await {
                let v: Value = serde_json::from_str(&txt).unwrap();
                match v["type"].as_str() {
                    Some("status") => {
                        assert_eq!(v["mode"], "read_only");
                        assert_eq!(v["can_pause"], false);
                        got_status = true;
                    }
                    Some("frame") => got_frame = true,
                    _ => {}
                }
            }
        }
    })
    .await
    .expect("read_only WS received status and frame");

    for blocked_cmd in [
        json!({ "type": "mouse", "event": "mousePressed", "x": 10, "y": 10, "button": "left" }),
        json!({ "type": "mouse", "event": "mouseWheel", "x": 10, "y": 10, "deltaX": 0, "deltaY": 50 }),
        json!({ "type": "key", "event": "keyDown", "key": "a", "code": "KeyA" }),
        json!({ "type": "text", "text": "secret" }),
        json!({ "type": "navigate", "url": "https://example.org" }),
        json!({ "type": "pause" }),
        json!({ "type": "resume" }),
    ] {
        ro_ws
            .send(WsMsg::Text(blocked_cmd.to_string().into()))
            .await
            .unwrap();
        let err_msg = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Some(Ok(WsMsg::Text(txt))) = ro_ws.next().await {
                    let v: Value = serde_json::from_str(&txt).unwrap();
                    if v["type"] == "error" {
                        return v;
                    }
                }
            }
        })
        .await
        .expect("read_only error reply");
        assert_eq!(err_msg["status"], 403);
        assert_eq!(err_msg["code"], "forbidden");
    }
    assert!(!*pause_tx.borrow());
    assert!(
        !recorded_cmds
            .lock()
            .unwrap()
            .iter()
            .any(|m| m.starts_with("Input.") || m == "Page.navigate"),
        "read_only embed must never forward input or navigation to CDP"
    );

    // 2. Interactive embed token forwards input & pause/resume commands cleanly.
    let int_resp = body(
        call(
            &srv,
            "POST",
            &format!("/v1/live/{session_id}/embed"),
            r#"{"mode":"interactive","ttl_secs":300}"#,
        )
        .await,
    )
    .await;
    let int_ws_path = int_resp["ws_url"].as_str().unwrap();
    let (mut int_ws, _) =
        tokio_tungstenite::connect_async(&format!("ws://{addr}{int_ws_path}"))
            .await
            .unwrap();

    // Wait for status.
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(Ok(WsMsg::Text(txt))) = int_ws.next().await {
            let v: Value = serde_json::from_str(&txt).unwrap();
            if v["type"] == "status" {
                assert_eq!(v["mode"], "interactive");
                assert_eq!(v["can_pause"], true);
                break;
            }
        }
    })
    .await
    .expect("interactive status");

    int_ws
        .send(WsMsg::Text(r#"{"type":"pause"}"#.into()))
        .await
        .unwrap();
    int_ws
        .send(WsMsg::Text(r#"{"type":"text","text":"ok"}"#.into()))
        .await
        .unwrap();

    tokio::time::timeout(Duration::from_secs(3), async {
        while !*pause_tx.borrow()
            || !recorded_cmds
                .lock()
                .unwrap()
                .iter()
                .any(|m| m == "Input.insertText")
        {
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
    })
    .await
    .expect("interactive embed forwarded pause and Input.insertText");

    // 3. TTL expiry + extend_embeds check.
    let now = std::time::Instant::now();
    let (ttl_tok, ttl_clamped, _) = srv
        .inner
        .live
        .issue_embed_at(&session_id, live::EmbedMode::ReadOnly, 10, now, 1_000)
        .unwrap();
    assert_eq!(ttl_clamped, 10);
    assert!(srv
        .inner
        .live
        .verify_embed_at(&ttl_tok, Some(&session_id), now + Duration::from_secs(9))
        .is_some());
    srv.inner
        .live
        .extend_embeds(&session_id, now + Duration::from_secs(60));
    assert!(srv
        .inner
        .live
        .verify_embed_at(&ttl_tok, Some(&session_id), now + Duration::from_secs(59))
        .is_some());
    assert!(srv
        .inner
        .live
        .verify_embed_at(&ttl_tok, Some(&session_id), now + Duration::from_secs(60))
        .is_none());

    // 4. LiveGuard drop immediately ends active embed WS streams and revokes embed tokens.
    drop(guard);
    let ended_reason = tokio::time::timeout(Duration::from_secs(3), async {
        while let Some(Ok(WsMsg::Text(txt))) = ro_ws.next().await {
            let v: Value = serde_json::from_str(&txt).unwrap();
            if v["type"] == "ended" {
                return v["reason"].as_str().unwrap().to_string();
            }
        }
        String::new()
    })
    .await
    .expect("ro_ws ended on LiveGuard::drop");
    assert_eq!(ended_reason, "session closed");

    let revoked_ws = tokio_tungstenite::connect_async(&format!("ws://{addr}{int_ws_path}"))
        .await
        .unwrap_err();
    assert!(matches!(
        revoked_ws,
        tokio_tungstenite::tungstenite::Error::Http(ref r) if r.status() == StatusCode::UNAUTHORIZED
    ));

    srv.shutdown(Duration::ZERO).await.unwrap();
    srv_handle.abort();
    cdp_task.abort();
}

#[tokio::test]
async fn managed_sessions_and_scrape_validate_auth_and_bounds() {
    let coordinator = HttpServer::coordinator(ServerConfig::new(TOKEN.into()).unwrap()).unwrap();
    for (method, path) in [
        ("POST", "/v1/sessions"),
        ("GET", "/v1/sessions"),
        ("GET", "/v1/sessions/sess-1"),
        ("POST", "/v1/sessions/sess-1/extend"),
        ("DELETE", "/v1/sessions/sess-1"),
        ("POST", "/v1/scrape"),
    ] {
        let r = call(&coordinator, method, path, Body::empty()).await;
        assert_eq!(r.status(), StatusCode::NOT_FOUND, "{method} {path}");
    }

    // Standalone server with an idle (NeverStarts) engine to exercise route auth/validation.
    let eng = idle_engine().await;
    let mut cfg = ServerConfig::new(TOKEN.into()).unwrap();
    cfg.allow_query_token = true;
    let srv = HttpServer::new(eng, cfg).unwrap();

    // REST session & scrape endpoints never accept ?token=.
    for (method, path) in [
        ("POST", format!("/v1/sessions?token={TOKEN}")),
        ("GET", format!("/v1/sessions?token={TOKEN}")),
        ("POST", format!("/v1/scrape?token={TOKEN}")),
    ] {
        let unauth = srv
            .router()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(&path)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unauth.status(), StatusCode::UNAUTHORIZED, "{method} {path}");
    }

    // /v1/sessions/{id}/cdp DOES accept ?token= when allow_query_token=true (reaches handler -> 404).
    let missing_cdp = srv
        .router()
        .oneshot(
            Request::builder()
                .uri(format!("/v1/sessions/sess-missing/cdp?token={TOKEN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(missing_cdp.status(), StatusCode::NOT_FOUND);

    // Request validation on POST /v1/sessions and POST /v1/scrape.
    assert_eq!(
        call(&srv, "POST", "/v1/sessions", r#"{"ttl_secs":5}"#)
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        call(&srv, "POST", "/v1/sessions", r#"{"viewport":{"width":0,"height":720}}"#)
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        call(&srv, "POST", "/v1/sessions", r#"{"unknown":true}"#)
            .await
            .status(),
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(
        call(&srv, "GET", "/v1/sessions/bad/id", Body::empty())
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(&srv, "GET", "/v1/sessions/bad..id", Body::empty())
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );

    assert_eq!(
        call(&srv, "POST", "/v1/scrape", r#"{"url":"https://example.com","formats":[]}"#)
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        call(&srv, "POST", "/v1/scrape", r#"{"url":"http://169.254.169.254/latest"}"#)
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        call(&srv, "POST", "/v1/scrape", r#"{"url":"https://example.com","timeout_ms":999999}"#)
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    // POST /v1/scrape enforces the 64 KiB MAX_REQUEST_BYTES body cap.
    let oversized_scrape = format!(
        r#"{{"url":"https://example.com/{}"}}"#,
        "a".repeat(MAX_REQUEST_BYTES + 128)
    );
    assert_eq!(
        call(&srv, "POST", "/v1/scrape", oversized_scrape)
            .await
            .status(),
        StatusCode::PAYLOAD_TOO_LARGE
    );

    srv.shutdown(Duration::ZERO).await.unwrap();
}

#[tokio::test]
async fn replay_network_map_and_byob_auth_bounds_and_ssrf() {
    // 1. Coordinator returns 404 for replay, network, and /v1/map.
    let coordinator = HttpServer::coordinator(ServerConfig::new(TOKEN.into()).unwrap()).unwrap();
    for (method, path) in [
        ("GET", "/v1/sessions/sess-1/replay"),
        ("GET", "/v1/sessions/sess-1/network"),
        ("POST", "/v1/map"),
    ] {
        let r = call(&coordinator, method, path, Body::empty()).await;
        assert_eq!(r.status(), StatusCode::NOT_FOUND, "{method} {path}");
    }

    // 2. Standalone server with allow_query_token=true still rejects ?token= on replay/network/map.
    let eng = idle_engine().await;
    let mut cfg = ServerConfig::new(TOKEN.into()).unwrap();
    cfg.allow_query_token = true;
    let srv = HttpServer::new(eng, cfg).unwrap();

    for (method, path) in [
        ("GET", format!("/v1/sessions/sess-1/replay?token={TOKEN}")),
        ("GET", format!("/v1/sessions/sess-1/network?token={TOKEN}")),
        ("POST", format!("/v1/map?token={TOKEN}")),
    ] {
        let r = srv
            .router()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(&path)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED, "{method} {path}");
    }

    // Query parameter validation on /replay and /network.
    assert_eq!(
        call(&srv, "GET", "/v1/sessions/bad..id/replay", Body::empty())
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        call(&srv, "GET", "/v1/sessions/sess-1/replay?kind=invalid", Body::empty())
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        call(&srv, "GET", "/v1/sessions/sess-1/replay?include_frames=maybe", Body::empty())
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        call(&srv, "GET", "/v1/sessions/sess-1/network?format=xml", Body::empty())
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        call(&srv, "GET", "/v1/sessions/sess-1/network?api_only=maybe", Body::empty())
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        call(&srv, "GET", "/v1/sessions/sess-1/replay", Body::empty())
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(&srv, "GET", "/v1/sessions/sess-1/network", Body::empty())
            .await
            .status(),
        StatusCode::NOT_FOUND
    );

    // 3. BYOB cdp_endpoint fails closed with 403 byob_disabled when opt-in flag is off.
    let byob_off = call(
        &srv,
        "POST",
        "/v1/sessions",
        r#"{"cdp_endpoint":"ws://127.0.0.1:9222/devtools/browser/1"}"#,
    )
    .await;
    assert_eq!(byob_off.status(), StatusCode::FORBIDDEN);
    assert_eq!(body(byob_off).await["error"]["code"], "byob_disabled");

    // 4. /v1/map bounds & SSRF validation.
    for payload in [
        r#"{"url":"https://example.com","limit":0}"#,
        r#"{"url":"https://example.com","limit":1001}"#,
        r#"{"url":"https://example.com","timeout_ms":0}"#,
        r#"{"url":"https://example.com","timeout_ms":120001}"#,
        r#"{"url":"http://169.254.169.254/latest/meta-data"}"#,
        r#"{"url":"file:///etc/passwd"}"#,
    ] {
        assert_eq!(
            call(&srv, "POST", "/v1/map", payload).await.status(),
            StatusCode::BAD_REQUEST,
            "{payload}"
        );
    }
    assert_eq!(
        call(
            &srv,
            "POST",
            "/v1/map",
            r#"{"url":"http://169.254.169.254/latest/meta-data","block_cookie_banners":true}"#
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        call(&srv, "POST", "/v1/map", r#"{"url":"https://example.com","unknown":1}"#)
            .await
            .status(),
        StatusCode::UNPROCESSABLE_ENTITY
    );
    let oversized_map = format!(
        r#"{{"url":"https://example.com/{}"}}"#,
        "a".repeat(MAX_REQUEST_BYTES + 128)
    );
    assert_eq!(
        call(&srv, "POST", "/v1/map", oversized_map).await.status(),
        StatusCode::PAYLOAD_TOO_LARGE
    );

    // Dashboard HTML exposes Managed Sessions, Replay, and Network modals.
    let dash = srv
        .router()
        .oneshot(Request::builder().uri("/dashboard").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let dash_html = String::from_utf8(
        to_bytes(dash.into_body(), 1024 * 1024).await.unwrap().to_vec(),
    )
    .unwrap();
    assert!(dash_html.contains("Managed Sessions"));
    assert!(dash_html.contains("replay-dialog"));
    assert!(dash_html.contains("network-dialog"));

    srv.shutdown(Duration::ZERO).await.unwrap();

    // 5. BYOB SSRF validation when allow_session_cdp_endpoint = true.
    let eng2 = idle_engine().await;
    let mut byob_cfg = ServerConfig::new(TOKEN.into()).unwrap();
    byob_cfg.allow_session_cdp_endpoint = true;
    let byob_srv = HttpServer::new(eng2, byob_cfg).unwrap();

    for bad_ep in [
        "http://169.254.169.254/json/version",
        "ws://169.254.169.254:9222/devtools/browser/1",
        "ws://2852039166:9222/devtools/browser/1",
        "ws://10.0.0.1:9222/devtools/browser/1",
        "ws://172.16.0.1:9222/devtools/browser/1",
        "ws://192.168.1.10:9222/devtools/browser/1",
        "ws://[fe80::1]:9222/devtools/browser/1",
        "ws://[fd00:ec2::254]:9222/devtools/browser/1",
        "ws://[::ffff:169.254.169.254]:9222/devtools/browser/1",
        "http://metadata.google.internal:9222",
        "ftp://127.0.0.1:9222",
        "ws://user@127.0.0.1:9222",
    ] {
        let payload = json!({ "cdp_endpoint": bad_ep }).to_string();
        let resp = call(&byob_srv, "POST", "/v1/sessions", payload).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{bad_ep}");
    }
    byob_srv.shutdown(Duration::ZERO).await.unwrap();

    // Loopback endpoints pass syntax/SSRF validation (for local stealth browsers).
    for ok_ep in [
        "ws://127.0.0.1:9222/devtools/browser/1",
        "http://localhost:9222",
        "ws://[::1]:9222/devtools/browser/1",
        "https://cdp.example.com:9443/devtools/browser/1",
    ] {
        assert!(
            sessions::validate_cdp_endpoint_syntax(ok_ep, false).is_ok(),
            "{ok_ep}"
        );
    }
    assert!(sessions::validate_cdp_endpoint_syntax("ws://10.0.0.1:9222", false).is_err());
    assert!(sessions::validate_cdp_endpoint_syntax("ws://10.0.0.1:9222", true).is_ok());
}

#[test]
fn replay_har_redaction_bounds_and_map_filter_unit() {
    use crate::replay::{
        RetainedReplay, RetainedReplayStore, SessionRecorder, MAX_FRAME_BYTES, MAX_NETWORK_ENTRIES,
        MAX_RETAINED_FRAMES, MAX_RETAINED_REPLAYS, MAX_TIMELINE_EVENTS, RETAINED_REPLAY_TTL_MS,
    };

    let mut rec = SessionRecorder::default();
    rec.push_nav(1_000, "https://app.example.com/login");
    rec.record_cdp_event(
        1_010,
        "Runtime.consoleAPICalled",
        &json!({
            "type": "error",
            "args": [{ "value": "login" }, { "value": "failed" }]
        }),
        "https://app.example.com/login",
    );
    rec.record_cdp_event(
        1_020,
        "Network.requestWillBeSent",
        &json!({
            "requestId": "r1",
            "type": "Fetch",
            "request": {
                "url": "https://app.example.com/api/auth?scope=read",
                "method": "POST",
                "headers": {
                    "Authorization": "Bearer secret-jwt",
                    "Cookie": "sid=abc",
                    "X-Api-Key": "key-999",
                    "Proxy-Authorization": "Basic xyz",
                    "Content-Type": "application/json"
                },
                "postData": "{\"user\":\"ada\"}"
            }
        }),
        "https://app.example.com/login",
    );
    rec.record_cdp_event(
        1_030,
        "Network.responseReceived",
        &json!({
            "requestId": "r1",
            "response": {
                "status": 401,
                "statusText": "Unauthorized",
                "mimeType": "application/json",
                "headers": {
                    "Set-Cookie": "sess=bad; HttpOnly",
                    "X-Trace": "t1"
                }
            }
        }),
        "https://app.example.com/login",
    );
    rec.record_cdp_event(
        1_040,
        "Network.requestWillBeSent",
        &json!({
            "requestId": "r2",
            "type": "Document",
            "request": {
                "url": "https://app.example.com/home",
                "method": "GET",
                "headers": {}
            }
        }),
        "https://app.example.com/home",
    );
    rec.record_cdp_event(
        1_050,
        "Network.responseReceived",
        &json!({
            "requestId": "r2",
            "response": {
                "status": 200,
                "statusText": "OK",
                "mimeType": "text/html",
                "headers": {}
            }
        }),
        "https://app.example.com/home",
    );
    rec.push_frame(1_060, "https://app.example.com/home", "/9j/FAKEJPEG".into());

    // Searchable replay queries.
    let (total, matched) = rec.query_replay(Some("login failed"), Some("console"), true);
    assert_eq!(total, 4); // nav + console + network(401) + frame
    assert_eq!(matched.len(), 1);
    assert_eq!(matched[0].level, "error");

    let (_, no_frames) = rec.query_replay(None, Some("frame"), false);
    assert_eq!(no_frames.len(), 1);
    assert!(no_frames[0].frame_base64.is_none());

    // Sensitive header redaction & api_only filter.
    let (total_net, api_redacted) = rec.export_network_entries(None, true, false);
    assert_eq!(total_net, 2);
    assert_eq!(api_redacted.len(), 1);
    for (k, v) in &api_redacted[0].request_headers {
        if matches!(
            k.to_ascii_lowercase().as_str(),
            "authorization" | "cookie" | "x-api-key" | "proxy-authorization"
        ) {
            assert_eq!(v, "[REDACTED]", "{k}");
        }
    }
    assert_eq!(
        api_redacted[0]
            .response_headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("set-cookie"))
            .map(|(_, v)| v.as_str()),
        Some("[REDACTED]")
    );

    // include_auth=true preserves raw secrets.
    let (_, api_raw) = rec.export_network_entries(Some("/api/auth"), true, true);
    assert_eq!(api_raw.len(), 1);
    assert_eq!(
        api_raw[0]
            .request_headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("authorization"))
            .map(|(_, v)| v.as_str()),
        Some("Bearer secret-jwt")
    );

    // HAR 1.2 export & roundtrip parse.
    let har = rec.export_har(None, true, false);
    let har_json = serde_json::to_string(&har).unwrap();
    let parsed_har = fluxwright::Har::from_json_str(&har_json).unwrap();
    assert_eq!(parsed_har.log.version, "1.2");
    assert_eq!(parsed_har.log.creator.name, "fluxwright");
    assert_eq!(parsed_har.log.entries.len(), 1);
    assert_eq!(parsed_har.log.entries[0].response.status, 401);
    assert!(parsed_har.log.entries[0]
        .request
        .headers
        .iter()
        .any(|h| h.name.eq_ignore_ascii_case("authorization") && h.value == "[REDACTED]"));

    // Frame count & byte caps + timeline/network bounds.
    let mut bounded = SessionRecorder::default();
    for i in 0..(MAX_TIMELINE_EVENTS + 30) {
        bounded.push_nav(i as u64, &format!("https://example.com/{i}"));
    }
    assert_eq!(bounded.timeline.len(), MAX_TIMELINE_EVENTS);

    for i in 0..(MAX_RETAINED_FRAMES + 10) {
        bounded.push_frame(i as u64, "https://example.com", "A".repeat(100));
    }
    assert_eq!(bounded.frame_count, MAX_RETAINED_FRAMES);
    assert_eq!(bounded.frame_bytes, MAX_RETAINED_FRAMES * 100);

    // Large frames stay within MAX_FRAME_BYTES (2 MiB).
    let mut byte_capped = SessionRecorder::default();
    for i in 0..5 {
        byte_capped.push_frame(i as u64, "https://example.com", "B".repeat(700_000));
    }
    assert!(byte_capped.frame_bytes <= MAX_FRAME_BYTES);
    assert_eq!(byte_capped.frame_count, 2);

    for i in 0..(MAX_NETWORK_ENTRIES + 20) {
        bounded.record_cdp_event(
            i as u64,
            "Network.requestWillBeSent",
            &json!({
                "requestId": format!("r{i}"),
                "type": "XHR",
                "request": { "url": format!("https://example.com/api/{i}"), "method": "GET", "headers": {} }
            }),
            "https://example.com",
        );
    }
    assert_eq!(bounded.network.len(), MAX_NETWORK_ENTRIES);

    // RetainedReplayStore capacity (32) and TTL (900_000 ms).
    let mut store = RetainedReplayStore::default();
    for i in 0..(MAX_RETAINED_REPLAYS + 5) {
        store.push(
            RetainedReplay {
                id: format!("sess-{i}"),
                created_at: 100,
                closed_at: 1_000 + i as u64,
                url: "https://example.com".into(),
                keep_alive: true,
                profile: None,
                proxy_session: None,
                byob: false,
                record_frames: false,
                recorder: SessionRecorder::default(),
            },
            2_000,
        );
    }
    assert!(store.with_entry("sess-0", 2_000, |_| ()).is_none());
    assert!(store.with_entry("sess-36", 2_000, |_| ()).is_some());
    assert!(store
        .with_entry("sess-36", 1_036 + RETAINED_REPLAY_TTL_MS, |_| ())
        .is_none());

    // /v1/map link filtering (same_origin, include_subdomains, search, limit, dedup).
    let raw = vec![
        json!("https://example.com/docs/intro"),
        json!("https://example.com/docs/intro"),
        json!("https://docs.example.com/api"),
        json!("https://notexample.com/docs"),
        json!("https://example.com/about"),
        json!("mailto:hello@example.com"),
    ];
    assert_eq!(
        map::filter_links(&raw, "https://example.com/", true, false, None, 10),
        vec![
            "https://example.com/docs/intro",
            "https://example.com/about",
        ]
    );
    assert_eq!(
        map::filter_links(&raw, "https://example.com/", true, true, Some("DOCS"), 10),
        vec![
            "https://example.com/docs/intro",
            "https://docs.example.com/api",
        ]
    );
    assert_eq!(
        map::filter_links(&raw, "https://example.com/", false, false, None, 2),
        vec![
            "https://example.com/docs/intro",
            "https://docs.example.com/api",
        ]
    );
}

#[test]
fn fast_html_markdown_links_and_sitemap_extraction() {
    let html = r#"<!doctype html>
<html>
  <head>
    <title>FastPath Guide</title>
    <base href="https://example.com/docs/">
    <style>.hidden { display: none; }</style>
    <script>console.log("ignored script text");</script>
  </head>
  <body>
    <noscript>ignored noscript</noscript>
    <template><p>ignored template</p></template>
    <svg><title>svg title</title><text>ignored svg</text></svg>
    <iframe src="/frame">ignored iframe</iframe>
    <div hidden>hidden div</div>
    <div aria-hidden="true">aria hidden</div>
    <div style="display: none">inline hidden</div>
    <h1>Welcome</h1>
    <h2>Details</h2>
    <p>Visit <a href="getting-started">Getting <strong>Started</strong></a> and <em>enjoy</em> <code>fast()</code>.</p>
    <p><a href="javascript:void(0)">Noop</a> and <a href="https://other.test/raw"></a></p>
    <ul>
      <li>Alpha<ul><li>Nested</li></ul></li>
      <li>Beta</li>
    </ul>
    <ol start="3">
      <li>Third</li>
      <li>Fourth</li>
    </ol>
    <pre><code>let x = 1;
let y = 2;</code></pre>
    <blockquote><p>Quoted line</p></blockquote>
    <hr>
    <table>
      <tr><th>Col | A</th><th>Col \ B</th></tr>
      <tr><td>val1</td></tr>
    </table>
    <a href="mailto:hi@example.com">Mail</a>
  </body>
</html>"#;

    let extracted = fast_html::extract_html(html, "https://example.com/blog/post");
    assert_eq!(extracted.title, "FastPath Guide");

    let md = &extracted.markdown;
    assert!(!md.contains("ignored script"));
    assert!(!md.contains("ignored noscript"));
    assert!(!md.contains("ignored template"));
    assert!(!md.contains("ignored svg"));
    assert!(!md.contains("ignored iframe"));
    assert!(!md.contains("hidden div"));
    assert!(!md.contains("aria hidden"));
    assert!(!md.contains("inline hidden"));
    assert!(md.contains("# Welcome"));
    assert!(md.contains("## Details"));
    assert!(md.contains("[Getting **Started**](https://example.com/docs/getting-started)"));
    assert!(md.contains("*enjoy*"));
    assert!(md.contains("`fast()`"));
    assert!(md.contains("Noop and https://other.test/raw"));
    assert!(md.contains("- Alpha"));
    assert!(md.contains("  - Nested"));
    assert!(md.contains("3. Third"));
    assert!(md.contains("4. Fourth"));
    assert!(md.contains("```\nlet x = 1;\nlet y = 2;\n```"));
    assert!(md.contains("> Quoted line"));
    assert!(md.contains("| Col \\| A | Col \\\\ B |"));
    assert!(md.contains("| val1 |  |"));

    assert!(!extracted.text.contains("ignored script"));
    assert!(extracted.text.contains("Welcome"));
    assert!(extracted.text.contains("Getting Started"));

    assert_eq!(
        extracted.links,
        vec![
            "https://example.com/docs/getting-started",
            "https://other.test/raw",
        ]
    );

    let sitemap_xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<!-- comment with <loc>https://ignored.example/x</loc> -->
<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">
  <url><loc>https://example.com/a?x=1&amp;y=2</loc></url>
  <url><loc><![CDATA[https://example.com/cdata]]></loc></url>
  <url><loc>ftp://example.com/ignored</loc></url>
  <url><loc>https://example.com/a?x=1&amp;y=2</loc></url>
</urlset>"#;
    assert_eq!(
        fast_html::parse_sitemap_locs(sitemap_xml),
        vec![
            "https://example.com/a?x=1&y=2",
            "https://example.com/cdata",
        ]
    );

    let scoped_md = fast_html::html_to_markdown(
        "<header>Nav</header><main><h1>Core</h1><img src='/pic.png' alt='Logo'><a href='/x'>X</a></main>",
        "https://example.com/",
        true,
        false,
        true,
    );
    assert!(!scoped_md.contains("Nav"));
    assert!(scoped_md.contains("# Core"));
    assert!(scoped_md.contains("![Logo](https://example.com/pic.png)"));
    assert!(!scoped_md.contains("https://example.com/x"));
}

#[test]
fn fast_html_escalation_detection_covers_status_markers_and_spa() {
    let ok_headers: Vec<(String, String)> = Vec::new();
    let ok_html = "<!doctype html><title>Static</title><p>Hello world</p>";

    for status in [401, 403, 429, 500, 502, 503] {
        let reason = fast_html::detect_escalation(status, &ok_headers, Some("text/html"), ok_html);
        assert!(reason.is_some(), "expected escalation for status {status}");
    }
    assert!(fast_html::detect_escalation(200, &ok_headers, Some("text/html"), ok_html).is_none());
    assert!(fast_html::detect_escalation(404, &ok_headers, Some("text/html"), ok_html).is_none());

    // Non-HTML Content-Type escalates.
    assert!(
        fast_html::detect_escalation(200, &ok_headers, Some("application/json"), r#"{"ok":true}"#)
            .is_some()
    );
    assert!(
        fast_html::detect_escalation(200, &ok_headers, Some("application/pdf"), "%PDF-1.7")
            .is_some()
    );

    // Challenge headers and markers escalate.
    let cf_headers = vec![("CF-Mitigated".to_string(), "challenge".to_string())];
    assert!(
        fast_html::detect_escalation(200, &cf_headers, Some("text/html"), ok_html)
            .unwrap()
            .contains("cf-mitigated")
    );
    for marker_html in [
        "<html><title>Just a moment...</title></html>",
        "<html><script src='/cdn-cgi/challenge-platform/h/b/orchestrate'></script></html>",
        "<html><iframe src='/_Incapsula_Resource?SWJIYLWA=1'></iframe></html>",
        "<html><body>Protected by DDoS-Guard</body></html>",
    ] {
        assert!(
            fast_html::detect_escalation(200, &ok_headers, Some("text/html"), marker_html)
                .is_some(),
            "expected challenge escalation for {marker_html}"
        );
    }

    // Unhydrated SPA shells escalate; hydrated SSR pages with >=200 chars text do not.
    for spa_html in [
        "<!doctype html><body><div id='root'></div><script src='/main.js'></script></body>",
        "<!doctype html><body><div id='app'>Loading...</div><script src='/app.js'></script></body>",
        "<!doctype html><body><div id='__next'></div><script src='/_next/static/app.js'></script></body>",
        "<!doctype html><body><div id='__nuxt'></div><script src='/_nuxt/entry.js'></script></body>",
        "<!doctype html><body><noscript>You need to enable JavaScript to run this app.</noscript></body>",
    ] {
        assert!(
            fast_html::detect_escalation(200, &ok_headers, Some("text/html"), spa_html).is_some(),
            "expected SPA escalation for {spa_html}"
        );
    }

    let hydrated_ssr = format!(
        "<!doctype html><body><div id='__next'><p>{}</p></div><script src='/app.js'></script></body>",
        "SSR content word ".repeat(20)
    );
    assert!(
        fast_html::detect_escalation(200, &ok_headers, Some("text/html"), &hydrated_ssr).is_none()
    );

    // Benign tracking pixel inside <noscript> or non-SPA minimal <nav> with <script> must not escalate.
    let benign_html = "<!doctype html><body><noscript><img src='/p.gif'></noscript><nav><a href='/a'>Link</a></nav><script src='/analytics.js'></script></body>";
    assert!(
        fast_html::detect_escalation(200, &ok_headers, Some("text/html"), benign_html).is_none()
    );
}

#[test]
fn fetch_mode_and_request_deserialization() {
    use fast_fetch::FetchMode;

    assert_eq!(
        serde_json::from_str::<FetchMode>(r#""auto""#).unwrap(),
        FetchMode::Auto
    );
    assert_eq!(
        serde_json::from_str::<FetchMode>(r#""http""#).unwrap(),
        FetchMode::Http
    );
    assert_eq!(
        serde_json::from_str::<FetchMode>(r#""browser""#).unwrap(),
        FetchMode::Browser
    );
    assert!(serde_json::from_str::<FetchMode>(r#""curl""#).is_err());

    let scrape_default: scrape::ScrapeRequest =
        serde_json::from_str(r#"{"url":"https://example.com"}"#).unwrap();
    assert_eq!(scrape_default.mode, FetchMode::Auto);

    let scrape_http: scrape::ScrapeRequest =
        serde_json::from_str(r#"{"url":"https://example.com","mode":"http"}"#).unwrap();
    assert_eq!(scrape_http.mode, FetchMode::Http);

    let map_default: map::MapRequest =
        serde_json::from_str(r#"{"url":"https://example.com"}"#).unwrap();
    assert_eq!(map_default.mode, FetchMode::Auto);
    assert!(!map_default.include_sitemap);

    let map_opts: map::MapRequest = serde_json::from_str(
        r#"{"url":"https://example.com","mode":"browser","include_sitemap":true}"#,
    )
    .unwrap();
    assert_eq!(map_opts.mode, FetchMode::Browser);
    assert!(map_opts.include_sitemap);
}

#[tokio::test]
async fn fast_fetch_blocks_ssrf_redirects_and_decodes_compressed_bodies() {
    use std::io::Write as _;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let base = format!("http://127.0.0.1:{port}");

    let srv = tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                let target = req
                    .lines()
                    .next()
                    .and_then(|l| l.split_whitespace().nth(1))
                    .unwrap_or("/");

                let response: Vec<u8> = match target {
                    "/redir-imds" => b"HTTP/1.1 302 Found\r\nLocation: http://169.254.169.254/latest/meta-data\r\nContent-Length: 0\r\n\r\n".to_vec(),
                    "/redir-fe80" => b"HTTP/1.1 302 Found\r\nLocation: http://[fe80::1]/\r\nContent-Length: 0\r\n\r\n".to_vec(),
                    "/redir-fd00" => b"HTTP/1.1 302 Found\r\nLocation: http://[fd00:ec2::254]/\r\nContent-Length: 0\r\n\r\n".to_vec(),
                    "/redir-v4mapped" => b"HTTP/1.1 302 Found\r\nLocation: http://[::ffff:169.254.169.254]/\r\nContent-Length: 0\r\n\r\n".to_vec(),
                    "/redir-dec" => b"HTTP/1.1 302 Found\r\nLocation: http://2852039166/\r\nContent-Length: 0\r\n\r\n".to_vec(),
                    "/redir-hex" => b"HTTP/1.1 302 Found\r\nLocation: http://0xa9.0xfe.0xa9.0xfe/\r\nContent-Length: 0\r\n\r\n".to_vec(),
                    "/redir-file" => b"HTTP/1.1 302 Found\r\nLocation: file:///etc/passwd\r\nContent-Length: 0\r\n\r\n".to_vec(),
                    "/redir-loop" => b"HTTP/1.1 302 Found\r\nLocation: /redir-loop\r\nContent-Length: 0\r\n\r\n".to_vec(),
                    "/redir-ok" => b"HTTP/1.1 302 Found\r\nLocation: /gzip\r\nContent-Length: 0\r\n\r\n".to_vec(),
                    "/gzip" => {
                        let mut enc = flate2::write::GzEncoder::new(
                            Vec::new(),
                            flate2::Compression::default(),
                        );
                        enc.write_all(b"<!doctype html><title>Gz</title><p>gzipped</p>")
                            .unwrap();
                        let gz = enc.finish().unwrap();
                        let mut chunked = Vec::new();
                        chunked.extend_from_slice(
                            format!("{:x}\r\n", gz.len()).as_bytes(),
                        );
                        chunked.extend_from_slice(&gz);
                        chunked.extend_from_slice(b"\r\n0\r\n\r\n");
                        let mut out = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Encoding: gzip\r\nTransfer-Encoding: chunked\r\n\r\n"
                        )
                        .into_bytes();
                        out.extend_from_slice(&chunked);
                        out
                    }
                    "/deflate" => {
                        let mut enc = flate2::write::ZlibEncoder::new(
                            Vec::new(),
                            flate2::Compression::default(),
                        );
                        enc.write_all(b"<p>deflated</p>").unwrap();
                        let z = enc.finish().unwrap();
                        let mut out = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Encoding: deflate\r\nContent-Length: {}\r\n\r\n",
                            z.len()
                        )
                        .into_bytes();
                        out.extend_from_slice(&z);
                        out
                    }
                    "/too-big" => format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\n\r\n",
                        fast_fetch::MAX_FAST_FETCH_BODY_BYTES + 1
                    )
                    .into_bytes(),
                    _ => b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: 10\r\n\r\n<p>ok</p>!".to_vec(),
                };
                let _ = sock.write_all(&response).await;
                let _ = sock.shutdown().await;
            });
        }
    });

    // SSRF redirects blocked before connecting across IPv4/IPv6 link-local, AWS IMDS, decimal/hex IPv4, and file://.
    for path in [
        "/redir-imds",
        "/redir-fe80",
        "/redir-fd00",
        "/redir-v4mapped",
        "/redir-dec",
        "/redir-hex",
        "/redir-file",
    ] {
        let err = fast_fetch::fetch_url(
            &format!("{base}{path}"),
            Duration::from_secs(5),
            false,
            None,
            None,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, fast_fetch::FastFetchError::InvalidOrBlocked(_)),
            "expected InvalidOrBlocked for {path}, got {err:?}"
        );
    }

    // Redirect loop > 10 hops fails cleanly.
    let loop_err = fast_fetch::fetch_url(
        &format!("{base}/redir-loop"),
        Duration::from_secs(5),
        false,
        None,
        None,
    )
    .await
    .unwrap_err();
    assert!(matches!(loop_err, fast_fetch::FastFetchError::Transport(_)));

    // 302 -> /gzip follows redirect and decodes chunked gzip body.
    let gz_resp = fast_fetch::fetch_url(
        &format!("{base}/redir-ok"),
        Duration::from_secs(5),
        false,
        None,
        None,
    )
    .await
    .unwrap();
    assert_eq!(gz_resp.final_url, format!("{base}/gzip"));
    assert_eq!(
        String::from_utf8_lossy(&gz_resp.body),
        "<!doctype html><title>Gz</title><p>gzipped</p>"
    );

    // Deflate decoding.
    let def_resp = fast_fetch::fetch_url(
        &format!("{base}/deflate"),
        Duration::from_secs(5),
        false,
        None,
        None,
    )
    .await
    .unwrap();
    assert_eq!(String::from_utf8_lossy(&def_resp.body), "<p>deflated</p>");

    // 10 MiB body cap enforced before reading oversized payload and on decompressed gzip zip-bombs.
    let big_err = fast_fetch::fetch_url(
        &format!("{base}/too-big"),
        Duration::from_secs(5),
        false,
        None,
        None,
    )
    .await
    .unwrap_err();
    assert!(matches!(big_err, fast_fetch::FastFetchError::Transport(_)));

    let mut bomb_enc =
        flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    bomb_enc
        .write_all(&vec![b'A'; fast_fetch::MAX_FAST_FETCH_BODY_BYTES + 1024])
        .unwrap();
    let bomb_gz = bomb_enc.finish().unwrap();
    assert!(bomb_gz.len() < 128 * 1024);
    let bomb_err = fast_fetch::decode_content_encoding(bomb_gz, Some("gzip")).unwrap_err();
    assert!(matches!(bomb_err, fast_fetch::FastFetchError::Transport(_)));

    // HTTP proxy pool integration (absolute-form request with Basic Proxy-Authorization).
    let proxy_seen = Arc::new(Mutex::new(String::new()));
    let proxy_seen_srv = proxy_seen.clone();
    let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_port = proxy_listener.local_addr().unwrap().port();
    let proxy_task = tokio::spawn(async move {
        if let Ok((mut sock, _)) = proxy_listener.accept().await {
            let mut buf = [0u8; 4096];
            let n = sock.read(&mut buf).await.unwrap_or(0);
            *proxy_seen_srv.lock().unwrap() = String::from_utf8_lossy(&buf[..n]).into_owned();
            let body = "<!doctype html><title>ViaProxy</title><p>proxied</p>";
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = sock.write_all(resp.as_bytes()).await;
            let _ = sock.shutdown().await;
        }
    });
    let pool = fluxwright::ProxyPool::from_urls([format!("http://alice:s3cr3t@127.0.0.1:{proxy_port}")])
        .unwrap();
    let proxied = fast_fetch::fetch_url(
        "http://example.invalid/page",
        Duration::from_secs(5),
        false,
        Some(&pool),
        Some("sess-1"),
    )
    .await
    .unwrap();
    assert!(String::from_utf8_lossy(&proxied.body).contains("ViaProxy"));
    let raw_proxy_req = proxy_seen.lock().unwrap().clone();
    assert!(raw_proxy_req.starts_with("GET http://example.invalid/page HTTP/1.1\r\n"));
    assert!(raw_proxy_req.contains("Proxy-Authorization: Basic "));

    // mode: "http" succeeds on idle_engine (without Chromium) and updates Prometheus fastpath hits.
    let eng = idle_engine().await;
    let http_srv = HttpServer::new(eng, ServerConfig::new(TOKEN.into()).unwrap()).unwrap();
    let scrape_resp = call(
        &http_srv,
        "POST",
        "/v1/scrape",
        json!({ "url": format!("{base}/gzip"), "mode": "http", "formats": ["markdown", "text"] })
            .to_string(),
    )
    .await;
    assert_eq!(scrape_resp.status(), StatusCode::OK);
    let scrape_json = body(scrape_resp).await;
    assert_eq!(scrape_json["engine"], "http");
    assert_eq!(scrape_json["title"], "Gz");
    assert_eq!(scrape_json["markdown"], "gzipped");
    assert_eq!(scrape_json["text"], "gzipped");

    // mode: "http" rejects visual format with 400 and rejects SSRF redirect with 400.
    let bad_vis = call(
        &http_srv,
        "POST",
        "/v1/scrape",
        json!({ "url": format!("{base}/gzip"), "mode": "http", "formats": ["screenshot"] })
            .to_string(),
    )
    .await;
    assert_eq!(bad_vis.status(), StatusCode::BAD_REQUEST);

    let bad_ssrf = call(
        &http_srv,
        "POST",
        "/v1/scrape",
        json!({ "url": format!("{base}/redir-imds"), "mode": "http" }).to_string(),
    )
    .await;
    assert_eq!(bad_ssrf.status(), StatusCode::BAD_REQUEST);

    let metrics_text = String::from_utf8(
        to_bytes(call(&http_srv, "GET", "/metrics", Body::empty()).await.into_body(), 1024 * 1024)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(metrics_text.contains("fluxwright_fastpath_hits_total 1\n"));
    assert!(metrics_text.contains("fluxwright_fastpath_fallbacks_total 0\n"));

    http_srv.shutdown(Duration::ZERO).await.unwrap();
    proxy_task.abort();
    srv.abort();
}

#[tokio::test]
async fn with_fast_cdp_endpoint_validates_and_wires_config() {
    let cfg = ServerConfig::new(TOKEN.into())
        .unwrap()
        .with_fast_cdp_endpoint("WS://127.0.0.1:9222/devtools/browser/1".into())
        .expect("valid ws endpoint");
    let srv = HttpServer::new(idle_engine().await, cfg).unwrap();
    assert_eq!(
        srv.inner.fast_cdp_endpoint.as_deref(),
        Some("ws://127.0.0.1:9222/devtools/browser/1")
    );
    srv.shutdown(Duration::ZERO).await.unwrap();

    for ok in [
        "wss://cdp.example.com:9443/ws",
        "http://localhost:9222",
        "https://[::1]:9222/json/version",
    ] {
        assert!(
            ServerConfig::new(TOKEN.into())
                .unwrap()
                .with_fast_cdp_endpoint(ok.into())
                .is_ok(),
            "expected valid fast-CDP endpoint: {ok}"
        );
    }

    for bad in [
        "",
        "ftp://127.0.0.1:9222",
        "127.0.0.1:9222",
        "ws://",
        "ws://user:pass@127.0.0.1:9222",
        "ws://127.0.0.1:0",
        "ws://127.0.0.1:abc",
        "ws://[::1",
        "ws://127.0.0.1:9222 bad",
        "ws://127.0.0.1\\9222",
    ] {
        let err = match ServerConfig::new(TOKEN.into())
            .unwrap()
            .with_fast_cdp_endpoint(bad.into())
        {
            Ok(_) => panic!("expected invalid fast-CDP endpoint to fail: {bad:?}"),
            Err(e) => e,
        };
        assert!(
            matches!(err, ConfigError::InvalidFastCdpEndpoint(_)),
            "unexpected error for {bad:?}: {err}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn fast_cdp_tier_handles_escalated_scrape_and_map_with_32601_fallback() {
    use futures_util::{SinkExt, StreamExt};
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio_tungstenite::tungstenite::Message;

    let http_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http_port = http_listener.local_addr().unwrap().port();
    let base = format!("http://127.0.0.1:{http_port}");
    let http_srv_task = tokio::spawn(async move {
        while let Ok((mut sock, _)) = http_listener.accept().await {
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                let target = req
                    .lines()
                    .next()
                    .and_then(|l| l.split_whitespace().nth(1))
                    .unwrap_or("/");
                let body = match target {
                    "/map-spa" => {
                        "<!doctype html><title>MapShell</title><body><div id='app'></div><script>app()</script></body>"
                    }
                    _ => {
                        "<!doctype html><title>Shell</title><body><div id='root'></div><script>hydrate()</script></body>"
                    }
                };
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.shutdown().await;
            });
        }
    });

    let cdp_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let cdp_addr = cdp_listener.local_addr().unwrap();
    let closed_targets = Arc::new(AtomicUsize::new(0));
    let closed_targets_srv = closed_targets.clone();
    let base_for_cdp = base.clone();

    let cdp_task = tokio::spawn(async move {
        while let Ok((stream, _)) = cdp_listener.accept().await {
            let closed_targets_conn = closed_targets_srv.clone();
            let base_conn = base_for_cdp.clone();
            tokio::spawn(async move {
                let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else {
                    return;
                };
                let mut target_seq = 0usize;
                let mut current_url = String::from("about:blank");
                while let Some(Ok(Message::Text(text))) = ws.next().await {
                    let req: Value = serde_json::from_str(&text).unwrap();
                    let id = req["id"].clone();
                    let method = req["method"].as_str().unwrap_or_default();
                    let reply = match method {
                        "Browser.getVersion" => json!({
                            "id": id,
                            "result": { "product": "Lightpanda/0.1.0", "protocolVersion": "1.3" }
                        }),
                        "Target.createBrowserContext"
                        | "Target.disposeBrowserContext"
                        | "Page.setLifecycleEventsEnabled"
                        | "Fetch.enable"
                        | "Emulation.setDeviceMetricsOverride"
                        | "Network.enable"
                        | "Network.setBlockedURLs" => json!({
                            "id": id,
                            "error": { "code": -32601, "message": "Method not found" }
                        }),
                        "Target.createTarget" => {
                            target_seq += 1;
                            let tid = format!("T-FAST-{target_seq}");
                            let sid = format!("S-FAST-{target_seq}");
                            let ev = json!({
                                "method": "Target.attachedToTarget",
                                "params": {
                                    "sessionId": sid,
                                    "waitingForDebugger": false,
                                    "targetInfo": {
                                        "targetId": tid,
                                        "type": "page",
                                        "url": "about:blank"
                                    }
                                }
                            });
                            if ws.send(Message::Text(ev.to_string().into())).await.is_err() {
                                break;
                            }
                            json!({ "id": id, "result": { "targetId": tid } })
                        }
                        "Target.closeTarget" => {
                            closed_targets_conn.fetch_add(1, Ordering::SeqCst);
                            json!({ "id": id, "result": { "success": true } })
                        }
                        "Page.navigate" => {
                            let url = req["params"]["url"].as_str().unwrap_or("about:blank");
                            current_url = url.to_string();
                            let tid = format!("T-FAST-{target_seq}");
                            let sid = format!("S-FAST-{target_seq}");
                            let ev = json!({
                                "method": "Page.frameNavigated",
                                "sessionId": sid,
                                "params": { "frame": { "id": tid, "url": url } }
                            });
                            if ws.send(Message::Text(ev.to_string().into())).await.is_err() {
                                break;
                            }
                            json!({ "id": id, "result": { "frameId": tid } })
                        }
                        "Runtime.evaluate" => {
                            let expr = req["params"]["expression"].as_str().unwrap_or("");
                            let val = if expr == "location.href" {
                                if current_url.ends_with("/ssrf-sim") {
                                    json!("http://169.254.169.254/latest/meta-data")
                                } else {
                                    json!(current_url)
                                }
                            } else if expr == "document.title" {
                                json!("LightCDP Hydrated")
                            } else if expr == "document.documentElement.outerHTML" {
                                json!("<html><body><h1>LightCDP</h1></body></html>")
                            } else if expr.contains("innerText") {
                                json!("LightCDP hydrated text")
                            } else if expr.contains("SKIP_TAGS") {
                                json!("# LightCDP\n\nHydrated by external fast-CDP")
                            } else if expr.contains("querySelectorAll('a[href]')") {
                                json!([
                                    format!("{base_conn}/spa-link"),
                                    "https://external.example/skip"
                                ])
                            } else {
                                json!("complete")
                            };
                            json!({
                                "id": id,
                                "result": { "result": { "type": "object", "value": val } }
                            })
                        }
                        _ => json!({ "id": id, "result": {} }),
                    };
                    if ws.send(Message::Text(reply.to_string().into())).await.is_err() {
                        break;
                    }
                }
            });
        }
    });

    // Uses idle_engine() (NeverStarts): passing proves Tier 2 fast-CDP handled the escalated requests
    // without touching local Chromium.
    let cfg = ServerConfig::new(TOKEN.into())
        .unwrap()
        .with_fast_cdp_endpoint(format!("ws://{cdp_addr}"))
        .unwrap();
    let srv = HttpServer::new(idle_engine().await, cfg).unwrap();

    let scrape_resp = call(
        &srv,
        "POST",
        "/v1/scrape",
        json!({
            "url": format!("{base}/spa"),
            "formats": ["markdown", "html", "text"]
        })
        .to_string(),
    )
    .await;
    assert_eq!(scrape_resp.status(), StatusCode::OK);
    let scrape_json = body(scrape_resp).await;
    assert_eq!(scrape_json["engine"], "browser");
    assert!(scrape_json["fallback_reason"]
        .as_str()
        .unwrap_or("")
        .contains("SPA shell"));
    assert_eq!(scrape_json["title"], "LightCDP Hydrated");
    assert_eq!(
        scrape_json["markdown"],
        "# LightCDP\n\nHydrated by external fast-CDP"
    );
    assert_eq!(
        scrape_json["html"],
        "<html><body><h1>LightCDP</h1></body></html>"
    );
    assert_eq!(scrape_json["text"], "LightCDP hydrated text");

    let map_resp = call(
        &srv,
        "POST",
        "/v1/map",
        json!({ "url": format!("{base}/map-spa") }).to_string(),
    )
    .await;
    assert_eq!(map_resp.status(), StatusCode::OK);
    let map_json = body(map_resp).await;
    assert_eq!(map_json["engine"], "browser");
    assert!(map_json["fallback_reason"].is_string());
    assert_eq!(map_json["links"], json!([format!("{base}/spa-link")]));

    // If fast-CDP navigation lands on a link-local SSRF destination, it is blocked with 400.
    let ssrf_resp = call(
        &srv,
        "POST",
        "/v1/scrape",
        json!({ "url": format!("{base}/ssrf-sim") }).to_string(),
    )
    .await;
    assert_eq!(ssrf_resp.status(), StatusCode::BAD_REQUEST);
    let ssrf_json = body(ssrf_resp).await;
    assert_eq!(ssrf_json["error"]["code"], "invalid_request");

    assert_eq!(closed_targets.load(Ordering::SeqCst), 3);

    let metrics_text = String::from_utf8(
        to_bytes(call(&srv, "GET", "/metrics", Body::empty()).await.into_body(), 1024 * 1024)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(metrics_text.contains("fluxwright_fastpath_hits_total 0\n"), "{metrics_text}");
    assert!(
        metrics_text.contains("fluxwright_fastpath_fallbacks_total 3\n"),
        "{metrics_text}"
    );

    srv.shutdown(Duration::from_secs(2)).await.unwrap();
    cdp_task.abort();
    http_srv_task.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn scrape_extract_crawl_and_openapi_endpoints() {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let base = format!("http://127.0.0.1:{port}");
    let filler = "Fluxwright Tier 1 HTTP scraper and crawler verification payload with plenty of readable content so static HTML extraction triggers cleanly without launching headless Chromium.";

    let site_task = tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let filler = filler.to_string();
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                let target = req
                    .lines()
                    .next()
                    .and_then(|l| l.split_whitespace().nth(1))
                    .unwrap_or("/");
                let html = match target {
                    "/" => format!(
                        "<!doctype html><title>Home</title><body><h1>Home Hub</h1><a href=\"/docs\">Docs</a><a href=\"/admin/secret\">Admin</a><p>{filler}</p></body>"
                    ),
                    "/docs" => format!(
                        "<!doctype html><title>Docs</title><body><h1>Docs Guide</h1><a href=\"/docs/api\">API</a><p>{filler}</p></body>"
                    ),
                    "/docs/api" => format!(
                        "<!doctype html><title>API</title><body><h1>API Reference</h1><a href=\"/\">Back</a><p>{filler}</p></body>"
                    ),
                    "/admin/secret" => format!(
                        "<!doctype html><title>Secret</title><body><h1>Secret</h1><p>{filler}</p></body>"
                    ),
                    _ => "<!doctype html><title>404</title><body>Missing</body>".to_string(),
                };
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    html.len(),
                    html
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            });
        }
    });

    let srv = server(16).await;

    // 1. OpenAPI schema is public without auth.
    let spec_resp = srv
        .router()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/openapi.json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(spec_resp.status(), StatusCode::OK);
    let spec = body(spec_resp).await;
    assert_eq!(spec["openapi"], "3.1.0");
    assert!(spec["paths"]["/v1/crawl"]["post"].is_object());

    // 2. Declarative CSS extraction on /v1/scrape in Tier 1 HTTP mode.
    let scrape_resp = call(
        &srv,
        "POST",
        "/v1/scrape",
        json!({
            "url": format!("{base}/"),
            "mode": "http",
            "formats": ["markdown"],
            "extract": {
                "heading": "h1",
                "links": { "selector": "a", "attr": "href", "all": true }
            }
        })
        .to_string(),
    )
    .await;
    assert_eq!(scrape_resp.status(), StatusCode::OK);
    let scrape_json = body(scrape_resp).await;
    assert_eq!(scrape_json["engine"], "http");
    assert_eq!(scrape_json["extracted"]["heading"], "Home Hub");
    assert_eq!(
        scrape_json["extracted"]["links"],
        json!(["/docs", "/admin/secret"])
    );

    let bad_extract = call(
        &srv,
        "POST",
        "/v1/scrape",
        json!({
            "url": format!("{base}/"),
            "mode": "http",
            "extract": { "bad": "[[[" }
        })
        .to_string(),
    )
    .await;
    assert_eq!(bad_extract.status(), StatusCode::BAD_REQUEST);

    // 3. Async recursive /v1/crawl with depth + exclude_paths + extract rules.
    let start_resp = call(
        &srv,
        "POST",
        "/v1/crawl",
        json!({
            "url": format!("{base}/"),
            "mode": "http",
            "max_depth": 2,
            "max_pages": 5,
            "concurrency": 2,
            "exclude_paths": ["/admin*"],
            "extract": { "heading": "h1" }
        })
        .to_string(),
    )
    .await;
    assert_eq!(start_resp.status(), StatusCode::ACCEPTED);
    let started_crawl = body(start_resp).await;
    let crawl_id = started_crawl["id"].as_str().unwrap().to_string();

    let mut final_crawl = Value::Null;
    for _ in 0..80 {
        let poll_resp = call(&srv, "GET", &format!("/v1/crawl/{crawl_id}"), Body::empty()).await;
        assert_eq!(poll_resp.status(), StatusCode::OK);
        let snap = body(poll_resp).await;
        if snap["status"] == "completed" || snap["status"] == "failed" {
            final_crawl = snap;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    assert_eq!(final_crawl["status"], "completed");
    assert_eq!(final_crawl["completed"], 3);
    assert_eq!(final_crawl["failed"], 0);
    let pages = final_crawl["pages"].as_array().unwrap();
    assert_eq!(pages.len(), 3);
    assert!(!pages.iter().any(|p| p["url"].as_str().unwrap().contains("/admin")));
    assert!(pages.iter().all(|p| p["engine"] == "http" && p["extracted"]["heading"].is_string()));

    srv.shutdown(Duration::from_secs(2)).await.unwrap();
    site_task.abort();
}
