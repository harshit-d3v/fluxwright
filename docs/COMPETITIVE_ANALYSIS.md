# Competitive analysis

Researched 2026-09-21; updated 2026-11-27 against 2026 competitor repositories, `v0.14.0` capabilities, and GitHub Actions CI runs (`45939361663` and `45825257140`). Fluxwright is a 3-tier fleet engine (HTTP fast path, optional light-CDP, pooled Chromium), not just a Playwright rewrite. This document judges alternatives on pooling, admission control, recycling, crash recovery, licensing, and reproducible CI metrics.

**How to read this.** Each section splits **claimed** (the project’s own words) from **verified** (read from source, repo APIs, docs, or CI benchmark artifacts). Unverified marketing numbers and unverified third-party pricing tiers are not treated as facts.

---

## 1. Pooling and fleet tools

These are the real competition.

### 1.1 Browserless

**What it solves.** A hosted or self-hosted browser *service*: clients connect over WebSocket (CDP or Playwright’s native protocol) and get a Chromium (or Chrome / Firefox / WebKit via Playwright paths) that Browserless launched and will tear down. The product is infrastructure, not a library API.

**Concurrency architecture (verified).** `Limiter` in `src/limiter.ts` is a queue with:

- `CONCURRENT` — max running jobs
- `QUEUED` — max waiting jobs
- `TIMEOUT` — job timeout

When the queue is full and `QUEUED=0`, new work is rejected with HTTP 429. Default `QUEUED` is 10. Enterprise docs recommend queue ≈ 1.5–2× concurrent. `BrowserManager` (`src/browsers/index.ts`) tracks each Chrome/CDP instance, its WebSocket endpoint, connections, TTL / keep-alive timers, crashes, and `user-data-dir` cleanup. `/pressure` is the health/load signal used for autoscaling.

**Browser lifecycle (verified).** A connection typically gets a browser (or reconnects to one). `Browserless.reconnect` can keep a remote browser alive across disconnects for a TTL. Session replay, captcha, and proxy features sit on top. Enterprise docs tell operators to *reuse contexts/pages* for efficiency and to scale **out** (more containers) rather than raise `CONCURRENT` on one box.

**Memory behaviour (verified from docs, not our measurement).**

- Sizing guide (Enterprise Docker): ~2 CPU / 4 GB for 5–10 concurrent; 8+ CPU / 16 GB+ for 20–50.
- `HEALTH=true` plus `MAX_MEMORY_PERCENT` / `MAX_CPU_PERCENT` **rejects new sessions** when the *container* is hot. In-flight sessions keep running. Docs warn the OS OOM killer can still kill the container.
- `--shm-size=2g` is recommended; they advise against `--disable-dev-shm-usage`.
- There is no documented measurement of Chromium **process-tree RSS** as an admission signal. Memory is container-level, not “sum of browser trees vs a ceiling”.

**API limitations.** You bring Puppeteer/Playwright (or raw CDP). Browserless does not give you a lease type, a Rust API, or in-process bindings. Isolation is whatever the client creates (new page vs new context vs new browser). Reusing pages/contexts, which their pooling advice encourages, shares cookies and storage.

**Benchmark methodology.** Public material is architectural (“1,000+ sessions”) and product-oriented. We did not find a published, reproducible suite that compares Browserless to puppeteer-cluster / Crawlee on a local deterministic server with median + spread.

**Documented weaknesses.**

- One worker’s Chrome processes contend for CPU and RAM; they tell you not to keep raising `CONCURRENT`.
- Health thresholds do not stop a running session from OOMing the box.
- Connection reuse vs job isolation is left to the caller.
- Open-source core is a Node service; the fleet is a process around Chrome, not a library inside your job runtime.

**Claimed vs verified.** “High-concurrency browser pools” and managed isolation are product claims. Verified: the limiter, 429 backpressure, TTL, crash bookkeeping, `/pressure`, container-level health gates.

---

### 1.2 puppeteer-cluster

**What it solves.** An in-process Node library that turns Puppeteer into a worker pool: queue URLs/jobs, run a task function with a `page`, retry on error, restart Chromium after a crash.

