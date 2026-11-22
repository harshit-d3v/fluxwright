use std::collections::HashMap;
use std::time::Duration;

use axum::{response::Html, routing::get, Json, Router};
use fluxwright::{
    expect_locator, expect_page, BrowserEngine, NetEvent, SnapshotOptions, TracingStartOptions,
    WaitState,
};
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

fn parse_zip_store(bytes: &[u8]) -> HashMap<String, Vec<u8>> {
    assert!(bytes.len() >= 22, "zip too short: {}", bytes.len());
    assert_eq!(&bytes[0..4], b"PK\x03\x04", "missing ZIP local header magic");

    let eocd_pos = bytes
        .windows(4)
        .rposition(|w| w == b"PK\x05\x06")
        .expect("missing EOCD");
    let total_entries = u16::from_le_bytes([bytes[eocd_pos + 10], bytes[eocd_pos + 11]]) as usize;
    let cd_offset =
        u32::from_le_bytes(bytes[eocd_pos + 16..eocd_pos + 20].try_into().unwrap()) as usize;

    let mut out = HashMap::new();
    let mut pos = cd_offset;
    for _ in 0..total_entries {
        assert_eq!(&bytes[pos..pos + 4], b"PK\x01\x02", "bad CD header magic");
        let comp_size = u32::from_le_bytes(bytes[pos + 20..pos + 24].try_into().unwrap()) as usize;
        let name_len = u16::from_le_bytes([bytes[pos + 28], bytes[pos + 29]]) as usize;
        let extra_len = u16::from_le_bytes([bytes[pos + 30], bytes[pos + 31]]) as usize;
        let comment_len = u16::from_le_bytes([bytes[pos + 32], bytes[pos + 33]]) as usize;
        let lfh_offset =
            u32::from_le_bytes(bytes[pos + 42..pos + 46].try_into().unwrap()) as usize;
        let name = std::str::from_utf8(&bytes[pos + 46..pos + 46 + name_len])
            .unwrap()
            .to_string();
        pos += 46 + name_len + extra_len + comment_len;

        let lfh_name_len =
            u16::from_le_bytes([bytes[lfh_offset + 26], bytes[lfh_offset + 27]]) as usize;
        let lfh_extra_len =
            u16::from_le_bytes([bytes[lfh_offset + 28], bytes[lfh_offset + 29]]) as usize;
        let data_start = lfh_offset + 30 + lfh_name_len + lfh_extra_len;
        out.insert(name, bytes[data_start..data_start + comp_size].to_vec());
    }
    out
}

fn parse_ndjson(bytes: &[u8]) -> Vec<Value> {
    std::str::from_utf8(bytes)
        .expect("utf-8 ndjson")
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str::<Value>(l).expect("valid json line"))
        .collect()
}

/// Fix #1: Stale isolated-world cache must be cleared on navigation (and recovered in
/// `query_once`) so post-navigation `count()`, `is_visible()`, `is_hidden()`,
/// `all_text_contents()`, and `expect_locator()` always succeed.
#[tokio::test(flavor = "multi_thread")]
async fn stale_world_cache_cleared_across_navigations() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let app = Router::new()
        .route(
            "/p1",
            get(|| async {
                Html(
                    r#"<!doctype html>
                    <h1>Page One</h1>
                    <a id="go-p3" href="/p3">Go Three</a>"#,
                )
            }),
        )
        .route(
            "/p2",
            get(|| async {
                Html(
                    r#"<!doctype html>
                    <h1>Page Two</h1>
                    <p class="item">Alpha</p>
                    <p class="item">Beta</p>"#,
                )
            }),
        )
        .route("/p3", get(|| async { Html("<!doctype html><h1>Page Three</h1>") }));
    let srv = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let base = format!("http://127.0.0.1:{port}");

    let eng = engine().await;
    let page = eng.acquire().await.unwrap();

    // First page populates the isolated-world cache.
    page.goto(&format!("{base}/p1")).await.unwrap();
    assert_eq!(page.locator("h1").count().await.unwrap(), 1);
    assert!(page.locator("h1").is_visible().await.unwrap());
    assert!(page.locator("#missing").is_hidden().await.unwrap());
    expect_locator(&page.locator("h1"))
        .to_have_text("Page One")
        .await
        .unwrap();

    // Navigating to /p2 must not fail with stale context id.
    page.goto(&format!("{base}/p2")).await.unwrap();
    assert_eq!(page.locator("h1").count().await.unwrap(), 1);
    assert!(page.locator("h1").is_visible().await.unwrap());
    assert_eq!(
        page.locator(".item").all_text_contents().await.unwrap(),
        vec!["Alpha", "Beta"]
    );
    expect_locator(&page.locator("h1"))
        .to_have_text("Page Two")
        .await
        .unwrap();

    // Reloading /p2 must keep queries working.
    page.reload().await.unwrap();
    assert_eq!(page.locator("h1").count().await.unwrap(), 1);
    expect_locator(&page.locator("h1"))
        .to_have_text("Page Two")
        .await
        .unwrap();

    // History navigation (go_back) must keep queries working.
    assert!(page.go_back().await.unwrap());
    assert_eq!(page.locator("h1").count().await.unwrap(), 1);
    expect_locator(&page.locator("h1"))
        .to_have_text("Page One")
        .await
        .unwrap();

    // Link-click navigation (no page.goto) must invalidate via Page.frameNavigated.
    page.locator("#go-p3").click().await.unwrap();
    expect_page(&page)
        .to_have_url(format!("{base}/p3"))
        .await
        .unwrap();
    assert_eq!(page.locator("h1").count().await.unwrap(), 1);
    expect_locator(&page.locator("h1"))
        .to_have_text("Page Three")
        .await
        .unwrap();

    // set_content must also leave a fresh isolated world.
    page.set_content("<h1>Inline Content</h1>").await.unwrap();
    assert_eq!(page.locator("h1").count().await.unwrap(), 1);
    expect_locator(&page.locator("h1"))
        .to_have_text("Inline Content")
        .await
        .unwrap();

    drop(page);
    eng.shutdown(Duration::from_secs(5)).await.unwrap();
    srv.abort();
}

