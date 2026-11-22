# Fluxwright MCP

stdio MCP server so **Claude Desktop**, **Claude Code**, **Codex**, and **Cursor** can drive Fluxwright (Chromium leases).

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

## Where you can deploy

MCP **stdio** is a local child process. The Chrome fleet has to run **next to the server**.

| Place | Works? | Why |
|---|---|---|
| **Your PC** (Claude / Codex / Cursor) | Yes — this is the intended setup | Chrome is already here |
| **Vercel / Cloudflare / Lambda** | No | No long-lived Chromium, no MCP stdio child |
| **Docker on a VPS** (Fly.io, Railway, Render, ECS, a cheap VM) | Yes, with caveats | Install Chromium, `FLUXWRIGHT_HEADLESS=1` `FLUXWRIGHT_NO_SANDBOX=1`, reach MCP over HTTP through a stdio→SSE proxy that only you can reach (see below) |
| **Browserless / Browserbase** | Different product | Hosted Chrome; Fluxwright would need a remote `BrowserSource` (not in this server yet) |

Remote sketch (VPS):

```dockerfile
FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release -p fluxwright-mcp

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y chromium ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/fluxwright-mcp /usr/local/bin/
ENV FLUXWRIGHT_HEADLESS=1 FLUXWRIGHT_NO_SANDBOX=1 FLUXWRIGHT_CHROMIUM=/usr/bin/chromium
# Clients cannot speak stdio over the internet. Put a proxy in front, e.g.
#   npx -y supergateway --stdio "fluxwright-mcp" --port 8787
CMD ["fluxwright-mcp"]
```

Then point a **remote MCP** client at that HTTP/SSE URL. Claude Desktop still prefers **local stdio**.

**Never publish that port.** The proxy has no login, so anyone who reaches it drives a browser inside your network. Bind it to `127.0.0.1` and connect over SSH (`ssh -L 8787:127.0.0.1:8787 your-vm`) or a VPN, or put a reverse proxy that requires authentication in front of it.
