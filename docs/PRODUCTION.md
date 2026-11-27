# Production Operations Guide

Actionable sizing, soak verification, multi-tenancy, graceful shutdown, cluster topology, and Prometheus alerting guidance for running `fluxwright serve` and `fluxwright coordinate` in production.

## Memory ceilings & browser recycling

Chromium renderer memory grows over long-running workloads. Bound fleet memory with explicit limits rather than relying on the host OOM killer:

- **Container auto-sizing**: Under Linux cgroups v1/v2 (`docker run --memory=4g --cpus=4`), Fluxwright defaults to 1 browser per 4 CPUs and a memory ceiling of 75% of the container limit. Always allocate at least `--shm-size=1g` (or `512m` for single-browser nodes) so Chromium's `/dev/shm` does not crash renderers.
- **Hard ceiling (`--memory-ceiling-mb`)**: Caps total managed browser process-tree RSS and pauses new browser launches when memory pressure is reached.
- **Proactive RSS recycling (`--recycle-rss-mb <MB>`)**: Drains and recycles any managed local Chromium process once its process-tree RSS crosses `<MB>` MiB, letting in-flight jobs finish before replacing the browser. Pair with `--low-memory` (`--disable-gpu`, `--disable-software-rasterizer`, `--js-flags=--max-old-space-size=256`) on memory-constrained nodes.
- **Adaptive admission (`--adaptive-concurrency`)**: Enables an AIMD concurrency limiter that backs off smoothly under RSS pressure or elevated job failure rates and recovers toward configured capacity (`--max-browsers` × `--max-contexts`) once healthy.
- **Pre-warmed contexts (`--prewarm-contexts 2`)**: Keeps blank contexts ready per browser to trim p50/p95 cold-context latency without spawning extra browsers.

Example baseline for a 4 vCPU / 4 GiB worker container:

```bash
fluxwright serve \
  --listen 0.0.0.0:7331 \
  --max-browsers 2 \
  --max-contexts 6 \
  --prewarm-contexts 2 \
  --memory-ceiling-mb 3072 \
  --recycle-rss-mb 1200 \
  --adaptive-concurrency \
  --store /var/lib/fluxwright/jobs.sqlite
```

## 2-hour chaos soak proof

Fluxwright runs an automated weekly 2-hour soak workflow ([`.github/workflows/soak.yml`](../.github/workflows/soak.yml), Sunday `03:00 UTC`, also triggerable on demand via `workflow_dispatch`) backed by `fluxwright-benchmarks --soak-minutes 120 --chaos --assert-soak`:

- **Chaos injection**: Kills a top-level Chromium process every 2 seconds during the soak to continuously verify crash detection, transparent job retry, and process reaping.
- **Enforced health gates (`--assert-soak`)**: Exits non-zero if any gate fails after `engine.shutdown()`:
  - `leaked_processes == 0`: Zero Chromium processes left alive 2 seconds after shutdown.
  - `leaked_temp_dirs == 0`: Zero leftover `fluxwright-profile-*`, `fluxwright-dl-*`, `fluxwright-sess-dl-*`, or `fluxwright-cdp-dl-*` directories in the OS temp dir.
  - `failure_rate == 0.0`: Zero failed jobs across the soak.
  - `rss_slope_mb_per_hour <= 50.0` (configurable via `--max-rss-slope-mb-per-hour`): Post-warmup linear regression RSS slope stays flat.
  - Bounded Linux FD delta (`/proc/self/fd` end minus start `<= 64`).
- **Artifacts**: Always uploads `benchmarks/soak-results/latest.json` (including per-minute `memory_samples_mb`) for trend inspection and post-mortem debugging.

Run a shorter local soak verification before rolling out Chromium upgrades:

```bash
cargo run --release -p fluxwright-benchmarks -- \
  --tools fluxwright --scenario soak --soak-minutes 15 --chaos \
  --assert-soak --max-rss-slope-mb-per-hour 50 --out benchmarks/soak-results
```

## Multi-tenant API keys & quotas (`--tenants-file`)

