# Migrating to Fluxwright

Fluxwright implements Playwright's Chromium page API on top of an in-process Rust browser fleet (`BrowserEngine`) plus a Tier 1 pure-Rust HTTP fast path. Most single-machine Chromium scripts migrate by swapping the import, while multi-browser scrapers and WebDriver suites remove custom pool and wait boilerplate.

## Decision Check: When to Stay on Playwright or Selenium

Stay on **Playwright** or **Selenium** if your workload strictly depends on:
- **Firefox or Safari (WebKit) browser testing** (Fluxwright is Chromium-only via CDP).
- **Java, C#, or Ruby in-process SDKs** (Fluxwright provides Rust, Node.js, Python, HTTP `/v1`, CDP WebSockets, and MCP).
- **Cloud mobile / real-device farms** (Appium / BrowserStack / Sauce Labs grids).
- **Playwright interactive GUI Codegen** (Fluxwright writes Playwright Trace Viewer-compatible `.zip` traces via `context.tracing`, without shipping its own desktop recorder GUI).

Switch to **Fluxwright** when you run high-concurrency Chromium workloads (scraping, crawling, PDF/screenshot generation, AI browser agents, or parallel Chromium E2E test suites) and want pooling, RSS admission limits, browser recycling, crash retries, and optional pure-Rust HTTP fast paths built in.

---

## 1. From Playwright (Node.js / TypeScript)

Single-page scripts usually need only an import change plus optional `maxBrowsers`:

```diff
- import { chromium } from "playwright";
+ import { chromium } from "fluxwright";

- const browser = await chromium.launch();
+ const browser = await chromium.launch({ maxBrowsers: 4, memoryCeilingMb: 4096 });
  const page = await browser.newPage(); // isolated incognito context on a pooled Chrome
  await page.goto("https://example.com");
  const title = await page.getByRole("heading").textContent();
  await page.close();
  await browser.close();
```

### E2E Test Runner (`fluxwright/test`)

```diff
- import { test, expect } from "@playwright/test";
+ import { test, expect } from "fluxwright/test";

  test("renders heading", async ({ page }) => {
    await page.goto("https://example.com");
    await expect(page.getByRole("heading")).toHaveText("Example Domain");
    await expect(page).toHaveScreenshot("home.png");
  });
```

Run tests in parallel across the pooled browser fleet:

```bash
npx fluxwright-test --workers=4 --retries=1 --reporter=list,html
```

**Important difference on unknown options:** In Node, passing an unimplemented action/launch option throws a `TypeError` instead of silently doing nothing, so unsupported Playwright options fail loudly at startup.

---

## 2. From Playwright or Selenium (Python)

### From `playwright` (Python)

```diff
- from playwright.async_api import async_playwright
+ from fluxwright.async_api import async_playwright

  async with async_playwright() as p:
-     browser = await p.chromium.launch()
+     browser = await p.chromium.launch(max_browsers=4)
      page = await browser.new_page()
      await page.goto("https://example.com")
      print(await page.title())
      await browser.close()
```

Sync scripts swap `from playwright.sync_api import sync_playwright` to `from fluxwright.sync_api import sync_playwright`. Existing `pytest` suites using `page`, `browser`, `context`, and `expect` auto-load the built-in `fluxwright` `pytest11` plugin.

### From Selenium (`selenium` in Python)

Replace `WebDriver` + `WebDriverWait` + manual `driver.quit()` loops with pooled contexts and auto-waiting locators:

```python
# Before (Selenium: 1 driver binary + explicit waits + manual Grid/process restarts)
from selenium import webdriver
from selenium.webdriver.common.by import By
from selenium.webdriver.support.ui import WebDriverWait
from selenium.webdriver.support import expected_conditions as EC

driver = webdriver.Chrome()
try:
    driver.get("https://example.com/login")
    wait = WebDriverWait(driver, 10)
    wait.until(EC.visibility_of_element_located((By.NAME, "email"))).send_keys("a@b.com")
    driver.find_element(By.CSS_SELECTOR, "button[type=submit]").click()
finally:
    driver.quit()
```

