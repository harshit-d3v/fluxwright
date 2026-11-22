//! Black-box E2E test spawning real `fluxwright coordinate` + `fluxwright serve --join` workers.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::{Child, Command};
use tokio::sync::Notify;
use tokio::task::JoinHandle;

const SERVER_TOKEN: &str = "server_token_0123456789abcdef0123456789abcdef";
const WORKER_TOKEN: &str = "worker_token_fedcba9876543210fedcba9876543210";

struct AbortOnDrop(JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

struct TempDirGuard(PathBuf);

impl TempDirGuard {
    fn new(prefix: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("{prefix}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        Self(dir)
    }
}

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct ManagedProcs {
    children: Vec<(String, Child)>,
    drain_tasks: Vec<JoinHandle<()>>,
    logs: Arc<Mutex<Vec<String>>>,
}

impl ManagedProcs {
    fn new() -> Self {
        Self {
            children: Vec::new(),
            drain_tasks: Vec::new(),
            logs: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn spawn(&mut self, label: &str, args: &[&str]) {
        self.spawn_inner(label, args, Some(SERVER_TOKEN), WORKER_TOKEN);
    }

    fn spawn_worker_only_token(&mut self, label: &str, args: &[&str]) {
        self.spawn_inner(label, args, None, WORKER_TOKEN);
    }

    fn spawn_inner(
        &mut self,
        label: &str,
        args: &[&str],
        server_token: Option<&str>,
        worker_token: &str,
    ) {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_fluxwright"));
        cmd.args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("FLUXWRIGHT_WORKER_TOKEN", worker_token)
            .env("FLUXWRIGHT_NO_SANDBOX", "1")
            .env("FLUXWRIGHT_HEADLESS", "1")
            .kill_on_drop(true);
        if let Some(tok) = server_token {
            cmd.env("FLUXWRIGHT_SERVER_TOKEN", tok);
        } else {
            cmd.env_remove("FLUXWRIGHT_SERVER_TOKEN");
        }

        let mut child = cmd
            .spawn()
            .unwrap_or_else(|e| panic!("failed to spawn {label}: {e}"));

        if let Some(stdout) = child.stdout.take() {
            self.spawn_drain(label, "stdout", stdout);
        }
        if let Some(stderr) = child.stderr.take() {
            self.spawn_drain(label, "stderr", stderr);
        }
        self.children.push((label.to_string(), child));
    }

    fn spawn_drain<R: tokio::io::AsyncRead + Unpin + Send + 'static>(
        &mut self,
        label: &str,
        stream_name: &str,
        reader: R,
    ) {
        let tag = format!("{label}:{stream_name}");
        let logs = self.logs.clone();
        let handle = tokio::spawn(async move {
            let mut lines = BufReader::new(reader).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let mut guard = logs.lock().unwrap();
                if guard.len() >= 300 {
                    guard.remove(0);
                }
                guard.push(format!("[{tag}] {line}"));
            }
        });
        self.drain_tasks.push(handle);
    }

    async fn kill_and_wait(&mut self, label: &str) {
        let pos = self
            .children
            .iter()
            .position(|(name, _)| name == label)
            .unwrap_or_else(|| panic!("unknown process {label}"));
        let (_, mut child) = self.children.remove(pos);
        let _ = child.kill().await;
        let _ = tokio::time::timeout(Duration::from_secs(10), child.wait()).await;
    }

    fn dump_logs(&self) -> String {
        self.logs.lock().unwrap().join("\n")
    }
}

impl Drop for ManagedProcs {
    fn drop(&mut self) {
        for (_, mut child) in self.children.drain(..) {
            let _ = child.start_kill();
            for _ in 0..30 {
                if matches!(child.try_wait(), Ok(Some(_))) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        for task in self.drain_tasks.drain(..) {
            task.abort();
        }
    }
}

fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    listener.local_addr().expect("ephemeral addr").port()
}

fn decode_chunked(mut data: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    loop {
        let Some(line_end) = data.windows(2).position(|w| w == b"\r\n") else {
            return Err("truncated chunked response".into());
        };
        let size_hex = std::str::from_utf8(&data[..line_end])
            .map_err(|_| "invalid chunk size")?
            .split(';')
            .next()
            .unwrap_or("")
            .trim();
        let size = usize::from_str_radix(size_hex, 16).map_err(|_| "invalid chunk hex")?;
        data = &data[line_end + 2..];
        if size == 0 {
            return Ok(out);
        }
        if data.len() < size + 2 {
            return Err("truncated chunk payload".into());
        }
        out.extend_from_slice(&data[..size]);
        data = &data[size + 2..];
    }
}

async fn http_request(
    addr: SocketAddr,
    method: &str,
    path: &str,
    bearer: Option<&str>,
    body: Option<&[u8]>,
) -> Result<(u16, Vec<u8>), String> {
    let io = async {
        let mut stream = TcpStream::connect(addr).await.map_err(|e| e.to_string())?;
        let mut head = format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n");
        if let Some(tok) = bearer {
            head.push_str(&format!("Authorization: Bearer {tok}\r\n"));
        }
        if let Some(payload) = body {
            head.push_str("Content-Type: application/json\r\n");
            head.push_str(&format!("Content-Length: {}\r\n", payload.len()));
        }
        head.push_str("\r\n");

        stream
            .write_all(head.as_bytes())
            .await
            .map_err(|e| e.to_string())?;
        if let Some(payload) = body {
            stream.write_all(payload).await.map_err(|e| e.to_string())?;
        }
        stream.flush().await.map_err(|e| e.to_string())?;

        let mut raw = Vec::new();
        stream
            .read_to_end(&mut raw)
            .await
            .map_err(|e| e.to_string())?;
        Ok::<Vec<u8>, String>(raw)
    };

    let raw = tokio::time::timeout(Duration::from_secs(10), io)
        .await
        .map_err(|_| "HTTP request timed out".to_string())??;

    let sep = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| "invalid HTTP response".to_string())?;
    let header_text = std::str::from_utf8(&raw[..sep]).map_err(|_| "non-UTF8 HTTP headers")?;
    let mut lines = header_text.split("\r\n");
    let status_line = lines.next().unwrap_or("");
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| format!("invalid HTTP status line: {status_line}"))?;
    let is_chunked = lines.any(|l| {
        l.split_once(':').is_some_and(|(k, v)| {
            k.trim().eq_ignore_ascii_case("transfer-encoding")
                && v.to_ascii_lowercase().contains("chunked")
        })
    });
    let raw_body = &raw[sep + 4..];
    let decoded = if is_chunked {
        decode_chunked(raw_body)?
    } else {
        raw_body.to_vec()
    };
    Ok((status, decoded))
}

async fn http_json(
    addr: SocketAddr,
    method: &str,
    path: &str,
    bearer: Option<&str>,
    payload: Option<&Value>,
) -> (u16, Value) {
    let bytes = payload.map(|v| serde_json::to_vec(v).expect("serialize JSON"));
    let (status, body) = http_request(addr, method, path, bearer, bytes.as_deref())
        .await
        .unwrap_or_else(|e| panic!("{method} {path} failed: {e}"));
    let parsed = serde_json::from_slice(&body).unwrap_or_else(|e| {
        panic!(
            "invalid JSON from {method} {path} (status {status}): {e}: {}",
            String::from_utf8_lossy(&body)
        )
    });
    (status, parsed)
}

async fn wait_for_job(addr: SocketAddr, job_id: &str, procs: &ManagedProcs) -> Value {
    let path = format!("/v1/jobs/{job_id}");
    tokio::time::timeout(Duration::from_secs(90), async {
        loop {
            let (status, job) = http_json(addr, "GET", &path, Some(SERVER_TOKEN), None).await;
            assert_eq!(status, 200, "unexpected GET {path} status: {job}");
            match job["status"].as_str() {
                Some("succeeded") => return job,
                Some("failed") | Some("cancelled") => {
                    panic!("job {job_id} ended unexpectedly: {job}\nlogs:\n{}", procs.dump_logs());
                }
                _ => tokio::time::sleep(Duration::from_millis(100)).await,
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for job {job_id}\nlogs:\n{}", procs.dump_logs()))
}

async fn spawn_fixture_server() -> (String, Arc<AtomicUsize>, Arc<Notify>, AbortOnDrop) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fixture HTTP server");
    let addr = listener.local_addr().expect("fixture addr");
    let html = concat!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">",
        "<title>Cluster E2E Page</title></head><body>",
        "<h1>Cluster Worker OK</h1>",
        "</body></html>"
    );
    let failover_hold_html = concat!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">",
        "<title>Failover Hold</title></head><body>",
        "<p>Waiting for failover...</p>",
        "</body></html>"
    );
    let failover_ok_html = concat!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">",
        "<title>Failover Recovered</title></head><body>",
        "<h1 id=\"ready\">Recovered</h1>",
        "</body></html>"
    );

    let failover_hits = Arc::new(AtomicUsize::new(0));
    let failover_started = Arc::new(Notify::new());
    let hits_clone = failover_hits.clone();
    let started_clone = failover_started.clone();

    let task = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let hits = hits_clone.clone();
            let started = started_clone.clone();
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let mut filled = 0usize;
                loop {
                    match stream.read(&mut buf[filled..]).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => {
                            filled += n;
                            if buf[..filled].windows(4).any(|w| w == b"\r\n\r\n")
                                || filled == buf.len()
                            {
                                break;
                            }
                        }
                    }
                }
                let req_text = std::str::from_utf8(&buf[..filled]).unwrap_or("");
                let path = req_text
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .unwrap_or("/");
                let body = if path == "/failover" {
                    let hit = hits.fetch_add(1, Ordering::SeqCst) + 1;
                    if hit == 1 {
                        started.notify_one();
                        failover_hold_html
                    } else {
                        failover_ok_html
                    }
                } else {
                    html
                };
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(resp.as_bytes()).await;
                let _ = stream.shutdown().await;
            });
        }
    });

    (
        format!("http://{addr}"),
        failover_hits,
        failover_started,
        AbortOnDrop(task),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn cluster_coordinator_and_workers_e2e() {
    let (fixture_base, failover_hits, failover_started, _fixture_guard) =
        spawn_fixture_server().await;
    let fixture_url = format!("{fixture_base}/");
    let failover_url = format!("{fixture_base}/failover");
    let tmp = TempDirGuard::new("fluxwright-cluster-e2e");
    let journal_path = tmp.0.join("journal.jsonl");
    let journal_str = journal_path.to_str().expect("UTF-8 temp path").to_string();

    let port = free_port();
    let coord_addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let coord_listen = coord_addr.to_string();
    let coord_url = format!("http://{coord_listen}");

    let mut procs = ManagedProcs::new();
    procs.spawn(
        "coordinator",
        &[
            "coordinate",
            "--listen",
            &coord_listen,
            "--journal",
            &journal_str,
        ],
    );

    // Wait for coordinator /healthz
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if let Ok((200, _)) = http_request(coord_addr, "GET", "/healthz", None, None).await {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("coordinator did not become healthy\nlogs:\n{}", procs.dump_logs()));

    // Verify coordinator rejects pause_for_human immediately with 400 (no local browser / HITL).
    let (pause_status, pause_body) = http_json(
        coord_addr,
        "POST",
        "/v1/jobs",
        Some(SERVER_TOKEN),
        Some(&json!({
            "steps": [
                { "action": "goto", "url": fixture_url },
                { "action": "pause_for_human", "message": "not supported on coordinator" }
            ]
        })),
    )
    .await;
    assert_eq!(
        pause_status, 400,
        "coordinator must reject pause_for_human with 400, got {pause_status}: {pause_body}"
    );

    // Verify worker token is rejected with 401 on client job submission endpoint
    let (unauth_status, unauth_body) = http_json(
        coord_addr,
        "POST",
        "/v1/jobs",
        Some(WORKER_TOKEN),
        Some(&json!({ "steps": [{ "action": "title" }] })),
    )
    .await;
    assert_eq!(
        unauth_status, 401,
        "expected worker token to get 401 on /v1/jobs, got {unauth_status}: {unauth_body}"
    );

    // Spawn worker-a with ONLY FLUXWRIGHT_WORKER_TOKEN (no FLUXWRIGHT_SERVER_TOKEN!)
    procs.spawn_worker_only_token(
        "worker-a",
        &[
            "serve",
            "--join",
            &coord_url,
            "--worker-id",
            "worker-a",
            "--max-browsers",
            "1",
            "--max-contexts",
            "2",
        ],
    );

    // Wait until /v1/workers lists worker-a online with true capacity max_jobs == 2
    let workers_snapshot = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let (status, body) =
                http_json(coord_addr, "GET", "/v1/workers", Some(SERVER_TOKEN), None).await;
            assert_eq!(status, 200, "{body}");
            let has_a = body["workers"].as_array().is_some_and(|list| {
                list.iter()
                    .any(|w| w["worker"] == "worker-a" && w["status"] == "online")
            });
            if has_a {
                return body;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("worker-a did not register\nlogs:\n{}", procs.dump_logs()));
    assert_eq!(workers_snapshot["summary"]["online"], 1, "{workers_snapshot}");
    assert_eq!(workers_snapshot["summary"]["capacity"], 2, "{workers_snapshot}");

    // Submit real browser job #1 and verify it succeeds on worker-a
    let (sub1_status, sub1_body) = http_json(
        coord_addr,
        "POST",
        "/v1/jobs",
        Some(SERVER_TOKEN),
        Some(&json!({
            "timeout_ms": 60_000,
            "steps": [
                { "action": "goto", "url": fixture_url },
                { "action": "title" },
                { "action": "text_content", "selector": "h1" }
            ]
        })),
    )
    .await;
    assert_eq!(sub1_status, 202, "submit job 1 failed: {sub1_body}");
    let job1_id = sub1_body["id"].as_str().expect("job 1 id");
    let done1 = wait_for_job(coord_addr, job1_id, &procs).await;
    assert_eq!(done1["results"][1], json!("Cluster E2E Page"));
    assert_eq!(done1["results"][2], json!("Cluster Worker OK"));

    // Submit failover job (retries: 1) that holds on hit #1 until worker-a is SIGKILLed.
    let (fail_sub_status, fail_sub_body) = http_json(
        coord_addr,
        "POST",
        "/v1/jobs",
        Some(SERVER_TOKEN),
        Some(&json!({
            "retries": 1,
            "timeout_ms": 90_000,
            "steps": [
                { "action": "goto", "url": failover_url },
                { "action": "wait_for_selector", "selector": "#ready" },
                { "action": "text_content", "selector": "#ready" },
                { "action": "title" }
            ]
        })),
    )
    .await;
    assert_eq!(fail_sub_status, 202, "submit failover job failed: {fail_sub_body}");
    let failover_job_id = fail_sub_body["id"].as_str().expect("failover job id");

    // Wait until worker-a is actively executing hit #1 of /failover.
    tokio::time::timeout(Duration::from_secs(30), failover_started.notified())
        .await
        .unwrap_or_else(|_| {
            panic!(
                "worker-a did not start failover job\nlogs:\n{}",
                procs.dump_logs()
            )
        });

    // Wait >3 worker poll cycles (200ms each) while worker-a holds 1 of 2 slots, then verify
    // GET /v1/workers still reports true capacity max_jobs == 2 (not overwritten to want=1).
    tokio::time::sleep(Duration::from_millis(750)).await;
    let (busy_status, busy_workers) =
        http_json(coord_addr, "GET", "/v1/workers", Some(SERVER_TOKEN), None).await;
    assert_eq!(busy_status, 200, "{busy_workers}");
    let worker_a_busy = busy_workers["workers"]
        .as_array()
        .and_then(|arr| arr.iter().find(|w| w["worker"] == "worker-a"))
        .unwrap_or_else(|| panic!("worker-a missing while busy: {busy_workers}"));
    assert_eq!(
        worker_a_busy["active_jobs"], 1,
        "worker-a should have 1 active job while holding /failover: {busy_workers}"
    );
    assert_eq!(
        worker_a_busy["max_jobs"], 2,
        "worker-a max_jobs capacity must remain 2 while busy (not overwritten by want=1): {busy_workers}"
    );
    assert_eq!(
        busy_workers["summary"]["capacity"], 2,
        "cluster summary capacity must remain 2 while busy: {busy_workers}"
    );

    // SIGKILL worker-a mid-job (no graceful /release or /result).
    procs.kill_and_wait("worker-a").await;

    // Spawn worker-b (also with ONLY FLUXWRIGHT_WORKER_TOKEN) to pick up the requeued job
    // once worker-a's lease expires.
    procs.spawn_worker_only_token(
        "worker-b",
        &[
            "serve",
            "--join",
            &coord_url,
            "--worker-id",
            "worker-b",
            "--max-browsers",
            "1",
            "--max-contexts",
            "2",
        ],
    );

    // Wait for worker-b to register online.
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let (status, body) =
                http_json(coord_addr, "GET", "/v1/workers", Some(SERVER_TOKEN), None).await;
            assert_eq!(status, 200, "{body}");
            let has_b = body["workers"].as_array().is_some_and(|list| {
                list.iter()
                    .any(|w| w["worker"] == "worker-b" && w["status"] == "online")
            });
            if has_b {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("worker-b did not register\nlogs:\n{}", procs.dump_logs()));

    // Wait for coordinator lease expiry (30s default) -> requeue -> execution on worker-b.
    let done_failover = wait_for_job(coord_addr, failover_job_id, &procs).await;
    assert_eq!(done_failover["status"], json!("succeeded"), "{done_failover}");
    assert_eq!(
        done_failover["attempts"], 2,
        "expected failover job to succeed on attempt 2 after lease expiry: {done_failover}"
    );
    assert_eq!(done_failover["results"][2], json!("Recovered"), "{done_failover}");
    assert_eq!(
        done_failover["results"][3],
        json!("Failover Recovered"),
        "{done_failover}"
    );
    assert_eq!(
        failover_hits.load(Ordering::SeqCst),
        2,
        "expected /failover to be hit once by worker-a and once by worker-b"
    );

    let (_, after_failover) =
        http_json(coord_addr, "GET", "/v1/workers", Some(SERVER_TOKEN), None).await;
    let a_expired = after_failover["workers"]
        .as_array()
        .and_then(|arr| arr.iter().find(|w| w["worker"] == "worker-a"))
        .and_then(|w| w["expired_leases"].as_u64())
        .unwrap_or_else(|| panic!("worker-a missing after failover: {after_failover}"));
    let b_succeeded = after_failover["workers"]
        .as_array()
        .and_then(|arr| arr.iter().find(|w| w["worker"] == "worker-b"))
        .and_then(|w| w["succeeded_jobs"].as_u64())
        .unwrap_or_else(|| panic!("worker-b missing after failover: {after_failover}"));
    assert_eq!(
        a_expired, 1,
        "expected worker-a to record 1 expired lease after mid-job SIGKILL: {after_failover}"
    );
    assert!(
        b_succeeded >= 1,
        "expected worker-b to complete the requeued failover job: {after_failover}"
    );
}

async fn wait_for_healthz(addr: SocketAddr, procs: &ManagedProcs) {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if let Ok((200, _)) = http_request(addr, "GET", "/healthz", None, None).await {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("coordinator did not become healthy\nlogs:\n{}", procs.dump_logs()));
}

async fn wait_for_workers_online(addr: SocketAddr, names: &[&str], procs: &ManagedProcs) {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let (status, body) =
                http_json(addr, "GET", "/v1/workers", Some(SERVER_TOKEN), None).await;
            assert_eq!(status, 200, "{body}");
            let all_online = names.iter().all(|name| {
                body["workers"].as_array().is_some_and(|list| {
                    list.iter()
                        .any(|w| w["worker"] == *name && w["status"] == "online")
                })
            });
            if all_online {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "workers {names:?} did not register online\nlogs:\n{}",
            procs.dump_logs()
        )
    });
}

