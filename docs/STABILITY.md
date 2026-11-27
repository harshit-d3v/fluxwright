# Fluxwright v1.0 Stability Contract

This document defines the Semantic Versioning (`MAJOR.MINOR.PATCH`) stability guarantees for Fluxwright starting at `v1.0.0`.

- **Patch releases (`1.x.Y`)**: Backward-compatible bug fixes, security fixes, and performance improvements.
- **Minor releases (`1.X.0`)**: Backward-compatible additions (new optional fields, endpoints, flags, metrics, SDK methods, or MCP tools).
- **Major releases (`2.0.0`)**: Breaking changes to any surface covered below, preceded by a deprecation window.

---

## 1. Covered Surfaces

### Rust Crates

All six workspace crates follow SemVer 2.0 across their public Rust APIs (`pub` items) and CLI flag contracts:

| Crate | Stability Scope |
|---|---|
| `fluxwright` | High-level `BrowserEngine`, `ContextLease`, `PageLease`, `Locator`, options structs, error variants, and re-exports. |
| `fluxwright-core` | Pool lifecycle, scheduler/priority queues, adaptive concurrency, profile/proxy management, and metrics snapshots. |
| `fluxwright-cdp` | Public CDP client/session types, action helpers, ARIA snapshot/markdown APIs, and `netguard` SSRF utilities. |
| `fluxwright-server` | `HttpServer`, `ServerConfig`, `JobRequest`, `Step`, `ServerMetrics`, and `ConfigError` public APIs. |
| `fluxwright-cli` | Top-level subcommands (`serve`, `coordinate`, `worker`, `install`, `start`, `stats`, `browsers`) and documented CLI flags/exit codes. |
| `fluxwright-mcp` | `--http` / stdio transport behavior, bearer auth enforcement (`FLUXWRIGHT_MCP_TOKEN`), `/healthz`, `/mcp`, and the 22 MCP tools below. |

**MSRV policy:** Raising the Minimum Supported Rust Version (`rust-version` in `Cargo.toml`, currently `1.95`) is treated as a minor-version bump (`1.X.0`) and announced in release notes.

### Node.js & Python SDKs

| Ecosystem | Stable Modules | Stability Scope |
|---|---|---|
| Node.js (`fluxwright`) | `fluxwright`, `fluxwright/test`, `fluxwright/client` | Playwright-compatible browser/context/page/locator APIs, `expect` assertions, `test` / `defineConfig` / `createFluxwrightFixtures` runner APIs, `fluxwright-test` CLI flags, and typed HTTP server client methods. TypeScript `.d.ts` signatures remain backward-compatible within `1.x`. |
| Python (`fluxwright`) | `fluxwright`, `fluxwright.async_api`, `fluxwright.sync_api`, `fluxwright.client` | Async and sync Playwright-compatible APIs, `expect` assertions, pytest plugin fixtures (`pytest11` entry point), error hierarchy (`Error`, `TimeoutError`), and HTTP server client methods. |

### HTTP `/v1/*` API & `/openapi.json`

All `/v1/*` routes served by `fluxwright serve` and `fluxwright coordinate` (along with `/healthz`, `/openapi.json`, `/cdp`, `/json/version`, and `/metrics`) follow additive compatibility within `1.x`:

- **Endpoints**:
  - Health & discovery: `GET /healthz`, `GET /openapi.json`, `GET /json/version`, `GET (WS) /cdp`
  - Imperative jobs: `POST /v1/jobs`, `GET /v1/jobs/{id}`, `DELETE /v1/jobs/{id}`, `POST /v1/jobs/{id}/pause`, `POST /v1/jobs/{id}/resume`
  - Extraction & crawling: `POST /v1/scrape`, `POST /v1/map`, `POST /v1/crawl`, `GET /v1/crawl/{id}`, `DELETE /v1/crawl/{id}`
  - Managed sessions: `POST /v1/sessions`, `GET /v1/sessions`, `GET /v1/sessions/{id}`, `DELETE /v1/sessions/{id}`, `POST /v1/sessions/{id}/extend`, `GET /v1/sessions/{id}/replay`, `GET /v1/sessions/{id}/network`, `GET (WS) /v1/sessions/{id}/cdp`
  - Live view & telemetry: `GET /v1/live`, `POST /v1/live/{id}/ticket`, `POST /v1/live/{id}/embed`, `GET (WS) /v1/live/{id}/ws`, `GET /v1/stats`, `GET /v1/events`
  - State & cluster: `GET /v1/profiles`, `DELETE /v1/profiles/{name}`, `GET /v1/proxies`, and coordinator worker endpoints (`/v1/workers`, `/v1/workers/{worker}`, `/v1/workers/{worker}/drain|lease|heartbeat|release`, `/v1/jobs/{id}/result`)
- **Wire contract**:
  - Existing request fields, response field names, JSON types, and HTTP status codes do not change incompatibly in `1.x`.
  - New optional request fields (with backward-compatible defaults) and new response properties may be added in minor releases; clients must tolerate additive response fields.
  - Error responses preserve the `{ "error": { "code": "...", "message": "..." } }` envelope.
  - `GET /openapi.json` remains valid OpenAPI `3.1.0` describing the `/v1/*` surface.

