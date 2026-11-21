use std::net::SocketAddr;
use std::num::{NonZeroU64, NonZeroUsize};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use clap::{Parser, Subcommand};
use fluxwright::{BrowserEngine, QueueFullMode};
use fluxwright_server::{HttpServer, ServerConfig, WorkerHandle};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

#[derive(Parser)]
#[command(name = "fluxwright", about = "Chromium fleet engine")]
struct Cli {
    #[arg(long, global = true)]
    socket: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Serve authenticated browser jobs over HTTP (FLUXWRIGHT_SERVER_TOKEN is required).
    Serve {
        /// Listen address for local HTTP endpoints (defaults to 127.0.0.1:7331, or 127.0.0.1:0 with --join).
        #[arg(long)]
        listen: Option<SocketAddr>,
        /// Defaults to one browser per four available CPUs.
        #[arg(long)]
        max_browsers: Option<NonZeroUsize>,
        #[arg(long, default_value = "8")]
        max_contexts: NonZeroUsize,
        /// Active and retained completed jobs combined (1-4096).
        #[arg(long, default_value = "128")]
        max_jobs: NonZeroUsize,
        #[arg(long)]
        memory_ceiling_mb: Option<NonZeroU64>,
        /// Connect to an external browser over CDP (e.g. ws://... or http://...) instead of launching local Chrome.
        #[arg(long)]
        cdp_endpoint: Option<String>,
        /// Pages that may be on one host at once, across the fleet.
        #[arg(long)]
        max_per_host: Option<NonZeroUsize>,
        /// Milliseconds between the starts of two navigations to one host, at least.
        #[arg(long)]
        min_host_delay_ms: Option<NonZeroU64>,
        /// Keep accepted jobs in this file, and queue the unfinished ones again on start.
        #[arg(long)]
        journal: Option<std::path::PathBuf>,
        /// Pull and execute jobs from a coordinator URL (e.g. http://127.0.0.1:7331).
        #[arg(long)]
        join: Option<String>,
        /// Worker name registered with the coordinator when `--join` is set.
        #[arg(long)]
        worker_id: Option<String>,
        /// Pre-warmed blank context slots kept ready per running browser.
        #[arg(long, default_value_t = 0)]
        prewarm_contexts: usize,
        /// Browser channel (`chrome`, `chrome-beta`, `msedge`, `chromium`, ...).
        #[arg(long)]
        channel: Option<String>,
        /// Extra CLI flags passed to Chromium on launch (repeatable).
        #[arg(long = "chromium-arg")]
        chromium_args: Vec<String>,
    },
    /// Coordinate jobs over HTTP for remote `fluxwright serve --join` workers without running browsers locally.
    Coordinate {
        #[arg(long, default_value = "127.0.0.1:7331")]
        listen: SocketAddr,
        /// Active and retained completed jobs combined (1-4096).
        #[arg(long, default_value = "128")]
        max_jobs: NonZeroUsize,
        /// Keep accepted jobs in this file, and queue the unfinished ones again on start.
        #[arg(long)]
        journal: Option<std::path::PathBuf>,
    },
    Start {
        #[arg(long, default_value_t = 4)]
        max_browsers: usize,
        #[arg(long, default_value_t = 8)]
        max_contexts: usize,
    },
    Stats,
    Browsers,
    Doctor,
    Benchmark {
        #[arg(long, default_value = "benchmarks/results")]
        out: PathBuf,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "op")]
enum Request {
    Stats,
    Browsers,
    Doctor,
}

