#!/usr/bin/env node
// Reproducible benchmark comparing POST /v1/scrape and POST /v1/map across:
//   - mode: "auto" (Tier 1 HTTP fast path + SPA escalation to browser)
//   - mode: "browser" with resource_preset: "scraping"
//   - mode: "browser" without explicit resource_preset
// Reports req/s, p50/p95 ms, engine breakdown, and server RSS MB.

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

const ONE_BY_ONE_PNG = Buffer.from(
  "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=",
  "base64",
);
const HERO_PNG = Buffer.alloc(131_072, 0x89);
const BANNER_JPG = Buffer.alloc(131_072, 0xff);
const FONT_WOFF2 = Buffer.alloc(65_536, 0x77);

function printUsage() {
  process.stdout.write(
    [
      "Usage: node benchmarks/fastpath-bench.mjs [options]",
      "",
      "Options:",
      "  --jobs <n>                Requests per scenario (default: 30, env: BENCH_JOBS)",
      "  --concurrency <n>         In-flight requests per scenario (default: 8, env: BENCH_CONCURRENCY)",
      "  --timeout-ms <ms>         Per-request timeout_ms (default: 30000, env: BENCH_TIMEOUT_MS)",
      "  --scenario <name>         Run only matching scenario name (env: BENCH_SCENARIO)",
      "  --out <dir>               Output directory for fastpath-latest.json (default: benchmarks/results)",
      "  --no-write                Do not write JSON output file",
      "  --json                    Print JSON report to stdout",
      "  --server-url <url>        Use an existing fluxwright serve instance instead of spawning one",
      "  --token <token>           Bearer token (defaults to FLUXWRIGHT_SERVER_TOKEN or random)",
      "  --bin <path>              Path to fluxwright binary (env: FLUXWRIGHT_BIN)",
      "  --max-browsers <n>        Passed as --max-browsers when spawning fluxwright serve",
      "  --max-contexts <n>        Passed as --max-contexts when spawning (default: 8)",
      "  --low-memory              Pass --low-memory when spawning fluxwright serve",
      "  --recycle-rss-mb <mb>     Pass --recycle-rss-mb when spawning fluxwright serve",
      "  --fast-cdp-endpoint <u>   Pass --fast-cdp-endpoint when spawning fluxwright serve",
      "  -h, --help                Show this help",
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

function parseArgs(argv) {
  const opts = {
    jobs: parsePositiveInt(process.env.BENCH_JOBS || "30", "BENCH_JOBS"),
    concurrency: parsePositiveInt(process.env.BENCH_CONCURRENCY || "8", "BENCH_CONCURRENCY"),
    timeoutMs: parsePositiveInt(process.env.BENCH_TIMEOUT_MS || "30000", "BENCH_TIMEOUT_MS"),
    scenario: process.env.BENCH_SCENARIO || null,
    outDir: process.env.BENCH_OUT || path.join(REPO_ROOT, "benchmarks", "results"),
    writeFile: true,
    jsonStdout: false,
    serverUrl: process.env.BENCH_SERVER_URL || null,
    token: process.env.FLUXWRIGHT_SERVER_TOKEN || null,
    bin: process.env.FLUXWRIGHT_BIN || null,
    maxBrowsers: process.env.BENCH_BROWSERS
      ? parsePositiveInt(process.env.BENCH_BROWSERS, "BENCH_BROWSERS")
      : null,
    maxContexts: parsePositiveInt(process.env.BENCH_CONTEXTS || "8", "BENCH_CONTEXTS"),
    lowMemory: process.env.BENCH_LOW_MEMORY === "1",
    recycleRssMb: process.env.BENCH_RECYCLE_RSS_MB
      ? parsePositiveInt(process.env.BENCH_RECYCLE_RSS_MB, "BENCH_RECYCLE_RSS_MB")
      : null,
    fastCdpEndpoint: process.env.BENCH_FAST_CDP_ENDPOINT || null,
    help: false,
  };

  for (let i = 0; i < argv.length; i++) {
    const arg = argv[i];
    const next = () => {
      if (i + 1 >= argv.length) throw new Error(`missing value for ${arg}`);
      return argv[++i];
    };
    if (arg === "-h" || arg === "--help") opts.help = true;
    else if (arg === "--jobs") opts.jobs = parsePositiveInt(next(), arg);
    else if (arg === "--concurrency") opts.concurrency = parsePositiveInt(next(), arg);
    else if (arg === "--timeout-ms") opts.timeoutMs = parsePositiveInt(next(), arg);
    else if (arg === "--scenario") opts.scenario = next();
    else if (arg === "--out") opts.outDir = path.resolve(next());
    else if (arg === "--no-write") opts.writeFile = false;
    else if (arg === "--json") opts.jsonStdout = true;
    else if (arg === "--server-url") opts.serverUrl = next().replace(/\/+$/, "");
    else if (arg === "--token") opts.token = next();
    else if (arg === "--bin") opts.bin = next();
    else if (arg === "--max-browsers") opts.maxBrowsers = parsePositiveInt(next(), arg);
    else if (arg === "--max-contexts") opts.maxContexts = parsePositiveInt(next(), arg);
    else if (arg === "--low-memory") opts.lowMemory = true;
    else if (arg === "--recycle-rss-mb") opts.recycleRssMb = parsePositiveInt(next(), arg);
    else if (arg === "--fast-cdp-endpoint") opts.fastCdpEndpoint = next();
    else throw new Error(`unknown argument: ${arg}`);
  }

  return opts;
}

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
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
    if (fs.existsSync(candidate)) {
      return candidate;
    }
  }
  const pathEnv = process.env.PATH || "";
  for (const dir of pathEnv.split(path.delimiter)) {
    if (!dir) continue;
    const candidate = path.join(dir, `fluxwright${ext}`);
    if (fs.existsSync(candidate)) {
      return candidate;
    }
  }
  throw new Error(
    `Could not find compiled fluxwright binary (checked ${candidates.join(", ")}; pass --bin, set FLUXWRIGHT_BIN, or pass --server-url)`,
  );
}

async function startFixtureServer() {
  const staticHtml = `<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <title>Fluxwright Static Benchmark</title>
  <link rel="stylesheet" href="/assets/style.css">
</head>
<body>
  <main>
    <article>
      <h1>Product Catalog Overview</h1>
      <p>Fluxwright static HTML benchmark page with sufficient readable content for Tier 1 HTTP extraction and markdown formatting without requiring client-side hydration.</p>
      <p>Each static page includes headings, structured specifications, navigation links, stylesheet references, webfont rules, and media assets to measure browser asset-blocking versus HTTP fast path.</p>
      <ul>
        <li>Tier 1 HTTP fast path extracts HTML, text, markdown, and links in pure Rust.</li>
        <li>Browser mode with scraping preset blocks images, fonts, and media.</li>
        <li>Browser mode without preset loads all static subresources.</li>
      </ul>
      <table>
        <thead><tr><th>Tier</th><th>Engine</th><th>Preset</th></tr></thead>
        <tbody>
          <tr><td>Tier 1</td><td>http</td><td>n/a</td></tr>
          <tr><td>Tier 2</td><td>browser</td><td>scraping</td></tr>
        </tbody>
      </table>
      <img src="/assets/hero.png" alt="hero">
      <img src="/assets/banner.jpg" alt="banner">
      <img src="/assets/pixel.png" alt="pixel">
      <nav>
        <a href="/static/docs">Documentation</a>
        <a href="/static/pricing">Pricing</a>
        <a href="/static/guides">Guides</a>
        <a href="/static/guides">Duplicate Guides</a>
        <a href="https://external.example/reference">External Reference</a>
      </nav>
    </article>
  </main>
</body>
</html>`;

  const spaHtml = `<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <title>Loading...</title>
  <link rel="stylesheet" href="/assets/style.css">
</head>
<body>
  <div id="root"></div>
  <noscript>You need to enable JavaScript to run this app.</noscript>
  <script src="/assets/spa.js"></script>
</body>
</html>`;

  const spaJs = `(() => {
  document.title = "SPA Product Catalog";
  const root = document.getElementById("root");
  if (!root) return;
  root.innerHTML = [
    "<main><article>",
    "<h1>SPA Product Catalog</h1>",
    "<p>Hydrated client-side JavaScript application content rendered after script execution in a pooled browser context.</p>",
    "<p>Tier 1 auto mode detects the unhydrated #root shell and escalates cleanly to browser execution.</p>",
    '<img src="/assets/hero.png" alt="spa-hero">',
    "<nav>",
    '<a href="/spa/docs">SPA Docs</a>',
    '<a href="/spa/pricing">SPA Pricing</a>',
    '<a href="/spa/guides">SPA Guides</a>',
    "</nav>",
    "</article></main>",
  ].join("");
})();`;

  const styleCss = [
    "@font-face { font-family: 'BenchFont'; src: url('/assets/font.woff2') format('woff2'); }",
    "body { font-family: 'BenchFont', sans-serif; margin: 0; padding: 16px; }",
    "article { max-width: 720px; border: 1px solid #ddd; padding: 16px; }",
  ].join("\n");

  const assetHits = { css: 0, hero: 0, banner: 0, font: 0, spaJs: 0, sitemap: 0 };

  let baseUrl = "";
  const server = http.createServer((req, res) => {
    const pathname = new URL(req.url || "/", "http://127.0.0.1").pathname;
    if (pathname === "/static" || pathname.startsWith("/static/")) {
      res.writeHead(200, {
        "Content-Type": "text/html; charset=utf-8",
        "Content-Length": Buffer.byteLength(staticHtml),
        "Cache-Control": "no-store",
      });
      res.end(staticHtml);
      return;
    }
    if (pathname === "/spa" || pathname.startsWith("/spa/")) {
      res.writeHead(200, {
        "Content-Type": "text/html; charset=utf-8",
        "Content-Length": Buffer.byteLength(spaHtml),
        "Cache-Control": "no-store",
      });
      res.end(spaHtml);
      return;
    }
    if (pathname === "/assets/spa.js") {
      assetHits.spaJs++;
      res.writeHead(200, {
        "Content-Type": "application/javascript; charset=utf-8",
        "Content-Length": Buffer.byteLength(spaJs),
        "Cache-Control": "no-store",
      });
      res.end(spaJs);
      return;
    }
    if (pathname === "/assets/style.css") {
      assetHits.css++;
      res.writeHead(200, {
        "Content-Type": "text/css; charset=utf-8",
        "Content-Length": Buffer.byteLength(styleCss),
        "Cache-Control": "no-store",
      });
      res.end(styleCss);
      return;
    }
    if (pathname === "/assets/hero.png") {
      assetHits.hero++;
      res.writeHead(200, {
        "Content-Type": "image/png",
        "Content-Length": HERO_PNG.length,
        "Cache-Control": "no-store",
      });
      res.end(HERO_PNG);
      return;
    }
    if (pathname === "/assets/banner.jpg") {
      assetHits.banner++;
      res.writeHead(200, {
        "Content-Type": "image/jpeg",
        "Content-Length": BANNER_JPG.length,
        "Cache-Control": "no-store",
      });
      res.end(BANNER_JPG);
      return;
    }
    if (pathname === "/assets/pixel.png") {
      res.writeHead(200, {
        "Content-Type": "image/png",
        "Content-Length": ONE_BY_ONE_PNG.length,
        "Cache-Control": "no-store",
      });
      res.end(ONE_BY_ONE_PNG);
      return;
    }
    if (pathname === "/assets/font.woff2") {
      assetHits.font++;
      res.writeHead(200, {
        "Content-Type": "font/woff2",
        "Content-Length": FONT_WOFF2.length,
        "Cache-Control": "no-store",
      });
      res.end(FONT_WOFF2);
      return;
    }
    if (pathname === "/sitemap.xml") {
      assetHits.sitemap++;
      const xml = [
        '<?xml version="1.0" encoding="UTF-8"?>',
        '<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">',
        `  <url><loc>${baseUrl}/static</loc></url>`,
        `  <url><loc>${baseUrl}/static/docs</loc></url>`,
        `  <url><loc>${baseUrl}/static/sitemap-only</loc></url>`,
        "</urlset>",
      ].join("\n");
      res.writeHead(200, {
        "Content-Type": "application/xml; charset=utf-8",
        "Content-Length": Buffer.byteLength(xml),
        "Cache-Control": "no-store",
      });
      res.end(xml);
      return;
    }
    res.writeHead(404, { "Content-Type": "text/plain" });
    res.end("not found");
  });

  await new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", () => resolve());
  });
  const addr = server.address();
  baseUrl = `http://127.0.0.1:${addr.port}`;
  return {
    baseUrl,
    assetHits,
    close: () => new Promise((resolve) => server.close(() => resolve())),
  };
}

