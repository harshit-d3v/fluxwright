"use client";

import { useEffect, useState } from "react";
import { GITHUB, Logo, NPM, PYPI, RELEASES, SECTIONS } from "./site";

function GitHubMark() {
  return (
    <svg viewBox="0 0 16 16" width="16" height="16" aria-hidden="true" focusable="false" fill="currentColor">
      <path d="M8 0C3.58 0 0 3.58 0 8c0 3.54 2.29 6.53 5.47 7.59.4.07.55-.17.55-.38 0-.19-.01-.82-.01-1.49-2.01.37-2.53-.49-2.69-.94-.09-.23-.48-.94-.82-1.13-.28-.15-.68-.52-.01-.53.63-.01 1.08.58 1.23.82.72 1.21 1.87.87 2.33.66.07-.52.28-.87.51-1.07-1.78-.2-3.64-.89-3.64-3.95 0-.87.31-1.59.82-2.15-.08-.2-.36-1.02.08-2.12 0 0 .67-.21 2.2.82.64-.18 1.32-.27 2-.27.68 0 1.36.09 2 .27 1.53-1.04 2.2-.82 2.2-.82.44 1.1.16 1.92.08 2.12.51.56.82 1.27.82 2.15 0 3.07-1.87 3.75-3.65 3.95.29.25.54.73.54 1.48 0 1.07-.01 1.93-.01 2.2 0 .21.15.46.55.38A8.013 8.013 0 0016 8c0-4.42-3.58-8-8-8z" />
    </svg>
  );
}

function ThemeIcon({ theme }: { theme: "light" | "dark" }) {
  if (theme === "dark") {
    return (
      <svg viewBox="0 0 20 20" width="16" height="16" aria-hidden="true" fill="none" stroke="currentColor" strokeWidth="1.6">
        <circle cx="10" cy="10" r="3.5" />
        <path d="M10 2.5v2M10 15.5v2M2.5 10h2M15.5 10h2M4.7 4.7l1.4 1.4M13.9 13.9l1.4 1.4M15.3 4.7l-1.4 1.4M6.1 13.9l-1.4 1.4" strokeLinecap="round" />
      </svg>
    );
  }
  return (
    <svg viewBox="0 0 20 20" width="16" height="16" aria-hidden="true" fill="none" stroke="currentColor" strokeWidth="1.6">
      <path d="M15.8 11.9A6.2 6.2 0 0 1 8.1 4.2a6.4 6.4 0 1 0 7.7 7.7Z" strokeLinejoin="round" />
    </svg>
  );
}

export function SiteHeader() {
  const [open, setOpen] = useState(false);
  const [theme, setTheme] = useState<"light" | "dark">("light");

  useEffect(() => {
    const explicit = document.documentElement.dataset.theme;
    if (explicit === "light" || explicit === "dark") {
      setTheme(explicit);
      return;
    }
    setTheme(window.matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light");
  }, []);

  useEffect(() => {
    if (!open) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") setOpen(false);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [open]);

  function toggleTheme() {
    const next = theme === "dark" ? "light" : "dark";
    setTheme(next);
    document.documentElement.dataset.theme = next;
    try {
      localStorage.setItem("fw-theme", next);
    } catch {
      // Ignore storage failures in private contexts.
    }
  }

  return (
    <>
      <div className="announce-bar">
        <div className="container announce-inner">
          <span className="announce-pill">v0.13.2 Published</span>
          <span className="announce-copy">
            <strong>main</strong> now ships the 3-tier hybrid engine, Rust HTTP fast path, and pluggable light-CDP support.
          </span>
          <a className="announce-link" href={`${RELEASES}/tag/v0.13.2`}>
            Release v0.13.2 →
          </a>
        </div>
      </div>

      <header className="site-header">
        <div className="container header-row">
          <a className="wordmark" href="#top">
            <Logo />
            <span>Fluxwright<span className="wordmark-dot" aria-hidden="true">.</span></span>
          </a>

          <nav id="site-nav" aria-label="Sections" className="site-nav" data-open={open}>
            <ul>
              {SECTIONS.map((s) => (
                <li key={s.href}>
                  <a href={s.href} onClick={() => setOpen(false)}>
                    {s.label}
                  </a>
                </li>
              ))}
            </ul>
          </nav>

          <div className="header-actions">
            <a className="header-link header-registry" href={NPM}>
              npm
            </a>
            <a className="header-link header-registry" href={PYPI}>
              PyPI
            </a>
            <button
              type="button"
              className="theme-toggle"
              onClick={toggleTheme}
              aria-label={theme === "dark" ? "Switch to light theme" : "Switch to dark theme"}
              title={theme === "dark" ? "Light theme" : "Dark theme"}
            >
              <ThemeIcon theme={theme} />
            </button>
            <a className="header-cta ghost" href={GITHUB}>
              <GitHubMark />
              <span>GitHub</span>
            </a>
            <a className="header-cta solid" href="#install">
              Get Started
            </a>
            <button
              type="button"
              className="menu-button"
              aria-expanded={open}
              aria-controls="site-nav"
              onClick={() => setOpen((o) => !o)}
            >
              {open ? "Close" : "Menu"}
            </button>
          </div>
        </div>
      </header>
    </>
  );
}
