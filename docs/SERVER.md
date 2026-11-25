# HTTP server mode

One HTTP service owns one Fluxwright fleet. Clients submit JSON steps, get a job ID immediately,
then poll for results or cancel the job. No Node or Python binding is required on the client.

## Start

Requires Rust 1.95+ and a locally installed Chrome, Chromium, or chrome-headless-shell, just like
the Rust API. Run from the repository root:

```bash
export FLUXWRIGHT_SERVER_TOKEN="$(python -c 'import secrets; print(secrets.token_hex(32))')"
cargo run --release -p fluxwright-cli -- serve
```

PowerShell:

```powershell
$env:FLUXWRIGHT_SERVER_TOKEN = python -c "import secrets; print(secrets.token_hex(32))"
cargo run --release -p fluxwright-cli -- serve
```

The listener defaults to `127.0.0.1:7331`. The token is required even on loopback. It must contain
32–256 printable ASCII characters without whitespace; use a random 32-byte token, not a password.
The server never prints it. Set the same token in the environment of each client.

```bash
cargo run --release -p fluxwright-cli -- serve \
  --listen 127.0.0.1:7331 --max-browsers 4 --max-contexts 8 \
  --max-jobs 128 --memory-ceiling-mb 4096
```

To use an external or remote browser instead of launching local Chrome processes, specify `--cdp-endpoint`:

```bash
cargo run --release -p fluxwright-cli -- serve \
  --cdp-endpoint ws://127.0.0.1:9222/devtools/browser/...
# Or using an HTTP base URL:
cargo run --release -p fluxwright-cli -- serve \
  --cdp-endpoint http://127.0.0.1:9222
```

`--cdp-endpoint <url>` accepts WebSocket URLs (`ws://`, `wss://`) or HTTP endpoints (`http://`). When given an HTTP URL, the server queries `/json/version` over TCP to discover the browser's WebSocket debugger endpoint. Remote browsers have no local child processes, skipping local process killing and memory walks.

A remote browser gets one connection, so set concurrency with `--max-contexts`. More connections to the same browser (`--max-browsers`) each attach to every page, and each answers dialogs on pages it does not own. Closing or recycling a connection never closes the remote browser; it only disposes of the contexts that connection created.

`--max-per-host <n>` caps the pages on one host at once across the fleet, and
`--min-host-delay-ms <ms>` spaces the navigations to one host: a `goto` step waits its turn, and
the wait counts toward the job's deadline. Only `goto` steps are counted, not clicks or redirects
that move a page to another host.

`--journal <file>` keeps accepted jobs across restarts. Every accepted job is appended to the
file and marked done when it succeeds, fails or is cancelled by a client. On start, jobs with no
done mark are queued again under their old ids, so a client polling `/v1/jobs/{id}` finds its job
after the restart. Three things to know:

- A job that was running when the server stopped runs again from its first step, so a click or a
  form submission can happen twice. Use it for jobs that are safe to repeat.
- Results are not kept: a job that had finished is gone after a restart (404).
- The file holds the job steps as sent, URLs and typed values included. It is created
  owner-only on Linux and macOS; put it somewhere only the server can read.

`--store <file>` (mutually exclusive with `--journal`) persists jobs in a SQLite WAL database
(`busy_timeout = 5000ms`, owner-only `0600` on Linux and macOS) on both `fluxwright serve` and
`fluxwright coordinate`. Like `--journal`, unfinished jobs (`queued`/`running`) are queued again on
startup, and unlike `--journal`, completed job snapshots (`results`, `error`, `failure_evidence`,
`status`, `attempts`, `finished_at`) are also retained across restarts up to `--max-jobs` (newest
first) and purged once older than `--result-ttl-secs <secs>` (default `3600`).

`--profiles-dir <dir>` stores named persistent browser profiles (`storageState` cookies,
`localStorage`, and IndexedDB) as owner-only JSON files (`0600` on Unix) so jobs with `"profile"`
resume authenticated sessions cleanly across restarts.

`--proxy <url>` (repeatable) configures an outbound proxy rotation pool
(`http://`, `https://`, `socks5://`, `socks5h://`, with optional `user:pass@`) featuring sticky
session affinity (`"proxy_session"`) and exponential cooldown backoff on failures.

`--adaptive-concurrency` enables an AIMD concurrency limiter that backs off under high browser RSS
pressure or failed jobs and grows smoothly back toward configured capacity while queue demand exists.

Without `--max-browsers` or `--memory-ceiling-mb`, the engine sizes itself to the machine.

Additional server operations flags on `fluxwright serve` (also available on `fluxwright coordinate` except `--embed-frame-ancestors`):

- `--webhook-secret <secret>` / `FLUXWRIGHT_WEBHOOK_SECRET`: HMAC-SHA256 signing key for terminal job webhook deliveries (`X-Fluxwright-Signature`).
- `--otlp-endpoint <url>` / `OTEL_EXPORTER_OTLP_ENDPOINT`: OTLP/HTTP JSON trace collector base URL (`POST <base>/v1/traces`).
- `--otlp-service-name <name>` / `OTEL_SERVICE_NAME`: `service.name` resource attribute on exported OTLP spans (default `fluxwright-server`).
- `--embed-frame-ancestors <policy>`: `Content-Security-Policy` `frame-ancestors` directive served on `GET /embed/live/{token}` (`fluxwright serve` only; default `*`).
- `--allow-session-cdp-endpoint`: opt-in flag enabling per-session `"cdp_endpoint"` (BYOB external browser) on `POST /v1/sessions` (`fluxwright serve` only; disabled by default).
`--max-contexts` defaults to 8 per browser. Ctrl+C (and SIGTERM on Unix) stops admission,
cancels outstanding jobs, and shuts down Chrome, allowing up to 10 seconds for lease cleanup.
The existing local `start` / `stats` / `browsers` commands are unchanged: the HTTP server is a
separate process, not an HTTP bridge to an already-running local daemon.

## Docker

The repository root includes a multi-stage `Dockerfile` (`rust:1-bookworm` builder to `debian:bookworm-slim` runtime) that packages `fluxwright`, `fluxwright-mcp`, system Chromium (`/usr/bin/chromium`), essential fonts (`fonts-dejavu-core`, `fonts-liberation`, `fonts-noto-color-emoji`), CA certificates, and `tini` running under a dedicated non-root user (`fluxwright`, UID/GID `10001`).

