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
    /// Validate soak health gates after `--soak-minutes` and exit non-zero if violated.
    #[arg(long)]
    assert_soak: bool,
    /// Maximum allowed post-warmup RSS growth slope (MB/hour) when `--assert-soak` is set.
    #[arg(long, default_value_t = 50.0)]
    max_rss_slope_mb_per_hour: f64,
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
    /// Soak only: Fluxwright temp directories left behind after engine shutdown.
    #[serde(default)]
    leaked_temp_dirs: u32,
    /// Soak only (Linux): open file descriptors in `/proc/self/fd` before the soak.
    #[serde(default)]
    open_fds_start: Option<u64>,
    /// Soak only (Linux): open file descriptors in `/proc/self/fd` after engine shutdown.
    #[serde(default)]
    open_fds_end: Option<u64>,
    /// Soak only: post-warmup linear regression RSS growth slope in MB/hour.
    #[serde(default)]
    rss_slope_mb_per_hour: Option<f64>,
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
            self.rss_slope_mb_per_hour = compute_rss_slope_mb_per_hour(&watch.samples);
            self.memory_samples_mb = watch.samples;
        }
        self
    }
}

const FLUXWRIGHT_TEMP_DIR_PREFIXES: &[&str] = &[
    "fluxwright-profile-",
    "fluxwright-dl-",
    "fluxwright-sess-dl-",
    "fluxwright-cdp-dl-",
];

const MAX_SOAK_FD_DELTA: u64 = 64;

fn is_fluxwright_temp_dir_name(name: &str) -> bool {
    FLUXWRIGHT_TEMP_DIR_PREFIXES
        .iter()
        .any(|prefix| name.starts_with(prefix))
}

fn count_fluxwright_temp_dirs(dir: &Path) -> u32 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(is_fluxwright_temp_dir_name)
        })
        .count() as u32
}

#[cfg(target_os = "linux")]
fn count_open_fds() -> Option<u64> {
    std::fs::read_dir("/proc/self/fd")
        .ok()
        .map(|entries| entries.count() as u64)
}

#[cfg(not(target_os = "linux"))]
fn count_open_fds() -> Option<u64> {
    None
}

/// Ordinary least-squares slope of per-minute RSS samples, converted to MB/hour.
/// Skips the first warmup sample when at least 3 samples were collected.
fn compute_rss_slope_mb_per_hour(samples: &[u64]) -> Option<f64> {
    let series = if samples.len() >= 3 {
        &samples[1..]
    } else {
        samples
    };
    let n = series.len();
    if n < 2 {
        return None;
    }
    let nf = n as f64;
    let mean_x = (n - 1) as f64 / 2.0;
    let mean_y = series.iter().map(|&y| y as f64).sum::<f64>() / nf;
    let mut num = 0.0;
    let mut den = 0.0;
    for (i, &y) in series.iter().enumerate() {
        let dx = i as f64 - mean_x;
        let dy = y as f64 - mean_y;
        num += dx * dy;
        den += dx * dx;
    }
    if den == 0.0 {
        return Some(0.0);
    }
    Some((num / den) * 60.0)
}

