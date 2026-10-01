# Fluxwright

Run hundreds of headless Chrome jobs on one machine. Fluxwright pools browsers, queues jobs when every slot is busy, restarts browsers before they bloat, and retries jobs when a browser crashes, behind a Playwright-style API for Node and Rust.

It is not a Playwright rewrite. Playwright drives a browser; Fluxwright runs a fleet of them.

**Docs:** https://fluxwright.vercel.app · **Roadmap:** [ROADMAP.md](ROADMAP.md)

## Quick start (Node)

```bash
npm install fluxwright
```

```ts
import { chromium } from "fluxwright";

const browser = await chromium.launch({ maxBrowsers: 4 });
const page = await browser.newPage(); // a fresh context on a pooled Chrome
await page.goto("https://example.com");
console.log(await page.getByRole("heading").textContent()); // "Example Domain"
console.log(await page.evaluate(() => location.hostname));
await page.close(); // the context is destroyed and the slot is free again
await browser.close();
```

Fluxwright uses the Chrome on your machine. For headless jobs, install chrome-headless-shell: pages open 5-10x faster than on Chrome's new headless mode in our runs.

```bash
npx @puppeteer/browsers install chrome-headless-shell@stable
```

## What a page can do

| Area | API |
|---|---|
| Navigate | `goto(url, { waitUntil })` with `load` (default), `domcontentloaded`, `networkidle` or `commit`, event-driven like Playwright |
| Find | `getByRole(role, { name })`, `getByText`, `getByLabel`, `getByPlaceholder`, `getByTestId`, `locator(css \| "text=…" \| "role=…")`, `frameLocator(iframe)` for same- and cross-origin iframes. Narrow with `nth(i)`, `first()`, `last()`, `filter({ hasText })` and chained getBy… calls |
| Act | `click()` and `fill()` wait until the element is visible, enabled, stable and not covered; `waitFor()`, `waitForSelector()` |
| Read | `title()`, `content()`, `textContent()`, `evaluate(fn, arg)` or `evaluate("expression")`, `locator.evaluate((el, arg) => …)`, `locator.boundingBox()` |
| Capture | `screenshot({ fullPage, path })`, `locator.screenshot({ path })` (also inside scrolling containers and iframes), `setViewportSize({ width, height })` |
| Errors | `consoleMessages()` and `pageErrors()` from the page, its popups and iframes, so a job can fail on JavaScript errors |
| Emulate | `newPage({ userAgent, locale, timezoneId, geolocation, permissions, viewport, deviceScaleFactor, colorScheme })`, Playwright's option names |
| Sessions | `page.storageState({ path })` saves cookies and localStorage; `newPage({ storageState })` starts from them, so you log in once. Playwright's file format |
| Network | a proxy per page, with a username and password; blocking by resource type or URL (Rust `JobOptions`) |

The full Node API is in [bindings/node/README.md](bindings/node/README.md).

## What the fleet does

- **A fresh page per job:** every lease is a new browser context on a reused Chrome, so cookies, storage and cache never leak between jobs.
- **A queue with priorities:** when every slot is busy, jobs wait by priority, or fail fast if you would rather shed load.
- **A memory ceiling:** no new work starts while the fleet's real memory footprint is over budget (PSS on Linux, private bytes on Windows).
- **Recycling:** a browser restarts after a number of jobs, an age or a memory threshold, and finishes its running jobs first.
- **Crash recovery:** a dead browser fails its jobs at once and they retry on a healthy one. Chrome is tied to the engine process, so it never outlives it.
- **For AI agents:** an MCP server lets Claude, Codex or Cursor drive the fleet.

## When to use Playwright instead

- **Firefox or Safari (WebKit):** Fluxwright drives Chromium only.
- **End-to-end tests:** Playwright Test gives you a runner, `expect`, fixtures, a trace viewer and a recorder. Fluxwright is an engine, not a test framework.
- **Python, Java or .NET:** Fluxwright has Node and Rust today. Python is on the [roadmap](ROADMAP.md).
- **Request mocking (`page.route`), touch and mobile emulation, downloads:** not yet. See [MISSING.md](bindings/node/MISSING.md) and the roadmap.