For shared deployments serving multiple internal teams or customer workloads, configure per-tenant API keys and quotas with `--tenants-file <PATH>` instead of sharing one global `FLUXWRIGHT_SERVER_TOKEN`:

- **Key isolation & scoping**: Each tenant entry binds a distinct high-entropy bearer token (`32–256` printable ASCII characters) to a tenant ID so job ownership, listing, cancellation, and usage accounting stay scoped per tenant. Keep the tenants file owner-only (`chmod 0600`) and outside any web-accessible directory.
- **Quotas & fairness**: Assign per-tenant concurrency caps and rate/job quotas so a bursty tenant cannot starve the shared browser fleet or fill `--max-jobs`.
- **Worker credential separation**: Always set a dedicated `FLUXWRIGHT_WORKER_TOKEN` on `fluxwright coordinate` and `fluxwright serve --join` workers so neither tenant tokens nor `FLUXWRIGHT_SERVER_TOKEN` can call worker lease/heartbeat/result routes.
- **Network egress boundary**: `netguard` blocks `file:` and cloud metadata targets (`169.254.0.0/16`, `fe80::/10`, `fd00:ec2::254`, `metadata.google.internal`) by default, but loopback and RFC1918 networks remain reachable from Chromium unless restricted at the VPC/egress firewall layer. Never disable `netguard` (`FLUXWRIGHT_SERVER_ANY_URL=1`) on multi-tenant fleets.

## Graceful shutdown & cancellation guarantees

- **PID 1 signal forwarding**: Run containers with `tini` (included in the repository `Dockerfile`) or `--init` so `SIGTERM` reaches `fluxwright` and orphaned Chromium children are reaped.
- **Standalone `fluxwright serve`**: On `SIGTERM`/`SIGINT`, stops admitting new HTTP jobs, cancels outstanding work, and shuts down managed Chromium instances cleanly with up to 10 seconds for lease cleanup. SSE streams (`GET /v1/events`) close immediately so open dashboards never block shutdown.
- **Cluster workers (`fluxwright serve --join`)**: Stops pulling new leases, waits up to 10 seconds for in-flight jobs to finish and post results, then calls `POST /v1/workers/{worker}/release` to return unfinished jobs to `queued` without consuming a retry attempt.
- **Client cancellation (`DELETE /v1/jobs/{id}`)**: Idempotent while a job is retained (returns `200` with `status: "cancelled"`, or `409` if already succeeded/failed). In cluster mode, cancelled job IDs propagate to the owning worker on the next 10-second heartbeat (`POST /v1/workers/{worker}/heartbeat`), which aborts the running task and disposes of its browser context.
- **At-least-once semantics**: With `--store` or `--journal`, coordinator restarts, worker crashes (30 s lease expiry), and post-grace worker shutdown releases replay unfinished jobs from step 0. Keep mutating workflows idempotent or omit durable replay when duplicate side effects are unacceptable.

## Cluster topology

```
clients ──HTTPS──▶ TLS proxy ──▶ fluxwright coordinate (--store coord.sqlite)
                                        ▲
                                        │ HTTPS pull (FLUXWRIGHT_WORKER_TOKEN)
                        ┌───────────────┼───────────────┐
                   worker-1         worker-2         worker-N
               (fluxwright serve  (fluxwright serve  (fluxwright serve
                --join https://)   --join https://)   --join https://)
```

- **Coordinator**: Run `fluxwright coordinate` on durable local storage with `--store /var/lib/fluxwright/coord.sqlite --result-ttl-secs 3600` (WAL mode, `0600` permissions) plus cluster-wide polite crawling caps `--max-per-host` and `--min-host-delay-ms`.
- **Workers**: Join stateless browser workers over native `https://` (`rustls` + WebPKI roots) with `FLUXWRIGHT_WORKER_TOKEN`. Workers pull only up to free capacity, need no inbound ports, and can scale horizontally behind your orchestrator. Use `affinity_key` on `POST /v1/jobs` when follow-up jobs benefit from warm cache/session locality on the same worker.
- **Verification**: Benchmark your cluster sizing and `SIGKILL` recovery SLA with [`benchmarks/cluster-bench.mjs`](../benchmarks/cluster-bench.mjs) (see [CLUSTER.md](CLUSTER.md)).
- **Scope boundary**: Fluxwright uses a single coordinator process; multi-coordinator active-active HA is out of scope. Protect the coordinator with fast restart orchestration (systemd/Kubernetes Deployment with persistent volume) so live workers reconnect and resume automatically.

