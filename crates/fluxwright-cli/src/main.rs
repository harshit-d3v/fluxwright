use std::net::SocketAddr;
use std::num::{NonZeroU64, NonZeroUsize};
use std::path::PathBuf;
use std::time::Duration;

mod install;

use anyhow::Result;
use clap::{Parser, Subcommand};
use fluxwright::{AdaptiveConfig, BrowserEngine, ProxyPool, QueueFullMode};
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
        #[arg(long, conflicts_with = "store")]
        journal: Option<std::path::PathBuf>,
        /// Persist accepted jobs and completed results in a SQLite database file across restarts.
        #[arg(long, conflicts_with = "journal")]
        store: Option<std::path::PathBuf>,
        /// How long finished job results remain valid in the SQLite store (seconds).
        #[arg(long, default_value_t = 3600)]
        result_ttl_secs: u64,
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
        #[arg(long = "chromium-arg", allow_hyphen_values = true)]
        chromium_args: Vec<String>,
        /// Accept `?token=<token>` on `/cdp` and `/json/version` for CDP clients that cannot send Bearer headers.
        #[arg(long, default_value_t = false)]
        allow_query_token: bool,
        /// Allow callers to supply `cdp_endpoint` (BYOB external browser) on `POST /v1/sessions`.
        #[arg(long, default_value_t = false)]
        allow_session_cdp_endpoint: bool,
        /// Directory storing named persistent profiles (`storageState`) for `profile` jobs.
        #[arg(long)]
        profiles_dir: Option<PathBuf>,
        /// Outbound proxy URL (`scheme://[user[:pass]@]host[:port]`, repeatable for pooled rotation).
        #[arg(long = "proxy")]
        proxies: Vec<String>,
        /// Enable AIMD adaptive concurrency limiting driven by browser RSS pressure and failures.
        #[arg(long, default_value_t = false)]
        adaptive_concurrency: bool,
        /// HMAC-SHA256 signing secret for job completion webhooks (falls back to FLUXWRIGHT_WEBHOOK_SECRET).
        #[arg(long)]
        webhook_secret: Option<String>,
        /// OTLP/HTTP JSON trace collector endpoint base URL (falls back to OTEL_EXPORTER_OTLP_ENDPOINT).
        #[arg(long)]
        otlp_endpoint: Option<String>,
        /// OpenTelemetry service.name attribute (falls back to OTEL_SERVICE_NAME, default "fluxwright-server").
        #[arg(long)]
        otlp_service_name: Option<String>,
        /// CSP frame-ancestors policy for GET /embed/live/{token} (default "*").
        #[arg(long, default_value = "*")]
        embed_frame_ancestors: String,
        /// Enable low-memory headless Chromium flags (`--disable-gpu`, `--disable-software-rasterizer`, `--js-flags=--max-old-space-size=256`).
        #[arg(long, default_value_t = false)]
        low_memory: bool,
        /// Recycle a managed Chromium process once its process-tree RSS reaches this threshold (MB).
        #[arg(long, value_name = "MB")]
        recycle_rss_mb: Option<NonZeroU64>,
        /// Optional external lightweight CDP endpoint URL for fast Tier-2 fallback (e.g. ws://127.0.0.1:9222).
        #[arg(long, value_name = "URL")]
        fast_cdp_endpoint: Option<String>,
    },
    /// Coordinate jobs over HTTP for remote `fluxwright serve --join` workers without running browsers locally.
    Coordinate {
        #[arg(long, default_value = "127.0.0.1:7331")]
        listen: SocketAddr,
        /// Active and retained completed jobs combined (1-4096).
        #[arg(long, default_value = "128")]
        max_jobs: NonZeroUsize,
        /// Concurrent running jobs that may target one host at once across all workers.
        #[arg(long)]
        max_per_host: Option<NonZeroUsize>,
        /// Milliseconds between leasing consecutive jobs targeting the same host across the cluster.
        #[arg(long)]
        min_host_delay_ms: Option<NonZeroU64>,
        /// Keep accepted jobs in this file, and queue the unfinished ones again on start.
        #[arg(long, conflicts_with = "store")]
        journal: Option<std::path::PathBuf>,
        /// Persist accepted jobs and completed results in a SQLite database file across restarts.
        #[arg(long, conflicts_with = "journal")]
        store: Option<std::path::PathBuf>,
        /// How long finished job results remain valid in the SQLite store (seconds).
        #[arg(long, default_value_t = 3600)]
        result_ttl_secs: u64,
        /// HMAC-SHA256 signing secret for job completion webhooks (falls back to FLUXWRIGHT_WEBHOOK_SECRET).
        #[arg(long)]
        webhook_secret: Option<String>,
        /// OTLP/HTTP JSON trace collector endpoint base URL (falls back to OTEL_EXPORTER_OTLP_ENDPOINT).
        #[arg(long)]
        otlp_endpoint: Option<String>,
        /// OpenTelemetry service.name attribute (falls back to OTEL_SERVICE_NAME, default "fluxwright-server").
        #[arg(long)]
        otlp_service_name: Option<String>,
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
    /// Download and install managed Chromium binaries into Fluxwright's browser cache.
    Install {
        /// Browser to install: `chrome-headless-shell` (default), `chrome`, `chromium`, or `all`.
        #[arg(default_value = "chrome-headless-shell")]
        browser: String,
        /// Chrome for Testing release channel (`stable`, `beta`, `dev`, `canary`).
        #[arg(long, default_value = "stable")]
        channel: String,
        /// Pin a specific Chrome for Testing version (overrides --channel).
        #[arg(long)]
        version: Option<String>,
        /// Re-download and overwrite even if already installed.
        #[arg(long)]
        force: bool,
        /// Resolve download metadata and print JSON without downloading.
        #[arg(long)]
        dry_run: bool,
        /// Override destination cache directory (defaults to FLUXWRIGHT_BROWSERS_PATH or OS cache dir).
        #[arg(long)]
        path: Option<PathBuf>,
    },
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
            store,
            result_ttl_secs,
            join,
            worker_id,
            prewarm_contexts,
            channel,
            chromium_args,
            allow_query_token,
            allow_session_cdp_endpoint,
            profiles_dir,
            proxies,
            adaptive_concurrency,
            webhook_secret,
            otlp_endpoint,
            otlp_service_name,
            embed_frame_ancestors,
            low_memory,
            recycle_rss_mb,
            fast_cdp_endpoint,
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
                store,
                result_ttl_secs,
                join,
                worker_id,
                prewarm_contexts,
                channel,
                chromium_args,
                allow_query_token,
                allow_session_cdp_endpoint,
                profiles_dir,
                proxies,
                adaptive_concurrency,
                webhook_secret,
                otlp_endpoint,
                otlp_service_name,
                embed_frame_ancestors,
                low_memory,
                recycle_rss_mb,
                fast_cdp_endpoint,
            )
            .await
        }
        Cmd::Coordinate {
            listen,
            max_jobs,
            max_per_host,
            min_host_delay_ms,
            journal,
            store,
            result_ttl_secs,
            webhook_secret,
            otlp_endpoint,
            otlp_service_name,
        } => {
            coordinator_daemon(
                listen,
                max_jobs.get(),
                max_per_host,
                min_host_delay_ms,
                journal,
                store,
                result_ttl_secs,
                webhook_secret,
                otlp_endpoint,
                otlp_service_name,
            )
            .await
        }
        Cmd::Start {
            max_browsers,
            max_contexts,
        } => daemon(sock, max_browsers, max_contexts).await,
        Cmd::Stats => rpc(&sock, Request::Stats).await,
        Cmd::Browsers => rpc(&sock, Request::Browsers).await,
        Cmd::Doctor => doctor().await,
        Cmd::Install {
            browser,
            channel,
            version,
            force,
            dry_run,
            path,
        } => {
            install::run(install::InstallArgs {
                browser,
                channel,
                version,
                force,
                dry_run,
                path,
            })
            .await
        }
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