async function spawnFluxwrightServer(opts, token) {
  const bin = resolveFluxwrightBin(opts.bin);
  const port = await getFreePort();
  const listen = `127.0.0.1:${port}`;
  const args = [
    "serve",
    "--listen",
    listen,
    "--max-contexts",
    String(opts.maxContexts),
    "--max-jobs",
    String(Math.max(128, opts.concurrency * 4)),
  ];
  if (opts.maxBrowsers !== null) {
    args.push("--max-browsers", String(opts.maxBrowsers));
  }
  if (opts.lowMemory) {
    args.push("--low-memory");
  }
  if (opts.recycleRssMb !== null) {
    args.push("--recycle-rss-mb", String(opts.recycleRssMb));
  }
  if (opts.fastCdpEndpoint) {
    args.push("--fast-cdp-endpoint", opts.fastCdpEndpoint);
  }

  const stderrLines = [];
  const child = spawn(bin, args, {
    cwd: REPO_ROOT,
    env: {
      ...process.env,
      FLUXWRIGHT_SERVER_TOKEN: token,
      FLUXWRIGHT_SERVER_ANY_URL: "1",
    },
    stdio: ["ignore", "ignore", "pipe"],
  });
  child.stderr.on("data", (chunk) => {
    const text = chunk.toString("utf8");
    for (const line of text.split(/\r?\n/)) {
      if (line.trim()) {
        stderrLines.push(line);
        if (stderrLines.length > 40) stderrLines.shift();
      }
    }
  });

  const serverUrl = `http://${listen}`;
  await waitForServer(serverUrl, child, stderrLines);
  return {
    serverUrl,
    stop: async () => {
      if (child.exitCode !== null || child.signalCode !== null) return;
      child.kill("SIGTERM");
      const deadline = Date.now() + 8_000;
      while (child.exitCode === null && child.signalCode === null && Date.now() < deadline) {
        await sleep(100);
      }
      if (child.exitCode === null && child.signalCode === null) {
        child.kill("SIGKILL");
      }
    },
  };
}

