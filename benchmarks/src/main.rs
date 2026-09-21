use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::Result;
use clap::Parser;
use fluxwright::{BrowserEngine, JobOptions, QueueFullMode};
use serde::{Deserialize, Serialize};
use sysinfo::{Pid, ProcessesToUpdate, System};

#[derive(Parser)]
struct Args {
    #[arg(long, default_value = "benchmarks/results")]
    out: PathBuf,
    #[arg(long)]
    scenario: Option<String>,
    /// Two-hour soak with recycling; records RSS samples.
    #[arg(long)]
    soak: bool,
}

#[derive(Serialize, Deserialize, Clone)]
struct Record {
    tool: String,
    scenario: String,
    jobs: u32,
    duration_ms: u128,
    jobs_per_second: f64,
    peak_memory_mb: u64,
    p95_latency_ms: u128,
    failure_rate: f64,
    os: String,
    arch: String,
    chrome: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    std::fs::create_dir_all(&args.out)?;
    let server = benchmark_server::spawn("127.0.0.1:0").await?;
    let url = format!("{}/", server.base_url);
    let chrome = fluxwright::find_chrome(None).ok();

    let scenarios = [
        ("1-browser-10-pages", 1, 10, 100u32),
        ("10-concurrent-browsers", 10, 2, 40),
        ("100-concurrent", 8, 16, 100),
        ("500-concurrent", 10, 20, 500),
        ("1000-queued", 4, 8, 1000),
    ];

    let mut records = Vec::new();
    for (name, browsers, ctx, jobs) in scenarios {
        if let Some(filter) = &args.scenario {
            if filter != name {
                continue;
            }
        }
        eprintln!("scenario {name} jobs={jobs} browsers={browsers} ctx={ctx}");
        let rec = run_fluxwright(&url, name, browsers, ctx, jobs, chrome.as_ref()).await?;
        records.push(rec);
        for tool in ["playwright", "puppeteer"] {
            if let Some(rec) =
                run_node_tool(tool, name, jobs, browsers, ctx, &url, chrome.as_ref())
            {
                records.push(rec);
            } else {
                eprintln!("skip {tool} {name}: node adapter not installed");
            }
        }
    }

    if which("rustwright").is_some() {
        eprintln!("rustwright binary present; like-for-like fleet API is not available — skipped");
    }
    eprintln!("kitewright is an MCP server; like-for-like library workload is not possible — skipped");

    if args.soak {
        records.push(run_soak(&url, chrome.as_ref()).await?);
    }

    let path = args.out.join("latest.json");
    std::fs::write(&path, serde_json::to_string_pretty(&records)?)?;
    print_table(&records);
    println!("wrote {}", path.display());
    Ok(())
}

fn print_table(records: &[Record]) {
    println!();
    println!(
        "{:<12} {:<24} {:>6} {:>10} {:>8} {:>10} {:>8}",
        "tool", "scenario", "jobs", "ms", "jobs/s", "p95_ms", "fail"
    );
    for r in records {
        println!(
            "{:<12} {:<24} {:>6} {:>10} {:>8.2} {:>10} {:>8.1}%",
            r.tool,
            r.scenario,
            r.jobs,
            r.duration_ms,
            r.jobs_per_second,
            r.p95_latency_ms,
            r.failure_rate * 100.0
        );
    }
    println!();
    println!(
        "Same Chrome, fresh context per job, same job counts. Playwright/Puppeteer in-flight contexts capped at 24 so Chrome does not deadlock on Windows. Driver RSS only. Not a ranking."
    );
}

