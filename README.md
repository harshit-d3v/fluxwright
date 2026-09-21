# Fluxwright

A Rust engine for **high-concurrency Chromium automation**: pooling, admission control, recycling, crash recovery, and metrics.

It is not a Playwright rewrite. Client-side overhead (Node driver, idle RAM) is a rounding error next to Chromium. Fluxwright’s job is fleet management.

No speed or memory-advantage claims appear here. Run `cargo run -p fluxwright-benchmarks` and read `benchmarks/results/`.

**Docs site:** https://fluxwright.vercel.app — source in `www/` (`cd www && npm install && npm run dev` → http://localhost:3456).

**CI/CD:** GitHub Actions runs Rust tests and builds the site on every push. Vercel deploys `www/` on `main` (production) and on pull requests (preview). Add a custom domain later in the Vercel project (`fluxwright`); `fluxwright.com` is taken, `fluxwright.dev` / `fluxwright.io` / `fluxwright.app` are free to register.

**npm:** `bindings/node` — `npm install fluxwright` after publish. Local: `cd bindings/node && npm run build`.

**MCP:** `crates/fluxwright-mcp` — `install-mcp.bat`.

## Require

- Rust 1.85+
- Google Chrome or Chromium (`FLUXWRIGHT_CHROMIUM`, `CHROME`, or `CHROMIUM`)

`--no-sandbox` is off unless you set `FLUXWRIGHT_NO_SANDBOX=1` (logs a warning).

## Example

```rust
use fluxwright::{BrowserEngine, JobOptions, Priority};

# async fn demo() -> fluxwright::Result<()> {
let engine = BrowserEngine::builder()
    .max_browsers(50)
    .max_contexts_per_browser(10)
    .memory_ceiling_mb(8192)
    .build()
    .await?;

let page = engine.acquire().await?;
page.goto("https://example.com").await?;
let title = page.title().await?;
drop(page); // context destroyed, slot returned

let title = engine
    .run(JobOptions::default().priority(Priority::High), |page| async move {
        page.goto("https://example.com").await?;
        page.title().await
    })
    .await?;
# let _ = title;
# Ok(())
# }
```

Each lease is a **fresh browser context** on a reused browser. Context reuse is not the default.

## Crates

| Crate | Role |
|-------|------|
| `fluxwright-cdp` | One WebSocket per browser, flat sessions |
| `fluxwright-core` | Pool, scheduler, RSS ceiling, recycle, metrics |
| `fluxwright` | Public API |
| `fluxwright-cli` | `fluxwright start` / `stats` / `browsers` / `doctor` / `benchmark` |

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
cargo run -p fluxwright-benchmarks
# two-hour soak (RSS over time): cargo run -p fluxwright-benchmarks -- --soak
```

The harness talks to `benchmark-server` (localhost only). Playwright / Puppeteer adapters run if Node packages are installed; Kitewright is skipped (MCP server, not a library). Results go to `benchmarks/results/`.

## TypeScript

`bindings/node` is a napi-rs addon over the lease API. See `bindings/node/MISSING.md` for Playwright methods that are intentionally absent.

```bash
cd bindings/node && npm install && npm run build
```

```ts
import { chromium } from "fluxwright";
const browser = await chromium.launch({ maxBrowsers: 50 });
const page = await browser.newPage();
await page.goto("https://example.com");
console.log(await page.title());
await browser.close();
```

## Docs

- `docs/COMPETITIVE_ANALYSIS.md`
- `docs/CDP_DECISION.md`
- `docs/DECISIONS.md`

## License

MIT OR Apache-2.0
