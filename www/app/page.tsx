import type { CSSProperties } from "react";
import { CopyCommand } from "./_components/CopyCommand";
import { FleetBoard } from "./_components/FleetBoard";
import { SiteHeader } from "./_components/SiteHeader";
import { GITHUB, Logo, NPM, PYPI, RELEASES, SECTIONS } from "./_components/site";
import { Tabs } from "./_components/Tabs";

const FEATURES: { name: string; text: string; api: string }[] = [
  {
    name: "A fresh page per job",
    text: "Every job gets its own browser context on a pooled Chrome. Cookies, storage and cache never leak into the next job.",
    api: "browser.newPage()",
  },
  {
    name: "A queue with priorities",
    text: "When every slot is busy, jobs wait in a priority queue, or fail fast if you would rather shed load.",
    api: "QueueFullMode::Wait",
  },
  {
    name: "Container-aware memory ceiling",
    text: "Auto-sizes to 1 browser per 4 CPUs and 75% of free RAM (reading cgroup limits in Docker). No new work starts above budget.",
    api: "memory_ceiling_mb",
  },
  {
    name: "Recycling",
    text: "A browser drains running jobs without pausing the pool after a job count, age, or memory threshold, and shuts down when idle.",
    api: "recycle_after_jobs",
  },
  {
    name: "Crash recovery",
    text: "A dead browser fails its jobs at once and they retry on a healthy one. Chrome is tied to the engine, so it never outlives a crash.",
    api: "JobOptions::retries",
  },
  {
    name: "Full Playwright-style API",
    text: "Locators, cross-origin frameLocator, page.clock, route({ times }) & route.fallback(overrides), screenshot masking, page.pdf(), HAR 1.2 record/replay, prewarmContexts, ResourcePreset, multi-tab contexts, APIRequestContext, and newCDPSession.",
    api: "page.getByRole()",
  },
  {
    name: "End-to-end testing built in",
    text: "Auto-retrying web-first expect() assertions (Rust, Node/TS, Python), Playwright Trace Viewer-compatible .zip context tracing with DOM frame snapshots, page video recording, visual toHaveScreenshot() regression, open Shadow DOM CSS piercing, the fluxwright/test CLI runner (parallel workers, sharding, retries, HTML reports), and an auto-loaded pytest plugin.",
    api: "fluxwright/test",
  },
  {
    name: "Profiles, proxies & AIMD",
    text: "Named persistent profiles, rotating proxy pools with sticky affinity and exponential cooldowns, per-host rate limits, and AIMD adaptive concurrency under memory pressure.",
    api: "--profiles-dir",
  },
  {
    name: "Pooled CDP gateway",
    text: "Connect existing Playwright, Puppeteer, or Chrome DevTools scripts straight to Fluxwright's pooled Chrome fleet with context isolation and /json/version discovery.",
    api: "GET /cdp",
  },
  {
    name: "Live screencast & HITL",
    text: "Watch live JPEG streams in /dashboard, generate signed read-only or interactive embeddable Live View URLs, pause sessions with pause_for_human, drive mouse/keyboard/navigation remotely, and resume cleanly.",
    api: "/v1/live/{id}/embed",
  },
  {
    name: "HTTP server & cluster mode",
    text: "Bearer-authenticated job API, signed HMAC-SHA256 webhooks, OTLP/HTTP JSON trace export, Prometheus /metrics, W3C traceparent spans, --journal and SQLite --store persistence, Docker image, and pull-based fluxwright coordinate clusters.",
    api: "fluxwright serve",
  },
  {
    name: "Built for AI agents",
    text: "21-tool MCP server over stdio or Streamable HTTP (--http) with per-session isolation, diff-only @eN ARIA snapshots, Set-of-Marks annotated screenshots, and clean Markdown extraction.",
    api: "fluxwright-mcp --http",
  },
];

const STEPS: { title: string; text: string }[] = [
  { title: "Submit", text: "Your code asks for a page with browser.newPage(), or hands a job to engine.run()." },
  {
    title: "Admit",
    text: "If every slot is busy or memory is over the ceiling, the job waits in the queue by priority.",
  },
  { title: "Lease", text: "The job gets a fresh context on the least busy Chrome. Nothing is shared with other jobs." },
  { title: "Run", text: "Your code drives the page: navigation, locators, evaluate, screenshots." },
  {
    title: "Release",
    text: "Closing the page destroys the context. A browser that reached its limits drains and restarts.",
  },
];

