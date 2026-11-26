"use client";

import { useState } from "react";

export type Category =
  | "All"
  | "Hybrid Engine"
  | "Fleet Core"
  | "Testing & E2E"
  | "Cloud & HITL"
  | "AI & MCP";

const CATEGORIES: Category[] = [
  "All",
  "Hybrid Engine",
  "Fleet Core",
  "Testing & E2E",
  "Cloud & HITL",
  "AI & MCP",
];

type Capability = {
  id: string;
  num: string;
  title: string;
  badge: string;
  api: string;
  summary: string;
  categories: Exclude<Category, "All">[];
  href: string;
  tone: "obsidian" | "cobalt" | "moss" | "paper" | "ember" | "slate";
};

const CAPABILITIES: Capability[] = [
  {
    id: "hybrid-tier",
    num: "01",
    title: "3-Tier Hybrid Engine",
    badge: "NEW",
    api: "POST /v1/scrape",
    summary:
      "Static pages resolve via pure Rust HTTP first, then escalate cleanly to an external light-CDP tier or pooled Chrome when scripts or pixels require it.",
    categories: ["Hybrid Engine", "AI & MCP"],
    href: "#code",
    tone: "obsidian",
  },
  {
    id: "ssrf-fastpath",
    num: "02",
    title: "DNS-Pinned HTTP Fetcher",
    badge: "RUST",
    api: "POST /v1/map",
    summary:
      "Pure-Rust rustls + scraper pipeline extracts clean Markdown, text, links, and sitemaps with netguard SSRF validation repeated on every 3xx hop.",
    categories: ["Hybrid Engine", "Cloud & HITL"],
    href: "#code",
    tone: "paper",
  },
  {
    id: "light-cdp",
    num: "03",
    title: "Light-CDP & Low-RAM Mode",
    badge: "CDP",
    api: "--fast-cdp-endpoint",
    summary:
      "Connect pluggable external light-CDP servers (Moli, Obscura, Lightpanda) with -32601 method fallback, plus --low-memory flags and RSS recycling.",
    categories: ["Hybrid Engine", "Fleet Core"],
    href: "#install",
    tone: "slate",
  },
  {
    id: "isolated-context",
    num: "04",
    title: "A Fresh Page Per Job",
    badge: "CORE",
    api: "browser.newPage()",
    summary:
      "Every lease creates an isolated BrowserContext on pooled Chrome so cookies, localStorage, and cache never bleed into sibling jobs.",
    categories: ["Fleet Core"],
    href: "#code",
    tone: "cobalt",
  },
  {
    id: "priority-crash",
    num: "05",
    title: "Priority Queue & Crash Retry",
    badge: "CORE",
    api: "JobOptions::retries",
    summary:
      "Wait in a priority queue or fail fast under load. Parent-tethered Chromes restart fast when a renderer crashes and automatically replay failed jobs.",
    categories: ["Fleet Core"],
    href: "#how",
    tone: "ember",
  },
  {
    id: "cgroup-recycle",
    num: "06",
    title: "Container-Aware RAM Ceiling",
    badge: "FLEET",
    api: "memory_ceiling_mb",
    summary:
      "Auto-sizes to 1 browser per 4 CPUs and 75% of free RAM using cgroup limits in Docker, draining and recycling processes without pausing the fleet.",
    categories: ["Fleet Core", "Cloud & HITL"],
    href: "#how",
    tone: "moss",
  },
  {
    id: "e2e-suite",
    num: "07",
    title: "Built-in E2E Test Batteries",
    badge: "E2E",
    api: "fluxwright/test",
    summary:
      "Auto-retrying expect() assertions, .zip trace archives, WebM/AVI video recording, toHaveScreenshot() diffs, Shadow DOM piercing, and pytest plugin.",
    categories: ["Testing & E2E"],
    href: "#code",
    tone: "paper",
  },
  {
    id: "playwright-parity",
    num: "08",
    title: "Full Playwright-Style API",
    badge: "API",
    api: "page.getByRole()",
    summary:
      "Role locators, cross-origin frameLocator, deterministic page.clock, route({ times }) & fallback, HAR 1.2 record/replay, PDFs, and APIRequestContext.",
    categories: ["Testing & E2E", "Fleet Core"],
    href: "#code",
    tone: "obsidian",
  },
  {
    id: "managed-sessions",
    num: "09",
    title: "Managed Sessions & Replay",
    badge: "CLOUD",
    api: "POST /v1/sessions",
    summary:
      "Reconnectable TTL sessions with persistent profiles, 15-minute searchable Replay streams, redacted HAR/API export, Dashboard modals, and BYOB CDP.",
    categories: ["Cloud & HITL"],
    href: "#code",
    tone: "cobalt",
  },
  {
    id: "live-hitl",
    num: "10",
    title: "Live Screencast & HITL",
    badge: "HITL",
    api: "/v1/live/{id}/embed",
    summary:
      "Stream real-time JPEG frames to /dashboard, issue signed read-only or interactive embed URLs, and hand control to a human with pause_for_human.",
    categories: ["Cloud & HITL"],
    href: "#code",
    tone: "ember",
  },
  {
    id: "cluster-ops",
    num: "11",
    title: "Cluster, Webhooks & OTLP",
    badge: "OPS",
    api: "fluxwright coordinate",
    summary:
      "Scale across workers with lease coordination, SQLite --store, per-host concurrency limits, AIMD throttling, HMAC webhooks, Prometheus, and OTLP spans.",
    categories: ["Cloud & HITL", "Fleet Core"],
    href: "#install",
    tone: "slate",
  },
  {
    id: "ai-mcp",
    num: "12",
    title: "22-Tool MCP & Agent Guards",
    badge: "MCP",
    api: "fluxwright-mcp --http",
    summary:
      "Stdio or Streamable HTTP MCP exposing @eN@rK revision-pinned snapshots, Set-of-Marks screenshots, Markdown, and curl/JSON export_api_calls.",
    categories: ["AI & MCP", "Hybrid Engine"],
    href: "#code",
    tone: "moss",
  },
];

