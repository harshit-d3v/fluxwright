# Contributing to Fluxwright

Thanks for helping improve Fluxwright! Before starting major work, check [ROADMAP.md](ROADMAP.md) to make sure your idea fits the project's scope.

> **Out of scope ("Not planned" in `ROADMAP.md`):** Firefox or WebKit support, a built-in test runner / `expect` / trace viewer / recorder, and anti-bot evasion.

## Repository layout

- `crates/` — Core Rust engine, CDP layer, HTTP server, cluster mode, CLI, and `fluxwright-mcp`
- `bindings/node/` — NAPI-RS native Node.js binding and TypeScript wrapper (`fluxwright`)
- `bindings/python/` — PyO3 Python binding (`fluxwright.sync_api` and `fluxwright.async_api`)
- `www/` — Documentation and benchmark site (Next.js)
- `benchmarks/` — Reproducible comparison suites against Playwright and Puppeteer

## Prerequisites

- **Rust:** stable toolchain `1.95+` (`rustup`)
- **Browser:** Google Chrome or Chromium installed locally
- **Node.js bindings / site:** Node.js `20+` and `npm`
- **Python bindings:** Python `3.10+`, `maturin`, and `pytest`

## Local verification commands

Run the checks for every layer your change touches before opening a pull request:

```bash
# Rust workspace (tests run serially to avoid Chrome contention)
cargo test --workspace -- --test-threads=1
cargo clippy --workspace --all-targets -- -D warnings

# Node.js binding (from bindings/node/)
npm ci
npm run test:types
npm run build:debug
npm test

# Python binding (from bindings/python/)
maturin develop
pytest

# Docs site (from www/)
npm ci
npm run build
```

## Pull request guidelines

1. **Discuss large changes first:** Open an issue or [Discussion](https://github.com/harshit-d3v/fluxwright/discussions) before rewriting architecture, adding new APIs, or adding dependencies.
2. **Keep PRs focused:** One cohesive fix or feature per PR makes review and bisecting fast. Avoid unrelated refactors or formatting churn.
3. **Add tests:** Bug fixes should include a regression test; non-trivial new behavior should include unit or integration coverage.
4. **AI-assisted contributions:** AI coding tools are fine to use, provided you personally read, run, test, and understand every line in your diff. Unverified or drive-by AI-generated PRs that fail CI or do not match the codebase may be closed without review.

## Community & security

- All participants are expected to follow our [Code of Conduct](CODE_OF_CONDUCT.md).
- To report a security issue privately, follow [SECURITY.md](SECURITY.md) instead of opening a public issue or PR.
