#!/usr/bin/env node
// Self-contained multi-worker cluster scaling and SIGKILL failover benchmark.
// Spawns `fluxwright coordinate` + 1, 2, 4 `fluxwright serve --join` workers against a local HTTP
// fixture server to measure horizontal throughput scaling and lease-expiry failover recovery time.

import { spawn } from "node:child_process";
import crypto from "node:crypto";
import fs from "node:fs";
import http from "node:http";
import net from "node:net";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);
const REPO_ROOT = path.resolve(__dirname, "..");

function printUsage() {
  process.stdout.write(
    [
      "Usage: node benchmarks/cluster-bench.mjs [options]",
      "",
      "Options:",
      "  --bin <path>            Path to fluxwright binary (env: FLUXWRIGHT_BIN)",
      "  --workers <list>        Comma-separated worker counts (default: 1,2,4)",
      "  --jobs <n>              Jobs per scaling run (default: 60)",
      "  --max-browsers <n>      Browsers per worker (default: 1)",
      "  --max-contexts <n>      Contexts per browser per worker (default: 4)",
      "  --failover              Run SIGKILL worker failover benchmark (default: true)",
      "  --no-failover           Skip SIGKILL worker failover benchmark",
      "  --out <path>            Output JSON path or directory (default: benchmarks/results/cluster-latest.json)",
      "  --json                  Print JSON report to stdout",
      "  -h, --help              Show this help",
      "",
    ].join("\n"),
  );
}

function parsePositiveInt(raw, flag) {
  const n = Number(raw);
  if (!Number.isInteger(n) || n <= 0) {
    throw new Error(`${flag} must be a positive integer, got: ${raw}`);
  }
  return n;
}

function parseWorkerList(raw) {
  const list = raw
    .split(",")
    .map((s) => s.trim())
    .filter(Boolean)
    .map((s) => parsePositiveInt(s, "--workers"));
  if (list.length === 0) {
    throw new Error("--workers must contain at least one positive integer");
  }
  return list;
}

function parseArgs(argv) {
  const opts = {
    bin: process.env.FLUXWRIGHT_BIN || null,
    workers: [1, 2, 4],
    jobs: 60,
    maxBrowsers: 1,
    maxContexts: 4,
    failover: true,
    out: path.join(REPO_ROOT, "benchmarks", "results", "cluster-latest.json"),
    jsonStdout: false,
    help: false,
  };

  for (let i = 0; i < argv.length; i++) {
    const arg = argv[i];
    const next = () => {
      if (i + 1 >= argv.length) throw new Error(`missing value for ${arg}`);
      return argv[++i];
    };
    if (arg === "-h" || arg === "--help") opts.help = true;
    else if (arg === "--bin") opts.bin = next();
    else if (arg === "--workers") opts.workers = parseWorkerList(next());
    else if (arg === "--jobs") opts.jobs = parsePositiveInt(next(), arg);
    else if (arg === "--max-browsers") opts.maxBrowsers = parsePositiveInt(next(), arg);
    else if (arg === "--max-contexts") opts.maxContexts = parsePositiveInt(next(), arg);
    else if (arg === "--failover") opts.failover = true;
    else if (arg === "--no-failover") opts.failover = false;
    else if (arg.startsWith("--failover=")) {
      const val = arg.slice("--failover=".length).toLowerCase();
      opts.failover = val !== "false" && val !== "0" && val !== "no";
    } else if (arg === "--out") {
      const raw = path.resolve(next());
      opts.out = raw.endsWith(".json") ? raw : path.join(raw, "cluster-latest.json");
    } else if (arg === "--json") opts.jsonStdout = true;
    else throw new Error(`unknown argument: ${arg}`);
  }

  return opts;
}

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function percentile(sorted, p) {
  if (sorted.length === 0) return 0;
  const idx = Math.min(sorted.length - 1, Math.floor((sorted.length - 1) * p));
  return sorted[idx];
}