#[derive(Serialize, Deserialize)]
struct Reply {
    ok: bool,
    body: serde_json::Value,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("fluxwright=info".parse()?),
        )
        .init();
    let cli = Cli::parse();
    let sock = cli.socket.unwrap_or_else(default_socket);
    match cli.cmd {
        Cmd::Serve {
            listen,
            max_browsers,
            max_contexts,
            max_jobs,
            memory_ceiling_mb,
            cdp_endpoint,
            max_per_host,
            min_host_delay_ms,
            journal,
            join,
            worker_id,
            prewarm_contexts,
            channel,
            chromium_args,
        } => {
            let address = listen.unwrap_or_else(|| {
                if join.is_some() {
                    SocketAddr::from(([127, 0, 0, 1], 0))
                } else {
                    SocketAddr::from(([127, 0, 0, 1], 7331))
                }
            });
            http_daemon(
                address,
                max_browsers,
                max_contexts.get(),
                max_jobs.get(),
                memory_ceiling_mb,
                cdp_endpoint,
                max_per_host,
                min_host_delay_ms,
                journal,
                join,
                worker_id,
                prewarm_contexts,
                channel,
                chromium_args,
            )
            .await
        }
        Cmd::Coordinate {
            listen,
            max_jobs,
            journal,
        } => coordinator_daemon(listen, max_jobs.get(), journal).await,
        Cmd::Start {
            max_browsers,
            max_contexts,
        } => daemon(sock, max_browsers, max_contexts).await,
        Cmd::Stats => rpc(&sock, Request::Stats).await,
        Cmd::Browsers => rpc(&sock, Request::Browsers).await,
        Cmd::Doctor => doctor().await,
        Cmd::Benchmark { out } => {
            let status = std::process::Command::new("cargo")
                .args([
                    "run",
                    "-p",
                    "fluxwright-benchmarks",
                    "--",
                    "--out",
                    &out.to_string_lossy(),
                ])
                .status()?;
            anyhow::ensure!(status.success(), "benchmark failed");
            Ok(())
        }
    }
}

fn server_config(max_jobs: usize, journal: Option<std::path::PathBuf>) -> Result<(String, ServerConfig)> {
    anyhow::ensure!((1..=4096).contains(&max_jobs), "--max-jobs must be between 1 and 4096");
    let token = std::env::var("FLUXWRIGHT_SERVER_TOKEN").map_err(|_| {
        anyhow::anyhow!(
            "set FLUXWRIGHT_SERVER_TOKEN to a random token (32-256 printable ASCII characters)"
        )
    })?;
    let mut config = ServerConfig::new(token.clone())?;
    config.max_jobs = max_jobs;
    config.journal = journal;
    Ok((token, config))
}

async fn coordinator_daemon(
    address: SocketAddr,
    max_jobs: usize,
    journal: Option<std::path::PathBuf>,
) -> Result<()> {
    let (_token, config) = server_config(max_jobs, journal)?;
    let listener = tokio::net::TcpListener::bind(address).await?;
    let server = HttpServer::coordinator(config)?;
    eprintln!(
        "fluxwright coordinator listening on http://{} (bearer authentication required)",
        listener.local_addr()?
    );
    serve_http(listener, address, server, None).await
}

#[allow(clippy::too_many_arguments)]
async fn http_daemon(
    address: SocketAddr,
    max_browsers: Option<NonZeroUsize>,
    max_contexts: usize,
    max_jobs: usize,
    memory_ceiling_mb: Option<NonZeroU64>,
    cdp_endpoint: Option<String>,
    max_per_host: Option<NonZeroUsize>,
    min_host_delay_ms: Option<NonZeroU64>,
    journal: Option<std::path::PathBuf>,
    join: Option<String>,
    worker_id: Option<String>,
    prewarm_contexts: usize,
    channel: Option<String>,
    chromium_args: Vec<String>,
) -> Result<()> {
    let (token, config) = server_config(max_jobs, journal)?;
    // Bind before creating the engine so an occupied address never starts browser work.
    let listener = tokio::net::TcpListener::bind(address).await?;
    let mut builder = BrowserEngine::builder();
    // First, so an explicit --max-browsers still overrides its one connection.
    if let Some(endpoint) = cdp_endpoint {
        builder = builder.connect_over_cdp(endpoint);
    }
    builder = builder
        .max_contexts_per_browser(max_contexts)
        .prewarm_contexts(prewarm_contexts)
        .queue_capacity(max_jobs)
        .queue_full_mode(QueueFullMode::Wait)
        .acquire_timeout(Duration::from_secs(300));
    if let Some(ch) = channel {
        builder = builder.channel(ch);
    }
    if !chromium_args.is_empty() {
        builder = builder.args(chromium_args);
    }
    if let Some(max) = max_browsers {
        builder = builder.max_browsers(max.get());
    }
    if let Some(ceiling) = memory_ceiling_mb {
        builder = builder.memory_ceiling_mb(ceiling.get());
    }
    if let Some(n) = max_per_host {
        builder = builder.max_concurrency_per_host(n.get());
    }
    if let Some(ms) = min_host_delay_ms {
        builder = builder.min_delay_per_host(Duration::from_millis(ms.get()));
    }
    let engine = builder.build().await?;
    let worker = match join {
        Some(url) => {
            let slots = engine
                .config()
                .max_browsers
                .saturating_mul(engine.config().max_contexts_per_browser)
                .max(1);
            let handle = fluxwright_server::spawn_worker(
                engine.clone(),
                url.clone(),
                token,
                worker_id,
                slots,
            )?;
            eprintln!("fluxwright worker joined coordinator {url} ({slots} slots, experimental)");
            if !handle.coordinator_is_loopback() {
                eprintln!(
                    "warning: --join uses unencrypted HTTP; keep the coordinator link on a trusted private network or local TLS tunnel"
                );
            }
            Some(handle)
        }
        None => None,
    };
    let server = HttpServer::new(engine, config)?;
    eprintln!(
        "fluxwright HTTP listening on http://{} (bearer authentication required)",
        listener.local_addr()?
    );
    serve_http(listener, address, server, worker).await
}

