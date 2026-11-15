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
