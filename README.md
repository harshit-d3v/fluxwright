# Fluxwright

Run hundreds of headless Chrome jobs on one machine. Fluxwright pools browsers, queues jobs when every slot is busy, restarts browsers before they bloat, and retries jobs when a browser crashes, behind a Playwright-style API for Node, Python and Rust.

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

## Quick start (Python)

```bash
pip install fluxwright
```

```python
import asyncio
from fluxwright.async_api import async_playwright

async def main():
    async with async_playwright() as p:
        browser = await p.chromium.launch(max_browsers=4)
        page = await browser.new_page()
        await page.goto("https://example.com")
        print(await page.get_by_role("heading").text_content())  # "Example Domain"
        await browser.close()

asyncio.run(main())
```

Blocking code uses `fluxwright.sync_api` and `sync_playwright()` the same way. The names are Playwright's, so a Playwright script for these features runs after changing its import. In Node, an option Fluxwright does not implement, such as a click's `noWaitAfter`, throws a TypeError instead of being ignored (0.6.0 ignored unimplemented options), so such a script fails loudly at the call rather than quietly doing something else.

## What a page can do

| Area | API |
|---|---|
| Navigate | `goto(url, { waitUntil })` with `load` (default), `domcontentloaded`, `networkidle` or `commit`, event-driven like Playwright |
| Find | `getByRole(role, { name })`, `getByText`, `getByLabel`, `getByPlaceholder`, `getByAltText`, `getByTitle`, `getByTestId`, `locator(css \| "text=…" \| "role=…")` (pierces open Shadow DOM by default; `css:light=` for light DOM), `frameLocator(iframe)` for same- and cross-origin iframes. Narrow with `nth(i)`, `first()`, `last()`, `filter({ hasText })` and chained getBy… calls |
| Act | `click()`, `dblclick()`, `hover()`, `tap()`, `focus()`, `blur()`, `scrollIntoViewIfNeeded()`, `dragAndDrop()`, `fill()`, `clear()`, `press()`, `pressSequentially()`, `check()`/`uncheck()`, `selectOption()`, `setInputFiles()`, `page.mouse`/`keyboard`/`touchscreen`, `waitFor()`, `waitForSelector()` |
| Read | `title()`, `content()`, `textContent()`, `evaluate(fn, arg)` or `evaluate("expression")`, `locator.evaluate((el, arg) => …)`, `locator.boundingBox()` |
| Capture | `screenshot({ fullPage, path, type, quality, clip, omitBackground, mask, maskColor, animations, caret, annotate })` in PNG, JPEG, or WebP, `locator.screenshot(...)` (also inside scrolling containers and iframes), `pdf({ format, margin, printBackground, ... })` with Playwright's options (headless), `setViewportSize({ width, height })` |
| Errors | `consoleMessages()` and `pageErrors()` from the page, its popups and iframes, so a job can fail on JavaScript errors; `page.on('console' \| 'pageerror', handler)` as they happen |
| Requests | `page.route(url, handler, { times })` to fulfill, change or abort requests (`route.fulfill({ json })`, `route.continue({ headers })`, `route.abort()`, `route.fallback(overrides)`), with globs, RegExps or functions |
| Clock | `page.clock` (`install`, `setFixedTime`, `setSystemTime`, `pauseAt`, `fastForward`, `runFor`, `resume`) for deterministic timers and `Date` mocking |
| Downloads | `page.waitForDownload()` then `download.saveAs(path)`; each job's files stay in its own folder until the page closes |
| Emulate | `newPage({ userAgent, locale, timezoneId, geolocation, permissions, viewport, deviceScaleFactor, colorScheme, isMobile, hasTouch, extraHTTPHeaders, ignoreHTTPSErrors, javaScriptEnabled, offline, baseURL, bypassCSP, httpCredentials })`, `emulateMedia({ media, colorScheme })`, Playwright's option names |
| Sessions | `page.storageState({ path })` saves cookies and localStorage; `newPage({ storageState })` starts from them, `context.addCookies()` / `clearCookies()`, so you log in once. Playwright's file format |
| Network | a proxy per page, with a username and password; blocking by resource type or URL (Rust `JobOptions`) |
| AI | `ariaSnapshot()` / `snapshot({ diff })` for compact (or diff-only) ARIA trees with `@eN` interactive element refs, `screenshot({ annotate: true })` Set-of-Marks overlays, and `markdown()` for clean HTML-to-Markdown extraction |
| E2E testing | Auto-retrying `expect()`, Playwright-compatible `.zip` `context.tracing`, `recordVideo` / `page.video()`, visual `toHaveScreenshot()`, `fluxwright/test` (`fluxwright-test` CLI), and built-in `pytest` plugin |