async fn serve_http(
    listener: tokio::net::TcpListener,
    address: SocketAddr,
    server: HttpServer,
    worker: Option<WorkerHandle>,
) -> Result<()> {
    if !address.ip().is_loopback() {
        eprintln!("warning: remote access enabled; use a private network and a TLS reverse proxy; this is a trusted-operator API");
    }
    let shutdown = server.clone();
    let worker_on_shutdown = worker.clone();
    let result = axum::serve(listener, server.router())
        .with_graceful_shutdown(async move {
            if let Err(error) = shutdown_signal().await {
                tracing::error!(%error, "could not wait for the shutdown signal");
            }
            if let Some(w) = worker_on_shutdown {
                w.shutdown(Duration::from_secs(10)).await;
            }
            let _ = shutdown.shutdown(Duration::from_secs(10)).await;
        })
        .await;
    if let Some(w) = worker {
        w.abort();
    }
    // Also clean up if the listener fails instead of receiving a signal.
    server.shutdown(Duration::from_secs(10)).await?;
    result?;
    Ok(())
}

async fn shutdown_signal() -> std::io::Result<()> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result?,
            _ = terminate.recv() => {},
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await
    }
}

fn default_socket() -> PathBuf {
    #[cfg(windows)]
    {
        PathBuf::from(r"\\.\pipe\fluxwright")
    }
    #[cfg(unix)]
    {
        std::env::temp_dir().join("fluxwright.sock")
    }
}

async fn daemon(sock: PathBuf, max_browsers: usize, max_contexts: usize) -> Result<()> {
    let engine = BrowserEngine::builder()
        .max_browsers(max_browsers)
        .max_contexts_per_browser(max_contexts)
        .build()
        .await?;
    eprintln!("fluxwright listening on {}", sock.display());
    let res = serve(sock, engine.clone()).await;
    let _ = engine.shutdown(Duration::from_secs(5)).await;
    res
}

const MAX_DAEMON_LINE_BYTES: usize = 64 * 1024;

#[cfg(windows)]
async fn serve(sock: PathBuf, engine: BrowserEngine) -> Result<()> {
    use tokio::net::windows::named_pipe::ServerOptions;
    let mut first = true;
    loop {
        let mut opts = ServerOptions::new();
        if first {
            opts.first_pipe_instance(true);
            first = false;
        }
        let server = opts.create(&sock)?;
        tokio::select! {
            accepted = server.connect() => accepted?,
            sig = shutdown_signal() => return sig.map_err(Into::into),
        }
        let engine = engine.clone();
        tokio::spawn(async move {
            let _ = daemon_session(server, engine).await;
        });
    }
}

#[cfg(unix)]
async fn serve(sock: PathBuf, engine: BrowserEngine) -> Result<()> {
    use tokio::net::{UnixListener, UnixStream};
    // Refuse to unlink another live daemon's socket; only remove stale socket files.
    if UnixStream::connect(&sock).await.is_ok() {
        anyhow::bail!("daemon already listening on {}", sock.display());
    }
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock)?;
    let res = loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    let engine = engine.clone();
                    tokio::spawn(async move {
                        let _ = daemon_session(stream, engine).await;
                    });
                }
                Err(e) => break Err(e.into()),
            },
            sig = shutdown_signal() => break sig.map_err(Into::into),
        }
    };
    let _ = std::fs::remove_file(&sock);
    res
}

