# Decisions

Picked the simpler option when the spec was silent. See also `CDP_DECISION.md`.

## 2026-09-21 — CDP: types unused, own wire client

`chromiumoxide` `Handler`/`Page` are unsafe for us (`CDP_DECISION.md`). Generated PDL types pull a slow compile and a Page-shaped API we would ignore. **v0 speaks CDP as JSON** (`method` + `params` + `sessionId`) over one WebSocket. Protocol strings match Chromium’s names. If JSON drift bites, we can depend on `chromiumoxide_cdp` without taking Handler.

## 2026-09-21 — `waitForDebuggerOnStart: false`

Auto-attach uses `flatten: true` and re-arms `Target.setAutoAttach` on every attached session. New targets are **not** paused. Pausing requires a resume race we do not need for scraping; it is a common OOPIF hang. Revisit if frames attach too late for `evaluate` on first paint.

## 2026-09-21 — Lease drop is fire-and-forget dispose

`Drop` cannot await. Dropping a lease sends a dispose to the engine task (destroy context, free slot). `close().await` does the same and waits. Cancelled `acquire`/`run` futures drop the lease and still clean up.

## 2026-09-21 — Queue `Wait` vs `Error`

Bounded queue of waiters. **Error**: `acquire` returns `QueueFull` when the queue is at capacity. **Wait**: the caller parks until there is queue space or a lease, and never sees `QueueFull`. Capacity still bounds *queued* work so memory cannot grow without limit from waiter structs… waiters in Wait mode that cannot enter the queue sit on a `Notify` (one extra parking set). Documented as “wait for admission,” not an unbounded job list.

## 2026-09-21 — `max_pages_per_context` (removed 2026-11-12)

Each lease creates one context and one page. The option was never read by anything, so setting it did nothing; it is gone.

## 2026-09-21 — Memory ceiling is process-tree RSS (superseded 2026-09-30)

`sysinfo` walks the Chrome pid and descendants. Windows and Linux. This is an estimate: shared mappings can be counted more than once. Named as such in metrics.

## 2026-09-21 — Soak is opt-in

A two-hour soak is `--soak` on `fluxwright-benchmarks`. The default command still writes JSON for the concurrency scenarios to `benchmarks/results/`. Defaulting to 2h would block CI and local iteration.

GitHub-hosted Linux often cannot use the SUID sandbox. CI sets `FLUXWRIGHT_NO_SANDBOX=1`, which is the documented opt-in and logs a warning. Default remains sandboxed.

## 2026-09-30 — Memory ceiling is Chrome’s footprint

Summed RSS counted pages shared between Chrome’s processes once per process: one browser with four pages read 1202 MB against a 665 MB footprint, so the ceiling tripped at about half its setting. Each process now counts PSS on Linux and private bytes on Windows, the measures behind Chrome’s own memory footprint. macOS keeps RSS.

## 2026-09-30 — DevTools pipe on Unix

Linux and macOS launch Chrome with `--remote-debugging-pipe`. Chrome exits when the pipe closes, which is the only way to tie it to the engine on macOS (no parent-death signal). A debugging port is also reachable by every local user; the pipe is not. Windows keeps the WebSocket, tied to the engine by a kill-on-close job object.

## 2026-09-30 — Headless launches prefer chrome-headless-shell

Playwright’s default. Pages opened 5–10x faster than on Chrome’s new headless mode in interleaved runs. Picked from `PATH`, then Puppeteer’s and Playwright’s caches; `FLUXWRIGHT_CHROMIUM` and `EngineBuilder::chrome` override.

## 2026-11-03 — HTTP jobs are structured, bounded and single-operator

`fluxwright serve` owns a separate fleet and accepts JSON steps rather than uploaded host-side
scripts. A submission returns an ID immediately; the existing engine handles priority, page
isolation and retries. A total deadline also covers queue wait and retries. Retries default to
zero because replaying a click or form submission can duplicate side effects.