function getFreePort() {
  return new Promise((resolve, reject) => {
    const srv = net.createServer();
    srv.unref();
    srv.on("error", reject);
    srv.listen(0, "127.0.0.1", () => {
      const addr = srv.address();
      srv.close((err) => {
        if (err) reject(err);
        else resolve(addr.port);
      });
    });
  });
}

function resolveFluxwrightBin(explicitBin) {
  if (explicitBin) {
    if (!fs.existsSync(explicitBin)) {
      throw new Error(`fluxwright binary not found at ${explicitBin}`);
    }
    return explicitBin;
  }
  const ext = process.platform === "win32" ? ".exe" : "";
  const candidates = [
    path.join(REPO_ROOT, "target", "release", `fluxwright${ext}`),
    path.join(REPO_ROOT, "target", "debug", `fluxwright${ext}`),
  ];
  for (const candidate of candidates) {
    if (fs.existsSync(candidate)) return candidate;
  }
  for (const dir of (process.env.PATH || "").split(path.delimiter)) {
    if (!dir) continue;
    const candidate = path.join(dir, `fluxwright${ext}`);
    if (fs.existsSync(candidate)) return candidate;
  }
  throw new Error(
    `Could not find fluxwright binary (checked ${candidates.join(", ")}; pass --bin or set FLUXWRIGHT_BIN)`,
  );
}

async function startFixtureServer() {
  const pageHtml = [
    "<!doctype html>",
    '<html lang="en"><head><meta charset="utf-8"><title>Cluster Bench Page</title></head>',
    "<body><main><h1>Cluster Scaling OK</h1><p>Fluxwright multi-worker benchmark fixture.</p></main></body>",
    "</html>",
  ].join("");

  const holdHtml = [
    "<!doctype html>",
    '<html lang="en"><head><meta charset="utf-8"><title>Failover Hold</title></head>',
    "<body><p>Holding until worker SIGKILL...</p></body>",
    "</html>",
  ].join("");

  const recoveredHtml = [
    "<!doctype html>",
    '<html lang="en"><head><meta charset="utf-8"><title>Failover Recovered</title></head>',
    '<body><h1 id="ready">Recovered</h1></body>',
    "</html>",
  ].join("");

  let failoverHits = 0;
  let resolveFailoverHit = null;
  let failoverStarted = new Promise((resolve) => {
    resolveFailoverHit = resolve;
  });

  const server = http.createServer((req, res) => {
    const pathname = new URL(req.url || "/", "http://127.0.0.1").pathname;
    if (pathname === "/failover") {
      failoverHits += 1;
      const body = failoverHits === 1 ? holdHtml : recoveredHtml;
      if (failoverHits === 1 && resolveFailoverHit) {
        resolveFailoverHit();
        resolveFailoverHit = null;
      }
      res.writeHead(200, {
        "Content-Type": "text/html; charset=utf-8",
        "Content-Length": Buffer.byteLength(body),
        "Cache-Control": "no-store",
      });
      res.end(body);
      return;
    }
    res.writeHead(200, {
      "Content-Type": "text/html; charset=utf-8",
      "Content-Length": Buffer.byteLength(pageHtml),
      "Cache-Control": "no-store",
    });
    res.end(pageHtml);
  });

  await new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", () => resolve());
  });
  const addr = server.address();
  return {
    baseUrl: `http://127.0.0.1:${addr.port}`,
    resetFailover() {
      failoverHits = 0;
      failoverStarted = new Promise((resolve) => {
        resolveFailoverHit = resolve;
      });
    },
    waitFailoverHit(timeoutMs) {
      return Promise.race([
        failoverStarted,
        sleep(timeoutMs).then(() => {
          throw new Error(`timed out waiting ${timeoutMs}ms for worker to hit /failover`);
        }),
      ]);
    },
    getFailoverHits: () => failoverHits,
    close: () => new Promise((resolve) => server.close(() => resolve())),
  };
}

class ProcessGroup {
  constructor(bin) {
    this.bin = bin;
    this.procs = new Map();
    this.logs = [];
  }