Build the image from the repository root:

```bash
docker build -t fluxwright .
```

Run the HTTP server container:

```bash
export FLUXWRIGHT_SERVER_TOKEN="$(python -c 'import secrets; print(secrets.token_hex(32))')"

docker run --rm \
  -e FLUXWRIGHT_SERVER_TOKEN="$FLUXWRIGHT_SERVER_TOKEN" \
  -p 127.0.0.1:7331:7331 \
  --shm-size=1g \
  --memory=4g \
  --cpus=4 \
  --cap-add=SYS_ADMIN \
  fluxwright
```

Key container runtime details:

- **Shared memory (`--shm-size=1g`)**: Docker defaults `/dev/shm` to 64 MiB, which causes Chromium renderer processes to crash on complex pages. Allocate at least `1g` (or `512m` for smaller fleets) via `--shm-size`.
- **PID 1 & signal reaping (`tini` / `--init`)**: The image sets `ENTRYPOINT ["/usr/bin/tini", "--", "fluxwright"]` and `CMD ["serve", "--listen", "0.0.0.0:7331"]` so `tini` forwards `SIGTERM`/`SIGINT` cleanly and reaps any orphaned Chromium subprocesses. Passing `--init` to `docker run` is also compatible.
- **Container cgroup auto-sizing (`--memory` and `--cpus`)**: Fluxwright inspects Linux cgroups (v1 and v2) at startup. When `--memory` and `--cpus` are passed to `docker run`, the engine automatically sizes its default browser count (1 browser per 4 CPUs) and memory ceiling (75% of the container limit) without requiring explicit `--max-browsers` or `--memory-ceiling-mb` flags. You can still override them by passing CLI arguments after the image name (for example, `fluxwright serve --listen 0.0.0.0:7331 --max-browsers 2 --max-jobs 256`).
- **Chromium sandbox and security (`--cap-add=SYS_ADMIN` / seccomp)**: The Chromium sandbox stays enabled by default inside the container (`FLUXWRIGHT_NO_SANDBOX` is not set). Unprivileged Docker containers often block the `clone` user-namespace syscalls required by Chromium's sandbox under the default seccomp profile. Grant `--cap-add=SYS_ADMIN` or provide a seccomp profile that permits user namespaces (`--security-opt seccomp=chrome.json`). Only set `-e FLUXWRIGHT_NO_SANDBOX=1` when running in locked-down container environments where user namespaces cannot be enabled and all target pages are trusted.
- **Loopback host binding (`-p 127.0.0.1:7331:7331`)**: Inside the container, `fluxwright serve` listens on `0.0.0.0:7331` so Docker port forwarding reaches it, and includes a built-in `HEALTHCHECK` against `http://127.0.0.1:7331/healthz`. Bind the host port to `127.0.0.1` (or a private network interface behind a TLS reverse proxy) rather than publishing `-p 7331:7331` on all public host interfaces.

## Submit and poll

```bash
curl -sS http://127.0.0.1:7331/v1/jobs \
  -H "Authorization: Bearer $FLUXWRIGHT_SERVER_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"steps":[{"action":"goto","url":"https://example.com"},{"action":"title"}]}'
```

Returns **202 Accepted** with a `Location: /v1/jobs/<id>` header, a W3C `traceparent` header, and a snapshot:

```json
{
  "id": "c179ccaf-0204-4b6c-b044-9691300fd473",
  "status": "queued",
  "submitted_at": 1793664000000,
  "started_at": null,
  "finished_at": null,
  "attempts": 0,
  "step_index": null,
  "results": null,
  "error": null,
  "trace_id": "4bf92f3577b34da6a3ce929d0e0e4736"
}
```

Poll the location with the same authorization header:

```bash
curl -sS http://127.0.0.1:7331/v1/jobs/<id> \
  -H "Authorization: Bearer $FLUXWRIGHT_SERVER_TOKEN"
```

States are `queued`, `running`, `succeeded`, `failed`, and `cancelled`. A successful job's
`results` array has one value per step, for example `[null, "Example Domain"]`. Failed jobs
have an `error`, no retained partial results, and diagnostic `failure_evidence` (captured console
logs, network activity, and a failure screenshot). `step_index` is zero-based; it is null if no
step ran. `attempts` counts attempts that acquired a page. Timestamps are Unix epoch milliseconds.
Poll at a reasonable interval, for example every 250–1000 ms.

Cancel queued or running work:

```bash
curl -sS -X DELETE http://127.0.0.1:7331/v1/jobs/<id> \
  -H "Authorization: Bearer $FLUXWRIGHT_SERVER_TOKEN"
```

Cancellation returns a **200** snapshot with `status: "cancelled"`; repeated cancellation is
idempotent while the record is retained. Lease disposal runs asynchronously. Cancellation does
not undo requests, form submissions, or clicks that already happened. Cancelling a succeeded or
failed job returns **409**.

## Job format

```json
{
  "timeout_ms": 60000,
  "priority": "normal",
  "retries": 0,
  "profile": "acct-prod",
  "persist_profile": true,
  "proxy_session": "user-42",
  "webhook_url": "https://hooks.example.com/fluxwright",
  "webhook_headers": {
    "Authorization": "Bearer hook-token"
  },
  "steps": [
    { "action": "goto", "url": "https://example.com", "wait_until": "load" },
    { "action": "text_content", "selector": "h1" },
    { "action": "evaluate", "expression": "location.hostname" }
  ]
}
```

Optional `webhook_url` (`http://` or `https://`, validated against SSRF rules unless `FLUXWRIGHT_SERVER_ANY_URL=1` and DNS-pinned on delivery) triggers an asynchronous completion callback when the job reaches a terminal state (`succeeded`, `failed`, or `cancelled`). Optional `webhook_headers` supports up to 16 custom headers (CR/LF/control characters and reserved `host`, `content-length`, `connection`, `transfer-encoding`, and `x-fluxwright-*` names are rejected) and is omitted from public `Job` responses so secrets do not leak.

