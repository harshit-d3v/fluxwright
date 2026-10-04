"""Smoke tests against a real Chrome: ``pytest`` after ``maturin develop``. Needs Chrome or
chrome-headless-shell (see the README)."""

import asyncio
import gc
import os
import re
import subprocess
import sys
import threading
import time
import weakref
from concurrent.futures import ThreadPoolExecutor
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

import pytest

from fluxwright import Error, TimeoutError
from fluxwright.async_api import async_playwright
from fluxwright.sync_api import sync_playwright

FETCH_API = "() => fetch('/api').then((r) => r.text())"
CLICK_REPORT = "() => { const a = document.createElement('a'); a.href = '/report'; document.body.append(a); a.click() }"


class Handler(BaseHTTPRequestHandler):
    """Echoes the user agent and languages; /login sets a cookie; /report is a download; /hdr
    echoes x-test; /api is the real API that routes stand in for."""

    def do_GET(self):
        headers = {"Content-Type": "text/html"}
        if self.path == "/report":
            headers = {"Content-Disposition": 'attachment; filename="report.csv"'}
            body = b"a,b\n1,2\n"
        elif self.path == "/hdr":
            headers = {"Content-Type": "text/plain"}
            body = str(self.headers.get("x-test")).encode()
        elif self.path == "/api":
            headers = {"Content-Type": "application/json"}
            body = b'{"real":true}'
        else:
            if self.path == "/login":
                headers["Set-Cookie"] = "sid=s3cret; Path=/"
            body = f"<title>{self.headers.get('user-agent')}|{self.headers.get('accept-language')}</title>".encode()
        self.send_response(200)
        for name, value in headers.items():
            self.send_header(name, value)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass


@pytest.fixture(scope="module")
def base():
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    yield f"http://127.0.0.1:{server.server_address[1]}"
    server.shutdown()


def test_async_api(base, tmp_path):
    asyncio.run(async_smoke(base, tmp_path))


