# fluxwright

Run hundreds of headless Chrome jobs on one machine, with a Playwright-style API. Each `newPage` is a fresh browser context on a pooled Chrome. The engine queues jobs when every slot is busy, restarts browsers before they bloat, and replaces browsers that crash.

Docs: https://fluxwright.vercel.app

Requires a local Chrome/Chromium binary (`FLUXWRIGHT_CHROMIUM`, `CHROME`, `CHROMIUM`, or a standard install path). For headless jobs, chrome-headless-shell is picked up from `PATH` or Puppeteer's/Playwright's cache and opens pages much faster: `npx @puppeteer/browsers install chrome-headless-shell@stable`.

## Install

```bash
npm install fluxwright
```

If no prebuild exists for your platform, you need Rust + Chrome, then from this package directory:

```bash
npm run build
```

## Use

```ts
import { chromium } from "fluxwright";

const browser = await chromium.launch({ maxBrowsers: 4 });
const page = await browser.newPage();
await page.goto("https://example.com");
console.log(await page.getByRole("heading").textContent()); // "Example Domain"
console.log(await page.evaluate(() => location.hostname));
const png = await page.screenshot({ fullPage: true }); // Buffer
await page.close();
await browser.close();
```

`chromium.launch` starts the Fluxwright engine in-process. Each `newPage` is a **new browser context**. Drop/`close` destroys that context and returns the slot.

### API

| Method | What it does |
|---|---|
| `chromium.launch({ maxBrowsers?, executablePath?, headless?, queueTimeout? })` | Start the pool. Headless uses chrome-headless-shell when installed (as Playwright does), else Chrome; `FLUXWRIGHT_CHROMIUM` or `executablePath` overrides. When every slot is busy, `newPage` waits its turn however long the queue is, so starting every job at once works; with `queueTimeout` (ms) it rejects after that long instead, counting a browser start when one is needed |
| `browser.newPage(options?)` | Acquire a lease (a fresh context). Every option applies to this page only: `proxy: { server, bypass?, username?, password? }`, `userAgent`, `locale`, `timezoneId`, `geolocation: { latitude, longitude, accuracy? }` (with `permissions: ['geolocation']`), `permissions`, `viewport: { width, height }`, `deviceScaleFactor`, `colorScheme`, `storageState` (an object or a file path) |
| `page.storageState({ path? })` | Every cookie in the context plus localStorage of the current origin, in Playwright's format. With `path`, also saved as JSON for `newPage({ storageState: path })` |
| `page.goto(url, { waitUntil? })` | Navigate. `waitUntil`: `load` (default), `domcontentloaded`, `networkidle`, `commit`. Event-driven, like Playwright |
| `page.title()` / `page.content()` | Read |
| `page.click(selector)` / `page.fill(selector, value)` | Scrolls into view, waits until enabled, stable, and not covered. Selector: CSS, `text=Foo`, `text="Exact"`, `role=button[name="Save"]` |
| `page.getByRole(role, { name?, exact? })` / `page.getByText(text, { exact? })` / `page.getByLabel(text, { exact? })` / `page.getByPlaceholder(text, { exact? })` / `page.getByTestId(id)` / `page.locator(selector)` | `Locator` with `click()`, `fill(v)`, `waitFor()`, `textContent()`, `boundingBox()`, `screenshot({ path? })`, and `evaluate((el, arg) => …, arg?)` in the page's own world. Implicit ARIA roles and accessible names; hidden elements never match a role |
| `locator.nth(i)` / `first()` / `last()` / `filter({ hasText })` / `locator.getByRole(...)` and the other getBy… | Narrow a locator: by position (negative counts from the end), by contained text (case-insensitive), or by searching inside its matches |
| `page.frameLocator(iframeSelector)` | Same locators inside an iframe, same- or cross-origin; nest with `.frameLocator()` |
| `page.evaluate(fn, arg?)` / `page.evaluate(expression)` | Runs a function with a JSON-serializable `arg`, or a string expression, in the page. Awaits promises and returns JSON. As in Playwright, the function is sent as source text, so it can't use variables from Node. A thrown JS error rejects with its message and stack |
| `page.screenshot({ fullPage?, path? })` | PNG `Buffer`, also written to `path` |
| `page.route(url, handler)` / `page.unroute(url, handler?)` | Playwright's request interception: `url` is a glob (`**/api/*`), a RegExp or a function of the URL; the handler gets `(route, request)` and answers with `route.fulfill({ status, headers, body, json, path })`, `route.continue({ url, method, headers, postData })`, `route.abort(code?)` or `route.fallback()`. The handler added last runs first. Requests of popups and iframes too; blocked resource types never reach it |
| `page.on('console' \| 'pageerror', handler)` / `once` / `off` | Console messages (`{ type, text }`) and uncaught errors (`Error`) as they happen |
| `page.waitForDownload({ timeout? })` | The next download the page finished (ones that finished earlier queue up, so call it before or after the click): `url()`, `suggestedFilename()`, `path()`, `saveAs(path)`. Files are deleted when the page closes |
| `page.consoleMessages()` / `page.pageErrors()` | What the page, its popups and iframes logged (`{ type, text }`), and the exceptions nothing caught (`Error` objects), the last 1000 of each. Fail a job with `assert.deepEqual(await page.pageErrors(), [])` |
| `page.setViewportSize({ width, height })` | CSS-pixel viewport for this page |
| `page.waitForSelector(selector)` | Waits until visible (Playwright's default state) |
| `page.close()` / `browser.close()` | Dispose context / shut down |

Close pages when a job is done. A page you forget is cleaned up when Node garbage-collects it, which may be much later.

See `MISSING.md` for Playwright features that are planned or out of scope.

## Develop

```bash
npm install
npm run build   # needs Rust
npm test        # smoke test against a real Chrome
```

## License

Licensed under the [Apache License, Version 2.0](LICENSE). Versions up to 0.2.0 were released under the MIT license.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this project shall be licensed under the Apache License, Version 2.0, without any additional terms or conditions.
