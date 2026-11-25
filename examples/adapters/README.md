# Fluxwright Agent Framework Adapters

Minimal reference adapters connecting **Browser-Use**, **Stagehand**, and the **Vercel AI SDK** to a running Fluxwright browser fleet (`fluxwright serve`) or MCP server (`fluxwright-mcp --http`).

## 1. Start the Fluxwright HTTP server

Generate a random bearer token (32–256 printable ASCII characters without whitespace) and start `fluxwright serve`:

```bash
export FLUXWRIGHT_SERVER_TOKEN="$(python -c 'import secrets; print(secrets.token_hex(32))')"
export FLUXWRIGHT_SERVER_URL="http://127.0.0.1:7331"

cargo run --release -p fluxwright-cli -- serve \
  --listen 127.0.0.1:7331 \
  --allow-query-token
```

PowerShell:

```powershell
$env:FLUXWRIGHT_SERVER_TOKEN = python -c "import secrets; print(secrets.token_hex(32))"
$env:FLUXWRIGHT_SERVER_URL = "http://127.0.0.1:7331"

cargo run --release -p fluxwright-cli -- serve --listen 127.0.0.1:7331 --allow-query-token
```

- `--allow-query-token` allows CDP WebSocket clients that cannot send custom handshake headers to pass `?token=<token>` on `/cdp`, `/json/version`, and `/v1/sessions/{id}/cdp`.
- HTTP REST endpoints (`/v1/sessions`, `/v1/scrape`, `/v1/map`, `/v1/jobs`, `/metrics`) always require `Authorization: Bearer <token>`.

## 2. Run an adapter

### Browser-Use (Python)

Creates an interactive managed session (`keep_alive: false`, `live_view_mode: "interactive"`, `record_frames: true`), prints `live_view_url`, attaches `BrowserSession(cdp_url=...)`, prints the `/v1/sessions/{id}/replay` summary, and cleans up via `DELETE /v1/sessions/{id}` in `finally`. (Network/HAR exports are also available at `GET /v1/sessions/{id}/network`.)

```bash
pip install -r examples/adapters/browser-use/requirements.txt
export OPENAI_API_KEY="sk-..."
python examples/adapters/browser-use/main.py
```

Set `FLUXWRIGHT_CDP_QUERY_TOKEN=1` to pass `?token=...` in the WebSocket URL instead of the `Authorization` header (requires `--allow-query-token` on `fluxwright serve`).

### Stagehand (TypeScript)

Creates a managed session via `POST /v1/sessions`, attaches Stagehand (`env: "LOCAL"` with `localBrowserLaunchOptions: { cdpUrl, cdpHeaders }`), runs `act()` and `extract()`, and deletes the Fluxwright session in `finally`.

```bash
cd examples/adapters/stagehand
npm install
export OPENAI_API_KEY="sk-..."
npm start
```

### Vercel AI SDK (TypeScript)

Defines AI SDK `tool()` wrappers over `POST /v1/scrape` and `POST /v1/map`, plus an `@ai-sdk/mcp` Streamable HTTP client connecting directly to `fluxwright-mcp --http`.

```bash
cd examples/adapters/vercel-ai-sdk
npm install
export AI_GATEWAY_API_KEY="..."   # or configure a provider model in index.ts
npm start                         # HTTP tools (default)
FLUXWRIGHT_MODE=mcp npm start     # Streamable HTTP MCP tools
```

## 3. Connect directly over `fluxwright-mcp --http`

Any MCP-compatible client (AI SDK `@ai-sdk/mcp`, Claude, Cursor, custom agents) can talk directly to Fluxwright's 21 browser tools over Streamable HTTP:

```bash
FLUXWRIGHT_HEADLESS=1 cargo run --release -p fluxwright-mcp -- --http 127.0.0.1:8090
```

- **MCP endpoint:** `http://127.0.0.1:8090/mcp`
- **Health check:** `http://127.0.0.1:8090/healthz`
- **Bearer auth:** Set `FLUXWRIGHT_MCP_TOKEN` (>= 32 printable ASCII characters) to require `Authorization: Bearer <token>` on `/mcp` (mandatory when binding non-loopback addresses unless `--allow-unauthenticated` is passed).
