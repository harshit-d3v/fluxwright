import type { CSSProperties } from "react";
import { CopyCommand } from "./_components/CopyCommand";
import { FleetBoard } from "./_components/FleetBoard";
import { SiteHeader } from "./_components/SiteHeader";
import { GITHUB, Logo, NPM, SECTIONS } from "./_components/site";
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
    name: "A memory ceiling",
    text: "No new work starts while the fleet's real memory footprint is over your budget. Shared pages are not counted twice.",
    api: "memory_ceiling_mb",
  },
  {
    name: "Recycling",
    text: "A browser restarts after a number of jobs, an age, or a memory threshold. It drains its running jobs first.",
    api: "recycle_after_jobs",
  },
  {
    name: "Crash recovery",
    text: "A dead browser fails its jobs at once and they retry on a healthy one. Chrome is tied to the engine, so it never outlives a crash.",
    api: "JobOptions::retries",
  },
  {
    name: "A Playwright-style API",
    text: "getByRole, getByText, frameLocator for cross-origin iframes, waitUntil, and clicks that wait until the element can take them.",
    api: "page.getByRole()",
  },
  {
    name: "A proxy per job",
    text: "Each job can use its own proxy, with a username and password, on the same browser as jobs that use none.",
    api: "newPage({ proxy })",
  },
  {
    name: "Built for agents",
    text: "An MCP server lets Claude, Codex or Cursor drive the fleet on your machine.",
    api: "fluxwright-mcp",
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

const BENCH: { setup: string; flux: [number, number]; pw: [number, number] }[] = [
  { setup: "5 browsers, 2 pages each", flux: [66, 70], pw: [45, 51] },
  { setup: "1 browser, 10 pages", flux: [54, 58], pw: [42, 46] },
];
const BENCH_MAX = 80;

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

await page.close(); // the context is destroyed and the slot is free
await browser.close();`;

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

function Code({ children, label }: { children: string; label: string }) {
  return (
    <pre className="code" aria-label={label}>
      <code>{children}</code>
    </pre>
  );
}

function Bar({ range }: { range: [number, number] }) {
  const [lo, hi] = range;
  return (
    <span className="bar" aria-hidden="true">
      <span
        className="bar-fill"
        style={{ "--from": `${(lo / BENCH_MAX) * 100}%`, "--to": `${(hi / BENCH_MAX) * 100}%` } as CSSProperties}
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
              it from Node, from Rust, or from an AI agent over MCP.
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
              Version 0.2.1, open source under Apache-2.0. Prebuilt for Windows, macOS and Linux.
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
              <p>The same five steps whether a job comes from Node, Rust or an agent.</p>
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
                handles the fleet. Node gets a native addon, so there is no driver process in between.
              </p>
              <p className="note">
                The MCP server exposes open, goto, click, fill, evaluate, screenshot and close.
                Selectors accept CSS, <code>text=</code> and <code>role=</code>.
              </p>
            </div>
            <Tabs
              label="Code examples"
              tabs={[
                { title: "TypeScript", content: <Code label="TypeScript example">{TS_EXAMPLE}</Code> },
                { title: "Rust", content: <Code label="Rust example">{RUST_EXAMPLE}</Code> },
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
                Jobs per second, higher is better. Both tools ran on the same chrome-headless-shell
                binary, alternating runs, with 10 jobs in flight loading a small local page.
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
                      <span className="bench-value">
                        {b.flux[0]}–{b.flux[1]}
                      </span>
                      <Bar range={b.flux} />
                    </td>
                    <td className="bench-other">
                      <span className="bench-value">
                        {b.pw[0]}–{b.pw[1]}
                      </span>
                      <Bar range={b.pw} />
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
            <p className="note bench-note">
              One Windows machine, 30 September 2026. The first Fluxwright run of each series ran
              cold and slower (30 jobs/s) and is left out of the ranges. On Chrome&apos;s regular
              headless mode the two were level, at about 13 jobs/s. One machine and one page shape
              say little about yours, so measure your own workload with{" "}
              <code>cargo run --release -p fluxwright-benchmarks</code>.
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
                        Prebuilt for Windows x64, macOS on Apple silicon and Intel, and Linux x64
                        (glibc). Node 18 or later.
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
                      <p className="note">Rust 1.85 or later.</p>
                    </>
                  ),
                },
                {
                  title: "MCP",
                  content: (
                    <>
                      <Code label="Install the MCP server">{`cargo install --git https://github.com/harshit-d3v/fluxwright fluxwright-mcp`}</Code>
                      <p className="note">
                        Add it to Claude Desktop, Codex or Cursor as shown in the code section. Use the
                        full path to <code>fluxwright-mcp</code> if it is not on your PATH. The fleet
                        runs on your machine, so serverless hosts cannot run it.
                      </p>
                    </>
                  ),
                },
                {
                  title: "CLI",
                  content: (
                    <>
                      <Code label="Install and use the CLI">{`cargo install --git https://github.com/harshit-d3v/fluxwright fluxwright-cli
fluxwright doctor
fluxwright start --max-browsers 4 --max-contexts 8
fluxwright stats`}</Code>
                      <p className="note">
                        The daemon listens on a Unix socket, or a named pipe on Windows. Never on TCP.
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
                  <li>Test running: no fixtures, assertions, tracing, HAR or video. Bring your own test framework.</li>
                  <li>More than one page per context, downloads and uploads.</li>
                  <li>locator.filter, nth and getByLabel. getByRole, getByText and frameLocator are supported.</li>
                  <li>Attaching to a browser that is already running. Planned.</li>
                </ul>
              </div>
              <div>
                <h3>Safe by default</h3>
                <ul>
                  <li>Chrome&apos;s sandbox stays on unless you set FLUXWRIGHT_NO_SANDBOX=1.</li>
                  <li>On Linux and macOS the engine talks to Chrome over a private pipe, not a debugging port.</li>
                  <li>Jobs never share cookies or storage.</li>
                  <li>Chrome exits when the engine exits, even after a crash.</li>
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
            </ul>
          </nav>
          <p className="footer-note">Apache-2.0 licensed.</p>
        </div>
      </footer>
    </>
  );
}