| Action | Fields | Result |
|---|---|---|
| `goto` | `url`, optional `wait_until` | `null` |
| `url` | None | Current page URL string |
| `reload` | Optional `wait_until` | `null` |
| `go_back` / `go_forward` | Optional `wait_until` | Boolean (`false` at boundary of history) |
| `set_content` | `html`, optional `wait_until` | `null` |
| `wait_for_load_state` | Optional `state` (`"load"` \| `"domcontentloaded"` \| `"networkidle"`) | `null` |
| `wait_for_url` | `url` glob, optional `wait_until` | `null` |
| `wait_for_function` | `expression`, optional `polling_ms` (`1–60000`) | JSON result of the truthy expression |
| `add_init_script` | `script` | `null` |
| `add_script_tag` | `url` or `content`, optional `type` | `null` |
| `add_style_tag` | `url` or `content` | `null` |
| `click` | `selector`, optional `button` (`"left"` \| `"right"` \| `"middle"`), `click_count`, `delay_ms`, `position` (`{ x, y }`), `modifiers` (`["Alt" \| "Control" \| "Meta" \| "Shift" \| "ControlOrMeta"]`), `force`, `trial` | `null` |
| `dblclick` | `selector`, optional `button`, `delay_ms`, `position`, `modifiers`, `force`, `trial` | `null` |
| `hover` | `selector`, optional `position`, `modifiers`, `force`, `trial` | `null` |
| `focus` / `blur` | `selector` | `null` |
| `press` | `selector`, `key`, optional `delay_ms` | `null` |
| `type` | `selector`, `text`, optional `delay_ms` | `null` |
| `clear` | `selector`, optional `force` | `null` |
| `fill` | `selector`, `value` | `null` |
| `check` / `uncheck` | `selector`, optional `position`, `force`, `trial` | `null` |
| `select_option` | `selector`, `values` (string, `{ value \| label \| index }`, or array), optional `force` | Array of selected option values |
| `tap` | `selector`, optional `position`, `modifiers`, `force`, `trial` | `null` |
| `wait_for_selector` | `selector` | `null` |
| `text_content` | `selector`, optional `strict` | String or `null` |
| `inner_text` / `inner_html` / `input_value` | `selector`, optional `strict` | String |
| `get_attribute` | `selector`, `name`, optional `strict` | String or `null` |
| `count` | `selector` | Non-negative integer |
| `all_text_contents` / `all_inner_texts` | `selector` | Array of strings |
| `is_visible` / `is_hidden` / `is_enabled` / `is_disabled` / `is_checked` / `is_editable` | `selector`, optional `strict` | Boolean |
| `evaluate` | `expression` | JSON value, evaluated in the page, **not** on the host |
| `title` | None | String |
| `content` | None | HTML string |
| `snapshot` | Optional `interactive_only`, `compact` (default `true`), `max_depth` (`1–64`), `max_nodes` (`1–5000`) | `{ "tree": "…", "refs": [{ "id", "role", "name" }], "truncated": bool }` |
| `markdown` | Optional `selector`, `main_content_only`, `include_links` (default `true`), `include_images` | Markdown string |
| `pause_for_human` | Optional `message` (max 512 chars), `timeout_ms` (`1–600000`) | Pauses live job execution until resumed via `/dashboard` or API; resolves `{ "resumed": true }` |
| `screenshot` | Optional `full_page` (default `false`), `type` (`"png"` \| `"jpeg"` \| `"webp"`), `quality` (0–100, JPEG/WebP only), `clip` (`{ x, y, width, height }`), `omit_background` | `{ "mime_type": "image/png" \| "image/jpeg" \| "image/webp", "base64": "…" }` |
| `pdf` | Optional `format`, `width`, `height`, `margin` (`{ top, right, bottom, left }`), `landscape`, `print_background`, `scale` (0.1–2), `page_ranges`, `prefer_css_page_size`, `display_header_footer`, `header_template`, `footer_template`, `outline`, `tagged` | `{ "mime_type": "application/pdf", "base64": "…" }` |

Selectors support the engine's CSS, `@eN` / `ref=eN` snapshot refs, `xpath=…` (plus bare `//…` and `..`), `text=…`, `role=…`, and chained `>>` syntax. `wait_until`
accepts `load`, `domcontentloaded`, `networkidle`, or `commit`. Explicit `goto` URLs must use
HTTP/HTTPS or be `about:blank`; this validation is **not** a network isolation policy.
Unknown request/step fields are rejected instead of silently ignored. Filesystem output paths
and host-side scripts are not accepted.

Every job gets its own fresh context on a pooled browser. No cookies, storage, page handles,
or live contexts are shared between jobs. All steps in one job run on the same page in order.

### Limits and retries

- Request body: **64 KiB**; steps: **1–64**.
- `timeout_ms`: **1–300000**, default **60000**. This is a total deadline including queue wait,
  acquisition, all steps, and all retry attempts. Engine navigation/action timeouts still apply.
- `priority`: `low`, `normal` (default), or `high`; the engine handles scheduling.
- `retries`: **0–3**, default **0**. Only retryable engine errors are retried within the deadline.
  Retries replay **the entire job**. Enable them only when its side effects are safe to repeat.
- Combined JSON step results: **1 MiB** per job. Oversized output fails the job; it is not
  silently truncated. Single `screenshot` and `pdf` outputs are capped at **768 KiB** raw
  (`MAX_SCREENSHOT_BYTES` / `MAX_PDF_BYTES`), because base64 expansion puts them at the 1 MiB
  retained-results limit. This bounds retained output, not transient Chrome/CDP memory.
- `--max-jobs`: **1–4096**, default **128**, counting active jobs and retained results together.
  When full, the oldest submitted completed record is evicted first. Active jobs (including
  cancellation still being processed) are never evicted. If no completed record is available,
  submission returns **429** and `Retry-After: 1`.
- History is **in-memory and bounded** by default: without `--store`, restarting the server loses
  completed results (evicted/unknown/expired IDs return **404**). With `--store <file>`,
  finished results survive restarts within `--result-ttl-secs` (default `3600`) up to `--max-jobs`.
  Increasing `--max-jobs` increases retained memory (up to roughly 1 MiB of result data per job,
  plus overhead).

