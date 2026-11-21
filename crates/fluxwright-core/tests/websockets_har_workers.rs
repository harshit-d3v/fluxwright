use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use fluxwright_core::{
    Engine, Har, HarNotFound, HarRouteOptions, JobOptions, RecordHarOptions, WebSocketEvent,
    WorkerEvent,
};
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

async fn spawn_http_server() -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let hello_hits = Arc::new(AtomicUsize::new(0));
    let seq_hits = Arc::new(AtomicUsize::new(0));
    let hits_c = hello_hits.clone();

    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let hello_hits = hits_c.clone();
            let seq_hits = seq_hits.clone();
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
                    .unwrap_or("/");
                let (ctype, extra_hdr, body): (&str, &str, Vec<u8>) = match path {
                    "/" => (
                        "text/html",
                        "",
                        b"<!doctype html><h1>pr4c</h1>".to_vec(),
                    ),
                    "/worker.js" => (
                        "application/javascript",
                        "",
                        b"self.workerValue = 21; self.emitLog = (m) => console.log(m);".to_vec(),
                    ),
                    "/api/hello" => {
                        let count = hello_hits.fetch_add(1, Ordering::SeqCst) + 1;
                        (
                            "application/json",
                            "X-Src: server\r\n",
                            format!(r#"{{"msg":"hello","hits":{count}}}"#).into_bytes(),
                        )
                    }
                    "/api/seq" => {
                        let count = seq_hits.fetch_add(1, Ordering::SeqCst) + 1;
                        (
                            "application/json",
                            "",
                            format!(r#"{{"n":{count}}}"#).into_bytes(),
                        )
                    }
                    "/api/other" => ("application/json", "", b"{\"other\":true}".to_vec()),
                    _ => ("text/plain", "", b"ok".to_vec()),
                };
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\n{extra_hdr}Content-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(&body).await;
                let _ = sock.shutdown().await;
            });
        }
    });

    (format!("http://{addr}"), hello_hits)
}

async fn spawn_ws_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else {
                    return;
                };
                while let Some(Ok(msg)) = ws.next().await {
                    match msg {
                        Message::Text(text) => {
                            let reply = format!("echo:{text}");
                            if ws.send(Message::Text(reply.into())).await.is_err() {
                                break;
                            }
                        }
                        Message::Binary(data) => {
                            let mut out = Vec::with_capacity(1 + data.len());
                            out.push(0xFF);
                            out.extend_from_slice(&data);
                            if ws.send(Message::Binary(out.into())).await.is_err() {
                                break;
                            }
                        }
                        Message::Close(_) => {
                            let _ = ws.close(None).await;
                            break;
                        }
                        _ => {}
                    }
                }
            });
        }
    });
    format!("ws://{addr}")
}