The full Node API is in [bindings/node/README.md](bindings/node/README.md) and the Python API, with the same features under Playwright's Python names (`get_by_role`, `wait_until`, `storage_state`, ...), in [bindings/python/README.md](bindings/python/README.md).

## End-to-end testing

Fluxwright ships E2E testing batteries across Rust, Node/TypeScript, and Python on top of the pooled Chrome fleet:

- **Auto-retrying `expect()` assertions:** web-first page and locator matchers (`toBeVisible`, `toHaveText`, `toContainText`, `toHaveValue`, `toHaveAttribute`, `toHaveClass`, `toHaveCount`, `toHaveTitle`, `toHaveURL`, `.not`, custom timeouts) across Rust (`expect_page` / `expect_locator`), Node/TS (`expect`), and Python (`expect` and `expect.soft`).
- **Playwright-compatible `.zip` tracing:** `context.tracing.start()` / `startChunk()` / `stopChunk()` / `stop({ path })` records actions, screencast frames, bounded DOM `frame-snapshot` events, console logs, and network events into `.zip` archives that open in Playwright Trace Viewer (`npx playwright show-trace trace.zip`).
- **Video recording & visual regression:** `recordVideo` (`record_video_dir` in Python) + `page.video()` records WebM (via `ffmpeg`) or MJPEG-in-AVI fallback; `expect(page | locator).toHaveScreenshot()` (`to_have_screenshot`) runs auto-retrying PNG pixel diffs with animation freezing and `FLUXWRIGHT_UPDATE_SNAPSHOTS=1`.
- **Built-in test runners:** Node ships `fluxwright/test` (`test`, `expect`, `defineConfig`, `createFluxwrightFixtures` for `node:test`/Vitest) and the `fluxwright-test` CLI (parallel `--workers`, `--shard`, `--retries`, `--grep`, `--trace`, `--video`, `--screenshot`, HTML/JSON/list reporters). Python auto-loads a built-in `pytest` plugin (`pytest11`) providing `playwright`, `browser`, `context`, and `page` fixtures with failure artifacts.

```ts
// example.spec.mjs — run with: npx fluxwright-test --workers=4 --retries=1 --reporter=list,html
import { test, expect } from "fluxwright/test";

test.use({ trace: "retain-on-failure", video: "retain-on-failure" });

test("homepage renders", async ({ page }) => {
  await page.goto("https://example.com");
  await expect(page).toHaveTitle(/Example/);
  await expect(page.getByRole("heading")).toHaveText("Example Domain");
  await expect(page).toHaveScreenshot("home.png");
});
```

```python
# test_example.py — run with: pytest --fluxwright-tracing=retain-on-failure
from fluxwright.sync_api import Page, expect

def test_homepage(page: Page) -> None:
    page.goto("https://example.com")
    expect(page).to_have_title("Example Domain")
    expect(page.get_by_role("heading")).to_have_text("Example Domain")
```

## What the fleet does