  spawn(label, args, extraEnv, removeEnv = []) {
    const env = { ...process.env, ...extraEnv };
    for (const key of removeEnv) {
      delete env[key];
    }
    const child = spawn(this.bin, args, {
      cwd: REPO_ROOT,
      env,
      stdio: ["ignore", "pipe", "pipe"],
    });
    const recordStream = (stream, name) => {
      stream.on("data", (chunk) => {
        for (const line of chunk.toString("utf8").split(/\r?\n/)) {
          if (!line.trim()) continue;
          this.logs.push(`[${label}:${name}] ${line}`);
          if (this.logs.length > 200) this.logs.shift();
        }
      });
    };
    recordStream(child.stdout, "stdout");
    recordStream(child.stderr, "stderr");
    this.procs.set(label, child);
    return child;
  }

  async kill(label, signal = "SIGKILL") {
    const child = this.procs.get(label);
    if (!child) return;
    this.procs.delete(label);
    if (child.exitCode !== null || child.signalCode !== null) return;
    child.kill(signal);
    const deadline = Date.now() + 8_000;
    while (child.exitCode === null && child.signalCode === null && Date.now() < deadline) {
      await sleep(50);
    }
    if (child.exitCode === null && child.signalCode === null) {
      child.kill("SIGKILL");
    }
  }

  async stopAll() {
    const entries = Array.from(this.procs.entries());
    this.procs.clear();
    for (const [, child] of entries) {
      if (child.exitCode === null && child.signalCode === null) {
        child.kill("SIGTERM");
      }
    }
    const deadline = Date.now() + 8_000;
    while (Date.now() < deadline) {
      const alive = entries.some(([, c]) => c.exitCode === null && c.signalCode === null);
      if (!alive) break;
      await sleep(50);
    }
    for (const [, child] of entries) {
      if (child.exitCode === null && child.signalCode === null) {
        child.kill("SIGKILL");
      }
    }
  }

  dumpLogs() {
    return this.logs.join("\n");
  }
}

async function apiJson(baseUrl, token, method, pathname, body) {
  const headers = {};
  if (token) headers.Authorization = `Bearer ${token}`;
  const init = { method, headers };
  if (body !== undefined) {
    headers["Content-Type"] = "application/json";
    init.body = JSON.stringify(body);
  }
  const res = await fetch(`${baseUrl}${pathname}`, init);
  const text = await res.text();
  let json = null;
  try {
    json = JSON.parse(text);
  } catch {
    json = null;
  }
  return { ok: res.ok, status: res.status, json, text };
}

async function waitForHealthz(coordUrl, procs, timeoutMs = 20_000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    try {
      const res = await fetch(`${coordUrl}/healthz`);
      if (res.ok) return;
    } catch {
      // retry until healthy
    }
    await sleep(50);
  }
  throw new Error(`coordinator did not become healthy at ${coordUrl}\n${procs.dumpLogs()}`);
}

async function waitForWorkersOnline(coordUrl, serverToken, expectedNames, procs, timeoutMs = 30_000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const res = await apiJson(coordUrl, serverToken, "GET", "/v1/workers");
    if (res.ok && res.json && Array.isArray(res.json.workers)) {
      const online = new Set(
        res.json.workers.filter((w) => w.status === "online").map((w) => w.worker),
      );
      if (expectedNames.every((name) => online.has(name))) {
        return res.json;
      }
    }
    await sleep(100);
  }
  throw new Error(
    `workers [${expectedNames.join(", ")}] did not register online\n${procs.dumpLogs()}`,
  );
}

async function submitAndWait(coordUrl, serverToken, payload, timeoutMs = 90_000) {
  const t0 = performance.now();
  const sub = await apiJson(coordUrl, serverToken, "POST", "/v1/jobs", payload);
  if (sub.status !== 202 || !sub.json?.id) {
    return {
      ok: false,
      status: "submit_error",
      latencyMs: performance.now() - t0,
      error: `POST /v1/jobs failed (${sub.status}): ${sub.text}`,
    };
  }
  const jobId = sub.json.id;
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const poll = await apiJson(coordUrl, serverToken, "GET", `/v1/jobs/${jobId}`);
    if (poll.ok && poll.json) {
      const st = poll.json.status;
      if (st === "succeeded" || st === "failed" || st === "cancelled") {
        return {
          ok: st === "succeeded",
          status: st,
          job: poll.json,
          latencyMs: performance.now() - t0,
        };
      }
    }
    await sleep(50);
  }
  return {
    ok: false,
    status: "timeout",
    latencyMs: performance.now() - t0,
    error: `job ${jobId} timed out after ${timeoutMs}ms`,
  };
}

