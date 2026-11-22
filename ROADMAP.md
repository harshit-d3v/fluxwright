# Roadmap

The goal is to be the best engine for running many browser jobs on your own machines. That is a narrower job than Playwright's, and the plan stays inside it: Playwright drives a browser, Fluxwright runs the fleet.

## How we measure "best"

- **Throughput:** jobs per second, and per GB of RAM, against Playwright and Puppeteer on the same Chrome binary. Published benchmarks, reproducible in CI.
- **Reliability:** no leaked Chrome processes, ever. A crashed browser is replaced and its jobs retried within seconds.
- **Easy to switch:** for the common page API, a Playwright job script runs after changing only the import.
- **Fast start:** from `npm install` to the first job in under two minutes on Windows, macOS and Linux.

## Done

- **0.2.2:** `page.evaluate(fn, arg)` as in Playwright; a page dropped without `close()` no longer crashes the process; valid TypeScript declarations.
- **0.3.0:** settings per page with Playwright's option names (user agent, locale, timezone, geolocation, permissions, viewport, device scale factor, color scheme); saved sessions (`page.storageState()` out, `newPage({ storageState })` in, in Playwright's file format); `getByLabel`, `getByPlaceholder`, `getByTestId`, `nth`, `first`, `last`, `filter({ hasText })` and chained locators; `locator.screenshot()`, `locator.boundingBox()`, `locator.evaluate()`; `page.consoleMessages()` and `page.pageErrors()`.
- **0.3.1:** `page.route` with Playwright's `Route` and `Request` (fulfill, continue with changes, abort, fallback; globs, RegExps and functions); `page.on('console' | 'pageerror')`; downloads with `page.waitForDownload()` and `saveAs`.
- **0.4.0:** Python, the biggest audience Fluxwright did not reach: the same API under Playwright's Python names, for asyncio (`fluxwright.async_api`) and blocking code (`fluxwright.sync_api`, whose objects work from any thread), with `async_playwright()` and `sync_playwright()` so Playwright scripts switch by changing the import. Wheels for Windows, macOS and Linux on PyPI. On npm, 0.4.0 saves storage state owner-only on Linux and macOS, and `page.route` no longer hangs when a URL function throws or when two first `route()` calls overlap.
- **0.4.1:** `newPage` / `new_page` waits for a free slot however long the queue is, so starting every job at once works; it used to fail past 256 waiting jobs or after 30 s. `queueTimeout` / `queue_timeout` fails after a set time instead.
- **0.5.0:** Playwright scripts need fewer changes: `browser.newContext()` / `new_context()` with `context.newPage()`; `page.on('request' | 'response' | 'requestfinished' | 'requestfailed')` with `response.body()` / `json()`, and `page.waitForResponse` / `waitForRequest` (`expect_response` / `expect_request` in Python); `page.on('dialog')` to accept or dismiss alerts, confirms and prompts. Clicks, boxes and screenshots land right inside iframes that CSS scales or rotates. Fixes a crash on Windows under heavy load (a stack overflow while measuring Chrome's memory). Both bindings are tested on Windows, macOS and Linux.
- **0.5.1 (PyPI):** a script that ended right after an awaited call could crash at exit on Linux (SIGSEGV, about 1 run in 16 on Python 3.10–3.13). It now exits cleanly.
- **0.6.0:** the fleet sizes itself: by default a browser per 4 CPUs, and a memory budget of 75% of the memory free at launch (inside a container, its limit), which `memoryCeilingMb` / `memory_ceiling_mb` overrides. Long runs stay healthy: a closed or recycled browser frees its profile folder, tasks and pipes; recycling a browser no longer pauses the pool; callers that gave up leave the queue; idle browsers close without new traffic; Chrome never outlives its engine. Text cut through an emoji no longer makes calls time out, a route reply Chrome rejects no longer leaves the request stuck, and route requests show lower-case header names. In the bindings, an unanswered `beforeunload` is accepted, routed pages and console listeners no longer keep closed pages alive, and `timeout: 0` waits as long as it takes. The MCP server opens web pages only and checks every request the page makes. Benchmarks run on GitHub's runner and time every tool the same way.
- **0.7.0:** HTTP server mode (`fluxwright serve`) with bearer auth, priorities, cancellation, Prometheus `/metrics`, W3C `traceparent` spans, SSE live dashboard, failure evidence, `--journal` restart persistence, `--max-concurrency-per-host` / `--min-delay-per-host`, experimental cluster mode (`fluxwright coordinate` + `fluxwright serve --join <url>` with coordinator-only lease/heartbeat/result endpoints and lease expiration requeue), and Docker image; `page.pdf()`, JPEG/WebP screenshots with `clip` and `omitBackground`; per-call `timeout` and `setDefaultTimeout` / `setDefaultNavigationTimeout`; `waitForSelector` / `waitFor` states (`attached`, `detached`, `visible`, `hidden`); `isMobile`, `hasTouch`, `extraHTTPHeaders`, `ignoreHTTPSErrors`, `javaScriptEnabled`, `offline`, `baseURL`, `bypassCSP`, and `httpCredentials`; `emulateMedia`, `addCookies` / `clearCookies`, `getByAltText` / `getByTitle`, and `chromium.connectOverCDP` across Rust, Node, and Python.
- **0.8.0:** Full Playwright-parity automation and fleet speed upgrades across Rust, Node, and Python: mouse/keyboard/touchscreen/wheel, `dragAndDrop`, locator content & state readers, `waitForURL` / `waitForFunction`, `addInitScript`, `exposeFunction` / `exposeBinding`, multi-tab `BrowserContext` + popups + imperative `Frame`, raw `newCDPSession`, `APIRequestContext` + `route.fetch()` + IndexedDB `storageState`, WebSockets + HAR 1.2 record/replay + dedicated Web Workers + generic `waitForEvent`; custom Chromium `args` / `ignoreDefaultArgs` / `channel`, pre-warmed context pool (`prewarmContexts`), `ResourcePreset` (`scraping` / `minimal` / `no-media`), `fluxwright install`, proactive coordinator lease-expiry reaper, `/v1/workers` drain/remove endpoints, and Cluster Workers dashboard view. Verified faster than Playwright across all 8 CI benchmark scenarios.
- **0.9.0:** Cloud browser gateway, AI agent snapshots, live HITL control, profiles/proxies/adaptive concurrency, and cluster hardening: pooled CDP WebSocket gateway (`GET /cdp` and `GET /json/version` on `fluxwright serve` with optional `--allow-query-token`), accessibility tree snapshots (`@eN` interactive element refs + `snapshot()`/`click("@eN")`/`fill("@eN")`) and clean Markdown extraction (`markdown()`), upgraded 21-tool `fluxwright-mcp` server, live JPEG screencast viewer + Human-in-the-Loop input/navigation/pause/resume controls in `/dashboard` (`pause_for_human`), named persistent profiles (`--profiles-dir`, `/v1/profiles`), outbound proxy session pool (`--proxy`, `/v1/proxies`) with sticky affinity and exponential failure cooldown, AIMD adaptive concurrency limiting (`--adaptive-concurrency`, `fluxwright_effective_concurrency_limit`), high-saturation batch-dispatch p95 latency tuning, separate `FLUXWRIGHT_WORKER_TOKEN` + native `https://` coordinator TLS on `--join`, Linux ARM64 native releases (`node`, `python`, CLI/MCP binaries), and `crates.io` release workflow.

## Next: the fleet at scale

Implement these milestones in order, verifying each before starting the next:

1. [x] **Server mode:** `fluxwright serve` exposes authenticated HTTP job submission, status/results, cancellation, bounded admission/output, priorities and total deadlines. Protocol and browser-backed tests verified. See [docs/SERVER.md](docs/SERVER.md).
2. [x] **Observability:** a Prometheus endpoint, W3C `traceparent` propagation with `tracing` spans per job and step (visible through any tracing subscriber; no OpenTelemetry SDK export yet), and a live fleet dashboard with real data.
3. [x] **Failure evidence:** per-job bounded network log, console output, and a screenshot on failure.
4. [x] **Bring your own browsers:** `connectOverCDP` across Rust, Node, Python, and `fluxwright serve --cdp-endpoint`.
5. [x] **Docker image** with Chrome and Fluxwright.
6. [x] **Per-host limits:** a cap on the pages on one host at once and a minimum delay between navigations to it, across the fleet.
7. [x] **Jobs that survive a restart:** `fluxwright serve --journal <file>` queues unfinished jobs again on start.

## Later: cluster hardening

Experimental cluster mode (`fluxwright coordinate` + `fluxwright serve --join <url>`) is in [docs/CLUSTER.md](docs/CLUSTER.md). Separate `FLUXWRIGHT_WORKER_TOKEN`, native `https://` TLS on `--join`, and the Cluster Workers dashboard table shipped in `0.9.0`; remaining items are SQLite store and coordinator-wide per-host limits.

## Distribution and trust (ongoing)

- Publish the crates on crates.io.
- Prebuilt MCP and CLI binaries workflow configured in `.github/workflows/release-binaries.yml`.
- Node binding tests and type checks in CI, on all three operating systems.
- CONTRIBUTING.md, issue templates and good first issues.

## Not planned

- **Firefox and WebKit:** the fleet features are built on Chrome's DevTools Protocol. We may revisit this through WebDriver BiDi if people ask.
- **A test runner, `expect`, a trace viewer or a recorder:** Playwright Test does this well.
- **Anti-bot evasion.**