/// Fix #3: Shadow DOM `@eN` refs from `aria_snapshot()` must pierce open shadow roots during
/// `elementFromPoint` hit-testing instead of timing out with `obscured by <host>`.
#[tokio::test(flavor = "multi_thread")]
async fn shadow_dom_aria_ref_click_succeeds() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let app = Router::new().route(
        "/",
        get(|| async {
            Html(
                r#"<!doctype html>
                <div id="host"></div>
                <div id="outer-host"></div>
                <div id="out">idle</div>
                <script>
                  const shadow = document.getElementById('host').attachShadow({ mode: 'open' });
                  const btn = document.createElement('button');
                  btn.textContent = 'Shadow Action';
                  btn.style.padding = '8px 16px';
                  btn.onclick = () => { document.getElementById('out').textContent = 'shadow-clicked'; };
                  shadow.appendChild(btn);

                  const outer = document.getElementById('outer-host').attachShadow({ mode: 'open' });
                  const innerHost = document.createElement('div');
                  outer.appendChild(innerHost);
                  const inner = innerHost.attachShadow({ mode: 'open' });
                  const nestedBtn = document.createElement('button');
                  nestedBtn.textContent = 'Nested Shadow Action';
                  nestedBtn.style.padding = '8px 16px';
                  nestedBtn.onclick = () => { document.getElementById('out').textContent = 'nested-clicked'; };
                  inner.appendChild(nestedBtn);
                </script>"#,
            )
        }),
    );
    let srv = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let eng = engine().await;
    let page = eng.acquire().await.unwrap();
    page.goto(&format!("http://127.0.0.1:{port}/")).await.unwrap();

    let snap = page
        .aria_snapshot_with(&SnapshotOptions {
            interactive_only: true,
            ..Default::default()
        })
        .await
        .unwrap();
    let shadow_ref = snap
        .refs
        .iter()
        .find(|r| r.role == "button" && r.name == "Shadow Action")
        .unwrap_or_else(|| panic!("missing Shadow Action ref in {:?}", snap.refs));
    let nested_ref = snap
        .refs
        .iter()
        .find(|r| r.role == "button" && r.name == "Nested Shadow Action")
        .unwrap_or_else(|| panic!("missing Nested Shadow Action ref in {:?}", snap.refs));

    page.click(format!("@{}", shadow_ref.id)).await.unwrap();
    assert_eq!(
        page.text_content("#out").await.unwrap().as_deref(),
        Some("shadow-clicked")
    );

    page.click(format!("@{}", nested_ref.id)).await.unwrap();
    assert_eq!(
        page.text_content("#out").await.unwrap().as_deref(),
        Some("nested-clicked")
    );

    drop(page);
    eng.shutdown(Duration::from_secs(5)).await.unwrap();
    srv.abort();
}

