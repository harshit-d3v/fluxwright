#[tokio::main]
async fn main() {
    let bind = std::env::var("BIND").unwrap_or_else(|_| "127.0.0.1:3456".into());
    let server = benchmark_server::spawn(&bind)
        .await
        .expect("bind localhost");
    eprintln!("benchmark-server {}", server.base_url);
    std::future::pending::<()>().await;
}