// GitHub Actions benchmarks run 45227204313 on ubuntu-latest (4 vCPU, chrome-headless-shell 158.0.8127.36), 23 November 2026.
const BENCH: { setup: string; flux: number; pw: number }[] = [
  { setup: "1 browser, 10 pages (100 jobs)", flux: 42.8, pw: 36.7 },
  { setup: "10 browsers, 2 pages each (40 jobs)", flux: 50.7, pw: 34.6 },
  { setup: "500 concurrent (10 browsers × 20 pages)", flux: 48.5, pw: 36.7 },
  { setup: "1,000 queued (4 browsers × 8 pages)", flux: 47.3, pw: 41.7 },
  { setup: "100 PDFs (8 browsers × 2 pages)", flux: 52.0, pw: 41.6 },
  { setup: "200 scraping jobs (images + fonts)", flux: 38.2, pw: 31.0 },
  { setup: "1,000 chaos jobs (browser kill / 2s)", flux: 49.2, pw: 23.2 },
];
const BENCH_MAX = 55;

const TS_EXAMPLE = `import { chromium } from "fluxwright";

const browser = await chromium.launch({ maxBrowsers: 4 });

// A fresh browser context on a pooled Chrome.
const page = await browser.newPage();
await page.goto("https://example.com", { waitUntil: "domcontentloaded" });
console.log(await page.title());

// Locators wait until the element can be used.
await page.getByRole("button", { name: "Accept" }).click();
await page.getByRole("textbox", { name: "Search" }).fill("chromium");

// Cross-origin iframes work the same way.
const payment = page.frameLocator("iframe#payment");
const card = payment.getByRole("textbox", { name: "Card number" });
await card.fill("4242 4242 4242 4242");

// AI-native accessibility tree (@eN refs) and clean Markdown.
const snapshot = await page.ariaSnapshot();
const markdown = await page.markdown();

await page.close(); // the context is destroyed and the slot is free
await browser.close();`;

const PY_EXAMPLE = `import asyncio
from fluxwright.async_api import async_playwright

async def main():
    async with async_playwright() as p:
        browser = await p.chromium.launch(max_browsers=4)

        # A fresh browser context on a pooled Chrome.
        page = await browser.new_page()
        await page.goto("https://example.com", wait_until="domcontentloaded")
        print(await page.title())

        # Locators wait until the element can be used.
        await page.get_by_role("button", name="Accept").click()
        await page.get_by_role("textbox", name="Search").fill("chromium")

        await page.close()  # the context is destroyed and the slot is free

asyncio.run(main())`;

const RUST_EXAMPLE = `use fluxwright::{BrowserEngine, JobOptions, Priority};

let engine = BrowserEngine::builder()
    .max_browsers(8)
    .max_contexts_per_browser(10)
    .memory_ceiling_mb(8192)   // admit no new jobs above this footprint
    .recycle_after_jobs(200)   // restart each Chrome after 200 jobs
    .build()
    .await?;

// run() queues, leases, retries after a crash, and always releases the page.
let job = JobOptions::default().priority(Priority::High).retries(2);
let title = engine
    .run(job, |page| async move {
        page.goto("https://example.com").await?;
        page.title().await
    })
    .await?;`;

const MCP_EXAMPLE = `{
  "mcpServers": {
    "fluxwright": { "command": "fluxwright-mcp" }
  }
}`;

const GATEWAY_EXAMPLE = `// Start the pooled server with CDP gateway, profiles, and AIMD
// $ fluxwright serve --allow-query-token --profiles-dir ./profiles --adaptive-concurrency

// Connect existing Playwright scripts straight to the pooled fleet
import { chromium } from "playwright";

const browser = await chromium.connectOverCDP(
  \`ws://127.0.0.1:7331/cdp?token=\${process.env.FLUXWRIGHT_SERVER_TOKEN}\`,
);
const page = await browser.newPage();
await page.goto("https://example.com");`;

const E2E_EXAMPLE = `import { test, expect } from "fluxwright/test";

test.use({ trace: "retain-on-failure", video: "retain-on-failure" });

test("homepage renders", async ({ page }) => {
  await page.goto("https://example.com");
  await expect(page).toHaveTitle(/Example/);
  await expect(page.getByRole("heading")).toHaveText("Example Domain");
  await expect(page).toHaveScreenshot("home.png");
});

// Run in parallel with retries, sharding, and HTML report:
// $ npx fluxwright-test --workers=4 --retries=1 --reporter=list,html`;

