# fluxwright

Run hundreds of headless Chrome jobs on one machine, with a Playwright-style API. Each `newPage` is a fresh browser context on a pooled Chrome. The engine queues jobs when every slot is busy, restarts browsers before they bloat, and replaces browsers that crash.

Docs: https://fluxwright.vercel.app

Requires a local Chrome/Chromium binary (`FLUXWRIGHT_CHROMIUM`, `CHROME`, `CHROMIUM`, or a standard install path). For headless jobs, chrome-headless-shell is picked up from `PATH` or Puppeteer's/Playwright's cache and opens pages much faster: `npx @puppeteer/browsers install chrome-headless-shell@stable`.

## Install

```bash
npm install fluxwright
```

Prebuilds cover Windows x64, macOS x64 and arm64, and Linux x64 (glibc). Elsewhere, `chromium.launch` rejects with the reason the addon did not load; build it from a source checkout instead (needs Rust):

```bash
cd bindings/node
npm install
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
| `chromium.launch({ maxBrowsers?, executablePath?, headless?, queueTimeout?, memoryCeilingMb?, maxConcurrencyPerHost?, minDelayPerHost? })` | Start the pool: by default a browser per 4 CPUs with 8 pages each, and a memory budget (`memoryCeilingMb`) of 75% of the memory free at launch: new pages wait while Chrome uses more. Headless uses chrome-headless-shell when installed (as Playwright does), else Chrome; `FLUXWRIGHT_CHROMIUM` or `executablePath` overrides. When every slot is busy, `newPage` waits its turn however long the queue is, so starting every job at once works; with `queueTimeout` (ms) it rejects after that long instead, counting a browser start when one is needed. For polite crawling, `maxConcurrencyPerHost` caps the pages on one host at once across the fleet and `minDelayPerHost` (ms) spaces the `goto`s to it: a `goto` waits its turn |
| `chromium.connectOverCDP(endpointURL)` | Connect to an already-running browser over CDP (`ws://...` or `http://...`) instead of launching local Chrome |
| `browser.newPage(options?)` | Acquire a lease (a fresh context). Every option applies to this page only: `proxy: { server, bypass?, username?, password? }`, `userAgent`, `locale`, `timezoneId`, `geolocation: { latitude, longitude, accuracy? }` (with `permissions: ['geolocation']`), `permissions`, `viewport: { width, height }`, `deviceScaleFactor`, `colorScheme`, `isMobile`, `hasTouch`, `extraHTTPHeaders`, `ignoreHTTPSErrors`, `javaScriptEnabled`, `offline`, `baseURL`, `httpCredentials: { username, password }`, `bypassCSP`, `storageState` (an object or a file path) |
| `browser.newContext(options?)` | A context with `newPage`'s options, as Playwright's `newContext`: multiple `context.newPage()` tabs and popups share cookies, storage, init scripts, exposed functions and routes; `pages()`, `route` / `unroute`, `addInitScript`, `exposeFunction`, `storageState`, `cookies()`, `addCookies()`, `clearCookies()`, `close()` |
| `page.storageState({ path? })` | Every cookie in the context plus localStorage of the current origin, in Playwright's format. With `path`, also saved as JSON for `newPage({ storageState: path })` |
| `page.goto(url, { waitUntil? })` | Navigate. `waitUntil`: `load` (default), `domcontentloaded`, `networkidle`, `commit`. Event-driven, like Playwright |
| `page.title()` / `page.content()` | Read |
| `page.click(selector, options?)` / `dblclick` / `hover` / `focus` / `dragAndDrop` / `fill` / `press` / `type` / `check` / `uncheck` / `setChecked` / `selectOption` / `setInputFiles` / `tap` | Pointer, keyboard (`Enter`, `ControlOrMeta+a`, `Shift+Tab`), checkbox/ARIA switch, `<select>`, file upload (`path` or `{ name, mimeType, buffer }`), and touch actions with actionability waits (`button`, `clickCount`, `delay`, `position`, `modifiers`, `force`, `trial`) |
| `page.mouse` / `page.keyboard` / `page.touchscreen` | Direct coordinate/key control: `mouse.move` / `down` / `up` / `click` / `dblclick` / `wheel`, `keyboard.down` / `up` / `press` / `type` / `insertText`, `touchscreen.tap` |
| `page.getByRole(role, { name?, exact?, checked?, disabled?, expanded?, includeHidden?, level?, pressed?, selected? })` / `getByText` / `getByLabel` / `getByPlaceholder` / `getByAltText` / `getByTitle` / `getByTestId` / `page.locator(selector, options?)` | Strict-mode `Locator` (`strict: false` optional on `page.*` shortcuts) with `click`, `dblclick`, `hover`, `focus`, `blur`, `scrollIntoViewIfNeeded`, `dragTo`, `fill`, `press`, `pressSequentially`, `type`, `clear`, `check`, `uncheck`, `setChecked`, `selectOption`, `setInputFiles`, `tap`, `waitFor`, `textContent`, `innerText`, `innerHTML`, `inputValue`, `getAttribute`, `count`, `all`, `allTextContents`, `allInnerTexts`, `isVisible`, `isHidden`, `isEnabled`, `isDisabled`, `isChecked`, `isEditable`, `boundingBox`, `screenshot`, and `evaluate`. Supports `xpath=` / bare `//` / `..` |
| `locator.nth(i)` / `first()` / `last()` / `filter({ has?, hasNot?, hasText?, hasNotText? })` / `and(other)` / `or(other)` | Narrow or combine locators (`hasText` / `hasNotText` / role `name` accept string or RegExp) |
| `page.frameLocator(iframeSelector)` | Same locators inside an iframe, same- or cross-origin; nest with `.frameLocator()` |
| `page.frames()` / `mainFrame()` / `frame(nameOrFilter)` | Imperative `Frame` handles (same-origin and cross-origin OOPIF): `url()`, `name()`, `page()`, `isDetached()`, `parentFrame()`, `childFrames()`, `goto()`, `evaluate()`, `title()`, `content()`, locators |
| `page.on('popup', handler)` / `waitForPopup` / `opener` / `bringToFront` | Capture `window.open()` and `target="_blank"` popups sharing the page's context |
| `page.evaluate(fn, arg?)` / `page.evaluate(expression)` | Runs a function with a JSON-serializable `arg`, or a string expression, in the page. Awaits promises and returns JSON. As in Playwright, the function is sent as source text, so it can't use variables from Node. A thrown JS error rejects with its message and stack |
| `page.screenshot({ fullPage?, type?, quality?, clip?, omitBackground?, path? })` | PNG or JPEG `Buffer` (inferred from `path` extension when `type` is omitted); `clip` crops `{ x, y, width, height }`, `omitBackground` clears the default white background, also written to `path` |
| `page.pdf(options?)` | Render to PDF `Buffer` (headless only): `format` (`Letter`–`A6`), `width`/`height` (`px`, `in`, `cm`, `mm` or px number), `margin`, `landscape`, `printBackground`, `scale` (0.1–2), `pageRanges`, `preferCSSPageSize`, `displayHeaderFooter`, `headerTemplate`, `footerTemplate`, `outline`, `tagged`, `path` |
| `page.route(url, handler)` / `page.unroute(url, handler?)` | Playwright's request interception: `url` is a glob (`**/api/*`), a RegExp or a function of the URL; the handler gets `(route, request)` and answers with `route.fulfill({ status, headers, body, json, path })`, `route.continue({ url, method, headers, postData })`, `route.abort(code?)` or `route.fallback()`. The handler added last runs first. Requests of popups and iframes too; blocked resource types never reach it. Answer before the handler returns (await or return the answer): a request it leaves unanswered continues, and a later answer is ignored with a warning |
| `page.on('console' \| 'pageerror', handler)` / `once` / `off` | Console messages (`{ type, text }`, properties rather than Playwright's methods) and uncaught errors (`Error`) as they happen. A listener that throws or rejects is logged, and the others still run |
| `page.on('request' \| 'response' \| 'requestfinished' \| 'requestfailed', handler)` | Network traffic of the page, its popups and iframes: `Request` (`url()`, `method()`, `headers()`, `postData()`, `resourceType()`, `failure()`) and `Response` (`status()`, `ok()`, `headers()`, `request()`, `body()`, `text()`, `json()`). As in Playwright, one request is one `Request` object in all its events and in `response.request()` |
| `page.waitForResponse(urlOrPredicate, { timeout? })` / `waitForRequest` | The first match from now on: a glob or RegExp for the URL, or a function of the Response / Request. Default timeout 30 s; closing the page rejects it |
| `page.on('dialog', handler)` | Answer `alert`, `confirm`, `prompt` and `beforeunload` with `dialog.accept(promptText?)` or `dialog.dismiss()`. Without a listener, or when no listener answers, a dialog is dismissed (`beforeunload` accepted), so the page never hangs |
| `page.on('filechooser', handler)` / `page.waitForFileChooser()` | Intercept `<input type="file">` dialogs: `fileChooser.isMultiple()`, `fileChooser.setFiles(pathsOrPayloads)` |
| `page.waitForDownload({ timeout? })` | The next download the page finished (ones that finished earlier queue up, so call it before or after the click): `url()`, `suggestedFilename()`, `path()`, `saveAs(path)`. Files are deleted when the page closes. `timeout: 0` waits as long as it takes |
| `page.consoleMessages()` / `page.pageErrors()` | What the page, its popups and iframes logged (`{ type, text }`), and the exceptions nothing caught (`Error` objects), the last 1000 of each. Fail a job with `assert.deepEqual(await page.pageErrors(), [])` |
| `page.setViewportSize({ width, height })` | CSS-pixel viewport for this page |
| `page.emulateMedia({ media?, colorScheme? })` | Changes CSS `media` (`screen` \| `print` \| `null`) and/or `prefers-color-scheme` (`light` \| `dark` \| `no-preference` \| `null`) |
| `page.waitForSelector(selector, { state?, timeout? })` | Waits until `state`: `visible` (the default), `attached`, `hidden` or `detached`, as in Playwright |
| `page.close()` / `browser.close()` | Dispose context / shut down. `page.close()` does not wait for calls still running on the page: they reject with `page closed` |

Close pages when a job is done. A page you forget is cleaned up when Node garbage-collects it, which may be much later, also when its listeners and routes use it.

`goto`, `click`, `fill`, `waitForSelector`, `screenshot` and the locator actions take Playwright's per-call `timeout` (ms; 0 for none), and `page.setDefaultTimeout(ms)` / `page.setDefaultNavigationTimeout(ms)` change the page's defaults (the engine's are 10 s for actions and 30 s for navigation).

An option Fluxwright does not implement (screenshot `mask`/`animations`/`caret`, `noWaitAfter`, ...) throws a TypeError rather than being ignored. See `MISSING.md` for these and the Playwright features that are planned or out of scope.

## Develop

```bash
npm install
npm run build   # needs Rust
npm test        # smoke test against a real Chrome
```

## License

Licensed under the [Apache License, Version 2.0](LICENSE). Versions up to 0.2.0 were released under the MIT license.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this project shall be licensed under the Apache License, Version 2.0, without any additional terms or conditions.
