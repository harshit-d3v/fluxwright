(() => {
  if (globalThis.__fwClock) {
    return globalThis.__fwClock;
  }

  const OrigDate = globalThis.Date;
  const origPerf = globalThis.performance;
  const origPerfNow =
    origPerf && typeof origPerf.now === 'function'
      ? origPerf.now.bind(origPerf)
      : () => OrigDate.now();
  const origSetTimeout = globalThis.setTimeout.bind(globalThis);
  const origClearTimeout = globalThis.clearTimeout.bind(globalThis);
  const origSetInterval = globalThis.setInterval.bind(globalThis);
  const origClearInterval = globalThis.clearInterval.bind(globalThis);
  const origRAF =
    typeof globalThis.requestAnimationFrame === 'function'
      ? globalThis.requestAnimationFrame.bind(globalThis)
      : null;
  const origCAF =
    typeof globalThis.cancelAnimationFrame === 'function'
      ? globalThis.cancelAnimationFrame.bind(globalThis)
      : null;
  const OrigPromise = globalThis.Promise;

  const MAX_REAL_TIMER_DELAY = 2147483647;
  const RAF_INTERVAL_MS = 16;

  let installed = false;
  let paused = false;
  let stepping = false;
  let tickNow = 0;
  let anchorPerfReal = origPerfNow();
  let perfZeroTick = -anchorPerfReal;
  let systemOffset = OrigDate.now();
  let fixedTime = null;
  let nextTimerId = 1;
  let pumpId = null;
  const timers = new Map();

  const getTickNow = () => {
    if (paused || stepping) return tickNow;
    return tickNow + Math.max(0, origPerfNow() - anchorPerfReal);
  };

  const currentDateNow = () => {
    if (fixedTime !== null) return fixedTime;
    const raw = getTickNow() + systemOffset;
    // Compensate for f64 ULP drift (~2.4e-4 ms at Unix epoch ~1.7e12) so Date.now() / new Date()
    // never truncate 1ms early after arithmetic with sub-millisecond performance.now() deltas.
    return raw >= 0 ? Math.floor(raw + 5e-4) : Math.ceil(raw - 5e-4);
  };

  const currentPerfNow = () => Math.max(0, getTickNow() - perfZeroTick);

  const snapshot = () => ({
    paused,
    tickTime: getTickNow(),
    systemOffset,
    fixedTime,
    anchorRealMs: OrigDate.now(),
  });

  const resolveTime = (raw) => {
    if (typeof raw === 'number') {
      if (!Number.isFinite(raw)) {
        throw new TypeError(`ClockTime must be a finite number, got ${raw}`);
      }
      return raw;
    }
    if (typeof raw === 'string') {
      if (!raw.trim()) {
        throw new TypeError('ClockTime string must not be empty');
      }
      const parsed = OrigDate.parse(raw);
      if (!Number.isFinite(parsed)) {
        throw new TypeError(`Invalid ClockTime string: ${JSON.stringify(raw)}`);
      }
      return parsed;
    }
    throw new TypeError('ClockTime must be a finite number or date string');
  };

  const resolveTicks = (raw) => {
    if (typeof raw !== 'number' || !Number.isFinite(raw) || raw < 0) {
      throw new TypeError(`ClockTicks must be a non-negative finite number, got ${raw}`);
    }
    return raw;
  };

  const cancelPump = () => {
    if (pumpId !== null) {
      origClearTimeout(pumpId);
      pumpId = null;
    }
  };

  const nextDueTimer = (limit) => {
    let best = null;
    for (const t of timers.values()) {
      if (t.fireAt > limit) continue;
      if (!best || t.fireAt < best.fireAt || (t.fireAt === best.fireAt && t.id < best.id)) {
        best = t;
      }
    }
    return best;
  };

  const earliestTimer = () => {
    let best = null;
    for (const t of timers.values()) {
      if (!best || t.fireAt < best.fireAt || (t.fireAt === best.fireAt && t.id < best.id)) {
        best = t;
      }
    }
    return best;
  };

  const flushMicrotasks = () => OrigPromise.resolve();

  const invokeTimer = (entry) => {
    try {
      if (entry.kind === 'raf') {
        if (typeof entry.fn === 'function') {
          entry.fn(currentPerfNow());
        }
      } else if (typeof entry.fn === 'function') {
        entry.fn(...entry.args);
      } else if (entry.fn != null) {
        (0, eval)(String(entry.fn));
      }
    } catch (err) {
      // Surface uncaught timer errors asynchronously like the browser event loop.
      origSetTimeout(() => {
        throw err;
      }, 0);
    }
  };

  const schedulePump = () => {
    cancelPump();
    if (!installed || paused || stepping) return;
    const earliest = earliestTimer();
    if (!earliest) return;
    const delay = Math.max(0, Math.min(earliest.fireAt - getTickNow(), MAX_REAL_TIMER_DELAY));
    pumpId = origSetTimeout(runPump, delay);
  };

  const runPump = async () => {
    pumpId = null;
    if (!installed || paused || stepping) return;
    stepping = true;
    const baseTick = tickNow;
    const basePerf = anchorPerfReal;
    try {
      while (!paused) {
        const budget = baseTick + Math.max(0, origPerfNow() - basePerf);
        const next = nextDueTimer(budget);
        if (!next) {
          const cur = origPerfNow();
          tickNow = Math.max(tickNow, baseTick + Math.max(0, cur - basePerf));
          anchorPerfReal = cur;
          break;
        }
        timers.delete(next.id);
        tickNow = next.fireAt;
        if (next.kind === 'interval') {
          timers.set(next.id, {
            ...next,
            fireAt: next.fireAt + Math.max(next.interval, 1),
          });
        }
        invokeTimer(next);
        await flushMicrotasks();
      }
    } finally {
      stepping = false;
      if (!paused) schedulePump();
    }
  };

  function FakeDate(year, monthIndex, date, hours, minutes, seconds, ms) {
    if (!new.target) {
      return new OrigDate(currentDateNow()).toString();
    }
    let inst;
    switch (arguments.length) {
      case 0:
        inst = new OrigDate(currentDateNow());
        break;
      case 1:
        inst = new OrigDate(year);
        break;
      default:
        inst = new OrigDate(...arguments);
        break;
    }
    if (new.target !== FakeDate) {
      Object.setPrototypeOf(inst, new.target.prototype);
    }
    return inst;
  }
  Object.defineProperty(FakeDate, 'name', { value: 'Date', configurable: true });
  FakeDate.prototype = OrigDate.prototype;
  Object.setPrototypeOf(FakeDate, OrigDate);
  FakeDate.now = () => Math.trunc(currentDateNow());
  FakeDate.UTC = OrigDate.UTC;
  FakeDate.parse = OrigDate.parse;

  const fakePerfNow = () => currentPerfNow();

  const scheduleTimeoutOrInterval = (kind, fn, delay, args) => {
    let ms = Number(delay);
    if (!Number.isFinite(ms) || ms < 0) ms = 0;
    const id = nextTimerId++;
    const now = getTickNow();
    timers.set(id, {
      id,
      kind,
      fn,
      args,
      fireAt: now + ms,
      interval: kind === 'interval' ? ms : 0,
    });
    if (!paused && !stepping) schedulePump();
    return id;
  };

  const fakeSetTimeout = (fn, delay, ...args) => scheduleTimeoutOrInterval('timeout', fn, delay, args);
  const fakeSetInterval = (fn, delay, ...args) => scheduleTimeoutOrInterval('interval', fn, delay, args);
  const fakeClearTimeout = (id) => {
    const t = timers.get(id);
    if (t && t.kind !== 'raf') timers.delete(id);
  };
  const fakeClearInterval = (id) => {
    const t = timers.get(id);
    if (t && t.kind !== 'raf') timers.delete(id);
  };
  const fakeRAF = (fn) => {
    const id = nextTimerId++;
    timers.set(id, {
      id,
      kind: 'raf',
      fn,
      args: [],
      fireAt: getTickNow() + RAF_INTERVAL_MS,
      interval: 0,
    });
    if (!paused && !stepping) schedulePump();
    return id;
  };
  const fakeCAF = (id) => {
    const t = timers.get(id);
    if (t && t.kind === 'raf') timers.delete(id);
  };

  const patchGlobals = () => {
    if (installed) return;
    installed = true;
    Object.defineProperty(OrigDate.prototype, 'constructor', {
      value: FakeDate,
      configurable: true,
      writable: true,
    });
    Object.defineProperty(globalThis, 'Date', {
      value: FakeDate,
      configurable: true,
      writable: true,
    });
    if (origPerf) {
      try {
        Object.defineProperty(origPerf, 'now', {
          value: fakePerfNow,
          configurable: true,
          writable: true,
        });
      } catch {
        // Ignore if non-configurable in restricted environments.
      }
    }
    globalThis.setTimeout = fakeSetTimeout;
    globalThis.clearTimeout = fakeClearTimeout;
    globalThis.setInterval = fakeSetInterval;
    globalThis.clearInterval = fakeClearInterval;
    if (origRAF) globalThis.requestAnimationFrame = fakeRAF;
    if (origCAF) globalThis.cancelAnimationFrame = fakeCAF;
  };

  const installState = (initialSystemTime, resetPerf) => {
    patchGlobals();
    cancelPump();
    timers.clear();
    paused = false;
    fixedTime = null;
    tickNow = 0;
    anchorPerfReal = origPerfNow();
    systemOffset = initialSystemTime;
    if (resetPerf) {
      perfZeroTick = tickNow - anchorPerfReal;
    }
    schedulePump();
  };

  const ensureInstalled = () => {
    if (!installed) {
      installState(OrigDate.now(), true);
    }
  };

  const fastForwardTicks = async (ticks) => {
    ensureInstalled();
    const target = getTickNow() + ticks;
    const wasStepping = stepping;
    stepping = true;
    cancelPump();
    try {
      const due = [];
      for (const t of timers.values()) {
        if (t.fireAt <= target) due.push(t);
      }
      due.sort((a, b) => a.fireAt - b.fireAt || a.id - b.id);
      tickNow = target;
      anchorPerfReal = origPerfNow();
      for (const next of due) {
        const current = timers.get(next.id);
        if (!current || current !== next) continue;
        timers.delete(next.id);
        if (next.kind === 'interval') {
          const period = Math.max(next.interval, 1);
          const missed = Math.floor((target - next.fireAt) / period) + 1;
          timers.set(next.id, {
            ...next,
            fireAt: next.fireAt + missed * period,
          });
        }
        invokeTimer(next);
        await flushMicrotasks();
      }
    } finally {
      stepping = wasStepping;
      if (!stepping && !paused) schedulePump();
    }
  };

  const runForTicks = async (ticks) => {
    ensureInstalled();
    const target = getTickNow() + ticks;
    const wasStepping = stepping;
    stepping = true;
    cancelPump();
    try {
      while (true) {
        const next = nextDueTimer(target);
        if (!next) break;
        timers.delete(next.id);
        tickNow = next.fireAt;
        if (next.kind === 'interval') {
          timers.set(next.id, {
            ...next,
            fireAt: next.fireAt + Math.max(next.interval, 1),
          });
        }
        invokeTimer(next);
        await flushMicrotasks();
      }
      tickNow = target;
      anchorPerfReal = origPerfNow();
    } finally {
      stepping = wasStepping;
      if (!stepping && !paused) schedulePump();
    }
  };

  const controller = {
    restore(state) {
      patchGlobals();
      cancelPump();
      timers.clear();
      paused = Boolean(state && state.paused);
      fixedTime = state && state.fixedTime != null ? state.fixedTime : null;
      systemOffset = state && typeof state.systemOffset === 'number' ? state.systemOffset : 0;
      const baseTick = state && typeof state.tickTime === 'number' ? state.tickTime : OrigDate.now();
      const anchorReal = state && typeof state.anchorRealMs === 'number' ? state.anchorRealMs : OrigDate.now();
      const elapsed = paused ? 0 : Math.max(0, OrigDate.now() - anchorReal);
      tickNow = baseTick + elapsed;
      anchorPerfReal = origPerfNow();
      perfZeroTick = tickNow;
      if (!paused) schedulePump();
    },

    async command(cmd) {
      switch (cmd.op) {
        case 'install': {
          const t = cmd.time !== undefined && cmd.time !== null ? resolveTime(cmd.time) : OrigDate.now();
          installState(t, !installed);
          return snapshot();
        }
        case 'pauseAt': {
          const t = resolveTime(cmd.time);
          if (!installed) {
            installState(t, true);
            paused = true;
            cancelPump();
          } else {
            fixedTime = null;
            const curSys = getTickNow() + systemOffset;
            if (t > curSys) {
              await fastForwardTicks(t - curSys);
            } else {
              const cur = getTickNow();
              tickNow = cur;
              anchorPerfReal = origPerfNow();
              systemOffset = t - cur;
            }
            paused = true;
            cancelPump();
          }
          return snapshot();
        }
        case 'setFixedTime': {
          const t = resolveTime(cmd.time);
          if (!installed) {
            installState(t, true);
          }
          fixedTime = t;
          return snapshot();
        }
        case 'setSystemTime': {
          const t = resolveTime(cmd.time);
          if (!installed) {
            installState(t, true);
          }
          fixedTime = null;
          systemOffset = t - getTickNow();
          return snapshot();
        }
        case 'fastForward': {
          const ticks = resolveTicks(cmd.ticks);
          await fastForwardTicks(ticks);
          return snapshot();
        }
        case 'runFor': {
          const ticks = resolveTicks(cmd.ticks);
          await runForTicks(ticks);
          return snapshot();
        }
        case 'resume': {
          ensureInstalled();
          if (paused) {
            paused = false;
            anchorPerfReal = origPerfNow();
            schedulePump();
          }
          return snapshot();
        }
        default:
          throw new Error(`Unknown clock operation: ${cmd.op}`);
      }
    },
  };

  globalThis.__fwClock = controller;
  return controller;
})()
