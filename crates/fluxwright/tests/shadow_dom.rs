use std::time::Duration;

use axum::{response::Html, routing::get, Router};
use fluxwright::{expect_locator, BrowserEngine};

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
async fn shadow_dom_css_and_nested_cross_boundary() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let app = Router::new().route(
        "/",
        get(|| async {
            Html(
                r#"<!doctype html>
                <html><body>
                  <div id="out">idle</div>
                  <button id="light-btn">Light Button</button>
                  <div id="shadow-host"></div>
                  <div id="outer"></div>
                  <script>
                    const out = document.getElementById('out');

                    const host = document.getElementById('shadow-host');
                    const sr = host.attachShadow({ mode: 'open' });
                    const btn = document.createElement('button');
                    btn.id = 'shadow-btn';
                    btn.textContent = 'Shadow Button';
                    btn.onclick = () => { out.textContent = 'clicked-shadow'; };
                    sr.appendChild(btn);

                    const outer = document.getElementById('outer');
                    const outerSr = outer.attachShadow({ mode: 'open' });
                    const wrap = document.createElement('div');
                    wrap.className = 'inner-wrap';
                    const sib = document.createElement('span');
                    sib.className = 'before-inner';
                    sib.textContent = 'Before';
                    const inner = document.createElement('div');
                    inner.id = 'inner';
                    wrap.appendChild(sib);
                    wrap.appendChild(inner);
                    outerSr.appendChild(wrap);

                    const innerSr = inner.attachShadow({ mode: 'open' });
                    const deepBtn = document.createElement('button');
                    deepBtn.id = 'deep-btn';
                    deepBtn.textContent = 'Deep Button';
                    deepBtn.onclick = () => { out.textContent = 'clicked-deep'; };
                    innerSr.appendChild(deepBtn);
                  </script>
                </body></html>"#,
            )
        }),
    );
    let srv = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let eng = engine().await;
    let page = eng.acquire().await.unwrap();
    page.goto(&format!("http://127.0.0.1:{port}/"))
        .await
        .unwrap();

    // Bare CSS pierces open shadow roots.
    assert_eq!(
        page.locator("#shadow-btn")
            .text_content()
            .await
            .unwrap()
            .as_deref(),
        Some("Shadow Button")
    );
    page.locator("#shadow-btn").click().await.unwrap();
    assert_eq!(
        page.locator("#out")
            .text_content()
            .await
            .unwrap()
            .as_deref(),
        Some("clicked-shadow")
    );

    // Nested open shadow roots: #outer -> #inner -> #deep-btn.
    assert_eq!(
        page.locator("#deep-btn")
            .text_content()
            .await
            .unwrap()
            .as_deref(),
        Some("Deep Button")
    );
    page.locator("#deep-btn").click().await.unwrap();
    assert_eq!(
        page.locator("#out")
            .text_content()
            .await
            .unwrap()
            .as_deref(),
        Some("clicked-deep")
    );

    // Cross-boundary CSS combinators and chained locators.
    assert_eq!(
        page.locator("#shadow-host #shadow-btn")
            .text_content()
            .await
            .unwrap()
            .as_deref(),
        Some("Shadow Button")
    );
    assert_eq!(
        page.locator("#outer > .inner-wrap #deep-btn")
            .text_content()
            .await
            .unwrap()
            .as_deref(),
        Some("Deep Button")
    );
    assert_eq!(
        page.locator("#outer > .inner-wrap > .before-inner + #inner #deep-btn")
            .text_content()
            .await
            .unwrap()
            .as_deref(),
        Some("Deep Button")
    );
    assert_eq!(
        page.locator("#shadow-host")
            .locator("#shadow-btn")
            .text_content()
            .await
            .unwrap()
            .as_deref(),
        Some("Shadow Button")
    );
    assert_eq!(
        page.locator("#outer")
            .locator("#inner")
            .locator("#deep-btn")
            .text_content()
            .await
            .unwrap()
            .as_deref(),
        Some("Deep Button")
    );

    // css:light= stays restricted to light DOM only.
    assert_eq!(
        page.locator("css:light=#shadow-btn").count().await.unwrap(),
        0
    );
    assert_eq!(
        page.locator("css:light=#shadow-host #shadow-btn")
            .count()
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        page.locator("css:light=#light-btn").count().await.unwrap(),
        1
    );
    expect_locator(&page.locator("css:light=#shadow-btn"))
        .not()
        .to_be_attached()
        .await
        .unwrap();

    drop(page);
    eng.shutdown(Duration::from_secs(5)).await.unwrap();
    srv.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn shadow_dom_semantic_locators_and_expect() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let app = Router::new().route(
        "/",
        get(|| async {
            Html(
                r#"<!doctype html>
                <html><body>
                  <div id="status">none</div>
                  <div id="comp"></div>
                  <div id="hidden-host" aria-hidden="true"></div>
                  <my-slot-btn><span class="slotted-label">Slotted Action</span></my-slot-btn>
                  <script>
                    const status = document.getElementById('status');

                    const comp = document.getElementById('comp');
                    const sr = comp.attachShadow({ mode: 'open' });
                    sr.innerHTML = `
                      <style>button { padding: 4px 10px; }</style>
                      <button id="role-btn">Save Profile</button>
                      <p id="greet">Welcome inside shadow</p>
                      <label for="email-in">Email Address</label>
                      <input id="email-in" type="text" placeholder="you@example.com" data-testid="shadow-email">
                      <span id="ext-lbl">Secret Token</span>
                      <input id="token-in" type="text" aria-labelledby="ext-lbl">
                      <img id="pic" alt="Shadow Logo" title="Brand Emblem" width="24" height="24"
                           src="data:image/gif;base64,R0lGODlhAQABAIAAAAAAAP///yH5BAEAAAAALAAAAAABAAEAAAIBRAA7">
                      <div id="delayed-wrap"></div>
                    `;
                    sr.getElementById('role-btn').onclick = () => { status.textContent = 'saved'; };

                    // Host with aria-hidden="true" hides its shadow descendants from get_by_role.
                    const hiddenHost = document.getElementById('hidden-host');
                    const hiddenSr = hiddenHost.attachShadow({ mode: 'open' });
                    hiddenSr.innerHTML = `<button>Hidden Shadow Role</button>`;

                    // Slotted content inside a shadow button.
                    const slotHost = document.querySelector('my-slot-btn');
                    const slotSr = slotHost.attachShadow({ mode: 'open' });
                    slotSr.innerHTML = `<button id="slot-btn"><slot></slot></button>`;
                    slotSr.getElementById('slot-btn').onclick = () => { status.textContent = 'slotted-clicked'; };

                    setTimeout(() => {
                      const d = document.createElement('div');
                      d.id = 'delayed-shadow-msg';
                      d.textContent = 'Loaded in shadow';
                      sr.getElementById('delayed-wrap').appendChild(d);
                    }, 100);
                  </script>
                </body></html>"#,
            )
        }),
    );
    let srv = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let eng = engine().await;
    let page = eng.acquire().await.unwrap();
    page.goto(&format!("http://127.0.0.1:{port}/"))
        .await
        .unwrap();

    // get_by_role inside open shadow root + slotted content accessible name.
    page.get_by_role("button", Some("Save Profile"), true)
        .click()
        .await
        .unwrap();
    assert_eq!(
        page.locator("#status")
            .text_content()
            .await
            .unwrap()
            .as_deref(),
        Some("saved")
    );
    page.get_by_role("button", Some("Slotted Action"), true)
        .click()
        .await
        .unwrap();
    assert_eq!(
        page.locator("#status")
            .text_content()
            .await
            .unwrap()
            .as_deref(),
        Some("slotted-clicked")
    );

    // aria-hidden="true" on outer host hides inner shadow button from get_by_role.
    assert_eq!(
        page.get_by_role("button", Some("Hidden Shadow Role"), true)
            .count()
            .await
            .unwrap(),
        0
    );

    // get_by_text inside shadow root & across slot.
    expect_locator(&page.get_by_text("Welcome inside shadow", true))
        .to_be_visible()
        .await
        .unwrap();
    expect_locator(&page.get_by_text("Slotted Action", true))
        .to_be_visible()
        .await
        .unwrap();

    // get_by_label (<label for> and aria-labelledby inside shadow root).
    page.get_by_label("Email Address", true)
        .fill("ada@example.com")
        .await
        .unwrap();
    assert_eq!(
        page.get_by_test_id("shadow-email")
            .input_value()
            .await
            .unwrap(),
        "ada@example.com"
    );
    page.get_by_label("Secret Token", true)
        .fill("tok-42")
        .await
        .unwrap();
    assert_eq!(
        page.locator("#token-in").input_value().await.unwrap(),
        "tok-42"
    );

    // get_by_placeholder, get_by_alt_text, get_by_title inside shadow root.
    assert_eq!(
        page.get_by_placeholder("you@example.com", true)
            .input_value()
            .await
            .unwrap(),
        "ada@example.com"
    );
    expect_locator(&page.get_by_alt_text("Shadow Logo", true))
        .to_be_visible()
        .await
        .unwrap();
    expect_locator(&page.get_by_title("Brand Emblem", true))
        .to_be_visible()
        .await
        .unwrap();

    // expect_locator auto-retry for delayed shadow DOM elements.
    expect_locator(&page.locator("#delayed-shadow-msg"))
        .to_be_visible()
        .await
        .unwrap();
    expect_locator(&page.locator("#delayed-shadow-msg"))
        .to_have_text("Loaded in shadow")
        .await
        .unwrap();

    drop(page);
    eng.shutdown(Duration::from_secs(5)).await.unwrap();
    srv.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn shadow_dom_deterministic_composed_order_across_hosts() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let app = Router::new().route(
        "/",
        get(|| async {
            Html(
                r#"<!doctype html>
                <html><body>
                  <div class="item">01-light-before</div>
                  <div id="host-a">
                    <span class="item" slot="after">04-slotted-after</span>
                    <span class="item" slot="before">02-slotted-before</span>
                  </div>
                  <div class="item">05-light-middle</div>
                  <div id="host-b"></div>
                  <div class="item">08-light-end</div>
                  <script>
                    const srA = document.getElementById('host-a').attachShadow({ mode: 'open' });
                    srA.innerHTML = `
                      <slot name="before"></slot>
                      <span class="item">03-shadow-a</span>
                      <slot name="after"></slot>
                    `;

                    const srB = document.getElementById('host-b').attachShadow({ mode: 'open' });
                    srB.innerHTML = `
                      <span class="item">06-shadow-b</span>
                      <div id="nested-b"></div>
                    `;
                    const nestedSr = srB.getElementById('nested-b').attachShadow({ mode: 'open' });
                    nestedSr.innerHTML = `<span class="item">07-nested-b</span>`;
                  </script>
                </body></html>"#,
            )
        }),
    );
    let srv = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let eng = engine().await;
    let page = eng.acquire().await.unwrap();
    page.goto(&format!("http://127.0.0.1:{port}/"))
        .await
        .unwrap();

    let texts = page.locator(".item").all_text_contents().await.unwrap();
    assert_eq!(
        texts,
        vec![
            "01-light-before",
            "02-slotted-before",
            "03-shadow-a",
            "04-slotted-after",
            "05-light-middle",
            "06-shadow-b",
            "07-nested-b",
            "08-light-end",
        ]
    );
    assert_eq!(
        page.locator(".item")
            .first()
            .text_content()
            .await
            .unwrap()
            .as_deref(),
        Some("01-light-before")
    );
    assert_eq!(
        page.locator(".item")
            .nth(2)
            .text_content()
            .await
            .unwrap()
            .as_deref(),
        Some("03-shadow-a")
    );
    assert_eq!(
        page.locator(".item")
            .last()
            .text_content()
            .await
            .unwrap()
            .as_deref(),
        Some("08-light-end")
    );

    drop(page);
    eng.shutdown(Duration::from_secs(5)).await.unwrap();
    srv.abort();
}