**Concurrency architecture (verified from README + `src/Cluster.ts`).** Three models:

| Mode | Isolation | Process model |
|------|-----------|----------------|
| `CONCURRENCY_PAGE` | None — cookies/storage shared | Pages on one (or few) browsers |
| `CONCURRENCY_CONTEXT` (default) | Incognito context per job | Contexts on shared browser(s) |
| `CONCURRENCY_BROWSER` | Context + own browser process | One Chromium per worker |

`maxConcurrency` is the worker count, **not** “tabs per browser”. There is no built-in “max contexts per browser” placement. Workers are launched as needed; `workerCreationDelay` staggers startups. The job queue is **unbounded**. `timeout` applies after a job *starts*, not while it sits in the queue (`execute` waiters can outlive the caller). `retryLimit` / `retryDelay` apply to `queue()`, not `execute()`.

**Browser lifecycle (verified).** Crash → worker repair / relaunch. Close path shuts workers then the concurrency implementation. Recycle-after-N-jobs and recycle-on-RSS are **not in the library**. PR #310 (`requestRestart`) was proposed because Chrome RSS grows over long runs; it was not the documented public API. Third-party posts invent `maxTasksPerWorker` — that option is not in the upstream README.

**Memory behaviour.** No admission control. Issue #3 (hang at `maxConcurrency > 50` under CONTEXT) was a timeout-vs-slow-launch accounting bug: a worker counted as open but unused, which blocked repair. Author’s own note on a 4-core machine: roughly 10 contexts before CPU sat at 100%. CONTEXT is cheaper than BROWSER; BROWSER isolates crashes at much higher RAM/CPU.

**API limitations.** Puppeteer-only. No priority queue. No `QueueFull`. No process-tree RSS. Monitor mode prints cluster-wide stats (workers, errors, some host RSS) — not per-browser tree RSS, reuse rate, or “memory per job”.

**Benchmark methodology.** None shipped as a fair multi-tool harness. Examples are crawl demos.

**Documented weaknesses.**

- Unbounded queue; no wait-vs-error backpressure.
- No drain/recycle policy, so long soaks leak until the operator restarts the process.
- CONTEXT vs BROWSER is a hard trade-off the library does not manage for you (crash isolation vs RAM).
- Hung-launch accounting has bitten people at modest concurrency.

**Claimed vs verified.** Claims: reuse Chromium, restart on crash, retry, three isolation modes. Verified in source. Claims of automatic memory hygiene are **not** verified — recycle-on-RSS is missing.

---

### 1.3 Crawlee browser pool

**What it solves.** Apify’s crawler toolkit includes `@crawlee/browser-pool`: launch many browsers, hand out pages, retire bloated browsers, close idle ones. Plugins wrap Playwright and Puppeteer.

**Concurrency architecture (verified from `BrowserPoolOptions` and `packages/browser-pool`).**

- `maxOpenPagesPerBrowser` default **20** — when full, launch another browser.
- Round-robin `newPage()` across browsers; `newPageWithEachPlugin` for multi-engine.
- `operationTimeoutSecs` default **15** — launch/new-page can get stuck; they bound it.
- Remote pool (`RemoteBrowserPool`) caps `maxOpenBrowsers` and can wait for a slot (unlike puppeteer-cluster’s unbounded queue).

**Browser lifecycle (verified).** Three states: active, inactive, closed. **Retire** means: no new pages, existing pages finish, then close (graceful drain). Stuck retired browsers are killed after a timeout.

- `retireBrowserAfterPageCount` default **100**
- Inactive browsers retired / closed on intervals (`retireInactiveBrowserAfterSecs`, `closeInactiveBrowserAfterSecs`)
- Session pool can retire a browser sooner

**Isolation (important).** Default `useIncognitoPages: false` — **pages share one context**. Cookies, storage, and service workers leak across jobs unless you opt into incognito/context-per-page. Playwright remote connections force incognito because `connect` / `connectOverCDP` have no persistent context.

**Memory behaviour.** Recycle is **page-count and idle time**, not RSS and not Chromium process-tree. No memory ceiling for admission. Defaults assume crawlers, not a hard RAM budget.