/// Fix #4: Detached `@eN` refs (known from the latest snapshot, then removed from the DOM)
/// must satisfy `wait_for_selector_state(Detached | Hidden)` and report `count() == 0`,
/// `is_visible() == false`, `is_hidden() == true`, while unknown refs remain fatal errors.
#[tokio::test(flavor = "multi_thread")]
async fn detached_aria_ref_state_and_queries() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let app = Router::new().route(
        "/",
        get(|| async {
            Html(
                r#"<!doctype html>
                <div id="wrap">
                  <button id="ephemeral">Remove Me</button>
                  <button id="delayed">Remove Later</button>
                  <button id="stay">Stay</button>
                </div>"#,
            )
        }),
    );
    let srv = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let eng = engine().await;
    let page = eng.acquire().await.unwrap();
    page.goto(&format!("http://127.0.0.1:{port}/")).await.unwrap();

    let snap = page
        .aria_snapshot_with(&SnapshotOptions {
            interactive_only: true,
            ..Default::default()
        })
        .await
        .unwrap();
    let eph = snap
        .refs
        .iter()
        .find(|r| r.role == "button" && r.name == "Remove Me")
        .expect("ephemeral ref");
    let delayed = snap
        .refs
        .iter()
        .find(|r| r.role == "button" && r.name == "Remove Later")
        .expect("delayed ref");
    let eph_sel = format!("@{}", eph.id);
    let delayed_sel = format!("@{}", delayed.id);

    assert_eq!(page.count(&eph_sel).await.unwrap(), 1);
    assert!(page.is_visible(&eph_sel).await.unwrap());
    assert!(!page.is_hidden(&eph_sel).await.unwrap());

    // Remove the node from the DOM.
    page.evaluate("document.getElementById('ephemeral').remove()")
        .await
        .unwrap();

    page.wait_for_selector_state(&eph_sel, WaitState::Detached)
        .await
        .unwrap();
    page.wait_for_selector_state(&eph_sel, WaitState::Hidden)
        .await
        .unwrap();
    assert_eq!(page.count(&eph_sel).await.unwrap(), 0);
    assert!(!page.is_visible(&eph_sel).await.unwrap());
    assert!(page.is_hidden(&eph_sel).await.unwrap());
    assert!(page.all_text_contents(&eph_sel).await.unwrap().is_empty());

    // Polling wait_for_selector_state(Detached) across a delayed removal must also succeed.
    page.evaluate("setTimeout(() => document.getElementById('delayed')?.remove(), 120)")
        .await
        .unwrap();
    page.wait_for_selector_state(&delayed_sel, WaitState::Detached)
        .await
        .unwrap();
    assert_eq!(page.count(&delayed_sel).await.unwrap(), 0);

    // Unknown ref must still fail fast as NotActionable.
    let err = page
        .count("@e9999")
        .await
        .expect_err("unknown ref must fail");
    assert!(
        err.to_string().contains("unknown element ref"),
        "unexpected error: {err}"
    );
    let err_wait = page
        .wait_for_selector_state("@e9999", WaitState::Detached)
        .await
        .expect_err("unknown ref must fail wait_for_selector_state");
    assert!(
        err_wait.to_string().contains("unknown element ref"),
        "unexpected error: {err_wait}"
    );

    drop(page);
    eng.shutdown(Duration::from_secs(5)).await.unwrap();
    srv.abort();
}

