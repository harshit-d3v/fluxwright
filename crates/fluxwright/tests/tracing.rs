use std::collections::HashMap;
use std::time::Duration;

use axum::{response::Html, routing::get, Json, Router};
use fluxwright::{BrowserEngine, SelectEntry, TracingChunkOptions, TracingStartOptions};
use serde_json::{json, Value};

async fn engine() -> BrowserEngine {
    BrowserEngine::builder()
        .max_browsers(1)
        .max_contexts_per_browser(2)
        .memory_ceiling_mb(16_384)
        .queue_capacity(64)
        .acquire_timeout(Duration::from_secs(60))
        .build()
        .await
        .expect("engine")
}

fn crc32_verify(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

fn sha1_verify(data: &[u8]) -> String {
    let mut h0: u32 = 0x6745_2301;
    let mut h1: u32 = 0xEFCD_AB89;
    let mut h2: u32 = 0x98BA_DCFE;
    let mut h3: u32 = 0x1032_5476;
    let mut h4: u32 = 0xC3D2_E1F0;
    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());
    for chunk in msg.chunks_exact(64) {
        let mut w = [0u32; 80];
        for i in 0..16 {
            let j = i * 4;
            w[i] = u32::from_be_bytes([chunk[j], chunk[j + 1], chunk[j + 2], chunk[j + 3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) = (h0, h1, h2, h3, h4);
        for (i, &wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | ((!b) & d), 0x5A82_7999u32),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1u32),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                _ => (b ^ c ^ d, 0xCA62_C1D6),
            };
            let t = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = t;
        }
        h0 = h0.wrapping_add(a);
        h1 = h1.wrapping_add(b);
        h2 = h2.wrapping_add(c);
        h3 = h3.wrapping_add(d);
        h4 = h4.wrapping_add(e);
    }
    format!("{h0:08x}{h1:08x}{h2:08x}{h3:08x}{h4:08x}")
}

fn parse_zip_store(bytes: &[u8]) -> HashMap<String, Vec<u8>> {
    assert!(bytes.len() >= 22, "zip too short: {}", bytes.len());
    assert_eq!(
        &bytes[0..4],
        b"PK\x03\x04",
        "missing ZIP local header magic"
    );

    let eocd_pos = bytes
        .windows(4)
        .rposition(|w| w == b"PK\x05\x06")
        .expect("missing EOCD");
    let total_entries = u16::from_le_bytes([bytes[eocd_pos + 10], bytes[eocd_pos + 11]]) as usize;
    let cd_size =
        u32::from_le_bytes(bytes[eocd_pos + 12..eocd_pos + 16].try_into().unwrap()) as usize;
    let cd_offset =
        u32::from_le_bytes(bytes[eocd_pos + 16..eocd_pos + 20].try_into().unwrap()) as usize;
    assert_eq!(cd_offset + cd_size, eocd_pos);

    let mut out = HashMap::new();
    let mut pos = cd_offset;
    for _ in 0..total_entries {
        assert_eq!(&bytes[pos..pos + 4], b"PK\x01\x02", "bad CD header magic");
        let method = u16::from_le_bytes([bytes[pos + 10], bytes[pos + 11]]);
        assert_eq!(method, 0, "expected Store method 0");
        let cd_crc = u32::from_le_bytes(bytes[pos + 16..pos + 20].try_into().unwrap());
        let comp_size = u32::from_le_bytes(bytes[pos + 20..pos + 24].try_into().unwrap()) as usize;
        let uncomp_size =
            u32::from_le_bytes(bytes[pos + 24..pos + 28].try_into().unwrap()) as usize;
        assert_eq!(comp_size, uncomp_size);
        let name_len = u16::from_le_bytes([bytes[pos + 28], bytes[pos + 29]]) as usize;
        let extra_len = u16::from_le_bytes([bytes[pos + 30], bytes[pos + 31]]) as usize;
        let comment_len = u16::from_le_bytes([bytes[pos + 32], bytes[pos + 33]]) as usize;
        let lfh_offset = u32::from_le_bytes(bytes[pos + 42..pos + 46].try_into().unwrap()) as usize;
        let name = std::str::from_utf8(&bytes[pos + 46..pos + 46 + name_len])
            .unwrap()
            .to_string();
        pos += 46 + name_len + extra_len + comment_len;

        assert_eq!(&bytes[lfh_offset..lfh_offset + 4], b"PK\x03\x04");
        let lfh_name_len =
            u16::from_le_bytes([bytes[lfh_offset + 26], bytes[lfh_offset + 27]]) as usize;
        let lfh_extra_len =
            u16::from_le_bytes([bytes[lfh_offset + 28], bytes[lfh_offset + 29]]) as usize;
        let data_start = lfh_offset + 30 + lfh_name_len + lfh_extra_len;
        let payload = bytes[data_start..data_start + comp_size].to_vec();
        assert_eq!(crc32_verify(&payload), cd_crc, "CRC32 mismatch for {name}");
        out.insert(name, payload);
    }
    assert_eq!(pos, eocd_pos);
    out
}

