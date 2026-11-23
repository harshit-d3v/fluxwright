# Fluxwright MCP

MCP server (**stdio** by default, or **Streamable HTTP** via `--http <ADDR>`) so **Claude Desktop**, **Claude Code**, **Codex**, **Cursor**, and remote MCP clients can drive Fluxwright (Chromium leases).

Tools (21):
- **Navigation & Context**: `open`, `goto`, `back`, `forward`, `tabs`, `close`
- **AI Inspection**: `snapshot` (compact ARIA tree with short `@eN` refs), `markdown` (clean Markdown), `title`, `content`, `screenshot`
- **Actions**: `click`, `hover`, `fill`, `press`, `select`, `scroll`, `wait_for`, `evaluate`
- **Diagnostics**: `console`, `network`

### Recommended AI Workflow

1. Call `snapshot()` (or `snapshot(interactive_only=true)`) to inspect controls and their `@eN` refs (`[ref=e1]`, `[ref=e2]`, ...).
2. Act using short refs (`click("@e3")`, `fill("@e2", "query")`, `select("@e4", value="pro")`). `@eN` refs clear automatically on navigation — run `snapshot()` again after page loads.
3. Call `markdown(main_content_only=true)` to read clean prose/tables instead of raw `content()` HTML or full screenshots.

## Install

Install from GitHub (requires Rust 1.95+):

```bash
cargo install --git https://github.com/harshit-d3v/fluxwright fluxwright-mcp
```

Or from a local checkout:

```bash
cargo install --path crates/fluxwright-mcp --force
```

Prebuilt archives are also available on [GitHub Releases](https://github.com/harshit-d3v/fluxwright/releases).

Chrome must be installed. Visible window by default; set `FLUXWRIGHT_HEADLESS=1` for headless mode.

## Claude Desktop

Add to your Claude Desktop config (`%APPDATA%\Claude\claude_desktop_config.json` on Windows, `~/Library/Application Support/Claude/claude_desktop_config.json` on macOS):

```json
{
  "mcpServers": {
    "fluxwright": {
      "command": "fluxwright-mcp"
    }
  }
}
```

If `~/.cargo/bin` (or `%USERPROFILE%\.cargo\bin`) is not on your `PATH`, pass the full path to the binary (e.g. `/path/to/fluxwright-mcp`).

Restart Claude Desktop. Ask: “Open Amazon and get iPhone 16 prices.”

## Codex

`~/.codex/config.toml` (Windows: `%USERPROFILE%\.codex\config.toml`)

```toml
[mcp_servers.fluxwright]
command = "fluxwright-mcp"
```

## Cursor

Project file `.cursor/mcp.json` (already added) or Cursor Settings → MCP.

## What the agent may open

`open` and `goto` take `http` and `https` URLs and `about:blank`. They refuse `file://`, `chrome://` and other schemes, and link-local hosts such as `169.254.169.254` (the cloud metadata service, which hands out instance credentials), however the address is written. A page the agent reads can try to talk it into opening those. Set `FLUXWRIGHT_MCP_ANY_URL=1` to allow any URL.

## Streamable HTTP Transport (`--http`)

Run `fluxwright-mcp` as an HTTP MCP server (for remote agents, containers, or shared local tooling) instead of `stdio`:

```bash
# Local loopback (optional bearer auth):
FLUXWRIGHT_HEADLESS=1 fluxwright-mcp --http 127.0.0.1:8088

# Authenticated (required for non-loopback addresses, >= 32 characters):
export FLUXWRIGHT_MCP_TOKEN="$(openssl rand -hex 32)"
FLUXWRIGHT_HEADLESS=1 fluxwright-mcp --http 0.0.0.0:8088
```

Endpoints:
- `POST /mcp`, `GET /mcp`, `DELETE /mcp`: MCP Streamable HTTP endpoint (`Mcp-Session-Id` per client session).
- `GET /healthz`: unauthenticated liveness check returning `200 OK` JSON (`{"status":"ok","transport":"streamable-http"}`).

Security & Isolation:
- **Fail-closed remote binding:** binding `--http <ADDR>` to a non-loopback IP refuses to start unless `FLUXWRIGHT_MCP_TOKEN` is at least 32 characters, or `--allow-unauthenticated` is explicitly passed (use only behind an authenticating TLS reverse proxy or private network).
- **Bearer authentication:** whenever `FLUXWRIGHT_MCP_TOKEN` is set, `/mcp` requires `Authorization: Bearer <token>` (constant-time comparison), returning `401 Unauthorized` otherwise.
- **Session isolation:** all HTTP sessions share one lazy `BrowserEngine` fleet while each MCP session receives its own isolated browser context (`ContextLease`) and tab list, cleaned up automatically when the session closes or expires.

## Where you can deploy

MCP **stdio** is a local child process; **Streamable HTTP** (`--http`) accepts remote MCP clients over HTTP/SSE. The Chrome fleet still runs **next to the server**.

| Place | Works? | Why |
|---|---|---|
| **Your PC** (Claude / Codex / Cursor) | Yes — `stdio` or `--http 127.0.0.1:8088` | Chrome is already here |
| **Vercel / Cloudflare / Lambda** | No | No long-lived Chromium process |
| **Docker on a VPS** (Fly.io, Railway, Render, ECS, a cheap VM) | Yes | Install Chromium, `FLUXWRIGHT_HEADLESS=1` `FLUXWRIGHT_NO_SANDBOX=1`, run `fluxwright-mcp --http 0.0.0.0:8088` with `FLUXWRIGHT_MCP_TOKEN` behind TLS |
| **Browserless / Browserbase** | Different product | Hosted Chrome; Fluxwright would need a remote `BrowserSource` (not in this server yet) |

Remote sketch (VPS):

```dockerfile
FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p fluxwright-mcp

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y chromium ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/fluxwright-mcp /usr/local/bin/
ENV FLUXWRIGHT_HEADLESS=1 FLUXWRIGHT_NO_SANDBOX=1 FLUXWRIGHT_CHROMIUM=/usr/bin/chromium
EXPOSE 8088
CMD ["fluxwright-mcp", "--http", "0.0.0.0:8088"]
```

Pass `FLUXWRIGHT_MCP_TOKEN` (>= 32 random printable ASCII characters) at runtime and put a TLS reverse proxy in front when exposing it outside localhost (`http://<host>:8088/mcp` or `https://<domain>/mcp`). Never pass `--allow-unauthenticated` on a public interface without upstream authentication.