/// Fix #5: Popup page console, `pageerror`, and network events must NOT be misattributed or
/// double-recorded onto the opener page's `PageLog` or context trace.
#[tokio::test(flavor = "multi_thread")]
async fn popup_page_log_and_trace_isolation() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let app = Router::new()
        .route(
            "/opener",
            get(|| async {
                Html(
                    r#"<!doctype html>
                    <button id="open" onclick="window.open('/popup', '_blank')">Open</button>"#,
                )
            }),
        )
        .route(
            "/popup",
            get(|| async {
                Html(
                    r#"<!doctype html>
                    <script>console.log('popup-init-log');</script>
                    <div id="pop-ready">Popup Ready</div>"#,
                )
            }),
        )
        .route("/opener-api", get(|| async { Json(json!({ "from": "opener" })) }))
        .route("/popup-api", get(|| async { Json(json!({ "from": "popup" })) }));
    let srv = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let base = format!("http://127.0.0.1:{port}");

    let tmp = std::env::temp_dir().join(format!("fw-popup-trace-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let zip_path = tmp.join("popup-trace.zip");

    let eng = engine().await;
    let ctx = eng.acquire_context().await.unwrap();
    ctx.tracing()
        .start(TracingStartOptions {
            name: Some("popup-iso".into()),
            title: Some("Popup Isolation".into()),
            screenshots: false,
            snapshots: false,
            ..Default::default()
        })
        .await
        .unwrap();

    let opener = ctx.new_page().await.unwrap();
    let mut opener_net = opener.subscribe_network();
    let mut popups = opener.subscribe_popups();

    opener.goto(&format!("{base}/opener")).await.unwrap();
    opener
        .evaluate(
            r#"(() => {
                console.log('opener-only-log');
                return fetch('/opener-api').then(r => r.json());
            })()"#,
        )
        .await
        .unwrap();

    opener.locator("#open").click().await.unwrap();
    let popup = tokio::time::timeout(Duration::from_secs(10), popups.recv())
        .await
        .expect("popup within 10s")
        .expect("popup open");
    let mut popup_net = popup.subscribe_network();

    expect_locator(&popup.locator("#pop-ready"))
        .to_have_text("Popup Ready")
        .await
        .unwrap();

    popup
        .evaluate(
            r#"(() => {
                console.log('popup-action-log');
                setTimeout(() => { throw new Error('popup-only-err'); }, 0);
                return fetch('/popup-api').then(r => r.json());
            })()"#,
        )
        .await
        .unwrap();

    for _ in 0..50 {
        let has_console = popup
            .console_messages()
            .iter()
            .any(|m| m.text == "popup-action-log");
        let has_err = popup
            .page_errors()
            .iter()
            .any(|e| e.message.contains("popup-only-err"));
        if has_console && has_err {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_millis(120)).await;

    ctx.tracing().stop(Some(&zip_path)).await.unwrap();

    let opener_console: Vec<String> = opener
        .console_messages()
        .into_iter()
        .map(|m| m.text)
        .collect();
    let popup_console: Vec<String> = popup
        .console_messages()
        .into_iter()
        .map(|m| m.text)
        .collect();

    assert!(
        opener_console.iter().any(|t| t == "opener-only-log"),
        "opener missing own console log: {opener_console:?}"
    );
    assert!(
        !opener_console.iter().any(|t| t.contains("popup-")),
        "popup console leaked into opener: {opener_console:?}"
    );
    assert!(
        popup_console.iter().any(|t| t == "popup-init-log"),
        "popup missing early init console log: {popup_console:?}"
    );
    assert!(
        popup_console.iter().any(|t| t == "popup-action-log"),
        "popup missing action console log: {popup_console:?}"
    );
    assert!(
        !popup_console.iter().any(|t| t == "opener-only-log"),
        "opener console leaked into popup: {popup_console:?}"
    );

    assert!(
        opener.page_errors().is_empty(),
        "popup pageerror leaked into opener: {:?}",
        opener.page_errors()
    );
    assert!(
        popup
            .page_errors()
            .iter()
            .any(|e| e.message.contains("popup-only-err")),
        "popup missing own pageerror: {:?}",
        popup.page_errors()
    );

    let mut opener_urls = Vec::new();
    while let Ok(ev) = opener_net.try_recv() {
        if let NetEvent::Request(req) = ev {
            opener_urls.push(req.url);
        }
    }
    let mut popup_urls = Vec::new();
    while let Ok(ev) = popup_net.try_recv() {
        if let NetEvent::Request(req) = ev {
            popup_urls.push(req.url);
        }
    }
    assert!(
        opener_urls.iter().any(|u| u.ends_with("/opener-api")),
        "opener missing /opener-api: {opener_urls:?}"
    );
    assert!(
        !opener_urls.iter().any(|u| u.ends_with("/popup-api")),
        "popup network leaked into opener: {opener_urls:?}"
    );
    assert!(
        popup_urls.iter().any(|u| u.ends_with("/popup-api")),
        "popup missing /popup-api: {popup_urls:?}"
    );

    let files = parse_zip_store(&std::fs::read(&zip_path).expect("trace zip"));
    let trace_events = parse_ndjson(&files["trace.trace"]);
    let net_events = parse_ndjson(&files["trace.network"]);

    let popup_console_count = trace_events
        .iter()
        .filter(|e| {
            e["type"] == "console"
                && e["text"].as_str().is_some_and(|t| t == "popup-action-log")
        })
        .count();
    assert_eq!(
        popup_console_count, 1,
        "popup console event must be recorded once in trace, got {popup_console_count}"
    );

    let popup_err_count = trace_events
        .iter()
        .filter(|e| {
            e["type"] == "event"
                && e["method"] == "pageError"
                && e["params"]["error"]["error"]["message"]
                    .as_str()
                    .is_some_and(|m| m.contains("popup-only-err"))
        })
        .count();
    assert_eq!(
        popup_err_count, 1,
        "popup pageError must be recorded once in trace, got {popup_err_count}"
    );

    let popup_api_count = net_events
        .iter()
        .filter(|e| {
            e["type"] == "resource-snapshot"
                && e["snapshot"]["request"]["url"]
                    .as_str()
                    .is_some_and(|u| u.ends_with("/popup-api"))
        })
        .count();
    assert_eq!(
        popup_api_count, 1,
        "popup /popup-api network entry must be recorded once in trace, got {popup_api_count}"
    );

    drop(popup);
    drop(opener);
    ctx.close().await.unwrap();
    eng.shutdown(Duration::from_secs(5)).await.unwrap();
    let _ = std::fs::remove_dir_all(&tmp);
    srv.abort();
}