One bounded in-memory registry holds active work and completed results. Admission evicts the
oldest submitted finished job, never active work; otherwise it returns 429. Cancellation holds
its capacity until the worker has actually stopped. There is no durable queue or delivery
guarantee in this milestone. A required bearer token and loopback default protect a trusted
operator API, not a multi-tenant execution sandbox or an egress boundary. See `SERVER.md`.

## 2026-11-11 — Observability: Prometheus metrics, OpenTelemetry W3C trace propagation, and SSE live dashboard

Fleet observability requires zero-friction integration with existing telemetry stacks without introducing heavy external dependencies or build pipelines:

1. **Prometheus text exposition format (v0.0.4) on `/metrics`**:
   Metrics are generated directly from the engine's `MetricsSnapshot` and server registry counters in standard Prometheus text format. Generating text avoids pulling heavy metric export runtimes. Authentication is the `Authorization: Bearer <token>` header only (superseded 2026-11-12): a `?token=` query parameter, accepted at first for scrapers and `EventSource`, put the full API token in proxy access logs, browser history and `Referer` headers. Prometheus sends the header itself (`authorization.credentials_file`), and the dashboard reads its stream with `fetch`.

2. **OpenTelemetry and W3C TraceContext propagation**:
   Distributed systems calling Fluxwright need end-to-end tracing across service boundaries. We support the W3C `traceparent` header on `POST /v1/jobs` (`tracestate` is ignored: nothing used it). When present and valid, the client's `trace_id` and parent span ID are preserved; when absent or invalid, a fresh high-entropy 16-byte hex trace ID and 8-byte hex root span ID are generated. The response echoes the trace via `traceparent` header and the `trace_id` field on the job JSON. Job and step executions emit structured `tracing` spans (`fluxwright.job` and `fluxwright.step`) following OpenTelemetry naming conventions.

3. **SSE-backed live fleet dashboard**:
   The live fleet dashboard at `/dashboard` and `/` is completely self-contained in a single responsive, accessible (WCAG AA compliant) HTML/CSS/JS document with light and dark themes. It requires no Node/npm build steps or external CDN requests. Server-Sent Events (`GET /v1/events`) was chosen over WebSockets because the dashboard stream is strictly unidirectional from server to client, works seamlessly through HTTP reverse proxies and firewalls without WebSocket upgrade negotiation, and leverages browser-native automatic reconnect logic in `EventSource`. A native accessible dialog captures and caches the operator token in `sessionStorage`.

## 2026-11-11 — Bring your own browsers: `connectOverCDP` architecture

Operators often run Chrome elsewhere: inside dedicated containers, on remote browser grids (Browserless, Playwright Server, Selenoid), or rented browser clouds. Fluxwright supports connecting directly to existing browsers over the Chrome DevTools Protocol via `connectOverCDP`:

1. **Remote browser source (`RemoteCdp`)**:
   Instead of launching a local child process and creating temporary profile directories, `RemoteCdp` connects to an existing CDP WebSocket URL (`ws://`, `wss://`) or HTTP endpoint (`http://host:port` / `http://host:port/json/version`). For HTTP endpoints, the debugger WebSocket URL is resolved via a zero-dependency minimal HTTP GET to `/json/version` over raw TCP.

2. **No local process manipulation**:
   Remote browsers have no local child process (`pid: 0`, `_child: None`). The engine avoids OS process killing, process-tree walks, or signal termination on disposal. When an engine lease drops or a job finishes, contexts and targets are detached or closed over CDP (`Target.closeTarget`), leaving the remote browser process under its external orchestrator's management.

3. **Handling disconnects and recycling**:
   If the remote WebSocket disconnects or fails the keep-alive / CDP handshake, the connection is marked dead (`conn.mark_dead()`). The fleet pool detects dead connections, discards the stale browser reference, and establishes a fresh connection to the remote endpoint on subsequent lease acquisitions.