fn server_config_for(
    is_worker: bool,
    max_jobs: usize,
    journal: Option<std::path::PathBuf>,
    store: Option<std::path::PathBuf>,
    result_ttl_secs: u64,
) -> Result<(String, ServerConfig)> {
    anyhow::ensure!((1..=4096).contains(&max_jobs), "--max-jobs must be between 1 and 4096");
    anyhow::ensure!(
        !(journal.is_some() && store.is_some()),
        "cannot specify both --journal and --store"
    );
    let server_env = std::env::var("FLUXWRIGHT_SERVER_TOKEN").ok();
    let worker_env = worker_token_from_env();
    let (local_token, join_token) = resolve_serve_tokens(is_worker, server_env, worker_env)?;
    let mut config = ServerConfig::new(local_token)?;
    if is_worker {
        // Validate the coordinator worker token upfront as well.
        let _ = ServerConfig::new(join_token.clone())?;
    }
    config.max_jobs = max_jobs;
    config.journal = journal;
    config.store = store;
    config.result_ttl = Duration::from_secs(result_ttl_secs);
    Ok((join_token, config))
}

fn resolve_serve_tokens(
    is_worker: bool,
    server_token: Option<String>,
    worker_token: Option<String>,
) -> Result<(String, String)> {
    let server_token = server_token.filter(|v| !v.is_empty());
    let worker_token = worker_token.filter(|v| !v.is_empty());
    match (is_worker, server_token, worker_token) {
        (true, Some(st), Some(wt)) => Ok((st, wt)),
        (true, Some(st), None) => Ok((st.clone(), st)),
        (true, None, Some(wt)) => Ok((wt.clone(), wt)),
        (true, None, None) => anyhow::bail!(
            "set FLUXWRIGHT_WORKER_TOKEN (or FLUXWRIGHT_SERVER_TOKEN) to a random token (32-256 printable ASCII characters)"
        ),
        (false, Some(st), _) => Ok((st.clone(), st)),
        (false, None, _) => anyhow::bail!(
            "set FLUXWRIGHT_SERVER_TOKEN to a random token (32-256 printable ASCII characters)"
        ),
    }
}

