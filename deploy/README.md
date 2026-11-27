# Self-Hosting Fluxwright (`v0.14.0+`)

Deploy your own 3-tier browser fleet (pure-Rust HTTP fast path, optional external light-CDP, and pooled Chromium with automatic crash recovery and Live View) in under two minutes using `ghcr.io/harshit-d3v/fluxwright:latest` (`linux/amd64` + `linux/arm64`).

## 1. Docker Compose (recommended for single-host production)

```bash
cd deploy
cp .env.example .env
# Generate a random 64-char hex bearer token in .env:
python -c "import secrets; print(secrets.token_hex(32))"

docker compose up -d
```

Optional Prometheus metrics scraping on `http://127.0.0.1:9090`:

```bash
docker compose --profile monitoring up -d
```

Health and fast-path sanity check:

```bash
curl -fsS http://127.0.0.1:7331/healthz
curl -fsS -X POST http://127.0.0.1:7331/v1/scrape \
  -H "Authorization: Bearer $FLUXWRIGHT_SERVER_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"url":"https://example.com","mode":"auto","formats":["markdown"]}'
```

## 2. Fly.io

```bash
cd deploy
fly launch --copy-config --no-deploy
fly volumes create fluxwright_data --size 10
fly secrets set FLUXWRIGHT_SERVER_TOKEN="$(python -c 'import secrets; print(secrets.token_hex(32))')"
fly deploy
```

## 3. Render

Push this repository (or use `deploy/render.yaml` as a Render Blueprint). Render auto-generates `FLUXWRIGHT_SERVER_TOKEN`, attaches a 10 GB persistent disk at `/home/fluxwright/data`, and polls `/healthz`.

## 4. Kubernetes

Create the bearer token Secret first, then apply the deployment and service:

```bash
kubectl create secret generic fluxwright-secrets \
  --from-literal=server-token="$(python -c 'import secrets; print(secrets.token_hex(32))')"
kubectl apply -f deploy/k8s/deployment.yaml
```

Note on `/dev/shm`: Always keep the 1 GiB `Memory` `emptyDir` mount on `/dev/shm` (`shm_size: 2gb` in Docker Compose). Chromium renderers crash on complex pages under Kubernetes/Docker's default 64 MiB `/dev/shm`.

## 5. Multi-Machine Cluster (`fluxwright coordinate`)

To fan jobs across multiple workers, start one coordinator (`fluxwright coordinate --store /data/jobs.db`) and point each worker host at it with `--join https://coordinator:7331` and `FLUXWRIGHT_WORKER_TOKEN`. See [docs/CLUSTER.md](../docs/CLUSTER.md) and [docs/SERVER.md](../docs/SERVER.md) for the complete flag reference.