### Prometheus `/metrics` Names

The following metric names and label keys emitted at `GET /metrics` (Prometheus text exposition format `v0.0.4`) are stable across `1.x`:

- **Fleet gauges**: `fluxwright_browsers`, `fluxwright_contexts`, `fluxwright_pages`, `fluxwright_active_leases`, `fluxwright_queued_requests`, `fluxwright_warm_contexts`, `fluxwright_browser_reuse_rate`, `fluxwright_engine_rss_bytes`, `fluxwright_browser_tree_rss_bytes`, `fluxwright_memory_per_job_bytes_estimate`, `fluxwright_effective_concurrency_limit`
- **Fleet counters**: `fluxwright_warm_context_hits_total`, `fluxwright_crash_count_total`, `fluxwright_recycle_count_total`
- **HTTP server & cluster**: `fluxwright_http_jobs_total{status}`, `fluxwright_http_jobs_active{status}`, `fluxwright_http_jobs_capacity`, `fluxwright_cluster_workers{status}`, `fluxwright_cluster_lease_expirations_total`
- **CDP gateway & fast path**: `fluxwright_cdp_sessions_active`, `fluxwright_cdp_sessions_total`, `fluxwright_fastpath_hits_total`, `fluxwright_fastpath_fallbacks_total`

New metrics may be added in minor releases; existing names, types, and label keys will not be renamed or removed in `1.x`.

### `fluxwright-mcp` Tools (22 Tools)

All 22 MCP tool names and their existing input parameter names/types are frozen for `1.x` and verified in CI by `v1_mcp_tool_schema_contract_snapshot` in `crates/fluxwright-mcp/src/main.rs`:

| Tool | Required Parameters | Optional Parameters |
|---|---|---|
| `open` | — | `url` |
| `goto` | `url` | — |
| `title` | — | — |
| `content` | — | — |
| `snapshot` | — | `interactive_only`, `compact`, `max_depth`, `max_nodes`, `diff` |
| `markdown` | — | `selector`, `main_content_only`, `include_links`, `include_images` |
| `click` | `selector` | — |
| `hover` | `selector` | — |
| `fill` | `selector`, `value` | — |
| `press` | `key` | `selector` |
| `select` | `selector` | `value`, `label`, `index` |
| `scroll` | — | `selector`, `delta_x`, `delta_y` |
| `wait_for` | — | `selector`, `state`, `url`, `timeout_ms` |
| `back` | — | — |
| `forward` | — | — |
| `tabs` | `action` | `index`, `url` |
| `console` | — | `limit` |
| `network` | — | `limit`, `filter` |
| `export_api_calls` | — | `limit`, `filter`, `include_auth`, `format` |
| `evaluate` | `expression` | — |
| `screenshot` | — | `full_page`, `annotate` |
| `close` | — | — |

Within `1.x`, Fluxwright will not rename/remove any of these 22 tools, will not add new **required** parameters to existing tools, and will preserve `"type": "object"` JSON Schema compatibility. New optional parameters and new tools may be added in minor releases.

---

## 2. Deprecation Window Policy

Before any covered API, CLI flag, HTTP endpoint/field, metric name, or MCP tool is removed or changed incompatibly:

1. **Minimum notice window:** At least **one full minor release** (`1.X.0` → `1.(X+1).0`) must ship with an explicit deprecation notice before removal in the next major release (`2.0.0`).
2. **Deprecation signals:**
   - Rust items are marked `#[deprecated(since = "1.X.0", note = "...")]`.
   - Node.js/Python symbols carry JSDoc `@deprecated` / `warnings.warn(..., DeprecationWarning)` notices.
   - HTTP/CLI/MCP deprecations are documented in `CHANGELOG.md`, `docs/`, and OpenAPI `deprecated: true` metadata.
3. **Security exceptions:** Mitigations for active security vulnerabilities (e.g., SSRF bypasses or auth hardening) may ship in a patch release even if they reject previously accepted unsafe inputs.

---

## 3. Explicitly Excluded from Stability Guarantees

The following implementation details may change in any release (including patch releases) without a major version bump:

- **Internal CDP wire timing and command sequencing:** Exact CDP frame ordering, retry backoff intervals, internal session IDs, and verbatim Chromium error strings.
- **HTML dashboard and embed markup:** DOM structure, CSS classes, inline scripts, and layout of `GET /`, `GET /dashboard`, and `GET /embed/live/{token}` (the `/v1/events` and `/v1/live/*` APIs backing them remain covered above).
- **Exact Chromium rendering output:** Pixel-for-pixel screenshot/PDF bytes across Chromium or OS font updates, and exact `@eN` reference numbers across unrelated DOM changes.
- **Unstructured log lines:** Human-readable `tracing` log text (structured OTLP spans and Prometheus metrics remain covered).
- **Explicitly unstable / experimental features:** Any CLI flag, API field, or crate feature explicitly marked `experimental` or `unstable` in documentation.
