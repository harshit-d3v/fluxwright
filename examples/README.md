# Fluxwright Examples

Production starter apps and framework adapters showing how to deploy Fluxwright across scraping, rendering, AI agents, and E2E testing.

## Production Starter Apps

| Example | Use Case | Surface Used | Dependencies |
|---|---|---|---|
| [`rag-crawler/`](./rag-crawler/) | Recursive site crawl → clean JSONL chunks for vector DB ingestion | `POST /v1/crawl` (`mode: "auto"`, CSS `extract`, `markdown`), `GET /v1/crawl/{id}` | None (Node 18+ built-in `fetch`) |
| [`pdf-service/`](./pdf-service/) | Self-hosted HTTP microservice for PDF & screenshot rendering | `POST /v1/jobs` (`pdf`, `screenshot` steps), `GET /v1/jobs/{id}` | None (Node 18+ `node:http`, `fetch`) |
| [`ai-agent/`](./ai-agent/) | Streamable HTTP MCP agent loop + HITL Live View session allocation | `fluxwright-mcp --http` (`/mcp`), `POST /v1/sessions` (`live_view_url`) | None (Node 18+ built-in `fetch`) |
| [`e2e-migration/`](./e2e-migration/) | Playwright → Fluxwright 1-line import swap checkout test suite | `fluxwright/test`, `fluxwright-test` CLI, locators, `expect`, traces | `fluxwright` |

## Agent Framework Adapters

| Adapter | Framework | Language | Transport |
|---|---|---|---|
| [`adapters/browser-use/`](./adapters/browser-use/) | Browser-Use | Python | Managed `/v1/sessions` CDP + Live View + Replay |
| [`adapters/stagehand/`](./adapters/stagehand/) | Stagehand | TypeScript | Managed `/v1/sessions` CDP |
| [`adapters/vercel-ai-sdk/`](./adapters/vercel-ai-sdk/) | Vercel AI SDK | TypeScript | `/v1/scrape`, `/v1/map`, or `fluxwright-mcp --http` |

See [`adapters/README.md`](./adapters/README.md) for adapter setup instructions.

## Quick Prerequisites

Most HTTP examples talk to a running `fluxwright serve` instance:

```bash
export FLUXWRIGHT_SERVER_TOKEN="$(python -c 'import secrets; print(secrets.token_hex(32))')"
fluxwright serve --listen 127.0.0.1:7331
```

For MCP examples (`ai-agent/` and `adapters/vercel-ai-sdk/`):

```bash
FLUXWRIGHT_HEADLESS=1 fluxwright-mcp --http 127.0.0.1:8090
```