async function waitForServer(serverUrl, child, stderrLines) {
  const deadline = Date.now() + 20_000;
  while (Date.now() < deadline) {
    if (child && (child.exitCode !== null || child.signalCode !== null)) {
      throw new Error(
        `fluxwright serve exited early (code=${child.exitCode}, signal=${child.signalCode}):\n${stderrLines.join("\n")}`,
      );
    }
    try {
      const res = await fetch(`${serverUrl}/healthz`);
      if (res.ok) return;
    } catch {
      // retry until ready
    }
    await sleep(100);
  }
  throw new Error(`timed out waiting for ${serverUrl}/healthz\n${stderrLines.join("\n")}`);
}

async function apiJson(serverUrl, token, method, pathname, body) {
  const headers = { Authorization: `Bearer ${token}` };
  const init = { method, headers };
  if (body !== undefined) {
    headers["Content-Type"] = "application/json";
    init.body = JSON.stringify(body);
  }
  const res = await fetch(`${serverUrl}${pathname}`, init);
  const text = await res.text();
  let json = null;
  try {
    json = JSON.parse(text);
  } catch {
    json = null;
  }
  return { ok: res.ok, status: res.status, json, text };
}

async function fetchStats(serverUrl, token) {
  const res = await apiJson(serverUrl, token, "GET", "/v1/stats");
  if (!res.ok || !res.json) {
    return null;
  }
  return {
    engine_rss_bytes: Number(res.json.engine_rss_bytes || 0),
    browser_tree_rss_bytes: Number(res.json.browser_tree_rss_bytes || 0),
    browsers: Number(res.json.browsers || 0),
    contexts: Number(res.json.contexts || 0),
  };
}