fn validate_soak_record(rec: &Record, max_slope_mb_per_hour: f64) -> Result<(), String> {
    let mut violations = Vec::new();
    if rec.jobs == 0 {
        violations.push("soak completed 0 jobs".to_string());
    }
    if rec.leaked_processes > 0 {
        violations.push(format!("leaked_processes={} (expected 0)", rec.leaked_processes));
    }
    if rec.leaked_temp_dirs > 0 {
        violations.push(format!("leaked_temp_dirs={} (expected 0)", rec.leaked_temp_dirs));
    }
    if rec.failure_rate > 0.0 || !rec.failure_rate.is_finite() {
        violations.push(format!(
            "failure_rate={:.4}% (expected 0.0%)",
            rec.failure_rate * 100.0
        ));
    }
    if let Some(slope) = rec.rss_slope_mb_per_hour {
        if !slope.is_finite() || slope > max_slope_mb_per_hour {
            violations.push(format!(
                "rss_slope_mb_per_hour={slope:.2} exceeds max {max_slope_mb_per_hour:.2}"
            ));
        }
    }
    if let (Some(start), Some(end)) = (rec.open_fds_start, rec.open_fds_end) {
        let delta = end.saturating_sub(start);
        if delta > MAX_SOAK_FD_DELTA {
            violations.push(format!(
                "open_fds delta={delta} ({start} -> {end}) exceeds max {MAX_SOAK_FD_DELTA}"
            ));
        }
    }
    if violations.is_empty() {
        Ok(())
    } else {
        Err(violations.join("; "))
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

    let mut soak_failure: Option<String> = None;
    if let Some(minutes) = args.soak_minutes {
        let rec = run_soak(&url, minutes, args.chaos, chrome.as_ref()).await?;
        if args.assert_soak {
            if let Err(err) = validate_soak_record(&rec, args.max_rss_slope_mb_per_hour) {
                soak_failure = Some(err);
            }
        }
        records.push(rec);
    }

    let path = args.out.join("latest.json");
    std::fs::write(&path, serde_json::to_string_pretty(&records)?)?;
    print_table(&records);
    println!("wrote {}", path.display());
    if let Some(err) = soak_failure {
        anyhow::bail!("soak health gate failed: {err}");
    }
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
    for r in records {
        if r.scenario.starts_with("soak-") {
            let slope = r
                .rss_slope_mb_per_hour
                .map_or_else(|| "n/a".to_string(), |s| format!("{s:.2} MB/h"));
            let fds = match (r.open_fds_start, r.open_fds_end) {
                (Some(s), Some(e)) => format!("{s}->{e} (delta={})", e.saturating_sub(s)),
                _ => "n/a".to_string(),
            };
            println!(
                "soak diagnostics ({}): rss_slope={slope}, leaked_temp_dirs={}, open_fds={fds}",
                r.scenario, r.leaked_temp_dirs
            );
        }
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
        leaked_temp_dirs: 0,
        open_fds_start: None,
        open_fds_end: None,
        rss_slope_mb_per_hour: None,
        os: std::env::consts::OS.into(),
        arch: std::env::consts::ARCH.into(),
        chrome: chrome.map(|p| p.display().to_string()),
    }
    .measured(watched))
}