function StageVisual({ item }: { item: Capability }) {
  switch (item.id) {
    case "hybrid-tier":
      return (
        <div className="stage-art stage-hybrid" aria-hidden="true">
          <div className="art-row art-active">
            <span>T1 · RUST HTTP</span>
            <strong>14 ms · hit</strong>
          </div>
          <div className="art-row">
            <span>T2 · LIGHT CDP</span>
            <span>-32601 safe</span>
          </div>
          <div className="art-row">
            <span>T3 · CHROMIUM</span>
            <span>SPA escalate</span>
          </div>
        </div>
      );
    case "ssrf-fastpath":
      return (
        <div className="stage-art stage-doc" aria-hidden="true">
          <div className="art-code-line">GET https://docs.example/guide</div>
          <div className="art-code-line">DNS pin -&gt; 93.184.216.34 ok</div>
          <div className="art-code-line art-block">302 -&gt; 169.254.169.254 BLOCKED</div>
          <div className="art-code-line"># Markdown · 4.2 KB out</div>
        </div>
      );
    case "light-cdp":
      return (
        <div className="stage-art stage-flags" aria-hidden="true">
          <span className="art-chip">--low-memory</span>
          <span className="art-chip">--recycle-rss-mb=512</span>
          <span className="art-chip">--fast-cdp-endpoint</span>
          <span className="art-chip art-chip-accent">Apache-2.0 clean</span>
        </div>
      );
    case "isolated-context":
      return (
        <div className="stage-art stage-matrix" aria-hidden="true">
          {Array.from({ length: 16 }, (_, i) => (
            <span key={i} className={i % 3 === 0 || i === 5 || i === 10 ? "art-cell on" : "art-cell"} />
          ))}
        </div>
      );
    case "priority-crash":
      return (
        <div className="stage-art stage-pipeline" aria-hidden="true">
          <div className="art-badge-row">
            <span className="art-pill-tag hi">HIGH</span>
            <span className="art-pill-tag">NORMAL</span>
            <span className="art-pill-tag">LOW</span>
          </div>
          <div className="art-crash-line">
            <span className="art-x">×</span> renderer died -&gt; re-leased 38 ms
          </div>
        </div>
      );
    case "cgroup-recycle":
      return (
        <div className="stage-art stage-gauge" aria-hidden="true">
          <div className="art-gauge-label">
            <span>cgroup budget 75%</span>
            <span>3,072 MB</span>
          </div>
          <div className="art-gauge-track">
            <span className="art-gauge-fill" />
          </div>
          <div className="art-gauge-sub">drain -&gt; recycle -&gt; fresh 80 MB</div>
        </div>
      );
    case "e2e-suite":
      return (
        <div className="stage-art stage-doc" aria-hidden="true">
          <div className="art-code-line art-pass">✓ expect(locator).toHaveText()</div>
          <div className="art-code-line art-pass">✓ toHaveScreenshot(&quot;home.png&quot;)</div>
          <div className="art-code-line">trace.zip · video.webm · shadow&gt;&gt;</div>
        </div>
      );
    case "playwright-parity":
      return (
        <div className="stage-art stage-code-card" aria-hidden="true">
          <div>page.clock.setFixedTime()</div>
          <div>page.frameLocator(&quot;iframe&quot;)</div>
          <div>page.route(&quot;**&quot;, {"{ times: 1 }"})</div>
        </div>
      );
    case "managed-sessions":
      return (
        <div className="stage-art stage-timeline" aria-hidden="true">
          <div className="art-frames">
            <span />
            <span />
            <span />
            <span />
          </div>
          <div className="art-code-line">replay · HAR 1.2 · redaction on</div>
        </div>
      );
    case "live-hitl":
      return (
        <div className="stage-art stage-hitl" aria-hidden="true">
          <div className="art-viewport">
            <span className="art-cursor" />
            <span className="art-live-pill">LIVE EMBED</span>
          </div>
          <div className="art-hitl-bar">pause_for_human -&gt; interactive</div>
        </div>
      );
    case "cluster-ops":
      return (
        <div className="stage-art stage-cluster" aria-hidden="true">
          <div className="art-node coord">COORDINATOR · SQLite</div>
          <div className="art-node-row">
            <span>w-1 (AIMD)</span>
            <span>w-2 (TLS)</span>
          </div>
        </div>
      );
    case "ai-mcp":
    default:
      return (
        <div className="stage-art stage-mcp" aria-hidden="true">
          <div className="art-ref-pill">@e12@r4 [button &quot;Submit&quot;]</div>
          <div className="art-ref-pill som">SoM #12 · export_api_calls</div>
        </div>
      );
  }
}

