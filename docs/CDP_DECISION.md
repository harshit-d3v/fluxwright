# CDP client decision

Researched 2026-09-21. Writing a correct CDP client is months of protocol edge cases. This document evaluates **chromiumoxide** against Fluxwright’s requirements and picks: wrap its protocol + transport, **replace its Handler**.

---

## What we need

From the engine spec, the CDP layer must provide:

1. **One WebSocket per browser with flat session multiplexing** — commands carry `sessionId`; no nested `Target.sendMessageToTarget` (deprecated; crbug.com/991325).
2. **No blocking thread per operation** — Tokio, multiplexed I/O, no `spawn_blocking` around Chrome.
3. **Correct event ordering** — do not assume `Target.targetCreated` arrives before `Target.createTarget`’s response; do not drop `Target.attachedToTarget` for late OOPIFs.
4. **Out-of-process iframe handling** — `Target.setAutoAttach` with `flatten: true`, **re-armed on every attached session**, frames that keep identity across process swaps.
5. **Prompt detection of a dead browser** — WebSocket close, child-process exit, and `Target.targetCrashed` (browser or renderer) must fail in-flight leases quickly and be marked retryable.

The public page API is small. We do not need Playwright’s full protocol surface. We do need the transport to be boringly correct under many concurrent contexts on one Chrome.

---

## What chromiumoxide is

