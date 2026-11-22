//! Black-box E2E test spawning the real `fluxwright-mcp` binary over stdio JSON-RPC.

use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::task::JoinHandle;

struct AbortOnDrop(JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

struct McpClient {
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    stderr_lines: Arc<Mutex<Vec<String>>>,
    stderr_task: JoinHandle<()>,
}

impl McpClient {
    fn spawn() -> Self {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_fluxwright-mcp"));
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("FLUXWRIGHT_NO_SANDBOX", "1")
            .env("FLUXWRIGHT_HEADLESS", "1")
            .kill_on_drop(true);

        let mut child = cmd.spawn().expect("spawn fluxwright-mcp binary");
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = BufReader::new(child.stdout.take().expect("piped stdout"));
        let stderr = BufReader::new(child.stderr.take().expect("piped stderr"));

        let stderr_lines = Arc::new(Mutex::new(Vec::new()));
        let captured = stderr_lines.clone();
        let stderr_task = tokio::spawn(async move {
            let mut lines = stderr.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let mut guard = captured.lock().unwrap();
                if guard.len() >= 200 {
                    guard.remove(0);
                }
                guard.push(line);
            }
        });

        Self {
            child: Some(child),
            stdin: Some(stdin),
            stdout,
            stderr_lines,
            stderr_task,
        }
    }

    fn dump_stderr(&self) -> String {
        self.stderr_lines.lock().unwrap().join("\n")
    }

    async fn send_line(&mut self, msg: &Value) {
        let stdin = self.stdin.as_mut().expect("stdin still open");
        let mut raw = serde_json::to_string(msg).expect("serialize json-rpc");
        raw.push('\n');
        stdin
            .write_all(raw.as_bytes())
            .await
            .expect("write json-rpc line");
        stdin.flush().await.expect("flush stdin");
    }

    async fn send_notification(&mut self, method: &str) {
        self.send_line(&json!({
            "jsonrpc": "2.0",
            "method": method,
        }))
        .await;
    }

    async fn request(&mut self, id: u64, method: &str, params: Option<Value>) -> Value {
        let payload = match params {
            Some(p) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": method,
                "params": p,
            }),
            None => json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": method,
            }),
        };
        self.send_line(&payload).await;

        let res = tokio::time::timeout(Duration::from_secs(60), async {
            let mut line = String::new();
            loop {
                line.clear();
                let n = self
                    .stdout
                    .read_line(&mut line)
                    .await
                    .expect("read json-rpc stdout");
                if n == 0 {
                    panic!(
                        "fluxwright-mcp closed stdout waiting for id={id} ({method}); stderr:\n{}",
                        self.dump_stderr()
                    );
                }
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let val: Value = serde_json::from_str(trimmed).unwrap_or_else(|e| {
                    panic!("invalid JSON on MCP stdout ({e}): {trimmed:?}")
                });
                if val.get("id").and_then(Value::as_u64) == Some(id) {
                    return val;
                }
            }
        })
        .await;

        match res {
            Ok(val) => {
                assert!(
                    val.get("error").is_none(),
                    "MCP JSON-RPC error for {method}: {val}\nstderr:\n{}",
                    self.dump_stderr()
                );
                val
            }
            Err(_) => {
                panic!(
                    "timed out waiting for MCP response id={id} ({method})\nstderr:\n{}",
                    self.dump_stderr()
                );
            }
        }
    }

    async fn call_tool(&mut self, id: u64, name: &str, arguments: Value) -> String {
        let resp = self
            .request(
                id,
                "tools/call",
                Some(json!({
                    "name": name,
                    "arguments": arguments,
                })),
            )
            .await;
        let result = &resp["result"];
        assert_ne!(
            result.get("isError").and_then(Value::as_bool),
            Some(true),
            "tool {name} returned isError=true: {resp}"
        );
        result["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("expected text content for tool {name}: {resp}"))
            .to_string()
    }

    async fn shutdown_cleanly(mut self) {
        drop(self.stdin.take());
        if let Some(mut child) = self.child.take() {
            let status = tokio::time::timeout(Duration::from_secs(15), child.wait())
                .await
                .expect("fluxwright-mcp did not exit after stdin EOF")
                .expect("wait fluxwright-mcp");
            assert!(
                status.success(),
                "fluxwright-mcp exited with {status}\nstderr:\n{}",
                self.dump_stderr()
            );
        }
        self.stderr_task.abort();
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.start_kill();
            for _ in 0..30 {
                if matches!(child.try_wait(), Ok(Some(_))) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        self.stderr_task.abort();
    }
}

async fn spawn_fixture_server() -> (String, AbortOnDrop) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind local test server");
    let addr = listener.local_addr().expect("local addr");
    let html = concat!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">",
        "<title>MCP Stdio E2E</title></head><body>",
        "<main>",
        "<h1>MCP Fixture Heading</h1>",
        "<label for=\"msg\">Message</label>",
        "<input id=\"msg\" type=\"text\">",
        "<button id=\"apply\" type=\"button\" ",
        "onclick=\"document.getElementById('status').textContent = 'Applied: ' + document.getElementById('msg').value\">",
        "Apply</button>",
        "<p id=\"status\">Pending</p>",
        "</main></body></html>"
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
                let req = String::from_utf8_lossy(&buf[..filled]);
                let path = req
                    .lines()
                    .next()
                    .and_then(|l| l.split_whitespace().nth(1))
                    .unwrap_or("/");
                let resp = if path == "/" || path.starts_with("/?") {
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{html}",
                        html.len()
                    )
                } else {
                    "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        .to_string()
                };
                let _ = stream.write_all(resp.as_bytes()).await;
                let _ = stream.shutdown().await;
            });
        }
    });

    (format!("http://{addr}/"), AbortOnDrop(task))
}