function parsePromCounter(text, metricName) {
  const re = new RegExp(`^${metricName}(?:\\{[^}]*\\})?\\s+([0-9]+(?:\\.[0-9]+)?)$`, "gm");
  let sum = 0;
  for (const match of text.matchAll(re)) {
    sum += Number(match[1]);
  }
  return sum;
}

async function fetchFastpathMetrics(serverUrl, token) {
  const res = await fetch(`${serverUrl}/metrics`, {
    headers: { Authorization: `Bearer ${token}` },
  });
  if (!res.ok) {
    return { hits: 0, fallbacks: 0 };
  }
  const text = await res.text();
  return {
    hits: parsePromCounter(text, "fluxwright_fastpath_hits_total"),
    fallbacks: parsePromCounter(text, "fluxwright_fastpath_fallbacks_total"),
  };
}

function percentile(sorted, p) {
  if (sorted.length === 0) return 0;
  const idx = Math.min(sorted.length - 1, Math.floor((sorted.length - 1) * p));
  return sorted[idx];
}

function buildScenarios(baseUrl, timeoutMs) {
  const staticUrl = `${baseUrl}/static`;
  const spaUrl = `${baseUrl}/spa`;

  return [
    // Run pure static Tier 1 HTTP fast-path scenarios before launching any browser
    // so server_rss_mb reflects baseline process RSS prior to browser pool warmup.
    {
      name: "scrape-static-auto",
      endpoint: "/v1/scrape",
      pageKind: "static",
      mode: "auto",
      resourcePreset: "n/a",
      makeBody: () => ({
        url: staticUrl,
        formats: ["markdown"],
        mode: "auto",
        timeout_ms: timeoutMs,
      }),
      validate: (data) => typeof data.markdown === "string" && data.markdown.includes("Product Catalog Overview"),
    },
    {
      name: "map-static-auto",
      endpoint: "/v1/map",
      pageKind: "static",
      mode: "auto",
      resourcePreset: "n/a",
      makeBody: () => ({
        url: staticUrl,
        mode: "auto",
        same_origin: true,
        timeout_ms: timeoutMs,
      }),
      validate: (data) => Array.isArray(data.links) && data.links.some((u) => u.endsWith("/static/docs")),
    },
    {
      name: "scrape-mixed-auto",
      endpoint: "/v1/scrape",
      pageKind: "80%static/20%spa",
      mode: "auto",
      resourcePreset: "default",
      makeBody: (i) => ({
        url: i % 5 === 0 ? spaUrl : staticUrl,
        formats: ["markdown"],
        mode: "auto",
        timeout_ms: timeoutMs,
      }),
      validate: (data) =>
        typeof data.markdown === "string" &&
        (data.markdown.includes("Product Catalog Overview") || data.markdown.includes("SPA Product Catalog")),
    },
    {
      name: "map-mixed-auto",
      endpoint: "/v1/map",
      pageKind: "80%static/20%spa",
      mode: "auto",
      resourcePreset: "default",
      makeBody: (i) => ({
        url: i % 5 === 0 ? spaUrl : staticUrl,
        mode: "auto",
        same_origin: true,
        timeout_ms: timeoutMs,
      }),
      validate: (data) => Array.isArray(data.links) && data.links.length > 0,
    },
    {
      name: "scrape-spa-auto",
      endpoint: "/v1/scrape",
      pageKind: "spa",
      mode: "auto",
      resourcePreset: "default",
      makeBody: () => ({
        url: spaUrl,
        formats: ["markdown"],
        mode: "auto",
        timeout_ms: timeoutMs,
      }),
      validate: (data) => typeof data.markdown === "string" && data.markdown.includes("SPA Product Catalog"),
    },
    {
      name: "map-spa-auto",
      endpoint: "/v1/map",
      pageKind: "spa",
      mode: "auto",
      resourcePreset: "default",
      makeBody: () => ({
        url: spaUrl,
        mode: "auto",
        same_origin: true,
        timeout_ms: timeoutMs,
      }),
      validate: (data) => Array.isArray(data.links) && data.links.some((u) => u.endsWith("/spa/docs")),
    },
    {
      name: "scrape-static-browser-scraping",
      endpoint: "/v1/scrape",
      pageKind: "static",
      mode: "browser",
      resourcePreset: "scraping",
      makeBody: () => ({
        url: staticUrl,
        formats: ["markdown"],
        mode: "browser",
        resource_preset: "scraping",
        timeout_ms: timeoutMs,
      }),
      validate: (data) => typeof data.markdown === "string" && data.markdown.includes("Product Catalog Overview"),
    },
    {
      name: "scrape-static-browser-no-preset",
      endpoint: "/v1/scrape",
      pageKind: "static",
      mode: "browser",
      resourcePreset: "omitted",
      makeBody: () => ({
        url: staticUrl,
        formats: ["markdown"],
        mode: "browser",
        timeout_ms: timeoutMs,
      }),
      validate: (data) => typeof data.markdown === "string" && data.markdown.includes("Product Catalog Overview"),
    },
    {
      name: "scrape-spa-browser-scraping",
      endpoint: "/v1/scrape",
      pageKind: "spa",
      mode: "browser",
      resourcePreset: "scraping",
      makeBody: () => ({
        url: spaUrl,
        formats: ["markdown"],
        mode: "browser",
        resource_preset: "scraping",
        timeout_ms: timeoutMs,
      }),
      validate: (data) => typeof data.markdown === "string" && data.markdown.includes("SPA Product Catalog"),
    },
    {
      name: "scrape-spa-browser-no-preset",
      endpoint: "/v1/scrape",
      pageKind: "spa",
      mode: "browser",
      resourcePreset: "omitted",
      makeBody: () => ({
        url: spaUrl,
        formats: ["markdown"],
        mode: "browser",
        timeout_ms: timeoutMs,
      }),
      validate: (data) => typeof data.markdown === "string" && data.markdown.includes("SPA Product Catalog"),
    },
    {
      name: "map-static-browser-scraping",
      endpoint: "/v1/map",
      pageKind: "static",
      mode: "browser",
      resourcePreset: "scraping",
      makeBody: () => ({
        url: staticUrl,
        mode: "browser",
        resource_preset: "scraping",
        same_origin: true,
        timeout_ms: timeoutMs,
      }),
      validate: (data) => Array.isArray(data.links) && data.links.some((u) => u.endsWith("/static/docs")),
    },
    {
      name: "map-static-browser-no-preset",
      endpoint: "/v1/map",
      pageKind: "static",
      mode: "browser",
      resourcePreset: "omitted",
      makeBody: () => ({
        url: staticUrl,
        mode: "browser",
        same_origin: true,
        timeout_ms: timeoutMs,
      }),
      validate: (data) => Array.isArray(data.links) && data.links.some((u) => u.endsWith("/static/docs")),
    },
    {
      name: "map-spa-browser-scraping",
      endpoint: "/v1/map",
      pageKind: "spa",
      mode: "browser",
      resourcePreset: "scraping",
      makeBody: () => ({
        url: spaUrl,
        mode: "browser",
        resource_preset: "scraping",
        same_origin: true,
        timeout_ms: timeoutMs,
      }),
      validate: (data) => Array.isArray(data.links) && data.links.some((u) => u.endsWith("/spa/docs")),
    },
    {
      name: "map-spa-browser-no-preset",
      endpoint: "/v1/map",
      pageKind: "spa",
      mode: "browser",
      resourcePreset: "omitted",
      makeBody: () => ({
        url: spaUrl,
        mode: "browser",
        same_origin: true,
        timeout_ms: timeoutMs,
      }),
      validate: (data) => Array.isArray(data.links) && data.links.some((u) => u.endsWith("/spa/docs")),
    },
  ];
}