async fn daemon_session<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(
    stream: S,
    engine: BrowserEngine,
) -> Result<()> {
    let (reader, mut writer) = tokio::io::split(stream);
    let mut reader = BufReader::new(reader);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        let n = (&mut reader)
            .take((MAX_DAEMON_LINE_BYTES + 1) as u64)
            .read_until(b'\n', &mut buf)
            .await?;
        if n == 0 {
            break;
        }
        // A full line is up to MAX bytes plus its '\n'; MAX + 1 bytes without one is oversized.
        if !buf.ends_with(b"\n") {
            if buf.len() > MAX_DAEMON_LINE_BYTES {
                let reply = Reply {
                    ok: false,
                    body: serde_json::json!({ "error": "request line exceeds 64 KiB limit" }),
                };
                let _ = writer
                    .write_all(serde_json::to_string(&reply)?.as_bytes())
                    .await;
                let _ = writer.write_all(b"\n").await;
            }
            break;
        }
        let line = String::from_utf8_lossy(&buf);
        let reply = dispatch(&engine, line.trim()).await;
        writer
            .write_all(serde_json::to_string(&reply)?.as_bytes())
            .await?;
        writer.write_all(b"\n").await?;
    }
    Ok(())
}

async fn dispatch(engine: &BrowserEngine, line: &str) -> Reply {
    match serde_json::from_str::<Request>(line) {
        Ok(Request::Stats) => Reply {
            ok: true,
            body: serde_json::to_value(engine.metrics().await).unwrap_or_default(),
        },
        Ok(Request::Browsers) => Reply {
            ok: true,
            body: serde_json::to_value(engine.browsers().await).unwrap_or_default(),
        },
        Ok(Request::Doctor) => Reply {
            ok: true,
            body: serde_json::json!({ "daemon_pid": std::process::id() }),
        },
        Err(e) => Reply {
            ok: false,
            body: serde_json::json!({ "error": e.to_string() }),
        },
    }
}

async fn rpc_roundtrip<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(
    mut stream: S,
    payload: &[u8],
) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(10), async {
        stream.write_all(payload).await?;
        stream.write_all(b"\n").await?;
        let mut reader = BufReader::new(stream);
        let mut buf = Vec::new();
        (&mut reader)
            .take(1024 * 1024)
            .read_until(b'\n', &mut buf)
            .await?;
        print!("{}", String::from_utf8_lossy(&buf));
        Ok(())
    })
    .await
    .map_err(|_| anyhow::anyhow!("daemon request timed out"))?
}

async fn rpc(sock: &PathBuf, req: Request) -> Result<()> {
    let payload = serde_json::to_vec(&req)?;
    #[cfg(windows)]
    {
        use tokio::net::windows::named_pipe::ClientOptions;
        rpc_roundtrip(ClientOptions::new().open(sock)?, &payload).await
    }
    #[cfg(unix)]
    {
        use tokio::net::UnixStream;
        rpc_roundtrip(UnixStream::connect(sock).await?, &payload).await
    }
}

async fn doctor() -> Result<()> {
    let chrome = fluxwright::find_browser(true);
    let report = serde_json::json!({
        "chrome": chrome.as_ref().ok().map(|p| p.display().to_string()),
        "chrome_ok": chrome.is_ok(),
        "no_sandbox_env": std::env::var("FLUXWRIGHT_NO_SANDBOX").ok(),
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
    });
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}