fn find_ref_on_line(snapshot: &str, needle: &str) -> String {
    for line in snapshot.lines() {
        if !line.contains(needle) {
            continue;
        }
        if let Some(idx) = line.find("[ref=") {
            let after = &line[idx + 5..];
            if let Some(end) = after.find(']') {
                let id = &after[..end];
                if id.starts_with('e') && id.len() > 1 && id[1..].chars().all(|c| c.is_ascii_digit())
                {
                    return format!("@{id}");
                }
            }
        }
    }
    panic!("could not find @eN ref for line matching {needle:?} in snapshot:\n{snapshot}");
}

#[tokio::test(flavor = "multi_thread")]
async fn mcp_stdio_e2e_handshake_snapshot_refs_and_markdown() {
    let (base_url, _server_guard) = spawn_fixture_server().await;
    let mut client = McpClient::spawn();

    // 1. MCP JSON-RPC handshake: initialize -> notifications/initialized
    let init = client
        .request(
            1,
            "initialize",
            Some(json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "fluxwright-stdio-e2e", "version": "0.1.0" },
            })),
        )
        .await;
    assert_eq!(
        init["result"]["serverInfo"]["name"],
        json!("fluxwright-mcp"),
        "unexpected initialize response: {init}"
    );
    client.send_notification("notifications/initialized").await;

    // 2. tools/list asserts all 21 tools
    let listed = client.request(2, "tools/list", None).await;
    let tools = listed["result"]["tools"]
        .as_array()
        .expect("tools array in tools/list");
    assert_eq!(
        tools.len(),
        21,
        "expected 21 MCP tools, got {}: {tools:?}",
        tools.len()
    );

    // 3. Drive open -> goto -> snapshot -> fill(@eN) -> click(@eN) -> markdown -> close
    let opened = client.call_tool(3, "open", json!({})).await;
    assert!(opened.contains("Chromium page ready"), "{opened}");

    let navigated = client.call_tool(4, "goto", json!({ "url": base_url })).await;
    assert!(navigated.contains("MCP Stdio E2E"), "{navigated}");

    let snap = client.call_tool(5, "snapshot", json!({})).await;
    assert!(snap.contains("[ref=e"), "expected @eN refs in snapshot:\n{snap}");
    let input_ref = find_ref_on_line(&snap, "textbox \"Message\"");
    let button_ref = find_ref_on_line(&snap, "button \"Apply\"");

    let filled = client
        .call_tool(
            6,
            "fill",
            json!({ "selector": input_ref, "value": "stdio-e2e-ok" }),
        )
        .await;
    assert!(filled.contains("filled @e"), "{filled}");

    let clicked = client
        .call_tool(7, "click", json!({ "selector": button_ref }))
        .await;
    assert!(clicked.contains("clicked @e"), "{clicked}");

    let md = client.call_tool(8, "markdown", json!({})).await;
    assert!(
        md.contains("# MCP Fixture Heading"),
        "expected heading in markdown:\n{md}"
    );
    assert!(
        md.contains("Applied: stdio-e2e-ok"),
        "expected updated DOM text in markdown:\n{md}"
    );

    let closed = client.call_tool(9, "close", json!({})).await;
    assert!(closed.contains("page closed"), "{closed}");

    client.shutdown_cleanly().await;
}
