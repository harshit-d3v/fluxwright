# Playwright Compatibility Matrix (`v0.14.0`)

Fluxwright implements Playwright's Chromium page, context, locator, network, tracing, and `expect()` APIs across **Node.js/TypeScript**, **Python (`asyncio` and `sync_api`)**, and **Rust (`BrowserEngine`)**.

> **Fail-loud option checking (Node & Python):** Unlike wrappers that silently ignore unknown options, Fluxwright throws a `TypeError` when an unimplemented Playwright option is passed in Node (see [bindings/node/MISSING.md](../bindings/node/MISSING.md)).

Run the dual-engine parity suite locally against both Playwright and Fluxwright (changing only the imported `chromium` object):

```bash
node tests/parity/playwright-parity.mjs
```

---

## Status Legend

- **Supported**: Behavioral parity with Playwright on Chromium.
- **Partial**: Core behavior matches with documented differences or subsets (listed in Notes).
- **Unsupported**: Not implemented or out of scope.

---

## 1. Engine, Launching, and Contexts

| Playwright API | Node.js | Python (`async`/`sync`) | Rust | Notes |
|---|---|---|---|---|
| `chromium.launch(opts)` | Supported | Supported | `BrowserEngine::builder()` | Adds fleet options (`maxBrowsers`, `memoryCeilingMb`, `queueTimeout`, `maxConcurrencyPerHost`, `minDelayPerHost`, `prewarmContexts`). Supports `executablePath`, `headless`, `args`, `ignoreDefaultArgs`, `channel`. |
| `chromium.connectOverCDP(url)` | Supported | Supported | Supported | Connects over `ws://`, `wss://`, or `http://` `/json/version` discovery. |
| `browser.newPage(opts)` | Supported | Supported | `engine.acquire()` / `engine.run()` | Allocates an isolated incognito context per call on a pooled Chrome. |
| `browser.newContext(opts)` | Supported | Supported | Supported | Supports `proxy`, `userAgent`, `locale`, `timezoneId`, `geolocation`, `permissions`, `viewport`, `deviceScaleFactor`, `colorScheme`, `isMobile`, `hasTouch`, `extraHTTPHeaders`, `ignoreHTTPSErrors`, `javaScriptEnabled`, `offline`, `baseURL`, `httpCredentials`, `bypassCSP`, `storageState`, `recordHar`, `recordVideo`. Plus Fluxwright's `resourcePreset`, `blockResources`, `blockUrlPatterns`. |
| `context.newPage()` / `pages()` | Supported | Supported | Supported | Multi-tab context and popup tracking. |
| `context.cookies()` / `addCookies()` / `clearCookies()` | Supported | Supported | Supported | Supports `name`, `domain`, `path`, `expires`, `httpOnly`, `secure`, `sameSite`, `partitionKey`, `_crHasCrossSiteAncestor`. |
| `context.storageState({ path, indexedDB })` | Supported | Supported | Supported | Playwright JSON format (`cookies`, `origins` with `localStorage` and optional IndexedDB); files transfer between Playwright and Fluxwright. |
| `context.addInitScript()` / `exposeFunction()` | Supported | Supported | Supported | Applies to main frames and popups in the context. |
| `context.newCDPSession(pageOrFrame)` | Supported | Supported | Supported | Raw CDP `send`, `on`, `once`, `off`, `detach` (pass `--site-per-process` when attaching OOPIFs on `chrome-headless-shell`). |
| `context.request` (`APIRequestContext`) | Supported | Supported | Supported | `fetch`, `get`, `post`, `put`, `patch`, `delete`, `head`, `storageState`, `dispose`. |
| `firefox` / `webkit` | Unsupported | Unsupported | Unsupported | Chromium over CDP only. |

---

## 2. Navigation, Frames, Events, and Waits

| Playwright API | Node.js | Python | Rust | Notes |
|---|---|---|---|---|
| `page.goto` / `reload` / `goBack` / `goForward` / `setContent` | Supported | Supported | Supported | Supports `waitUntil: 'load' \| 'domcontentloaded' \| 'networkidle' \| 'commit'` and `timeout` (`0` = wait indefinitely). |
| `page.waitForLoadState` / `waitForURL` / `waitForFunction` / `waitForTimeout` | Supported | Supported (sync/async) | Supported | `waitForFunction` supports `'raf'` or ms polling; `waitForTimeout` added to Node & Python. |
| `page.waitForSelector` | Supported | Supported | Supported | `state: 'attached' \| 'detached' \| 'visible' \| 'hidden'`, `strict`, `timeout`. |
| `page.waitForResponse` / `waitForRequest` | Supported | `expect_response` / `expect_request` | Supported | Accepts URL glob, RegExp, or predicate function. |
| `page.waitForEvent('download')` / `page.waitForDownload` | Supported | `expect_download` / `wait_for_download` | Supported | Node supports Playwright's `page.waitForEvent('download')` as well as `waitForDownload()`. Isolated temp dir cleaned on close. |
| `page.waitForPopup` / `page.waitForFileChooser` / `page.waitForEvent` | Supported | Supported | Supported | Supports `console`, `pageerror`, `request`, `response`, `requestfinished`, `requestfailed`, `dialog`, `download`, `filechooser`, `popup`, `websocket`, `worker`. |
| `page.frames()` / `mainFrame()` / `frame()` | Supported | Supported | Supported | Imperative `Frame` navigation, evaluation, and locators. |
| `page.emulateMedia` / `setViewportSize` / `bringToFront` | Supported | Supported | Supported | `media: 'screen' \| 'print' \| null`, `colorScheme: 'light' \| 'dark' \| 'no-preference' \| null`. |
| `page.close({ runBeforeUnload })` | Partial | Partial | Supported | Unanswered `beforeunload` dialogs auto-accept; `{ runBeforeUnload }` throws `TypeError` in Node. |

