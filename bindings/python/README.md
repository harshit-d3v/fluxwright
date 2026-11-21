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
| `chromium.launch(max_browsers=None, executable_path=None, headless=None, queue_timeout=None, memory_ceiling_mb=None, max_concurrency_per_host=None, min_delay_per_host=None)` | Start the pool: by default a browser per 4 CPUs with 8 pages each, and a memory budget (`memory_ceiling_mb`) of 75% of the memory free at launch: new pages wait while Chrome uses more. Headless uses chrome-headless-shell when installed (as Playwright does), else Chrome; `FLUXWRIGHT_CHROMIUM` or `executable_path` overrides. When every slot is busy, `new_page` waits its turn however long the queue is, so starting every job at once works; with `queue_timeout` (ms) it raises `TimeoutError` after that long instead, counting a browser start when one is needed. For polite crawling, `max_concurrency_per_host` caps the pages on one host at once across the fleet and `min_delay_per_host` (ms) spaces the `goto` calls to it: a `goto` waits its turn |
| `chromium.connect_over_cdp(endpoint_url)` | Connect to an already-running browser over CDP (`ws://...` or `http://...`) instead of launching local Chrome |
| `browser.new_page(...)` | A fresh browser context. Every option applies to this page only: `proxy={"server", "bypass", "username", "password"}`, `user_agent`, `locale`, `timezone_id`, `geolocation={"latitude", "longitude", "accuracy"}` (with `permissions=["geolocation"]`), `permissions`, `viewport={"width", "height"}`, `device_scale_factor`, `color_scheme`, `is_mobile`, `has_touch`, `extra_http_headers`, `ignore_https_errors`, `java_script_enabled`, `offline`, `base_url`, `http_credentials={"username", "password"}`, `bypass_csp`, `storage_state` (a dict or a file path), `record_har_path`, `record_har_omit_content`, `record_har_content`, `record_har_mode`, `record_har_url_filter` |
| `browser.new_context(...)` | A context with `new_page`'s options, as Playwright's `new_context`: multiple `context.new_page()` tabs and popups share cookies, storage, init scripts, exposed functions and routes; `pages`, `request` (`APIRequestContext`), `route` / `unroute`, `route_from_har`, `add_init_script`, `expose_function`, `storage_state(path=None, indexed_db=False)`, `cookies(urls=None)`, `add_cookies()`, `clear_cookies(name=None, domain=None, path=None)`, `new_cdp_session(page)`, `close()` |
| `page.storage_state(path=None, indexed_db=False)` | Every cookie in the context plus localStorage (and IndexedDB when `indexed_db=True`) of the current origin, in Playwright's format. With `path`, also saved as JSON for `new_page(storage_state=path)` |
| `page.goto(url, wait_until=None)` | Navigate. `wait_until`: `load` (default), `domcontentloaded`, `networkidle`, `commit` |
| `page.title()` / `page.content()` | Read |
| `page.click(selector, ...)` / `dblclick` / `hover` / `focus` / `blur` / `scroll_into_view_if_needed` / `drag_and_drop` / `tap` | Pointer actions (supporting `button`, `click_count`, `delay`, `position`, `modifiers`, `force`, `trial`) that scroll into view and wait until actionable. Selector: CSS, `text=Foo`, `text="Exact"`, `role=button[name="Save"]` |
| `page.fill(selector, value)` / `clear` / `press` / `press_sequentially` / `check` / `uncheck` / `set_checked` / `select_option` / `set_input_files` | Keyboard typing, shortcut chords (`ControlOrMeta+a`), checkboxes/radios/ARIA toggles, `<select>` options, and `<input type="file">` uploads (paths or `{name, mimeType, buffer}`) |
| `page.mouse` / `page.keyboard` / `page.touchscreen` | Low-level pointer (`move`, `down`, `up`, `click`, `dblclick`, `wheel`), keyboard (`down`, `up`, `press`, `type`, `insert_text`), and touch (`tap`) controls in viewport CSS pixels |
| `page.get_by_role(role, name=None, exact=None, checked=None, disabled=None, expanded=None, include_hidden=None, level=None, pressed=None, selected=None)` / `get_by_text` / `get_by_label` / `get_by_placeholder` / `get_by_alt_text` / `get_by_title` / `get_by_test_id` / `page.locator(selector, ...)` | Strict-mode `Locator` (`strict=False` optional on `page.*` shortcuts) with action, reader (`text_content`, `inner_text`, `inner_html`, `input_value`, `get_attribute`), bulk (`count`, `all`, `all_text_contents`, `all_inner_texts`), state (`is_visible`, `is_hidden`, `is_enabled`, `is_disabled`, `is_checked`, `is_editable`), `bounding_box`, `screenshot`, and `evaluate`. Supports `xpath=` / `//` / `..` |
| `locator.nth(i)` / `.first` / `.last` / `filter(has=..., has_not=..., has_text=..., has_not_text=...)` / `and_(other)` / `or_(other)` | Narrow or combine locators (`has_text`, `has_not_text`, and role `name` accept `str` or compiled regex) |
| `page.frame_locator(iframe_selector)` | The same finders inside an iframe, same- or cross-origin; nest with `.frame_locator()` |
| `page.frames` / `main_frame` / `frame(name=..., url=...)` | Imperative `Frame` handles (same-origin and cross-origin OOPIF): `.url`, `.name`, `.page`, `is_detached()`, `.parent_frame`, `.child_frames`, `goto()`, `evaluate()`, `title()`, `content()`, locators |
| `page.on("popup", handler)` / `expect_popup` / `opener` / `bring_to_front` | Capture `window.open()` and `target="_blank"` popups sharing the page's context |
| `page.evaluate(expression, arg=None)` | Runs JavaScript in the page and returns its JSON result, awaiting promises. A function (`"(x) => x * 2"`) is called with `arg`; anything else is evaluated as is. A thrown error raises `fluxwright.Error` |
| `page.screenshot(full_page=None, type=None, quality=None, clip=None, omit_background=None, path=None)` | PNG or JPEG `bytes` (inferred from `path` extension when `type` is omitted); `clip` crops `{x, y, width, height}`, `omit_background` clears the white background, also written to `path` |
| `page.pdf(...)` | Render to PDF `bytes` (headless only): `format`, `width`/`height` (`px`, `in`, `cm`, `mm` or float px), `margin`, `landscape`, `print_background`, `scale` (0.1–2), `page_ranges`, `prefer_css_page_size`, `display_header_footer`, `header_template`, `footer_template`, `outline`, `tagged`, `path` |
| `page.route(url, handler)` / `page.unroute(url, handler=None)` / `page.route_from_har(har, ...)` | Request interception (`route.fulfill`, `route.fetch`, `route.continue_`, `route.abort`, `route.fallback`) and HAR 1.2 replay/recording (`url`, `not_found`, `update`, `update_mode`, `update_content`) |
| `page.on("websocket", handler)` / `expect_websocket` | Inspect WebSocket connections (`ws.url`, `ws.is_closed()`, `ws.on("framesent" \| "framereceived" \| "socketerror" \| "close")`, `ws.expect_event`) |
| `page.workers` / `page.on("worker", handler)` / `expect_worker` | Track and evaluate inside dedicated Web Workers (`worker.url`, `worker.evaluate(...)`, `worker.on("console" \| "close")`, `worker.expect_event`) |
| `page.on("console" \| "pageerror", handler)` / `once` / `remove_listener` / `expect_event` | Console messages (`.type`, `.text`) and uncaught errors (`fluxwright.Error` with `.name`, `.message`, `.stack`) as they happen |
| `page.on("request" \| "response" \| "requestfinished" \| "requestfailed", handler)` | Network traffic of the page, its popups and iframes: `Request` (`.url`, `.method`, `.headers`, `.post_data`, `.resource_type`, `.failure`) and `Response` (`.status`, `.ok`, `.headers`, `.request`, `body()`, `text()`, `json()`) |
| `page.expect_response(url_or_predicate, timeout=None)` / `expect_request` | `with` block: the first match from now on, a glob or regex for the URL or a function of the Response / Request; `info.value` is it. Default timeout 30 s |
| `page.on("dialog", handler)` | Answer `alert`, `confirm`, `prompt` and `beforeunload` with `dialog.accept(prompt_text)` or `dialog.dismiss()`. Without a listener, or when no listener answers, a dialog is dismissed (`beforeunload` accepted), so the page never hangs |
| `page.on("filechooser", handler)` / `page.expect_file_chooser(predicate=None, timeout=None)` | Intercept native file choosers opened by `<input type="file">`: `file_chooser.is_multiple()`, `file_chooser.set_files(files)` |
| `page.expect_download(timeout=None)` / `page.wait_for_download(timeout=None)` | A finished download: `.url`, `.suggested_filename`, `path()`, `save_as(path)`. `expect_download` waits for one the `with` block starts; `wait_for_download` also returns ones that finished before the call, oldest first. Files are deleted when the page closes. `timeout=0` waits as long as it takes |
| `page.console_messages()` / `page.page_errors()` | What the page, its popups and iframes logged, and the exceptions nothing caught, the last 1000 of each. Fail a job with `assert await page.page_errors() == []` |
| `page.set_viewport_size({"width", "height"})` | CSS-pixel viewport for this page |
| `page.emulate_media(media=..., color_scheme=...)` | Changes CSS `media` (`"screen"` \| `"print"` \| `None`) and/or `prefers-color-scheme` (`"light"` \| `"dark"` \| `"no-preference"` \| `None`) |
| `page.wait_for_selector(selector, state=None, timeout=None)` | Waits until `state`: `visible` (the default), `attached`, `hidden` or `detached`, as in Playwright |
| `page.close()` / `browser.close()` | Dispose the context / shut down |

Timeouts are in milliseconds, as in Playwright. `goto`, `click`, `fill`, `wait_for_selector`, `screenshot` and the locator actions take a per-call `timeout=` (0 for none), and `page.set_default_timeout(ms)` / `page.set_default_navigation_timeout(ms)` change the page's defaults (the engine's are 10 s for actions and 30 s for navigation). Errors are `fluxwright.Error`, and `fluxwright.TimeoutError` when something ran out of time.

Close pages when a job is done, in a `finally`, so a job that raises gives its slot back too:

```python
page = await browser.new_page()
try:
    page.on("response", lambda r: print(page, r.status))
    await page.goto(url)
finally:
    await page.close()
```

Don't count on garbage collection instead. A page you drop is freed when Python collects it only if no `page.on` listener or `page.route` handler refers to the page, like the lambda above or a method of an object that holds the page: the page's pending event streams keep those handlers, and with them the page, alive, so its slot in the pool is never given back.

In the sync API, route handlers run on worker threads and each page's event listeners one at a time on that page's own thread, so both can call the page.

## Develop

```bash
pip install maturin pytest
maturin develop   # needs Rust
pytest            # against a real Chrome
```

## License

Licensed under the [Apache License, Version 2.0](LICENSE).