## Endpoints

| Method | Path | Authentication | Purpose |
|---|---|---|---|
| GET | `/healthz` | None | Liveness only; does not start Chrome or check readiness |
| POST | `/v1/jobs` | Bearer token | Submit; returns 202 with `Location` and `traceparent` headers |
| GET | `/v1/jobs/{id}` | Bearer token | Read status/results; returns 200 |
| DELETE | `/v1/jobs/{id}` | Bearer token | Cancel; returns 200 |
| GET | `/v1/stats` | Bearer token | Existing fleet metrics as JSON |
| GET | `/metrics` | Bearer token | Prometheus scrape endpoint (text format v0.0.4) |
| GET | `/dashboard` | None | Live HTML fleet dashboard |
| GET | `/` | None | Live HTML fleet dashboard (alias for `/dashboard`) |
| GET | `/v1/events` | Bearer token | Server-Sent Events (SSE) stream of fleet and job snapshots |
| GET | `/v1/live` | Bearer token | List active live browser sessions (jobs and `/cdp` gateway sessions) (standalone only) |
| POST | `/v1/live/{id}/ticket` | Bearer token | Mint a single-use 30-second WebSocket ticket bound to `{id}` (standalone only) |
| POST | `/v1/live/{id}/embed` | Bearer token | Mint an embeddable signed live viewer URL & WebSocket token (`read_only` or `interactive`, `10..=3600`s TTL) (standalone only) |
| GET | `/embed/live/{token}` | Signed `{token}` | Minimal standalone HTML live viewer (`Referrer-Policy: no-referrer`, SHA-256 CSP, configurable `frame-ancestors`) |
| GET (WS) | `/v1/live/{id}/ws` | Bearer token, `?ticket=`, or `?embed=` | Live JPEG screencast stream + HITL mouse/keyboard/navigate/pause forwarding (`?embed=` enforces token mode) (standalone only) |
| POST | `/v1/jobs/{id}/pause` | Bearer token | Pause a running live job before its next step (standalone only) |
| POST | `/v1/jobs/{id}/resume` | Bearer token | Resume a paused live job (standalone only) |
| GET | `/v1/profiles` | Bearer token | List saved named profiles (metadata only; requires `--profiles-dir`) |
| DELETE | `/v1/profiles/{name}` | Bearer token | Delete a saved named profile (requires `--profiles-dir`) |
| GET | `/v1/proxies` | Bearer token | Inspect sanitized proxy pool health/cooldown stats (requires `--proxy`) |
| GET | `/json/version` | Bearer (or `?token=` with `--allow-query-token`) | Standard DevTools discovery JSON pointing at `ws(s)://<host>/cdp` (standalone only) |
| GET (WS) | `/cdp` | Bearer (or `?token=` with `--allow-query-token`) | Pooled, context-isolated Chrome DevTools Protocol WebSocket gateway (standalone only) |
| POST | `/v1/sessions` | Bearer token | Mint a persistent managed cloud session with `cdp_ws_url`, `live_view_url`, `profile`, `proxy_session`, `viewport`, and TTL (standalone only) |
| GET | `/v1/sessions` | Bearer token | List active managed browser sessions (standalone only) |
| GET | `/v1/sessions/{id}` | Bearer token | Read managed session metadata and connection state (standalone only) |
| POST | `/v1/sessions/{id}/extend` | Bearer token | Reset a managed session's expiration TTL (`10..=3600`s) (standalone only) |
| DELETE | `/v1/sessions/{id}` | Bearer token | Close a managed session, persist profile cookies if enabled, and dispose its context (standalone only) |
| GET (WS) | `/v1/sessions/{id}/cdp` | Bearer (or `?token=` with `--allow-query-token`) | Connect or reconnect (`keep_alive: true`) over CDP to an existing managed session (`409` if already connected) (standalone only) |
| GET | `/v1/sessions/{id}/replay` | Bearer token | Searchable session replay timeline (`?q=`, `?kind=`, `?include_frames=`) for active or recently closed sessions (standalone only) |
| GET | `/v1/sessions/{id}/network` | Bearer token | Export recorded session network/API requests (`?filter=`, `?api_only=`, `?format=json\|har`, `?include_auth=`) (standalone only) |
| POST | `/v1/scrape` | Bearer token | Synchronously navigate and extract `markdown`, `html`, `text`, `snapshot`, `screenshot`, and/or `pdf` in one HTTP call (standalone only) |
| POST | `/v1/map` | Bearer token | Synchronously discover and filter deduplicated `http:`/`https:` links on a page (standalone only) |

The token is accepted only in the `Authorization` header by default (`--allow-query-token` unlocks `?token=` solely on `/cdp`, `/json/version`, and `/v1/sessions/{id}/cdp`, never `/v1/jobs*`, `/v1/sessions`, `/v1/sessions/*/replay`, `/v1/sessions/*/network`, `/v1/scrape`, `/v1/map`, or `/metrics`), so proxy access logs and `Referer` headers do not retain tokens accidentally. Browser `/dashboard` live streams authenticate WebSocket upgrades with single-use `?ticket=` tokens from `POST /v1/live/{id}/ticket`, while iframe embeds authenticate `GET /embed/live/{token}` and `/v1/live/{id}/ws?embed={token}` with capability tokens minted by `POST /v1/live/{id}/embed`.

## Managed cloud browser sessions (`/v1/sessions`)

Mint long-lived, isolated Chrome contexts upfront for agent frameworks (Browser-Use, Stagehand, Playwright `connectOverCDP`, Puppeteer) paired with an embeddable Live View URL:

```bash
curl -sS -X POST http://127.0.0.1:7331/v1/sessions \
  -H "Authorization: Bearer $FLUXWRIGHT_SERVER_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "ttl_secs": 300,
    "keep_alive": true,
    "profile": "account_1",
    "viewport": { "width": 1280, "height": 720 },
    "live_view_mode": "interactive",
    "record_frames": true
  }'
```

