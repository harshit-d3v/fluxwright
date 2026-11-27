import type { CSSProperties } from "react";
import { BackToTop } from "./_components/BackToTop";
import { CapabilityGallery } from "./_components/CapabilityGallery";
import { CodeBlock } from "./_components/CodeBlock";
import { CopyCommand } from "./_components/CopyCommand";
import { FleetBoard } from "./_components/FleetBoard";
import { SiteHeader } from "./_components/SiteHeader";
import {
  CLUSTER_DOCS,
  COMPARE_DOCS,
  CRATES,
  DEPLOY_DOCS,
  GHCR,
  GITHUB,
  Logo,
  MIGRATION_DOCS,
  NPM,
  PYPI,
  RELEASES,
  SECTIONS,
  SERVER_DOCS,
} from "./_components/site";
import { Tabs } from "./_components/Tabs";

const HERO_PRESETS = [
  { label: "npm", command: "npm install fluxwright" },
  { label: "pip", command: "pip install fluxwright" },
  { label: "CLI", command: "cargo install fluxwright-cli" },
  { label: "Docker", command: "docker pull ghcr.io/harshit-d3v/fluxwright:latest" },
];

const TIERS: { tier: string; title: string; detail: string; spec: string }[] = [
  {
    tier: "TIER 1 · DEFAULT",
    title: "Pure-Rust HTTP + html5ever",
    detail:
      "Fetches static docs, blogs, sitemaps, and server-rendered pages using rustls + ring and DNS-pinned SSRF checks on every redirect hop. Zero Chrome launched.",
    spec: "mode: \"auto\" | \"http\"",
  },
  {
    tier: "TIER 2 · OPTIONAL",
    title: "External Light-CDP Server",
    detail:
      "Routes non-visual escalated JS jobs to a user-supplied lightweight CDP endpoint (such as Moli, Obscura, or Lightpanda) with graceful -32601 fallback and zero AGPL entanglement.",
    spec: "--fast-cdp-endpoint <url>",
  },
  {
    tier: "TIER 3 · FULL FIDELITY",
    title: "Pooled chrome-headless-shell",
    detail:
      "Automatically takes over for SPAs, challenge pages, interactions, screenshots, PDFs, or snapshots with isolated contexts, crash retries, and optional --low-memory flags.",
    spec: "--low-memory · cgroup 75%",
  },
];

const STEPS: { num: string; title: string; text: string }[] = [
  { num: "01", title: "Route", text: "Static scrapes hit Tier 1 Rust HTTP first; browser jobs enter engine.run()." },
  { num: "02", title: "Admit", text: "Busy pools queue by priority or shed load above the cgroup RAM ceiling." },
  { num: "03", title: "Lease", text: "Each job gets an isolated context on the least busy Chrome. Zero shared state." },
  { num: "04", title: "Run", text: "Drives navigation, locators, snapshots, screencasts, or visual diffs safely." },
  { num: "05", title: "Recycle", text: "Destroys the context immediately and drains bloated browsers without stalling." },
];

// GitHub Actions benchmarks run 45952022742 on ubuntu-latest (4 vCPU, chrome-headless-shell 158.0.8127.88), 27 November 2026.
const BENCH: {
  setup: string;
  flux: number;
  cluster: number | null;
  crawlee: number;
  pw: number;
  sel: number | null;
}[] = [
  { setup: "1 browser, 10 pages (100 jobs)", flux: 35.2, cluster: 33.5, crawlee: 21.8, pw: 31.0, sel: 28.1 },
  { setup: "10 browsers, 2 pages each (40 jobs)", flux: 41.8, cluster: 30.7, crawlee: 19.6, pw: 31.3, sel: 25.5 },
  { setup: "500 concurrent (10 browsers × 20 pages)", flux: 43.8, cluster: 33.5, crawlee: 23.9, pw: 33.3, sel: 26.0 },
  { setup: "1,000 queued (4 browsers × 8 pages)", flux: 42.7, cluster: 32.7, crawlee: 24.0, pw: 34.5, sel: 27.7 },
  { setup: "100 PDFs (4 browsers × 8 pages)", flux: 37.6, cluster: 29.1, crawlee: 22.1, pw: 32.5, sel: 23.7 },
  { setup: "200 scraping jobs (images + fonts)", flux: 30.2, cluster: 27.1, crawlee: 20.6, pw: 25.1, sel: 20.8 },
  { setup: "1,000 chaos jobs (browser kill / 2s)", flux: 40.0, cluster: 23.3, crawlee: 3.3, pw: 25.8, sel: null },
];
const BENCH_MAX = 48;