async function startCluster(bin, workerIds, opts, serverToken, workerToken) {
  const port = await getFreePort();
  const listen = `127.0.0.1:${port}`;
  const coordUrl = `http://${listen}`;
  const maxJobs = Math.max(256, opts.jobs * 2);
  const procs = new ProcessGroup(bin);

  procs.spawn(
    "coordinator",
    ["coordinate", "--listen", listen, "--max-jobs", String(maxJobs)],
    {
      FLUXWRIGHT_SERVER_TOKEN: serverToken,
      FLUXWRIGHT_WORKER_TOKEN: workerToken,
    },
  );
  await waitForHealthz(coordUrl, procs);

  for (const workerId of workerIds) {
    procs.spawn(
      workerId,
      [
        "serve",
        "--join",
        coordUrl,
        "--worker-id",
        workerId,
        "--max-browsers",
        String(opts.maxBrowsers),
        "--max-contexts",
        String(opts.maxContexts),
      ],
      {
        FLUXWRIGHT_WORKER_TOKEN: workerToken,
        FLUXWRIGHT_HEADLESS: "1",
      },
      ["FLUXWRIGHT_SERVER_TOKEN"],
    );
  }
  await waitForWorkersOnline(coordUrl, serverToken, workerIds, procs);
  return { coordUrl, procs };
}

async function runScalingTier(bin, workerCount, opts, fixtureUrl, serverToken, workerToken) {
  const workerIds = Array.from({ length: workerCount }, (_, i) => `bench-w${i + 1}`);
  const { coordUrl, procs } = await startCluster(bin, workerIds, opts, serverToken, workerToken);
  try {
    const jobPayload = {
      timeout_ms: 60_000,
      steps: [
        { action: "goto", url: fixtureUrl },
        { action: "title" },
        { action: "text_content", selector: "h1" },
      ],
    };

    // Warmup one job per worker so initial Chromium boot is outside the timed batch.
    await Promise.all(
      workerIds.map(() => submitAndWait(coordUrl, serverToken, jobPayload, 60_000)),
    );

    const beforeSnap = await apiJson(coordUrl, serverToken, "GET", "/v1/workers");
    const beforeCounts = new Map(
      (beforeSnap.json?.workers || []).map((w) => [w.worker, Number(w.succeeded_jobs || 0)]),
    );

    const totalSlots = workerCount * opts.maxBrowsers * opts.maxContexts;
    const concurrency = Math.min(opts.jobs, totalSlots);
    const latencies = [];
    let succeeded = 0;
    let failed = 0;
    let nextIdx = 0;

    const tStart = performance.now();
    await Promise.all(
      Array.from({ length: concurrency }, async () => {
        for (;;) {
          const idx = nextIdx++;
          if (idx >= opts.jobs) return;
          const out = await submitAndWait(coordUrl, serverToken, jobPayload, 60_000);
          latencies.push(out.latencyMs);
          if (out.ok) succeeded += 1;
          else failed += 1;
        }
      }),
    );
    const durationMs = Math.max(performance.now() - tStart, 1);

    const afterSnap = await apiJson(coordUrl, serverToken, "GET", "/v1/workers");
    const perWorkerJobs = {};
    for (const w of afterSnap.json?.workers || []) {
      const delta = Number(w.succeeded_jobs || 0) - (beforeCounts.get(w.worker) || 0);
      perWorkerJobs[w.worker] = Math.max(0, delta);
    }

    latencies.sort((a, b) => a - b);
    const jobsPerSec = opts.jobs / (durationMs / 1000);
    return {
      workers: workerCount,
      slots_per_worker: opts.maxBrowsers * opts.maxContexts,
      total_slots: totalSlots,
      jobs: opts.jobs,
      succeeded,
      failed,
      failure_rate: Number((failed / opts.jobs).toFixed(4)),
      duration_ms: Math.round(durationMs),
      jobs_per_second: Number(jobsPerSec.toFixed(2)),
      p50_ms: Number(percentile(latencies, 0.5).toFixed(1)),
      p95_ms: Number(percentile(latencies, 0.95).toFixed(1)),
      p99_ms: Number(percentile(latencies, 0.99).toFixed(1)),
      per_worker_succeeded: perWorkerJobs,
    };
  } finally {
    await procs.stopAll();
  }
}