export function CapabilityGallery() {
  const [selected, setSelected] = useState<Category>("All");
  const [view, setView] = useState<"grid" | "list">("grid");

  const items =
    selected === "All"
      ? CAPABILITIES
      : CAPABILITIES.filter((c) => c.categories.includes(selected));

  return (
    <section className="section" id="features" aria-labelledby="features-title">
      <div className="container">
        <div className="gallery-intro">
          <div>
            <span className="eyebrow">01 / PLATFORM DIRECTORY</span>
            <h2 id="features-title">
              Every capability, <em className="serif-italic">built for scale</em>.
            </h2>
          </div>
          <p className="gallery-lead">
            Playwright drives one browser cleanly. Running hundreds across machines takes a control
            plane: Tier 1 Rust fast path, cgroup budgets, crash retries, session replay, and 22 MCP tools.
          </p>
        </div>

        {/* Awwwards Nominees recessed pill filter tray */}
        <div className="filter-bar" role="toolbar" aria-label="Filter capabilities by track">
          <div className="filter-pills">
            {CATEGORIES.map((cat) => (
              <button
                key={cat}
                type="button"
                className={selected === cat ? "filter-pill active" : "filter-pill"}
                aria-pressed={selected === cat}
                onClick={() => setSelected(cat)}
              >
                {cat}
              </button>
            ))}
          </div>
          <div className="filter-meta">
            <span className="filter-count-badge" aria-live="polite" title={`${items.length} shown`}>
              {items.length}
            </span>
            <button
              type="button"
              className="filter-reset"
              disabled={selected === "All"}
              onClick={() => setSelected("All")}
            >
              Reset filters
            </button>
          </div>
        </div>

        {/* Subheader strip mirroring Awwwards "Nominees [48K]" + view toggle icons */}
        <div className="subheader-bar">
          <div className="subheader-left">
            <span className="subheader-title">{selected === "All" ? "Capabilities" : selected}</span>
            <span className="count-box">{items.length}</span>
          </div>
          <div className="subheader-right">
            <span className="subheader-hint">Zero anti-bot evasion. Pure Chromium &amp; HTTP precision.</span>
            <div className="view-toggles" role="group" aria-label="Layout view">
              <button
                type="button"
                className={view === "grid" ? "view-btn active" : "view-btn"}
                aria-pressed={view === "grid"}
                aria-label="Gallery grid view"
                onClick={() => setView("grid")}
              >
                <svg viewBox="0 0 16 16" width="15" height="15" aria-hidden="true" fill="currentColor">
                  <rect x="1" y="1" width="6" height="6" rx="1" />
                  <rect x="9" y="1" width="6" height="6" rx="1" />
                  <rect x="1" y="9" width="6" height="6" rx="1" />
                  <rect x="9" y="9" width="6" height="6" rx="1" />
                </svg>
              </button>
              <button
                type="button"
                className={view === "list" ? "view-btn active" : "view-btn"}
                aria-pressed={view === "list"}
                aria-label="Compact specification view"
                onClick={() => setView("list")}
              >
                <svg viewBox="0 0 16 16" width="15" height="15" aria-hidden="true" fill="none" stroke="currentColor" strokeWidth="1.6">
                  <path d="M2 3.5h12M2 8h12M2 12.5h12" strokeLinecap="round" />
                </svg>
              </button>
            </div>
          </div>
        </div>

        {view === "grid" ? (
          <div className="nominee-grid">
            {items.map((item) => (
              <article className="nominee-card" key={item.id}>
                <div className={`nominee-stage tone-${item.tone}`}>
                  <div className="stage-top">
                    <span className="stage-num">{item.num}</span>
                    <span className="stage-api">{item.api}</span>
                  </div>
                  <StageVisual item={item} />
                  <div className="nominee-hover">
                    <span className="hover-cat">
                      {item.categories[0]} · {item.api}
                    </span>
                    <a className="hover-cta" href={item.href}>
                      Inspect code →
                    </a>
                  </div>
                </div>
                <div className="nominee-meta">
                  <div className="nominee-author">
                    <span className={`meta-dot dot-${item.tone}`} aria-hidden="true" />
                    <h3>{item.title}</h3>
                    <span className="tier-tag">{item.badge}</span>
                  </div>
                  <p>{item.summary}</p>
                </div>
              </article>
            ))}
          </div>
        ) : (
          <dl className="features">
            {items.map((item) => (
              <div className="feature" key={item.id}>
                <dt>
                  <span className="tier-tag">{item.badge}</span> {item.title}
                </dt>
                <dd>{item.summary}</dd>
                <dd className="feature-api">
                  <code>{item.api}</code>
                </dd>
              </div>
            ))}
          </dl>
        )}
      </div>
    </section>
  );
}