Returns `201 Created` with `{ "id", "status", "cdp_ws_url", "cdp_path", "live_view_url", "live_view_mode", "keep_alive", "connected", "url", "profile", "proxy_session", "record_frames", "byob", "created_at", "expires_at", "ttl_secs" }`. With `"keep_alive": true`, disconnecting the CDP WebSocket detaches only the client while preserving pages, cookies, and storage so a subsequent worker can reconnect until `ttl_secs` expires or `DELETE /v1/sessions/{id}` runs.

### Opt-in BYOB (`cdp_endpoint`)

Start `fluxwright serve --allow-session-cdp-endpoint` to let `POST /v1/sessions` attach a single-use session to a caller-supplied external browser (`ws://`, `wss://`, `http://`, or `https://`), such as a local stealth browser (Patchright/Camoufox/Chrome):

- **Fail-closed default:** When `--allow-session-cdp-endpoint` is off, supplying `"cdp_endpoint"` returns `403` (`byob_disabled`).
- **SSRF boundary:** Permits loopback (`localhost`, `127.0.0.1`, `::1`) while blocking cloud metadata (`169.254.0.0/16`, `metadata.google.internal`), link-local (`fe80::/10`), and private RFC 1918 / ULA IP ranges unless `FLUXWRIGHT_SERVER_ANY_URL=1` is set.
- **Isolation:** Closing or expiring a BYOB session disposes only its created browser context and shuts down the temporary CDP client without terminating the external browser process.

### Session replay timeline & Network/API inspector

Every managed session maintains bounded replay and network logs (and retains up to `32` closed sessions for `15` minutes / `900s` after `DELETE` or TTL expiry):

- **Replay timeline (`GET /v1/sessions/{id}/replay`):** Supports `?q=<substring>`, `?kind=nav|console|network|frame`, and `?include_frames=true|false` (default `true`). Timeline buffer caps at `256` events; when `"record_frames": true`, top-level navigations capture throttled JPEG screenshots (`quality: 45`, capped at `40` frames and `2 MiB` base64 per session).
- **Network & HAR export (`GET /v1/sessions/{id}/network`):** Supports `?filter=<substring>`, `?api_only=true|false` (`xhr`/`fetch` or `json`/`graphql` response content types), `?format=json|har` (HAR 1.2), and `?include_auth=true|false` (default `false`). Sensitive headers (`authorization`, `cookie`, `set-cookie`, `x-api-key`, `proxy-authorization`) are redacted to `"[REDACTED]"` unless `include_auth=true`.

## Synchronous one-shot scrape (`POST /v1/scrape`)

Extract page content or renders without polling `/v1/jobs`:

```bash
curl -sS -X POST http://127.0.0.1:7331/v1/scrape \
  -H "Authorization: Bearer $FLUXWRIGHT_SERVER_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "url": "https://example.com",
    "formats": ["markdown", "snapshot", "screenshot"],
    "resource_preset": "scraping",
    "timeout_ms": 30000
  }'
```

Supports `formats` entries `"markdown"`, `"html"`, `"text"`, `"snapshot"`, `"screenshot"`, and `"pdf"`, optional `wait_until` / `wait_for_selector`, `profile` / `proxy_session`, `screenshot_full_page` / `screenshot_annotate`, and `snapshot_interactive_only`. Enforces the same SSRF guards and 1 MiB result size cap as `/v1/jobs`.

## Synchronous site link discovery (`POST /v1/map`)

Discover deduplicated `http:`/`https:` links from a rendered page in one call:

```bash
curl -sS -X POST http://127.0.0.1:7331/v1/map \
  -H "Authorization: Bearer $FLUXWRIGHT_SERVER_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "url": "https://example.com",
    "same_origin": true,
    "include_subdomains": false,
    "search": "docs",
    "limit": 100
  }'
```

Supports `same_origin` (default `true`), `include_subdomains` (default `false`), case-insensitive `search` substring filter, `limit` (`1..=1000`, default `100`), `wait_until`, `timeout_ms` (`1..=120000`, default `30000`), `resource_preset`, `profile`, and `proxy_session`. Returns `{ "url", "count", "links", "duration_ms" }` and enforces the same SSRF checks as `/v1/jobs` and `/v1/scrape`.

## Live screencast viewer & human-in-the-loop

In standalone mode (`fluxwright serve`), `/dashboard` displays active browser jobs and `/cdp` gateway sessions in **Live Browser Sessions** with a **Watch Live** action:

- **Screencast transport:** `GET /v1/live/{id}/ws` attaches an observer CDP session and streams `Page.startScreencast` JPEG frames (`quality` `20–95`, `max_width` `320–1920`, `max_height` `240–1200`), automatically acking each frame for backpressure and capturing an immediate fallback frame when viewing an idle/paused page.
- **Human takeover:** With **Interactive** enabled in the dashboard modal, pointer clicks, movement, mouse wheel, keyboard input, `Send Esc`, and URL navigation forward directly to the remote tab (URL navigation enforces SSRF checks unless `FLUXWRIGHT_SERVER_ANY_URL=1`).
- **Pause & resume:** Jobs can include a `{ "action": "pause_for_human", "message": "Solve CAPTCHA", "timeout_ms": 120000 }` step or be paused/resumed manually via `POST /v1/jobs/{id}/pause`, `POST /v1/jobs/{id}/resume`, or the dashboard viewer.
- **Embeddable signed live views:** Mint a scoped shareable viewer URL via `POST /v1/live/{id}/embed` with optional `{ "mode": "read_only" | "interactive", "ttl_secs": 300 }` (default `"read_only"`, clamped `10..=3600` seconds), returning `{ "url", "ws_url", "mode", "expires_at" }`. `GET /embed/live/{token}` serves a minimal standalone viewer without admin tokens or dashboard navigation (hiding controls in `read_only` mode), guarded by `Referrer-Policy: no-referrer`, a SHA-256 inline script/style CSP, and `--embed-frame-ancestors` (default `*`). WebSocket connections using `?embed={token}` enforce `mode` server-side (rejecting interactive/control frames with a `403` error in `read_only` mode). Active tokens are bounded (`256` max), expire on TTL, and revoke immediately when the live session closes.

## Pooled CDP WebSocket gateway (`GET /cdp`, `GET /json/version`)

