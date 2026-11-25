//! Black-box E2E test spawning the real `fluxwright-mcp` binary over stdio JSON-RPC.

use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
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

    async fn call_tool_raw(&mut self, id: u64, name: &str, arguments: Value) -> Value {
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
        resp["result"].clone()
    }

    async fn call_tool(&mut self, id: u64, name: &str, arguments: Value) -> String {
        let result = self.call_tool_raw(id, name, arguments).await;
        assert_ne!(
            result.get("isError").and_then(Value::as_bool),
            Some(true),
            "tool {name} returned isError=true: {result}"
        );
        result["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("expected text content for tool {name}: {result}"))
            .to_string()
    }

    async fn call_tool_expect_error(&mut self, id: u64, name: &str, arguments: Value) -> String {
        let result = self.call_tool_raw(id, name, arguments).await;
        assert_eq!(
            result.get("isError").and_then(Value::as_bool),
            Some(true),
            "expected tool {name} to return isError=true, got: {result}"
        );
        result["content"][0]["text"]
            .as_str()
            .unwrap_or_else(|| panic!("expected error text content for tool {name}: {result}"))
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
        "<button id=\"hover-btn\" type=\"button\" ",
        "onmouseenter=\"document.getElementById('hover-out').textContent = 'Hovered'\">Hover Me</button>",
        "<span id=\"hover-out\">Idle</span>",
        "<input id=\"key-in\" type=\"text\" ",
        "onkeydown=\"if (event.key === 'Enter') document.getElementById('key-out').textContent = 'PressedEnter'\">",
        "<span id=\"key-out\">None</span>",
        "<select id=\"color\"><option value=\"red\">Red</option><option value=\"blue\">Blue</option></select>",
        "<a id=\"popup-link\" href=\"/popup\" target=\"_blank\">Open Popup</a>",
        "<span id=\"net-done\">loading</span>",
        "<div style=\"height:2000px\">Spacer</div>",
        "<div id=\"bottom\">Bottom Anchor</div>",
        "</main>",
        "<script>",
        "console.log('mcp-console-hello');",
        "fetch('/api/ping').then(r => r.json()).then(() => { document.getElementById('net-done').textContent = 'ping-ok'; });",
        "</script>",
        "</body></html>"
    );
    let page2_html = concat!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">",
        "<title>MCP Page Two</title></head><body><h1>Second Page</h1></body></html>"
    );
    let popup_html = concat!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">",
        "<title>MCP Popup</title></head><body><h1>Popup Window</h1></body></html>"
    );
    let ping_json = r#"{"ok":true}"#;

    let task = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let mut filled = 0usize;
                let mut header_end = None;
                loop {
                    match stream.read(&mut buf[filled..]).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => {
                            filled += n;
                            if let Some(pos) = buf[..filled].windows(4).position(|w| w == b"\r\n\r\n") {
                                header_end = Some(pos + 4);
                                break;
                            }
                            if filled == buf.len() {
                                break;
                            }
                        }
                    }
                }
                let head = String::from_utf8_lossy(&buf[..header_end.unwrap_or(filled)]).into_owned();
                if let Some(end) = header_end {
                    let content_len = head
                        .lines()
                        .find_map(|l| {
                            let (k, v) = l.split_once(':')?;
                            k.trim().eq_ignore_ascii_case("content-length")
                                .then(|| v.trim().parse::<usize>().ok())
                                .flatten()
                        })
                        .unwrap_or(0)
                        .min(64 * 1024);
                    while filled < end + content_len {
                        let mut extra = [0u8; 1024];
                        match stream.read(&mut extra).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => filled += n,
                        }
                    }
                }
                let path = head
                    .lines()
                    .next()
                    .and_then(|l| l.split_whitespace().nth(1))
                    .unwrap_or("/");
                let resp = if path == "/" || path.starts_with("/?") {
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{html}",
                        html.len()
                    )
                } else if path == "/page2" {
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{page2_html}",
                        page2_html.len()
                    )
                } else if path == "/popup" {
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{popup_html}",
                        popup_html.len()
                    )
                } else if path == "/api/ping" || path == "/api/secret" {
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{ping_json}",
                        ping_json.len()
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

fn parse_snapshot_rev(snapshot: &str) -> u32 {
    for line in snapshot.lines() {
        if let Some(rest) = line.strip_prefix("rev: ") {
            return rest
                .trim()
                .parse::<u32>()
                .unwrap_or_else(|e| panic!("invalid rev line {line:?} ({e}):\n{snapshot}"));
        }
    }
    panic!("missing `rev: ` header in snapshot:\n{snapshot}");
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

    // 2. tools/list asserts all 22 tools including export_api_calls
    let listed = client.request(2, "tools/list", None).await;
    let tools = listed["result"]["tools"]
        .as_array()
        .expect("tools array in tools/list");
    assert_eq!(
        tools.len(),
        22,
        "expected 22 MCP tools, got {}: {tools:?}",
        tools.len()
    );
    assert!(
        tools.iter().any(|t| t["name"] == "export_api_calls"),
        "expected export_api_calls in tools/list: {tools:?}"
    );

    // 3. Drive open -> goto -> snapshot (with rev:) -> fill(@eN@rK) -> click(@eN@rK) -> markdown -> close
    let opened = client.call_tool(3, "open", json!({})).await;
    assert!(opened.contains("Chromium page ready"), "{opened}");

    let navigated = client.call_tool(4, "goto", json!({ "url": base_url })).await;
    assert!(navigated.contains("MCP Stdio E2E"), "{navigated}");

    let snap = client.call_tool(5, "snapshot", json!({})).await;
    assert!(snap.contains("[ref=e"), "expected @eN refs in snapshot:\n{snap}");
    let snap_rev = parse_snapshot_rev(&snap);
    assert!(snap_rev >= 1, "expected positive snapshot rev:\n{snap}");
    let input_ref = find_ref_on_line(&snap, "textbox \"Message\"");
    let button_ref = find_ref_on_line(&snap, "button \"Apply\"");
    let pinned_input_ref = format!("{input_ref}@r{snap_rev}");
    let pinned_button_ref = format!("{button_ref}@r{snap_rev}");

    let filled = client
        .call_tool(
            6,
            "fill",
            json!({ "selector": pinned_input_ref, "value": "stdio-e2e-ok" }),
        )
        .await;
    assert!(filled.contains("filled @e"), "{filled}");

    let clicked = client
        .call_tool(7, "click", json!({ "selector": pinned_button_ref }))
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

    // 4. title + wait_for (network fetch completion) + console + network
    let page_title = client.call_tool(9, "title", json!({})).await;
    assert_eq!(page_title, "MCP Stdio E2E");

    let waited = client
        .call_tool(10, "wait_for", json!({ "selector": "text=ping-ok", "timeout_ms": 10_000 }))
        .await;
    assert!(waited.contains("satisfied"), "{waited}");

    let console_out = client.call_tool(11, "console", json!({ "limit": 20 })).await;
    assert!(
        console_out.contains("mcp-console-hello"),
        "expected console log entry:\n{console_out}"
    );

    let net_out = client
        .call_tool(12, "network", json!({ "limit": 20, "filter": "/api/ping" }))
        .await;
    assert!(
        net_out.contains("/api/ping") && net_out.contains("200"),
        "expected /api/ping 200 in network log:\n{net_out}"
    );

    // 5. hover, press, select, scroll, evaluate
    let _ = client.call_tool(13, "hover", json!({ "selector": "#hover-btn" })).await;
    let _ = client
        .call_tool(14, "press", json!({ "selector": "#key-in", "key": "Enter" }))
        .await;
    let _ = client
        .call_tool(15, "select", json!({ "selector": "#color", "label": "Blue" }))
        .await;
    let _ = client.call_tool(16, "scroll", json!({ "delta_y": 400 })).await;
    let mut seq = 17u64;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let wheel_y = client
                .call_tool(seq, "evaluate", json!({ "expression": "window.scrollY > 0" }))
                .await;
            seq += 1;
            if wheel_y == "true" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("expected wheel scroll to advance scrollY");

    let _ = client.call_tool(seq, "scroll", json!({ "selector": "#bottom" })).await;
    seq += 1;
    let dom_state = client
        .call_tool(
            seq,
            "evaluate",
            json!({
                "expression": "JSON.stringify([document.getElementById('hover-out').textContent, document.getElementById('key-out').textContent, document.getElementById('color').value, window.scrollY > 500])"
            }),
        )
        .await;
    seq += 1;
    assert_eq!(
        dom_state,
        r#""[\"Hovered\",\"PressedEnter\",\"blue\",true]""#,
        "unexpected interaction DOM state: {dom_state}"
    );

    // 6. screenshot returns valid PNG image content block
    let shot_res = client.call_tool_raw(seq, "screenshot", json!({})).await;
    seq += 1;
    assert_ne!(shot_res.get("isError").and_then(Value::as_bool), Some(true), "{shot_res}");
    assert_eq!(shot_res["content"][0]["type"], json!("image"), "{shot_res}");
    assert_eq!(shot_res["content"][0]["mimeType"], json!("image/png"), "{shot_res}");
    let png_b64 = shot_res["content"][0]["data"]
        .as_str()
        .expect("screenshot base64 data");
    let png_bytes = base64::engine::general_purpose::STANDARD
        .decode(png_b64)
        .expect("valid base64 screenshot");
    assert_eq!(&png_bytes[..8], b"\x89PNG\r\n\x1a\n");

    // 7. goto /page2 -> stale @eN@rK fails -> back -> forward -> back
    let page2_url = format!("{base_url}page2");
    let _ = client.call_tool(seq, "goto", json!({ "url": page2_url })).await;
    seq += 1;
    let stale_err = client
        .call_tool_expect_error(seq, "click", json!({ "selector": pinned_button_ref }))
        .await;
    seq += 1;
    assert!(
        stale_err.contains("stale snapshot rev="),
        "expected stale rev error after navigation, got: {stale_err}"
    );
    assert_eq!(client.call_tool(seq, "title", json!({})).await, "MCP Page Two");
    seq += 1;
    let went_back = client.call_tool(seq, "back", json!({})).await;
    seq += 1;
    assert!(went_back.contains("went back"), "{went_back}");
    assert_eq!(client.call_tool(seq, "title", json!({})).await, "MCP Stdio E2E");
    seq += 1;
    let went_fwd = client.call_tool(seq, "forward", json!({})).await;
    seq += 1;
    assert!(went_fwd.contains("went forward"), "{went_fwd}");
    assert_eq!(client.call_tool(seq, "title", json!({})).await, "MCP Page Two");
    seq += 1;
    let _ = client.call_tool(seq, "back", json!({})).await;
    seq += 1;

    // 8. tabs: new -> list -> switch -> popup captured in tabs -> close
    let opened_tab = client
        .call_tool(seq, "tabs", json!({ "action": "new", "url": page2_url }))
        .await;
    seq += 1;
    assert!(opened_tab.contains("opened tab 1"), "{opened_tab}");
    let tab_list = client.call_tool(seq, "tabs", json!({ "action": "list" })).await;
    seq += 1;
    assert!(tab_list.contains("[0]") && tab_list.contains("[1]*"), "{tab_list}");
    assert!(tab_list.contains("MCP Page Two"), "{tab_list}");

    let switched = client
        .call_tool(seq, "tabs", json!({ "action": "switch", "index": 0 }))
        .await;
    seq += 1;
    assert!(switched.contains("switched to tab 0"), "{switched}");
    assert_eq!(client.call_tool(seq, "title", json!({})).await, "MCP Stdio E2E");
    seq += 1;

    // Click target=_blank link and verify popup tab appears in tabs(action="list")
    let _ = client.call_tool(seq, "click", json!({ "selector": "#popup-link" })).await;
    seq += 1;
    let popup_list = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let list = client.call_tool(seq, "tabs", json!({ "action": "list" })).await;
            seq += 1;
            if list.contains("/popup") && list.contains("MCP Popup") {
                return list;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("popup tab did not appear in MCP tabs list");
    assert!(popup_list.contains("[2]"), "{popup_list}");

    let closed_popup = client
        .call_tool(seq, "tabs", json!({ "action": "close", "index": 2 }))
        .await;
    seq += 1;
    assert!(closed_popup.contains("closed tab 2"), "{closed_popup}");
    let closed_tab1 = client
        .call_tool(seq, "tabs", json!({ "action": "close", "index": 1 }))
        .await;
    seq += 1;
    assert!(closed_tab1.contains("closed tab 1"), "{closed_tab1}");

    // 8b. export_api_calls filters API traffic, redacts auth headers by default, and exports curl.
    let _ = client
        .call_tool(
            seq,
            "evaluate",
            json!({
                "expression": "fetch('/api/secret', { method: 'POST', headers: { 'Authorization': 'Bearer super-secret-tok', 'X-Api-Key': 'secret-key-99', 'Content-Type': 'application/json' }, body: JSON.stringify({ hello: 'world' }) }).then(r => r.status)"
            }),
        )
        .await;
    seq += 1;

    let exported_json = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let raw = client
                .call_tool(seq, "export_api_calls", json!({ "limit": 20 }))
                .await;
            seq += 1;
            let parsed: Value = serde_json::from_str(&raw).unwrap_or_else(|e| {
                panic!("expected valid JSON from export_api_calls ({e}):\n{raw}")
            });
            let items = parsed.as_array().expect("export_api_calls JSON array");
            if items
                .iter()
                .any(|c| c["url"].as_str().is_some_and(|u| u.contains("/api/secret")) && c["status"] == json!(200))
            {
                return parsed;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("timed out waiting for /api/secret in export_api_calls");

    let items = exported_json.as_array().unwrap();
    assert!(
        items.iter().all(|c| !c["url"].as_str().unwrap_or("").ends_with("/page2")),
        "HTML navigation should not be exported as API call: {exported_json}"
    );
    assert!(
        items.iter().any(|c| c["url"].as_str().is_some_and(|u| u.contains("/api/ping"))),
        "expected /api/ping in export_api_calls: {exported_json}"
    );
    let secret_call = items
        .iter()
        .find(|c| c["url"].as_str().is_some_and(|u| u.contains("/api/secret")))
        .expect("/api/secret record");
    assert_eq!(secret_call["method"], json!("POST"));
    assert_eq!(secret_call["headers"]["authorization"], json!("[REDACTED]"));
    assert_eq!(secret_call["headers"]["x-api-key"], json!("[REDACTED]"));
    assert!(
        secret_call["content_type"]
            .as_str()
            .is_some_and(|ct| ct.contains("application/json")),
        "{secret_call}"
    );
    assert!(
        secret_call["post_data"]
            .as_str()
            .is_some_and(|b| b.contains("hello")),
        "{secret_call}"
    );

    let unredacted_raw = client
        .call_tool(
            seq,
            "export_api_calls",
            json!({ "filter": "/api/secret", "include_auth": true }),
        )
        .await;
    seq += 1;
    let unredacted: Value = serde_json::from_str(&unredacted_raw).unwrap();
    assert_eq!(
        unredacted[0]["headers"]["authorization"],
        json!("Bearer super-secret-tok")
    );
    assert_eq!(
        unredacted[0]["headers"]["x-api-key"],
        json!("secret-key-99")
    );

    let curl_out = client
        .call_tool(
            seq,
            "export_api_calls",
            json!({ "filter": "/api/secret", "format": "curl" }),
        )
        .await;
    seq += 1;
    assert!(curl_out.contains("curl -X 'POST'"), "{curl_out}");
    assert!(curl_out.contains("/api/secret"), "{curl_out}");
    assert!(curl_out.contains("[REDACTED]"), "{curl_out}");
    assert!(curl_out.contains("--data-raw"), "{curl_out}");

    // 9. Runtime & SSRF errors return MCP tool error (`isError: true`), not JSON-RPC -32603.
    let ssrf_file = client
        .call_tool_expect_error(seq, "goto", json!({ "url": "file:///etc/passwd" }))
        .await;
    seq += 1;
    assert!(ssrf_file.contains("http"), "{ssrf_file}");

    let ssrf_imds = client
        .call_tool_expect_error(
            seq,
            "goto",
            json!({ "url": "http://169.254.169.254/latest/meta-data/" }),
        )
        .await;
    seq += 1;
    assert!(ssrf_imds.contains("blocked"), "{ssrf_imds}");

    let sel_err = client
        .call_tool_expect_error(
            seq,
            "wait_for",
            json!({ "selector": "#definitely-missing-el", "timeout_ms": 300 }),
        )
        .await;
    seq += 1;
    assert!(!sel_err.is_empty());

    let bad_css_err = client
        .call_tool_expect_error(seq, "click", json!({ "selector": ":::invalid[" }))
        .await;
    seq += 1;
    assert!(!bad_css_err.is_empty());

    let js_err = client
        .call_tool_expect_error(
            seq,
            "evaluate",
            json!({ "expression": "(() => { throw new Error('mcp-runtime-boom'); })()" }),
        )
        .await;
    seq += 1;
    assert!(js_err.contains("mcp-runtime-boom"), "{js_err}");

    let closed = client.call_tool(seq, "close", json!({})).await;
    assert!(closed.contains("page closed"), "{closed}");

    client.shutdown_cleanly().await;
}