#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn workspace_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(|p| p.parent())
            .expect("workspace root")
            .to_path_buf()
    }

    #[test]
    fn cli_parses_default_container_serve_command() {
        let cli = Cli::try_parse_from(["fluxwright", "serve", "--listen", "0.0.0.0:7331"])
            .expect("container CMD should parse cleanly");
        match cli.cmd {
            Cmd::Serve {
                listen,
                max_browsers,
                max_contexts,
                max_jobs,
                memory_ceiling_mb,
                cdp_endpoint,
                max_per_host,
                min_host_delay_ms,
                journal,
                join,
                worker_id,
                prewarm_contexts,
                channel,
                chromium_args,
            } => {
                assert!(
                    max_per_host.is_none()
                        && min_host_delay_ms.is_none()
                        && journal.is_none()
                        && join.is_none()
                        && worker_id.is_none()
                        && channel.is_none()
                        && chromium_args.is_empty()
                );
                assert_eq!(prewarm_contexts, 0);
                assert_eq!(listen, Some("0.0.0.0:7331".parse::<SocketAddr>().unwrap()));
                assert!(max_browsers.is_none());
                assert_eq!(max_contexts.get(), 8);
                assert_eq!(max_jobs.get(), 128);
                assert!(memory_ceiling_mb.is_none());
                assert!(cdp_endpoint.is_none());
            }
            _ => panic!("expected Cmd::Serve"),
        }
        let custom = Cli::try_parse_from([
            "fluxwright",
            "serve",
            "--prewarm-contexts",
            "2",
            "--channel",
            "chrome",
            "--chromium-arg",
            "--disable-gpu",
        ])
        .expect("serve launch/prewarm flags should parse");
        match custom.cmd {
            Cmd::Serve {
                prewarm_contexts,
                channel,
                chromium_args,
                ..
            } => {
                assert_eq!(prewarm_contexts, 2);
                assert_eq!(channel.as_deref(), Some("chrome"));
                assert_eq!(chromium_args, vec!["--disable-gpu"]);
            }
            _ => panic!("expected Cmd::Serve"),
        }
        assert!(Cli::try_parse_from(["fluxwright", "coordinate", "--max-jobs", "256"]).is_ok());
        assert!(Cli::try_parse_from([
            "fluxwright",
            "serve",
            "--join",
            "http://127.0.0.1:7331",
            "--worker-id",
            "w1",
        ])
        .is_ok());
    }

    #[test]
    fn dockerfile_and_dockerignore_match_workspace_contract() {
        let root = workspace_root();
        let dockerfile =
            std::fs::read_to_string(root.join("Dockerfile")).expect("Dockerfile at repo root");
        let dockerignore =
            std::fs::read_to_string(root.join(".dockerignore")).expect(".dockerignore at repo root");

        // Multi-stage build and required packages/runtime settings
        assert!(dockerfile.contains("FROM rust:1-bookworm AS builder"));
        assert!(dockerfile.contains("FROM debian:bookworm-slim AS runtime"));
        assert!(dockerfile.contains("-p fluxwright-cli -p fluxwright-mcp"));
        assert!(dockerfile.contains("chromium"));
        assert!(dockerfile.contains("tini"));
        assert!(dockerfile.contains("ca-certificates"));
        assert!(dockerfile.contains("fonts-liberation"));
        assert!(dockerfile.contains("fonts-noto-color-emoji"));
        assert!(dockerfile.contains("FLUXWRIGHT_CHROMIUM=/usr/bin/chromium"));
        assert!(dockerfile.contains("USER fluxwright"));
        assert!(dockerfile.contains("EXPOSE 7331"));
        assert!(dockerfile.contains("http://127.0.0.1:7331/healthz"));
        assert!(dockerfile.contains("ENTRYPOINT [\"/usr/bin/tini\", \"--\", \"fluxwright\"]"));
        assert!(dockerfile.contains("CMD [\"serve\", \"--listen\", \"0.0.0.0:7331\"]"));
        // Sandbox must stay enabled by default in the image
        assert!(!dockerfile.contains("ENV FLUXWRIGHT_NO_SANDBOX=1"));

        // .dockerignore must exclude heavy directories while keeping workspace members intact
        let ignored_lines: Vec<&str> = dockerignore
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .collect();
        assert!(ignored_lines.iter().any(|l| *l == "/target" || *l == "target/"));
        assert!(ignored_lines.iter().any(|l| l.contains("node_modules")));
        assert!(ignored_lines.iter().any(|l| *l == ".git" || *l == ".git/"));
        assert!(ignored_lines.iter().any(|l| *l == "video/" || *l == "video"));
        assert!(ignored_lines.iter().any(|l| *l == ".claude" || *l == ".claude/"));
        assert!(ignored_lines.iter().any(|l| *l == "bindings/" || *l == "bindings"));

        // Workspace members in root Cargo.toml must not be blanket-excluded
        for member in ["crates", "benchmark-server", "benchmarks"] {
            assert!(
                !ignored_lines
                    .iter()
                    .any(|l| *l == member || *l == format!("{member}/") || *l == format!("/{member}")),
                "workspace member {member} must not be excluded by .dockerignore"
            );
        }
    }

    #[tokio::test]
    async fn daemon_session_dispatches_valid_command_line() {
        let engine = BrowserEngine::builder().build().await.expect("engine");
        let (mut client, server) = tokio::io::duplex(4096);
        let session = tokio::spawn(async move { daemon_session(server, engine).await });

        client
            .write_all(b"{\"op\":\"Doctor\"}\n")
            .await
            .expect("write request");

        let mut reader = BufReader::new(client);
        let mut line = String::new();
        reader.read_line(&mut line).await.expect("read reply");
        let reply: Reply = serde_json::from_str(line.trim()).expect("valid json reply");
        assert!(reply.ok);
        assert_eq!(reply.body["daemon_pid"], serde_json::json!(std::process::id()));

        drop(reader);
        session.await.expect("join").expect("daemon_session ok");
    }

    #[tokio::test]
    async fn daemon_session_accepts_a_line_of_exactly_the_limit() {
        let engine = BrowserEngine::builder().build().await.expect("engine");
        let (mut client, server) = tokio::io::duplex(128 * 1024);
        let session = tokio::spawn(async move { daemon_session(server, engine).await });

        let mut request = b"{\"op\":\"Doctor\"}".to_vec();
        request.resize(MAX_DAEMON_LINE_BYTES, b' ');
        request.push(b'\n');
        client.write_all(&request).await.expect("write request");

        let mut reader = BufReader::new(client);
        let mut line = String::new();
        reader.read_line(&mut line).await.expect("read reply");
        let reply: Reply = serde_json::from_str(line.trim()).expect("valid json reply");
        assert!(reply.ok, "a {MAX_DAEMON_LINE_BYTES}-byte line was rejected: {line}");

        drop(reader);
        session.await.expect("join").expect("daemon_session ok");
    }

    #[tokio::test]
    async fn daemon_session_rejects_oversized_line_without_unbounded_buffering() {
        let engine = BrowserEngine::builder().build().await.expect("engine");
        let (mut client, server) = tokio::io::duplex(128 * 1024);
        let session = tokio::spawn(async move { daemon_session(server, engine).await });

        let oversized = vec![b'a'; MAX_DAEMON_LINE_BYTES + 1024];
        client.write_all(&oversized).await.expect("write oversized");

        let mut reader = BufReader::new(client);
        let mut line = String::new();
        reader.read_line(&mut line).await.expect("read error reply");
        let reply: Reply = serde_json::from_str(line.trim()).expect("valid json error reply");
        assert!(!reply.ok);
        assert!(reply.body["error"]
            .as_str()
            .unwrap_or_default()
            .contains("64 KiB"));

        session.await.expect("join").expect("daemon_session terminates cleanly");
    }

    #[tokio::test]
    async fn rpc_roundtrip_writes_and_reads_reply() {
        let engine = BrowserEngine::builder().build().await.expect("engine");
        let (client, server) = tokio::io::duplex(4096);
        let session = tokio::spawn(async move { daemon_session(server, engine).await });
        rpc_roundtrip(client, b"{\"op\":\"Doctor\"}").await.expect("roundtrip ok");
        session.await.expect("join").expect("daemon_session ok");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unix_serve_refuses_active_socket_without_deleting_it() {
        let sock =
            std::env::temp_dir().join(format!("fluxwright-cli-test-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        let listener = tokio::net::UnixListener::bind(&sock).expect("bind test listener");
        let accept = tokio::spawn(async move {
            let _ = listener.accept().await;
        });
        let engine = BrowserEngine::builder().min_browsers(0).build().await.expect("engine");
        let err = serve(sock.clone(), engine.clone()).await.unwrap_err();
        assert!(err.to_string().contains("already listening"), "{err}");
        assert!(sock.exists(), "active daemon socket was removed");
        accept.await.unwrap();
        let _ = std::fs::remove_file(&sock);
        engine.shutdown(Duration::from_secs(1)).await.unwrap();
    }
}