- Crate **0.9.1** (crates.io 2026-02-25), repo [mattsse/chromiumoxide](https://github.com/mattsse/chromiumoxide). Maintainers: Matthias Seitz, Émile Fugulin (Sytten). ~3M downloads, 100+ reverse deps, Tokio-only since 0.8.
- **`chromiumoxide_pdl` / `chromiumoxide_cdp`**: PDL parser + generated command/event types (~60k lines). This is the expensive, uninteresting part of a CDP client.
- **`Connection`**: one WebSocket, submit command with optional `session_id`, stream of `Message::Response | Message::Event`.
- **`Handler`**: must-poll stream that owns targets, sessions, navigations, command timeouts. `Browser` / `Page` talk to it over channels.
- Launch + connect, `Target.setDiscoverTargets`, `AttachToTarget` with **`flatten(true)`**, page init sends `SetAutoAttach { flatten: true, auto_attach: true, wait_for_debugger_on_start: true }`.

Callers must `tokio::spawn` a loop on `handler.next()`. If they do not, everything stalls. That is documented in the README.

---

## Requirement by requirement

### 1. One WebSocket, flat sessions — **met at the wire**

Verified in `src/handler/mod.rs` and `src/handler/target.rs`:

- `conn.submit_command(method, session_id, params)`
- `AttachToTargetParams` built with `.flatten(true)`
- `SetAutoAttachParams` built with `.flatten(true)`
- Incoming events with `event.session_id` are routed to the matching `Session` → `Target`

We would keep this model. Nested `sendMessageToTarget` is not required.

**Caveat (verified).** `Handler::on_attached_to_target` stores `SessionId → TargetId` and overwrites `target.session_id` with the latest attach. Multiple sessions per target are stored in the map, but the `Target` only remembers one current id. Child iframe sessions are not first-class pages (next section).

### 2. No blocking thread per operation — **met**

The Handler is a single `Stream` polled on Tokio. Commands use oneshot channels. There is no thread-per-CDP-call. `headless_chrome` is the counterexample (sync + OS threads); we will not use it.

**Caveat.** If we used `Page` helpers that wait by polling inside a future, that is still async. We must not add `std::thread` or `block_on` on the runtime. chromiumoxide does not force us to.

### 3. Correct event ordering — **not met in Handler**

Verified panic in `Handler::on_response` for `PendingRequest::CreateTarget`:

```text
if let Some(target) = self.targets.get_mut(&resp.target_id) {
    target.set_initiator(tx);
} else {
    // TODO can this even happen?
    panic!("Created target not present")
}
```

`targets` is filled only in `on_target_created`. CDP does **not** promise `Target.targetCreated` before the `createTarget` result. Local Chrome often emits the event first; remote endpoints (reproduced against Browserbase with chromiumoxide 0.8–0.9) can return the command **first**. Result: panic, not an error.

Same class of bug: assuming attach/discover events arrive while a one-shot event drain is running (see agent-browser#1445 for dynamically inserted OOPIFs). A fleet client must treat attach/create/destroy as a **continuous** state machine, with command completions that wait on maps rather than panicking.

**This alone is enough not to call `Handler` / `Browser::new_page` in production.**

### 4. Out-of-process iframes — **not met in Page/Target**

Verified:

- `Target::poll` returns `None` immediately when `!self.is_page()`. Iframe targets have type `"iframe"`, which is `TargetType::Unknown("iframe")`, so they are never initialized, never get FrameManager, and `GetPages` filters them out (`HandlerMessage::GetPages`).
- Issue **#280** (open, 2025-12): `page.frames()` misses cross-origin frames; `SetAutoAttach` from the outer page “didn’t change anything”; attaching manually yields a `SessionId` that can run raw `DOM.getDocument` but `Page` helpers time out; evaluating JS fails with `Either objectId or executionContextId or uniqueContextId must be specified`.
- `SetAutoAttach` is sent once in `page_init_commands` with `wait_for_debugger_on_start(true)`. Chrome’s docs say you **might want to call this recursively** on auto-attached targets. chromiumoxide does not re-arm on child sessions. Combined with `wait_for_debugger_on_start`, child targets can sit paused if nobody resumes them after init.
- PR **#331** (opened 2026-07-23, **still open** on 2026-09-21): large OOPIF patch (session-aware frames, recursive attach, process-swap identity). It is not in 0.9.1. Follow-ups listed in the PR: network setting fan-out, expose_function, preload/stealth replay, gating during `frameAttached` → `attachedToTarget`.

Scraping hits OOPIFs constantly (consent, payments, ads, captcha). We do not need Playwright parity, but `click`/`fill`/`evaluate`/`wait_for_selector` on a locator inside a cross-origin frame must not be a dead end.

Kitewright builds on chromiumoxide and therefore inherits this unless they patched it privately; their public engine description does not claim OOPIF locators.

### 5. Dead browser detection — **partial**

Verified in `Handler::poll_next`:

- WebSocket `Err` (except optional ignore of invalid JSON) → `Poll::Ready(Some(Err(err)))`. If we poll the handler, a dropped socket is visible.
- `Target.targetCrashed` is **not** in the `on_event` match (only created / attached / destroyed / detached). A renderer crash can look like hung commands until `REQUEST_TIMEOUT` (30s default).
- `on_target_destroyed` removes the target with a `// TODO shutdown?` comment — pending oneshots are not all failed immediately.
- A Chrome process that dies without a clean WS close depends on the tungstenite error path; we still want a **child `wait()`** on the browser process tree so SIGKILL is prompt.

Prompt, lease-scoped failure needs our own overlay: process wait + WS error + `Target.targetCrashed` / `Inspector.targetCrashed` + command timeout, all mapped to a retryable `BrowserDead`.

---

## Other constraints (not blockers, still relevant)

| Topic | Finding |
|-------|---------|
| Maintenance | Alive (0.8.0 Nov 2025, 0.9.0/0.9.1 Feb 2026). CDP PDL bumps. Not abandoned. |
| Generated types vs Chrome | Historical serde breakage on new Chrome (`data did not match any variant of untagged enum Message`, issue #243). 0.8+ can ignore invalid messages. We should keep PDL reasonably current; generated types are why we should not write a PDL generator. |
| API shape | `Browser`/`Page`/`Element` are a second Playwright, not a lease. We would wrap them poorly. |
| Connect vs launch | `new_page` race is worse on `Browser::connect` than `launch`. Milestone “later” remote `BrowserSource` would hit this immediately if we used Handler. |
| Handler poll cost | Every wake iterates all targets. Fine for a few pages; we will have many contexts. A session router that is O(events) not O(targets) is preferable. |
| Forks (`chromey`, spider_*) | Fresher CDP, extra stealth/adblock. They do not fix our fleet model and they widen the dependency. |

---

## Options

### A. Use chromiumoxide `Browser`/`Page` as-is

Fastest to a demo. Fails OOPIF, panics on create-target races, slow/unclear crash, Page API we would have to fight. **Reject.**

### B. Fork chromiumoxide and land/finish PR #331

We would own 60k generated lines plus a Page-centric API we do not want. Merging a 13k-line OOPIF PR into a moving upstream is a standing tax. **Reject as the main strategy.** Cherry-pick ideas from #331 (frame identity vs session, init-while-paused, fail pending work on detach) into our session layer.

### C. Write a full CDP client (PDL + WS + sessions + domains)

The spec’s bar: *only if a named limitation blocks us*. The blockers are **Handler/Page**, not the protocol crate. Reimplementing PDL generation and every domain type is the “months of edge cases.” **Reject.**

### D. Thin layer: `chromiumoxide_cdp` + `chromiumoxide_types` + `Connection` (or equivalent WS), **our** multiplexer

- Reuse generated types and a proven WS connection.
- Own: session table, flatten routing, recursive `setAutoAttach`, createTarget without panic, iframe sessions, crash/exit, timeouts, tracing IDs.
- Do not export chromiumoxide `Page`. Fluxwright `PageLease` speaks a small command set through our session API.
- Pin a chromiumoxide version; regenerate types when we bump Chrome.

This is “building on it,” not “writing our own client.”

**Accept.**

---

## Recommendation

**Build on chromiumoxide’s protocol types and WebSocket connection. Do not use `Handler`, `Browser`, or `Page` as the runtime.** Implement `fluxwright-cdp` as that thin layer.

Reasons, mapped to the five needs:

| Need | Why this is enough |
|------|-------------------|
| Flat multiplexed WS | `Connection` already does it; we keep `sessionId` on every call. |
| Non-blocking | Same Tokio connection; one task per browser driving the socket. |
| Event ordering | Our maps: index by `targetId`/`sessionId`; `createTarget` completion waits until the target exists **or** we insert a placeholder from the command result. No panic. Continuous event loop (no one-shot drain). |
| OOPIF | Recursive `setAutoAttach` on attach; treat `iframe` (and workers we care about) as sessions; locators may target a frame’s session. Steal sequencing ideas from PR #331 / Puppeteer FrameManager, do not vendor their Page API. |
| Dead browser | Socket error, `Child` exit, `Target.targetCrashed` on the browser target, and command timeout all produce one retryable error. |

We write our own **session machine**, which is weeks and testable, not our own **PDL stack**, which is months.

If chromiumoxide’s `Connection` fights us (for example invalid-message handling, or attach-without-flatten remaining in some path), we replace only the socket codec and still keep `chromiumoxide_cdp` types. That is still not a from-scratch client.

### Explicit non-goals for `fluxwright-cdp` v0

- Full domain coverage beyond what the small page API needs (Page, Runtime, DOM, Target, Browser, Network, Fetch, Input, plus crashes).
- Stealth, fingerprinting, `Runtime.enable` avoidance.
- Firefox, WebKit, BiDi.
- Compatibility with chromiumoxide `Page` objects.

### Tests that lock the decision

These belong in `fluxwright-cdp` integration tests (local Chromium + tiny HTTP server), not as hopes:

1. `createTarget` path with events reordered (inject or tolerate response-before-event).
2. Cross-origin iframe: locator click/fill/evaluate on `http://127.0.0.1` inside `http://localhost` parent (site isolation on).
3. Dynamically inserted iframe after load still attaches.
4. `kill -9` on the browser PID fails in-flight commands within a tight bound (well under the 30s command timeout).
5. Renderer `Target.targetCrashed` fails that page’s operations without wedging the browser session if the browser process still lives.

---

## Decision

| Choice | Result |
|--------|--------|
| Build on chromiumoxide | **Yes** — types + connection |
| Fork chromiumoxide | **No** |
| Write our own CDP client | **No** — no limitation that blocks us lives in the PDL/types; the limitation is Handler/Page |
| Own session multiplexer | **Yes** — required for ordering, OOPIF, death |

Recorded as the Milestone 0 CDP decision. Implementation starts in Milestone 1 under `crates/fluxwright-cdp/`.
