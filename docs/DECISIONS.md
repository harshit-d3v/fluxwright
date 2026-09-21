# Decisions

Picked the simpler option when the spec was silent. See also `CDP_DECISION.md`.

## 2026-09-21 — CDP: types unused, own wire client

`chromiumoxide` `Handler`/`Page` are unsafe for us (`CDP_DECISION.md`). Generated PDL types pull a slow compile and a Page-shaped API we would ignore. **v0 speaks CDP as JSON** (`method` + `params` + `sessionId`) over one WebSocket. Protocol strings match Chromium’s names. If JSON drift bites, we can depend on `chromiumoxide_cdp` without taking Handler.

## 2026-09-21 — `waitForDebuggerOnStart: false`

Auto-attach uses `flatten: true` and re-arms `Target.setAutoAttach` on every attached session. New targets are **not** paused. Pausing requires a resume race we do not need for scraping; it is a common OOPIF hang. Revisit if frames attach too late for `evaluate` on first paint.

## 2026-09-21 — Lease drop is fire-and-forget dispose

`Drop` cannot await. Dropping a lease sends a dispose to the engine task (destroy context, free slot). `close().await` does the same and waits. Cancelled `acquire`/`run` futures drop the lease and still clean up.

## 2026-09-21 — Queue `Wait` vs `Error`

Bounded queue of waiters. **Error**: `acquire` returns `QueueFull` when the queue is at capacity. **Wait**: the caller parks until there is queue space or a lease, and never sees `QueueFull`. Capacity still bounds *queued* work so memory cannot grow without limit from waiter structs… waiters in Wait mode that cannot enter the queue sit on a `Notify` (one extra parking set). Documented as “wait for admission,” not an unbounded job list.

## 2026-09-21 — `max_pages_per_context`

Each lease creates one context and one page. The config exists and is enforced (a lease will not open more pages than the cap). Default is 1.

## 2026-09-21 — Memory ceiling is process-tree RSS

`sysinfo` walks the Chrome pid and descendants. Windows and Linux. This is an estimate: shared mappings can be counted more than once. Named as such in metrics.

## 2026-09-21 — Soak is opt-in

A two-hour soak is `--soak` on `fluxwright-benchmarks`. The default command still writes JSON for the concurrency scenarios to `benchmarks/results/`. Defaulting to 2h would block CI and local iteration.

GitHub-hosted Linux often cannot use the SUID sandbox. CI sets `FLUXWRIGHT_NO_SANDBOX=1`, which is the documented opt-in and logs a warning. Default remains sandboxed.