async function runFailoverBench(bin, opts, fixture, serverToken, workerToken) {
  fixture.resetFailover();
  const workerIds = ["failover-w1", "failover-w2"];
  const { coordUrl, procs } = await startCluster(bin, workerIds, opts, serverToken, workerToken);
  try {
    // Warm up browsers on both workers before measuring failover recovery.
    await Promise.all(
      workerIds.map(() =>
        submitAndWait(
          coordUrl,
          serverToken,
          { timeout_ms: 60_000, steps: [{ action: "goto", url: `${fixture.baseUrl}/` }, { action: "title" }] },
          60_000,
        ),
      ),
    );

    const sub = await apiJson(coordUrl, serverToken, "POST", "/v1/jobs", {
      retries: 1,
      timeout_ms: 90_000,
      steps: [
        { action: "goto", url: `${fixture.baseUrl}/failover` },
        { action: "wait_for_selector", selector: "#ready" },
        { action: "text_content", selector: "#ready" },
        { action: "title" },
      ],
    });
    if (sub.status !== 202 || !sub.json?.id) {
      throw new Error(`failed to submit failover job (${sub.status}): ${sub.text}`);
    }
    const jobId = sub.json.id;

    await fixture.waitFailoverHit(30_000);

    // Identify which worker currently owns the active failover job.
    let targetWorker = null;
    const findDeadline = Date.now() + 5_000;
    while (Date.now() < findDeadline && !targetWorker) {
      const snap = await apiJson(coordUrl, serverToken, "GET", "/v1/workers");
      const busy = (snap.json?.workers || []).find((w) => Number(w.active_jobs || 0) > 0);
      if (busy) targetWorker = busy.worker;
      else await sleep(50);
    }
    if (!targetWorker) {
      targetWorker = workerIds[0];
    }
    const survivorWorker = workerIds.find((w) => w !== targetWorker) || workerIds[1];

    const killStart = performance.now();
    await procs.kill(targetWorker, "SIGKILL");

    const deadline = Date.now() + 90_000;
    let finalJob = null;
    while (Date.now() < deadline) {
      const poll = await apiJson(coordUrl, serverToken, "GET", `/v1/jobs/${jobId}`);
      if (poll.ok && poll.json) {
        const st = poll.json.status;
        if (st === "succeeded" || st === "failed" || st === "cancelled") {
          finalJob = poll.json;
          break;
        }
      }
      await sleep(100);
    }
    const recoveryMs = Math.round(performance.now() - killStart);
    if (!finalJob) {
      throw new Error(`failover job ${jobId} did not finish within 90s\n${procs.dumpLogs()}`);
    }

    const afterWorkers = await apiJson(coordUrl, serverToken, "GET", "/v1/workers");
    const killedInfo = (afterWorkers.json?.workers || []).find((w) => w.worker === targetWorker);
    const recovered =
      finalJob.status === "succeeded" &&
      Array.isArray(finalJob.results) &&
      finalJob.results[2] === "Recovered";

    return {
      recovered,
      status: finalJob.status,
      killed_worker: targetWorker,
      survivor_worker: survivorWorker,
      attempts: Number(finalJob.attempts || 0),
      failover_hits: fixture.getFailoverHits(),
      expired_leases: Number(killedInfo?.expired_leases || 0),
      recovery_ms: recoveryMs,
    };
  } finally {
    await procs.stopAll();
  }
}

