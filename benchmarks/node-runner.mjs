#!/usr/bin/env node
// Same Chromium binary, fresh context per job, same job counts as Fluxwright.
// In-flight contexts are capped so Chrome on Windows does not deadlock.
import { availableParallelism } from "node:os";

const [tool, url] = process.argv.slice(2);
const chrome = process.env.FLUXWRIGHT_CHROMIUM || process.env.CHROME;
const jobs = Number(process.env.BENCH_JOBS || 20);
const nBrowsers = Math.max(1, Number(process.env.BENCH_BROWSERS || 1));
const nCtx = Math.max(1, Number(process.env.BENCH_CONTEXTS || 1));
const scenario = process.env.BENCH_SCENARIO || "node-smoke";
const slotTarget = nBrowsers * nCtx;
const concurrency = Math.min(jobs, slotTarget, 24);
const jobTimeoutMs = 30_000;
const budgetMs = Math.min(8 * 60_000, 45_000 + jobs * 2_000);

function record(partial) {
  const body = {
    tool,
    scenario,
    jobs,
    os: process.platform,
    arch: process.arch,
    chrome: chrome || null,
    ...partial,
  };
  process.stdout.write(JSON.stringify(body));
}

function sleep(ms) {
  return new Promise((r) => setTimeout(r, ms));
}

async function withTimeout(promise, ms, label) {
  let timer;
  try {
    return await Promise.race([
      promise,
      new Promise((_, rej) => {
        timer = setTimeout(() => rej(new Error(`${label} timeout ${ms}ms`)), ms);
      }),
    ]);
  } finally {
    clearTimeout(timer);
  }
}

async function runPool(browsers, newContext) {
  let next = 0;
  let fail = 0;
  const lat = [];
  const workers = Array.from({ length: concurrency }, async () => {
    for (;;) {
      const i = next++;
      if (i >= jobs) return;
      const browser = browsers[i % browsers.length];
      const s = Date.now();
      try {
        await withTimeout(
          (async () => {
            const ctx = await newContext(browser);
            try {
              const page = await ctx.newPage();
              await page.goto(url, { waitUntil: "load", timeout: jobTimeoutMs });
              await page.title();
            } finally {
              await ctx.close();
            }
          })(),
          jobTimeoutMs + 5_000,
          "job",
        );
      } catch {
        fail++;
      }
      lat.push(Date.now() - s);
    }
  });
  await Promise.all(workers);
  lat.sort((a, b) => a - b);
  return { fail, lat };
}

async function withPlaywright() {
  const { chromium } = await import("playwright");
  const browsers = [];
  for (let i = 0; i < nBrowsers; i++) {
    browsers.push(
      await chromium.launch({
        executablePath: chrome,
        headless: true,
      }),
    );
    await sleep(50);
  }
  const t0 = Date.now();
  const { fail, lat } = await runPool(browsers, (b) => b.newContext());
  const duration_ms = Date.now() - t0;
  await Promise.all(browsers.map((b) => b.close().catch(() => {})));
  record({
    duration_ms,
    jobs_per_second: jobs / Math.max(duration_ms / 1000, 0.001),
    peak_memory_mb: Math.round(process.memoryUsage().rss / 1048576),
    p95_latency_ms: lat[Math.floor(lat.length * 0.95)] || 0,
    failure_rate: fail / jobs,
  });
}

async function withPuppeteer() {
  const puppeteer = (await import("puppeteer")).default;
  const browsers = [];
  for (let i = 0; i < nBrowsers; i++) {
    browsers.push(
      await puppeteer.launch({
        executablePath: chrome,
        headless: true,
      }),
    );
    await sleep(50);
  }
  const t0 = Date.now();
  const { fail, lat } = await runPool(browsers, (b) => b.createBrowserContext());
  const duration_ms = Date.now() - t0;
  await Promise.all(browsers.map((b) => b.close().catch(() => {})));
  record({
    duration_ms,
    jobs_per_second: jobs / Math.max(duration_ms / 1000, 0.001),
    peak_memory_mb: Math.round(process.memoryUsage().rss / 1048576),
    p95_latency_ms: lat[Math.floor(lat.length * 0.95)] || 0,
    failure_rate: fail / jobs,
  });
}

try {
  if (!url) {
    process.stderr.write("usage: node-runner.mjs <tool> <url>\n");
    process.exit(2);
  }
  process.stderr.write(
    `${tool} scenario=${scenario} jobs=${jobs} browsers=${nBrowsers} ctx=${nCtx} in_flight=${concurrency} (cap 24) cores=${availableParallelism()}\n`,
  );
  const killer = setTimeout(() => {
    process.stderr.write(`node-runner exceeded ${budgetMs}ms budget\n`);
    process.exit(1);
  }, budgetMs);
  killer.unref?.();
  if (tool === "playwright") await withPlaywright();
  else if (tool === "puppeteer") await withPuppeteer();
  else {
    process.stderr.write(`unknown tool ${tool}\n`);
    process.exit(2);
  }
  clearTimeout(killer);
} catch (e) {
  process.stderr.write(String(e?.stack || e) + "\n");
  process.exit(1);
}