**API limitations.** Tied to Crawlee/Playwright/Puppeteer. Not a generic lease. No priority levels. Metrics are crawler-oriented (requests, sessions), not engine RSS vs browser-tree RSS.

**Benchmark methodology.** Crawlee publishes crawler throughput anecdotes. We did not find a like-for-like local-server comparison against puppeteer-cluster with isolation held constant.

**Documented weaknesses.**

- Shared-context default is a footgun for untrusted multi-tenant jobs.
- Retirement is heuristic (100 pages), not “this process tree is 2 GB”.
- Still Node + Playwright/Puppeteer; Chromium is the same cost as everyone else.

**Claimed vs verified.** “Browsers tend to get bloated after processing a lot of pages” — documented rationale, not a measured curve in the API docs. Drain-on-retire is verified in the README.

---

### 1.4 Selenium Grid 4

**What it solves.** Distributed WebDriver: Router → New Session Queue → Distributor → Node slots. Built for **test** farms (many browsers, many machines), not for packing hundreds of isolated jobs onto reused Chromium processes.

**Concurrency architecture (verified from Selenium docs).**

- New session requests are **FIFO**. Timeout default 300 s on the queue (`--session-request-timeout`).
- Distributor matches capabilities to Node slots. Default: **one Chromium/Firefox slot per CPU**; Safari one slot.
- If no slot, the request is retried (`--session-retry-interval`) until it times out.
- `--sessionqueue-batch-size` (default 20) is how many queued requests the distributor consumes at once.

**Browser lifecycle (verified).** A session occupies a slot until `DELETE /session/{id}`, inactivity (`--session-timeout`, default 300 s), or node drain. `--drain-after-session-count` stops accepting work after N sessions, then the node exits — Grid’s recycle analogue, at **node** granularity, not “replace this Chrome after 80 jobs while others keep serving”.

**Memory behaviour.** Isolation is typically **one browser process per session**. That is crash-safe and state-safe, and it is the expensive model Fluxwright is trying not to use as the default. Grid does not admit work based on process-tree RSS.

**API limitations.** WebDriver HTTP (plus BiDi in newer stacks), not CDP session multiplexing. No first-class browser *context* pool. Client libraries (Java, Python, JS, …) still speak WebDriver. Locators and auto-wait are poorer than Playwright unless you add layers.

**Benchmark methodology.** Grid is measured as test-infra (session create time, queue wait, node utilization). Not a scraping-jobs/sec suite.

**Documented weaknesses.**

- Slot = browser, so concurrency costs a full process each time.
- Queue is FIFO only — no job priority.
- Idle session timeout kills long think-time jobs unless retuned.
- Wrong tool if the bottleneck is “reuse Chrome, isolate with contexts, bound RAM”.

**Claimed vs verified.** Component diagram and CLI flags are verified from selenium.dev. Horizontal scale is real; context-level density is not what Grid optimizes.

---

## 2. Rust projects

### 2.1 Rustwright (Skyvern)

**What it solves (claimed).** Playwright-shaped API (Python, Node, and other bindings) on an in-process Rust CDP engine. No Node driver for Python. Chromium only. Alpha.

**Concurrency architecture (verified from their docs).** The engine is Tokio CDP (WebSocket, optional Unix-pipe). Python **async currently wraps the sync engine on threads**; they warn against more than ~**25 concurrent workflows per process**. Native async is on the roadmap (some README checkboxes say a native async engine has shipped — treat that as in flux; `LIMITATIONS.md` still states the thread-wrap limit). There is **no browser pool, no admission control, no recycle policy**. You `launch` a browser and create pages/contexts yourself.

**Browser lifecycle.** Standard Playwright shape: launch, context, page, close. Remote Chromium via `connect_over_cdp` only (not Playwright `run-server` wire). OOPIF auto-attach is described as new, with residual gaps (`JSHandle` in non-main frames, drag/screenshot/bbox).

**Memory behaviour (their numbers, not ours).** README headline: **2.55× faster** and **70% less memory**. Same README later: those are **local diagnostics, not capped-CI**; the 70% is **client** RSS (Python+Node driver vs Rust, 133.5 MiB vs 40.6 MiB on a form-fill). They explicitly say **Chromium-dominated whole-process memory is roughly equal**. That matches Fluxwright’s premise: beating the driver is a rounding error at fleet scale.