Connect any existing CDP client (Playwright `connectOverCDP`, Puppeteer `connect`, Stagehand, Browser-Use, or Fluxwright `connect_over_cdp`) to the pooled fleet without changing automation code:

```bash
export FLUXWRIGHT_SERVER_TOKEN="$(python -c 'import secrets; print(secrets.token_hex(32))')"
fluxwright serve --listen 127.0.0.1:7331 --allow-query-token
```

```javascript
import { chromium } from 'playwright';

const browser = await chromium.connectOverCDP(
  `ws://127.0.0.1:7331/cdp?token=${process.env.FLUXWRIGHT_SERVER_TOKEN}`,
);
```

- **Auth & discovery:** Authenticate via `Authorization: Bearer <token>`, or pass `?token=<token>` when `fluxwright serve` is started with `--allow-query-token`. `GET /json/version` returns DevTools metadata pointing at `/cdp` without echoing query tokens.
- **Query options on `/cdp`:** Optional `priority=low|normal|high`, `resource_preset=scraping|minimal|no-media`, `timeout_ms=1..300000`, and `initial_page=true|false` (default `false`).
- **Tenant isolation:** Each WebSocket connection holds its own pooled `ContextLease` (and up to 4 client-created child contexts). Commands and events (`Target.getTargets`, `Target.attachToTarget`, `Storage.*`) are scoped strictly to that lease's contexts/sessions; arbitrary browser-level shutdown/crash commands are rejected (`-32601`), and `Browser.close` only closes that WebSocket and disposes its leased context.
- **Metrics:** Active and cumulative gateway sessions are exposed as `fluxwright_cdp_sessions_active` and `fluxwright_cdp_sessions_total` on `/metrics`.

API errors use `{ "error": { "code": "…", "message": "…" } }`. Missing/invalid credentials
return **401**, invalid fields **400/422**, malformed JSON **400**, wrong content type **415**,
oversized bodies **413**, full capacity **429**, and shutdown admission **503**. An execution
failure is a retained job with `status: "failed"`, not an HTTP error on submission or polling.
Responses include `Cache-Control: no-store`; there is no cross-origin browser access (CORS).

### Reaching internal networks (SSRF)

By default, `netguard` blocks **link-local / cloud metadata** destinations and non-web schemes:
- **Blocked:** IPv4 link-local (`169.254.0.0/16`, including integer/hex/octal host forms and cloud metadata `169.254.169.254`), IPv6 link-local (`fe80::/10`), AWS IPv6 metadata (`fd00:ec2::254`), IPv4-mapped link-local (`::ffff:169.254.0.0/112`), `metadata.google.internal` / `metadata`, and `file:` URLs.
- **Checked on submission and runtime:** `goto` (and `add_script_tag` / `add_style_tag`) URLs are validated at job submission, and `netguard::guard_requests` intercepts every request the page makes — including redirects, script-initiated navigations, WebSockets, and subresources — aborting any literal match or DNS answer that resolves to a blocked address.
- **Allowed by default:** Loopback (`127.0.0.1`, `::1`, `localhost`) and private RFC 1918 / ULA intranet ranges (`10.0.0.0/8`, `172.16.0.0/12`, `192.168.0.0/16`, `fc00::/7`) remain reachable so local fixtures and intranet pages work out of the box.
- **Opt-out:** Set `FLUXWRIGHT_SERVER_ANY_URL=1` to disable the runtime page-request guard and HITL live-navigation URL check for trusted workloads (explicit `goto` step submission still validates non-link-local `http://`, `https://`, or `about:blank` URLs).

A DNS check alone can be beaten by **DNS rebinding**: a name can resolve to a safe address when
Fluxwright checks it and to the metadata address when Chrome connects. Block link-local/metadata
(and private ranges, if jobs should only reach the public internet) at the network level too:

- **AWS:** require IMDSv2 with a hop limit of 1 on the instance metadata service, and/or add a
  security-group/NACL or host firewall rule dropping `169.254.169.254` and `fd00:ec2::254`.
- **GCP/Azure:** firewall or route rules dropping `169.254.169.254` and `169.254.169.253`.
- **Any host:** an egress firewall rule for link-local (`169.254.0.0/16`, `fe80::/10`) and private
  ranges when jobs should only reach the public internet.

## Observability

### Prometheus metrics (`GET /metrics`)

Fluxwright exposes Prometheus metrics in the standard text exposition format (v0.0.4) at `GET /metrics`. Scrapers authenticate with the `Authorization: Bearer <token>` header.

```bash
curl -sS -H "Authorization: Bearer $FLUXWRIGHT_SERVER_TOKEN" http://127.0.0.1:7331/metrics
```

Prometheus sends the header itself; keep the token in a file rather than in the config:

```yaml
scrape_configs:
  - job_name: fluxwright
    authorization:
      credentials_file: /etc/prometheus/fluxwright-token
    static_configs:
      - targets: ["127.0.0.1:7331"]
```

Exported metrics include:
- **Fleet Gauges**:
  - `fluxwright_browsers`: Number of active browsers in the fleet.
  - `fluxwright_contexts`: Number of active browser contexts in the fleet.
  - `fluxwright_pages`: Number of open browser pages in the fleet.
  - `fluxwright_active_leases`: Number of browser leases currently running jobs.
  - `fluxwright_queued_requests`: Number of requests waiting in the engine pool for an available browser slot.
  - `fluxwright_browser_reuse_rate`: Ratio of browser acquisitions that reused an existing browser (0.0 to 1.0).
  - `fluxwright_engine_rss_bytes`: Resident set size of the engine process in bytes.
  - `fluxwright_browser_tree_rss_bytes`: Total resident memory of all managed browser process trees in bytes.
  - `fluxwright_memory_per_job_bytes_estimate`: Estimated browser process-tree RSS divided by active leases in bytes.
  - `fluxwright_effective_concurrency_limit`: Active concurrency admission limit (reflects AIMD adaptive concurrency when enabled).
- **Fleet Counters**:
  - `fluxwright_crash_count_total`: Total count of browser crashes detected and recovered.
  - `fluxwright_recycle_count_total`: Total count of browser recycles performed by the engine.