async fn wait_for_terminal_job(addr: SocketAddr, job_id: &str, procs: &ManagedProcs) -> Value {
    let path = format!("/v1/jobs/{job_id}");
    tokio::time::timeout(Duration::from_secs(90), async {
        loop {
            let (status, job) = http_json(addr, "GET", &path, Some(SERVER_TOKEN), None).await;
            assert_eq!(status, 200, "unexpected GET {path} status: {job}");
            if matches!(
                job["status"].as_str(),
                Some("succeeded") | Some("failed") | Some("cancelled")
            ) {
                return job;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for job {job_id}\nlogs:\n{}", procs.dump_logs()))
}

#[tokio::test(flavor = "multi_thread")]
async fn cluster_sqlite_store_preserves_finished_and_replays_queued_across_restart() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let fixture_addr = listener.local_addr().unwrap();
    let fixture_base = format!("http://{fixture_addr}");

    let hold_started = Arc::new(Notify::new());
    let (hold_release_tx, hold_release_rx) = tokio::sync::watch::channel(false);
    let hold_started_srv = hold_started.clone();

    let fixture_task = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let hold_started = hold_started_srv.clone();
            let mut release_rx = hold_release_rx.clone();
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let mut filled = 0usize;
                loop {
                    match stream.read(&mut buf[filled..]).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => {
                            filled += n;
                            if buf[..filled].windows(4).any(|w| w == b"\r\n\r\n")
                                || filled == buf.len()
                            {
                                break;
                            }
                        }
                    }
                }
                let req_text = std::str::from_utf8(&buf[..filled]).unwrap_or("");
                let path = req_text
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .unwrap_or("/");
                let body = if path == "/hold" {
                    if !*release_rx.borrow() {
                        hold_started.notify_one();
                        let _ = tokio::time::timeout(
                            Duration::from_secs(30),
                            release_rx.wait_for(|v| *v),
                        )
                        .await;
                    }
                    "<!doctype html><title>Hold Released OK</title><h1>Hold Done</h1>"
                } else {
                    "<!doctype html><title>Store E2E Page</title><h1>Store Worker OK</h1>"
                };
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(resp.as_bytes()).await;
                let _ = stream.shutdown().await;
            });
        }
    });
    let _fixture_guard = AbortOnDrop(fixture_task);

    let tmp = TempDirGuard::new("fluxwright-cluster-sqlite-e2e");
    let store_path = tmp.0.join("coord.sqlite");
    let store_str = store_path.to_str().expect("UTF-8 temp path").to_string();

    let port1 = free_port();
    let coord1_addr: SocketAddr = format!("127.0.0.1:{port1}").parse().unwrap();
    let coord1_listen = coord1_addr.to_string();
    let coord1_url = format!("http://{coord1_listen}");

    let mut procs = ManagedProcs::new();
    procs.spawn(
        "coordinator-1",
        &[
            "coordinate",
            "--listen",
            &coord1_listen,
            "--store",
            &store_str,
        ],
    );
    wait_for_healthz(coord1_addr, &procs).await;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&store_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "SQLite store must be owner-only 0o600");
    }

    procs.spawn_worker_only_token(
        "worker-a",
        &[
            "serve",
            "--join",
            &coord1_url,
            "--worker-id",
            "worker-a",
            "--max-browsers",
            "1",
            "--max-contexts",
            "1",
        ],
    );
    wait_for_workers_online(coord1_addr, &["worker-a"], &procs).await;

    // 1. Succeeded job persisted in SQLite.
    let (s1, b1) = http_json(
        coord1_addr,
        "POST",
        "/v1/jobs",
        Some(SERVER_TOKEN),
        Some(&json!({
            "steps": [
                { "action": "goto", "url": format!("{fixture_base}/") },
                { "action": "title" },
                { "action": "text_content", "selector": "h1" }
            ]
        })),
    )
    .await;
    assert_eq!(s1, 202, "{b1}");
    let job1_id = b1["id"].as_str().unwrap().to_string();
    let done1 = wait_for_job(coord1_addr, &job1_id, &procs).await;
    assert_eq!(done1["results"][1], json!("Store E2E Page"));
    assert_eq!(done1["results"][2], json!("Store Worker OK"));

    // 2. Failed job with failure_evidence persisted in SQLite.
    let (s2, b2) = http_json(
        coord1_addr,
        "POST",
        "/v1/jobs",
        Some(SERVER_TOKEN),
        Some(&json!({
            "steps": [
                { "action": "goto", "url": format!("{fixture_base}/") },
                { "action": "evaluate", "expression": "(() => { console.error('e2e-store-err'); throw new Error('e2e-store-fail'); })()" }
            ]
        })),
    )
    .await;
    assert_eq!(s2, 202, "{b2}");
    let job2_id = b2["id"].as_str().unwrap().to_string();
    let done2 = wait_for_terminal_job(coord1_addr, &job2_id, &procs).await;
    assert_eq!(done2["status"], json!("failed"), "{done2}");

    // 3. Running job (held on /hold) + 4. Queued job (waiting behind worker-a's 1 slot).
    let (s3, b3) = http_json(
        coord1_addr,
        "POST",
        "/v1/jobs",
        Some(SERVER_TOKEN),
        Some(&json!({
            "steps": [
                { "action": "goto", "url": format!("{fixture_base}/hold") },
                { "action": "title" }
            ]
        })),
    )
    .await;
    assert_eq!(s3, 202, "{b3}");
    let job3_id = b3["id"].as_str().unwrap().to_string();

    tokio::time::timeout(Duration::from_secs(30), hold_started.notified())
        .await
        .unwrap_or_else(|_| panic!("worker-a did not start /hold\nlogs:\n{}", procs.dump_logs()));

    let (s4, b4) = http_json(
        coord1_addr,
        "POST",
        "/v1/jobs",
        Some(SERVER_TOKEN),
        Some(&json!({
            "steps": [
                { "action": "goto", "url": format!("{fixture_base}/") },
                { "action": "title" }
            ]
        })),
    )
    .await;
    assert_eq!(s4, 202, "{b4}");
    let job4_id = b4["id"].as_str().unwrap().to_string();

    // Kill coordinator-1 and worker-a mid-flight, then allow /hold to return fast on replay.
    procs.kill_and_wait("coordinator-1").await;
    procs.kill_and_wait("worker-a").await;
    let _ = hold_release_tx.send(true);

    // Restart coordinator-2 with the same --store SQLite DB.
    let port2 = free_port();
    let coord2_addr: SocketAddr = format!("127.0.0.1:{port2}").parse().unwrap();
    let coord2_listen = coord2_addr.to_string();
    let coord2_url = format!("http://{coord2_listen}");
    procs.spawn(
        "coordinator-2",
        &[
            "coordinate",
            "--listen",
            &coord2_listen,
            "--store",
            &store_str,
        ],
    );
    wait_for_healthz(coord2_addr, &procs).await;

    // Before starting any worker, completed/failed jobs are already pollable and pending jobs are Queued.
    let (g1, r1) = http_json(
        coord2_addr,
        "GET",
        &format!("/v1/jobs/{job1_id}"),
        Some(SERVER_TOKEN),
        None,
    )
    .await;
    assert_eq!(g1, 200, "{r1}");
    assert_eq!(r1["status"], json!("succeeded"));
    assert_eq!(r1["results"][1], json!("Store E2E Page"));
    assert_eq!(r1["results"][2], json!("Store Worker OK"));

    let (g2, r2) = http_json(
        coord2_addr,
        "GET",
        &format!("/v1/jobs/{job2_id}"),
        Some(SERVER_TOKEN),
        None,
    )
    .await;
    assert_eq!(g2, 200, "{r2}");
    assert_eq!(r2["status"], json!("failed"));
    assert!(r2["error"]
        .as_str()
        .unwrap_or_default()
        .contains("e2e-store-fail"));
    assert!(
        r2["failure_evidence"]["screenshot"]
            .as_str()
            .is_some_and(|s| s.starts_with("iVBOR")),
        "expected failure screenshot to survive restart: {r2}"
    );

    for pending_id in [&job3_id, &job4_id] {
        let (gp, rp) = http_json(
            coord2_addr,
            "GET",
            &format!("/v1/jobs/{pending_id}"),
            Some(SERVER_TOKEN),
            None,
        )
        .await;
        assert_eq!(gp, 200, "{rp}");
        assert_eq!(rp["status"], json!("queued"), "{rp}");
    }

    // Start worker-b and verify both replayed jobs complete.
    procs.spawn_worker_only_token(
        "worker-b",
        &[
            "serve",
            "--join",
            &coord2_url,
            "--worker-id",
            "worker-b",
            "--max-browsers",
            "1",
            "--max-contexts",
            "2",
        ],
    );
    wait_for_workers_online(coord2_addr, &["worker-b"], &procs).await;

    let done3 = wait_for_job(coord2_addr, &job3_id, &procs).await;
    assert_eq!(done3["results"][1], json!("Hold Released OK"));
    let done4 = wait_for_job(coord2_addr, &job4_id, &procs).await;
    assert_eq!(done4["results"][1], json!("Store E2E Page"));
}

