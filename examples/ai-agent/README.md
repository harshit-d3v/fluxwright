# AI Agent Starter (MCP + HITL Live View)

Zero-dependency Node.js ESM starter showing how an AI agent drives Chromium through `fluxwright-mcp --http` (Streamable HTTP) and allocates an interactive `/v1/sessions` browser with a `live_view_url` for Human-in-the-Loop (HITL) inspection.

## Run

1. Start `fluxwright-mcp` over Streamable HTTP:

```bash
FLUXWRIGHT_HEADLESS=1 fluxwright-mcp --http 127.0.0.1:8090
```

2. *(Optional, for the HITL session section)* Start `fluxwright serve`:

```bash
export FLUXWRIGHT_SERVER_TOKEN="$(python -c 'import secrets; print(secrets.token_hex(32))')"
fluxwright serve --listen 127.0.0.1:7331
```

3. Run the agent starter:

```bash
cd examples/ai-agent
node agent.mjs https://example.com
```

Set `HITL_HOLD_SECS=30` to pause after printing `live_view_url` so a human can open and interact with the live session in a browser before teardown.

## Configuration

| Variable | Default | Description |
|---|---|---|
| `FLUXWRIGHT_MCP_URL` | `http://127.0.0.1:8090/mcp` | Streamable HTTP MCP endpoint |
| `FLUXWRIGHT_MCP_TOKEN` | *(optional)* | Bearer token if MCP auth is enabled |
| `FLUXWRIGHT_SERVER_URL` | `http://127.0.0.1:7331` | Base URL for `/v1/sessions` HITL demo |
| `FLUXWRIGHT_SERVER_TOKEN` | *(optional)* | Bearer token for `fluxwright serve` |
| `TARGET_URL` | `https://example.com` | URL opened via MCP `open` tool |
| `HITL_HOLD_SECS` | `0` | Seconds to keep HITL session open before cleanup |