async def async_smoke(base: str, tmp: Path) -> None:
    async with async_playwright() as p:
        browser = await p.chromium.launch(max_browsers=1)

        page = await browser.new_page()
        await page.goto("data:text/html,<title>Hi</title><p>x</p>")
        assert await page.evaluate("document.title") == "Hi"
        assert await page.evaluate("() => document.title") == "Hi"
        assert await page.evaluate("({ a, b }) => a + b", {"a": 2, "b": 3}) == 5
        assert await page.evaluate("async (s) => s.toUpperCase()", "ok") == "OK"
        assert await page.evaluate("xs => xs.map((x) => x * 2)", [1, 2]) == [2, 4]
        assert await page.evaluate("function (x) { return x + 1 }", 1) == 2
        assert await page.evaluate("x => x === null") is True  # None arrives as null, as in Playwright
        # Statements, a trailing semicolon or a trailing comment: read as written.
        assert await page.evaluate("const n = 2; n * 3") == 6
        assert await page.evaluate("document.title;") == "Hi"
        assert await page.evaluate("(x) => x + 1 // add one", 1) == 2
        with pytest.raises(Error, match="boom"):
            await page.evaluate("() => { throw new Error('boom') }")
        await page.close()
        with pytest.raises(Error, match="page closed"):
            await page.title()

        # Per-page emulation, with Playwright's option names.
        emulated = await browser.new_page(
            user_agent="FluxSmoke/1",
            locale="fr-FR",
            timezone_id="Europe/Paris",
            viewport={"width": 600, "height": 500},
            device_scale_factor=2,
            color_scheme="dark",
        )
        await emulated.goto(base)
        assert (await emulated.title()).startswith("FluxSmoke/1|fr-FR")
        settings = await emulated.evaluate(
            "() => [navigator.language, Intl.DateTimeFormat().resolvedOptions().timeZone, innerWidth,"
            " devicePixelRatio, matchMedia('(prefers-color-scheme: dark)').matches].join('|')"
        )
        assert settings == "fr-FR|Europe/Paris|600|2|true"
        await emulated.close()
        with pytest.raises(ValueError, match="color_scheme"):
            await browser.new_page(color_scheme="purple")

        # Log in once, save to a file in a folder that does not exist yet, start another page from it.
        login = await browser.new_page()
        await login.goto(f"{base}/login")
        await login.evaluate("() => localStorage.setItem('token', 't1')")
        file = tmp / ".auth" / "state.json"
        state = await login.storage_state(path=file)
        if os.name == "posix":  # saved cookies are the owner's alone
            assert file.stat().st_mode & 0o077 == 0 and file.parent.stat().st_mode & 0o077 == 0
        assert any(c["name"] == "sid" and c["value"] == "s3cret" for c in state["cookies"])
        await login.close()
        for saved in (file, str(file), state):
            again = await browser.new_page(storage_state=saved)
            await again.goto(base)
            saved_state = await again.evaluate("() => `${document.cookie}|${localStorage.getItem('token')}`")
            assert saved_state == "sid=s3cret|t1"
            await again.close()

        # Locators, element screenshots, boxes, evaluate on an element, console and page errors.
        ui = await browser.new_page()
        await ui.goto(
            'data:text/html,<body style="margin:0"><label>Email <input id=e></label>'
            '<input placeholder="Your city" id=c>'
            '<b data-testid=total style="position:absolute;left:10px;top:50px;width:40px;height:20px;'
            'background:red">7</b>'
            '<ul><li>Apple <button onclick="document.title=1">Buy</button></li>'
            '<li>Pear <button onclick="document.title=2">Buy</button></li></ul>'
            '<script>console.log("ready", 1); setTimeout(() => { throw new TypeError("late") }, 0)</script>'
        )
        await ui.get_by_label("email").fill("a@b.c")
        await ui.get_by_placeholder("city").fill("Pune")
        assert await ui.evaluate("() => [e.value, c.value].join('|')") == "a@b.c|Pune"
        assert await ui.get_by_test_id("total").text_content() == "7"
        assert (await ui.locator("li").last.text_content()).startswith("Pear")
        await ui.get_by_role("listitem").filter(has_text="apple").get_by_role("button").click()
        assert await ui.title() == "1"
        assert await ui.get_by_test_id("total").bounding_box() == {"x": 10, "y": 50, "width": 40, "height": 20}
        assert await ui.locator("#e").evaluate("(el, suffix) => el.value + suffix", "!") == "a@b.c!"
        assert await ui.locator("#e").evaluate("(el, x) => x === null") is True
        png = await ui.get_by_test_id("total").screenshot(path=tmp / "el.png")
        assert (int.from_bytes(png[16:20], "big"), int.from_bytes(png[20:24], "big")) == (40, 20)
        assert (tmp / "el.png").read_bytes() == png
        for _ in range(50):
            if await ui.page_errors():
                break
            await asyncio.sleep(0.02)
        assert any(m.type == "log" and m.text == "ready 1" for m in await ui.console_messages())
        [late] = await ui.page_errors()
        assert isinstance(late, Error) and late.name == "TypeError" and late.message == "late", repr(late)
        with pytest.raises(Error, match="invalid URL: not a url"):
            await ui.goto("not a url")
        await ui.close()

        # page.on, page.route (glob, regex, function, fallback, unroute) and downloads.
        live = await browser.new_page()
        seen = []
        live.on("console", lambda m: seen.append(f"{m.type}:{m.text}"))
        live.on("pageerror", lambda e: seen.append(f"{e.name}:{e.message}"))
        await live.route("**/api", lambda route: route.fulfill(json={"fake": True}))
        await live.route("**/api", lambda route: route.fallback())  # added last, runs first, passes it on
        await live.route(
            re.compile(r"/hdr$"),
            lambda route, request: route.continue_(headers={**request.headers, "x-test": "routed"}),
        )
        await live.route(lambda url: url.endswith("/gone"), lambda route: route.abort())
        await live.goto(f"{base}/hdr")
        assert await live.evaluate("() => document.body.textContent") == "routed"
        assert await live.evaluate(FETCH_API) == '{"fake": true}'
        assert await live.evaluate("() => fetch('/gone').then(() => 'loaded', () => 'failed')") == "failed"
        await live.unroute("**/api")
        assert await live.evaluate(FETCH_API) == '{"real":true}'
        await live.evaluate("() => { console.warn('careful'); setTimeout(() => { throw new SyntaxError('oops') }) }")
        for _ in range(50):
            if len(seen) >= 2:
                break
            await asyncio.sleep(0.02)
        assert seen == ["warning:careful", "SyntaxError:oops"]
        await live.goto(base)
        async with live.expect_download() as info:
            await live.evaluate(CLICK_REPORT)
        download = await info.value
        assert download.suggested_filename == "report.csv"
        await download.save_as(tmp / "dl" / "report.csv")
        assert (tmp / "dl" / "report.csv").read_text() == "a,b\n1,2\n"
        await live.close()

        # Waiting for a download before the click must not block the click; failed, missing or
        # invalid answers let the request through; a burst of console output arrives whole.
        rv = await browser.new_page()
        await rv.goto(base)
        pending = asyncio.ensure_future(rv.wait_for_download(timeout=10000))
        await rv.evaluate(CLICK_REPORT)
        assert (await pending).suggested_filename == "report.csv"
        await rv.route("**/api", lambda route: route.fulfill(path=tmp / "no-such-file"))
        assert await rv.evaluate(FETCH_API) == '{"real":true}'
        await rv.unroute("**/api")
        await rv.route("**/api", lambda route: None)  # answers nothing
        assert await rv.evaluate(FETCH_API) == '{"real":true}'
        await rv.unroute("**/api")
        rejected = []

        async def bad_status(route):
            try:
                await route.fulfill(status=70000)
            except ValueError as e:
                rejected.append(str(e))

        await rv.route("**/api", bad_status)
        assert await rv.evaluate(FETCH_API) == '{"real":true}'
        assert "status must be between 100 and 599" in rejected[0]
        await rv.unroute("**/api")

        def broken(url):
            raise RuntimeError("matcher broke")

        await rv.route(broken, lambda route: route.fulfill(body="never"))
        assert await rv.evaluate(FETCH_API) == '{"real":true}'  # skipped, not stuck
        await rv.unroute(broken)
        count = 0

        def counted(_):
            nonlocal count
            count += 1

        rv.on("console", counted)
        await rv.evaluate("() => { for (let i = 0; i < 600; i++) console.log('n' + i) }")
        for _ in range(250):
            if count >= 600:
                break
            await asyncio.sleep(0.02)
        assert count == 600
        with pytest.raises(TimeoutError):
            await rv.wait_for_download(timeout=100)
        await rv.close()

        # A second route() made while the first is turning interception on returns after it.
        race = await browser.new_page()
        first = asyncio.ensure_future(race.route("**/api", lambda route: route.fulfill(body="first")))
        await asyncio.sleep(0)
        await race.route("**/other", lambda route: route.abort())
        assert first.done()
        await race.goto(base)
        assert await race.evaluate(FETCH_API) == "first"
        await race.close()

        # A page with listeners and routes that nobody holds is collected, so its slot comes back.
        forgotten = await browser.new_page()
        forgotten.on("console", lambda m: None)
        await forgotten.route("**/api", lambda route: route.continue_())
        gone = weakref.ref(forgotten)
        del forgotten
        gc.collect()
        assert gone() is None

        # Left open on purpose: the context manager's exit closes the browser under it.
        await browser.new_page()


