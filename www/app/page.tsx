function Fleet() {
  const rows = [
    [1, 1, 1, 0, 0, 0, 0, 0],
    [1, 1, 0, 0, 0, 0, 0, 0],
    [1, 1, 1, 1, 1, 0, 0, 0],
    [0, 0, 0, 0, 0, 0, 0, 0],
    [1, 0, 0, 0, 0, 0, 0, 0],
  ];
  return (
    <div className="fleet" aria-hidden="true">
      {rows.map((cells, i) => (
        <div className="row" key={i}>
          <span>browser {i + 1}</span>
          {cells.map((on, j) => (
            <div className={on ? "cell on" : "cell"} key={j} />
          ))}
        </div>
      ))}
    </div>
  );
}

export default function Home() {
  return (
    <div className="wrap">
      <aside className="rail">
        <a className="mark" href="/" translate="no">
          Fluxwright
        </a>
        <nav aria-label="On this page">
          <a href="#what">What it is</a>
          <a href="#install">Install</a>
          <a href="#rust">Rust</a>
          <a href="#npm">TypeScript / npm</a>
          <a href="#cli">CLI</a>
          <a href="#mcp">MCP</a>
          <a href="#bench">Benchmarks</a>
          <a href="#security">Security</a>
          <a href="#missing">Not Playwright</a>
        </nav>
      </aside>
      <main id="main">
        <h1>A fleet for Chromium, not another driver.</h1>
        <p className="lede">
          Fluxwright hands out page leases: a fresh browser context on a pooled
          Chrome process, with a queue, an RSS ceiling, recycle, and crash
          retry. Client overhead is noise next to Chromium. The job is running
          many browsers without deadlocking the box.
        </p>
        <div className="btns">
          <a className="btn" href="#install">
            Install
          </a>
          <a className="btn ghost" href="#npm">
            npm i fluxwright
          </a>
          <a className="btn ghost" href="#mcp">
            Claude / Codex MCP
          </a>
        </div>
        <Fleet />
        <p className="cap">
          Five browser processes. Filled cells are live contexts (leases). Empty
          cells are free slots. Drop the lease, the context is destroyed.
        </p>

        <h2 id="what">What it is</h2>
        <p>
          Automation libraries (Playwright, Puppeteer) speak CDP well. They do
          not, by themselves, decide how many Chromes to launch, when to refuse
          work, when to recycle a process after <em>n</em> jobs, or how to
          recover when a renderer dies. Fluxwright is that control plane.
        </p>
        <table>
          <thead>
            <tr>
              <th>Piece</th>
              <th>Role</th>
            </tr>
          </thead>
          <tbody>
            <tr>
              <td translate="no">fluxwright-cdp</td>
              <td>One WebSocket per browser, flat sessions</td>
            </tr>
            <tr>
              <td translate="no">fluxwright-core</td>
              <td>Pool, scheduler, RSS, recycle, metrics</td>
            </tr>
            <tr>
              <td translate="no">fluxwright</td>
              <td>Public Rust API</td>
            </tr>
            <tr>
              <td translate="no">fluxwright-cli</td>
              <td>
                Daemon: start / stats / browsers / doctor
              </td>
            </tr>
            <tr>
              <td translate="no">fluxwright-mcp</td>
              <td>MCP stdio server for Claude, Codex, Cursor</td>
            </tr>
            <tr>
              <td translate="no">npm fluxwright</td>
              <td>Node native addon (napi-rs)</td>
            </tr>
          </tbody>
        </table>
        <p className="note">
          Requires Google Chrome or Chromium, and Rust 1.85+ for the crate.
          Set <code>FLUXWRIGHT_CHROMIUM</code>, <code>CHROME</code>, or{" "}
          <code>CHROMIUM</code> if it is not on a standard path.
        </p>

        <h2 id="install">Install</h2>
        <h3>Rust (source)</h3>
        <pre>
          <code>{`git clone https://github.com/harshit-d3v/fluxwright
cd fluxwright
# Windows cmd:
set PATH=%USERPROFILE%\\.cargo\\bin;%PATH%
cargo test -p fluxwright --test integration -- --test-threads=1
cargo run -p fluxwright --example thousand_jobs`}</code>
        </pre>
        <h3>npm</h3>
        <pre>
          <code>{`npm install fluxwright
# first publish ships a prebuilt for your OS when CI artifacts exist.
# otherwise, from the repo:
cd bindings/node
npm install
npm run build`}</code>
        </pre>
        <h3>MCP binary</h3>
        <pre>
          <code>{`cargo install --path crates/fluxwright-mcp --force
# binary: fluxwright-mcp`}</code>
        </pre>

        <h2 id="rust">Rust</h2>
        <p>
          Each <code>acquire</code> is a <strong>fresh context</strong> on a
          reused browser. Context reuse is not the default. Drop the lease (or
          let it drop) and the context is disposed.
        </p>
        <pre>
          <code>{`use fluxwright::{BrowserEngine, JobOptions, Priority};

let engine = BrowserEngine::builder()
    .max_browsers(4)
    .max_contexts_per_browser(8)
    .memory_ceiling_mb(8192)
    .build()
    .await?;

let page = engine.acquire().await?;
page.goto("https://example.com").await?;
let title = page.title().await?;
drop(page);

let title = engine
    .run(JobOptions::default().priority(Priority::High), |page| async move {
        page.goto("https://example.com").await?;
        page.title().await
    })
    .await?;`}</code>
        </pre>
        <p>
          Page actions: <code>goto</code>, <code>title</code>,{" "}
          <code>content</code>, <code>click</code>, <code>fill</code>,{" "}
          <code>evaluate</code>, <code>screenshot</code>,{" "}
          <code>wait_for_selector</code>, CSS <code>locator</code>. Queue full
          mode is <code>Error</code> or <code>Wait</code>. Recycle by job count,
          age, or process-tree RSS.
        </p>
        <p>
          Visible Chrome: <code>.headless(false)</code>. Example:{" "}
          <code>cargo run -p fluxwright --example amazon_iphones</code> (or
          double-click <code>run-amazon-iphones.bat</code> on Windows).
        </p>

        <h2 id="npm">TypeScript / npm</h2>
        <p>
          The package is a small lease API, not Playwright.{" "}
          <code>chromium.launch</code> starts the in-process engine.
        </p>
        <pre>
          <code>{`import { chromium } from "fluxwright";

const browser = await chromium.launch({ maxBrowsers: 4 });
const page = await browser.newPage();
await page.goto("https://example.com");
console.log(await page.title());
await page.click("button.submit");
await page.fill("input[name=q]", "fluxwright");
console.log(await page.evaluate("document.body.innerText.slice(0, 200)"));
const png = await page.screenshot();
await page.close();
await browser.close();`}</code>
        </pre>
        <p className="note">
          Native addon (napi-rs). Chrome must be installed on the machine
          running Node. See <a href="#missing">what is omitted on purpose</a>.
        </p>

        <h2 id="cli">CLI</h2>
        <p>
          The daemon speaks a Unix socket, or{" "}
          <code>\\.\pipe\fluxwright</code> on Windows. Nothing listens on TCP
          by default.
        </p>
        <pre>
          <code>{`cargo run -p fluxwright-cli -- doctor
cargo run -p fluxwright-cli -- start --max-browsers 4 --max-contexts 8
cargo run -p fluxwright-cli -- stats
cargo run -p fluxwright-cli -- browsers
cargo run -p fluxwright-cli -- benchmark`}</code>
        </pre>

        <h2 id="mcp">MCP (Claude, Codex, Cursor)</h2>
        <p>
          <code>fluxwright-mcp</code> is a stdio server. The Chromium fleet
          runs <em>on the same machine</em> as the assistant. Serverless hosts
          cannot run this.
        </p>
        <p>
          Tools: <code>open</code>, <code>goto</code>, <code>title</code>,{" "}
          <code>content</code>, <code>click</code>, <code>fill</code>,{" "}
          <code>evaluate</code>, <code>screenshot</code>, <code>close</code>.
        </p>
        <h3>Claude Desktop</h3>
        <p>
          <code>%APPDATA%\Claude\claude_desktop_config.json</code>
        </p>
        <pre>
          <code>{`{
  "mcpServers": {
    "fluxwright": {
      "command": "C:\\\\Users\\\\YOU\\\\.cargo\\\\bin\\\\fluxwright-mcp.exe"
    }
  }
}`}</code>
        </pre>
        <h3>Codex</h3>
        <p>
          <code>%USERPROFILE%\.codex\config.toml</code>
        </p>
        <pre>
          <code>{`[mcp_servers.fluxwright]
command = "C:\\\\Users\\\\YOU\\\\.cargo\\\\bin\\\\fluxwright-mcp.exe"`}</code>
        </pre>
        <h3>Cursor</h3>
        <p>
          This repo already has <code>.cursor/mcp.json</code>. Reload MCP in
          Cursor settings.
        </p>
        <p className="callout">
          Chrome opens visible unless you set{" "}
          <code>FLUXWRIGHT_HEADLESS=1</code>. Remote deploy only works on a VM
          with Chromium (Fly, Railway, a VPS) plus a stdio→HTTP proxy — not on
          Vercel or Lambda.
        </p>

        <h2 id="bench">Benchmarks</h2>
        <p>
          Fluxwright does not claim to be faster or lighter than Playwright or
          Puppeteer. Run the harness yourself:
        </p>
        <pre>
          <code>{`cd benchmarks && npm install
cargo run -p fluxwright-benchmarks --release`}</code>
        </pre>
        <p>
          Results: <code>benchmarks/results/latest.json</code>. Same Chrome,
          fresh context per job. Playwright/Puppeteer in-flight contexts are
          capped at 24 on Windows so Chrome does not deadlock; Fluxwright uses
          the scenario slot count. Throughput (<code>jobs/s</code>) is the
          comparable figure; p95 is not like-for-like (Fluxwright includes
          queue wait from submit).
        </p>

        <h2 id="security">Security</h2>
        <p>
          Chromium’s sandbox stays <strong>on</strong> unless you opt in. Set{" "}
          <code>FLUXWRIGHT_NO_SANDBOX=1</code> only for CI images that cannot
          use user namespaces. The engine logs a warning. Do not expose the
          daemon on TCP. Leases are isolated contexts; cookies from one job are
          not visible in the next.
        </p>

        <h2 id="missing">Not Playwright</h2>
        <p>Intentionally absent from the page API:</p>
        <ul>
          <li>Firefox / WebKit</li>
          <li>Tracing, HAR, video</li>
          <li>Playwright Test, fixtures, expect</li>
          <li>
            <code>getByRole</code> / <code>getByText</code> (CSS locator only)
          </li>
          <li>Multiple pages per context, downloads, uploads</li>
          <li>
            <code>connectOverCDP</code> (planned as a later BrowserSource)
          </li>
        </ul>
        <hr />
        <p className="note">
          License MIT OR Apache-2.0. Source in this repository. npm package
          name: <span translate="no">fluxwright</span>.
        </p>
      </main>
    </div>
  );
}