**API limitations (verified `LIMITATIONS.md`).** Chromium only; incomplete behavioural parity; Node surface is a subset; stealth is partial (CreepJS still flags headless); telemetry on by default (`engine_launched`).

**Benchmark methodology (claimed).** Warm browser, 5 iterations, 17 cases vs playwright-python on a dev host. Not a multi-browser-pool soak, not a 500-job admission test, not process-tree RSS of Chrome.

**Documented weaknesses.** Alpha; async fan-out; no fleet layer; OOPIF gaps; they compete in the “replace Playwright’s client” lane Fluxwright is refusing.

**What they do that we will not copy.** Driver-elimination speed claims in the README. Fleet is not their product.

---

### 2.2 Kitewright

**What it solves.** A small Rust **MCP server** (stdio / Streamable HTTP) for agents: navigate, snapshot, extract, screenshot, PDF. Experimental napi-rs Puppeteer-shaped Node bindings. Engine is **chromiumoxide**.

**Concurrency architecture (verified README).** One Chromium, **per-MCP-session browser context** (cookie isolation between agents). `MCP_CONTEXT_POOL` default **2** pre-warmed blank contexts so the *next session* skips launch+context create. This is a **tiny warm pool for interactive agents**, not hundreds of jobs. HTTP mode can serve many MCP sessions; stdio is one session.

**Browser lifecycle.** Lazy launch; **idle reaper** (`KITE_IDLE_TIMEOUT_SECS` default 1800) kills the headless browser; next call relaunches and can restore cookies. Headed mode does not reap. Lite mode blocks images/media/fonts + ad hosts via `Network.setBlockedURLs`.

**Memory behaviour (claimed, methodology in their BENCHMARKS.md).** Vs `@playwright/mcp`: idle server RSS **7.6 MB** vs 102–125 MB; cold start 75 ms vs 354 ms; **warm navigate is a tie** (both speak CDP to the same Chrome). They state Chromium cost is language-independent. Context pool drains on idle-reap so idle footprint returns to the small server RSS.

**API limitations.** MCP tools, not a `PageLease`. Default HTTP bind is documented inconsistently (`0.0.0.0:8090` vs loopback in the security section). Not a job scheduler. napi-rs API is experimental.

**Benchmark methodology (stronger than most Rust peers).** Same machine, same `chrome-headless-shell`, documented methodology file. Still MCP-shaped (start, idle RSS, first/warm navigate), not 500 concurrent isolated jobs.

**Documented weaknesses.** Wrong shape for a scrape fleet: one browser + a handful of contexts + idle kill. Inherits chromiumoxide’s CDP gaps (see `CDP_DECISION.md`). Auto-wait exists and is worth learning from (present/visible/enabled/not covered/stable).

---

### 2.3 chromiumoxide

Low-level async CDP for Rust (mattsse / Sytten). Latest crates.io **0.9.1** (2026-02-25). Generated types from Chromium PDL (~60k lines). Tokio-only since 0.8.

Covered in detail in [`CDP_DECISION.md`](CDP_DECISION.md). Summary for this file: one WebSocket, flattened `sessionId`, Handler stream (no thread per command). **Not a pool.** Page/Element layer treats `iframe` targets as non-pages (`poll` returns immediately). `CreateTarget` response before `Target.targetCreated` **panics**. `Target.targetCrashed` is not handled as a first-class handler event. Open PR #331 (2026-07) adds OOPIF; it was still open when this was written.

**Forks.** `chromey` / `spider_chromiumoxide_*` keep CDP fresher and advertise high-concurrency scraping. They are still CDP clients + crawler features, not Fluxwright’s lease/admission layer. Using a fork trades maintenance for extra surface we do not need.

---

### 2.4 headless_chrome (`rust-headless-chrome`)

**What it solves.** Synchronous, thread-based “Puppeteer for Rust”. Tabs, screenshots, PDF, interception, JS coverage, incognito, optional Chromium fetch.

