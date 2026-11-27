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
const isPdf = scenario.endsWith("-pdfs");
const isScraping = scenario === "scraping-200";
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

function percentile(sorted, p) {
  if (!sorted.length) return 0;
  const idx = Math.min(Math.floor((sorted.length * p) / 100), sorted.length - 1);
  return sorted[idx];
}

function latencyMetrics(lat, fail, duration_ms) {
  lat.sort((a, b) => a - b);
  return {
    duration_ms,
    jobs_per_second: jobs / Math.max(duration_ms / 1000, 0.001),
    peak_memory_mb: Math.round(process.memoryUsage().rss / 1048576),
    p50_latency_ms: percentile(lat, 50),
    p95_latency_ms: percentile(lat, 95),
    p99_latency_ms: percentile(lat, 99),
    failure_rate: fail / jobs,
  };
}

async function assertPdf(page) {
  const pdf = await page.pdf({ format: "A4", printBackground: true });
  // Puppeteer v24+ returns Uint8Array whose .toString("latin1") ignores encoding; wrap via Buffer.
  if (!pdf || Buffer.from(pdf).subarray(0, 5).toString("latin1") !== "%PDF-") {
    throw new Error("unexpected pdf output");
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
              if (isPdf) await assertPdf(page);
              else await page.title();
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
  record(latencyMetrics(lat, fail, duration_ms));
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
  record(latencyMetrics(lat, fail, duration_ms));
}

async function withPuppeteerCluster() {
  const { Cluster } = await import("puppeteer-cluster");
  const cluster = await Cluster.launch({
    concurrency: Cluster.CONCURRENCY_CONTEXT,
    maxConcurrency: concurrency,
    retryLimit: 2,
    retryDelay: 50,
    timeout: 15_000,
    monitor: false,
    skipDuplicateUrls: false,
    sameDomainDelay: 0,
    puppeteerOptions: {
      executablePath: chrome,
      headless: true,
    },
  });

  // Warmup using an explicit task fn so task metrics below only track timed benchmark jobs.
  await cluster.execute({ targetUrl: url }, async ({ page, data }) => {
    await page.goto(data.targetUrl, { waitUntil: "load", timeout: 15_000 });
    await page.title();
  });

  let fail = 0;
  let succeeded = 0;
  const lat = [];
  cluster.on("taskerror", (_err, data, willRetry) => {
    if (!willRetry) data?.done?.(false);
  });

  await cluster.task(async ({ page, data }) => {
    await page.goto(data.targetUrl, { waitUntil: "load", timeout: 15_000 });
    if (isPdf) await assertPdf(page);
    else await page.title();
    data.done(true);
  });

  let next = 0;
  const t0 = Date.now();
  const workers = Array.from({ length: concurrency }, async () => {
    for (;;) {
      const i = next++;
      if (i >= jobs) return;
      const s = Date.now();
      const passed = await new Promise((resolve) => {
        let settled = false;
        const done = (ok) => {
          if (settled) return;
          settled = true;
          resolve(ok);
        };
        cluster.queue({ targetUrl: url, done });
      });
      if (passed) succeeded++;
      else fail++;
      lat.push(Date.now() - s);
    }
  });
  await withTimeout(Promise.all(workers), Math.max(30_000, budgetMs - 15_000), "cluster");
  const duration_ms = Date.now() - t0;
  await cluster.close().catch(() => {});
  fail += Math.max(0, jobs - succeeded - fail);
  record(latencyMetrics(lat, fail, duration_ms));
}

async function withCrawlee() {
  // Keep stdout clean for single-line JSON output and avoid writing ./storage on disk.
  process.env.CRAWLEE_PERSIST_STORAGE = "0";
  process.env.CRAWLEE_LOG_LEVEL = "OFF";
  const { PlaywrightCrawler, Configuration, log } = await import("crawlee");
  log.setLevel(log.LEVELS.OFF);

  let succeeded = 0;
  let fail = 0;
  const lat = [];
  const config = new Configuration({ persistStorage: false });

  const crawler = new PlaywrightCrawler(
    {
      minConcurrency: concurrency,
      maxConcurrency: concurrency,
      maxRequestRetries: 2,
      navigationTimeoutSecs: 15,
      requestHandlerTimeoutSecs: 20,
      useSessionPool: false,
      keepAlive: false,
      browserPoolOptions: {
        useFingerprints: false,
        maxOpenPagesPerBrowser: nCtx,
        retireBrowserAfterPageCount: 10_000,
      },
      launchContext: {
        useIncognitoPages: true,
        launchOptions: {
          executablePath: chrome,
          headless: true,
        },
      },
      autoscaledPoolOptions: {
        desiredConcurrency: concurrency,
        minConcurrency: concurrency,
        maxConcurrency: concurrency,
        systemStatusOptions: {
          maxMemoryOverloadedRatio: 1,
          maxCpuOverloadedRatio: 1,
          maxEventLoopOverloadedRatio: 1,
        },
      },
      preNavigationHooks: [
        async ({ blockRequests }, gotoOptions) => {
          gotoOptions.waitUntil = "load";
          if (isScraping) await blockRequests();
        },
      ],
      requestHandler: async ({ page }) => {
        if (isPdf) await assertPdf(page);
        else await page.title();
        succeeded++;
      },
      failedRequestHandler: async () => {
        fail++;
      },
    },
    config,
  );

  // Measure each job from worker pickup before browserPool.newPage() through retries and _cleanupContext().
  const firstStart = new Map();
  const finished = new Set();
  const origStart = crawler.stats.startJob.bind(crawler.stats);
  const origFinish = crawler.stats.finishJob.bind(crawler.stats);
  const origFail = crawler.stats.failJob.bind(crawler.stats);
  const origCleanup = crawler._cleanupContext.bind(crawler);
  crawler.stats.startJob = (id, ...rest) => {
    if (!firstStart.has(id)) firstStart.set(id, Date.now());
    return origStart(id, ...rest);
  };
  crawler.stats.finishJob = (id, ...rest) => {
    finished.add(id);
    return origFinish(id, ...rest);
  };
  crawler.stats.failJob = (id, ...rest) => {
    finished.add(id);
    return origFail(id, ...rest);
  };
  crawler._cleanupContext = async (ctx) => {
    try {
      return await origCleanup(ctx);
    } finally {
      const id = ctx?.request?.id || ctx?.request?.uniqueKey;
      if (id && finished.delete(id)) {
        const t = firstStart.get(id);
        if (t !== undefined) {
          firstStart.delete(id);
          lat.push(Date.now() - t);
        }
      }
    }
  };

  const requests = Array.from({ length: jobs }, (_, i) => ({
    url,
    uniqueKey: `job-${i}`,
  }));

  const t0 = Date.now();
  await withTimeout(crawler.run(requests), Math.max(30_000, budgetMs - 15_000), "crawlee");
  const duration_ms = Date.now() - t0;
  await crawler.teardown().catch(() => {});
  fail += Math.max(0, jobs - succeeded - fail);
  record(latencyMetrics(lat, fail, duration_ms));
}

async function bidiSend(bidi, method, params) {
  const res = await bidi.send({ method, params });
  if (!res || res.type === "error" || "error" in res) {
    throw new Error(res?.message || res?.error || `BiDi ${method} failed`);
  }
  return res.result;
}

async function withSelenium() {
  const { Builder } = await import("selenium-webdriver");
  const chromeMod = await import("selenium-webdriver/chrome.js");
  const chromedriver = process.env.FLUXWRIGHT_CHROMEDRIVER;

  const opts = new chromeMod.Options();
  opts.enableBidi();
  if (chrome) opts.setChromeBinaryPath(chrome);
  opts.addArguments("--headless", "--disable-gpu", "--no-first-run", "--no-default-browser-check");
  if (process.env.FLUXWRIGHT_NO_SANDBOX) {
    opts.addArguments("--no-sandbox");
  }

  const sessions = [];
  for (let i = 0; i < nBrowsers; i++) {
    let builder = new Builder().forBrowser("chrome").setChromeOptions(opts);
    if (chromedriver) {
      builder = builder.setChromeService(new chromeMod.ServiceBuilder(chromedriver));
    }
    const driver = await builder.build();
    const bidi = await driver.getBidi();
    sessions.push({ driver, bidi });
    await sleep(50);
  }

  let next = 0;
  let fail = 0;
  const lat = [];
  const t0 = Date.now();
  const workers = Array.from({ length: concurrency }, async () => {
    for (;;) {
      const i = next++;
      if (i >= jobs) return;
      const { bidi } = sessions[i % sessions.length];
      const s = Date.now();
      try {
        await withTimeout(
          (async () => {
            const { userContext } = await bidiSend(bidi, "browser.createUserContext", {});
            try {
              const { context } = await bidiSend(bidi, "browsingContext.create", {
                type: "tab",
                userContext,
              });
              await bidiSend(bidi, "browsingContext.navigate", {
                context,
                url,
                wait: "complete",
              });
              if (isPdf) {
                const pdfRes = await bidiSend(bidi, "browsingContext.print", {
                  context,
                  background: true,
                  page: { width: 21.0, height: 29.7 },
                });
                if (
                  !pdfRes?.data ||
                  Buffer.from(pdfRes.data, "base64").subarray(0, 5).toString("latin1") !== "%PDF-"
                ) {
                  throw new Error("unexpected pdf output");
                }
              } else {
                await bidiSend(bidi, "script.evaluate", {
                  expression: "document.title",
                  target: { context },
                  awaitPromise: false,
                });
              }
            } finally {
              await bidiSend(bidi, "browser.removeUserContext", { userContext }).catch(() => {});
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
  const duration_ms = Date.now() - t0;
  await withTimeout(
    Promise.all(sessions.map(({ driver }) => driver.quit().catch(() => {}))),
    5_000,
    "selenium-quit",
  ).catch(() => {});
  record(latencyMetrics(lat, fail, duration_ms));
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
  else if (tool === "puppeteer-cluster") await withPuppeteerCluster();
  else if (tool === "crawlee") await withCrawlee();
  else if (tool === "selenium") await withSelenium();
  else {
    process.stderr.write(`unknown tool ${tool}\n`);
    process.exit(2);
  }
  clearTimeout(killer);
} catch (e) {
  process.stderr.write(String(e?.stack || e) + "\n");
  process.exit(1);
}