- **A fresh page per job:** every lease is a new browser context on a reused Chrome, so cookies, storage and cache never leak between jobs.
- **A queue with priorities:** when every slot is busy, jobs wait by priority, or fail fast if you would rather shed load.
- **A memory ceiling:** no new work starts while the fleet's real memory footprint is over budget (PSS on Linux, private bytes on Windows).
- **Recycling:** a browser restarts after a number of jobs, an age or a memory threshold, and finishes its running jobs first.
- **Polite crawling:** a cap on the pages on one host at once and a minimum delay between navigations to it, kept across the whole fleet (`maxConcurrencyPerHost`, `minDelayPerHost`).
- **Crash recovery:** a dead browser fails its jobs at once and they retry on a healthy one. Chrome is tied to the engine process, so it never outlives it.
- **Adaptive fleet control:** AIMD concurrency scaling under memory pressure or failures (`--adaptive-concurrency`), named persistent profiles (`--profiles-dir`), rotating proxy sessions with exponential cooldown (`--proxy`), pooled CDP WebSocket gateway (`GET /cdp`), signed job webhooks, OTLP/HTTP JSON traces (`--otlp-endpoint`), and live `/dashboard` screencasts with embeddable Live View URLs and Human-in-the-Loop controls.
- **For AI agents:** a 22-tool MCP server (`fluxwright-mcp`, stdio or Streamable HTTP `--http <ADDR>` with bearer auth and isolated per-session contexts), `@eN` accessibility tree snapshots (`snapshot` with `{ diff: true }`), Set-of-Marks annotated screenshots (`screenshot({ annotate: true })`), and clean Markdown extraction (`markdown`) let Claude, Codex or Cursor drive the fleet.

## When to use Playwright instead

- **Firefox or Safari (WebKit):** Fluxwright drives Chromium only.
- **Interactive GUI codegen or a hosted Trace Viewer UI:** Fluxwright includes `expect()`, `.zip` tracing (inspect with Playwright Trace Viewer), video recording, visual regression, `fluxwright/test`, and a `pytest` plugin, but has no interactive GUI recorder yet.
- **Java or .NET:** Fluxwright has Node, Python and Rust.

Use Fluxwright when you run many browser jobs — scraping, crawling, PDF and screenshot rendering, AI agent browsing, or parallel Chromium E2E test suites — and want the fleet handled for you.

## Speed

Measured on GitHub's ubuntu-latest runner (4 vCPU, 2026-11-23, run `45227204313`): every tool on the same chrome-headless-shell 158.0.8127.36 binary with fresh isolated contexts per job. Fluxwright beat Playwright and Puppeteer in all 8 scenarios with 0.0% failures and 0 leaked processes: `1-browser-10-pages` **42.8 vs 36.7** jobs/s (-13.3% peak RAM), `10-concurrent-browsers` **50.7 vs 34.6** jobs/s (-37.3% peak RAM), `100-concurrent` **50.9 vs 36.6** jobs/s (-24.2% peak RAM), `500-concurrent` **48.5 vs 36.7** jobs/s (-18.8% peak RAM), `1000-queued` **47.3 vs 41.7** jobs/s, `100-pdfs` **52.0 vs 41.6** jobs/s, `scraping-200` (with `ResourcePreset::Scraping`) **38.2 vs 31.0** jobs/s, and `chaos-1000-queued` **49.2 jobs/s with 0.0% failures** across 9 browser kills (vs 69.9% failures on Playwright and Puppeteer). A shared cloud machine varies from run to run, so rerun it (Actions → benchmarks) and measure your own workload with `cargo run --release -p fluxwright-benchmarks` (results go to `benchmarks/results/`).

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

Requires Rust 1.95+ and Chrome or Chromium (found on the standard install paths, or set `FLUXWRIGHT_CHROMIUM`, `CHROME` or `CHROMIUM`). `--no-sandbox` is off unless you set `FLUXWRIGHT_NO_SANDBOX=1` (logs a warning).

| Crate | Role |
|-------|------|
| `fluxwright-cdp` | One connection per browser (DevTools pipe on Linux/macOS, WebSocket on Windows), flat sessions |
| `fluxwright-core` | Pool, scheduler, memory ceiling, recycling, metrics |
| `fluxwright` | Public API |
| `fluxwright-cli` | `fluxwright start` / `stats` / `browsers` / `doctor` / `install` / `benchmark` / `serve` / `coordinate` |
| `fluxwright-mcp` | 22-tool MCP server (`open`, `goto`, `back`, `forward`, `tabs`, `close`, `snapshot` with `@eN` refs, `markdown`, `title`, `content`, `click`, `hover`, `fill`, `press`, `select`, `scroll`, `wait_for`, `evaluate`, `console`, `network`, `screenshot`, `export_api_calls`) |
| `fluxwright-server` | Authenticated HTTP job API over one local or remote CDP fleet |

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

