# fluxwright

Run hundreds of headless Chrome jobs on one machine, with a Playwright-style API. Each `new_page` is a fresh browser context on a pooled Chrome. The engine queues jobs when every slot is busy, restarts browsers before they bloat, and replaces browsers that crash.

Docs: https://fluxwright.vercel.app

Requires a local Chrome/Chromium binary (`FLUXWRIGHT_CHROMIUM`, `CHROME`, `CHROMIUM`, or a standard install path). For headless jobs, chrome-headless-shell is picked up from `PATH` or Puppeteer's/Playwright's cache and opens pages much faster: `npx @puppeteer/browsers install chrome-headless-shell@stable`.

## Install

```bash
pip install fluxwright
```

Wheels cover Windows x64, macOS (Intel and Apple Silicon) and Linux x86_64, for Python 3.10 and newer. Anywhere else, pip builds from source, which needs Rust.

## Use

asyncio:

```python
import asyncio
from fluxwright.async_api import async_playwright

async def main():
    async with async_playwright() as p:
        browser = await p.chromium.launch(max_browsers=4)
        page = await browser.new_page()
        await page.goto("https://example.com")
        print(await page.get_by_role("heading").text_content())  # "Example Domain"
        print(await page.evaluate("() => location.hostname"))
        png = await page.screenshot(full_page=True)  # bytes
        await page.close()
        await browser.close()

asyncio.run(main())
```

Blocking code:

```python
from fluxwright.sync_api import sync_playwright

with sync_playwright() as p:
    browser = p.chromium.launch()
    page = browser.new_page()
    page.goto("https://example.com")
    print(page.title())
    browser.close()
```

The names are Playwright's, so a Playwright script for the features below runs after changing `playwright.async_api` to `fluxwright.async_api` (or `sync_api`). `chromium` can also be imported directly: `from fluxwright.async_api import chromium`.

Many jobs at once: start them all and let the engine queue them.

```python
async def job(browser, url):
    page = await browser.new_page()
    try:
        await page.goto(url)
        return await page.title()
    finally:
        await page.close()

titles = await asyncio.gather(*(job(browser, url) for url in urls))
```

The sync API runs every call on one event loop in a background thread, so its objects work from any thread: a `ThreadPoolExecutor` of jobs sharing one browser is fine (Playwright's sync API does not allow that).

### API

| Method | What it does |
|---|---|
| `chromium.launch(max_browsers=None, executable_path=None, headless=None, queue_timeout=None)` | Start the pool (4 browsers by default). Headless uses chrome-headless-shell when installed (as Playwright does), else Chrome; `FLUXWRIGHT_CHROMIUM` or `executable_path` overrides. When every slot is busy, `new_page` waits its turn however long the queue is, so starting every job at once works; with `queue_timeout` (ms) it raises `TimeoutError` after that long instead |
| `browser.new_page(...)` | A fresh browser context. Every option applies to this page only: `proxy={"server", "bypass", "username", "password"}`, `user_agent`, `locale`, `timezone_id`, `geolocation={"latitude", "longitude", "accuracy"}` (with `permissions=["geolocation"]`), `permissions`, `viewport={"width", "height"}`, `device_scale_factor`, `color_scheme`, `storage_state` (a dict or a file path) |
| `page.storage_state(path=None)` | Every cookie in the context plus localStorage of the current origin, in Playwright's format. With `path`, also saved as JSON for `new_page(storage_state=path)` |
| `page.goto(url, wait_until=None)` | Navigate. `wait_until`: `load` (default), `domcontentloaded`, `networkidle`, `commit` |
| `page.title()` / `page.content()` | Read |
| `page.click(selector)` / `page.fill(selector, value)` | Scrolls into view, waits until enabled, stable, and not covered. Selector: CSS, `text=Foo`, `text="Exact"`, `role=button[name="Save"]` |
| `page.get_by_role(role, name=None, exact=None)` / `get_by_text(text, exact=None)` / `get_by_label` / `get_by_placeholder` / `get_by_test_id(id)` / `page.locator(selector)` | A `Locator` with `click()`, `fill(v)`, `wait_for()`, `text_content()`, `bounding_box()`, `screenshot(path=None)` and `evaluate("(el, arg) => ...", arg)` |
| `locator.nth(i)` / `.first` / `.last` / `filter(has_text=...)` / `locator.get_by_role(...)` and the other finders | Narrow a locator: by position (negative counts from the end), by contained text (case-insensitive), or by searching inside its matches |
| `page.frame_locator(iframe_selector)` | The same finders inside an iframe, same- or cross-origin; nest with `.frame_locator()` |
| `page.evaluate(expression, arg=None)` | Runs JavaScript in the page and returns its JSON result, awaiting promises. A function (`"(x) => x * 2"`) is called with `arg`; anything else is evaluated as is. A thrown error raises `fluxwright.Error` |
| `page.screenshot(full_page=None, path=None)` | PNG bytes, also written to `path` |
| `page.route(url, handler)` / `page.unroute(url, handler=None)` | Request interception: `url` is a glob (`**/api/*`), a compiled regex or a function of the URL; the handler gets `route` (and `request`, if it takes two arguments) and answers with `route.fulfill(status, headers, body, json, path, content_type)`, `route.continue_(url, method, headers, post_data)`, `route.abort(error_code)` or `route.fallback()`. The handler added last runs first |
| `page.on("console" \| "pageerror", handler)` / `once` / `remove_listener` | Console messages (`.type`, `.text`) and uncaught errors (`fluxwright.Error` with `.name`, `.message`, `.stack`) as they happen |
| `page.expect_download(timeout=None)` / `page.wait_for_download(timeout=None)` | The next download the page finished: `.url`, `.suggested_filename`, `path()`, `save_as(path)`. Files are deleted when the page closes |
| `page.console_messages()` / `page.page_errors()` | What the page, its popups and iframes logged, and the exceptions nothing caught, the last 1000 of each. Fail a job with `assert await page.page_errors() == []` |
| `page.set_viewport_size({"width", "height"})` | CSS-pixel viewport for this page |
| `page.wait_for_selector(selector)` | Waits until visible (Playwright's default state) |
| `page.close()` / `browser.close()` | Dispose the context / shut down |

Timeouts are in milliseconds, as in Playwright. Errors are `fluxwright.Error`, and `fluxwright.TimeoutError` when something ran out of time. Close pages when a job is done; a page you forget is cleaned up when Python collects it.

In the sync API, route handlers run on worker threads and event listeners one at a time on a thread of their own, so both can call the page.

## Develop

```bash
pip install maturin pytest
maturin develop   # needs Rust
pytest            # against a real Chrome
```

## License

Licensed under the [Apache License, Version 2.0](LICENSE).