Use Fluxwright when you run many browser jobs, such as scraping, crawling, PDF and screenshot rendering, or automation at volume, and want the fleet handled for you.

## Speed

Measured on one Windows machine (2026-09-30): both tools on the same chrome-headless-shell binary, alternating runs, 10 concurrent jobs loading a small local page. Fluxwright ran 54–70 jobs/s, Playwright 42–51. On Chrome's new headless mode the two were level (about 13 jobs/s each with 5 browsers). One machine and one page shape say little about yours, so measure your own workload with `cargo run --release -p fluxwright-benchmarks` (results go to `benchmarks/results/`).

## Rust

```toml
[dependencies]
fluxwright = { git = "https://github.com/harshit-d3v/fluxwright" }
```

```rust
use fluxwright::{BrowserEngine, JobOptions, Priority};

#[tokio::main]
async fn main() -> fluxwright::Result<()> {
    let engine = BrowserEngine::builder()
        .max_browsers(50)
        .max_contexts_per_browser(10)
        .memory_ceiling_mb(8192)
        .build()
        .await?;

    let page = engine.acquire().await?;
    page.goto("https://example.com").await?;
    println!("{}", page.title().await?);
    drop(page); // context destroyed, slot returned

    // run() queues, leases, retries after a crash, and always releases the page.
    let title = engine
        .run(JobOptions::default().priority(Priority::High).retries(2), |page| async move {
            page.goto("https://example.com").await?;
            page.title().await
        })
        .await?;
    println!("{title}");
    Ok(())
}
```

Requires Rust 1.85+ and Chrome or Chromium (found on the standard install paths, or set `FLUXWRIGHT_CHROMIUM`, `CHROME` or `CHROMIUM`). `--no-sandbox` is off unless you set `FLUXWRIGHT_NO_SANDBOX=1` (logs a warning).

| Crate | Role |
|-------|------|
| `fluxwright-cdp` | One connection per browser (DevTools pipe on Linux/macOS, WebSocket on Windows), flat sessions |
| `fluxwright-core` | Pool, scheduler, memory ceiling, recycling, metrics |
| `fluxwright` | Public API |
| `fluxwright-cli` | `fluxwright start` / `stats` / `browsers` / `doctor` / `benchmark` |
| `fluxwright-mcp` | MCP server: open, goto, click, fill, evaluate, screenshot, close |

## MCP server

```bash
cargo install --git https://github.com/harshit-d3v/fluxwright fluxwright-mcp
```

Then add `{ "mcpServers": { "fluxwright": { "command": "fluxwright-mcp" } } }` to your client's configuration (Windows: `install-mcp.bat`).

## CLI

```bash
cargo run -p fluxwright-cli -- start
cargo run -p fluxwright-cli -- stats
cargo run -p fluxwright-cli -- browsers
cargo run -p fluxwright-cli -- doctor
```

The control channel is a Unix socket, or `\\.\pipe\fluxwright` on Windows. Nothing listens on TCP.

## Benchmarks

```bash
cargo run --release -p fluxwright-benchmarks
# two-hour soak (memory over time): cargo run --release -p fluxwright-benchmarks -- --soak
```

The harness talks to `benchmark-server` (localhost only). Playwright and Puppeteer adapters run if their Node packages are installed.

## Development

- Site: `cd www && npm install && npm run dev` (http://localhost:3456). Vercel deploys `www/` from `main`.
- Node binding: `cd bindings/node && npm install && npm run build && npm test`.
- Rust tests need Chrome: `cargo test --workspace -- --test-threads=1`.
- Design notes: `docs/COMPETITIVE_ANALYSIS.md`, `docs/CDP_DECISION.md`, `docs/DECISIONS.md`.

## License

Licensed under the [Apache License, Version 2.0](LICENSE). Versions up to 0.2.0 were released under the MIT license.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this project shall be licensed under the Apache License, Version 2.0, without any additional terms or conditions.