fn parse_ndjson(bytes: &[u8]) -> Vec<Value> {
    let text = std::str::from_utf8(bytes).expect("utf-8 ndjson");
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str::<Value>(l).expect("valid json line"))
        .collect()
}

fn find_snapshot_elements<'a>(node: &'a Value, tag: &str, out: &mut Vec<&'a Value>) {
    let Some(arr) = node.as_array() else {
        return;
    };
    if arr.first().and_then(Value::as_str) == Some(tag) {
        out.push(node);
    }
    for child in arr.iter().skip(2) {
        find_snapshot_elements(child, tag, out);
    }
}

fn count_snapshot_nodes_and_text(node: &Value) -> (usize, usize) {
    if let Some(s) = node.as_str() {
        return (1, s.encode_utf16().count());
    }
    let Some(arr) = node.as_array() else {
        return (0, 0);
    };
    let mut nodes = 1usize;
    let mut chars = 0usize;
    for child in arr.iter().skip(2) {
        let (cn, cc) = count_snapshot_nodes_and_text(child);
        nodes += cn;
        chars += cc;
    }
    (nodes, chars)
}

#[tokio::test(flavor = "multi_thread")]
async fn tracing_start_stop_zip_bundle() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let app = Router::new()
        .route(
            "/app",
            get(|| async {
                Html(
                    r#"<!doctype html>
                    <html><head><title>Trace App</title></head>
                    <body style="margin:0;background:#1e293b;color:#f8fafc;padding:24px">
                      <h1 style="font-size:28px">Tracing Fixture</h1>
                      <input id="name" type="text" placeholder="Name" />
                      <button id="go" onclick="document.getElementById('out').textContent='clicked:' + document.getElementById('name').value">Go</button>
                      <div id="out" style="margin-top:16px;width:240px;height:40px;background:#0ea5e9">idle</div>
                    </body></html>"#,
                )
            }),
        )
        .route("/api/ping", get(|| async { Json(json!({ "ok": true })) }));
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let base = format!("http://127.0.0.1:{port}");
    let tmp = std::env::temp_dir().join(format!("fw-rust-trace-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let zip_path = tmp.join("nested").join("trace.zip");

    let eng = engine().await;
    let ctx = eng.acquire_context().await.unwrap();
    ctx.tracing()
        .start(TracingStartOptions {
            name: Some("rust-smoke".into()),
            title: Some("Rust Trace Title".into()),
            screenshots: true,
            snapshots: true,
            ..Default::default()
        })
        .await
        .unwrap();

    let page = ctx.new_page().await.unwrap();
    page.set_viewport_size(800, 600).await.unwrap();
    page.goto(&format!("{base}/app")).await.unwrap();
    page.locator("#name").fill("Fluxwright").await.unwrap();
    page.locator("#go").click().await.unwrap();
    let _shot = page.locator("#out").screenshot().await.unwrap();
    let _eval = page
        .locator("#out")
        .evaluate("(el) => el.textContent", None)
        .await
        .unwrap();
    page.evaluate(
        r#"(() => {
            console.log('trace-console-msg', 42);
            setTimeout(() => { throw new Error('trace-boom'); }, 10);
            return fetch('/api/ping').then(r => r.json());
        })()"#,
    )
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(120)).await;

    ctx.tracing().stop(Some(&zip_path)).await.unwrap();

    let zip_bytes = std::fs::read(&zip_path).expect("zip written");
    let files = parse_zip_store(&zip_bytes);
    assert!(files.contains_key("trace.trace"));
    assert!(files.contains_key("trace.network"));

    let trace_events = parse_ndjson(&files["trace.trace"]);
    assert!(
        trace_events.iter().any(|e| {
            e["type"] == "context-options"
                && e["title"] == "Rust Trace Title"
                && e["sdkLanguage"] == "rust"
                && e["browserName"] == "chromium"
        }),
        "missing context-options: {trace_events:?}"
    );
    assert!(
        trace_events
            .iter()
            .any(|e| e["type"] == "before" && e["method"] == "goto"),
        "missing goto before: {trace_events:?}"
    );
    assert!(
        trace_events.iter().any(|e| {
            e["type"] == "before"
                && e["class"] == "Locator"
                && e["method"] == "fill"
                && e["apiName"] == "locator.fill"
                && e["params"]["selector"] == "#name"
        }),
        "missing locator.fill before: {trace_events:?}"
    );
    assert!(
        trace_events.iter().any(|e| {
            e["type"] == "before"
                && e["class"] == "Locator"
                && e["method"] == "click"
                && e["apiName"] == "locator.click"
                && e["params"]["selector"] == "#go"
        }),
        "missing locator.click before: {trace_events:?}"
    );
    assert!(
        trace_events.iter().any(|e| {
            e["type"] == "before"
                && e["class"] == "Locator"
                && e["method"] == "screenshot"
                && e["apiName"] == "locator.screenshot"
                && e["params"]["selector"] == "#out"
        }),
        "missing locator.screenshot before: {trace_events:?}"
    );
    assert!(
        trace_events.iter().any(|e| {
            e["type"] == "before"
                && e["class"] == "Locator"
                && e["method"] == "evaluate"
                && e["apiName"] == "locator.evaluate"
                && e["params"]["selector"] == "#out"
        }),
        "missing locator.evaluate before: {trace_events:?}"
    );
    assert!(trace_events.iter().any(|e| e["type"] == "after"));
    assert!(
        trace_events.iter().any(|e| {
            e["type"] == "console"
                && e["text"]
                    .as_str()
                    .is_some_and(|t| t.contains("trace-console-msg"))
        }),
        "missing console event: {trace_events:?}"
    );
    assert!(
        trace_events
            .iter()
            .any(|e| e["type"] == "event" && e["method"] == "pageError"),
        "missing pageError event: {trace_events:?}"
    );

    let frames: Vec<&Value> = trace_events
        .iter()
        .filter(|e| e["type"] == "screencast-frame")
        .collect();
    assert!(!frames.is_empty(), "expected at least one screencast-frame");
    for f in frames {
        let sha = f["sha1"].as_str().expect("sha1 string");
        assert!(sha.ends_with(".jpeg"), "expected .jpeg suffix: {sha}");
        let res_key = format!("resources/{sha}");
        let jpeg = files.get(&res_key).expect("missing jpeg resource in zip");
        assert!(
            jpeg.len() >= 3 && jpeg[0..3] == [0xFF, 0xD8, 0xFF],
            "invalid JPEG magic in {res_key}"
        );
        assert_eq!(format!("{}.jpeg", sha1_verify(jpeg)), sha);
    }

    let snapshots_by_name: HashMap<&str, &Value> = trace_events
        .iter()
        .filter(|e| e["type"] == "frame-snapshot")
        .map(|e| {
            let s = &e["snapshot"];
            (
                s["snapshotName"].as_str().expect("snapshotName"),
                s,
            )
        })
        .collect();
    assert!(
        !snapshots_by_name.is_empty(),
        "expected frame-snapshot events when snapshots: true"
    );

    let fill_before = trace_events
        .iter()
        .find(|e| e["type"] == "before" && e["apiName"] == "locator.fill")
        .expect("fill before");
    let fill_call_id = fill_before["callId"].as_str().unwrap();
    let fill_before_snap_name = fill_before["beforeSnapshot"]
        .as_str()
        .expect("beforeSnapshot on locator.fill");
    assert_eq!(fill_before_snap_name, format!("before@{fill_call_id}"));
    let fill_after = trace_events
        .iter()
        .find(|e| e["type"] == "after" && e["callId"] == fill_call_id)
        .expect("fill after");
    let fill_after_snap_name = fill_after["afterSnapshot"]
        .as_str()
        .expect("afterSnapshot on locator.fill");
    assert_eq!(fill_after_snap_name, format!("after@{fill_call_id}"));

    let fill_after_snap = snapshots_by_name
        .get(fill_after_snap_name)
        .expect("matching frame-snapshot for afterSnapshot");
    assert_eq!(fill_after_snap["callId"], fill_call_id);
    assert_eq!(fill_after_snap["frameId"], "main");
    assert_eq!(fill_after_snap["doctype"], "html");
    assert_eq!(fill_after_snap["isMainFrame"], true);
    assert_eq!(fill_after_snap["resourceOverrides"], json!([]));
    assert!(fill_after_snap["frameUrl"]
        .as_str()
        .is_some_and(|u| u.ends_with("/app")));
    assert_eq!(fill_after_snap["viewport"], json!({ "width": 800, "height": 600 }));
    assert_eq!(fill_after_snap["html"][0], "HTML");

    let mut inputs = Vec::new();
    find_snapshot_elements(&fill_after_snap["html"], "INPUT", &mut inputs);
    assert!(
        inputs.iter().any(|el| {
            el[1]["id"] == "name" && el[1]["__playwright_value__"] == "Fluxwright"
        }),
        "expected filled input value in afterSnapshot: {inputs:?}"
    );

    let net_events = parse_ndjson(&files["trace.network"]);
    assert!(
        net_events.iter().any(|e| {
            e["type"] == "resource-snapshot"
                && e["snapshot"]["request"]["url"]
                    .as_str()
                    .is_some_and(|u| u.ends_with("/app"))
        }),
        "missing /app network entry: {net_events:?}"
    );
    assert!(
        net_events.iter().any(|e| {
            e["type"] == "resource-snapshot"
                && e["snapshot"]["request"]["url"]
                    .as_str()
                    .is_some_and(|u| u.ends_with("/api/ping"))
        }),
        "missing /api/ping network entry: {net_events:?}"
    );

    drop(page);
    ctx.close().await.unwrap();
    eng.shutdown(Duration::from_secs(5)).await.unwrap();
    let _ = std::fs::remove_dir_all(&tmp);
}

