//! Black-box E2E test spawning real `fluxwright coordinate` + `fluxwright serve --join` workers.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::{Child, Command};
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
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_fluxwright"));
        cmd.args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("FLUXWRIGHT_SERVER_TOKEN", SERVER_TOKEN)
            .env("FLUXWRIGHT_WORKER_TOKEN", WORKER_TOKEN)
            .env("FLUXWRIGHT_NO_SANDBOX", "1")
            .env("FLUXWRIGHT_HEADLESS", "1")
            .kill_on_drop(true);

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

async fn spawn_fixture_server() -> (String, AbortOnDrop) {
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

    let task = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
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
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{html}",
                    html.len()
                );
                let _ = stream.write_all(resp.as_bytes()).await;
                let _ = stream.shutdown().await;
            });
        }
    });

    (format!("http://{addr}/"), AbortOnDrop(task))
}

#[tokio::test(flavor = "multi_thread")]
async fn cluster_coordinator_and_workers_e2e() {
    let (fixture_url, _fixture_guard) = spawn_fixture_server().await;
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

    // Spawn 2 workers joining the coordinator
    procs.spawn(
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
    procs.spawn(
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

    // Wait until /v1/workers lists both worker-a and worker-b online
    let workers_snapshot = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let (status, body) =
                http_json(coord_addr, "GET", "/v1/workers", Some(SERVER_TOKEN), None).await;
            assert_eq!(status, 200, "{body}");
            let workers = body["workers"].as_array();
            let has_a = workers.is_some_and(|list| {
                list.iter()
                    .any(|w| w["worker"] == "worker-a" && w["status"] == "online")
            });
            let has_b = workers.is_some_and(|list| {
                list.iter()
                    .any(|w| w["worker"] == "worker-b" && w["status"] == "online")
            });
            if has_a && has_b {
                return body;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("workers did not register\nlogs:\n{}", procs.dump_logs()));
    assert_eq!(workers_snapshot["summary"]["online"], 2, "{workers_snapshot}");

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

    // Submit real browser job #1 and verify it succeeds
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

    // Record worker-b's completed count before killing worker-a
    let (_, before_kill) =
        http_json(coord_addr, "GET", "/v1/workers", Some(SERVER_TOKEN), None).await;
    let b_before = before_kill["workers"]
        .as_array()
        .and_then(|arr| arr.iter().find(|w| w["worker"] == "worker-b"))
        .and_then(|w| w["succeeded_jobs"].as_u64())
        .expect("worker-b entry before kill");

    // Kill worker-a and submit job #2; worker-b must execute and complete it
    procs.kill_and_wait("worker-a").await;

    let (sub2_status, sub2_body) = http_json(
        coord_addr,
        "POST",
        "/v1/jobs",
        Some(SERVER_TOKEN),
        Some(&json!({
            "timeout_ms": 60_000,
            "steps": [
                { "action": "goto", "url": fixture_url },
                { "action": "evaluate", "expression": "document.querySelector('h1').textContent" }
            ]
        })),
    )
    .await;
    assert_eq!(sub2_status, 202, "submit job 2 failed: {sub2_body}");
    let job2_id = sub2_body["id"].as_str().expect("job 2 id");
    let done2 = wait_for_job(coord_addr, job2_id, &procs).await;
    assert_eq!(done2["results"][1], json!("Cluster Worker OK"));

    let (_, after_kill) =
        http_json(coord_addr, "GET", "/v1/workers", Some(SERVER_TOKEN), None).await;
    let b_after = after_kill["workers"]
        .as_array()
        .and_then(|arr| arr.iter().find(|w| w["worker"] == "worker-b"))
        .and_then(|w| w["succeeded_jobs"].as_u64())
        .expect("worker-b entry after kill");
    assert_eq!(
        b_after,
        b_before + 1,
        "expected job 2 to execute on worker-b after killing worker-a: {after_kill}"
    );
}
