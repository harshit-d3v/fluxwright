use axum::response::{Html, IntoResponse};
use axum::routing::get;
use axum::Router;
use tokio::net::TcpListener;

pub struct Server {
    pub base_url: String,
    handle: tokio::task::JoinHandle<()>,
}

/// A Chrome anyone can connect to over CDP, as a remote browser is: started with a debugging
/// port, whose address it writes to `DevToolsActivePort`. Fluxwright's own browsers talk over a
/// private pipe on Linux and macOS, so they have no address to borrow. Killed when dropped.
pub struct DebuggableChrome {
    child: std::process::Child,
    profile: std::path::PathBuf,
    pub ws_url: String,
}

impl DebuggableChrome {
    pub async fn start(chrome: &std::path::Path) -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        // Named like Fluxwright's profiles, so a crashed run's folder is swept later.
        let profile = std::env::temp_dir().join(format!("fluxwright-{}-cdp-test-{nanos}", std::process::id()));
        let mut cmd = std::process::Command::new(chrome);
        cmd.args(["--headless=new", "--remote-debugging-port=0", "--no-first-run", "--no-default-browser-check"])
            .arg(format!("--user-data-dir={}", profile.display()))
            .arg("about:blank")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        if std::env::var_os("FLUXWRIGHT_NO_SANDBOX").is_some() {
            cmd.arg("--no-sandbox");
        }
        let child = cmd.spawn().expect("start chrome");
        for _ in 0..200 {
            if let Ok(text) = std::fs::read_to_string(profile.join("DevToolsActivePort")) {
                let mut lines = text.lines();
                if let (Some(port), Some(path)) = (lines.next(), lines.next()) {
                    let ws_url = format!("ws://127.0.0.1:{}{}", port.trim(), path.trim());
                    return Self { child, profile, ws_url };
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        panic!("chrome wrote no DevToolsActivePort within 20 s");
    }
}

impl Drop for DebuggableChrome {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.profile);
    }
}

impl Server {
    pub fn abort(self) {
        self.handle.abort();
    }
}

pub async fn spawn(bind: &str) -> std::io::Result<Server> {
    let app = router();
    let listener = TcpListener::bind(bind).await?;
    let addr = listener.local_addr()?;
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok(Server {
        base_url: format!("http://{addr}"),
        handle,
    })
}

pub fn router() -> Router {
    Router::new()
        .route("/", get(home))
        .route("/heavy", get(heavy))
        .route("/pixel.png", get(pixel_png))
        .route("/scraping", get(scraping))
        .route("/assets/hero.png", get(asset_hero))
        .route("/assets/banner.jpg", get(asset_banner))
        .route("/assets/font.woff2", get(asset_font))
        .route("/assets/style.css", get(asset_style))
        .route("/assets/app.js", get(asset_script))
        .route("/dynamic", get(dynamic))
        .route("/forms", get(forms))
        .route("/javascript", get(javascript))
        .route("/many-elements", get(many_elements))
        .route("/headers", get(headers))
        .route("/set-cookie", get(set_cookie))
        .route("/show-cookie", get(show_cookie))
        .route("/slow", get(slow))
        .route("/slow-subresource", get(slow_subresource))
}

async fn home() -> Html<&'static str> {
    Html("<!doctype html><title>home</title><h1>fluxwright</h1><p>ok</p>")
}

async fn heavy() -> Html<&'static str> {
    Html(
        r#"<!doctype html><title>heavy</title>
        <img src="/pixel.png" width="10" height="10">
        <p>lots of filler text for a heavier document. abcdefghijklmnopqrstuvwxyz</p>
        <div class="card">content</div>"#,
    )
}

async fn dynamic() -> Html<&'static str> {
    Html(
        r#"<!doctype html><title>dynamic</title>
        <div id="out">loading</div>
        <script>
          setTimeout(() => { document.getElementById('out').textContent = 'ready'; }, 50);
        </script>"#,
    )
}

async fn forms() -> Html<&'static str> {
    Html(
        r#"<!doctype html><title>forms</title>
        <form>
          <input id="name" name="name">
          <button id="go" type="button" onclick="document.getElementById('name').value += '-ok'">go</button>
        </form>"#,
    )
}

async fn javascript() -> Html<&'static str> {
    Html(
        r#"<!doctype html><title>javascript</title>
        <script>window.FW = { n: 7 };</script>
        <p id="x">js</p>"#,
    )
}

