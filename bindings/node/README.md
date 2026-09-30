# fluxwright

In-process **Chromium fleet** for Node. Lease API (fresh context per `newPage`), not Playwright parity.

Requires a local Chrome/Chromium binary (`FLUXWRIGHT_CHROMIUM`, `CHROME`, `CHROMIUM`, or a standard install path).

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
console.log(await page.title());
await page.click("css-selector");
await page.fill("input", "value");
const html = await page.content();
const png = await page.screenshot(); // Buffer
await page.close();
await browser.close();
```

`chromium.launch` starts the Fluxwright engine in-process. Each `newPage` is a **new browser context**. Drop/`close` destroys that context and returns the slot.

### API

| Method | What it does |
|---|---|
| `chromium.launch({ maxBrowsers?, executablePath?, headless? })` | Start the pool. Headless uses chrome-headless-shell when installed (as Playwright does), else Chrome; `FLUXWRIGHT_CHROMIUM` or `executablePath` overrides |
| `browser.newPage({ proxy? })` | Acquire a lease (a fresh context). `proxy: { server, bypass?, username?, password? }` applies to this page only |
| `page.goto(url, { waitUntil? })` | Navigate. `waitUntil`: `load` (default), `domcontentloaded`, `networkidle`, `commit`. Event-driven, like Playwright |
| `page.title()` / `page.content()` | Read |
| `page.click(selector)` / `page.fill(selector, value)` | Scrolls into view, waits until enabled, stable, and not covered. Selector: CSS, `text=Foo`, `text="Exact"`, `role=button[name="Save"]` |
| `page.getByRole(role, { name?, exact? })` / `page.getByText(text, { exact? })` / `page.locator(selector)` | `Locator` with `click()`, `fill(v)`, `waitFor()`, `textContent()`. Implicit ARIA roles and accessible names; hidden elements never match a role |
| `page.frameLocator(iframeSelector)` | Same locators inside an iframe, same- or cross-origin; nest with `.frameLocator()` |
| `page.evaluate(expression)` | `Runtime.evaluate`, JSON. A thrown JS error rejects with its message and stack |
| `page.screenshot({ fullPage? })` | PNG `Buffer` |
| `page.setViewportSize({ width, height })` | CSS-pixel viewport for this page |
| `page.waitForSelector(selector)` | Waits until visible (Playwright's default state) |
| `page.close()` / `browser.close()` | Dispose context / shut down |

See `MISSING.md` for Playwright methods that will not be added.

## License

MIT OR Apache-2.0