#[tokio::test(flavor = "multi_thread")]
async fn cluster_coordinator_max_per_host_limits_concurrent_host_a_and_runs_host_b_immediately() {
    // Host A (127.0.0.1:<portA>): holds /slow until released and tracks peak concurrent requests.
    let listener_a = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port_a = listener_a.local_addr().unwrap().port();
    let active_a = Arc::new(AtomicUsize::new(0));
    let max_concurrent_a = Arc::new(AtomicUsize::new(0));
    let hits_a = Arc::new(AtomicUsize::new(0));
    let first_a_entered = Arc::new(Notify::new());
    let (release_a_tx, release_a_rx) = tokio::sync::watch::channel(false);

    let active_a_srv = active_a.clone();
    let max_a_srv = max_concurrent_a.clone();
    let hits_a_srv = hits_a.clone();
    let entered_srv = first_a_entered.clone();
    let task_a = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener_a.accept().await else {
                break;
            };
            let active = active_a_srv.clone();
            let max_c = max_a_srv.clone();
            let hits = hits_a_srv.clone();
            let entered = entered_srv.clone();
            let mut release_rx = release_a_rx.clone();
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let mut filled = 0usize;
                loop {
                    match stream.read(&mut buf[filled..]).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => {
                            filled += n;
                            if buf[..filled].windows(4).any(|w| w == b"\r\n\r\n")
                                || filled == buf.len()
                            {
                                break;
                            }
                        }
                    }
                }
                let hit = hits.fetch_add(1, Ordering::SeqCst) + 1;
                let cur = active.fetch_add(1, Ordering::SeqCst) + 1;
                max_c.fetch_max(cur, Ordering::SeqCst);
                if hit == 1 {
                    entered.notify_one();
                }
                let _ = tokio::time::timeout(
                    Duration::from_secs(20),
                    release_rx.wait_for(|v| *v),
                )
                .await;
                active.fetch_sub(1, Ordering::SeqCst);
                let body = "<!doctype html><title>Host A OK</title><h1>Host A</h1>";
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(resp.as_bytes()).await;
                let _ = stream.shutdown().await;
            });
        }
    });
    let _guard_a = AbortOnDrop(task_a);

    // Host B (localhost:<portB>): distinct normalized hostname ("localhost" vs "127.0.0.1").
    // Bind 127.0.0.1 and best-effort [::1] on the same port so `localhost` connects immediately on all OSes.
    let listener_b4 = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port_b = listener_b4.local_addr().unwrap().port();
    let listener_b6 = TcpListener::bind(SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, port_b)))
        .await
        .ok();
    let hits_b = Arc::new(AtomicUsize::new(0));

    let serve_b = |listener: TcpListener, hits: Arc<AtomicUsize>| {
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let hits = hits.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let mut filled = 0usize;
                    loop {
                        match stream.read(&mut buf[filled..]).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => {
                                filled += n;
                                if buf[..filled].windows(4).any(|w| w == b"\r\n\r\n")
                                    || filled == buf.len()
                                {
                                    break;
                                }
                            }
                        }
                    }
                    hits.fetch_add(1, Ordering::SeqCst);
                    let body = "<!doctype html><title>Host B OK</title><h1>Host B</h1>";
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(resp.as_bytes()).await;
                    let _ = stream.shutdown().await;
                });
            }
        })
    };
    let _guard_b4 = AbortOnDrop(serve_b(listener_b4, hits_b.clone()));
    let _guard_b6 = listener_b6.map(|l| AbortOnDrop(serve_b(l, hits_b.clone())));

    let coord_port = free_port();
    let coord_addr: SocketAddr = format!("127.0.0.1:{coord_port}").parse().unwrap();
    let coord_listen = coord_addr.to_string();
    let coord_url = format!("http://{coord_listen}");

    let mut procs = ManagedProcs::new();
    procs.spawn(
        "coordinator",
        &[
            "coordinate",
            "--listen",
            &coord_listen,
            "--max-per-host",
            "1",
        ],
    );
    wait_for_healthz(coord_addr, &procs).await;

    for wid in ["worker-1", "worker-2"] {
        procs.spawn_worker_only_token(
            wid,
            &[
                "serve",
                "--join",
                &coord_url,
                "--worker-id",
                wid,
                "--max-browsers",
                "1",
                "--max-contexts",
                "1",
            ],
        );
    }
    wait_for_workers_online(coord_addr, &["worker-1", "worker-2"], &procs).await;

    // Submit Job A1 on Host A (holds on /slow).
    let (sa1, ba1) = http_json(
        coord_addr,
        "POST",
        "/v1/jobs",
        Some(SERVER_TOKEN),
        Some(&json!({
            "steps": [
                { "action": "goto", "url": format!("http://127.0.0.1:{port_a}/slow") },
                { "action": "title" }
            ]
        })),
    )
    .await;
    assert_eq!(sa1, 202, "{ba1}");
    let job_a1_id = ba1["id"].as_str().unwrap().to_string();

    tokio::time::timeout(Duration::from_secs(30), first_a_entered.notified())
        .await
        .unwrap_or_else(|_| panic!("job A1 did not enter Host A\nlogs:\n{}", procs.dump_logs()));

    // Queue Job A2 (same host as A1) BEFORE Job B1 (different host).
    let (sa2, ba2) = http_json(
        coord_addr,
        "POST",
        "/v1/jobs",
        Some(SERVER_TOKEN),
        Some(&json!({
            "steps": [
                { "action": "goto", "url": format!("http://127.0.0.1:{port_a}/slow") },
                { "action": "title" }
            ]
        })),
    )
    .await;
    assert_eq!(sa2, 202, "{ba2}");
    let job_a2_id = ba2["id"].as_str().unwrap().to_string();

    let (sb1, bb1) = http_json(
        coord_addr,
        "POST",
        "/v1/jobs",
        Some(SERVER_TOKEN),
        Some(&json!({
            "steps": [
                { "action": "goto", "url": format!("http://localhost:{port_b}/fast") },
                { "action": "title" }
            ]
        })),
    )
    .await;
    assert_eq!(sb1, 202, "{bb1}");
    let job_b1_id = bb1["id"].as_str().unwrap().to_string();

    // Job B1 must finish immediately on the second worker while A1 is still held and A2 waits.
    let done_b1 = wait_for_job(coord_addr, &job_b1_id, &procs).await;
    assert_eq!(done_b1["results"][1], json!("Host B OK"));

    // Wait >3 worker poll cycles while A1 is still held; A2 must remain queued and Host A hit once.
    tokio::time::sleep(Duration::from_millis(750)).await;
    assert_eq!(
        hits_a.load(Ordering::SeqCst),
        1,
        "Host A must not receive a second concurrent request while Job A1 is active"
    );
    let (ga2, state_a2) = http_json(
        coord_addr,
        "GET",
        &format!("/v1/jobs/{job_a2_id}"),
        Some(SERVER_TOKEN),
        None,
    )
    .await;
    assert_eq!(ga2, 200);
    assert_eq!(
        state_a2["status"],
        json!("queued"),
        "Job A2 must stay queued while Host A is at --max-per-host 1: {state_a2}"
    );

    // Release Host A gate so A1 finishes and A2 can run next.
    let _ = release_a_tx.send(true);
    let done_a1 = wait_for_job(coord_addr, &job_a1_id, &procs).await;
    let done_a2 = wait_for_job(coord_addr, &job_a2_id, &procs).await;
    assert_eq!(done_a1["results"][1], json!("Host A OK"));
    assert_eq!(done_a2["results"][1], json!("Host A OK"));
    assert_eq!(
        max_concurrent_a.load(Ordering::SeqCst),
        1,
        "peak concurrency on Host A must never exceed 1"
    );
    assert_eq!(hits_a.load(Ordering::SeqCst), 2);
    assert_eq!(hits_b.load(Ordering::SeqCst), 1);
}
