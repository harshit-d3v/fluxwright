# Fluxwright MCP

stdio MCP server so **Claude Desktop**, **Claude Code**, **Codex**, and **Cursor** can drive Fluxwright (Chromium leases).

Tools: `open`, `goto`, `title`, `content`, `click`, `fill`, `evaluate`, `screenshot`, `close`.

## Install (this machine)

```bat
set PATH=%USERPROFILE%\.cargo\bin;%PATH%
cd C:\harshitJet\fluxwright
cargo install --path crates/fluxwright-mcp --force
```

Binary: `%USERPROFILE%\.cargo\bin\fluxwright-mcp.exe`

Chrome must be installed. Visible window by default. Headless:

```bat
set FLUXWRIGHT_HEADLESS=1
```

## Claude Desktop

File: `%APPDATA%\Claude\claude_desktop_config.json`

```json
{
  "mcpServers": {
    "fluxwright": {
      "command": "C:\\Users\\harsh\\.cargo\\bin\\fluxwright-mcp.exe"
    }
  }
}
```

Restart Claude Desktop. Ask: “Open Amazon and get iPhone 16 prices.”

## Codex

`~/.codex/config.toml` (Windows: `%USERPROFILE%\.codex\config.toml`)

```toml
[mcp_servers.fluxwright]
command = "C:\\Users\\harsh\\.cargo\\bin\\fluxwright-mcp.exe"
```

## Cursor

Project file `.cursor/mcp.json` (already added) or Cursor Settings → MCP.

## Where you can deploy

MCP **stdio** is a local child process. The Chrome fleet has to run **next to the server**.

| Place | Works? | Why |
|---|---|---|
| **Your PC** (Claude / Codex / Cursor) | Yes — this is the intended setup | Chrome is already here |
| **Vercel / Cloudflare / Lambda** | No | No long-lived Chromium, no MCP stdio child |
| **Docker on a VPS** (Fly.io, Railway, Render, ECS, a cheap VM) | Yes, with caveats | Install Chromium, `FLUXWRIGHT_HEADLESS=1` `FLUXWRIGHT_NO_SANDBOX=1`, expose MCP over HTTP with a stdio→SSE proxy |
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
