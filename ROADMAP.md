# Roadmap

The goal is to be the best engine for running many browser jobs on your own machines. That is a narrower job than Playwright's, and the plan stays inside it: Playwright drives a browser, Fluxwright runs the fleet.

## How we measure "best"

- **Throughput:** jobs per second, and per GB of RAM, against Playwright and Puppeteer on the same Chrome binary. Published benchmarks, reproducible in CI.
- **Reliability:** no leaked Chrome processes, ever. A crashed browser is replaced and its jobs retried within seconds.
- **Easy to switch:** for the common page API, a Playwright job script runs after changing only the import.
- **Fast start:** from `npm install` to the first job in under two minutes on Windows, macOS and Linux.

## Done

- **0.2.2:** `page.evaluate(fn, arg)` as in Playwright; a page dropped without `close()` no longer crashes the process; valid TypeScript declarations.
- **0.3.0:** settings per page with Playwright's option names (user agent, locale, timezone, geolocation, permissions, viewport, device scale factor, color scheme); saved sessions (`page.storageState()` out, `newPage({ storageState })` in, in Playwright's file format); `getByLabel`, `getByPlaceholder`, `getByTestId`, `nth`, `first`, `last`, `filter({ hasText })` and chained locators; `locator.screenshot()`, `locator.boundingBox()`, `locator.evaluate()`; `page.consoleMessages()` and `page.pageErrors()`.
- **0.3.1:** `page.route` with Playwright's `Route` and `Request` (fulfill, continue with changes, abort, fallback; globs, RegExps and functions); `page.on('console' | 'pageerror')`; downloads with `page.waitForDownload()` and `saveAs`.

## 0.3: jobs that need state and control

Logged-in scraping and automation are where teams give up on hand-rolled pools.

- **More events:** `page.on('request' | 'response' | 'popup' | 'dialog')` and `page.waitForResponse`.
- **Transformed iframes:** clicks, boxes and screenshots inside an iframe that CSS scales or rotates use untransformed offsets today; switch to `DOM.getContentQuads`, which accounts for transforms.

## 0.4: Python

Most large-scale scraping is written in Python, the biggest audience Fluxwright doesn't reach yet. PyO3 bindings with the same API, sync and asyncio, and wheels for Windows, macOS and Linux on PyPI.

## 0.5: the fleet at scale

- **Auto-tuning:** pick the number of browsers and pages per browser from free RAM and CPU, instead of hand-tuned numbers.
- **Server mode:** submit jobs over HTTP, so any language can use one fleet.
- **Observability:** a Prometheus endpoint, OpenTelemetry traces per job, and a live fleet dashboard with real data.
- **Failure evidence:** per-job network log (HAR), console output, and a screenshot on failure.
- **Bring your own browsers:** `connectOverCDP` to use browsers you already run or rent.
- **Docker image** with Chrome and Fluxwright.

## Later: many machines

One queue with workers on several hosts: the step from one machine to a cluster.

## Distribution and trust (ongoing)

- Publish the crates on crates.io.
- Prebuilt MCP server binaries on GitHub Releases and npm, so AI agents don't need Rust.
- Node binding tests and type checks in CI, on all three operating systems.
- CONTRIBUTING.md, issue templates and good first issues.

## Not planned

- **Firefox and WebKit:** the fleet features are built on Chrome's DevTools Protocol. We may revisit this through WebDriver BiDi if people ask.
- **A test runner, `expect`, a trace viewer or a recorder:** Playwright Test does this well.
- **Anti-bot evasion.**