#[tokio::test(flavor = "multi_thread")]
async fn websockets_har_and_workers_end_to_end() {
    let (base, hello_hits) = spawn_http_server().await;
    let ws_url = spawn_ws_server().await;
    let eng = Engine::builder()
        .max_browsers(1)
        .max_contexts_per_browser(2)
        .memory_ceiling_mb(16_384)
        .queue_capacity(16)
        .acquire_timeout(Duration::from_secs(60))
        .build()
        .await
        .expect("engine");

    // 1. WebSockets + Dedicated Web Workers
    {
        let ctx = eng.acquire_context_with(&JobOptions::default()).await.unwrap();
        let page = ctx.new_page().await.unwrap();
        page.goto(&format!("{base}/")).await.unwrap();

        let mut ws_rx = page.subscribe_websockets();
        let ws_script = format!(
            r#"(async () => {{
                const ws = new WebSocket({ws_url_json});
                ws.binaryType = 'arraybuffer';
                await new Promise((ok, err) => {{ ws.onopen = ok; ws.onerror = err; }});
                const replies = [];
                ws.onmessage = (ev) => {{
                    if (typeof ev.data === 'string') replies.push(ev.data);
                    else replies.push(Array.from(new Uint8Array(ev.data)));
                }};
                ws.send('hello-ws');
                ws.send(new Uint8Array([1, 2, 3]));
                const deadline = Date.now() + 5000;
                while (replies.length < 2 && Date.now() < deadline) {{
                    await new Promise(r => setTimeout(r, 15));
                }}
                ws.close(1000, 'bye');
                await new Promise(r => {{ ws.onclose = r; setTimeout(r, 1000); }});
                return replies;
            }})()"#,
            ws_url_json = serde_json::to_string(&ws_url).unwrap()
        );
        let replies = page.evaluate(&ws_script).await.unwrap();
        assert_eq!(replies, json!(["echo:hello-ws", [0xFF, 1, 2, 3]]));

        let mut saw_created = false;
        let mut saw_sent_text = false;
        let mut saw_sent_bin = false;
        let mut saw_recv_text = false;
        let mut saw_recv_bin = false;
        let mut saw_closed = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !saw_closed && tokio::time::Instant::now() < deadline {
            match tokio::time::timeout_at(deadline, ws_rx.recv()).await {
                Ok(Some(ev)) => match ev {
                    WebSocketEvent::Created { url, .. } => {
                        assert_eq!(url, format!("{ws_url}/"));
                        saw_created = true;
                    }
                    WebSocketEvent::FrameSent {
                        payload, is_text, ..
                    } => {
                        if is_text && payload == b"hello-ws" {
                            saw_sent_text = true;
                        } else if !is_text && payload == [1, 2, 3] {
                            saw_sent_bin = true;
                        }
                    }
                    WebSocketEvent::FrameReceived {
                        payload, is_text, ..
                    } => {
                        if is_text && payload == b"echo:hello-ws" {
                            saw_recv_text = true;
                        } else if !is_text && payload == [0xFF, 1, 2, 3] {
                            saw_recv_bin = true;
                        }
                    }
                    WebSocketEvent::Closed { .. } => saw_closed = true,
                    WebSocketEvent::Error { .. } => {}
                },
                _ => break,
            }
        }
        assert!(
            saw_created && saw_sent_text && saw_sent_bin && saw_recv_text && saw_recv_bin && saw_closed,
            "missing WS events: created={saw_created} st={saw_sent_text} sb={saw_sent_bin} rt={saw_recv_text} rb={saw_recv_bin} closed={saw_closed}"
        );

        let mut worker_rx = page.subscribe_workers();
        page.evaluate("window.__w = new Worker('/worker.js')").await.unwrap();
        let worker = loop {
            match tokio::time::timeout(Duration::from_secs(10), worker_rx.recv()).await {
                Ok(Some(WorkerEvent::Created(w))) => break w,
                Ok(Some(_)) => continue,
                other => panic!("expected WorkerEvent::Created, got {other:?}"),
            }
        };
        assert!(worker.url().ends_with("/worker.js"), "{}", worker.url());
        assert!(page.workers().iter().any(|w| w.target_id() == worker.target_id()));
        assert!(page.worker(worker.target_id()).is_some());

        let mut eval_val = serde_json::Value::Null;
        for _ in 0..40 {
            eval_val = worker
                .evaluate(
                    "typeof self.workerValue === 'number' ? self.workerValue * 2 : null",
                    Duration::from_secs(10),
                )
                .await
                .unwrap();
            if eval_val == json!(42) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert_eq!(eval_val, json!(42));

        worker
            .evaluate("self.emitLog('worker-hi')", Duration::from_secs(5))
            .await
            .unwrap();
        let mut saw_worker_console = false;
        let c_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !saw_worker_console && tokio::time::Instant::now() < c_deadline {
            match tokio::time::timeout_at(c_deadline, worker_rx.recv()).await {
                Ok(Some(WorkerEvent::Console { target_id, message }))
                    if target_id == worker.target_id() && message.text == "worker-hi" =>
                {
                    saw_worker_console = true;
                }
                Ok(Some(_)) => continue,
                _ => break,
            }
        }
        assert!(saw_worker_console, "expected WorkerEvent::Console");

        page.evaluate("window.__w.terminate()").await.unwrap();
        let mut saw_worker_closed = false;
        let w_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !saw_worker_closed && tokio::time::Instant::now() < w_deadline {
            match tokio::time::timeout_at(w_deadline, worker_rx.recv()).await {
                Ok(Some(WorkerEvent::Closed { target_id, .. }))
                    if target_id == worker.target_id() =>
                {
                    saw_worker_closed = true;
                }
                Ok(Some(_)) => continue,
                _ => break,
            }
        }
        assert!(saw_worker_closed, "expected WorkerEvent::Closed");
        assert!(page.workers().is_empty());

        page.close().await.unwrap();
        ctx.close().await.unwrap();
    }

    // 2. HAR 1.2 recording, sequential replay, abort/fallback, and update
    let tmp_dir = std::env::temp_dir().join(format!("fw-har-{}", uuid::Uuid::new_v4()));
    let har_path = tmp_dir.join("rec.har");
    let har_update_path = tmp_dir.join("upd.har");

    {
        let opts = JobOptions::default().record_har(RecordHarOptions {
            url_filter: Some("**/api/*".into()),
            mode: HarMode::Minimal,
            ..RecordHarOptions::new(&har_path)
        });
        let rec_ctx = eng.acquire_context_with(&opts).await.unwrap();
        let rec_page = rec_ctx.new_page().await.unwrap();
        rec_page.goto(&format!("{base}/")).await.unwrap();
        let r_hello = rec_page
            .evaluate("fetch('/api/hello').then(r => r.json())")
            .await
            .unwrap();
        assert_eq!(r_hello["msg"], json!("hello"));
        let r_s1 = rec_page
            .evaluate("fetch('/api/seq').then(r => r.json())")
            .await
            .unwrap();
        let r_s2 = rec_page
            .evaluate("fetch('/api/seq').then(r => r.json())")
            .await
            .unwrap();
        assert_eq!((&r_s1["n"], &r_s2["n"]), (&json!(1), &json!(2)));
        rec_page.close().await.unwrap();
        rec_ctx.close().await.unwrap();
    }

    let recorded = Har::from_file(&har_path).unwrap();
    assert_eq!(recorded.log.version, "1.2");
    assert!(
        recorded
            .log
            .entries
            .iter()
            .all(|e| e.request.url.contains("/api/")),
        "url_filter should keep only /api/ entries"
    );

    hello_hits.store(0, Ordering::SeqCst);
    {
        let rep_ctx = eng.acquire_context_with(&JobOptions::default()).await.unwrap();
        rep_ctx
            .route_from_har(
                &har_path,
                &HarRouteOptions {
                    url: Some("**/api/*".into()),
                    not_found: HarNotFound::Abort,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let rep_page = rep_ctx.new_page().await.unwrap();
        rep_page.goto(&format!("{base}/")).await.unwrap();
        let r_hello = rep_page
            .evaluate("fetch('/api/hello').then(r => r.json())")
            .await
            .unwrap();
        assert_eq!(r_hello["msg"], json!("hello"));
        let s1 = rep_page
            .evaluate("fetch('/api/seq').then(r => r.json())")
            .await
            .unwrap();
        let s2 = rep_page
            .evaluate("fetch('/api/seq').then(r => r.json())")
            .await
            .unwrap();
        assert_eq!((&s1["n"], &s2["n"]), (&json!(1), &json!(2)));
        assert_eq!(
            hello_hits.load(Ordering::SeqCst),
            0,
            "HAR replay must not hit upstream HTTP server"
        );
        let unrecorded_ok = rep_page
            .evaluate("fetch('/api/other').then(() => true).catch(() => false)")
            .await
            .unwrap();
        assert_eq!(unrecorded_ok, json!(false));
        rep_ctx.close().await.unwrap();
    }

    {
        let fb_ctx = eng.acquire_context_with(&JobOptions::default()).await.unwrap();
        fb_ctx
            .route_from_har(
                &har_path,
                &HarRouteOptions {
                    url: Some("**/api/*".into()),
                    not_found: HarNotFound::Fallback,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let fb_page = fb_ctx.new_page().await.unwrap();
        fb_page.goto(&format!("{base}/")).await.unwrap();
        let fb_res = fb_page
            .evaluate("fetch('/api/other').then(r => r.json())")
            .await
            .unwrap();
        assert_eq!(fb_res["other"], json!(true));
        fb_ctx.close().await.unwrap();
    }

    {
        let upd_ctx = eng.acquire_context_with(&JobOptions::default()).await.unwrap();
        upd_ctx
            .route_from_har(
                &har_update_path,
                &HarRouteOptions {
                    url: Some("**/api/*".into()),
                    update: true,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let upd_page = upd_ctx.new_page().await.unwrap();
        upd_page.goto(&format!("{base}/")).await.unwrap();
        let _ = upd_page
            .evaluate("fetch('/api/other').then(r => r.json())")
            .await
            .unwrap();
        upd_ctx.close().await.unwrap();
    }
    let updated = Har::from_file(&har_update_path).unwrap();
    assert!(
        updated
            .log
            .entries
            .iter()
            .any(|e| e.request.url.ends_with("/api/other"))
    );

    let _ = std::fs::remove_dir_all(&tmp_dir);
    eng.shutdown(Duration::from_secs(5)).await.unwrap();
}