4. **Metrics and memory ceiling**:
   Because remote browser processes reside outside the host OS, their process tree cannot be walked via `sysinfo`. Memory ceiling checks and RSS metrics treat remote browser PIDs (0) as 0 bytes, ensuring local resource constraints do not prematurely throttle or crash jobs executing against remote browser infrastructure.

## 2026-11-11 — Docker packaging: multi-stage Debian Bookworm image with Chromium and tini

Running a headless browser fleet inside containers requires matching glibc/system libraries, clean zombie process reaping, shared-memory sizing, and container-aware resource limits:

1. **Multi-stage `debian:bookworm-slim` + system `chromium`**:
   The builder stage (`rust:1-bookworm`) compiles stripped release binaries for `fluxwright` (`fluxwright-cli`) and `fluxwright-mcp` with `--locked`, while `.dockerignore` excludes Node/Python/web assets and `target/` directories so the build context stays minimal while preserving all Cargo workspace members. The runtime stage uses `debian:bookworm-slim` and installs Debian's packaged `chromium` (`/usr/bin/chromium`), `ca-certificates`, and core fonts (`fonts-dejavu-core`, `fonts-liberation`, `fonts-noto-color-emoji`). Alpine/musl was rejected because Chromium and its graphics/NSS stack perform poorly or lack official support on musl, and Debian's system `chromium` package resolves all shared library dependencies cleanly on both `amd64` and `arm64`.

2. **PID 1 reaping with `tini`**:
   Chromium spawns numerous renderer, GPU, and utility subprocesses. If a browser crashes or is recycled inside a container where PID 1 does not reap adopted children, defunct zombie processes accumulate and exhaust the container PID table. Setting `ENTRYPOINT ["/usr/bin/tini", "--", "fluxwright"]` guarantees proper signal forwarding (`SIGTERM`/`SIGINT` for graceful 10-second lease drain) and immediate zombie reaping without requiring operators to remember `docker run --init`.

3. **Non-root user with sandbox enabled by default**:
   The container runs as a dedicated unprivileged user (`fluxwright`, UID/GID `10001`) with writable `HOME` (`/home/fluxwright`) and `TMPDIR` (`/tmp/fluxwright`). In accordance with our security policy, `FLUXWRIGHT_NO_SANDBOX` is not set by default in the image; operators run with `--cap-add=SYS_ADMIN` or a seccomp profile permitting user namespaces, and only set `FLUXWRIGHT_NO_SANDBOX=1` when explicitly opting out in restricted container runtimes.

4. **Cgroup auto-tuning (`--memory` and `--cpus`)**:
   Instead of hardcoding container pool sizes in the `Dockerfile`, the default `CMD ["serve", "--listen", "0.0.0.0:7331"]` relies on `fluxwright-core`'s Linux cgroup v1/v2 inspection. Passing `--memory` and `--cpus` to `docker run` automatically scales the default browser count and sets the 75% memory ceiling to the container's cgroup allocation.

## 2026-11-15 — Jobs that survive a restart: an append-only journal, not a database

`fluxwright serve --journal <file>` appends each accepted job as one JSON line and a second line
when it ends; on start, jobs with no end line are queued again under their old ids, and the file
is rewritten with just those.

- **Why a file and not SQLite:** the server needs two operations, "remember this job" and
  "forget it", and one reader, at startup. A journal does that in about a hundred lines with no
  new dependency; SQLite would add a C library to the build and the Docker image for no query we
  need. Revisit when results must survive too, or when several servers share one queue.
- **At least once:** a job that was running when the server stopped runs again from its first
  step. Exactly-once would need per-step checkpoints and idempotent steps, which a click is not.
- **Only work, not results:** finished jobs are not kept, so the file stays as small as the
  queue. A client that polls after a restart gets 404 for a job that had already finished.
- **Durability:** each line is synced to disk before the job starts, so a job is never running
  without being on file. That costs about a millisecond per submission.
