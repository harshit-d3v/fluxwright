# PDF & Screenshot Microservice Starter

Zero-dependency self-hosted rendering microservice (`node:http`) backed by `fluxwright serve`'s `/v1/jobs` API. Exposes `POST /render/pdf` (`application/pdf`) and `POST /render/screenshot` (`image/png`, `image/jpeg`, or `image/webp`).

## Run

1. Start `fluxwright serve`:

```bash
export FLUXWRIGHT_SERVER_TOKEN="$(python -c 'import secrets; print(secrets.token_hex(32))')"
fluxwright serve --listen 127.0.0.1:7331
```

2. Start the rendering service:

```bash
cd examples/pdf-service
node server.mjs
```

3. Render a PDF or screenshot:

```bash
curl -sS -X POST http://127.0.0.1:3000/render/pdf \
  -H 'Content-Type: application/json' \
  -d '{"url":"https://example.com","format":"A4","print_background":true}' \
  --output example.pdf

curl -sS -X POST http://127.0.0.1:3000/render/screenshot \
  -H 'Content-Type: application/json' \
  -d '{"url":"https://example.com","full_page":true,"type":"png"}' \
  --output example.png
```

## Endpoints

| Method | Path | Response |
|---|---|---|
| `GET` | `/healthz` | `200 application/json` (`{ "status": "ok" }`) |
| `POST` | `/render/pdf` | `200 application/pdf` with `X-Fluxwright-Job-Id` |
| `POST` | `/render/screenshot` | `200 image/png` (or `image/jpeg` / `image/webp`) |

## Configuration

- `HOST` (default `127.0.0.1`), `PORT` (default `3000`)
- `FLUXWRIGHT_SERVER_URL` (default `http://127.0.0.1:7331`)
- `FLUXWRIGHT_SERVER_TOKEN` *(required)*
- `POLL_INTERVAL_MS` (default `250`)
