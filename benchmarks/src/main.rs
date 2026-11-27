use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use clap::Parser;
use fluxwright::{BrowserEngine, JobOptions, PdfOptions, QueueFullMode, ResourcePreset};
use serde::{Deserialize, Serialize};
use sysinfo::{Pid, ProcessesToUpdate, System};

#[derive(Parser)]
struct Args {
    #[arg(long, default_value = "benchmarks/results")]
    out: PathBuf,
    #[arg(long)]
    scenario: Option<String>,
    /// Comma-separated subset of tools to run (default: all).
    #[arg(long, value_delimiter = ',')]
    tools: Option<Vec<String>>,
    /// Also run the chaos scenario: a browser process is killed every two seconds.
    #[arg(long)]
    chaos: bool,
    /// Also run a soak of this many minutes with recycling, sampling memory once a minute.
    #[arg(long)]
    soak_minutes: Option<u64>,
}

fn tool_enabled(filter: &Option<Vec<String>>, name: &str) -> bool {
    match filter {
        None => true,
        Some(list) => list.iter().any(|t| t.trim().eq_ignore_ascii_case(name)),
    }
}

fn percentile(sorted: &[u128], p: usize) -> u128 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = (sorted.len().saturating_mul(p) / 100).min(sorted.len() - 1);
    sorted[idx]
}

#[derive(Serialize, Deserialize, Clone)]
struct Record {
    tool: String,
    scenario: String,
    jobs: u32,
    duration_ms: u128,
    jobs_per_second: f64,
    /// The driver and every process it started (Chrome included), at its highest.
    peak_memory_mb: u64,
    /// Jobs a second for each GB of that peak.
    #[serde(default)]
    jobs_per_gb: f64,
    #[serde(default)]
    p50_latency_ms: u128,
    p95_latency_ms: u128,
    #[serde(default)]
    p99_latency_ms: u128,
    /// Normalized average CPU utilization across the process tree (0-100% of host capacity).
    #[serde(default)]
    avg_cpu_pct: f64,
    /// Peak normalized CPU utilization across the process tree (0-100% of host capacity).
    #[serde(default)]
    peak_cpu_pct: f64,
    failure_rate: f64,
    /// Chaos only: browser processes the harness killed during the run.
    #[serde(default)]
    browsers_killed: u32,
    /// Chrome processes the tool started that were still alive two seconds after it finished.
    #[serde(default)]
    leaked_processes: u32,
    /// Soak only: memory once a minute.
    #[serde(default)]
    memory_samples_mb: Vec<u64>,
    os: String,
    arch: String,
    chrome: Option<String>,
}

impl Record {
    fn measured(mut self, watch: Watched) -> Self {
        self.peak_memory_mb = watch.peak_mb;
        self.jobs_per_gb = self.jobs_per_second / (watch.peak_mb.max(1) as f64 / 1024.0);
        self.avg_cpu_pct = watch.avg_cpu_pct;
        self.peak_cpu_pct = watch.peak_cpu_pct;
        self.browsers_killed = watch.killed;
        self.leaked_processes = watch.leaked;
        if self.scenario.starts_with("soak-") {
            self.memory_samples_mb = watch.samples;
        }
        self
    }
}

fn snapshot() -> System {
    let mut sys = System::new();
    sys.refresh_processes(ProcessesToUpdate::All, true);
    sys
}

/// `root` and every process it started, in one snapshot. Threads are not processes.
fn tree(sys: &System, root: u32) -> Vec<Pid> {
    let mut children: HashMap<Pid, Vec<(Pid, u64)>> = HashMap::new();
    for (pid, p) in sys.processes() {
        if p.thread_kind().is_some() {
            continue;
        }
        if let Some(parent) = p.parent() {
            children.entry(parent).or_default().push((*pid, p.start_time()));
        }
    }
    let root_pid = Pid::from_u32(root);
    let mut seen = HashSet::from([root_pid]);
    let mut found = vec![root_pid];
    let mut at = 0;
    while at < found.len() {
        let pid = found[at];
        let born = sys.process(pid).map_or(0, |p| p.start_time());
        if let Some(kids) = children.get(&pid) {
            for &(kid, start) in kids {
                // Windows keeps a dead parent's pid on surviving children, and a new process can
                // reuse that pid: skip kids older than pid so reused pids never pull in outsiders.
                if start >= born && seen.insert(kid) {
                    found.push(kid);
                }
            }
        }
        at += 1;
    }
    found
}