```python
# After (Fluxwright: direct CDP pipe, pooled contexts, auto-waiting locators, crash retry)
from fluxwright.sync_api import sync_playwright

with sync_playwright() as p:
    browser = p.chromium.launch(max_browsers=4)
    page = browser.new_page()
    page.goto("https://example.com/login")
    page.get_by_label("Email").fill("a@b.com")
    page.get_by_role("button", name="Sign in").click()
    page.close()
    browser.close()
```

Note for Selenium users running `chrome-headless-shell`: Fluxwright targets `chrome-headless-shell` natively over CDP pipes without needing `chromedriver` version-pinning or Selenium Grid routers/distributors.

---

## 3. From `puppeteer-cluster` (Node.js)

`puppeteer-cluster`'s `CONCURRENCY_CONTEXT` runs one shared Chrome process with unbounded queues and no memory-based recycling. Replace `Cluster.launch()` with `BrowserEngine` pooling via `chromium.launch()`:

```ts
// Before (puppeteer-cluster)
import { Cluster } from "puppeteer-cluster";

const cluster = await Cluster.launch({
  concurrency: Cluster.CONCURRENCY_CONTEXT,
  maxConcurrency: 8,
  retryLimit: 2,
});
await cluster.task(async ({ page, data: url }) => {
  await page.goto(url, { waitUntil: "load" });
  return page.title();
});
```

```ts
// After (Fluxwright: bounded multi-browser M x K fleet, RSS ceiling, crash retry)
import { chromium } from "fluxwright";

const browser = await chromium.launch({
  maxBrowsers: 4,
  maxContextsPerBrowser: 8,
  memoryCeilingMb: 4096,
  recycleAfterJobs: 100,
});

const titles = await Promise.all(
  urls.map(async (url) => {
    const page = await browser.newPage();
    try {
      await page.goto(url, { waitUntil: "load" });
      return await page.title();
    } finally {
      await page.close();
    }
  }),
);
await browser.close();
```

---

## 4. From Crawlee (`PlaywrightCrawler`)

Crawlee defaults to shared browser contexts unless `useIncognitoPages: true` is set, and serializes page creation behind `browser-pool`'s `pLimit(1)` lock. Fluxwright always isolates every job in a fresh context and configures polite per-host limits cleanly:

```ts
import { chromium } from "fluxwright";

const browser = await chromium.launch({
  maxBrowsers: 4,
  maxContextsPerBrowser: 8,
  maxConcurrencyPerHost: 2,
  minDelayPerHostMs: 250,
  recycleAfterJobs: 100,
});
```

For asset blocking (`crawlee`'s `blockRequests()`), pass resource presets in Rust (`ResourcePreset::Scraping`), on `POST /v1/scrape` (`"resource_preset": "scraping"`), or via `page.route("**/*.{png,jpg,css,woff2}", (r) => r.abort())` in Node/Python.

---

## 5. From Firecrawl, Browserbase, Steel.dev, or Browserless

Run a self-hosted Apache-2.0 container (see [deploy/README.md](../deploy/README.md)) exposing Firecrawl-style HTTP endpoints and Steel/Browserbase-style CDP sessions on one server:

```bash
export FLUXWRIGHT_SERVER_TOKEN="$(python -c 'import secrets; print(secrets.token_hex(32))')"
docker compose -f deploy/docker-compose.yml up -d
```

- **Replacing Firecrawl `/v1/scrape` and `/v1/map`:**

```bash
curl -s -X POST http://127.0.0.1:7331/v1/scrape \
  -H "Authorization: Bearer $FLUXWRIGHT_SERVER_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"url":"https://docs.example.com","mode":"auto","formats":["markdown","links"],"block_cookie_banners":true}'
```

`mode: "auto"` hits Tier 1 pure-Rust HTTP + `html5ever` first (~13 MB server RSS, 518+ req/s in CI) and automatically escalates JS SPAs to Tier 3 pooled Chromium.

- **Connecting existing Playwright / Stagehand / Browser-Use scripts over CDP:**
  Call `POST /v1/sessions` (returns `ws_url` + signed `live_view_url`) or connect directly via `chromium.connectOverCDP("ws://127.0.0.1:7331/cdp?token=...")`. Copy-paste starters live in [examples/adapters/](../examples/adapters/).