fn nonempty_env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

fn worker_token_from_env() -> Option<String> {
    nonempty_env("FLUXWRIGHT_WORKER_TOKEN")
}

fn resolve_opt_with_env(cli: Option<String>, env_name: &str) -> Option<String> {
    cli.filter(|v| !v.is_empty()).or_else(|| nonempty_env(env_name))
}

fn apply_server_ops_config(
    mut config: ServerConfig,
    webhook_secret: Option<String>,
    otlp_endpoint: Option<String>,
    otlp_service_name: Option<String>,
    embed_frame_ancestors: Option<String>,
) -> Result<ServerConfig> {
    if let Some(secret) = resolve_opt_with_env(webhook_secret, "FLUXWRIGHT_WEBHOOK_SECRET") {
        config = config.with_webhook_secret(secret)?;
    }
    if let Some(endpoint) = resolve_opt_with_env(otlp_endpoint, "OTEL_EXPORTER_OTLP_ENDPOINT") {
        config = config.with_otlp_endpoint(endpoint)?;
    }
    if let Some(svc) = resolve_opt_with_env(otlp_service_name, "OTEL_SERVICE_NAME") {
        config = config.with_otlp_service_name(svc);
    }
    if let Some(policy) = embed_frame_ancestors {
        config = config.with_embed_frame_ancestors(policy)?;
    }
    Ok(config)
}

#[allow(clippy::too_many_arguments)]
async fn coordinator_daemon(
    address: SocketAddr,
    max_jobs: usize,
    max_per_host: Option<NonZeroUsize>,
    min_host_delay_ms: Option<NonZeroU64>,
    journal: Option<std::path::PathBuf>,
    store: Option<std::path::PathBuf>,
    result_ttl_secs: u64,
    webhook_secret: Option<String>,
    otlp_endpoint: Option<String>,
    otlp_service_name: Option<String>,
) -> Result<()> {
    let (_token, mut config) =
        server_config_for(false, max_jobs, journal, store, result_ttl_secs)?;
    config.max_concurrency_per_host = max_per_host.map(NonZeroUsize::get);
    config.min_delay_per_host = min_host_delay_ms.map(|ms| Duration::from_millis(ms.get()));
    let separate_worker_auth = if let Some(wt) = worker_token_from_env() {
        config = config.with_worker_token(wt)?;
        true
    } else {
        false
    };
    config = apply_server_ops_config(
        config,
        webhook_secret,
        otlp_endpoint,
        otlp_service_name,
        None,
    )?;
    let listener = tokio::net::TcpListener::bind(address).await?;
    let server = HttpServer::coordinator(config)?;
    eprintln!(
        "fluxwright coordinator listening on http://{} (bearer authentication required)",
        listener.local_addr()?
    );
    if separate_worker_auth {
        eprintln!("fluxwright coordinator: separate worker bearer authentication enabled");
    }
    serve_http(listener, address, server, None).await
}