function Code({ children, label }: { children: string; label: string }) {
  return (
    <pre className="code" aria-label={label}>
      <code>{children}</code>
    </pre>
  );
}

function Bar({ value }: { value: number }) {
  return (
    <span className="bar" aria-hidden="true">
      <span
        className="bar-fill"
        style={{ "--from": "0%", "--to": `${(value / BENCH_MAX) * 100}%` } as CSSProperties}
      />
    </span>
  );
}

export default function Home() {
  return (
    <>
      <SiteHeader />
      <main id="main" tabIndex={-1}>
        <section className="hero container" id="top" aria-labelledby="hero-title">
          <div className="hero-text">
            <h1 id="hero-title">Run hundreds of Chrome jobs on one machine.</h1>
            <p className="lead">
              Fluxwright pools Chrome, gives every job a fresh isolated page, queues work when the
              fleet is full, restarts browsers before they bloat, and retries when one crashes. Use
              it from Node, Python, or Rust, over the HTTP server and pooled CDP gateway, or from
              AI agents via MCP.
            </p>
            <CopyCommand command="npm install fluxwright" />
            <div className="actions">
              <a className="button primary" href="#code">
                See the code
              </a>
              <a className="button" href={GITHUB}>
                View on GitHub
              </a>
            </div>
            <p className="meta">
              Version 0.11.0, open source under Apache-2.0. Prebuilt for Windows (x64 &amp; ARM64), macOS, and Linux (x64, ARM64 &amp; static musl) on npm, PyPI, and GitHub Releases.
            </p>
          </div>
          <FleetBoard />
        </section>

        <section className="section" id="features" aria-labelledby="features-title">
          <div className="container">
            <div className="section-head">
              <h2 id="features-title">What it takes care of</h2>
              <p>
                Libraries like Playwright and Puppeteer drive one browser well. Running many at once
                needs a control plane: how many Chromes, when to refuse work, when to restart one,
                what to do when a renderer dies. That is Fluxwright.
              </p>
            </div>
            <dl className="features">
              {FEATURES.map((f) => (
                <div className="feature" key={f.name}>
                  <dt>{f.name}</dt>
                  <dd>{f.text}</dd>
                  <dd className="feature-api">
                    <code>{f.api}</code>
                  </dd>
                </div>
              ))}
            </dl>
          </div>
        </section>

        <section className="section" id="how" aria-labelledby="how-title">
          <div className="container">
            <div className="section-head">
              <h2 id="how-title">How a job runs</h2>
              <p>The same five steps whether a job comes from Node, Python, Rust or an agent.</p>
            </div>
            <ol className="steps">
              {STEPS.map((s) => (
                <li key={s.title}>
                  <h3>{s.title}</h3>
                  <p>{s.text}</p>
                </li>
              ))}
            </ol>
          </div>
        </section>

        <section className="section" id="code" aria-labelledby="code-title">
          <div className="container split">
            <div className="section-head">
              <h2 id="code-title">The code</h2>
              <p>
                A small API on purpose: pages you lease, locators that wait, and an engine that
                handles the fleet. Node and Python load a native module, so there is no driver process in between.
              </p>
              <p className="note">
                The 21-tool MCP server exposes navigation and tabs (<code>open</code>,{" "}
                <code>goto</code>, <code>back</code>, <code>forward</code>, <code>tabs</code>,{" "}
                <code>close</code>), AI inspection (<code>snapshot</code> with <code>@eN</code>{" "}
                refs, <code>markdown</code>, <code>title</code>, <code>content</code>,{" "}
                <code>screenshot</code>), page actions (<code>click</code>, <code>hover</code>,{" "}
                <code>fill</code>, <code>press</code>, <code>select</code>, <code>scroll</code>,{" "}
                <code>wait_for</code>, <code>evaluate</code>), and diagnostics (
                <code>console</code>, <code>network</code>). Or connect Playwright/Puppeteer
                scripts over the pooled <code>/cdp</code> WebSocket gateway.
              </p>
            </div>
            <Tabs
              label="Code examples"
              tabs={[
                { title: "TypeScript", content: <Code label="TypeScript example">{TS_EXAMPLE}</Code> },
                { title: "Python", content: <Code label="Python example">{PY_EXAMPLE}</Code> },
                { title: "Rust", content: <Code label="Rust example">{RUST_EXAMPLE}</Code> },
                { title: "E2E Test", content: <Code label="E2E test runner example">{E2E_EXAMPLE}</Code> },
                { title: "Server / CDP", content: <Code label="Server and CDP gateway example">{GATEWAY_EXAMPLE}</Code> },
                { title: "MCP", content: <Code label="Claude Desktop configuration">{MCP_EXAMPLE}</Code> },
              ]}
            />
          </div>
        </section>

        <section className="section" id="benchmarks" aria-labelledby="bench-title">
          <div className="container">
            <div className="section-head">
              <h2 id="bench-title">Benchmarks</h2>
              <p>
                Jobs per second, higher is better. Every tool ran on the same chrome-headless-shell
                binary and loaded a small local page with fresh isolated contexts per job.
              </p>
            </div>
            <table className="bench">
              <caption className="visually-hidden">Throughput in jobs per second</caption>
              <thead>
                <tr>
                  <th scope="col">Setup</th>
                  <th scope="col">Fluxwright</th>
                  <th scope="col">Playwright</th>
                </tr>
              </thead>
              <tbody>
                {BENCH.map((b) => (
                  <tr key={b.setup}>
                    <th scope="row">{b.setup}</th>
                    <td>
                      <span className="bench-value">{b.flux.toFixed(1)}</span>
                      <Bar value={b.flux} />
                    </td>
                    <td className="bench-other">
                      <span className="bench-value">{b.pw.toFixed(1)}</span>
                      <Bar value={b.pw} />
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
            <p className="note bench-note">
              Measured on GitHub&apos;s ubuntu-latest runner (4 vCPU, 23 November 2026,
              chrome-headless-shell 158.0.8127.36). Fluxwright beat Playwright and Puppeteer in
              all 8 scenarios with 0.0% failures and 0 leaked processes; in chaos mode Playwright
              and Puppeteer dropped 69.9% of jobs. A shared cloud runner varies from run to
              run. Run it yourself from the repository&apos;s Actions tab (benchmarks), and measure
              your own workload with <code>cargo run --release -p fluxwright-benchmarks</code>.
            </p>
          </div>
        </section>

        <section className="section" id="install" aria-labelledby="install-title">
          <div className="container split">
            <div className="section-head">
              <h2 id="install-title">Install</h2>
              <p>
                You need Chrome or Chromium on the machine. Headless runs use chrome-headless-shell
                when it is installed, which opened pages 5 to 10 times faster in our runs.
              </p>
              <CopyCommand command="npx @puppeteer/browsers install chrome-headless-shell@stable" />
            </div>
            <Tabs
              label="Install instructions"
              tabs={[
                {
                  title: "npm",
                  content: (
                    <>
                      <Code label="npm install">{`npm install fluxwright`}</Code>
                      <p className="note">
                        Prebuilt for Windows x64, macOS (Apple silicon &amp; Intel), and Linux (x64
                        &amp; ARM64 glibc). Node 18 or later.
                      </p>
                    </>
                  ),
                },
                {
                  title: "pip",
                  content: (
                    <>
                      <Code label="pip install">{`pip install fluxwright`}</Code>
                      <p className="note">
                        Wheels for Windows x64, macOS (Apple silicon &amp; Intel), and Linux (x64
                        &amp; ARM64 glibc). Python 3.10 or later, with asyncio and blocking APIs.
                      </p>
                    </>
                  ),
                },
                {
                  title: "Rust",
                  content: (
                    <>
                      <Code label="Cargo.toml dependency">{`[dependencies]
fluxwright = { git = "https://github.com/harshit-d3v/fluxwright" }
tokio = { version = "1", features = ["full"] }`}</Code>
                      <p className="note">Rust 1.95 or later.</p>
                    </>
                  ),
                },
                {
                  title: "MCP",
                  content: (
                    <>
                      <Code label="Install the MCP server">{`cargo install --git https://github.com/harshit-d3v/fluxwright fluxwright-mcp`}</Code>
                      <p className="note">
                        Prebuilt binaries are also available on{" "}
                        <a href={RELEASES}>GitHub Releases</a>. Add it to Claude Desktop, Codex, or
                        Cursor as shown in the code section. The fleet runs on your machine or your
                        own container, so serverless hosts cannot run it.
                      </p>
                    </>
                  ),
                },
                {
                  title: "CLI",
                  content: (
                    <>
                      <Code label="Install and use the CLI">{`cargo install --git https://github.com/harshit-d3v/fluxwright fluxwright-cli
fluxwright install
fluxwright doctor
fluxwright start --max-browsers 4 --max-contexts 8
fluxwright serve --listen 127.0.0.1:7331`}</Code>
                      <p className="note">
                        Prebuilt CLI archives are on <a href={RELEASES}>GitHub Releases</a>.{" "}
                        <code>fluxwright install</code> fetches managed Chromium binaries; use{" "}
                        <code>fluxwright serve</code> and <code>fluxwright coordinate</code> for
                        HTTP and cluster mode.
                      </p>
                    </>
                  ),
                },
                {
                  title: "Docker",
                  content: (
                    <>
                      <Code label="Run Fluxwright in Docker">{`docker pull ghcr.io/harshit-d3v/fluxwright:latest
docker run --rm \\
  -e FLUXWRIGHT_SERVER_TOKEN="$FLUXWRIGHT_SERVER_TOKEN" \\
  -p 127.0.0.1:7331:7331 --shm-size=1g \\
  --memory=4g --cpus=4 --cap-add=SYS_ADMIN \\
  ghcr.io/harshit-d3v/fluxwright:latest`}</Code>
                      <p className="note">
                        Multi-arch image (<code>linux/amd64</code> + <code>linux/arm64</code>) on GHCR.
                        Reads container cgroup limits automatically for browser count and RAM ceiling,
                        and uses <code>tini</code> as PID 1 so Chromium subprocesses are always reaped.
                      </p>
                    </>
                  ),
                },
              ]}
            />
          </div>
        </section>

        <section className="section" id="scope" aria-labelledby="scope-title">
          <div className="container">
            <div className="section-head">
              <h2 id="scope-title">Know before you choose it</h2>
            </div>
            <div className="scope">
              <div>
                <h3>What it does not do</h3>
                <ul>
                  <li>Firefox or WebKit. Fluxwright is Chromium only.</li>
                  <li>
                    Interactive GUI codegen or a built-in Trace Viewer UI (Fluxwright writes
                    Playwright-compatible <code>.zip</code> traces you can open in Playwright Trace
                    Viewer).
                  </li>
                  <li>Java or .NET SDKs. Fluxwright targets Node, Python, and Rust.</li>
                  <li>Anti-bot evasion.</li>
                </ul>
              </div>
              <div>
                <h3>Safe by default</h3>
                <ul>
                  <li>Chrome&apos;s sandbox stays on unless you set FLUXWRIGHT_NO_SANDBOX=1.</li>
                  <li>On Linux and macOS the engine talks to Chrome over a private pipe, not a debugging port.</li>
                  <li>Jobs never share cookies or storage.</li>
                  <li>Chrome exits when the engine exits, even after a crash.</li>
                  <li>Bearer tokens are required on HTTP and CDP server endpoints even on loopback, with single-use WebSocket tickets for live dashboard streams.</li>
                  <li>
                    Saved profiles and job journals are written owner-only (<code>0600</code> on Unix),
                    and cluster workers support isolated <code>FLUXWRIGHT_WORKER_TOKEN</code> and native TLS.
                  </li>
                </ul>
              </div>
            </div>
          </div>
        </section>
      </main>

      <footer className="site-footer">
        <div className="container footer-row">
          <a className="wordmark" href="#top">
            <Logo />
            <span>Fluxwright</span>
          </a>
          <nav aria-label="Footer">
            <ul>
              {SECTIONS.map((s) => (
                <li key={s.href}>
                  <a href={s.href}>{s.label}</a>
                </li>
              ))}
              <li>
                <a href={GITHUB}>GitHub</a>
              </li>
              <li>
                <a href={NPM}>npm</a>
              </li>
              <li>
                <a href={PYPI}>PyPI</a>
              </li>
              <li>
                <a href={RELEASES}>Releases</a>
              </li>
            </ul>
          </nav>
          <p className="footer-note">Apache-2.0 licensed.</p>
        </div>
      </footer>
    </>
  );
}