async fn many_elements() -> Html<String> {
    let mut s = String::from("<!doctype html><title>many</title>");
    for i in 0..200 {
        s.push_str(&format!("<div class='item' data-i='{i}'>item {i}</div>"));
    }
    Html(s)
}

async fn set_cookie() -> impl IntoResponse {
    (
        [(axum::http::header::SET_COOKIE, "fw=secret; Path=/")],
        Html("<!doctype html><title>set</title><p>cookie set</p>"),
    )
}

/// The request's user agent, languages and `x-test` header, as the page text.
async fn headers(h: axum::http::HeaderMap) -> String {
    let get = |name| h.get(name).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    format!("{}\n{}\n{}", get("user-agent"), get("accept-language"), get("x-test"))
}

async fn show_cookie() -> Html<&'static str> {
    Html(
        r#"<!doctype html><title>show</title>
        <pre id="c"></pre>
        <script>document.getElementById('c').textContent = document.cookie;</script>"#,
    )
}

/// DOMContentLoaded at once, `load` only after /slow answers (30 s).
async fn slow_subresource() -> Html<&'static str> {
    Html("<!doctype html><title>dcl</title><img src=\"/slow\">")
}

async fn slow() -> Html<&'static str> {
    tokio::time::sleep(std::time::Duration::from_secs(30)).await;
    Html("<!doctype html><title>slow</title>done")
}

async fn pixel_png() -> impl IntoResponse {
    static PIXEL: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F,
        0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x78, 0xDA, 0x63, 0x64,
        0xF8, 0xCF, 0xC0, 0x50, 0x0F, 0x00, 0x04, 0x85, 0x01, 0x80, 0x84, 0xA9, 0x8C, 0x21, 0x00,
        0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];
    (
        [
            (axum::http::header::CONTENT_TYPE, "image/png"),
            (axum::http::header::CACHE_CONTROL, "no-store"),
        ],
        PIXEL,
    )
}

async fn scraping() -> Html<&'static str> {
    Html(
        r#"<!doctype html><title>scraping</title>
        <link rel="stylesheet" href="/assets/style.css">
        <script src="/assets/app.js"></script>
        <article>
          <h1>Product Catalog Item</h1>
          <p>Benchmark scraping payload with images, webfont, stylesheet, and script assets.</p>
          <img src="/assets/hero.png" alt="hero">
          <img src="/assets/banner.jpg" alt="banner">
        </article>"#,
    )
}

async fn asset_hero() -> impl IntoResponse {
    static HERO_PNG: [u8; 131_072] = [0x89; 131_072];
    (
        [
            (axum::http::header::CONTENT_TYPE, "image/png"),
            (axum::http::header::CACHE_CONTROL, "no-store"),
        ],
        HERO_PNG.as_slice(),
    )
}

async fn asset_banner() -> impl IntoResponse {
    static BANNER_JPG: [u8; 131_072] = [0xFF; 131_072];
    (
        [
            (axum::http::header::CONTENT_TYPE, "image/jpeg"),
            (axum::http::header::CACHE_CONTROL, "no-store"),
        ],
        BANNER_JPG.as_slice(),
    )
}

async fn asset_font() -> impl IntoResponse {
    static FONT_WOFF2: [u8; 65_536] = [0x77; 65_536];
    (
        [
            (axum::http::header::CONTENT_TYPE, "font/woff2"),
            (axum::http::header::CACHE_CONTROL, "no-store"),
        ],
        FONT_WOFF2.as_slice(),
    )
}

async fn asset_style() -> impl IntoResponse {
    static CSS: &str = concat!(
        "@font-face { font-family: 'BenchFont'; src: url('/assets/font.woff2') format('woff2'); }\n",
        "body { font-family: 'BenchFont', sans-serif; margin: 0; padding: 16px; }\n",
        "article { max-width: 720px; border: 1px solid #ddd; padding: 16px; }\n",
    );
    (
        [
            (axum::http::header::CONTENT_TYPE, "text/css"),
            (axum::http::header::CACHE_CONTROL, "no-store"),
        ],
        CSS,
    )
}

async fn asset_script() -> impl IntoResponse {
    static JS: &str = "window.__BENCH_READY = true; document.documentElement.dataset.ready = '1';";
    (
        [
            (axum::http::header::CONTENT_TYPE, "application/javascript"),
            (axum::http::header::CACHE_CONTROL, "no-store"),
        ],
        JS,
    )
}