const TS_EXAMPLE = `import { chromium } from "fluxwright";

const browser = await chromium.launch({ maxBrowsers: 4 });

// Fresh browser context on a pooled Chrome.
const page = await browser.newPage();
await page.goto("https://example.com", { waitUntil: "domcontentloaded" });

await page.getByRole("button", { name: "Accept" }).click();
await page.getByRole("textbox", { name: "Search" }).fill("chromium");

// Cross-origin iframes pierce cleanly with the same locator API.
const payment = page.frameLocator("iframe#payment");
await payment.getByRole("textbox", { name: "Card" }).fill("4242");

// AI accessibility snapshots (@eN@rK revision guardrails) + Markdown.
const snapshot = await page.ariaSnapshot();
const markdown = await page.markdown();

await page.close(); // destroys context and frees the pool slot
await browser.close();`;

const PY_EXAMPLE = `import asyncio
from fluxwright.async_api import async_playwright

async def main():
    async with async_playwright() as p:
        browser = await p.chromium.launch(max_browsers=4)

        # Fresh browser context on a pooled Chrome.
        page = await browser.new_page()
        await page.goto("https://example.com", wait_until="domcontentloaded")
        print(await page.title())

        await page.get_by_role("button", name="Accept").click()
        await page.get_by_role("textbox", name="Search").fill("chromium")

        await page.close()  # destroys context and frees the pool slot

asyncio.run(main())`;

const RUST_EXAMPLE = `use fluxwright::{BrowserEngine, JobOptions, Priority};

let engine = BrowserEngine::builder()
    .max_browsers(8)
    .max_contexts_per_browser(10)
    .memory_ceiling_mb(8192)
    .recycle_after_jobs(200)
    .low_memory(true)
    .build()
    .await?;

// run() queues, leases, retries on renderer crash, and drops the context.
let job = JobOptions::default().priority(Priority::High).retries(2);
let title = engine
    .run(job, |page| async move {
        page.goto("https://example.com").await?;
        page.title().await
    })
    .await?;`;

const HYBRID_EXAMPLE = `// POST /v1/scrape with Tier 1 pure-Rust HTTP fast path ("mode": "auto")
const res = await fetch("http://127.0.0.1:7331/v1/scrape", {
  method: "POST",
  headers: {
    "Authorization": \`Bearer \${process.env.FLUXWRIGHT_SERVER_TOKEN}\`,
    "Content-Type": "application/json",
  },
  body: JSON.stringify({
    url: "https://docs.example.com/guide",
    mode: "auto",                  // "auto" | "http" | "browser"
    formats: ["markdown", "text"],
    block_cookie_banners: true,
  }),
});

const data = await res.json();
// data.engine => "http" (static) or "browser" (auto-escalated SPA)
// data.fallback_reason => populated whenever Tier 1 escalates

// POST /v1/map discovers links + optional XML sitemaps cleanly:
// { "url": "https://example.com", "mode": "auto", "include_sitemap": true }`;

const E2E_EXAMPLE = `import { test, expect } from "fluxwright/test";

test.use({ trace: "retain-on-failure", video: "retain-on-failure" });

test("homepage renders", async ({ page }) => {
  await page.goto("https://example.com");
  await expect(page).toHaveTitle(/Example/);
  await expect(page.getByRole("heading")).toHaveText("Example Domain");
  await expect(page).toHaveScreenshot("home.png");
});

// Parallel runner with sharding, retries, and HTML report:
// $ npx fluxwright-test --workers=4 --retries=1 --reporter=list,html`;

const GATEWAY_EXAMPLE = `// Serve hybrid HTTP API + pooled /cdp gateway + managed sessions
// $ fluxwright serve \\
//     --low-memory \\
//     --recycle-rss-mb 512 \\
//     --fast-cdp-endpoint ws://127.0.0.1:9222 \\
//     --allow-query-token --profiles-dir ./profiles --adaptive-concurrency

// Point existing Playwright scripts directly at the pooled gateway:
import { chromium } from "playwright";

const browser = await chromium.connectOverCDP(
  \`ws://127.0.0.1:7331/cdp?token=\${process.env.FLUXWRIGHT_SERVER_TOKEN}\`,
);
const page = await browser.newPage();
await page.goto("https://example.com");`;

