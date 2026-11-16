# Many machines (cluster mode)

Status: core coordinator and pulling worker loop implemented in `0.7.0`. Remaining later items: SQLite store, separate worker token, coordinator-wide per-host limits, and a worker table on the dashboard.

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

- **Coordinator** (`fluxwright coordinate [--listen ADDR] [--max-jobs N] [--journal FILE]`): the public job API (`POST /v1/jobs`, `GET` and `DELETE /v1/jobs/{id}`), plus worker lease, heartbeat, and result endpoints, with no browsers of its own.
- **Worker** (`fluxwright serve --join <http://coordinator-url> [--worker-id NAME]`): today's server (defaulting `--listen` to `127.0.0.1:0` when `--join` is passed), plus a pull loop that asks the coordinator for work, heartbeats every 10 seconds, aborts cancelled jobs, and posts outcomes (with failure evidence) back. Uses `FLUXWRIGHT_SERVER_TOKEN` for bearer authentication; place TLS termination on a reverse proxy when crossing untrusted networks.

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
| A worker comes back after its lease expired | Its result call is refused (409: the lease is someone else's now) and it drops the work. One job may have run twice in between: at least once, as with `--journal`. |
| The coordinator restarts | It replays its journal: queued jobs stay queued; running jobs keep their lease for one more period, so workers that are still alive report in and carry on, and the rest are requeued. Workers retry their calls with backoff until it is back. |
| The coordinator's disk is lost | Accepted jobs are lost. Run it on a disk you trust, or see "Later". |
| A job makes Chrome crash every time | The attempt count ends it after `retries`, on whichever workers it lands. |

Side effects can repeat. A job that clicked "buy" and then lost its worker runs again from its
first step on another one. This is the same rule as the single-server journal, and the docs say
so: use the cluster for jobs that are safe to repeat, or set `retries: 0`.

## What the coordinator stores

The journal of today, extended: one line when a job is accepted, one when it is leased (worker,
expiry), one when it ends, with the result. Compacted on start as now. Results are kept for a
set time (default one hour) so a client can poll after the job ends, then dropped.

This is the point where a database earns its place: results, leases and expiry are queries, not
a log. The proposal is SQLite in one file on the coordinator, behind the same small interface
the journal has now, so a single server keeps the plain file.

## Limits across the fleet

- **Per-host limits** (`--max-per-host`, `--min-host-delay-ms`) are per worker today. Across a
  cluster the coordinator has to enforce them, because it is the only place that sees every
  job. It reads the host of each job's first `goto` and holds back a job whose host is full or
  too recent. Jobs that visit several hosts are limited on the first one only.
- **Priorities** work as now: the coordinator hands out high before normal before low.
- **Capacity**: `max_jobs` moves to the coordinator, and can be far larger, since a queued job
  there is a few hundred bytes and no browser.

## Security

- Workers authenticate to the coordinator with a worker token, separate from the client token,
  so a leaked client token cannot take jobs or post results.
- Job steps and results cross the network: run the coordinator behind TLS, as SERVER.md already
  says for the single server.
- A worker is trusted with every job it leases. Different trust levels need different clusters.

## Observability

The coordinator's `/metrics` adds: queued jobs, jobs per worker, lease expirations, worker count
and each worker's last heartbeat. The dashboard gets a worker table. Trace ids pass through to
the worker, so one trace covers the queue wait and the run.

## What is deliberately left out

- **A second coordinator.** One coordinator is one point of failure for accepting jobs.
  Removing it means consensus or an external store (Postgres, Redis), which is a different
  product size. If the coordinator is down, workers finish what they hold and report when it
  returns.
- **Pushing work to workers**, which would be faster by one poll interval but needs workers to
  be reachable.
- **Moving a running job** to another worker. A page's state lives in one Chrome.

## Build order, if approved

1. The store interface, with the journal behind it (no behaviour change).
2. The coordinator with the lease, heartbeat and result calls, and SQLite behind the store.
3. `serve --join`: the pull loop on the worker.
4. Lease expiry and requeue, with a test that kills a worker mid-job.
5. Per-host limits and metrics on the coordinator.

## Questions for review

1. Is one coordinator acceptable, or is a shared store (Postgres or Redis) required from the
   start?
2. Is at-least-once acceptable for your jobs, or do some need "never twice"?
3. Should results be kept at all on the coordinator, or sent to a webhook when a job ends?