fn is_chrome_process_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    !name.contains("chromedriver") && (name.contains("chrom") || name.contains("headless"))
}

fn is_chrome(sys: &System, pid: Pid) -> bool {
    sys.process(pid)
        .is_some_and(|p| is_chrome_process_name(&p.name().to_string_lossy()))
}

struct Watched {
    peak_mb: u64,
    avg_cpu_pct: f64,
    peak_cpu_pct: f64,
    killed: u32,
    leaked: u32,
    samples: Vec<u64>,
}

#[derive(Default)]
struct WatchLoopOut {
    peak_mb: u64,
    avg_cpu_pct: f64,
    peak_cpu_pct: f64,
    killed: u32,
    seen: HashSet<(Pid, u64)>,
    samples: Vec<u64>,
}

/// Samples the memory and CPU of `root`'s process tree four times a second on its own thread, so
/// no process-table walk sits inside a timed job. With `chaos`, also kills one of the top-level
/// browsers the driver started every two seconds.
struct Watch {
    stop: Arc<AtomicBool>,
    thread: std::thread::JoinHandle<WatchLoopOut>,
}

impl Watch {
    fn start(root: u32, chaos: bool) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let thread = std::thread::spawn(move || {
            let (mut peak, mut killed, mut seen, mut samples) =
                (0u64, 0u32, HashSet::new(), Vec::new());
            let mut sys = System::new();
            let logical_cores = std::thread::available_parallelism()
                .map(|n| n.get() as f64)
                .unwrap_or(1.0);
            let (mut tick, mut cpu_sum, mut cpu_samples, mut peak_cpu_pct) =
                (0u64, 0.0f64, 0u64, 0.0f64);
            let mut last_kill = Instant::now();
            let mut next_sample = Instant::now();
            while !flag.load(Ordering::Relaxed) {
                sys.refresh_cpu_usage();
                sys.refresh_processes(ProcessesToUpdate::All, true);
                let pids = tree(&sys, root);
                let bytes: u64 = pids
                    .iter()
                    .filter_map(|p| sys.process(*p))
                    .map(|p| p.memory())
                    .sum();
                let mb = bytes / (1024 * 1024);
                peak = peak.max(mb);
                // Skip tick 0 because sysinfo computes CPU% from the delta between refreshes.
                if tick > 0 {
                    let raw_cpu: f64 = pids
                        .iter()
                        .filter_map(|p| sys.process(*p))
                        .map(|p| p.cpu_usage() as f64)
                        .sum();
                    let norm_cpu = (raw_cpu / logical_cores).clamp(0.0, 100.0);
                    cpu_sum += norm_cpu;
                    cpu_samples += 1;
                    peak_cpu_pct = peak_cpu_pct.max(norm_cpu);
                }
                tick += 1;
                if Instant::now() >= next_sample {
                    samples.push(mb);
                    next_sample += Duration::from_secs(60);
                }
                seen.extend(
                    pids.iter()
                        .filter(|p| is_chrome(&sys, **p))
                        .filter_map(|p| sys.process(*p).map(|proc| (*p, proc.start_time()))),
                );
                if chaos && last_kill.elapsed() >= Duration::from_secs(2) {
                    // Target top-level Chrome browser processes even if a driver sits between root
                    // and Chrome, without directly killing renderer/GPU child processes.
                    let browsers: Vec<Pid> = pids
                        .iter()
                        .filter(|p| is_chrome(&sys, **p))
                        .filter(|p| {
                            !sys.process(**p)
                                .and_then(|x| x.parent())
                                .is_some_and(|parent| is_chrome(&sys, parent))
                        })
                        .copied()
                        .collect();
                    if !browsers.is_empty() {
                        let pick =
                            browsers[last_kill.elapsed().subsec_nanos() as usize % browsers.len()];
                        if sys.process(pick).is_some_and(|p| p.kill()) {
                            killed += 1;
                        }
                    }
                    last_kill = Instant::now();
                }
                std::thread::sleep(Duration::from_millis(250));
            }
            WatchLoopOut {
                peak_mb: peak,
                avg_cpu_pct: if cpu_samples == 0 {
                    0.0
                } else {
                    cpu_sum / cpu_samples as f64
                },
                peak_cpu_pct,
                killed,
                seen,
                samples,
            }
        });
        Self { stop, thread }
    }

    /// Call once the tool has shut down: whatever Chrome it started and left behind is a leak.
    fn finish(self) -> Watched {
        self.stop.store(true, Ordering::Relaxed);
        let out = self.thread.join().unwrap_or_default();
        std::thread::sleep(Duration::from_secs(2));
        let sys = snapshot();
        let leaked = out
            .seen
            .iter()
            .filter(|&&(p, born)| {
                is_chrome(&sys, p) && sys.process(p).is_some_and(|proc| proc.start_time() == born)
            })
            .count() as u32;
        Watched {
            peak_mb: out.peak_mb,
            avg_cpu_pct: out.avg_cpu_pct,
            peak_cpu_pct: out.peak_cpu_pct,
            killed: out.killed,
            leaked,
            samples: out.samples,
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    std::fs::create_dir_all(&args.out)?;
    let server = benchmark_server::spawn("127.0.0.1:0").await?;
    let url = format!("{}/", server.base_url);
    // One binary for every tool: chrome-headless-shell when installed, as Playwright defaults to.
    let chrome = fluxwright::find_browser(true).ok();

    let mut scenarios = vec![
        ("1-browser-10-pages", 1, 10, 100u32, false),
        ("10-concurrent-browsers", 10, 2, 40, false),
        ("100-concurrent", 8, 16, 100, false),
        ("500-concurrent", 10, 20, 500, false),
        ("1000-queued", 4, 8, 1000, false),
        ("100-pdfs", 4, 8, 100, false),
        ("scraping-200", 4, 8, 200, false),
    ];
    if args.chaos {
        scenarios.push(("chaos-1000-queued", 4, 8, 1000, true));
    }

    let mut records = Vec::new();
    for (name, browsers, ctx, jobs, chaos) in scenarios {
        if let Some(filter) = &args.scenario {
            if filter != name {
                continue;
            }
        }
        let scenario_url = if name == "scraping-200" {
            format!("{}/scraping", server.base_url)
        } else {
            url.clone()
        };
        eprintln!("scenario {name} jobs={jobs} browsers={browsers} ctx={ctx}");
        if tool_enabled(&args.tools, "fluxwright") {
            let rec = run_fluxwright(
                &scenario_url,
                name,
                browsers,
                ctx,
                jobs,
                chaos,
                chrome.as_ref(),
            )
            .await?;
            records.push(rec);
        }
        let tools: &[&str] = &[
            "playwright",
            "puppeteer",
            "puppeteer-cluster",
            "crawlee",
            "selenium",
        ];
        for &tool in tools {
            if !tool_enabled(&args.tools, tool) {
                continue;
            }
            if let Some(rec) = run_node_tool(
                tool,
                name,
                jobs,
                browsers,
                ctx,
                chaos,
                &scenario_url,
                chrome.as_ref(),
            ) {
                records.push(rec);
            } else {
                eprintln!("skip {tool} {name}: node adapter not installed, or it did not finish");
            }
        }
    }

    if which("rustwright").is_some() {
        eprintln!("rustwright binary present; like-for-like fleet API is not available — skipped");
    }
    eprintln!("kitewright is an MCP server; like-for-like library workload is not possible — skipped");

    if let Some(minutes) = args.soak_minutes {
        records.push(run_soak(&url, minutes, chrome.as_ref()).await?);
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
        "{:<18} {:<24} {:>5} {:>7} {:>8} {:>9} {:>7} {:>7} {:>7} {:>7} {:>6} {:>6} {:>6}",
        "tool",
        "scenario",
        "jobs",
        "jobs/s",
        "peak MB",
        "jobs/s/GB",
        "CPU%",
        "p50 ms",
        "p95 ms",
        "p99 ms",
        "fail",
        "kill",
        "leak"
    );
    for r in records {
        println!(
            "{:<18} {:<24} {:>5} {:>7.1} {:>8} {:>9.1} {:>6.1}% {:>7} {:>7} {:>7} {:>5.1}% {:>6} {:>6}",
            r.tool,
            r.scenario,
            r.jobs,
            r.jobs_per_second,
            r.peak_memory_mb,
            r.jobs_per_gb,
            r.avg_cpu_pct,
            r.p50_latency_ms,
            r.p95_latency_ms,
            r.p99_latency_ms,
            r.failure_rate * 100.0,
            r.browsers_killed,
            r.leaked_processes
        );
    }
    println!();
    println!(
        "Same Chrome, fresh context per job, same job counts. Fluxwright enables prewarm_contexts(2) per browser, \
         and `scraping-200` uses Fluxwright's `ResourcePreset::Scraping` against `/scraping` (images, font, CSS, JS); \
         Crawlee runs PlaywrightCrawler with incognito contexts, maxRequestRetries=2, and `blockRequests()` on \
         `scraping-200`; `puppeteer-cluster` runs CONCURRENCY_CONTEXT (single managed browser) with retryLimit=2; \
         selenium uses Selenium 4 WebDriver BiDi with fresh browser.createUserContext() per job; \
         raw Playwright/Puppeteer/Selenium run default navigation without relaunch code in chaos. In-flight contexts on Node \
         are capped at 24. Memory is total process-tree RSS; CPU% is normalized across all logical host cores."
    );
}

async fn run_fluxwright(
    url: &str,
    scenario: &str,
    max_browsers: usize,
    max_ctx: usize,
    jobs: u32,
    chaos: bool,
    chrome: Option<&PathBuf>,
) -> Result<Record> {
    // min_browsers launches every browser in build(), before the clock starts, as the
    // Node runner does for Playwright/Puppeteer. Lazy launch put up to 9 launches in the timed run.
    let mut builder = BrowserEngine::builder()
        .max_browsers(max_browsers)
        .min_browsers(max_browsers)
        .max_contexts_per_browser(max_ctx)
        .prewarm_contexts(2.min(max_ctx))
        .memory_ceiling_mb(32_768)
        .queue_capacity(jobs as usize + 32)
        .queue_full_mode(QueueFullMode::Wait)
        .acquire_timeout(Duration::from_secs(180));
    if let Some(c) = chrome {
        builder = builder.chrome(c);
    }
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

    let is_pdf = scenario.ends_with("-pdfs");
    let is_scraping = scenario == "scraping-200";
    let watch = Watch::start(std::process::id(), chaos);
    // As node-runner.mjs does: min(jobs, slots, 24) workers, each timing only its own job. Spawning
    // every job at once timed the queue too, so these latencies were not comparable.
    let workers = (jobs as usize).min(max_browsers * max_ctx).clamp(1, 24);
    let next = Arc::new(AtomicU32::new(0));
    let start = Instant::now();
    let mut joins = Vec::new();
    for _ in 0..workers {
        let (engine, url, next) = (engine.clone(), url.to_string(), next.clone());
        joins.push(tokio::spawn(async move {
            let mut done = Vec::new();
            while next.fetch_add(1, Ordering::Relaxed) < jobs {
                let url = url.clone();
                let t0 = Instant::now();
                let mut opts = JobOptions::default().timeout(Duration::from_secs(180));
                if is_scraping {
                    opts = opts.resource_preset(ResourcePreset::Scraping);
                }
                let r = engine
                    .run(opts, move |page| {
                        let url = url.clone();
                        async move {
                            page.goto(&url).await?;
                            if is_pdf {
                                let pdf = page
                                    .pdf(&PdfOptions {
                                        format: Some("A4".into()),
                                        print_background: true,
                                        ..Default::default()
                                    })
                                    .await?;
                                if !pdf.starts_with(b"%PDF-") {
                                    return Err(fluxwright::Error::Other(
                                        "unexpected pdf output".into(),
                                    ));
                                }
                            } else {
                                let _ = page.title().await?;
                            }
                            Ok(())
                        }
                    })
                    .await;
                done.push((r.is_ok(), t0.elapsed()));
            }
            done
        }));
    }
    let mut latencies = Vec::new();
    let mut failures = 0u32;
    for j in joins {
        for (ok, d) in j.await? {
            if !ok {
                failures += 1;
            }
            latencies.push(d.as_millis());
        }
    }
    let duration = start.elapsed();
    latencies.sort();
    let p50 = percentile(&latencies, 50);
    let p95 = percentile(&latencies, 95);
    let p99 = percentile(&latencies, 99);
    engine.shutdown(Duration::from_secs(10)).await.ok();
    let watched = tokio::task::spawn_blocking(move || watch.finish()).await?;
    Ok(Record {
        tool: "fluxwright".into(),
        scenario: scenario.into(),
        jobs,
        duration_ms: duration.as_millis(),
        jobs_per_second: jobs as f64 / duration.as_secs_f64().max(0.001),
        peak_memory_mb: 0,
        jobs_per_gb: 0.0,
        p50_latency_ms: p50,
        p95_latency_ms: p95,
        p99_latency_ms: p99,
        avg_cpu_pct: 0.0,
        peak_cpu_pct: 0.0,
        failure_rate: failures as f64 / jobs as f64,
        browsers_killed: 0,
        leaked_processes: 0,
        memory_samples_mb: Vec::new(),
        os: std::env::consts::OS.into(),
        arch: std::env::consts::ARCH.into(),
        chrome: chrome.map(|p| p.display().to_string()),
    }
    .measured(watched))
}

/// One job after another for `minutes`, with browsers recycled every 50 jobs: memory and latency
/// should end where they began.
async fn run_soak(url: &str, minutes: u64, chrome: Option<&PathBuf>) -> Result<Record> {
    let mut builder = BrowserEngine::builder()
        .max_browsers(3)
        .max_contexts_per_browser(6)
        .recycle_after_jobs(50)
        .memory_ceiling_mb(16_384);
    if let Some(c) = chrome {
        builder = builder.chrome(c);
    }
    let engine = builder.build().await?;
    let watch = Watch::start(std::process::id(), false);
    let deadline = Instant::now() + Duration::from_secs(minutes * 60);
    let (mut jobs, mut failures) = (0u32, 0u32);
    let mut latencies = Vec::new();
    let start = Instant::now();
    while Instant::now() < deadline {
        let url = url.to_string();
        jobs += 1;
        let t0 = Instant::now();
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
        latencies.push(t0.elapsed().as_millis());
        if !ok {
            failures += 1;
        }
    }
    let duration = start.elapsed();
    latencies.sort();
    let p50 = percentile(&latencies, 50);
    let p95 = percentile(&latencies, 95);
    let p99 = percentile(&latencies, 99);
    engine.shutdown(Duration::from_secs(10)).await.ok();
    let watched = tokio::task::spawn_blocking(move || watch.finish()).await?;
    Ok(Record {
        tool: "fluxwright".into(),
        scenario: format!("soak-{minutes}m"),
        jobs,
        duration_ms: duration.as_millis(),
        jobs_per_second: jobs as f64 / duration.as_secs_f64().max(0.001),
        peak_memory_mb: 0,
        jobs_per_gb: 0.0,
        p50_latency_ms: p50,
        p95_latency_ms: p95,
        p99_latency_ms: p99,
        avg_cpu_pct: 0.0,
        peak_cpu_pct: 0.0,
        failure_rate: if jobs == 0 { 0.0 } else { failures as f64 / jobs as f64 },
        browsers_killed: 0,
        leaked_processes: 0,
        memory_samples_mb: Vec::new(),
        os: std::env::consts::OS.into(),
        arch: std::env::consts::ARCH.into(),
        chrome: chrome.map(|p| p.display().to_string()),
    }
    .measured(watched))
}

#[allow(clippy::too_many_arguments)]
fn run_node_tool(
    tool: &str,
    scenario: &str,
    jobs: u32,
    browsers: usize,
    ctx: usize,
    chaos: bool,
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
    cmd.stdout(Stdio::piped()).stderr(Stdio::inherit());
    if let Some(c) = chrome {
        cmd.env("FLUXWRIGHT_CHROMIUM", c);
    }
    // Measured from outside, the same way as Fluxwright: the runner's own figure was its
    // Node process alone, without the browsers.
    let child = cmd.spawn().ok()?;
    let watch = Watch::start(child.id(), chaos);
    let out = child.wait_with_output();
    let watched = watch.finish();
    let out = out.ok()?;
    if !out.status.success() {
        eprintln!("{tool} exited with {}", out.status);
        return None;
    }
    let record: Record = serde_json::from_slice(&out.stdout).ok()?;
    Some(record.measured(watched))
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The harness sees itself and a child it starts, and stops seeing the child once it is gone.
    #[test]
    fn tree_holds_this_process_and_its_children() {
        let me = std::process::id();
        let mut child = if cfg!(windows) {
            Command::new("cmd").args(["/C", "ping -n 30 127.0.0.1 >NUL"]).spawn().unwrap()
        } else {
            Command::new("sleep").arg("30").spawn().unwrap()
        };
        let sys = snapshot();
        let pids = tree(&sys, me);
        assert!(pids.contains(&Pid::from_u32(me)));
        assert!(pids.contains(&Pid::from_u32(child.id())), "child not found under {me}");
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(!tree(&snapshot(), me).contains(&Pid::from_u32(child.id())));
    }

    #[test]
    fn percentile_handles_empty_single_and_ordered() {
        assert_eq!(percentile(&[], 50), 0);
        assert_eq!(percentile(&[42], 50), 42);
        assert_eq!(percentile(&[42], 99), 42);
        let sample = [10, 20, 30, 40, 50, 60, 70, 80, 90, 100];
        assert!(percentile(&sample, 50) <= percentile(&sample, 95));
        assert!(percentile(&sample, 95) <= percentile(&sample, 99));
        assert_eq!(percentile(&sample, 95), 100);
    }

    #[test]
    fn tool_enabled_filters_case_insensitively() {
        assert!(tool_enabled(&None, "crawlee"));
        let filter = Some(vec!["Fluxwright".into(), "Crawlee".into()]);
        assert!(tool_enabled(&filter, "fluxwright"));
        assert!(tool_enabled(&filter, "crawlee"));
        assert!(!tool_enabled(&filter, "puppeteer-cluster"));
    }

    #[test]
    fn chromedriver_name_is_not_classified_as_browser_process() {
        assert!(is_chrome_process_name("chrome"));
        assert!(is_chrome_process_name("chrome-headless-shell"));
        assert!(is_chrome_process_name("chrome.exe"));
        assert!(!is_chrome_process_name("chromedriver"));
        assert!(!is_chrome_process_name("chromedriver.exe"));
    }
}