async function runScenario(sc, opts, serverUrl, token) {
  // Single warmup request before timing so cold browser startup is isolated from steady state.
  await apiJson(serverUrl, token, "POST", sc.endpoint, sc.makeBody(0)).catch(() => {});

  const metricsBefore = await fetchFastpathMetrics(serverUrl, token);
  const initialStats = await fetchStats(serverUrl, token);
  let peakEngineBytes = initialStats ? initialStats.engine_rss_bytes : 0;
  let peakBrowserBytes = initialStats ? initialStats.browser_tree_rss_bytes : 0;
  let peakTotalBytes = peakEngineBytes + peakBrowserBytes;

  let polling = true;
  const pollLoop = (async () => {
    while (polling) {
      const snap = await fetchStats(serverUrl, token).catch(() => null);
      if (snap) {
        const total = snap.engine_rss_bytes + snap.browser_tree_rss_bytes;
        if (snap.engine_rss_bytes > peakEngineBytes) peakEngineBytes = snap.engine_rss_bytes;
        if (snap.browser_tree_rss_bytes > peakBrowserBytes) peakBrowserBytes = snap.browser_tree_rss_bytes;
        if (total > peakTotalBytes) peakTotalBytes = total;
      }
      await sleep(250);
    }
  })();

  const latencies = [];
  const engines = { http: 0, browser: 0, unknown: 0 };
  const fallbackReasons = {};
  let failures = 0;
  let nextIdx = 0;
  const workerCount = Math.min(opts.jobs, opts.concurrency);
  const tStart = performance.now();

  const workers = Array.from({ length: workerCount }, async () => {
    for (;;) {
      const idx = nextIdx++;
      if (idx >= opts.jobs) return;
      const reqStart = performance.now();
      try {
        const res = await apiJson(serverUrl, token, "POST", sc.endpoint, sc.makeBody(idx));
        const elapsed = performance.now() - reqStart;
        latencies.push(elapsed);
        if (!res.ok || !res.json || !sc.validate(res.json)) {
          failures++;
          continue;
        }
        const eng = res.json.engine;
        if (eng === "http") engines.http++;
        else if (eng === "browser") engines.browser++;
        else engines.unknown++;

        if (res.json.fallback_reason) {
          const reason = String(res.json.fallback_reason);
          fallbackReasons[reason] = (fallbackReasons[reason] || 0) + 1;
        }
      } catch {
        latencies.push(performance.now() - reqStart);
        failures++;
      }
    }
  });

  await Promise.all(workers);
  const durationMs = Math.max(performance.now() - tStart, 0.001);
  polling = false;
  await pollLoop;

  const endStats = await fetchStats(serverUrl, token).catch(() => null);
  if (endStats) {
    const total = endStats.engine_rss_bytes + endStats.browser_tree_rss_bytes;
    if (endStats.engine_rss_bytes > peakEngineBytes) peakEngineBytes = endStats.engine_rss_bytes;
    if (endStats.browser_tree_rss_bytes > peakBrowserBytes) peakBrowserBytes = endStats.browser_tree_rss_bytes;
    if (total > peakTotalBytes) peakTotalBytes = total;
  }
  const metricsAfter = await fetchFastpathMetrics(serverUrl, token);

  latencies.sort((a, b) => a - b);
  const p50Ms = Number(percentile(latencies, 0.5).toFixed(2));
  const p95Ms = Number(percentile(latencies, 0.95).toFixed(2));
  const reqPerSec = Number((opts.jobs / (durationMs / 1000)).toFixed(2));

  return {
    scenario: sc.name,
    endpoint: sc.endpoint,
    page_kind: sc.pageKind,
    mode: sc.mode,
    resource_preset: sc.resourcePreset,
    requests: opts.jobs,
    concurrency: workerCount,
    duration_ms: Math.round(durationMs),
    req_per_sec: reqPerSec,
    p50_ms: p50Ms,
    p95_ms: p95Ms,
    engine: engines,
    fallback_reasons: fallbackReasons,
    fastpath_hits_delta: Math.max(0, metricsAfter.hits - metricsBefore.hits),
    fastpath_fallbacks_delta: Math.max(0, metricsAfter.fallbacks - metricsBefore.fallbacks),
    server_rss_mb: Math.round(peakTotalBytes / 1048576),
    engine_rss_mb: Math.round(peakEngineBytes / 1048576),
    browser_tree_rss_mb: Math.round(peakBrowserBytes / 1048576),
    failure_rate: Number((failures / opts.jobs).toFixed(4)),
    failures,
  };
}

