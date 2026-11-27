# RAG Crawler Starter

Zero-dependency Node.js ESM crawler backed by `fluxwright serve`. Uses `POST /v1/crawl` with `mode: "auto"` (pure-Rust HTTP fast path with automatic Chromium escalation for SPAs), structured CSS `extract` rules, and `markdown` output to produce JSONL chunks ready for vector databases.

## Run

1. Start `fluxwright serve`:

```bash
export FLUXWRIGHT_SERVER_TOKEN="$(python -c 'import secrets; print(secrets.token_hex(32))')"
fluxwright serve --listen 127.0.0.1:7331
```

2. Crawl a site and write JSONL chunks:

```bash
cd examples/rag-crawler
node index.mjs https://docs.example.com > chunks.jsonl
```

Progress logs go to `stderr` so `stdout` is always valid JSONL.

## Configuration

| Variable | Default | Description |
|---|---|---|
| `FLUXWRIGHT_SERVER_URL` | `http://127.0.0.1:7331` | Base URL of `fluxwright serve` |
| `FLUXWRIGHT_SERVER_TOKEN` | *(required)* | Bearer token matching the server |
| `CRAWL_URL` | `https://example.com` | Seed URL (overridden by CLI arg) |
| `CRAWL_MAX_DEPTH` | `2` | Breadth-first crawl depth (`0..5`) |
| `CRAWL_MAX_PAGES` | `25` | Maximum pages to crawl (`1..250`) |
| `CRAWL_CONCURRENCY` | `4` | Concurrent page workers (`1..16`) |
| `CHUNK_SIZE` | `1000` | Target chunk size in characters |
| `CHUNK_OVERLAP` | `150` | Trailing character overlap between chunks |
| `OUT_FILE` | *(stdout)* | Optional path to write JSONL directly |