/// One job after another for `minutes`, with browsers recycled every 50 jobs: memory and latency
/// should end where they began.
async fn run_soak(
    url: &str,
    minutes: u64,
    chaos: bool,
    chrome: Option<&PathBuf>,
) -> Result<Record> {
    let temp_dir = std::env::temp_dir();
    let temp_dirs_before = count_fluxwright_temp_dirs(&temp_dir);
    let open_fds_start = count_open_fds();
    let mut builder = BrowserEngine::builder()
        .max_browsers(3)
        .max_contexts_per_browser(6)
        .recycle_after_jobs(50)
        .memory_ceiling_mb(16_384);
    if let Some(c) = chrome {
        builder = builder.chrome(c);
    }
    let engine = builder.build().await?;
    let watch = Watch::start(std::process::id(), chaos);
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
    let leaked_temp_dirs = count_fluxwright_temp_dirs(&temp_dir).saturating_sub(temp_dirs_before);
    let open_fds_end = count_open_fds();
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
        leaked_temp_dirs,
        open_fds_start,
        open_fds_end,
        rss_slope_mb_per_hour: None,
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

    #[test]
    fn compute_rss_slope_handles_flat_and_growing_series() {
        assert_eq!(compute_rss_slope_mb_per_hour(&[]), None);
        assert_eq!(compute_rss_slope_mb_per_hour(&[200]), None);

        // Two-point series has no warmup sample skipped: +2 MB/min = +120 MB/hour.
        let two = compute_rss_slope_mb_per_hour(&[200, 202]).unwrap();
        assert!((two - 120.0).abs() < 1e-6, "got {two}");

        // Warmup jump from 50 -> 200 MB is skipped when len >= 3, leaving a flat post-warmup series.
        let flat = compute_rss_slope_mb_per_hour(&[50, 200, 200, 200, 200]).unwrap();
        assert!(flat.abs() < 1e-6, "expected 0 slope, got {flat}");

        // +1 MB/min post-warmup -> +60 MB/hour.
        let growing = compute_rss_slope_mb_per_hour(&[50, 200, 201, 202, 203]).unwrap();
        assert!((growing - 60.0).abs() < 1e-6, "got {growing}");

        // Shrinking post-warmup series -> -60 MB/hour.
        let shrinking = compute_rss_slope_mb_per_hour(&[50, 203, 202, 201, 200]).unwrap();
        assert!((shrinking + 60.0).abs() < 1e-6, "got {shrinking}");
    }

    fn healthy_soak_record() -> Record {
        Record {
            tool: "fluxwright".into(),
            scenario: "soak-120m".into(),
            jobs: 1200,
            duration_ms: 7_200_000,
            jobs_per_second: 0.16,
            peak_memory_mb: 512,
            jobs_per_gb: 0.32,
            p50_latency_ms: 40,
            p95_latency_ms: 90,
            p99_latency_ms: 150,
            avg_cpu_pct: 12.0,
            peak_cpu_pct: 30.0,
            failure_rate: 0.0,
            browsers_killed: 40,
            leaked_processes: 0,
            memory_samples_mb: vec![120, 400, 405, 402, 404],
            leaked_temp_dirs: 0,
            open_fds_start: Some(24),
            open_fds_end: Some(28),
            rss_slope_mb_per_hour: Some(10.0),
            os: "linux".into(),
            arch: "x86_64".into(),
            chrome: None,
        }
    }

    #[test]
    fn validate_soak_record_checks_leaks_failures_and_slope() {
        let base = healthy_soak_record();
        assert!(validate_soak_record(&base, 50.0).is_ok());

        let mut leaked_proc = base.clone();
        leaked_proc.leaked_processes = 1;
        assert!(validate_soak_record(&leaked_proc, 50.0)
            .unwrap_err()
            .contains("leaked_processes=1"));

        let mut leaked_tmp = base.clone();
        leaked_tmp.leaked_temp_dirs = 2;
        assert!(validate_soak_record(&leaked_tmp, 50.0)
            .unwrap_err()
            .contains("leaked_temp_dirs=2"));

        let mut failed_jobs = base.clone();
        failed_jobs.failure_rate = 0.01;
        assert!(validate_soak_record(&failed_jobs, 50.0)
            .unwrap_err()
            .contains("failure_rate="));

        let mut steep_slope = base.clone();
        steep_slope.rss_slope_mb_per_hour = Some(75.5);
        assert!(validate_soak_record(&steep_slope, 50.0)
            .unwrap_err()
            .contains("rss_slope_mb_per_hour=75.50"));

        let mut fd_leak = base.clone();
        fd_leak.open_fds_start = Some(20);
        fd_leak.open_fds_end = Some(20 + MAX_SOAK_FD_DELTA + 1);
        assert!(validate_soak_record(&fd_leak, 50.0)
            .unwrap_err()
            .contains("open_fds delta="));

        // Ensure legacy Record JSON without new fields still deserializes cleanly.
        let legacy_json = r#"{
            "tool": "fluxwright",
            "scenario": "1-browser-10-pages",
            "jobs": 100,
            "duration_ms": 1000,
            "jobs_per_second": 100.0,
            "peak_memory_mb": 200,
            "p95_latency_ms": 50,
            "failure_rate": 0.0,
            "os": "linux",
            "arch": "x86_64",
            "chrome": null
        }"#;
        let parsed: Record = serde_json::from_str(legacy_json).unwrap();
        assert_eq!(parsed.leaked_temp_dirs, 0);
        assert_eq!(parsed.open_fds_start, None);
        assert_eq!(parsed.open_fds_end, None);
        assert_eq!(parsed.rss_slope_mb_per_hour, None);
    }

    #[test]
    fn fluxwright_temp_dir_matcher_identifies_owned_prefixes() {
        assert!(is_fluxwright_temp_dir_name("fluxwright-profile-123"));
        assert!(is_fluxwright_temp_dir_name("fluxwright-dl-abc"));
        assert!(is_fluxwright_temp_dir_name("fluxwright-sess-dl-xyz"));
        assert!(is_fluxwright_temp_dir_name("fluxwright-cdp-dl-999"));
        assert!(!is_fluxwright_temp_dir_name("fluxwright-other-123"));
        assert!(!is_fluxwright_temp_dir_name("chrome-profile-123"));
    }
}