**Concurrency.** **Blocking.** README contrasts itself with fantoccini: plain threads, not Tokio. A thread per in-flight operation is exactly what Fluxwright forbids.

**Lifecycle / memory.** Launch a browser, open tabs. No pool, no recycle, no RSS admission.

**API limitations.** Incomplete CDP (frames listed as missing). Sync API cannot sit on the Tokio runtime without `spawn_blocking`.

**Benchmarks.** None comparable. Related-crate note in their README is honest.

**Verdict.** Unusable as the Fluxwright transport. Useful only as a historical source of launch-flag / key-definition ideas (chromiumoxide already borrowed some).

---

### 2.5 fantoccini

**What it solves.** Mature async **WebDriver** client (jonhoo). Tokio, CSS locators, forms, multi-browser (anything with a driver: geckodriver, chromedriver, …). crates.io 0.22.x, still maintained in 2026.

**Concurrency.** Async HTTP to a driver, not one multiplexed CDP socket per browser. Each WebDriver session is typically one browser. Fine for tests; the opposite of context-dense CDP packing.

**Lifecycle.** Session create/delete via the driver. No Chromium process-tree metrics.

**API limitations.** No CDP: no reliable request blocking by resource type at Chrome’s network stack, no OOPIF sessions, no `Target` events. JS coverage and other DevTools features are absent (their own comparison vs headless_chrome).

**Verdict.** Wrong protocol for this engine. Keep it in mind only if a later milestone adds WebDriver browsers.

---

### 2.6 agent-browser (Vercel Labs)

**What it solves.** Agent-first **CLI + native daemon**: snapshot with refs, click/type by ref, sessions. Pure Rust CDP daemon; npm installer. Chrome for Testing via `agent-browser install`. Optional idle timeout (default one hour).

**Concurrency.** A daemon driving **a** Chrome (sessions/profiles), not a multi-browser admission pool. GitHub issue #1445: dynamically injected cross-origin iframes miss `Target.attachedToTarget` because `setAutoAttach` is armed once and the event drain is not continuous — the same OOPIF class of bug as chromiumoxide.

**Lifecycle.** Daemon persists between commands; idle exit discards transient state unless restore is configured.

**Memory / benchmarks.** Marketed as fast native CLI vs Playwright MCP. Not a 500-job fleet.

**API limitations.** CLI/MCP, not a Rust lease library. One-host agent workflow.

---

### 2.7 rusty-browser (dashn9) + rustenium

**What it solves.** **Distributed** automation: HTTP API / CLI / UI spawn isolated Chrome **agents**. Flux scales agents as processes (local subprocesses or cloud VMs). rustenium is the engine (WebDriver BiDi + optional CDP).

**Concurrency.** Scale-out: **one agent, one browser**. Demo claims (theirs): 36 Chromes on 3× e2-medium, then a 4th node. That is Grid-like isolation, not context packing.

**Lifecycle.** Serverless-style spawn and teardown. Redis registry. gRPC from server to agent.

**Memory.** Isolation by process (and optionally by VM). No shared-browser context pool.

**API limitations.** Platform, not a crate you embed. Stealth/identity layers are in-scope for them, out of scope for Fluxwright v0.

**Claimed vs verified.** Architecture from their docs/repo. Throughput and stealth claims unverified here.

---

### 2.8 Other active Rust CDP work (found while searching)