- **HTTP Server Metrics**:
  - `fluxwright_http_jobs_total{status="succeeded|failed|cancelled"}`: Total number of HTTP jobs completed by final status (counter).
  - `fluxwright_http_jobs_active{status="queued|running"}`: Current number of active HTTP jobs (gauge).
  - `fluxwright_http_jobs_capacity`: Configured maximum job capacity (`--max-jobs`).

### Live fleet dashboard (`GET /dashboard` and `GET /`)

A standalone, accessible (WCAG AA compliant) HTML/CSS/JS dashboard is served directly at `/dashboard` and `/`. It requires no node/npm build toolchain or external CDN connections, supporting both light and dark color schemes.

Opening the dashboard in a browser displays real-time fleet health, active leases, queue depth, browser reuse rate, memory utilization, **Managed Sessions** (with **Live**, searchable **Replay**, and **Network / API** JSON/HAR inspector modals for active and retained sessions), and an auto-updating tabular view of recent jobs. If unauthenticated, a native modal dialog prompts for the server token, which is stored in `sessionStorage` and used to subscribe to the live event stream.

### Server-Sent Events stream (`GET /v1/events`)

The dashboard reads `GET /v1/events`, a Server-Sent Events (SSE) stream, to receive continuous updates without polling. Clients authenticate with `Authorization: Bearer <token>`; the dashboard reads the stream with `fetch` because `EventSource` cannot send that header. The stream ends when the server shuts down, so a graceful shutdown never waits on an open dashboard.

Every 1000 ms, the server emits an event named `snapshot` with a JSON payload containing:
- `fleet`: Current engine `MetricsSnapshot` (active leases, browsers, contexts, pages, memory, recycle/crash counts, reuse rate).
- `server`: Current job totals (`queued_jobs`, `running_jobs`, `succeeded_jobs`, `failed_jobs`, `cancelled_jobs`, `max_jobs`).
- `live_sessions`: Active live browser sessions (`id`, `kind`, `url`, `started_at`, `paused`, `pause_reason`, `can_pause`).
- `managed_sessions`: Active and recently retained closed managed sessions (`id`, `status`, `url`, `keep_alive`, `connected`, `byob`, `record_frames`, `created_at`, `expires_at`, `closed_at`, `event_count`, `network_count`, `frame_count`).
- `jobs`: The 100 most recent job records, newest first, with `id`, `status`, `submitted_at`, `started_at`, `finished_at`, `attempts`, `step_index`, `error`, `trace_id`, `live`, and `paused` (the counts in `server` cover every retained job).

### Distributed tracing (W3C TraceContext & OpenTelemetry)

The server supports the W3C TraceContext `traceparent` header to propagate distributed traces across service boundaries (`tracestate` is ignored) and includes an OTLP/HTTP JSON trace exporter:

- **Incoming headers**: Clients can pass a standard W3C `traceparent` header (format `00-<trace_id>-<parent_id>-<flags>`) when submitting a job (`POST /v1/jobs`).
- **Trace propagation**: If a valid `traceparent` is provided, the server preserves the 32-hex `trace_id` and records the parent span ID. If missing or invalid, a fresh high-entropy 16-byte hex trace ID and 8-byte hex root span ID are generated automatically.
- **Response headers & payload**: The `POST /v1/jobs` response returns an outbound `traceparent` header (e.g. `00-<trace_id>-<root_span_id>-01`), and the `trace_id` is included in the `Job` JSON object (`trace_id`).
- **Spans**: Jobs and steps emit OpenTelemetry-compatible spans through `tracing` subscribers (`fluxwright.job` and `fluxwright.step`), and when `--otlp-endpoint <url>` / `OTEL_EXPORTER_OTLP_ENDPOINT` is configured on `fluxwright serve` or `fluxwright coordinate`, terminal jobs export `resourceSpans` over `POST <base>/v1/traces` (`Content-Type: application/json`):
  - **Resource & span fields**: Sets `service.name` from `--otlp-service-name` / `OTEL_SERVICE_NAME` (default `fluxwright-server`), `traceId`, `parentSpanId` (when supplied), root `spanId`, `startTimeUnixNano`, `endTimeUnixNano`, OTLP `status`, and attributes `job.id`, `job.status`, `job.steps`, and `job.attempts`.
  - **Batching**: Uses a non-blocking bounded queue (`512` spans) flushed asynchronously every `2s` or `32` spans (supporting both `http://` and `https://` collector URLs).

### Signed job webhooks

When a job specifies `webhook_url` (`http://` or `https://`), both standalone `fluxwright serve` and cluster `fluxwright coordinate` dispatch an asynchronous `POST` callback when the job reaches a terminal state (`succeeded`, `failed`, or `cancelled`):

- **Payload & headers**: Sends `{ "event": "job.completed", "job": { ... } }` with `Content-Type: application/json`, `X-Fluxwright-Event: job.completed`, `X-Fluxwright-Job-Id: <id>`, `X-Fluxwright-Timestamp: <unix_secs>`, plus any validated `webhook_headers` (up to `16` headers; CR/LF/control characters and reserved `host`, `content-length`, `connection`, `transfer-encoding`, and `x-fluxwright-*` names are rejected).
- **HMAC-SHA256 signing**: When `--webhook-secret <secret>` or `FLUXWRIGHT_WEBHOOK_SECRET` is configured, includes `X-Fluxwright-Signature: sha256=<hex(HMAC-SHA256(secret, "{timestamp}.{body}"))>`.
- **SSRF protection & delivery**: `webhook_url` is validated at submission unless `FLUXWRIGHT_SERVER_ANY_URL=1`, and delivery connects directly to the DNS-resolved allowed IP address with SNI/`Host` pinned to the URL hostname to prevent DNS rebinding. Retries up to `3` attempts (`500ms`, `1500ms` backoff), `10s` timeout per attempt, bounded by `32` concurrent in-flight deliveries.

## Failure evidence

When a job fails (due to a step error, action failure, or execution timeout), `fluxwright-server` captures diagnostic evidence before discarding the page context and attaches it in the `failure_evidence` field:

