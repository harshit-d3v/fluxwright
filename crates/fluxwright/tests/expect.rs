use std::time::{Duration, Instant};

use axum::{response::Html, routing::get, Router};
use fluxwright::{expect_locator, expect_page, BrowserEngine, JobOptions};

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

#[tokio::test(flavor = "multi_thread")]
async fn locator_expect_delayed_dom_and_not() {
    let eng = engine().await;
    let page = eng.acquire().await.unwrap();
    page.goto(concat!(
        "data:text/html,",
        "<div id='vis' style='display:none'>Hidden initially</div>",
        "<div id='hide'>Visible initially</div>",
        "<button id='btn' disabled>Wait</button>",
        "<input id='chk' type='checkbox'>",
        "<input id='edt' readonly value='initial'>",
        "<p id='txt'>  Loading   now  </p>",
        "<div id='cls' class='card pending'>Status</div>",
        "<ul id='list'><li>One</li></ul>",
        "<div id='transient-dup' class='dup'>A</div><div class='dup'>B</div>",
    ))
    .await
    .unwrap();

    // Schedule delayed async DOM mutations so polling is genuinely exercised.
    page.evaluate(
        r#"() => {
            setTimeout(() => {
                document.getElementById('vis').style.display = 'block';
                document.getElementById('hide').style.display = 'none';
                const btn = document.getElementById('btn');
                btn.disabled = false;
                btn.setAttribute('data-state', 'ready');
                document.getElementById('chk').checked = true;
                const edt = document.getElementById('edt');
                edt.readOnly = false;
                edt.value = 'updated';
                document.getElementById('txt').textContent = '  Ready\n  to   go  ';
                document.getElementById('cls').className = 'card active';
                const li = document.createElement('li');
                li.textContent = 'Two';
                document.getElementById('list').appendChild(li);
                const late = document.createElement('span');
                late.id = 'late';
                late.style.display = 'none';
                late.textContent = 'attached-only';
                document.body.appendChild(late);
                const dups = document.querySelectorAll('.dup');
                if (dups.length > 1) dups[1].remove();
            }, 150);
        }"#,
    )
    .await
    .unwrap();

    // Initially hidden / disabled / unchecked / readonly assertions pass immediately.
    expect_locator(&page.locator("#vis")).to_be_hidden().await.unwrap();
    expect_locator(&page.locator("#btn")).to_be_disabled().await.unwrap();
    expect_locator(&page.locator("#chk")).not().to_be_checked().await.unwrap();
    expect_locator(&page.locator("#edt")).not().to_be_editable().await.unwrap();
    expect_locator(&page.locator("#late")).not().to_be_attached().await.unwrap();

    // Wait for delayed DOM mutations via auto-retrying expect.
    expect_locator(&page.locator("#vis")).to_be_visible().await.unwrap();
    expect_locator(&page.locator("#hide")).to_be_hidden().await.unwrap();
    expect_locator(&page.locator("#hide")).not().to_be_visible().await.unwrap();
    expect_locator(&page.locator("#late")).to_be_attached().await.unwrap();
    expect_locator(&page.locator("#late")).to_be_hidden().await.unwrap();

    expect_locator(&page.locator("#btn")).to_be_enabled().await.unwrap();
    expect_locator(&page.locator("#btn")).not().to_be_disabled().await.unwrap();
    expect_locator(&page.locator("#btn"))
        .to_have_attribute("data-state", "ready")
        .await
        .unwrap();
    expect_locator(&page.locator("#btn"))
        .not()
        .to_have_attribute("data-state", "loading")
        .await
        .unwrap();
    expect_locator(&page.locator("#btn"))
        .not()
        .to_have_attribute("data-missing", "x")
        .await
        .unwrap();

    expect_locator(&page.locator("#chk")).to_be_checked().await.unwrap();
    expect_locator(&page.locator("#edt")).to_be_editable().await.unwrap();
    expect_locator(&page.locator("#edt")).to_have_value("updated").await.unwrap();
    expect_locator(&page.locator("#edt")).not().to_have_value("initial").await.unwrap();

    expect_locator(&page.locator("#txt")).to_have_text("Ready to go").await.unwrap();
    expect_locator(&page.locator("#txt")).to_contain_text("to go").await.unwrap();
    expect_locator(&page.locator("#txt")).not().to_have_text("Loading now").await.unwrap();
    expect_locator(&page.locator("#txt")).not().to_contain_text("Loading").await.unwrap();

    expect_locator(&page.locator("#cls")).to_have_class("card active").await.unwrap();
    expect_locator(&page.locator("#cls")).not().to_have_class("card pending").await.unwrap();

    expect_locator(&page.locator("#list li")).to_have_count(2).await.unwrap();
    expect_locator(&page.locator("#list li")).not().to_have_count(1).await.unwrap();
    expect_locator(&page.locator("#never")).to_have_count(0).await.unwrap();

    // Transient strict-mode violation resolved before timeout.
    expect_locator(&page.locator(".dup"))
        .poll_interval(Duration::from_millis(25))
        .to_have_text("A")
        .await
        .unwrap();

    // Double `.not()` cancels out.
    expect_locator(&page.locator("#vis")).not().not().to_be_visible().await.unwrap();

    // Toggle disabled back on after delay to verify positive `to_be_disabled` polling.
    page.evaluate("() => setTimeout(() => { document.getElementById('btn').disabled = true; }, 100)")
        .await
        .unwrap();
    expect_locator(&page.locator("#btn")).to_be_disabled().await.unwrap();

    drop(page);
    eng.shutdown(Duration::from_secs(5)).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn page_expect_title_url_and_diagnostics() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let app = Router::new()
        .route(
            "/start",
            get(|| async {
                Html(
                    "<title>Start Page</title><p id='msg'>Working</p>\
                     <script>setTimeout(() => { document.title = 'Done Page'; history.pushState({}, '', '/done'); }, 150);</script>",
                )
            }),
        )
        .route("/done", get(|| async { Html("<title>Done Page</title><p id='msg'>Done</p>") }));
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let eng = engine().await;
    let base = format!("http://127.0.0.1:{port}");
    let page = eng
        .acquire_job(&JobOptions::default().base_url(&base))
        .await
        .unwrap();
    page.set_default_timeout(Duration::from_secs(10));
    page.goto("/start").await.unwrap();

    expect_page(&page).not().to_have_url("/done").await.unwrap();
    expect_page(&page).to_have_title("Done Page").await.unwrap();
    expect_page(&page).to_have_url("/done").await.unwrap();
    expect_page(&page).to_have_url(format!("{base}/done")).await.unwrap();
    expect_page(&page).not().to_have_title("Start Page").await.unwrap();

    // Timeout diagnostics: clear expected vs actual, respects short timeout even when page default is 10s.
    let started = Instant::now();
    let err = expect_locator(&page.locator("#msg"))
        .timeout(Duration::from_millis(200))
        .poll_interval(Duration::from_millis(25))
        .to_have_text("Never")
        .await
        .unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(3), "took {:?}", started.elapsed());
    assert!(err.is_timeout(), "expected timeout error, got: {err}");
    let msg = err.to_string();
    assert!(msg.contains("#msg"), "{msg}");
    assert!(msg.contains("to_have_text"), "{msg}");
    assert!(msg.contains("expected \"Never\""), "{msg}");
    assert!(msg.contains("actual \"Working\""), "{msg}");

    // `.not()` on a missing element for text assertion must still fail (element not found).
    let missing_err = expect_locator(&page.locator("#missing"))
        .timeout(Duration::from_millis(150))
        .not()
        .to_have_text("Working")
        .await
        .unwrap_err();
    let missing_msg = missing_err.to_string();
    assert!(missing_msg.contains("element not found"), "{missing_msg}");

    // Page timeout diagnostics for URL / title.
    let url_err = expect_page(&page)
        .timeout(Duration::from_millis(150))
        .to_have_url("/never")
        .await
        .unwrap_err();
    assert!(url_err.is_timeout());
    let url_msg = url_err.to_string();
    assert!(url_msg.contains("to_have_url"), "{url_msg}");
    assert!(url_msg.contains("/never"), "{url_msg}");
    assert!(url_msg.contains("/done"), "{url_msg}");

    drop(page);
    eng.shutdown(Duration::from_secs(5)).await.unwrap();
}