function padRight(val, width) {
  const s = String(val);
  return s.length >= width ? s : s + " ".repeat(width - s.length);
}

function padLeft(val, width) {
  const s = String(val);
  return s.length >= width ? s : " ".repeat(width - s.length) + s;
}

function printTable(results, assetHits) {
  const header = [
    padRight("scenario", 32),
    padRight("endpoint", 11),
    padRight("mode", 8),
    padRight("preset", 9),
    padLeft("reqs", 5),
    padLeft("req/s", 9),
    padLeft("p50 ms", 8),
    padLeft("p95 ms", 8),
    padLeft("http", 5),
    padLeft("browser", 8),
    padLeft("rss MB", 7),
    padLeft("fail %", 7),
  ].join(" ");
  process.stdout.write(`\n${header}\n`);
  for (const r of results) {
    const row = [
      padRight(r.scenario, 32),
      padRight(r.endpoint, 11),
      padRight(r.mode, 8),
      padRight(r.resource_preset, 9),
      padLeft(r.requests, 5),
      padLeft(r.req_per_sec.toFixed(2), 9),
      padLeft(r.p50_ms.toFixed(1), 8),
      padLeft(r.p95_ms.toFixed(1), 8),
      padLeft(r.engine.http, 5),
      padLeft(r.engine.browser, 8),
      padLeft(r.server_rss_mb, 7),
      padLeft((r.failure_rate * 100).toFixed(1), 7),
    ].join(" ");
    process.stdout.write(`${row}\n`);
  }
  process.stdout.write(
    `\nFixture asset requests served: css=${assetHits.css} hero_png=${assetHits.hero} banner_jpg=${assetHits.banner} font_woff2=${assetHits.font} spa_js=${assetHits.spaJs}\n`,
  );
  const reasons = new Map();
  let totalHits = 0;
  let totalFallbacks = 0;
  for (const r of results) {
    totalHits += r.fastpath_hits_delta;
    totalFallbacks += r.fastpath_fallbacks_delta;
    for (const [k, v] of Object.entries(r.fallback_reasons || {})) {
      reasons.set(k, (reasons.get(k) || 0) + v);
    }
  }
  const reasonStr = reasons.size
    ? Array.from(reasons.entries())
        .map(([k, v]) => `${k}=${v}`)
        .join(", ")
    : "none";
  process.stdout.write(
    `Metrics deltas: fastpath_hits=${totalHits} fastpath_fallbacks=${totalFallbacks}; fallback_reasons: ${reasonStr}\n`,
  );
  process.stdout.write(
    "Note: non-visual /v1/scrape and /v1/map browser requests with preset=omitted default to resource_preset=\"scraping\" on the server.\n",
  );
}