---

## 3. Locators, Shadow DOM, and Input Actions

| Playwright API | Node.js | Python | Rust | Notes |
|---|---|---|---|---|
| `getByRole` / `getByText` / `getByLabel` / `getByPlaceholder` / `getByAltText` / `getByTitle` / `getByTestId` | Supported | Supported | Supported | Open Shadow DOM piercing by default (`css:light=` for light DOM only). |
| `locator(selector, options)` / `frameLocator(selector)` | Supported | Supported | Supported | Nested iframes (same- and cross-origin, CSS transformed), `filter({ has, hasNot, hasText, hasNotText })`, `and`, `or`, `nth`, `first`, `last`, `all`. |
| `click` / `dblclick` / `hover` / `tap` / `focus` / `blur` / `scrollIntoViewIfNeeded` | Partial | Partial | Partial | Auto-waits for visibility, stability, and hit target. `button`, `clickCount`, `delay`, `position`, `modifiers`, `force`, `trial`, `timeout` supported; `noWaitAfter` throws `TypeError` in Node. |
| `dragAndDrop` / `locator.dragTo` | Partial | Partial | Partial | Supports `force`, `strict`, `timeout`; `sourcePosition`/`targetPosition` throw `TypeError` in Node. |
| `fill` / `clear` / `press` / `pressSequentially` / `type` / `check` / `uncheck` / `setChecked` / `selectOption` / `setInputFiles` | Supported | Supported | Supported | Supports `FilePayload` buffers and disk paths. |
| `page.mouse` / `page.keyboard` / `page.touchscreen` | Supported | Supported | Supported | Raw coordinate move/down/up/click/dblclick/wheel, key down/up/press/type/insertText, touchscreen tap. |
| `textContent` / `innerText` / `innerHTML` / `inputValue` / `getAttribute` / `count` / `allTextContents` / `allInnerTexts` / `is*` / `boundingBox` | Supported | Supported | Supported | Matches Playwright action/query semantics. |

---

## 4. Network, Clock, Capture, and AI Helpers

| Playwright API | Node.js | Python | Rust | Notes |
|---|---|---|---|---|
| `page.route` / `context.route` / `unroute` / `routeFromHAR` | Supported | Supported | Supported | Supports `route.fulfill` (`status`, `headers`, `contentType`, `body`, `json`, `path`, `response`), `route.continue`, `route.abort`, `route.fallback`, `route.fetch`, `{ times }`. |
| `page.clock` (`install`, `setFixedTime`, `setSystemTime`, `pauseAt`, `fastForward`, `runFor`, `resume`) | Supported | Supported | Supported | Deterministic mock `Date` and timers (`setTimeout`, `setInterval`). |
| `page.evaluate` / `locator.evaluate` | Supported | Supported | Supported | JSON-serializable arguments and return values; promises awaited automatically. |
| `page.screenshot` / `locator.screenshot` | Partial | Partial | Supported | PNG & JPEG everywhere (`fullPage`, `clip`, `omitBackground`, `mask`, `maskColor`, `animations`, `caret`, `annotate`). WebP supported in Rust/`fluxwright serve`; `scale` and `style` throw `TypeError` in Node. |
| `page.pdf(options)` | Supported | Supported | Supported | Headless only (matching Playwright); throws on headed browsers. |
| `page.ariaSnapshot` / `page.markdown` | Fluxwright extension | Fluxwright extension | Fluxwright extension | `@eN@rK` interactive element refs, diff snapshots (`{ diff: true }`), Set-of-Marks screenshots, clean Markdown. |

---

## 5. End-to-End Testing, Tracing, Video, and Visual Regression

| Feature | Node.js (`fluxwright/test`) | Python (`pytest` plugin) | Rust | Notes |
|---|---|---|---|---|
| Web-first auto-retrying `expect()` matchers | Supported | Supported | `expect_page` / `expect_locator` | `toBeVisible`, `toBeHidden`, `toBeAttached`, `toBeEnabled`, `toBeDisabled`, `toBeChecked`, `toBeEditable`, `toHaveText`, `toContainText`, `toHaveValue`, `toHaveAttribute`, `toHaveClass`, `toHaveCount`, `toHaveTitle`, `toHaveURL`, `.not`, `soft`, `configure`. |
| Visual regression `toHaveScreenshot` | Supported | Supported | Supported | Dual-background alpha pixel diffing, animation freezing, masks, `FLUXWRIGHT_UPDATE_SNAPSHOTS=1`. |
| Context tracing `.zip` (`context.tracing.start/startChunk/stopChunk/stop`) | Supported | Supported | Supported | Opens directly in Playwright Trace Viewer (`npx playwright show-trace trace.zip`). |
| Video recording (`recordVideo` + `page.video()`) | Supported | Supported | Supported | WebM (when `ffmpeg` is on `PATH`) or built-in MJPEG-in-AVI fallback. |
| Test runner (`fluxwright-test` / `pytest`) | Supported | Supported (`pytest11`) | `cargo test` | Parallel `--workers`, `--shard`, `--retries`, `--grep`, HTML/JSON/list reporters, `createFluxwrightFixtures`. |
| Interactive GUI Codegen / hosted Trace Viewer UI | Unsupported | Unsupported | Unsupported | Use Playwright's Trace Viewer CLI with Fluxwright `.zip` trace bundles. |
