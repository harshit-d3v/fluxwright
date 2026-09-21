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
| `chromium.launch({ maxBrowsers? })` | Start the pool |
| `browser.newPage()` | Acquire a lease |
| `page.goto(url)` | Navigate |
| `page.title()` / `page.content()` | Read |
| `page.click(selector)` / `page.fill(selector, value)` | CSS only |
| `page.evaluate(expression)` | `Runtime.evaluate`, JSON |
| `page.screenshot()` | PNG `Buffer` |
| `page.waitForSelector(selector)` | Actionable wait |
| `page.close()` / `browser.close()` | Dispose context / shut down |

See `MISSING.md` for Playwright methods that will not be added.

## License

MIT OR Apache-2.0
