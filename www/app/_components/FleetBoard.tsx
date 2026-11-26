"use client";

import { useEffect, useReducer, useState, type CSSProperties } from "react";

// A simulated 3-tier fleet: incoming jobs hit Tier 1 HTTP fast path first, escalating
// dynamic/browser jobs into five pooled Chrome processes with eight page slots each.
// Illustrative simulation only: numbers are not measured benchmark data.

const SLOTS = 8;
const LIMIT_MB = 900;
const FRESH_MB = 80;
const QUEUE_MAX = 8;

type Slot = "free" | "lease" | "crash";
type Browser = { slots: Slot[]; mb: number; recycling: number | null };
type Fleet = {
  browsers: Browser[];
  queue: number;
  fastHits: number;
  done: number;
  recycled: number;
  retried: number;
  seed: number;
};

// mulberry32 deterministic PRNG so SSR and initial client render match.
function rand(state: Fleet): number {
  let t = (state.seed = (state.seed + 0x6d2b79f5) | 0);
  t = Math.imul(t ^ (t >>> 15), t | 1);
  t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
  return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
}

function initial(): Fleet {
  const pattern = [3, 2, 5, 0, 1];
  const mb = [412, 298, 811, 640, 120];
  return {
    browsers: pattern.map((n, i) => ({
      slots: Array.from({ length: SLOTS }, (_, j) => (j < n ? "lease" : "free")),
      mb: mb[i],
      recycling: null,
    })),
    queue: 3,
    fastHits: 842,
    done: 1284,
    recycled: 3,
    retried: 1,
    seed: 20260930,
  };
}

function step(prev: Fleet): Fleet {
  const s: Fleet = {
    ...prev,
    browsers: prev.browsers.map((b) => ({ ...b, slots: [...b.slots] })),
  };

  // Tier 1 pure-Rust HTTP resolves static/doc requests without touching Chrome slots.
  s.fastHits += 1 + Math.floor(rand(s) * 3);

  for (const b of s.browsers) {
    b.slots = b.slots.map((slot) => {
      if (slot === "crash") return "free";
      if (slot === "lease" && rand(s) < 0.14) {
        s.done += 1;
        b.mb = Math.max(FRESH_MB, b.mb - 24);
        return "free";
      }
      return slot;
    });
    const leased = b.slots.flatMap((slot, i) => (slot === "lease" ? [i] : []));
    if (leased.length && rand(s) < 0.015) {
      b.slots[leased[Math.floor(rand(s) * leased.length)]] = "crash";
      s.retried += 1;
      s.queue = Math.min(QUEUE_MAX, s.queue + 1);
    }
    if (b.recycling === null && b.mb >= LIMIT_MB) b.recycling = 2;
    if (b.recycling !== null && !b.slots.includes("lease")) {
      if (b.recycling === 0) {
        b.recycling = null;
        b.mb = FRESH_MB;
        s.recycled += 1;
      } else {
        b.recycling -= 1;
      }
    }
  }

  s.queue = Math.min(QUEUE_MAX, s.queue + Math.floor(rand(s) * 5));
  while (s.queue > 0) {
    const open = s.browsers
      .filter((b) => b.recycling === null && b.slots.includes("free"))
      .sort(
        (a, b) =>
          a.slots.filter((x) => x === "lease").length - b.slots.filter((x) => x === "lease").length,
      );
    if (!open.length) break;
    const b = open[0];
    b.slots[b.slots.indexOf("free")] = "lease";
    b.mb += 36;
    s.queue -= 1;
  }
  return s;
}

const fmt = new Intl.NumberFormat("en");

export function FleetBoard() {
  const [fleet, tick] = useReducer(step, undefined, initial);
  const [playing, setPlaying] = useState(false);

  // Motion is opt-out when reduced-motion is not active, with explicit pause control.
  useEffect(() => {
    setPlaying(!window.matchMedia("(prefers-reduced-motion: reduce)").matches);
  }, []);

  useEffect(() => {
    if (!playing) return;
    const id = window.setInterval(tick, 750);
    return () => window.clearInterval(id);
  }, [playing]);

  return (
    <figure className="board">
      <div className="board-head">
        <div className="board-title-group">
          <span className="board-kicker">LIVE TOPOLOGY SIMULATION</span>
          <p className="board-title">Hybrid fast-path &amp; pooled Chrome fleet</p>
        </div>
        <button type="button" className="board-toggle" onClick={() => setPlaying((p) => !p)}>
          {playing ? "Pause" : "Play"}
          <span className="visually-hidden"> the fleet animation</span>
        </button>
      </div>

      <div className="board-tier1" aria-hidden="true">
        <div className="tier1-head">
          <span className="tier1-dot" />
          <span className="tier1-name">Tier 1 · Rust HTTP + html5ever</span>
          <span className="tier1-tag">0 browsers launched</span>
        </div>
        <div className="tier1-meter">
          <span className="tier1-bar" />
        </div>
      </div>

      <div className="board-grid" aria-hidden="true">
        <div className="board-row">
          <span className="board-label">Escalated queue</span>
          <span className="queue">
            {Array.from({ length: QUEUE_MAX }, (_, i) => (
              <span key={i} className={i < fleet.queue ? "pill waiting" : "pill"} />
            ))}
          </span>
          <span className="board-mem">{fleet.queue} waiting</span>
        </div>
        {fleet.browsers.map((b, i) => (
          <div className={b.recycling === null ? "board-row" : "board-row recycling"} key={i}>
            <span className="board-label">Chrome {i + 1}</span>
            <span className="slots">
              {b.slots.map((slot, j) => (
                <span key={j} className={`slot ${slot}`} />
              ))}
            </span>
            <span className="board-mem">
              {b.recycling === null ? `${fmt.format(b.mb)} MB` : "recycling"}
              <span
                className="mem-bar"
                style={{ "--fill": `${Math.min(100, (b.mb / LIMIT_MB) * 100)}%` } as CSSProperties}
              />
            </span>
          </div>
        ))}
      </div>

      <dl className="board-stats">
        <div>
          <dt>HTTP fast hits</dt>
          <dd className="stat-accent">{fmt.format(fleet.fastHits)}</dd>
        </div>
        <div>
          <dt>Browser jobs</dt>
          <dd>{fmt.format(fleet.done)}</dd>
        </div>
        <div>
          <dt>Recycled</dt>
          <dd>{fleet.recycled}</dd>
        </div>
        <div>
          <dt>Retried</dt>
          <dd>{fleet.retried}</dd>
        </div>
      </dl>

      <figcaption className="board-caption">
        Static pages resolve in Tier 1 Rust HTTP without waking Chrome. Dynamic SPAs and visual
        jobs lease free contexts across pooled Chromes, drain above {LIMIT_MB} MB, and retry if a
        renderer dies. Illustrative simulation, not measured data.
      </figcaption>

      <ul className="legend" aria-hidden="true">
        <li>
          <span className="legend-dot tier1" /> Tier 1 HTTP hit
        </li>
        <li>
          <span className="slot lease" /> Active slot
        </li>
        <li>
          <span className="slot free" /> Free slot
        </li>
        <li>
          <span className="pill waiting" /> Queued
        </li>
        <li>
          <span className="slot crash" /> Crashed, retried
        </li>
      </ul>
    </figure>
  );
}