| Project | Shape | Fleet? | Notes |
|---------|--------|--------|--------|
| [ferrous-browser](https://github.com/theoxfaber/ferrous-browser) | Async CDP, Playwright-ish locators | No | Advertises session isolation and “register listeners before commands”. Benchmarks vs Playwright are micro-ops, not fleets. |
| [chromist](https://github.com/russellwmy/chromist) | chromiumoxide-like: PDL gen + Handler + Page | No | Tokio, CDP-only. Same architectural family as chromiumoxide. |
| [viewpoint](https://github.com/stephenstubbs/viewpoint) | CDP + test fixtures | No | Test-runner flavour. |
| [playwright-cdp](https://docs.rs/playwright-cdp) | Playwright-shaped API, native CDP | No | No Node driver; still a client library. |
| [playwright-rust](https://github.com/padamson/playwright-rust) | Official-style bindings | No | **Still launches Microsoft’s Node Playwright server.** Cross-browser, not a Rust engine. |
| chromey | chromiumoxide fork | Crawler extras | High-concurrency CDP claimed; not a lease/admission engine. |

None of these implement RSS-ceiling admission + drain-recycle + bounded priority queue + context-per-job as a single product.

---

## 3. Baselines

### 3.1 Playwright

**What it solves.** The default automation API: locators, auto-wait (attached, visible, stable, enabled), routing, tracing, Chromium/Firefox/WebKit, test runner with a context per test.

**Concurrency architecture (verified).** One WebSocket (or pipe) to a browser; contexts and pages multiplexed. **No built-in pool.** Docs: create a `BrowserContext` per test; `browser.newPage()` is a convenience that makes a context you should not use in production loops. Parallelism is **test workers** (one worker ≈ one browser) or whatever the user builds. `BrowserServer` / `connect` share a browser; a crash still kills all contexts on that process.

**Browser lifecycle.** Launch / connect / close. Long-running `newContext` loops have historically crashed or leaked (e.g. WebKit dying after ~1662 context create/dispose cycles — Playwright#8775; later versions claimed a fix). Context isolation is real: cookies, storage, permissions.

**Memory behaviour.** Contexts are cheaper than processes and still grow with pages and site weight. Playwright does not recycle browsers after N jobs or above RSS. Users who scrape must build Crawlee-like policy themselves. Process-tree RSS is not in the API.

**API limitations for our use.** Excellent page API; **zero fleet**. Node driver for Python/Java/.NET. Request interception vs service workers needs `serviceWorkers: 'block'`. Auto-wait is the bar our small page API should copy *in spirit*, not in width.

**Benchmark methodology.** Playwright’s own benches are runner/locator microbenchmarks, often vs other test tools, often against public or fixture pages — not “500 isolated jobs, same Chrome binary, report median RSS”.

**Documented weaknesses.** Driver cost (real, small vs Chrome). No admission control. Browser process is a SPOF for all its contexts. Firefox/WebKit protocol differences.

---

### 3.2 Puppeteer

**What it solves.** Chrome-team CDP library for Node. `BrowserContext` isolates storage; default context cannot be closed. Pages, interception, tracing.

**Concurrency.** Same as Playwright without the test runner: you invent the pool. puppeteer-cluster exists because Puppeteer does not.

**Lifecycle.** Launch, connect over WS, close. Incognito vs default context.

**Memory.** Same Chromium physics. No RSS policy.

**API.** Weaker locators/auto-wait than Playwright. Firefox support exists but CDP-on-Chrome is the centre.

**Benchmarks.** Puppeteer’s repo has examples, not a cross-tool fleet harness.

---

### 3.3 WebdriverIO

**What it solves.** Test runner + WebDriver/BiDi (and CDP via plugins). Local runner: **one Node worker process and one browser session per spec/capability**. `maxInstances` caps that.

**Concurrency.** Process isolation, not context packing. Sharing state across files requires explicit worker-message APIs.

**Lifecycle.** Session per worker; browser runner reloads the page between tests.

**Memory.** Docs-adjacent commentary: tens to hundreds of MB per worker *plus* a full browser. Fine for CI shards; poor density for 1,000 scrape jobs.

**API.** WebDriver-first; Playwright-level locators are not the default model.

**Weaknesses.** Isolation is bought with processes. No Chromium context pool. Not a fleet engine.

---

### 3.4 Selenium 4 WebDriver BiDi and Cypress

- **Selenium 4 WebDriver BiDi** (`SeleniumHQ/selenium`, Apache-2.0): adds bidirectional WebSocket event streaming and console/network interception over classic W3C HTTP WebDriver, narrowing the protocol gap on Chromium and Firefox while retaining cross-browser compliance (`Firefox`, `Safari`, `Edge`) and Java/C#/Ruby enterprise grids. **Verified limitation:** Grid slots remain browser-process-oriented rather than in-process M x K context pools with process-tree RSS admission ceilings, priority queues, or Tier 1 pure-Rust HTTP fast paths.
- **Cypress:** excels at interactive frontend time-travel DX and component testing, executing inside the browser event loop. Structurally mismatched for high-concurrency multi-tab scraping, PDF generation, or non-JS Rust/Python fleets.

---

## 4. AI-agent and cloud-browser platforms (2026 landscape)

These platforms compete for AI browser agents, RAG scraping, and remote Chrome sessions.

| Project (GitHub) | Verified License | Core Architecture | Strengths vs Fluxwright `v0.14.0` | Fluxwright `v0.14.0` Advantage |
|---|---|---|---|---|
| **Steel.dev** (`steel-dev/steel-browser`) | `Apache-2.0` | Node/Fastify API (`actions`, `cdp`, `commands`, `files`, `logs`, `recording`, `selenium`, `sessions`) + Docker/Render templates | Managed `/files` upload/download HTTP API, unpacked Chrome extensions, typed SDKs | Compiled Rust core + Tier 1 HTTP fast path (`518.8 req/s`), M x K RSS-ceiling fleet, priority queue, built-in E2E tests, multi-node cluster (`coordinate`) |
| **Firecrawl** (`mendableai/firecrawl`) | `AGPL-3.0` | Multi-service extraction API (`apps/api/src/routes/v2.ts`) | Recursive `/crawl`, schema JSON extraction, zero-ops SaaS, huge AI RAG ecosystem | Permissive `Apache-2.0` single binary, full stateful automation (`/v1/sessions`, Live View, HITL, CDP gateway), `518.8 req/s` pure-Rust `/v1/scrape` (`13 MB` RSS) |
| **Browserbase + Stagehand** (`browserbase/stagehand`) | SaaS (`stagehand` `MIT`) | Hosted cloud infrastructure + TypeScript `act`/`extract`/`observe` SDK | Zero-ops cloud, cached self-healing AI selectors, managed residential proxies | Self-hosted Apache-2.0 (`0` per-hour or proxy meter), in-process SDKs + MCP + Live View/replay + Tier 1 HTTP fast path (`examples/adapters/stagehand` works directly) |
| **Browserless** (`browserless/browserless`) | Non-commercial / commercial | Node BaaS service (`/scrape`, `/pdf`, `BrowserQL`, `/playwright` wire) | Mature BaaS brand, K8s/cloud familiarity | Permissive `Apache-2.0`, process-tree RSS admission + N-job/age/RSS drain, Tier 1 HTTP fast path, in-process Rust/Node/Python SDKs + MCP |
| **Hyperbrowser** | Managed SaaS | Cloud stealth browsers + microVM code sandboxes + `/scrape`/`/crawl` | Bundled microVM sandboxes and turnkey geo-proxies in one credit pool | Self-hosted Apache-2.0 single binary, full E2E test suite, deterministic Rust/Python/Node SDKs |
| **Lightpanda** (`lightpanda-io/browser`) | `AGPL-3.0` | Custom Zig + V8 + `html5ever` non-Chromium headless engine | Very low RAM and startup latency on lightweight DOM workloads | Full Chromium SPA fidelity — and Fluxwright `--fast-cdp-endpoint` delegates Tier 2 jobs to Lightpanda with `-32601` fallback to Chrome without AGPL entanglement |
| **Browser-Use** (`browser-use/browser-use`) | `MIT` | Python autonomous LLM agent framework | Massive Python agent adoption and multimodal loop orchestration | Runs *under* Browser-Use (`examples/adapters/browser-use`) as a crash-proof multi-browser pool with Live View, HITL, and HAR replay |

---

## 5. Cross-cutting comparison

| Capability | Fluxwright `v0.14.0` | Browserless | puppeteer-cluster | Crawlee pool | Selenium Grid | Steel.dev | Playwright / Puppeteer |
|---|---|---|---|---|---|---|---|
| License | `Apache-2.0` | Commercial / non-comm | `MIT` | `Apache-2.0` | `Apache-2.0` | `Apache-2.0` | `Apache-2.0` |
| Tier 1 pure-HTTP fast path | **Yes** (`mode: "auto"`) | No | No | `CheerioCrawler` separate | No | No | No |
| Context-per-job default isolation | **Yes** (strict) | Caller | Opt-in CONTEXT | Opt-in incognito (default **off**) | Process/session | Per session | Caller |
| Bounded priority queue + `QueueFull` | **Yes** (Wait/Error + priorities) | 429 (FIFO) | Unbounded | Remote: wait | FIFO + timeout | Queue/sessions | Caller |
| Recycle on jobs, age, or RSS with drain | **Yes** (all 3 + drain) | TTL/container % | Missing (no RSS recycle) | Page count + idle | Node drain | Session TTL | No |
| Process-tree RSS admission ceiling | **Yes** (cgroup-aware) | Container % | No | No | No | Container | No |
| Auto crash retry + zero leaked Chrome | **Yes** (`0.0%` chaos fail) | Session bookkeeping | `repair()` (can stall) | Retry + backoff | New session | Session errors | Caller |
| Built-in E2E tests + tracing + video | **Yes** (`fluxwright/test`, `pytest`, `.zip`, video) | No | No | No | External runners | No | **Yes** (Playwright) |
| Live View HITL + replay + HAR + MCP | **Yes** (22-tool MCP) | Live Debugger / CDP | No | No | Grid UI / no MCP | Recording / files / CDP | Playwright MCP |

---

## 6. Verified CI benchmark proof (`ubuntu-latest`, `chrome-headless-shell 158.0.8127.88`)

Measured on GitHub Actions (`ubuntu-latest`, 4 vCPU) with full-lifecycle job timing (`context acquire/create -> goto -> work -> close`), total process-tree RSS, and normalized host CPU%:

- **5-tool fleet & chaos (`run 45939361663`, 2026-11-27):** Fluxwright won throughput in **8/8 scenarios** with **0.0% failures** and **0 leaked Chrome processes** (`42.5` jobs/s on `10-concurrent-browsers`, `40.5` on `500-concurrent`, `37.5` on `1000-queued`, `32.7` on `100-pdfs`, `24.8` on `scraping-200`).
  - Under `chaos-1000-queued` (`SIGKILL` Chrome every 2s): Fluxwright held **`32.1` jobs/s** (`0.0%` failed across `13` kills, `722 ms` p50, `1,093 ms` p95), Crawlee recovered with `0.0%` failures across `26` kills at **`2.7` jobs/s** (`16,111 ms` p50, `826 MB` peak RSS), raw Playwright and Puppeteer failed `80.1%` and `78.5%`, and `puppeteer-cluster@0.25.0` timed out past `465s`.
  - Honest Crawlee memory note: Crawlee used `773–1,129 MB` peak RAM because `@crawlee/browser-pool` serializes `newPage()` behind a `pLimit(1)` lock (`apify/crawlee#1126`), running fewer simultaneous browsers/pages at roughly half the throughput.
- **Tier 1 HTTP fast path & 5-minute soak (`run 45825257140`, 2026-11-26):** Tier 1 static `/v1/scrape` hit **`518.8 req/s`** (`13.9 ms` p50, `13 MB` RSS vs `28.9 req/s` / `384 MB` in browser mode), `/v1/map` hit **`1,140.7 req/s`** (`6.7 ms` p50), and `soak-5m` completed `7,898` jobs with `0.0%` failures and flat RSS (`505 -> 517 -> 443 -> 442 MB`).

> Next CI run (with `selenium-webdriver` added in PR #151) will append head-to-head Selenium WebDriver numbers on the exact same `chrome-headless-shell` binary.

---

## 7. Remaining gaps Fluxwright still has to close

1. **Selenium WebDriver CI head-to-head proof** (in progress via `feat/bench-selenium` / `bench.yml`).
2. **Machine-verified `docs/COMPATIBILITY.md` + CI dual-runner Playwright parity suite** (`tests/parity/`).
3. **Firecrawl/Steel killer HTTP additions:** `POST /v1/crawl` recursive crawler, declarative JSON schema extraction, `/openapi.json` + typed Node/Python `FluxwrightClient`, and managed `/v1/sessions/{id}/files` transfer.
4. **2-hour soak proof, multi-tenant API keys/quotas, and multi-worker `fluxwright coordinate` scaling proof** ahead of `v1.0.0`.
