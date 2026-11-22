use std::collections::HashMap;
use std::time::Duration;

use axum::{response::Html, routing::get, Json, Router};
use fluxwright::{BrowserEngine, TracingChunkOptions, TracingStartOptions};
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
        })
        .await
        .unwrap();

    let page = ctx.new_page().await.unwrap();
    page.set_viewport_size(800, 600).await.unwrap();
    page.goto(&format!("{base}/app")).await.unwrap();
    page.locator("#name").fill("Fluxwright").await.unwrap();
    page.locator("#go").click().await.unwrap();
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
        trace_events
            .iter()
            .any(|e| e["type"] == "before" && e["method"] == "fill"),
        "missing fill before: {trace_events:?}"
    );
    assert!(
        trace_events
            .iter()
            .any(|e| e["type"] == "before" && e["method"] == "click"),
        "missing click before: {trace_events:?}"
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

    drop(page);
    ctx.close().await.unwrap();
    eng.shutdown(Duration::from_secs(5)).await.unwrap();
    let _ = std::fs::remove_dir_all(&tmp);
}