## Prometheus alert expressions

Scrape `GET /metrics` with `Authorization: Bearer <token>`. All metric names below match `crates/fluxwright-server/src/prometheus.rs`:

```yaml
groups:
  - name: fluxwright.rules
    rules:
      # High browser crash rate (indicates OOMs, missing /dev/shm, or hostile pages)
      - alert: FluxwrightBrowserCrashSpike
        expr: rate(fluxwright_crash_count_total[5m]) > 0.05
        for: 5m
        labels:
          severity: warning
        annotations:
          summary: "Elevated Chromium crash rate on {{ $labels.instance }}"

      # Elevated HTTP job failure ratio (> 5% over 10m with meaningful traffic)
      - alert: FluxwrightHighJobFailureRate
        expr: |
          sum(rate(fluxwright_http_jobs_total{status="failed"}[10m]))
          /
          clamp_min(sum(rate(fluxwright_http_jobs_total[10m])), 0.01) > 0.05
        for: 10m
        labels:
          severity: critical
        annotations:
          summary: "Fluxwright job failure rate exceeded 5% over 10m"

      # Job queue nearing configured --max-jobs capacity (> 85%)
      - alert: FluxwrightJobQueueSaturation
        expr: |
          (
            fluxwright_http_jobs_active{status="queued"}
            + fluxwright_http_jobs_active{status="running"}
          ) / clamp_min(fluxwright_http_jobs_capacity, 1) > 0.85
        for: 5m
        labels:
          severity: warning
        annotations:
          summary: "Fluxwright job registry >85% full on {{ $labels.instance }}"

      # Sustained engine pool backpressure (requests waiting for browser slots)
      - alert: FluxwrightPoolQueueBacklog
        expr: fluxwright_queued_requests > 10
        for: 5m
        labels:
          severity: warning
        annotations:
          summary: "Requests queued waiting for browser slots on {{ $labels.instance }}"

      # Combined engine + browser RSS approaching container memory limit (example: 3.4 GiB on 4 GiB node)
      - alert: FluxwrightHighMemoryUsage
        expr: (fluxwright_engine_rss_bytes + fluxwright_browser_tree_rss_bytes) > 3650722201
        for: 5m
        labels:
          severity: warning
        annotations:
          summary: "Fluxwright total RSS exceeded 3.4 GiB on {{ $labels.instance }}"

      # Cluster: no online workers attached to coordinator
      - alert: FluxwrightClusterNoOnlineWorkers
        expr: fluxwright_cluster_workers{status="online"} == 0
        for: 2m
        labels:
          severity: critical
        annotations:
          summary: "Coordinator {{ $labels.instance }} has zero online workers"

      # Cluster: stale workers or repeated lease expirations
      - alert: FluxwrightClusterLeaseExpirations
        expr: |
          fluxwright_cluster_workers{status="stale"} > 0
          or increase(fluxwright_cluster_lease_expirations_total[15m]) >= 3
        for: 2m
        labels:
          severity: warning
        annotations:
          summary: "Cluster worker staleness or lease expirations on {{ $labels.instance }}"

      # Fast-path escalation spike (more than 60% of auto scrape/map requests falling back to Chromium)
      - alert: FluxwrightFastpathFallbackSpike
        expr: |
          rate(fluxwright_fastpath_fallbacks_total[10m])
          /
          clamp_min(
            rate(fluxwright_fastpath_hits_total[10m]) + rate(fluxwright_fastpath_fallbacks_total[10m]),
            0.01
          ) > 0.60
        for: 10m
        labels:
          severity: info
        annotations:
          summary: "High Tier-1 HTTP fast-path fallback ratio on {{ $labels.instance }}"
```