#[tokio::test(flavor = "multi_thread")]
async fn tracing_chunks_and_validation() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let app = Router::new()
        .route(
            "/one",
            get(|| async { Html("<h1 style='color:red'>Page One</h1>") }),
        )
        .route(
            "/two",
            get(|| async { Html("<h1 style='color:blue'>Page Two</h1>") }),
        );
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let base = format!("http://127.0.0.1:{port}");

    let tmp = std::env::temp_dir().join(format!("fw-rust-chunks-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let c1_zip = tmp.join("chunk1.zip");
    let c2_zip = tmp.join("chunk2.zip");

    let eng = engine().await;
    let ctx = eng.acquire_context().await.unwrap();
    let tracing = ctx.tracing();

    // Validation before start.
    assert!(tracing
        .start_chunk(TracingChunkOptions::default())
        .await
        .is_err());
    assert!(tracing.stop_chunk(None).await.is_err());
    assert!(tracing.stop(Some(&c1_zip)).await.is_err());
    tracing.stop(None).await.unwrap(); // no-op when not started

    // Open page before starting tracing to verify existing pages attach too.
    let page = ctx.new_page().await.unwrap();
    tracing
        .start(TracingStartOptions {
            name: Some("c1".into()),
            title: Some("Chunk 1".into()),
            screenshots: true,
            snapshots: false,
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(tracing.start(TracingStartOptions::default()).await.is_err());

    page.goto(&format!("{base}/one")).await.unwrap();
    tokio::time::sleep(Duration::from_millis(80)).await;
    tracing.stop_chunk(Some(&c1_zip)).await.unwrap();
    assert!(tracing.stop_chunk(None).await.is_err());

    // Start second chunk, visit /two, and verify isolation.
    tracing
        .start_chunk(TracingChunkOptions {
            name: Some("c2".into()),
            title: Some("Chunk 2".into()),
        })
        .await
        .unwrap();
    page.goto(&format!("{base}/two")).await.unwrap();
    tokio::time::sleep(Duration::from_millis(80)).await;
    tracing.stop_chunk(Some(&c2_zip)).await.unwrap();

    // Start a third chunk and discard via stop_chunk(None), then stop(None).
    tracing
        .start_chunk(TracingChunkOptions::default())
        .await
        .unwrap();
    tracing.stop_chunk(None).await.unwrap();
    tracing.stop(None).await.unwrap();

    let f1 = parse_zip_store(&std::fs::read(&c1_zip).unwrap());
    let f2 = parse_zip_store(&std::fs::read(&c2_zip).unwrap());
    let ev1 = parse_ndjson(&f1["trace.trace"]);
    let ev2 = parse_ndjson(&f2["trace.trace"]);

    assert!(ev1
        .iter()
        .any(|e| e["type"] == "context-options" && e["title"] == "Chunk 1"));
    assert!(ev2
        .iter()
        .any(|e| e["type"] == "context-options" && e["title"] == "Chunk 2"));
    assert!(ev1.iter().any(|e| {
        e["type"] == "before"
            && e["params"]["url"]
                .as_str()
                .is_some_and(|u| u.ends_with("/one"))
    }));
    assert!(!ev1.iter().any(|e| {
        e["type"] == "before"
            && e["params"]["url"]
                .as_str()
                .is_some_and(|u| u.ends_with("/two"))
    }));
    assert!(ev2.iter().any(|e| {
        e["type"] == "before"
            && e["params"]["url"]
                .as_str()
                .is_some_and(|u| u.ends_with("/two"))
    }));
    assert!(!ev2.iter().any(|e| {
        e["type"] == "before"
            && e["params"]["url"]
                .as_str()
                .is_some_and(|u| u.ends_with("/one"))
    }));
    assert!(f2
        .keys()
        .any(|k| k.starts_with("resources/") && k.ends_with(".jpeg")));
    assert_eq!(
        ev1.iter().filter(|e| e["type"] == "frame-snapshot").count(),
        0,
        "snapshots: false must emit 0 frame-snapshot events"
    );
    assert_eq!(
        ev2.iter().filter(|e| e["type"] == "frame-snapshot").count(),
        0,
        "snapshots: false must emit 0 frame-snapshot events in chunk 2"
    );
    assert!(ev1
        .iter()
        .filter(|e| e["type"] == "before")
        .all(|e| e.get("beforeSnapshot").is_none()));
    assert!(ev1
        .iter()
        .filter(|e| e["type"] == "after")
        .all(|e| e.get("afterSnapshot").is_none()));

    drop(page);
    ctx.close().await.unwrap();
    eng.shutdown(Duration::from_secs(5)).await.unwrap();
    let _ = std::fs::remove_dir_all(&tmp);
}

#[tokio::test(flavor = "multi_thread")]
async fn tracing_unsubscribed_popup_cancellation_and_chunk_isolation() {
    use std::collections::HashSet;
    use std::sync::Arc;
    use tokio::sync::Notify;

    let popup_hit = Arc::new(Notify::new());
    let slow_entered = Arc::new(Notify::new());
    let slow_release = Arc::new(Notify::new());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let app = Router::new()
        .route(
            "/opener",
            get(|| async {
                Html(
                    r#"<!doctype html>
                    <html><body>
                      <button id="open" onclick="window.open('/popup', '_blank')">Open</button>
                    </body></html>"#,
                )
            }),
        )
        .route(
            "/popup",
            get(|| async {
                Html(
                    r#"<!doctype html>
                    <html><body style="margin:0;background:#16a34a;width:400px;height:300px">
                      <h1>Popup Page</h1>
                      <script>
                        setTimeout(() => {
                          console.log('popup-unsub-log');
                          fetch('/popup-ping');
                        }, 150);
                      </script>
                    </body></html>"#,
                )
            }),
        )
        .route(
            "/popup-ping",
            get({
                let popup_hit = Arc::clone(&popup_hit);
                move || {
                    let popup_hit = Arc::clone(&popup_hit);
                    async move {
                        popup_hit.notify_one();
                        Json(json!({ "popup": true }))
                    }
                }
            }),
        )
        .route(
            "/slow",
            get({
                let slow_entered = Arc::clone(&slow_entered);
                let slow_release = Arc::clone(&slow_release);
                move || {
                    let slow_entered = Arc::clone(&slow_entered);
                    let slow_release = Arc::clone(&slow_release);
                    async move {
                        slow_entered.notify_one();
                        slow_release.notified().await;
                        "done"
                    }
                }
            }),
        );
    let srv = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let base = format!("http://127.0.0.1:{port}");

    let tmp = std::env::temp_dir().join(format!("fw-rust-trace-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let popup_zip = tmp.join("popup.zip");
    let c1_zip = tmp.join("iso-c1.zip");
    let c2_zip = tmp.join("iso-c2.zip");

    let eng = engine().await;
    let ctx = eng.acquire_context().await.unwrap();
    let tracing = ctx.tracing();

    // 1. Unsubscribed popup tracing (NO subscribe_popups() listener attached).
    tracing
        .start(TracingStartOptions {
            name: Some("unsub-popup".into()),
            title: Some("Unsubscribed Popup".into()),
            screenshots: true,
            snapshots: false,
            sources: true,
        })
        .await
        .unwrap();

    let page = ctx.new_page().await.unwrap();
    page.goto(&format!("{base}/opener")).await.unwrap();
    page.locator("#open").click().await.unwrap();

    tokio::time::timeout(Duration::from_secs(10), popup_hit.notified())
        .await
        .expect("popup fetched /popup-ping");
    tokio::time::sleep(Duration::from_millis(120)).await;

    // 2. Cancelled future emits `after` with CancelledError instead of leaving an unclosed `before`.
    let cancelled = tokio::time::timeout(
        Duration::from_millis(80),
        page.locator("#does-not-exist").click(),
    )
    .await;
    assert!(cancelled.is_err(), "expected timeout cancelling locator.click");

    tracing.stop(Some(&popup_zip)).await.unwrap();

    let popup_files = parse_zip_store(&std::fs::read(&popup_zip).unwrap());
    let popup_events = parse_ndjson(&popup_files["trace.trace"]);
    let page_event_ids: HashSet<&str> = popup_events
        .iter()
        .filter(|e| e["type"] == "event" && e["class"] == "BrowserContext" && e["method"] == "page")
        .filter_map(|e| e["pageId"].as_str())
        .collect();
    assert!(
        page_event_ids.len() >= 2,
        "expected BrowserContext.page for opener and unsubscribed popup, got: {page_event_ids:?}"
    );
    assert!(
        popup_events.iter().any(|e| {
            e["type"] == "console"
                && e["text"]
                    .as_str()
                    .is_some_and(|t| t.contains("popup-unsub-log"))
        }),
        "missing popup console event: {popup_events:?}"
    );
    let popup_net = parse_ndjson(&popup_files["trace.network"]);
    assert!(
        popup_net.iter().any(|e| {
            e["type"] == "resource-snapshot"
                && e["snapshot"]["request"]["url"]
                    .as_str()
                    .is_some_and(|u| u.ends_with("/popup-ping"))
        }),
        "missing /popup-ping network entry: {popup_net:?}"
    );
    assert!(
        popup_events.iter().any(|e| e["type"] == "screencast-frame"),
        "expected screencast-frame in popup trace"
    );
    let cancelled_before = popup_events
        .iter()
        .find(|e| {
            e["type"] == "before"
                && e["class"] == "Locator"
                && e["apiName"] == "locator.click"
                && e["params"]["selector"] == "#does-not-exist"
        })
        .expect("missing cancelled locator.click before");
    let cancelled_after = popup_events
        .iter()
        .find(|e| e["type"] == "after" && e["callId"] == cancelled_before["callId"])
        .expect("cancelled future must still emit after");
    assert_eq!(cancelled_after["error"]["name"], "CancelledError");
    assert_eq!(cancelled_after["error"]["message"], "cancelled");

    // 3. Cross-chunk isolation & stop(Some(path)) after stop_chunk still tearing down tracing.
    tracing
        .start(TracingStartOptions {
            name: Some("iso".into()),
            ..Default::default()
        })
        .await
        .unwrap();

    let (eval_res, chunk_res) = tokio::join!(
        page.evaluate("fetch('/slow').then(r => r.text())"),
        async {
            tokio::time::timeout(Duration::from_secs(10), slow_entered.notified())
                .await
                .expect("slow request entered");
            tracing.stop_chunk(Some(&c1_zip)).await?;
            tracing
                .start_chunk(TracingChunkOptions {
                    name: Some("iso-2".into()),
                    title: Some("Iso 2".into()),
                })
                .await?;
            slow_release.notify_one();
            Ok::<_, fluxwright::Error>(())
        }
    );
    eval_res.unwrap();
    chunk_res.unwrap();

    tracing.stop_chunk(Some(&c2_zip)).await.unwrap();

    // Calling stop(Some(path)) after stop_chunk returns Err AND tears down tracing cleanly.
    assert!(tracing.stop(Some(&c2_zip)).await.is_err());
    // Re-starting tracing succeeds immediately (not stuck active).
    tracing.start(TracingStartOptions::default()).await.unwrap();
    tracing.stop(None).await.unwrap();

    let ev1 = parse_ndjson(&parse_zip_store(&std::fs::read(&c1_zip).unwrap())["trace.trace"]);
    let ev2 = parse_ndjson(&parse_zip_store(&std::fs::read(&c2_zip).unwrap())["trace.trace"]);
    let slow_before = ev1
        .iter()
        .find(|e| e["type"] == "before" && e["method"] == "evaluate")
        .expect("slow evaluate before in chunk 1");
    assert!(
        !ev2.iter()
            .any(|e| e["type"] == "after" && e["callId"] == slow_before["callId"]),
        "chunk 1 evaluate must not inject orphaned after into chunk 2: {ev2:?}"
    );

    drop(page);
    ctx.close().await.unwrap();
    eng.shutdown(Duration::from_secs(5)).await.unwrap();
    srv.abort();
    let _ = std::fs::remove_dir_all(&tmp);
}

#[tokio::test(flavor = "multi_thread")]
async fn tracing_dom_snapshots_standalone_form_state_and_bounds() {
    use std::sync::Arc;
    use tokio::sync::Notify;

    let slow_entered = Arc::new(Notify::new());
    let slow_release = Arc::new(Notify::new());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let app = Router::new()
        .route(
            "/form",
            get(|| async {
                Html(
                    r#"<!doctype html>
                    <html><head><title>DOM Snapshots</title></head>
                    <body>
                      <input id="name" type="text" />
                      <input id="agree" type="checkbox" />
                      <textarea id="bio"></textarea>
                      <select id="role">
                        <option value="dev">Dev</option>
                        <option value="ops">Ops</option>
                      </select>
                      <script>window.__secret_script_marker__ = "SECRET_SCRIPT_BODY_123";</script>
                    </body></html>"#,
                )
            }),
        )
        .route(
            "/large",
            get(|| async {
                let mut body = String::from("<!doctype html><html><body><div id='wrap'>");
                for i in 0..4500 {
                    body.push_str(&format!("<span>n{i}</span>"));
                }
                body.push_str("</div><pre id='big'>");
                body.push_str(&"X".repeat(25_000));
                body.push_str("</pre></body></html>");
                Html(body)
            }),
        )
        .route(
            "/slow",
            get({
                let slow_entered = Arc::clone(&slow_entered);
                let slow_release = Arc::clone(&slow_release);
                move || {
                    let slow_entered = Arc::clone(&slow_entered);
                    let slow_release = Arc::clone(&slow_release);
                    async move {
                        slow_entered.notify_one();
                        slow_release.notified().await;
                        "ok"
                    }
                }
            }),
        );
    let srv = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let base = format!("http://127.0.0.1:{port}");

    let tmp = std::env::temp_dir().join(format!("fw-rust-dom-snaps-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let standalone_zip = tmp.join("standalone.zip");
    let bounds_zip = tmp.join("bounds.zip");
    let iso1_zip = tmp.join("iso1.zip");
    let iso2_zip = tmp.join("iso2.zip");
    let disabled_zip = tmp.join("disabled.zip");

    let eng = engine().await;
    let ctx = eng.acquire_context().await.unwrap();
    let page = ctx.new_page().await.unwrap();

    // 1. Standalone DOM snapshots: snapshots: true, screenshots: false.
    ctx.tracing()
        .start(TracingStartOptions {
            name: Some("dom-only".into()),
            screenshots: false,
            snapshots: true,
            ..Default::default()
        })
        .await
        .unwrap();

    page.goto(&format!("{base}/form")).await.unwrap();
    page.locator("#name").fill("Alice").await.unwrap();
    page.locator("#agree").check().await.unwrap();
    page.locator("#bio").fill("Systems").await.unwrap();
    page.locator("#role")
        .select_option(&[SelectEntry::Value("ops".into())], false)
        .await
        .unwrap();

    ctx.tracing().stop(Some(&standalone_zip)).await.unwrap();

    let standalone_files = parse_zip_store(&std::fs::read(&standalone_zip).unwrap());
    let trace_raw = std::str::from_utf8(&standalone_files["trace.trace"]).unwrap();
    assert!(
        !trace_raw.contains("SECRET_SCRIPT_BODY_123"),
        "script body must be stripped from DOM snapshots"
    );
    assert!(
        !standalone_files.keys().any(|k| k.ends_with(".jpeg")),
        "screenshots: false must not store JPEG frames"
    );

    let events = parse_ndjson(&standalone_files["trace.trace"]);
    assert_eq!(
        events.iter().filter(|e| e["type"] == "screencast-frame").count(),
        0,
        "screenshots: false must emit 0 screencast-frame events"
    );
    let snaps: HashMap<&str, &Value> = events
        .iter()
        .filter(|e| e["type"] == "frame-snapshot")
        .map(|e| (e["snapshot"]["snapshotName"].as_str().unwrap(), &e["snapshot"]))
        .collect();
    assert!(!snaps.is_empty());

    // Every before/after reference resolves to a frame-snapshot event, and SCRIPT nodes are stripped.
    for b in events.iter().filter(|e| e["type"] == "before") {
        let name = b["beforeSnapshot"].as_str().expect("beforeSnapshot");
        assert!(snaps.contains_key(name), "missing snapshot {name}");
    }
    for a in events.iter().filter(|e| e["type"] == "after") {
        let name = a["afterSnapshot"].as_str().expect("afterSnapshot");
        assert!(snaps.contains_key(name), "missing snapshot {name}");
    }

    // Check final selectOption afterSnapshot has live form state on INPUT, CHECKBOX, TEXTAREA, SELECT, OPTION + stripped SCRIPT.
    let sel_after = events
        .iter()
        .filter(|e| e["type"] == "after")
        .last()
        .expect("last after");
    let final_snap = snaps[sel_after["afterSnapshot"].as_str().unwrap()];
    let mut scripts = Vec::new();
    find_snapshot_elements(&final_snap["html"], "SCRIPT", &mut scripts);
    assert!(!scripts.is_empty());
    for sc in scripts {
        assert_eq!(*sc, json!(["SCRIPT", {}]));
    }

    let mut inputs = Vec::new();
    find_snapshot_elements(&final_snap["html"], "INPUT", &mut inputs);
    assert!(inputs.iter().any(|el| {
        el[1]["id"] == "name" && el[1]["__playwright_value__"] == "Alice"
    }));
    assert!(inputs.iter().any(|el| {
        el[1]["id"] == "agree" && el[1]["__playwright_checked__"] == true
    }));

    let mut textareas = Vec::new();
    find_snapshot_elements(&final_snap["html"], "TEXTAREA", &mut textareas);
    assert!(textareas.iter().any(|el| {
        el[1]["id"] == "bio" && el[1]["__playwright_value__"] == "Systems"
    }));

    let mut selects = Vec::new();
    find_snapshot_elements(&final_snap["html"], "SELECT", &mut selects);
    assert!(selects.iter().any(|el| {
        el[1]["id"] == "role" && el[1]["__playwright_value__"] == "ops"
    }));

    let mut options = Vec::new();
    find_snapshot_elements(&final_snap["html"], "OPTION", &mut options);
    assert!(options.iter().any(|el| {
        el[1]["value"] == "dev" && el[1]["__playwright_selected__"] == false
    }));
    assert!(options.iter().any(|el| {
        el[1]["value"] == "ops" && el[1]["__playwright_selected__"] == true
    }));

    // 2. Bounded node count (4000) and text length (16384 chars) on a large page.
    ctx.tracing()
        .start(TracingStartOptions {
            snapshots: true,
            screenshots: false,
            ..Default::default()
        })
        .await
        .unwrap();
    page.goto(&format!("{base}/large")).await.unwrap();
    page.evaluate("1 + 1").await.unwrap();
    ctx.tracing().stop(Some(&bounds_zip)).await.unwrap();

    let bounds_events = parse_ndjson(&parse_zip_store(&std::fs::read(&bounds_zip).unwrap())["trace.trace"]);
    let eval_before = bounds_events
        .iter()
        .find(|e| e["type"] == "before" && e["method"] == "evaluate")
        .expect("eval before");
    let large_snap = bounds_events
        .iter()
        .find(|e| {
            e["type"] == "frame-snapshot"
                && e["snapshot"]["snapshotName"] == eval_before["beforeSnapshot"]
        })
        .expect("large frame-snapshot");
    let (node_count, text_chars) = count_snapshot_nodes_and_text(&large_snap["snapshot"]["html"]);
    assert!(
        node_count <= 4000,
        "serialized node count {node_count} must be capped at 4000"
    );
    assert!(
        text_chars <= 16384,
        "serialized text length {text_chars} must be capped at 16384"
    );

    // 3. Cross-chunk isolation with snapshots: true (in-flight action spanning chunks must not leak afterSnapshot).
    ctx.tracing()
        .start(TracingStartOptions {
            snapshots: true,
            screenshots: false,
            ..Default::default()
        })
        .await
        .unwrap();
    let (eval_res, chunk_res) = tokio::join!(
        page.evaluate("fetch('/slow').then(r => r.text())"),
        async {
            tokio::time::timeout(Duration::from_secs(10), slow_entered.notified())
                .await
                .expect("slow request entered");
            ctx.tracing().stop_chunk(Some(&iso1_zip)).await?;
            ctx.tracing()
                .start_chunk(TracingChunkOptions {
                    name: Some("iso2".into()),
                    title: None,
                })
                .await?;
            slow_release.notify_one();
            Ok::<_, fluxwright::Error>(())
        }
    );
    eval_res.unwrap();
    chunk_res.unwrap();
    ctx.tracing().stop(Some(&iso2_zip)).await.unwrap();

    let iso1_ev = parse_ndjson(&parse_zip_store(&std::fs::read(&iso1_zip).unwrap())["trace.trace"]);
    let iso2_ev = parse_ndjson(&parse_zip_store(&std::fs::read(&iso2_zip).unwrap())["trace.trace"]);
    let slow_call_id = iso1_ev
        .iter()
        .find(|e| e["type"] == "before" && e["method"] == "evaluate")
        .expect("slow before")["callId"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        !iso2_ev.iter().any(|e| {
            (e["type"] == "after" && e["callId"] == slow_call_id)
                || (e["type"] == "frame-snapshot" && e["snapshot"]["callId"] == slow_call_id)
        }),
        "cross-chunk snapshot or after leaked into chunk 2: {iso2_ev:?}"
    );

    // 4. snapshots: false emits 0 frame-snapshot events.
    ctx.tracing()
        .start(TracingStartOptions {
            snapshots: false,
            screenshots: false,
            ..Default::default()
        })
        .await
        .unwrap();
    page.goto(&format!("{base}/form")).await.unwrap();
    page.locator("#name").fill("Bob").await.unwrap();
    page.evaluate("2 + 2").await.unwrap();
    ctx.tracing().stop(Some(&disabled_zip)).await.unwrap();
    let disabled_ev = parse_ndjson(&parse_zip_store(&std::fs::read(&disabled_zip).unwrap())["trace.trace"]);
    assert_eq!(
        disabled_ev.iter().filter(|e| e["type"] == "frame-snapshot").count(),
        0
    );
    assert!(disabled_ev
        .iter()
        .filter(|e| e["type"] == "before")
        .all(|e| e.get("beforeSnapshot").is_none()));
    assert!(disabled_ev
        .iter()
        .filter(|e| e["type"] == "after")
        .all(|e| e.get("afterSnapshot").is_none()));

    drop(page);
    ctx.close().await.unwrap();
    eng.shutdown(Duration::from_secs(5)).await.unwrap();
    srv.abort();
    let _ = std::fs::remove_dir_all(&tmp);
}