The `start` / `stats` / `browsers` control channel is a Unix socket, or `\\.\pipe\fluxwright` on Windows. Those commands do not listen on TCP.

### HTTP server

`fluxwright serve` owns a fleet and accepts both one-shot synchronous scrapes & site discovery (`POST /v1/scrape` returning `markdown`, `html`, `text`, `snapshot`, `screenshot`, and `pdf`; `POST /v1/map` for rendered link discovery), async JSON browser jobs (`POST /v1/jobs`, `GET /v1/jobs/{id}`, `DELETE /v1/jobs/{id}`), and TTL-backed Managed Cloud Sessions (`POST /v1/sessions`, reconnectable `GET /v1/sessions/{id}/cdp`, searchable `GET /v1/sessions/{id}/replay`, HAR/API `GET /v1/sessions/{id}/network`, paired signed Live Views, and opt-in BYOB `cdp_endpoint` via `--allow-session-cdp-endpoint`, plus `block_cookie_banners`). It defaults
to `127.0.0.1:7331` and requires `FLUXWRIGHT_SERVER_TOKEN` (a random token of 32–256 printable
ASCII characters). Pass `--cdp-endpoint <url>` to connect to an existing browser over CDP instead
of launching local Chrome (one connection; set concurrency with `--max-contexts`). This is a
trusted-operator API, not a public multi-tenant service.

See [the HTTP server guide](docs/SERVER.md) for startup commands, authentication, job steps,
limits, and deployment precautions, and [agent framework adapters](examples/adapters/README.md) for Browser-Use, Stagehand, and Vercel AI SDK starters.

#### Docker quickstart

Run the self-contained multi-arch (`linux/amd64` + `linux/arm64`) image from GHCR (Debian Bookworm slim + Chromium + `tini` + `fluxwright`):

```bash
docker pull ghcr.io/harshit-d3v/fluxwright:latest
export FLUXWRIGHT_SERVER_TOKEN="$(python -c 'import secrets; print(secrets.token_hex(32))')"
docker run --rm \
  -e FLUXWRIGHT_SERVER_TOKEN="$FLUXWRIGHT_SERVER_TOKEN" \
  -p 127.0.0.1:7331:7331 \
  --shm-size=1g \
  --memory=4g --cpus=4 \
  --cap-add=SYS_ADMIN \
  ghcr.io/harshit-d3v/fluxwright:latest
```

Fluxwright automatically detects container `--memory` and `--cpus` limits via cgroups. See [Docker in `docs/SERVER.md`](docs/SERVER.md#docker) for sandbox configuration, custom flags, and security guidance.

## Benchmarks

```bash
cargo run --release -p fluxwright-benchmarks
# two-hour soak (memory over time): cargo run --release -p fluxwright-benchmarks -- --soak
```

The harness talks to `benchmark-server` (localhost only). Playwright and Puppeteer adapters run if their Node packages are installed.

## Development

- Site: `cd www && npm install && npm run dev` (http://localhost:3456). Vercel deploys `www/` from `main`.
- Node binding: `cd bindings/node && npm install && npm run build && npm test`.
- Python binding: `cd bindings/python && pip install maturin pytest && maturin develop && pytest` (in a virtualenv).
- Rust tests need Chrome: `cargo test --workspace -- --test-threads=1`.
- Design notes: `docs/COMPETITIVE_ANALYSIS.md`, `docs/CDP_DECISION.md`, `docs/DECISIONS.md`.

## License

Licensed under the [Apache License, Version 2.0](LICENSE). Versions up to 0.2.0 were released under the MIT license.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this project shall be licensed under the Apache License, Version 2.0, without any additional terms or conditions.