async fn run_fluxwright(
    url: &str,
    scenario: &str,
    max_browsers: usize,
    max_ctx: usize,
    jobs: u32,
    chrome: Option<&PathBuf>,
) -> Result<Record> {
    let builder = BrowserEngine::builder()
        .max_browsers(max_browsers)
        .max_contexts_per_browser(max_ctx)
        .memory_ceiling_mb(32_768)
        .queue_capacity(jobs as usize + 32)
        .queue_full_mode(QueueFullMode::Wait)
        .acquire_timeout(Duration::from_secs(180));
    let _ = chrome;
    let engine = builder.build().await?;
    // warmup
    {
        let url = url.to_string();
        let _ = engine
            .run(JobOptions::default(), move |page| {
                let url = url.clone();
                async move {
                    page.goto(&url).await?;
                    Ok(())
                }
            })
            .await;
    }

    let mut latencies = Vec::new();
    let mut failures = 0u32;
    let start = Instant::now();
    let mut peak = current_rss_mb();
    let mut joins = Vec::new();
    for _ in 0..jobs {
        let engine = engine.clone();
        let url = url.to_string();
        joins.push(tokio::spawn(async move {
            let t0 = Instant::now();
            let r = engine
                .run(JobOptions::default().timeout(Duration::from_secs(180)), move |page| {
                    let url = url.clone();
                    async move {
                        page.goto(&url).await?;
                        let _ = page.title().await?;
                        Ok(())
                    }
                })
                .await;
            (r.is_ok(), t0.elapsed())
        }));
    }
    for j in joins {
        let (ok, d) = j.await?;
        if !ok {
            failures += 1;
        }
        latencies.push(d.as_millis());
        peak = peak.max(current_rss_mb());
    }
    let duration = start.elapsed();
    latencies.sort();
    let p95 = latencies
        .get(latencies.len().saturating_mul(95) / 100)
        .copied()
        .unwrap_or(0);
    engine.shutdown(Duration::from_secs(10)).await.ok();
    Ok(Record {
        tool: "fluxwright".into(),
        scenario: scenario.into(),
        jobs,
        duration_ms: duration.as_millis(),
        jobs_per_second: jobs as f64 / duration.as_secs_f64().max(0.001),
        peak_memory_mb: peak,
        p95_latency_ms: p95,
        failure_rate: failures as f64 / jobs as f64,
        os: std::env::consts::OS.into(),
        arch: std::env::consts::ARCH.into(),
        chrome: chrome.map(|p| p.display().to_string()),
    })
}

async fn run_soak(url: &str, chrome: Option<&PathBuf>) -> Result<Record> {
    let engine = BrowserEngine::builder()
        .max_browsers(3)
        .max_contexts_per_browser(6)
        .recycle_after_jobs(50)
        .memory_ceiling_mb(16_384)
        .build()
        .await?;
    let deadline = Instant::now() + Duration::from_secs(2 * 60 * 60);
    let mut jobs = 0u32;
    let mut failures = 0u32;
    let mut peak = current_rss_mb();
    let start = Instant::now();
    while Instant::now() < deadline {
        let url = url.to_string();
        jobs += 1;
        let ok = engine
            .run(JobOptions::default(), move |page| {
                let url = url.clone();
                async move {
                    page.goto(&url).await?;
                    Ok(())
                }
            })
            .await
            .is_ok();
        if !ok {
            failures += 1;
        }
        peak = peak.max(current_rss_mb());
    }
    let duration = start.elapsed();
    engine.shutdown(Duration::from_secs(10)).await.ok();
    Ok(Record {
        tool: "fluxwright".into(),
        scenario: "soak-2h".into(),
        jobs,
        duration_ms: duration.as_millis(),
        jobs_per_second: jobs as f64 / duration.as_secs_f64().max(0.001),
        peak_memory_mb: peak,
        p95_latency_ms: 0,
        failure_rate: if jobs == 0 { 0.0 } else { failures as f64 / jobs as f64 },
        os: std::env::consts::OS.into(),
        arch: std::env::consts::ARCH.into(),
        chrome: chrome.map(|p| p.display().to_string()),
    })
}

fn run_node_tool(
    tool: &str,
    scenario: &str,
    jobs: u32,
    browsers: usize,
    ctx: usize,
    url: &str,
    chrome: Option<&PathBuf>,
) -> Option<Record> {
    let script = Path::new("benchmarks/node-runner.mjs");
    if !script.exists() {
        return None;
    }
    eprintln!("  {tool} {scenario} jobs={jobs}");
    let mut cmd = Command::new("node");
    cmd.arg(script).arg(tool).arg(url);
    cmd.env("BENCH_JOBS", jobs.to_string());
    cmd.env("BENCH_BROWSERS", browsers.to_string());
    cmd.env("BENCH_CONTEXTS", ctx.to_string());
    cmd.env("BENCH_SCENARIO", scenario);
    cmd.stderr(std::process::Stdio::inherit());
    if let Some(c) = chrome {
        cmd.env("FLUXWRIGHT_CHROMIUM", c);
    }
    let out = cmd.output().ok()?;
    if !out.status.success() {
        eprintln!("{tool} stderr {}", String::from_utf8_lossy(&out.stderr));
        return None;
    }
    serde_json::from_slice(&out.stdout).ok()
}

fn which(bin: &str) -> Option<PathBuf> {
    let cmd = if cfg!(windows) { "where" } else { "which" };
    Command::new(cmd)
        .arg(bin)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .next()
                .map(PathBuf::from)
        })
}

fn current_rss_mb() -> u64 {
    let mut sys = System::new();
    sys.refresh_processes(ProcessesToUpdate::All, true);
    sys.process(Pid::from_u32(std::process::id()))
        .map(|p| p.memory() / (1024 * 1024))
        .unwrap_or(0)
}