function printSummary(report) {
  process.stdout.write("\nCluster Horizontal Scaling\n");
  process.stdout.write(
    "workers | slots | jobs | jobs/s | speedup | eff%  | p50 ms | p95 ms | p99 ms | fail%\n",
  );
  process.stdout.write(
    "--------|-------|------|--------|---------|-------|--------|--------|--------|------\n",
  );
  for (const row of report.scaling) {
    process.stdout.write(
      [
        String(row.workers).padStart(7),
        String(row.total_slots).padStart(5),
        String(row.jobs).padStart(4),
        row.jobs_per_second.toFixed(2).padStart(6),
        `${row.speedup.toFixed(2)}x`.padStart(7),
        `${row.scaling_efficiency_pct.toFixed(1)}%`.padStart(5),
        row.p50_ms.toFixed(1).padStart(6),
        row.p95_ms.toFixed(1).padStart(6),
        row.p99_ms.toFixed(1).padStart(6),
        `${(row.failure_rate * 100).toFixed(1)}%`.padStart(5),
      ].join(" | ") + "\n",
    );
  }
  if (report.failover) {
    const f = report.failover;
    process.stdout.write(
      `\nWorker-Kill (SIGKILL) Failover: recovered=${f.recovered} killed=${f.killed_worker} survivor=${f.survivor_worker} attempts=${f.attempts} expired_leases=${f.expired_leases} recovery_ms=${f.recovery_ms}\n`,
    );
  }
}

async function main() {
  const opts = parseArgs(process.argv.slice(2));
  if (opts.help) {
    printUsage();
    return;
  }

  const bin = resolveFluxwrightBin(opts.bin);
  const serverToken = crypto.randomBytes(32).toString("hex");
  const workerToken = crypto.randomBytes(32).toString("hex");
  const fixture = await startFixtureServer();

  try {
    const scaling = [];
    for (const workerCount of opts.workers) {
      process.stderr.write(`running scaling tier: workers=${workerCount} jobs=${opts.jobs}\n`);
      const tier = await runScalingTier(
        bin,
        workerCount,
        opts,
        `${fixture.baseUrl}/`,
        serverToken,
        workerToken,
      );
      scaling.push(tier);
    }

    const base = scaling[0];
    for (const row of scaling) {
      const speedup = base && base.jobs_per_second > 0 ? row.jobs_per_second / base.jobs_per_second : 1;
      const workerRatio = base && base.workers > 0 ? row.workers / base.workers : 1;
      row.speedup = Number(speedup.toFixed(2));
      row.scaling_efficiency_pct = Number(((speedup / workerRatio) * 100).toFixed(1));
    }

    let failover = null;
    if (opts.failover) {
      process.stderr.write("running worker-kill (SIGKILL) lease-expiry failover benchmark\n");
      failover = await runFailoverBench(bin, opts, fixture, serverToken, workerToken);
    }

    const report = {
      generated_at: new Date().toISOString(),
      os: process.platform,
      arch: process.arch,
      cpus: os.cpus().length,
      binary: bin,
      config: {
        workers: opts.workers,
        jobs: opts.jobs,
        max_browsers_per_worker: opts.maxBrowsers,
        max_contexts_per_browser: opts.maxContexts,
        failover_enabled: opts.failover,
      },
      scaling,
      failover,
    };

    fs.mkdirSync(path.dirname(opts.out), { recursive: true });
    fs.writeFileSync(opts.out, JSON.stringify(report, null, 2) + "\n");

    if (opts.jsonStdout) {
      process.stdout.write(JSON.stringify(report, null, 2) + "\n");
    } else {
      printSummary(report);
      process.stdout.write(`\nwrote ${opts.out}\n`);
    }

    if (failover && !failover.recovered) {
      process.exitCode = 1;
    }
  } finally {
    await fixture.close();
  }
}

main().catch((err) => {
  process.stderr.write(`cluster-bench error: ${err?.stack || err}\n`);
  process.exit(1);
});