fn validate_fast_cdp_endpoint(raw: &str) -> Result<String> {
    Ok(fluxwright_server::validate_fast_cdp_endpoint(raw)?)
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
    store: Option<std::path::PathBuf>,
    result_ttl_secs: u64,
    join: Option<String>,
    worker_id: Option<String>,
    prewarm_contexts: usize,
    channel: Option<String>,
    chromium_args: Vec<String>,
    allow_query_token: bool,
    allow_session_cdp_endpoint: bool,
    profiles_dir: Option<PathBuf>,
    proxies: Vec<String>,
    adaptive_concurrency: bool,
    webhook_secret: Option<String>,
    otlp_endpoint: Option<String>,
    otlp_service_name: Option<String>,
    embed_frame_ancestors: String,
    low_memory: bool,
    recycle_rss_mb: Option<NonZeroU64>,
    fast_cdp_endpoint: Option<String>,
) -> Result<()> {
    let fast_cdp_endpoint = fast_cdp_endpoint
        .as_deref()
        .map(validate_fast_cdp_endpoint)
        .transpose()?;
    let is_worker = join.is_some();
    let (join_token, mut config) =
        server_config_for(is_worker, max_jobs, journal, store, result_ttl_secs)?;
    config.allow_query_token = allow_query_token;
    config.allow_session_cdp_endpoint = allow_session_cdp_endpoint;
    config = apply_server_ops_config(
        config,
        webhook_secret,
        otlp_endpoint,
        otlp_service_name,
        Some(embed_frame_ancestors),
    )?;
    if let Some(endpoint) = fast_cdp_endpoint {
        config = config.with_fast_cdp_endpoint(endpoint)?;
    }
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
    if low_memory {
        builder = builder.low_memory(true);
    }
    if let Some(mb) = recycle_rss_mb {
        builder = builder.recycle_rss_mb(mb.get());
    }
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
    if let Some(dir) = profiles_dir {
        builder = builder.profiles_dir(dir);
    }
    if !proxies.is_empty() {
        builder = builder.proxy_pool(ProxyPool::from_urls(proxies)?);
    }
    if adaptive_concurrency {
        builder = builder.adaptive_concurrency(AdaptiveConfig::default());
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
                join_token,
                worker_id,
                slots,
            )?;
            eprintln!("fluxwright worker joined coordinator {url} ({slots} slots, experimental)");
            if !handle.coordinator_uses_tls() && !handle.coordinator_is_loopback() {
                eprintln!(
                    "warning: --join uses unencrypted HTTP; keep the coordinator link on a trusted private network or local TLS tunnel"
                );
            }
            Some(handle)
        }
        None => None,
    };
    let server = HttpServer::new(engine, config)?;
    let bound = listener.local_addr()?;
    eprintln!("fluxwright HTTP listening on http://{bound} (bearer authentication required)");
    if !is_worker {
        eprintln!("fluxwright CDP gateway listening on ws://{bound}/cdp");
    }
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
        "managed_browsers_dir": fluxwright::managed_browsers_dir().map(|p| p.display().to_string()),
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
                store,
                result_ttl_secs,
                join,
                worker_id,
                prewarm_contexts,
                channel,
                chromium_args,
                allow_query_token,
                allow_session_cdp_endpoint,
                profiles_dir,
                proxies,
                adaptive_concurrency,
                webhook_secret,
                otlp_endpoint,
                otlp_service_name,
                embed_frame_ancestors,
                low_memory,
                recycle_rss_mb,
                fast_cdp_endpoint,
            } => {
                assert!(
                    max_per_host.is_none()
                        && min_host_delay_ms.is_none()
                        && journal.is_none()
                        && store.is_none()
                        && join.is_none()
                        && worker_id.is_none()
                        && channel.is_none()
                        && chromium_args.is_empty()
                        && profiles_dir.is_none()
                        && proxies.is_empty()
                        && webhook_secret.is_none()
                        && otlp_endpoint.is_none()
                        && otlp_service_name.is_none()
                        && recycle_rss_mb.is_none()
                        && fast_cdp_endpoint.is_none()
                );
                assert!(!low_memory);
                assert_eq!(embed_frame_ancestors, "*");
                assert_eq!(result_ttl_secs, 3600);
                assert!(!allow_query_token);
                assert!(!allow_session_cdp_endpoint);
                assert!(!adaptive_concurrency);
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
            "--allow-query-token",
            "--allow-session-cdp-endpoint",
            "--profiles-dir",
            "/tmp/fw-profiles",
            "--proxy",
            "http://127.0.0.1:8080",
            "--proxy",
            "socks5://127.0.0.1:1080",
            "--adaptive-concurrency",
            "--store",
            "/tmp/fw-jobs.sqlite",
            "--result-ttl-secs",
            "120",
            "--webhook-secret",
            "whsec-test",
            "--otlp-endpoint",
            "http://127.0.0.1:4318",
            "--otlp-service-name",
            "fw-custom",
            "--embed-frame-ancestors",
            "'self' https://app.example.com",
        ])
        .expect("serve launch/prewarm/store/ops flags should parse");
        match custom.cmd {
            Cmd::Serve {
                prewarm_contexts,
                channel,
                chromium_args,
                allow_query_token,
                allow_session_cdp_endpoint,
                profiles_dir,
                proxies,
                adaptive_concurrency,
                store,
                result_ttl_secs,
                webhook_secret,
                otlp_endpoint,
                otlp_service_name,
                embed_frame_ancestors,
                ..
            } => {
                assert_eq!(prewarm_contexts, 2);
                assert_eq!(channel.as_deref(), Some("chrome"));
                assert_eq!(chromium_args, vec!["--disable-gpu"]);
                assert!(allow_query_token);
                assert!(allow_session_cdp_endpoint);
                assert_eq!(profiles_dir.as_deref(), Some(std::path::Path::new("/tmp/fw-profiles")));
                assert_eq!(
                    proxies,
                    vec!["http://127.0.0.1:8080", "socks5://127.0.0.1:1080"]
                );
                assert!(adaptive_concurrency);
                assert_eq!(store.as_deref(), Some(std::path::Path::new("/tmp/fw-jobs.sqlite")));
                assert_eq!(result_ttl_secs, 120);
                assert_eq!(webhook_secret.as_deref(), Some("whsec-test"));
                assert_eq!(otlp_endpoint.as_deref(), Some("http://127.0.0.1:4318"));
                assert_eq!(otlp_service_name.as_deref(), Some("fw-custom"));
                assert_eq!(embed_frame_ancestors, "'self' https://app.example.com");
            }
            _ => panic!("expected Cmd::Serve"),
        }
        match Cli::try_parse_from([
            "fluxwright",
            "coordinate",
            "--max-jobs",
            "256",
            "--store",
            "/tmp/coord.sqlite",
            "--result-ttl-secs",
            "600",
            "--max-per-host",
            "2",
            "--min-host-delay-ms",
            "250",
            "--webhook-secret",
            "coord-secret",
            "--otlp-endpoint",
            "https://otlp.example.com/v1/traces",
            "--otlp-service-name",
            "fw-coord",
        ])
        .expect("coordinate store and host-limit flags should parse")
        .cmd
        {
            Cmd::Coordinate {
                max_jobs,
                max_per_host,
                min_host_delay_ms,
                journal,
                store,
                result_ttl_secs,
                webhook_secret,
                otlp_endpoint,
                otlp_service_name,
                ..
            } => {
                assert_eq!(max_jobs.get(), 256);
                assert_eq!(max_per_host.map(NonZeroUsize::get), Some(2));
                assert_eq!(min_host_delay_ms.map(NonZeroU64::get), Some(250));
                assert!(journal.is_none());
                assert_eq!(store.as_deref(), Some(std::path::Path::new("/tmp/coord.sqlite")));
                assert_eq!(result_ttl_secs, 600);
                assert_eq!(webhook_secret.as_deref(), Some("coord-secret"));
                assert_eq!(
                    otlp_endpoint.as_deref(),
                    Some("https://otlp.example.com/v1/traces")
                );
                assert_eq!(otlp_service_name.as_deref(), Some("fw-coord"));
            }
            _ => panic!("expected Cmd::Coordinate"),
        }
        assert!(Cli::try_parse_from([
            "fluxwright",
            "serve",
            "--journal",
            "a.jsonl",
            "--store",
            "b.sqlite",
        ])
        .is_err());
        assert!(Cli::try_parse_from([
            "fluxwright",
            "coordinate",
            "--journal",
            "a.jsonl",
            "--store",
            "b.sqlite",
        ])
        .is_err());
        assert!(Cli::try_parse_from([
            "fluxwright",
            "serve",
            "--join",
            "http://127.0.0.1:7331",
            "--worker-id",
            "w1",
        ])
        .is_ok());
        match Cli::try_parse_from([
            "fluxwright",
            "install",
            "chrome",
            "--channel",
            "beta",
            "--force",
            "--dry-run",
            "--path",
            "/tmp/fw-browsers",
        ])
        .expect("install subcommand should parse")
        .cmd
        {
            Cmd::Install {
                browser,
                channel,
                version,
                force,
                dry_run,
                path,
            } => {
                assert_eq!(browser, "chrome");
                assert_eq!(channel, "beta");
                assert!(version.is_none());
                assert!(force && dry_run);
                assert_eq!(path, Some(PathBuf::from("/tmp/fw-browsers")));
            }
            _ => panic!("expected Cmd::Install"),
        }
        assert!(Cli::try_parse_from(["fluxwright", "install"]).is_ok());
    }

    #[test]
    fn cli_parses_low_memory_recycle_and_fast_cdp_flags() {
        let parsed = Cli::try_parse_from([
            "fluxwright",
            "serve",
            "--low-memory",
            "--recycle-rss-mb",
            "512",
            "--fast-cdp-endpoint",
            "ws://127.0.0.1:9222",
        ])
        .expect("phase 2/3 flags should parse");
        match parsed.cmd {
            Cmd::Serve {
                low_memory,
                recycle_rss_mb,
                fast_cdp_endpoint,
                ..
            } => {
                assert!(low_memory);
                assert_eq!(recycle_rss_mb, Some(NonZeroU64::new(512).unwrap()));
                assert_eq!(
                    fast_cdp_endpoint.as_deref(),
                    Some("ws://127.0.0.1:9222")
                );
            }
            _ => panic!("expected Cmd::Serve"),
        }
        assert!(
            Cli::try_parse_from(["fluxwright", "serve", "--recycle-rss-mb", "0"]).is_err(),
            "--recycle-rss-mb 0 must be rejected by NonZeroU64"
        );
    }

    #[test]
    fn validate_fast_cdp_endpoint_checks_syntax() {
        assert_eq!(
            validate_fast_cdp_endpoint("WS://127.0.0.1:9222/devtools/browser/1").unwrap(),
            "ws://127.0.0.1:9222/devtools/browser/1"
        );
        assert_eq!(
            validate_fast_cdp_endpoint("wss://cdp.example.com:9443/ws").unwrap(),
            "wss://cdp.example.com:9443/ws"
        );
        assert_eq!(
            validate_fast_cdp_endpoint("http://localhost:9222").unwrap(),
            "http://localhost:9222"
        );
        assert_eq!(
            validate_fast_cdp_endpoint("https://[::1]:9222/json/version").unwrap(),
            "https://[::1]:9222/json/version"
        );
        for bad in [
            "",
            "ftp://127.0.0.1:9222",
            "127.0.0.1:9222",
            "ws://",
            "ws://user:pass@127.0.0.1:9222",
            "ws://127.0.0.1:0",
            "ws://127.0.0.1:abc",
            "ws://[::1",
            "ws://127.0.0.1:9222 bad",
        ] {
            assert!(
                validate_fast_cdp_endpoint(bad).is_err(),
                "expected invalid endpoint to fail: {bad:?}"
            );
        }
    }

    #[tokio::test]
    async fn engine_builder_wires_low_memory_and_recycle_rss_mb() {
        let engine = BrowserEngine::builder()
            .low_memory(true)
            .recycle_rss_mb(512)
            .build()
            .await
            .expect("engine");
        assert!(engine.config().launch.low_memory);
        assert_eq!(engine.config().recycle_rss_mb, Some(512));
        engine.shutdown(Duration::from_secs(1)).await.unwrap();
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

    #[test]
    fn resolve_serve_tokens_allows_worker_token_only_with_join() {
        let srv = "server_token_0123456789abcdef0123456789abcdef".to_string();
        let wrk = "worker_token_fedcba9876543210fedcba9876543210".to_string();

        // Worker with ONLY FLUXWRIGHT_WORKER_TOKEN succeeds (least-privilege isolation).
        assert_eq!(
            resolve_serve_tokens(true, None, Some(wrk.clone())).unwrap(),
            (wrk.clone(), wrk.clone())
        );
        // Worker with both tokens keeps them separated.
        assert_eq!(
            resolve_serve_tokens(true, Some(srv.clone()), Some(wrk.clone())).unwrap(),
            (srv.clone(), wrk.clone())
        );
        // Worker with only server token falls back to server token.
        assert_eq!(
            resolve_serve_tokens(true, Some(srv.clone()), None).unwrap(),
            (srv.clone(), srv.clone())
        );
        // Worker with neither fails.
        assert!(resolve_serve_tokens(true, None, None).is_err());

        // Standalone serve requires FLUXWRIGHT_SERVER_TOKEN even if FLUXWRIGHT_WORKER_TOKEN is set.
        assert!(resolve_serve_tokens(false, None, Some(wrk)).is_err());
        assert_eq!(
            resolve_serve_tokens(false, Some(srv.clone()), None).unwrap(),
            (srv.clone(), srv)
        );
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
