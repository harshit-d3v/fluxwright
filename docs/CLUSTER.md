# Many machines (experimental cluster mode)

Status: experimental in `0.7.0`. Core coordinator (`fluxwright coordinate`) and pulling worker loop (`fluxwright serve --join`) are implemented. Remaining hardening items: SQLite store, separate worker token, native worker TLS, coordinator-wide per-host limits, and a worker table on the dashboard.

## Goal

One queue, workers on several hosts. A client submits a job to one address and does not care
which machine runs it. Losing a worker loses no accepted job.

Out of scope: autoscaling the machines (use your orchestrator), sharing one browser between
hosts, and exactly-once execution.

## The shape

```
clients ──HTTP──▶ coordinator ◀──HTTP (pull)── worker (fluxwright serve --join)
                  queue + results                worker
                                                 worker
```

Two roles, one binary:

- **Coordinator** (`fluxwright coordinate [--listen ADDR] [--max-jobs N] [--journal FILE]`): the public job API (`POST /v1/jobs`, `GET` and `DELETE /v1/jobs/{id}`), plus worker lease, heartbeat, and result endpoints (not mounted on standalone `fluxwright serve`), with no browsers of its own. Bind a private network interface (for example `10.0.1.10:7331`) when workers run on other hosts.
- **Worker** (`fluxwright serve --join <http://coordinator-url> [--worker-id NAME]`): today's server (defaulting `--listen` to `127.0.0.1:0` when `--join` is passed), plus a pull loop that asks the coordinator for work, heartbeats on a dedicated background task every 10 seconds, aborts cancelled jobs, drains in-flight work on clean shutdown, and posts outcomes (with failure evidence) back. `--join` speaks `http://` only (`https://` is rejected) and authenticates with `FLUXWRIGHT_SERVER_TOKEN`, so the worker-to-coordinator link **must** stay on a trusted private network or pass through a local TLS tunnel/WireGuard VPN.

Workers pull; the coordinator never connects to a worker. So workers need no open port, sit
behind NAT without trouble, and a new one joins by starting.

## How a job moves

1. A client posts a job. The coordinator stores it as `queued` and returns its id.
2. A worker with a free slot calls `POST /v1/workers/{worker}/lease` with how many jobs it can
   take. The coordinator answers with up to that many, by priority then age, and marks each
   `running` with the worker's name and a lease that expires in 30 seconds.
3. While it works, the worker calls `POST /v1/workers/{worker}/heartbeat` every 10 seconds with
   the ids it still holds and each job's progress (`step_index`, `attempts`). That extends the
   leases. The answer lists ids the client cancelled, which the worker aborts.
4. When a job ends, the worker calls `POST /v1/jobs/{id}/result` with the outcome (results or
   error, and failure evidence). The coordinator stores it and the job is over.

A worker asks for only as much as it has room for, so the fleet's own limits (the memory
ceiling, pages per browser) keep working unchanged: a full worker does not ask.

## When things fail

| What fails | What happens |
|---|---|
| A worker dies or loses the network | Its leases expire after 30 seconds without a heartbeat. The coordinator marks those jobs `queued` again and counts an attempt. A job over its `retries` fails with "worker lost". |
| A worker restarts cleanly | It stops leasing new work and waits up to 10 seconds to finish in-flight jobs, then calls `POST /v1/workers/{worker}/release` to return any unfinished jobs to `queued` without counting an attempt (even when `retries: 0`). |
| A worker comes back after its lease expired | Its result call is refused (409: the lease is someone else's now) and it drops the work. One job may have run twice in between: at least once, as with `--journal`. |
| The coordinator restarts | It replays `--journal`: all unfinished jobs (queued and previously running) are queued again with no worker assigned. Live workers abort interrupted running jobs on their next heartbeat (and drop any late results with `409`), so those jobs rerun from the start — even when `retries: 0`. |
| The coordinator's disk is lost | Accepted jobs are lost. Run it on a disk you trust. |
| A job makes Chrome crash every time | The attempt count ends it after `retries`, on whichever workers it lands. |

Side effects can repeat. A job that clicked "buy" and then lost its worker, hit a coordinator restart with `--journal`, or exceeded a worker's 10-second shutdown grace runs again from its first step on another worker. `retries: 0` stops re-runs only after a worker crash or lease expiry, **not** across a coordinator `--journal` restart or a worker shutdown release: use the cluster only for repeatable jobs (or skip `--journal` if you strictly prefer failure over a coordinator-restart rerun).

## Current limits vs planned hardening

Implemented in `0.7.0` (experimental):
- One coordinator process with in-memory bounded job ring (`--max-jobs`) and optional `--journal` file replay for unfinished jobs.
- Pull-based leases (`POST /v1/workers/{worker}/lease`), priority + FIFO ordering, 2 MiB batch size cap per lease response, 30 s lease expiration/requeue, dedicated 10 s heartbeat task (`POST /v1/workers/{worker}/heartbeat`), cancellation propagation, `409 Conflict` rejection on late worker results (`POST /v1/jobs/{id}/result`), and graceful worker drain with post-grace `POST /v1/workers/{worker}/release` back into `queued` without counting an attempt.

Planned for later versions:
- **SQLite store** behind the journal interface, retaining finished results for a configurable TTL across coordinator restarts.
- **Per-host limits across the cluster** (`--max-per-host` / `--min-host-delay-ms` currently apply per worker).
- **Separate worker auth token** and **native `https://` client support** on `--join` (currently `FLUXWRIGHT_SERVER_TOKEN` and unencrypted `http://` only, so keep worker-to-coordinator traffic on a trusted private network or local TLS tunnel).
- **Coordinator worker metrics and dashboard table** (active workers, last heartbeat, lease expirations).

## Design choices

- **Single coordinator:** High-availability consensus or an external Redis/Postgres queue is outside Fluxwright's scope. While the coordinator is down during a restart, live workers finish in-flight steps and retry result calls with backoff; once a restarted coordinator reloads its `--journal` without prior leases, live workers abort previously running jobs on heartbeat and they restart from step 0.
- **At-least-once delivery:** Crashed workers, lease expirations, coordinator restarts with `--journal`, and post-grace worker shutdown releases can replay steps from the start.
- **Bounded retained results:** Finished results remain in the coordinator's `--max-jobs` ring buffer for polling; webhooks and external object stores are out of scope.