```json
{
  "id": "c179ccaf-0204-4b6c-b044-9691300fd473",
  "status": "failed",
  "submitted_at": 1793664000000,
  "started_at": 1793664000050,
  "finished_at": 1793664005100,
  "attempts": 1,
  "step_index": 2,
  "results": null,
  "error": "waiting for selector \"#submit\" failed: timeout 5000ms",
  "trace_id": "4bf92f3577b34da6a3ce929d0e0e4736",
  "failure_evidence": {
    "console": [
      { "kind": "error", "text": "Uncaught TypeError: button is null" }
    ],
    "network": [
      { "url": "https://example.com/api/v1/auth", "method": "POST", "status": 401, "resource_type": "fetch" }
    ],
    "screenshot": "iVBORw0KGgoAAAANSUhEUgAA..."
  }
}
```

### Components

- **`console`**: Array of console entries (`kind`, `text`) capturing `console.log`, `console.warn`, `console.error`, and unhandled page exceptions emitted during the job execution.
- **`network`**: Array of network entries (`url`, `method`, optional `status`, optional `resource_type`, optional `failure`) recording requests dispatched by the page, including response status codes and failure descriptions.
- **`screenshot`**: Base64-encoded PNG screenshot of the viewport captured immediately when an error or timeout occurs, prior to context teardown.

### Memory bounding

To safeguard server memory against chatty page loops or large payloads:
- Console entries are limited to 256 items, with text capped at 2,048 bytes per message.
- Network entries are limited to 256 items, with URLs capped at 2,048 bytes per request.
- Total failure evidence is strictly bounded to `MAX_RESULT_BYTES` (1 MiB), shedding older entries and dropping screenshots if the serialized payload exceeds the limit.

## Security and deployment boundary

This is a **single trusted-operator service**, not a multi-tenant or public scraping API.
Anyone with `FLUXWRIGHT_SERVER_TOKEN` can submit browser jobs and read/cancel every retained job.
By default, `netguard` blocks `file:` URLs and link-local / cloud metadata targets (`169.254.0.0/16`,
`fe80::/10`, `fd00:ec2::254`, `metadata.google.internal`, `metadata`) at submission and across
redirects, script navigations, and subresources via literal and DNS checks (unless disabled with
`FLUXWRIGHT_SERVER_ANY_URL=1`). However, loopback and private intranet ranges remain reachable by
default, and DNS rebinding can bypass a pre-connect DNS check — so pages and `evaluate` can still
reach loopback or private services visible to Chrome unless restricted at the network/firewall
level. Isolate the fleet's network and filesystem accordingly.

Keep the default loopback listener, or bind to a private interface behind a TLS reverse proxy,
firewall, and deployment-level access/rate controls. Do not send tokens over untrusted
plaintext networks or expose this listener directly to the internet. No per-tenant isolation,
private-network egress firewalling, or idempotency key is provided; use `--journal` only for
repeatable jobs that need restart recovery.
The Chrome sandbox stays enabled by default; do not disable it just to run the server.

## Cluster mode (experimental: coordinator and workers)

Run one coordinator without local browsers, and join one or more browser-running workers across machines over native `https://` TLS (behind a TLS reverse proxy) or a **trusted private network** (see [docs/CLUSTER.md](CLUSTER.md)):

```bash
# On the coordinator host (optional dedicated FLUXWRIGHT_WORKER_TOKEN isolates worker routes from client API routes):
FLUXWRIGHT_SERVER_TOKEN="$CLIENT_TOKEN" \
FLUXWRIGHT_WORKER_TOKEN="$WORKER_TOKEN" \
  fluxwright coordinate --listen 10.0.1.10:7331 --store coord.sqlite --result-ttl-secs 3600 --max-per-host 2 --min-host-delay-ms 250

# On each worker host (supports native https:// TLS or private-network http://;
# FLUXWRIGHT_SERVER_TOKEN protects the worker's own local listener and need not match $CLIENT_TOKEN):
FLUXWRIGHT_SERVER_TOKEN="$WORKER_LOCAL_TOKEN" \
FLUXWRIGHT_WORKER_TOKEN="$WORKER_TOKEN" \
  fluxwright serve --join https://coordinator.internal
```

Coordinators also accept `--max-per-host <n>` and `--min-host-delay-ms <ms>` to enforce cluster-wide per-host concurrency and lease spacing across all workers (based on the job's first `goto` host) without head-of-line blocking other hosts, and accept either `--store <file>` (SQLite with `--result-ttl-secs`, default `3600`) or `--journal <file>`. Workers pull up to their local capacity (`POST /v1/workers/{worker}/lease`), extend 30-second leases every 10 seconds (`POST /v1/workers/{worker}/heartbeat`), abort client-cancelled jobs, and post results or failure evidence (`POST /v1/jobs/{id}/result`). Those worker routes are exposed only by `fluxwright coordinate`. Expired leases requeue within `retries` or fail with `"worker lost"`. On Ctrl+C / SIGTERM, a worker drains already-leased jobs up to 10 seconds, then calls `POST /v1/workers/{worker}/release` so any still-unfinished jobs return to `queued` without losing an attempt.

> **Security boundary:** `fluxwright serve --join` supports native `https://` TLS (`rustls` + WebPKI roots, e.g. via a TLS reverse proxy in front of `fluxwright coordinate`) as well as `http://` (non-loopback `http://` logs a warning). Set a dedicated `FLUXWRIGHT_WORKER_TOKEN` on both coordinator and workers so worker credentials cannot call client/operator routes (`POST /v1/jobs`, `/v1/stats`, `/metrics`, `/v1/workers/*` management) and client tokens cannot pull or post worker results; when `FLUXWRIGHT_WORKER_TOKEN` is unset, both fall back to `FLUXWRIGHT_SERVER_TOKEN`. Keep plaintext `http://` links strictly inside a private subnet or VPN.

## Development checks

```bash
# Protocol/config/admission tests without launching Chrome:
cargo test -p fluxwright-server --lib -- --test-threads=1
# Browser-backed job, isolation, output-limit and cancellation tests:
cargo test -p fluxwright-server --test http -- --test-threads=1
# Existing and new Rust suites (Chrome required):
cargo test --workspace -- --test-threads=1
```

The server crate is a workspace member, so the existing Rust CI job includes its tests.