const MCP_EXAMPLE = `{
  "mcpServers": {
    "fluxwright": {
      "command": "fluxwright-mcp",
      "args": ["--http", "127.0.0.1:7440"]
    }
  }
}`;

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
            <p className="eyebrow">APACHE-2.0 · V0.14.0 RELEASED · 3-TIER HYBRID ENGINE</p>
            <h1 id="hero-title">
              Run a fast, isolated <em className="serif-italic">fleet</em> of headless Chrome.
            </h1>
            <p className="lead">
              Fluxwright resolves static pages in pure Rust HTTP first, routes non-visual scripts
              through an optional light-CDP tier, and pools isolated Chrome contexts with automatic
              crash recovery, replayable sessions, built-in E2E tests, and a 22-tool MCP server.
            </p>
            <CopyCommand presets={HERO_PRESETS} />
            <div className="actions">
              <a className="button primary" href="#code">
                Explore the code →
              </a>
              <a className="button" href="#benchmarks">
                View CI benchmarks
              </a>
            </div>
            <p className="meta">
              Published at <strong>v0.14.0</strong> across crates.io (6 crates), npm (6 unscoped packages, 5 native targets), PyPI (5 wheels + sdist), GitHub Releases (7 targets including Windows ARM64 and Linux musl), and multi-arch GHCR.
            </p>
          </div>
          <FleetBoard />
        </section>

        <CapabilityGallery />

        <section className="section" id="how" aria-labelledby="how-title">
          <div className="container">
            <div className="section-head">
              <span className="eyebrow">02 / EXECUTION ARCHITECTURE</span>
              <h2 id="how-title">
                Three tiers when speed counts, <em className="serif-italic">one</em> lifecycle underneath.
              </h2>
              <p>
                Stay in zero-browser Rust HTTP when the HTML already has what you need, or escalate
                seamlessly to external light-CDP or pooled Chromium without changing callers.
              </p>
            </div>

            <div className="tier-grid">
              {TIERS.map((t) => (
                <article className="tier-card" key={t.tier}>
                  <span className="tier-kicker">{t.tier}</span>
                  <h3>{t.title}</h3>
                  <p>{t.detail}</p>
                  <code className="tier-spec">{t.spec}</code>
                </article>
              ))}
            </div>

            <ol className="steps">
              {STEPS.map((s) => (
                <li key={s.title}>
                  <span className="step-badge">{s.num}</span>
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
              <span className="eyebrow">03 / POLYGLOT SDK &amp; PROTOCOLS</span>
              <h2 id="code-title">
                Native speed in every <em className="serif-italic">runtime</em>.
              </h2>
              <p>
                Node and Python load a compiled Rust native module directly in process, eliminating
                intermediate Node/Go driver hops. Use one Playwright-style API across TypeScript,
                Python, Rust, HTTP, CDP WebSockets, and MCP.
              </p>
              <p className="note">
                The 22-tool MCP server exposes navigation (<code>open</code>, <code>goto</code>,{" "}
                <code>back</code>, <code>forward</code>, <code>tabs</code>, <code>close</code>), AI
                inspection (<code>snapshot</code> with <code>@eN@rK</code> revision guardrails,{" "}
                <code>markdown</code>, <code>title</code>, <code>content</code>,{" "}
                <code>screenshot</code>), actions (<code>click</code>, <code>hover</code>,{" "}
                <code>fill</code>, <code>press</code>, <code>select</code>, <code>scroll</code>,{" "}
                <code>wait_for</code>, <code>evaluate</code>), and diagnostics (
                <code>console</code>, <code>network</code>, <code>export_api_calls</code>).
              </p>
            </div>
            <Tabs
              label="Code examples"
              tabs={[
                { title: "TypeScript", content: <CodeBlock label="TypeScript SDK">{TS_EXAMPLE}</CodeBlock> },
                { title: "Python", content: <CodeBlock label="Python async SDK">{PY_EXAMPLE}</CodeBlock> },
                { title: "Rust", content: <CodeBlock label="Rust BrowserEngine">{RUST_EXAMPLE}</CodeBlock> },
                { title: "Hybrid Scrape", content: <CodeBlock label="POST /v1/scrape and /v1/map">{HYBRID_EXAMPLE}</CodeBlock> },
                { title: "E2E Test", content: <CodeBlock label="fluxwright/test E2E runner">{E2E_EXAMPLE}</CodeBlock> },
                { title: "Server / CDP", content: <CodeBlock label="fluxwright serve and CDP gateway">{GATEWAY_EXAMPLE}</CodeBlock> },
                { title: "MCP", content: <CodeBlock label="fluxwright-mcp configuration">{MCP_EXAMPLE}</CodeBlock> },
              ]}
            />
          </div>
        </section>

        <section className="section" id="benchmarks" aria-labelledby="bench-title">
          <div className="container">
            <div className="section-head">
              <span className="eyebrow">04 / MEASURED IN GITHUB ACTIONS</span>
              <h2 id="bench-title">
                Benchmarks that hold under <em className="serif-italic">chaos</em>.
              </h2>
              <p>
                Every runner executed on identical <code>chrome-headless-shell 158.0.8127.88</code>{" "}
                binaries with isolated browser contexts per job on <code>ubuntu-latest</code> (4 vCPU).
              </p>
            </div>

            <div className="kpi-strip">
              <div className="kpi-card">
                <span className="kpi-label">8 / 8 scenarios</span>
                <strong>Fastest</strong>
                <span>Beat Crawlee, puppeteer-cluster, Playwright, Puppeteer, and Selenium</span>
              </div>
              <div className="kpi-card">
                <span className="kpi-label">1,000-job chaos</span>
                <strong>40.0 /s</strong>
                <span>0.0% failed across 11 browser kills (Crawlee 3.3 /s, Selenium 82.4% failed)</span>
              </div>
              <div className="kpi-card">
                <span className="kpi-label">Chaos p95 latency</span>
                <strong>869 ms</strong>
                <span>vs 16,105 ms for Crawlee under 2s kills (0 leaked Chrome)</span>
              </div>
              <div className="kpi-card">
                <span className="kpi-label">Tier 1 fast path</span>
                <strong>497.0 /s</strong>
                <span>Static /v1/scrape in 14 MB RSS vs 23.8–24.8 /s in browser</span>
              </div>
            </div>

            <table className="bench">
              <caption className="visually-hidden">Throughput in jobs per second</caption>
              <thead>
                <tr>
                  <th scope="col">Workload scenario</th>
                  <th scope="col">Fluxwright (jobs/s)</th>
                  <th scope="col">puppeteer-cluster</th>
                  <th scope="col">Crawlee</th>
                  <th scope="col">Playwright</th>
                  <th scope="col">Selenium (BiDi)</th>
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
                      {b.cluster === null ? (
                        <span className="bench-value">timeout</span>
                      ) : (
                        <>
                          <span className="bench-value">{b.cluster.toFixed(1)}</span>
                          <Bar value={b.cluster} />
                        </>
                      )}
                    </td>
                    <td className="bench-other">
                      <span className="bench-value">{b.crawlee.toFixed(1)}</span>
                      <Bar value={b.crawlee} />
                    </td>
                    <td className="bench-other">
                      <span className="bench-value">{b.pw.toFixed(1)}</span>
                      <Bar value={b.pw} />
                    </td>
                    <td className="bench-other">
                      {b.sel === null ? (
                        <span className="bench-value">82.4% failed</span>
                      ) : (
                        <>
                          <span className="bench-value">{b.sel.toFixed(1)}</span>
                          <Bar value={b.sel} />
                        </>
                      )}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>

            <p className="note bench-note">
              Measured in GitHub Actions run <code>45952022742</code> (27 November 2026) with
              full-lifecycle job timing, process-tree RSS, and normalized host CPU% on{" "}
              <code>chrome-headless-shell 158.0.8127.88</code>, plus <code>fastpath-bench.mjs</code>{" "}
              (497.0 req/s on <code>/v1/scrape</code>, 1,023.5 req/s on <code>/v1/map</code> in 14 MB RSS).
              Selenium runs WebDriver BiDi (fresh <code>browser.createUserContext()</code> per job); Crawlee
              uses <code>797–1,216 MB</code> peak RAM behind <code>pLimit(1)</code> page serialization. In
              chaos, raw Playwright (<code>74.0%</code> failed) and Selenium (<code>82.4%</code> failed) drop
              jobs once their 4 browsers die, while <code>soak-5m</code> (<code>45825257140</code>) held
              7,898 jobs at 0.0% failure.
            </p>
          </div>
        </section>

        <section className="section" id="install" aria-labelledby="install-title">
          <div className="container split">
            <div className="section-head">
              <span className="eyebrow">05 / DEPLOYMENT TARGETS</span>
              <h2 id="install-title">
                Install in seconds, run <em className="serif-italic">anywhere</em>.
              </h2>
              <p>
                Tier 1 HTTP fast-path endpoints run with zero browser binaries installed. For full
                Chromium rendering, install <code>chrome-headless-shell</code>, which opened pages 5
                to 10 times faster than full headed Chrome in our CI runs.
              </p>
              <CopyCommand command="fluxwright install" />
            </div>
            <Tabs
              label="Install instructions"
              tabs={[
                {
                  title: "npm",
                  content: (
                    <>
                      <CodeBlock label="Install via npm">{`npm install fluxwright`}</CodeBlock>
                      <p className="note">
                        Unscoped prebuilt binaries across Windows x64, macOS (Apple silicon &amp; Intel),
                        and Linux (x64 &amp; ARM64 glibc). Requires Node 18 or later.
                      </p>
                    </>
                  ),
                },
                {
                  title: "pip",
                  content: (
                    <>
                      <CodeBlock label="Install via pip">{`pip install fluxwright`}</CodeBlock>
                      <p className="note">
                        ABI3 wheels and source dist across Windows x64, macOS (Apple silicon &amp; Intel),
                        and Linux (x64 &amp; ARM64 glibc). Python 3.10+, asyncio &amp; sync APIs + pytest plugin.
                      </p>
                    </>
                  ),
                },
                {
                  title: "Rust",
                  content: (
                    <>
                      <CodeBlock label="Add to Cargo.toml">{`[dependencies]
fluxwright = "0.14"
tokio = { version = "1", features = ["full"] }`}</CodeBlock>
                      <p className="note">Published on crates.io for Rust 1.95+ with pure-Rust ring TLS (zero cmake/nasm required).</p>
                    </>
                  ),
                },
                {
                  title: "MCP",
                  content: (
                    <>
                      <CodeBlock label="Install fluxwright-mcp">{`cargo install fluxwright-mcp
fluxwright-mcp --http 127.0.0.1:7440`}</CodeBlock>
                      <p className="note">
                        Prebuilt <code>fluxwright-mcp</code> archives for all 7 targets are on{" "}
                        <a href={RELEASES}>GitHub Releases</a>.
                      </p>
                    </>
                  ),
                },
                {
                  title: "CLI",
                  content: (
                    <>
                      <CodeBlock label="Install and launch fluxwright-cli">{`cargo install fluxwright-cli
fluxwright install
fluxwright doctor
fluxwright serve --listen 127.0.0.1:7331 --low-memory --recycle-rss-mb 512`}</CodeBlock>
                      <p className="note">
                        Add <code>--fast-cdp-endpoint ws://...</code> for external light-CDP execution or use{" "}
                        <code>fluxwright coordinate</code> for multi-node cluster leases.
                      </p>
                    </>
                  ),
                },
                {
                  title: "Docker",
                  content: (
                    <>
                      <CodeBlock label="Multi-arch GHCR container">{`docker pull ghcr.io/harshit-d3v/fluxwright:latest
docker run --rm \\
  -e FLUXWRIGHT_SERVER_TOKEN="$FLUXWRIGHT_SERVER_TOKEN" \\
  -p 127.0.0.1:7331:7331 --shm-size=1g \\
  --memory=4g --cpus=4 --cap-add=SYS_ADMIN \\
  ghcr.io/harshit-d3v/fluxwright:latest`}</CodeBlock>
                      <p className="note">
                        Multi-arch image (<code>linux/amd64</code> + <code>linux/arm64</code>) on GHCR.
                        Reads container cgroup limits automatically and reaps Chromium subprocesses via{" "}
                        <code>tini</code>.
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
              <span className="eyebrow">06 / HONEST BOUNDARIES</span>
              <h2 id="scope-title">
                Know before you <em className="serif-italic">choose</em> it.
              </h2>
            </div>
            <div className="scope">
              <div className="scope-card">
                <h3>What it does not do</h3>
                <ul>
                  <li>Firefox or WebKit. Fluxwright is HTTP fast-path + Chromium CDP only.</li>
                  <li>
                    Interactive GUI codegen or a built-in Trace Viewer UI (Fluxwright writes
                    Playwright-compatible <code>.zip</code> traces you open in Playwright Trace Viewer).
                  </li>
                  <li>Java or .NET SDKs. Official bindings target Node, Python, and Rust.</li>
                  <li>Anti-bot evasion.</li>
                </ul>
              </div>
              <div className="scope-card">
                <h3>Safe by default</h3>
                <ul>
                  <li>Chrome&apos;s sandbox stays enabled unless <code>FLUXWRIGHT_NO_SANDBOX=1</code> is set.</li>
                  <li>Tier 1 HTTP fast path validates URLs via netguard DNS pinning on every 3xx redirect hop.</li>
                  <li>Linux and macOS control Chrome over a private pipe, never an open debug port.</li>
                  <li>Jobs never share cookies or storage; Chrome exits immediately when the engine exits.</li>
                  <li>
                    HTTP and CDP endpoints require bearer tokens; saved profiles and journals write{" "}
                    <code>0600</code> on Unix with isolated <code>FLUXWRIGHT_WORKER_TOKEN</code> TLS support.
                  </li>
                </ul>
              </div>
            </div>
          </div>
        </section>
      </main>

      <footer className="site-footer">
        <div className="container">
          <div className="footer-grid">
            <div className="footer-brand">
              <a className="wordmark" href="#top">
                <Logo />
                <span>
                  Fluxwright<span className="wordmark-dot" aria-hidden="true">.</span>
                </span>
              </a>
              <p>
                Built for fleets, not single tabs. Apache-2.0 open source engine in Rust, Node, and Python.
              </p>
            </div>
            <nav className="footer-col" aria-label="Page sections">
              <h4>Explore</h4>
              <ul>
                {SECTIONS.map((s) => (
                  <li key={s.href}>
                    <a href={s.href}>{s.label}</a>
                  </li>
                ))}
              </ul>
            </nav>
            <nav className="footer-col" aria-label="Documentation and distributions">
              <h4>Distributions</h4>
              <ul>
                <li>
                  <a href={CRATES}>crates.io crates</a>
                </li>
                <li>
                  <a href={NPM}>npm package</a>
                </li>
                <li>
                  <a href={PYPI}>PyPI package</a>
                </li>
                <li>
                  <a href={GHCR}>GHCR container</a>
                </li>
                <li>
                  <a href={RELEASES}>GitHub Releases</a>
                </li>
              </ul>
            </nav>
            <nav className="footer-col" aria-label="Documentation">
              <h4>References</h4>
              <ul>
                <li>
                  <a href={GITHUB}>Source repository</a>
                </li>
                <li>
                  <a href={SERVER_DOCS}>HTTP Server &amp; MCP docs</a>
                </li>
                <li>
                  <a href={CLUSTER_DOCS}>Cluster coordination docs</a>
                </li>
                <li>
                  <a href={MIGRATION_DOCS}>Migration guide</a>
                </li>
                <li>
                  <a href={COMPARE_DOCS}>Competitive analysis</a>
                </li>
                <li>
                  <a href={DEPLOY_DOCS}>Self-hosting kit</a>
                </li>
                <li>
                  <a href="/llms.txt">llms.txt</a>
                </li>
              </ul>
            </nav>
          </div>

          <hr className="footer-dotted" />

          <div className="footer-bottom">
            <p className="footer-note">Apache-2.0 licensed · Published release v0.14.0</p>
            <div className="footer-links">
              <a href={GITHUB}>GitHub</a>
              <a href={CRATES}>crates.io</a>
              <a href={NPM}>npm</a>
              <a href={PYPI}>PyPI</a>
              <a href={RELEASES}>Releases</a>
            </div>
          </div>
        </div>
      </footer>
      <BackToTop />
    </>
  );
}