def test_queue():
    asyncio.run(queue_smoke())


async def queue_smoke() -> None:
    async with async_playwright() as p:
        # One browser holds 8 pages; the 300 started after them wait their turn instead of
        # failing (the engine used to refuse jobs past 256 waiting, and give up after 30 s).
        browser = await p.chromium.launch(max_browsers=1)
        held = [await browser.new_page() for _ in range(8)]

        async def job() -> None:
            page = await browser.new_page()
            await page.close()

        waiting = [asyncio.ensure_future(job()) for _ in range(300)]
        await asyncio.sleep(0.5)
        assert not any(task.done() for task in waiting)
        for page in held:
            await page.close()
        await asyncio.gather(*waiting)

        # With queue_timeout, a job that cannot get a slot in time raises TimeoutError.
        hurried = await p.chromium.launch(max_browsers=1, queue_timeout=300)
        held = [await hurried.new_page() for _ in range(8)]
        with pytest.raises(TimeoutError, match="acquire a page lease"):
            await hurried.new_page()
        for page in held:
            await page.close()


def test_sync_api(base, tmp_path):
    with sync_playwright() as p:
        browser = p.chromium.launch(max_browsers=2)
        page = browser.new_page()
        page.goto("data:text/html,<title>Hi</title><ul><li>Apple</li><li>Pear</li></ul>")
        assert page.title() == "Hi"
        assert page.evaluate("(x) => x * 2", 21) == 42
        assert page.locator("li").first.text_content() == "Apple"
        with pytest.raises(Error, match="boom"):
            page.evaluate("() => { throw new Error('boom') }")

        # Handlers run on their own threads, so they can call the page.
        seen = []
        page.on("console", lambda m: seen.append(m.text))
        page.route("**/api", lambda route, request: route.fulfill(json={"method": request.method}))
        page.goto(base)
        assert page.evaluate("() => fetch('/api').then((r) => r.json())") == {"method": "GET"}
        page.evaluate("() => console.log('hello')")
        for _ in range(50):
            if seen:
                break
            time.sleep(0.02)
        assert seen == ["hello"]
        with page.expect_download() as info:
            page.evaluate(CLICK_REPORT)
        info.value.save_as(tmp_path / "report.csv")
        assert (tmp_path / "report.csv").read_text() == "a,b\n1,2\n"
        page.close()

        # One browser shared by a pool of threads.
        def job(n):
            tab = browser.new_page()
            try:
                tab.goto(f"data:text/html,<title>{n}</title>")
                return tab.title()
            finally:
                tab.close()

        with ThreadPoolExecutor(8) as pool:
            assert list(pool.map(job, range(16))) == [str(n) for n in range(16)]


@pytest.mark.parametrize(
    "script",
    [
        "import asyncio\n"
        "from fluxwright.async_api import chromium\n"
        "async def main():\n"
        "    browser = await chromium.launch(max_browsers=1)\n"
        "    page = await browser.new_page()\n"
        "    await page.goto('data:text/html,<title>x</title>')\n"
        "asyncio.run(main())\n"
        "print('exited')\n",
        "from fluxwright.sync_api import chromium\n"
        "browser = chromium.launch(max_browsers=1)\n"
        "page = browser.new_page()\n"
        "page.goto('data:text/html,<title>x</title>')\n"
        "print('exited')\n",
    ],
    ids=["async", "sync"],
)
def test_open_page_at_exit(script, tmp_path):
    """A browser and page left open neither crash nor hang the interpreter on exit."""
    # Files rather than pipes: a Chrome left behind would hold a pipe open and hang the test.
    out, err = tmp_path / "out.txt", tmp_path / "err.txt"
    with out.open("w") as stdout, err.open("w") as stderr:
        done = subprocess.run([sys.executable, "-c", script], stdout=stdout, stderr=stderr, timeout=120)
    assert done.returncode == 0, err.read_text()
    assert out.read_text().strip() == "exited"
