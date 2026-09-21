use std::net::SocketAddr;

use axum::response::{Html, IntoResponse};
use axum::routing::get;
use axum::Router;
use tokio::net::TcpListener;

pub struct Server {
    pub base_url: String,
    handle: tokio::task::JoinHandle<()>,
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
        .route("/set-cookie", get(set_cookie))
        .route("/show-cookie", get(show_cookie))
        .route("/slow", get(slow))
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

async fn show_cookie() -> Html<&'static str> {
    Html(
        r#"<!doctype html><title>show</title>
        <pre id="c"></pre>
        <script>document.getElementById('c').textContent = document.cookie;</script>"#,
    )
}

async fn slow() -> Html<&'static str> {
    tokio::time::sleep(std::time::Duration::from_secs(30)).await;
    Html("<!doctype html><title>slow</title>done")
}

#[allow(dead_code)]
pub fn addr_url(addr: SocketAddr) -> String {
    format!("http://{addr}")
}