async function main() {
  const opts = parseArgs(process.argv.slice(2));
  if (opts.help) {
    printUsage();
    return;
  }

  const token = opts.token || crypto.randomBytes(32).toString("hex");
  const fixture = await startFixtureServer();
  let spawned = null;

  const cleanup = async () => {
    if (spawned) {
      await spawned.stop().catch(() => {});
      spawned = null;
    }
    await fixture.close().catch(() => {});
  };

  const onSignal = () => {
    cleanup().finally(() => process.exit(130));
  };
  process.once("SIGINT", onSignal);
  process.once("SIGTERM", onSignal);

  try {
    let serverUrl = opts.serverUrl;
    if (!serverUrl) {
      spawned = await spawnFluxwrightServer(opts, token);
      serverUrl = spawned.serverUrl;
    } else {
      await waitForServer(serverUrl, null, []);
    }

    const scenarios = buildScenarios(fixture.baseUrl, opts.timeoutMs).filter(
      (sc) => !opts.scenario || sc.name === opts.scenario,
    );
    if (scenarios.length === 0) {
      throw new Error(`no scenario matched: ${opts.scenario}`);
    }

    process.stderr.write(
      `fastpath-bench server=${serverUrl} fixture=${fixture.baseUrl} jobs=${opts.jobs} concurrency=${opts.concurrency} os=${os.platform()} arch=${os.arch()}\n`,
    );

    const results = [];
    for (const sc of scenarios) {
      process.stderr.write(`  running ${sc.name} (${sc.endpoint} ${sc.pageKind} mode=${sc.mode})\n`);
      const rec = await runScenario(sc, opts, serverUrl, token);
      results.push(rec);
    }

    const report = {
      generated_at: new Date().toISOString(),
      os: os.platform(),
      arch: os.arch(),
      node_version: process.version,
      jobs_per_scenario: opts.jobs,
      concurrency: opts.concurrency,
      low_memory: opts.lowMemory,
      recycle_rss_mb: opts.recycleRssMb,
      fast_cdp_endpoint: opts.fastCdpEndpoint ? "(configured)" : null,
      fixture_asset_hits: fixture.assetHits,
      scenarios: results,
    };

    if (opts.writeFile) {
      fs.mkdirSync(opts.outDir, { recursive: true });
      const outPath = path.join(opts.outDir, "fastpath-latest.json");
      fs.writeFileSync(outPath, JSON.stringify(report, null, 2) + "\n", "utf8");
      if (!opts.jsonStdout) {
        process.stderr.write(`wrote ${outPath}\n`);
      }
    }

    if (opts.jsonStdout) {
      process.stdout.write(JSON.stringify(report, null, 2) + "\n");
    } else {
      printTable(results, fixture.assetHits);
    }

    const totalFailures = results.reduce((sum, r) => sum + r.failures, 0);
    if (totalFailures > 0) {
      process.exitCode = 1;
    }
  } finally {
    process.removeListener("SIGINT", onSignal);
    process.removeListener("SIGTERM", onSignal);
    await cleanup();
  }
}

main().catch((err) => {
  process.stderr.write(`${err?.stack || err}\n`);
  process.exit(1);
});
