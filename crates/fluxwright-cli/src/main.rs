use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use fluxwright::BrowserEngine;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

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
    serve(sock, engine).await
}

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
        server.connect().await?;
        let engine = engine.clone();
        tokio::spawn(async move {
            let _ = pipe_session(server, engine).await;
        });
    }
}

#[cfg(windows)]
async fn pipe_session(
    mut pipe: tokio::net::windows::named_pipe::NamedPipeServer,
    engine: BrowserEngine,
) -> Result<()> {
    let mut acc = Vec::new();
    let mut tmp = [0u8; 2048];
    loop {
        let n = pipe.read(&mut tmp).await?;
        if n == 0 {
            break;
        }
        acc.extend_from_slice(&tmp[..n]);
        while let Some(idx) = acc.iter().position(|b| *b == b'\n') {
            let line = acc.drain(..=idx).collect::<Vec<_>>();
            let line = String::from_utf8_lossy(&line);
            let reply = dispatch(&engine, line.trim()).await;
            pipe.write_all(serde_json::to_string(&reply)?.as_bytes())
                .await?;
            pipe.write_all(b"\n").await?;
        }
    }
    Ok(())
}

#[cfg(unix)]
async fn serve(sock: PathBuf, engine: BrowserEngine) -> Result<()> {
    use tokio::net::UnixListener;
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock)?;
    loop {
        let (stream, _) = listener.accept().await?;
        let engine = engine.clone();
        tokio::spawn(async move {
            let _ = unix_session(stream, engine).await;
        });
    }
}

#[cfg(unix)]
async fn unix_session(stream: tokio::net::UnixStream, engine: BrowserEngine) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut acc = Vec::new();
    let mut tmp = [0u8; 2048];
    let mut reader = reader;
    loop {
        let n = reader.read(&mut tmp).await?;
        if n == 0 {
            break;
        }
        acc.extend_from_slice(&tmp[..n]);
        while let Some(idx) = acc.iter().position(|b| *b == b'\n') {
            let line = acc.drain(..=idx).collect::<Vec<_>>();
            let line = String::from_utf8_lossy(&line);
            let reply = dispatch(&engine, line.trim()).await;
            writer
                .write_all(serde_json::to_string(&reply)?.as_bytes())
                .await?;
            writer.write_all(b"\n").await?;
        }
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

async fn rpc(sock: &PathBuf, req: Request) -> Result<()> {
    let payload = serde_json::to_vec(&req)?;
    #[cfg(windows)]
    {
        use tokio::net::windows::named_pipe::ClientOptions;
        let mut client = ClientOptions::new().open(sock)?;
        client.write_all(&payload).await?;
        client.write_all(b"\n").await?;
        let mut buf = Vec::new();
        let mut tmp = [0u8; 2048];
        loop {
            let n = client.read(&mut tmp).await?;
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&tmp[..n]);
            if buf.contains(&b'\n') {
                break;
            }
        }
        print!("{}", String::from_utf8_lossy(&buf));
    }
    #[cfg(unix)]
    {
        use tokio::net::UnixStream;
        let mut stream = UnixStream::connect(sock).await?;
        stream.write_all(&payload).await?;
        stream.write_all(b"\n").await?;
        let mut buf = Vec::new();
        let mut tmp = [0u8; 2048];
        loop {
            let n = stream.read(&mut tmp).await?;
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&tmp[..n]);
            if buf.contains(&b'\n') {
                break;
            }
        }
        print!("{}", String::from_utf8_lossy(&buf));
    }
    Ok(())
}

async fn doctor() -> Result<()> {
    let chrome = fluxwright::find_chrome(None);
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
