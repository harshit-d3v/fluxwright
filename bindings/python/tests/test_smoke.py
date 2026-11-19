"""Smoke tests against a real Chrome: ``pytest`` after ``maturin develop``. Needs Chrome or
chrome-headless-shell (see the README)."""

import asyncio
import base64
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
from urllib.parse import quote, urlparse

import pytest

from fluxwright import Error, TimeoutError
from fluxwright.async_api import async_playwright
from fluxwright.sync_api import sync_playwright

# A page that asks "leave this page?" once its button was clicked (Chrome wants a user gesture).
ARM_BEFOREUNLOAD = (
    "data:text/html,<button onclick=\"onbeforeunload = e => { e.preventDefault(); e.returnValue = '' }\">arm</button>"
)

FETCH_API = "() => fetch('/api').then((r) => r.text())"
CLICK_REPORT = "() => { const a = document.createElement('a'); a.href = '/report'; document.body.append(a); a.click() }"


_nav_lock = threading.Lock()
_nav_hits = 0


class Handler(BaseHTTPRequestHandler):
    """Echoes the user agent and languages; /login sets a cookie; /report and /report2 are
    downloads (report.csv, report2.csv); /hdr echoes x-test; /api is the real API that routes
    stand in for."""

    def do_GET(self):
        global _nav_hits
        status = 200
        headers = {"Content-Type": "text/html"}
        if self.path == "/nav-a":
            with _nav_lock:
                _nav_hits += 1
                n = _nav_hits
            body = f"<title>PyNavA-{n}</title><p id='p'>A</p>".encode()
        elif self.path == "/nav-redir":
            status = 302
            headers = {"Location": "/nav-b"}
            body = b""
        elif self.path == "/nav-b":
            body = b"<title>PyNavB</title><div id='b'>B</div>"
        elif self.path == "/nav-slow":
            time.sleep(0.25)
            headers = {"Content-Type": "text/plain"}
            body = b"slow-ok"
        elif self.path == "/nav-delayed":
            body = b'<script>setTimeout(() => { location.href = "/nav-done" }, 80)</script>'
        elif self.path == "/nav-done":
            body = b"<title>PyNavDone</title>"
        elif self.path == "/init-page":
            body = (
                b'<title>PyInitPage</title>'
                b'<script>window.__seenAtParse = { fromStr: window.__initStr, hasBound: typeof window.ctxFn === "function" }</script>'
                b'<p id="tag-target">Hello</p>'
            )
        elif self.path == "/tag.js":
            headers = {"Content-Type": "application/javascript"}
            body = b"window.__urlScript = (window.__urlScript || 0) + 1;"
        elif self.path == "/tag.css":
            headers = {"Content-Type": "text/css"}
            body = b"#tag-target { color: rgb(255, 0, 0); }"
        elif self.path == "/missing-tag.js":
            status = 404
            body = b"missing"
        elif self.path.startswith("/report"):
            headers = {"Content-Disposition": f'attachment; filename="{self.path[1:]}.csv"'}
            body = b"a,b\n1,2\n"
        elif self.path == "/hdr":
            headers = {"Content-Type": "text/plain"}
            body = str(self.headers.get("x-test")).encode()
        elif self.path == "/api":
            headers = {"Content-Type": "application/json"}
            body = b'{"real":true}'
        elif self.path == "/auth":
            expected = "Basic " + base64.b64encode(b"u:p").decode()
            if self.headers.get("Authorization") != expected:
                status = 401
                headers = {"WWW-Authenticate": 'Basic realm="test"'}
                body = b"denied"
            else:
                headers = {"Content-Type": "text/plain"}
                body = b"authed"
        elif self.path == "/csp":
            headers = {"Content-Type": "text/html", "Content-Security-Policy": "script-src 'none'"}
            body = b'<p id="n">0</p><script>document.getElementById("n").textContent="1"</script>'
        elif self.path == "/opener-page":
            body = (
                b"<title>Opener</title>"
                b"<button id=\"open-btn\" onclick=\"window.open('/popup-one', '_blank')\">Open</button>"
                b'<a id="blank-link" href="/popup-two" target="_blank">Link</a>'
            )
        elif self.path == "/popup-one":
            body = b"<title>Popup1</title><p id='pop'>popup-one</p>"
        elif self.path == "/popup-two":
            body = b"<title>Popup2</title><p id='pop2'>popup-two</p>"
        elif self.path == "/frames-host":
            port = self.server.server_port
            body = (
                f"<title>FramesHost</title>"
                f'<iframe id="same" name="same-fr" src="http://127.0.0.1:{port}/same-child"></iframe>'
                f'<iframe id="cross" name="cross-fr" src="http://localhost:{port}/cross-child"></iframe>'
            ).encode()
        elif self.path == "/same-child":
            body = b'<title>SameTitle</title><button id="sb" onclick="this.textContent=\'clicked-same\'">SameBtn</button>'
        elif self.path == "/same-next":
            body = b"<title>SameNext</title><p id='sn'>navigated-same</p>"
        elif self.path == "/cross-child":
            body = b'<title>CrossTitle</title><button id="cb" onclick="this.textContent=\'clicked-cross\'">CrossBtn</button>'
        else:
            if self.path == "/login":
                headers["Set-Cookie"] = "sid=s3cret; Path=/"
            body = f"<title>{self.headers.get('user-agent')}|{self.headers.get('accept-language')}</title>".encode()
        self.send_response(status)
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


RICH_HTML = (
    '<body style="margin:0;height:2000px">'
    '<button id="btn" style="border:0;padding:0;position:relative;width:80px;height:40px" '
    'oncontextmenu="event.preventDefault();window.__ctx=[event.button,event.shiftKey,Math.round(event.offsetX),Math.round(event.offsetY)]" '
    'onclick="window.__clicks=(window.__clicks||0)+1" '
    'ondblclick="window.__dbl=(window.__dbl||0)+1" '
    'onmouseenter="window.__hover=true" '
    'onmousedown="window.__down=(window.__down||0)+1" '
    'onmouseup="window.__up=(window.__up||0)+1" '
    'ontouchstart="window.__tap=(window.__tap||0)+1">Btn</button>'
    '<div id="covered" role="button" aria-disabled="true" onclick="window.__forced=true" style="position:relative;width:40px;height:20px">C</div>'
    '<div id="dragsrc" style="position:absolute;left:120px;top:20px;width:20px;height:20px" onmousedown="window.__dragStart=true"></div>'
    '<div id="dragdst" style="position:absolute;left:220px;top:20px;width:20px;height:20px" onmouseup="window.__dragEnd=true"></div>'
    '<input id="inp" value="old" onfocus="window.__foc=true" onblur="window.__foc=false" onkeydown="window.__keys=(window.__keys||[]);window.__keys.push(event.key)">'
    '<input id="chk" type="checkbox"><input id="rad" type="radio" name="r">'
    '<div id="sw" role="switch" tabindex="0" aria-checked="false" onclick="this.setAttribute(\'aria-checked\', String(this.getAttribute(\'aria-checked\')!==\'true\'))">Sw</div>'
    '<select id="sel" multiple><option value="a">Alpha</option><option value="b">Beta</option><option value="c">Gamma</option></select>'
    '<input id="file" type="file" multiple>'
    '<div id="below" style="margin-top:1400px">End</div>'
    '<script>window.addEventListener("wheel", e => { window.__wheel = e.deltaY; if (window.__resolveWheel) window.__resolveWheel(e.deltaY); }, { passive: false });</script></body>'
)


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
            has_touch=True,
            extra_http_headers={"x-flux": "1"},
        )
        await emulated.goto(base)
        assert (await emulated.title()).startswith("FluxSmoke/1|fr-FR")
        settings = await emulated.evaluate(
            "() => [navigator.language, Intl.DateTimeFormat().resolvedOptions().timeZone, innerWidth,"
            " devicePixelRatio, matchMedia('(prefers-color-scheme: dark)').matches,"
            " navigator.maxTouchPoints > 0].join('|')"
        )
        assert settings == "fr-FR|Europe/Paris|600|2|true|true"
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
        assert png[:4] == b"\x89PNG"
        assert (int.from_bytes(png[16:20], "big"), int.from_bytes(png[20:24], "big")) == (40, 20)
        assert (tmp / "el.png").read_bytes() == png
        # A call's timeout, then the page's default, replace the engine's 10 s.
        began = time.monotonic()
        with pytest.raises(TimeoutError):
            await ui.wait_for_selector("#never", timeout=300)
        with pytest.raises(Error):
            await ui.locator("#never").click(timeout=300)
        assert time.monotonic() - began < 5
        ui.set_default_timeout(300)
        began = time.monotonic()
        with pytest.raises(TimeoutError):
            await ui.locator("#never").wait_for()
        assert time.monotonic() - began < 5
        ui.set_default_timeout(10_000)
        # The other states: hidden and detached wait for it to go, attached takes a hidden one.
        await ui.wait_for_selector("#never", state="hidden")
        await ui.locator("#never").wait_for(state="detached")
        with pytest.raises(TimeoutError):
            await ui.wait_for_selector("#e", state="hidden", timeout=300)
        await ui.evaluate("() => setTimeout(() => (e.style.display = 'none'), 100)")
        await ui.wait_for_selector("#e", state="hidden")
        await ui.wait_for_selector("#e", state="attached")
        with pytest.raises(ValueError):
            await ui.wait_for_selector("#e", state="gone")
        await ui.evaluate("() => (e.style.display = '')")
        await ui.get_by_test_id("total").click(timeout=5_000)
        jpg = await ui.get_by_test_id("total").screenshot(path=tmp / "el.jpg", quality=60)
        assert jpg[:3] == b"\xff\xd8\xff"
        assert (tmp / "el.jpg").read_bytes() == jpg
        clip = await ui.screenshot(clip={"x": 10, "y": 50, "width": 40, "height": 20})
        assert clip[:4] == b"\x89PNG"
        assert (int.from_bytes(clip[16:20], "big"), int.from_bytes(clip[20:24], "big")) == (40, 20)
        clear = await ui.screenshot(omit_background=True)
        assert clear[:4] == b"\x89PNG"
        pdf_bytes = await ui.pdf(
            path=tmp / "report.pdf",
            format="A4",
            landscape=True,
            print_background=True,
            scale=0.9,
            page_ranges="1",
            margin={"top": 20, "bottom": "1cm"},
            display_header_footer=True,
            header_template="<span class=title></span>",
            footer_template="<span class=pageNumber></span>",
            outline=True,
            tagged=True,
        )
        assert pdf_bytes[:5] == b"%PDF-"
        assert (tmp / "report.pdf").read_bytes() == pdf_bytes
        with pytest.raises(ValueError, match="not png or jpeg"):
            await ui.screenshot(type="webp")
        with pytest.raises(ValueError, match="only .png, .jpg and .jpeg"):
            await ui.screenshot(path=tmp / "out.webp")
        with pytest.raises(ValueError, match="JPEG only"):
            await ui.screenshot(type="png", quality=50)
        with pytest.raises(TypeError):
            await ui.screenshot(animations="disabled")  # type: ignore[call-arg]
        with pytest.raises(Error, match="unknown format"):
            await ui.pdf(format="A9")
        with pytest.raises(ValueError):
            await ui.pdf(margin={"bogus": 1})  # type: ignore[typeddict-unknown-key]
        for _ in range(50):
            if await ui.page_errors():
                break
            await asyncio.sleep(0.02)
        assert any(m.type == "log" and m.text == "ready 1" for m in await ui.console_messages())
        [late] = await ui.page_errors()
        assert isinstance(late, Error) and late.name == "TypeError" and late.message == "late", repr(late)
        with pytest.raises(Error, match="invalid URL: not a url"):
            await ui.goto("not a url")
        with pytest.raises(TimeoutError):  # after the fixed 10 s action timeout
            await ui.wait_for_selector("#missing")
        await ui.close()

        # Locator reading, filtering, combinators, XPath, extended get_by_role, and strict mode (async).
        lr = await browser.new_page()
        await lr.goto(
            "data:text/html,"
            + quote(
                """<!doctype html>
                <h2 aria-level="2">Sub Header</h2>
                <ul id="list">
                  <li><b>Apple</b> <span>red</span></li>
                  <li><em>Pear</em> <span>green</span></li>
                  <li style="display:none">HiddenItem</li>
                </ul>
                <input id="ro" readonly value="locked" data-kind="ro">
                <input id="ed" value="ok">
                <input id="dis" disabled>
                <input id="chk" type="checkbox" checked>
                <div id="ce" contenteditable="true">Hi <b>there</b></div>
                <button id="tog" aria-pressed="true" aria-expanded="false">Toggle</button>
                <button id="hid" style="display:none">Secret</button>
                <div role="tab" aria-selected="true">TabA</div>
                <div id="parent"><span id="child">Kid</span></div>
                <p class="m">First</p><p class="m">Second</p>"""
            )
        )
        items = lr.locator("#list li")
        assert await items.count() == 3
        all_items = await items.all()
        assert len(all_items) == 3
        assert await all_items[1].inner_text() == "Pear green"
        assert await items.all_text_contents() == ["Apple red", "Pear green", "HiddenItem"]
        assert len(await items.all_inner_texts()) == 3
        assert await items.first.inner_html() == "<b>Apple</b> <span>red</span>"
        assert await lr.locator("#ed").input_value() == "ok"
        assert await lr.locator("#ro").get_attribute("data-kind") == "ro"
        assert await lr.locator("#ro").get_attribute("missing") is None
        assert await lr.locator("#missing").is_visible() is False
        assert await lr.locator("#missing").is_hidden() is True
        assert await lr.locator("#ed").is_enabled() is True
        assert await lr.locator("#dis").is_disabled() is True
        assert await lr.locator("#chk").is_checked() is True
        assert await lr.locator("#ed").is_editable() is True
        assert await lr.locator("#ro").is_editable() is False
        assert await lr.locator("#ce").is_editable() is True
        assert await lr.inner_text(".m") == "First"
        with pytest.raises(Error, match="strict mode violation"):
            await lr.locator(".m").inner_text()
        with pytest.raises(Error, match="strict mode violation"):
            await lr.inner_text(".m", strict=True)
        with pytest.raises(Error, match="strict mode violation"):
            await lr.locator(".m").is_visible()
        assert await items.filter(has=lr.locator("b")).inner_text() == "Apple red"
        assert (
            await lr.locator("#list li", has_not=lr.locator("b"), has_not_text=re.compile(r"Hidden")).inner_text()
            == "Pear green"
        )
        assert await items.filter(has_text=re.compile(r"^pear", re.I)).inner_text() == "Pear green"
        assert await lr.locator("input").and_(lr.locator("#ed")).input_value() == "ok"
        assert await lr.locator("#ed").or_(lr.locator("#chk")).count() == 2
        assert await lr.locator("//button[@id='tog']").inner_text() == "Toggle"
        assert await lr.locator("#child").locator("..").get_attribute("id") == "parent"
        assert await lr.get_by_role("heading", level=2, name=re.compile(r"^Sub")).inner_text() == "Sub Header"
        assert await lr.get_by_role("button", pressed=True, expanded=False).get_attribute("id") == "tog"
        assert await lr.get_by_role("button", name="Secret", include_hidden=True).get_attribute("id") == "hid"
        assert await lr.get_by_role("checkbox", checked=True).get_attribute("id") == "chk"
        assert await lr.get_by_role("tab", selected=True).inner_text() == "TabA"
        assert await lr.frame_locator("#nope").locator("div").count() == 0
        await lr.close()

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
        # expect_download returns the download its block starts, not one that finished before.
        # A navigation before each: Chrome may hold back a page's second download without a
        # user gesture.
        await rv.goto(base)
        await rv.evaluate(CLICK_REPORT)
        await asyncio.sleep(1)  # report.csv finishes, unclaimed
        await rv.goto(base)
        async with rv.expect_download() as info:
            await rv.evaluate(CLICK_REPORT.replace("/report", "/report2"))
        assert (await info.value).suggested_filename == "report2.csv"
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

        # Rich pointer, keyboard, form, upload, filechooser, and touch APIs.
        rich = await browser.new_page(has_touch=True)
        await rich.route("**/rich", lambda route: route.fulfill(content_type="text/html", body=RICH_HTML))
        await rich.goto(f"{base}/rich")
        await rich.click("#btn", button="right", trial=True)
        assert await rich.evaluate("() => window.__ctx") is None
        await rich.click("#btn", button="right", modifiers=["Shift"], position={"x": 10, "y": 12})
        assert await rich.evaluate("() => window.__ctx") == [2, True, 10, 12]
        await rich.dblclick("#btn")
        assert await rich.evaluate("() => window.__dbl") == 1
        await rich.hover("#btn")
        assert await rich.evaluate("() => window.__hover") is True
        await rich.locator("#covered").click(force=True)
        assert await rich.evaluate("() => window.__forced") is True
        await rich.drag_and_drop("#dragsrc", "#dragdst")
        assert await rich.evaluate("() => [window.__dragStart, window.__dragEnd]") == [True, True]
        await rich.evaluate("() => { window.__dragStart = false; window.__dragEnd = false; }")
        await rich.locator("#dragsrc").drag_to(rich.locator("#dragdst"))
        assert await rich.evaluate("() => [window.__dragStart, window.__dragEnd]") == [True, True]
        await rich.locator("#below").scroll_into_view_if_needed()
        assert await rich.evaluate("() => window.scrollY") > 500
        await rich.evaluate("() => window.scrollTo(0, 0)")
        await rich.mouse.move(15, 15, steps=2)
        await rich.mouse.down()
        await rich.mouse.up()
        await rich.mouse.click(15, 15, delay=5)
        await rich.mouse.dblclick(15, 15)
        assert await rich.evaluate("() => window.__dbl") == 2
        assert await rich.evaluate("() => [window.__down >= 3, window.__up >= 3]") == [True, True]
        await rich.mouse.wheel(0, 120)
        assert (
            await rich.evaluate(
                "() => window.__wheel ?? new Promise((res, rej) => { window.__resolveWheel = res; setTimeout(() => rej(new Error('wheel timeout')), 3000); })"
            )
            == 120
        )

        await rich.focus("#inp")
        assert await rich.evaluate("() => window.__foc") is True
        await rich.locator("#inp").clear()
        assert await rich.evaluate("() => inp.value") == ""
        await rich.locator("#inp").press_sequentially("hello")
        await rich.press("#inp", "ControlOrMeta+a")
        await rich.press("#inp", "Backspace")
        await rich.locator("#inp").type("hi")
        await rich.keyboard.down("Shift")
        await rich.keyboard.press("KeyA")
        await rich.keyboard.up("Shift")
        await rich.keyboard.type("z")
        await rich.keyboard.insert_text("世")
        await rich.press("#inp", "Enter")
        assert await rich.evaluate("() => inp.value") == "hiAz世"
        assert "Enter" in await rich.evaluate("() => window.__keys")
        await rich.locator("#inp").blur()
        assert await rich.evaluate("() => window.__foc") is False

        await rich.check("#chk")
        assert await rich.evaluate("() => chk.checked") is True
        await rich.uncheck("#chk")
        assert await rich.evaluate("() => chk.checked") is False
        await rich.locator("#sw").set_checked(True)
        assert await rich.evaluate("() => sw.getAttribute('aria-checked')") == "true"
        with pytest.raises(Error, match="cannot uncheck a radio"):
            await rich.locator("#rad").uncheck()
        assert await rich.select_option("#sel", label="Beta") == ["b"]
        assert await rich.locator("#sel").select_option([{"label": "Beta"}, {"index": 2}]) == ["b", "c"]
        with pytest.raises(ValueError, match="element"):
            await rich.select_option("#sel", element="unsupported")

        upload_file = tmp / "py-async-upload.txt"
        upload_file.write_text("async-upload")
        await rich.set_input_files("#file", upload_file)
        assert await rich.evaluate("() => file.files[0]?.name") == "py-async-upload.txt"
        await rich.set_input_files("#file", [])
        assert await rich.evaluate("() => file.files.length") == 0
        await rich.set_input_files("#file", {"name": "mem.txt", "mimeType": "text/plain", "buffer": b"mem"})
        assert await rich.evaluate("() => file.files[0]?.name") == "mem.txt"

        async with rich.expect_file_chooser() as fc_info:
            await rich.click("#file")
        fc = await fc_info.value
        assert fc.is_multiple() is True
        await fc.set_files({"name": "chosen.txt", "mimeType": "text/plain", "buffer": b"ok"})
        assert await rich.evaluate("() => file.files[0]?.name") == "chosen.txt"

        await rich.tap("#btn")
        await rich.touchscreen.tap(15, 15)
        assert await rich.evaluate("() => window.__tap") == 2
        await rich.close()

        # A page with listeners and routes that nobody holds is collected, so its slot comes back.
        forgotten = await browser.new_page()
        forgotten.on("console", lambda m: None)
        await forgotten.route("**/api", lambda route: route.continue_())
        gone = weakref.ref(forgotten)
        del forgotten
        gc.collect()
        assert gone() is None
        # Also when its handlers refer to the page: the loops reach it only weakly.
        def uses(page):
            return lambda *_: page.url

        forgotten = await browser.new_page()
        forgotten.on("console", uses(forgotten))
        forgotten.on("request", uses(forgotten))
        forgotten.on("dialog", uses(forgotten))
        forgotten.on("filechooser", uses(forgotten))
        await forgotten.route("**/api", uses(forgotten))
        gone = weakref.ref(forgotten)
        del forgotten
        gc.collect()
        assert gone() is None

        # Left open on purpose: the context manager's exit closes the browser under it.
        await browser.new_page()


async def first_page(browser):
    """A page from an engine with a short queue timeout, which a Chrome start or a slow runner
    making one context can outlast. A late page is released when it arrives."""
    for _ in range(30):
        try:
            return await browser.new_page()
        except TimeoutError:
            pass
    raise AssertionError("Chrome did not start")


def test_queue():
    asyncio.run(queue_smoke())


async def queue_smoke() -> None:
    async with async_playwright() as p:
        with pytest.raises(Error, match="max_browsers"):  # would wait forever for a page
            await p.chromium.launch(max_browsers=0)

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

        # With queue_timeout, a job that cannot get a page in time raises TimeoutError. The limit
        # also covers starting a browser and making the context, so fill the slots with retries.
        hurried = await p.chromium.launch(max_browsers=1, queue_timeout=2000)
        held = [await first_page(hurried) for _ in range(8)]
        with pytest.raises(TimeoutError, match="acquire a page lease"):
            await hurried.new_page()
        for page in held:
            await page.close()


def test_events(base, caplog):
    asyncio.run(event_smoke(base))
    assert "answered after its handler returned" in caplog.text


async def event_smoke(base: str) -> None:
    def path(r):
        return urlparse(r.url).path

    async with async_playwright() as p:
        browser = await p.chromium.launch(max_browsers=1)
        page = await browser.new_page()
        seen = []
        page.on("request", lambda r: seen.append(f"request {r.method} {path(r)}"))
        page.on("response", lambda r: seen.append(f"response {r.status} {path(r)}"))
        page.on("requestfinished", lambda r: seen.append(f"finished {path(r)}"))
        page.on("requestfailed", lambda r: seen.append(f"failed {path(r)} {r.failure}"))
        requests = []
        page.on("request", requests.append)
        await page.goto(base)
        async with page.expect_response("**/api") as info:
            await page.evaluate(FETCH_API)
        api = await info.value
        assert api.status == 200 and api.ok
        assert await api.json() == {"real": True}
        assert api.request.method == "GET"
        # One Request per request: the response hands over the one the request event did.
        await asyncio.sleep(0.1)
        assert any(r is api.request for r in requests), requests
        assert api.headers["content-type"] == "application/json"
        await page.route("**/gone", lambda route: route.abort())
        await page.evaluate("() => fetch('/gone').catch(() => 'failed')")
        for _ in range(50):
            if any(e.startswith("failed") for e in seen):
                break
            await asyncio.sleep(0.02)
        assert "request GET /" in seen, seen
        assert "response 200 /api" in seen, seen
        assert "finished /api" in seen, seen
        assert "failed /gone net::ERR_FAILED" in seen, seen
        # An answer after the handler returned is too late: it warns and resolves, the request
        # having gone on without it.
        late = []

        def answer_late(route):
            late.append(asyncio.get_running_loop().create_task(asyncio.sleep(0.05)))
            late[-1].add_done_callback(lambda _: late.append(route.fulfill(body="late")))

        await page.route("**/late", answer_late)
        assert await page.evaluate("() => fetch('/late').then((r) => r.text())") != "late"
        for _ in range(50):
            if len(late) == 2:
                break
            await asyncio.sleep(0.02)
        await late[1]
        await page.unroute("**/late")
        async with page.expect_request(lambda r: r.url.endswith("/hdr")) as request:
            await page.evaluate("() => fetch('/hdr')")
        assert (await request.value).method == "GET"
        with pytest.raises(TimeoutError, match="timeout 200ms exceeded"):
            async with page.expect_response("**/never", timeout=200):
                pass

        # A once and an on of one handler: the once fires once, the on stays.
        texts = []

        def note(m):
            texts.append(m.text)

        page.on("console", note)
        page.once("console", note)
        await page.evaluate("() => ['a', 'b', 'c'].forEach((t) => console.log(t))")
        for _ in range(50):
            if len(texts) >= 4:
                break
            await asyncio.sleep(0.02)
        assert texts == ["a", "a", "b", "c"]
        page.remove_listener("console", note)

        # The network stream ends once nobody listens, and starts again for the next listener.
        net = await browser.new_page()
        await net.goto(base)
        for _ in range(2):
            async with net.expect_response("**/api") as info:
                await net.evaluate(FETCH_API)
            assert (await info.value).ok
            await net.evaluate(FETCH_API)  # events nobody listens to
            for _ in range(50):
                if net._network_task.done():
                    break
                await asyncio.sleep(0.02)
            assert net._network_task.done()
        await net.close()

        # Without a listener a dialog is dismissed; a listener answers it; one it leaves is dismissed.
        assert await page.evaluate("() => confirm('sure?')") is False

        async def on_dialog(dialog):
            if dialog.type == "prompt":
                await dialog.accept(dialog.message + "!")

        page.on("dialog", on_dialog)
        assert await page.evaluate("() => prompt('name')") == "name!"
        assert await page.evaluate("() => confirm('sure?')") is False
        # A beforeunload one it leaves is accepted, as with no listener: leaving the page works.
        await page.goto(ARM_BEFOREUNLOAD)
        await page.click("button")
        await page.goto("data:text/html,<title>left</title>")
        assert await page.title() == "left"

        # A cancelled call drops its Rust future, and with it the downloads lock it held: a
        # second wait times out on its own clock instead of queueing behind the first.
        with pytest.raises(asyncio.TimeoutError):
            await asyncio.wait_for(page.wait_for_download(timeout=10_000), 0.2)
        start = time.monotonic()
        with pytest.raises(TimeoutError):
            await page.wait_for_download(timeout=300)
        assert time.monotonic() - start < 3

        # Navigation and wait helpers (url, reload, history, set_content, wait_for_load_state, wait_for_url, wait_for_function).
        nav = await browser.new_page(base_url=base)
        assert nav.url == "about:blank"
        await nav.go_back()
        await nav.go_forward()
        await nav.goto("/nav-a")
        assert nav.url == f"{base}/nav-a"
        first_n = int((await nav.title()).split("-")[1])
        await nav.reload()
        assert await nav.title() == f"PyNavA-{first_n + 1}"
        await nav.goto("/nav-redir")
        assert nav.url == f"{base}/nav-b"
        await nav.go_back()
        assert nav.url == f"{base}/nav-a"
        await nav.go_forward()
        assert nav.url == f"{base}/nav-b"
        await nav.evaluate("() => history.pushState({}, '', '/nav-b#s')")
        assert nav.url == f"{base}/nav-b#s"
        await nav.go_back()
        assert nav.url == f"{base}/nav-b"

        await nav.set_content('<h1 id="sc">Set</h1><script>window.__sc = 15</script>')
        assert await nav.inner_text("#sc") == "Set"
        assert await nav.evaluate("() => window.__sc") == 15

        await nav.goto("/nav-a")
        await nav.wait_for_load_state("load")
        await nav.evaluate("() => { window.__slowDone = false; fetch('/nav-slow').then(() => { window.__slowDone = true }) }")
        await nav.wait_for_load_state("networkidle")
        assert await nav.evaluate("() => window.__slowDone") is True

        await nav.goto("/nav-delayed")
        await nav.wait_for_url("**/nav-done")
        assert nav.url == f"{base}/nav-done"
        await nav.goto("/nav-delayed")
        await nav.wait_for_url(re.compile(r"/nav-done$"))
        await nav.goto("/nav-delayed")
        await nav.wait_for_url(lambda u: u.endswith("/nav-done"), wait_until="commit")
        with pytest.raises(TimeoutError):
            await nav.wait_for_url("/never", timeout=300)

        await nav.evaluate("() => setTimeout(() => { window.__fn = { n: 5 } }, 60)")
        assert await nav.wait_for_function("(w) => window.__fn && window.__fn.n + w", 3, polling=20) == 8
        assert await nav.wait_for_function("window.__fn && window.__fn.n") == 5
        with pytest.raises(Error, match="fn-boom"):
            await nav.wait_for_function("() => { throw new Error('fn-boom') }")
        with pytest.raises(TimeoutError):
            await nav.wait_for_function("() => false", timeout=250)
        with pytest.raises(ValueError):
            await nav.wait_for_load_state("bogus")  # type: ignore[arg-type]
        await nav.close()

    with sync_playwright() as p:
        browser = p.chromium.launch(max_browsers=1)
        page = browser.new_page()
        page.goto(base)
        with page.expect_response(lambda r: r.url.endswith("/api")) as info:
            page.evaluate(FETCH_API)
        assert info.value.json() == {"real": True}
        page.on("dialog", lambda dialog: dialog.accept("sync!"))
        assert page.evaluate("() => prompt('q')") == "sync!"

        # remove_listener takes back the once added again, not the one that already fired.
        heard = []
        page.once("console", heard.append)
        page.evaluate("() => console.log('one')")
        for _ in range(50):
            if heard:
                break
            time.sleep(0.02)
        page.once("console", heard.append)
        page.remove_listener("console", heard.append)
        page.evaluate("() => console.log('two')")
        time.sleep(0.3)
        assert [m.text for m in heard] == ["one"]


def test_close_does_not_wait(base):
    asyncio.run(close_smoke(base))


async def close_smoke(base: str) -> None:
    # close() does not wait for calls still running on the page (this one would run until the
    # action timeout): they fail at once, and so do pending waits.
    async with async_playwright() as p:
        browser = await p.chromium.launch(max_browsers=1)
        closing = await browser.new_page()
        await closing.goto(base)

        async def wait_response():
            async with closing.expect_response("**/never", timeout=0):
                pass

        open_call = asyncio.ensure_future(closing.wait_for_selector("#never"))
        open_wait = asyncio.ensure_future(wait_response())
        await asyncio.sleep(0.2)
        assert not open_call.done() and not open_wait.done()
        started = time.monotonic()
        await closing.close()
        assert time.monotonic() - started < 5, "close() waited for a running call"
        with pytest.raises(Error, match="page closed"):
            await open_call
        with pytest.raises(Error, match="page closed"):
            await open_wait


def test_contexts(base):
    asyncio.run(context_smoke(base))


async def context_smoke(base: str) -> None:
    async with async_playwright() as p:
        browser = await p.chromium.launch(max_browsers=1)
        # new_context(): its options, cookies and routes apply to its one page.
        context = await browser.new_context(
            user_agent="CtxSmoke/1",
            base_url=base,
            bypass_csp=True,
            http_credentials={"username": "u", "password": "p"},
        )
        await context.add_cookies([{"name": "pre", "value": "yes", "url": base}])
        await context.route("**/api", lambda route: route.fulfill(body="from the context"))
        assert context.pages == []
        page = await context.new_page()
        await page.goto("/login")
        assert (await page.title()).startswith("CtxSmoke/1")
        assert await page.evaluate(FETCH_API) == "from the context"
        assert context.pages == [page]
        cookies = await context.cookies()
        assert any(c["name"] == "sid" for c in cookies)
        assert any(c["name"] == "pre" for c in cookies)
        await context.add_cookies([{"name": "mid", "value": "2", "url": base}])
        assert any(c["name"] == "mid" for c in await context.cookies())
        await context.clear_cookies()
        assert await context.cookies() == []
        await page.goto("/csp")
        assert await page.locator("#n").text_content() == "1"
        await page.goto("/auth")
        assert await page.locator("body").text_content() == "authed"
        await page.goto(
            "data:text/html,<style>@media print { p { color: rgb(255, 0, 0); } }</style>"
            '<img alt="Site Logo" title="Hover Info"><p>T</p>'
        )
        assert await page.get_by_alt_text("Site Logo", exact=True).evaluate("(el) => el.tagName") == "IMG"
        assert await page.get_by_title("hover").evaluate("(el) => el.getAttribute('alt')") == "Site Logo"
        await page.emulate_media(media="print", color_scheme="dark")
        assert (
            await page.evaluate("() => getComputedStyle(document.querySelector('p')).color")
            == "rgb(255, 0, 0)"
        )
        assert await page.evaluate("() => matchMedia('(prefers-color-scheme: dark)').matches") is True
        await page.emulate_media(media=None, color_scheme=None)
        with pytest.raises(Error):
            await p.chromium.connect_over_cdp("ws://127.0.0.1:1/invalid")
        page2 = await context.new_page()
        assert len(context.pages) == 2
        await context.add_cookies([{"name": "shared", "value": "v2", "url": base}])
        await page2.goto("/")
        assert await page2.evaluate("() => document.cookie") == "shared=v2"
        await page.bring_to_front()
        await context.close()
        assert page.is_closed()
        assert page2.is_closed()
        assert context.pages == []
        with pytest.raises(Error, match="context is closed"):
            await context.new_page()

    with sync_playwright() as p:
        browser = p.chromium.launch(max_browsers=1)
        with browser.new_context(locale="de-DE", base_url=base) as context:
            context.add_cookies([{"name": "sync", "value": "1", "url": base}])
            context.route("**/api", lambda route: route.fulfill(body="sync context"))
            page = context.new_page()
            page.goto("/")
            assert "|de-DE" in page.title()
            assert page.evaluate(FETCH_API) == "sync context"
            assert any(c["name"] == "sync" for c in context.cookies())
            context.clear_cookies()
            assert context.cookies() == []
            page.emulate_media(media="print")
            page.emulate_media(media=None)
        assert page.is_closed()


def test_init_scripts_and_bindings(base, tmp_path):
    init_js = tmp_path / "py-init.js"
    init_js.write_text("window.__initPath = true;", encoding="utf-8")
    tag_js = tmp_path / "py-tag.js"
    tag_js.write_text('window.__fileTag = "from-file";', encoding="utf-8")
    tag_css = tmp_path / "py-tag.css"
    tag_css.write_text("#tag-target { margin-left: 17px; }", encoding="utf-8")

    async def run():
        async with async_playwright() as p:
            browser = await p.chromium.launch(max_browsers=1)
            try:
                ctx = await browser.new_context(base_url=base)
                await ctx.expose_function("ctxFn", lambda n: n * 3)
                await ctx.add_init_script('window.__initStr = "ok";')

                page = await ctx.new_page()
                await page.add_init_script(path=init_js)
                await page.goto("/init-page")

                assert await page.evaluate("() => window.__seenAtParse") == {"fromStr": "ok", "hasBound": True}
                assert await page.evaluate("() => window.__initPath") is True
                assert await page.evaluate("() => window.ctxFn(14)") == 42

                async def with_title(suffix):
                    return f"{await page.title()}:{suffix}"

                await page.expose_function("withTitle", with_title)
                assert await page.evaluate("() => window.withTitle('a')") == "PyInitPage:a"

                await page.expose_function("noReturn", lambda: None)
                await page.evaluate("() => window.noReturn()")

                def boom():
                    raise RuntimeError("py-cb-boom")

                await page.expose_function("boomFn", boom)
                with pytest.raises(Error, match="py-cb-boom"):
                    await page.evaluate("() => window.boomFn()")
                with pytest.raises(Error, match="already registered"):
                    await page.expose_function("ctxFn", lambda n: n)
                with pytest.raises(Error, match="already registered"):
                    await ctx.expose_function("ctxFn", lambda n: n)

                await page.reload()
                assert await page.evaluate("() => [window.__initStr, window.__initPath]") == ["ok", True]
                assert await page.evaluate("() => window.withTitle('r')") == "PyInitPage:r"

                await page.add_script_tag(content="window.__inlineTag = 7;")
                await page.add_script_tag(url=f"{base}/tag.js")
                await page.add_script_tag(path=tag_js)
                await page.add_script_tag(content="window.__modLoaded = true;", type="module")
                await page.wait_for_function("() => window.__modLoaded === true")
                assert await page.evaluate(
                    "() => [window.__inlineTag, window.__urlScript, window.__fileTag, "
                    "document.querySelector('script[type=\"module\"]') !== null]"
                ) == [7, 1, "from-file", True]
                with pytest.raises(Error):
                    await page.add_script_tag(url=f"{base}/missing-tag.js")
                with pytest.raises(ValueError):
                    await page.add_script_tag()

                await page.add_style_tag(content="body { background-color: rgb(1, 2, 3); }")
                await page.add_style_tag(url=f"{base}/tag.css")
                await page.add_style_tag(path=tag_css)
                assert await page.evaluate(
                    "() => [getComputedStyle(document.body).backgroundColor, "
                    "getComputedStyle(document.querySelector('#tag-target')).color, "
                    "getComputedStyle(document.querySelector('#tag-target')).marginLeft]"
                ) == ["rgb(1, 2, 3)", "rgb(255, 0, 0)", "17px"]
                await ctx.close()
            finally:
                await browser.close()

    asyncio.run(run())

    with sync_playwright() as p:
        browser = p.chromium.launch(max_browsers=1)
        try:
            with browser.new_context(base_url=base) as ctx:
                ctx.expose_function("syncMul", lambda a, b: a * b)
                ctx.add_init_script('window.__syncInit = "yes";')
                page = ctx.new_page()
                page.goto("/init-page")
                assert page.evaluate("async () => [window.__syncInit, await window.syncMul(6, 7)]") == ["yes", 42]
                # Re-entrant sync callback: calls page.title() without deadlocking the event loop.
                page.expose_function("syncTitle", lambda s: f"{page.title()}:{s}")
                assert page.evaluate("() => window.syncTitle('s')") == "PyInitPage:s"
                page.add_script_tag(content="window.__syncTag = 3;")
                assert page.evaluate("() => window.__syncTag") == 3
                page.add_style_tag(path=tag_css)
                assert page.evaluate("() => getComputedStyle(document.querySelector('#tag-target')).marginLeft") == "17px"
        finally:
            browser.close()


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
        page.goto("data:text/html,<div id=box style='width:30px;height:15px;background:blue'>hi</div>")
        assert page.screenshot(type="jpeg", quality=60)[:3] == b"\xff\xd8\xff"
        assert page.locator("#box").screenshot(type="jpeg")[:3] == b"\xff\xd8\xff"
        assert page.pdf(format="Letter", margin={"top": "10mm"})[:5] == b"%PDF-"
        page.close()

        rich = browser.new_page(has_touch=True)
        rich.route("**/rich", lambda route: route.fulfill(content_type="text/html", body=RICH_HTML))
        rich.goto(f"{base}/rich")
        rich.click("#btn", button="right", trial=True)
        assert rich.evaluate("() => window.__ctx") is None
        rich.click("#btn", button="right", modifiers=["Shift"], position={"x": 10, "y": 12})
        assert rich.evaluate("() => window.__ctx") == [2, True, 10, 12]
        rich.dblclick("#btn")
        assert rich.evaluate("() => window.__dbl") == 1
        rich.hover("#btn")
        assert rich.evaluate("() => window.__hover") is True
        rich.locator("#covered").click(force=True)
        assert rich.evaluate("() => window.__forced") is True
        rich.drag_and_drop("#dragsrc", "#dragdst")
        assert rich.evaluate("() => [window.__dragStart, window.__dragEnd]") == [True, True]
        rich.locator("#below").scroll_into_view_if_needed()
        assert rich.evaluate("() => window.scrollY") > 500
        rich.evaluate("() => window.scrollTo(0, 0)")
        rich.mouse.move(15, 15, steps=2)
        rich.mouse.wheel(0, 120)
        assert (
            rich.evaluate(
                "() => window.__wheel ?? new Promise((res, rej) => { window.__resolveWheel = res; setTimeout(() => rej(new Error('wheel timeout')), 3000); })"
            )
            == 120
        )

        rich.focus("#inp")
        assert rich.evaluate("() => window.__foc") is True
        rich.locator("#inp").clear()
        rich.locator("#inp").press_sequentially("hello")
        rich.press("#inp", "ControlOrMeta+a")
        rich.press("#inp", "Backspace")
        rich.keyboard.insert_text("世")
        rich.press("#inp", "Enter")
        assert rich.evaluate("() => inp.value") == "世"
        rich.locator("#inp").blur()
        assert rich.evaluate("() => window.__foc") is False

        rich.check("#chk")
        assert rich.evaluate("() => chk.checked") is True
        rich.uncheck("#chk")
        assert rich.evaluate("() => chk.checked") is False
        rich.set_checked("#sw", True)
        assert rich.evaluate("() => sw.getAttribute('aria-checked')") == "true"
        assert rich.select_option("#sel", [{"label": "Beta"}, {"index": 2}]) == ["b", "c"]

        upload_file = tmp_path / "py-sync-upload.txt"
        upload_file.write_text("sync-upload")
        rich.set_input_files("#file", upload_file)
        assert rich.evaluate("() => file.files[0]?.name") == "py-sync-upload.txt"
        rich.set_input_files("#file", [])
        assert rich.evaluate("() => file.files.length") == 0
        with rich.expect_file_chooser(lambda fc: fc.is_multiple()) as fc_info:
            rich.click("#file")
        fc_info.value.set_files({"name": "sync.txt", "mimeType": "text/plain", "buffer": b"ok"})
        assert rich.evaluate("() => file.files[0]?.name") == "sync.txt"

        rich.tap("#btn")
        rich.touchscreen.tap(15, 15)
        assert rich.evaluate("() => window.__tap") == 2
        rich.close()

        # Sync locator reading, filtering, combinators, XPath, extended get_by_role, and strict mode.
        lr_sync = browser.new_page()
        lr_sync.goto(
            "data:text/html,"
            + quote(
                """<!doctype html>
                <ul><li>One</li><li>Two</li></ul>
                <input id="inp" value="v"><input id="dis" disabled>
                <button aria-pressed="true">Tog</button>
                <span class="m">A</span><span class="m">B</span>"""
            )
        )
        assert lr_sync.locator("li").count() == 2
        assert [x.inner_text() for x in lr_sync.locator("li").all()] == ["One", "Two"]
        assert lr_sync.locator("li").filter(has_text=re.compile(r"^t", re.I)).inner_text() == "Two"
        assert lr_sync.locator("#inp").and_(lr_sync.locator("input")).input_value() == "v"
        assert lr_sync.locator("#inp").or_(lr_sync.locator("#dis")).count() == 2
        assert lr_sync.get_by_role("button", pressed=True).inner_text() == "Tog"
        assert lr_sync.inner_text(".m") == "A"
        with pytest.raises(Error, match="strict mode violation"):
            lr_sync.inner_text(".m", strict=True)
        with pytest.raises(Error, match="strict mode violation"):
            lr_sync.locator(".m").inner_text()
        lr_sync.close()

        nav_sync = browser.new_page(base_url=base)
        assert nav_sync.url == "about:blank"
        nav_sync.goto("/nav-a")
        first_sync = int(nav_sync.title().split("-")[1])
        nav_sync.reload()
        assert nav_sync.title() == f"PyNavA-{first_sync + 1}"
        nav_sync.goto("/nav-redir")
        assert nav_sync.url == f"{base}/nav-b"
        nav_sync.go_back()
        assert nav_sync.url == f"{base}/nav-a"
        nav_sync.go_forward()
        assert nav_sync.url == f"{base}/nav-b"
        nav_sync.set_content('<p id="sync-sc">SyncContent</p>')
        assert nav_sync.inner_text("#sync-sc") == "SyncContent"
        nav_sync.goto("/nav-delayed")
        nav_sync.wait_for_url("/nav-done")
        assert nav_sync.url == f"{base}/nav-done"
        nav_sync.evaluate("() => setTimeout(() => { window.__sfn = 21 }, 50)")
        assert nav_sync.wait_for_function("(m) => window.__sfn && window.__sfn * m", 2, polling=20) == 42
        nav_sync.close()

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
        "    page.on('console', print)\n"
        "    await page.goto('data:text/html,<title>x</title>')\n"
        "asyncio.run(main())\n"
        "print('exited')\n",
        "from fluxwright.sync_api import chromium\n"
        "browser = chromium.launch(max_browsers=1)\n"
        "page = browser.new_page()\n"
        "page.on('console', print)\n"
        "page.goto('data:text/html,<title>x</title>')\n"
        "print('exited')\n",
    ],
    ids=["async", "sync"],
)
def test_open_page_at_exit(script, tmp_path):
    """A browser and a page left open, with a listener, neither crash nor hang the interpreter on
    exit. In the sync API the listener's event stream ends during shutdown, when the page is
    freed: its result must not be handed to Python then."""
    # Files rather than pipes: a Chrome left behind would hold a pipe open and hang the test.
    out, err = tmp_path / "out.txt", tmp_path / "err.txt"
    with out.open("w") as stdout, err.open("w") as stderr:
        done = subprocess.run([sys.executable, "-c", script], stdout=stdout, stderr=stderr, timeout=120)
    assert done.returncode == 0, err.read_text()
    assert out.read_text().strip() == "exited"


def test_call_from_a_late_exit_handler_fails_instead_of_hanging(tmp_path):
    """Exit handlers run newest first, so one registered before fluxwright was imported runs after
    fluxwright's own, when no result can be handed over any more. Its calls fail at once."""
    script = (
        "import asyncio, atexit\n"
        "async def late():\n"
        "    from fluxwright.async_api import chromium\n"
        "    try:\n"
        "        await chromium.launch(max_browsers=1)\n"
        "    except Exception as e:\n"
        "        print('failed:', e)\n"
        "atexit.register(lambda: asyncio.run(late()))\n"
        "import fluxwright.async_api\n"
    )
    out, err = tmp_path / "out.txt", tmp_path / "err.txt"
    with out.open("w") as stdout, err.open("w") as stderr:
        done = subprocess.run([sys.executable, "-c", script], stdout=stdout, stderr=stderr, timeout=60)
    assert done.returncode == 0, err.read_text()
    assert "stopped at interpreter exit" in out.read_text()


def test_multi_tab_popups_and_frames(base):
    async def run_async():
        async with async_playwright() as p:
            browser = await p.chromium.launch(max_browsers=1)
            try:
                async with await browser.new_context(base_url=base, locale="fr-FR") as ctx:
                    await ctx.add_cookies([{"name": "pre", "value": "yes", "url": base}])
                    pre_state = await ctx.storage_state()
                    assert any(c["name"] == "pre" for c in pre_state["cookies"])
                    assert any(c["name"] == "pre" for c in await ctx.cookies())
                    await ctx.add_init_script('window.__ctxInit = "shared";')
                    await ctx.expose_function("ctxMul", lambda a, b: a * b)
                    await ctx.route("**/api", lambda r: r.fulfill(body="from-ctx-route"))

                    opener = await ctx.new_page()
                    await opener.goto("/opener-page")
                    seen_popups = []
                    opener.on("popup", seen_popups.append)

                    async with opener.expect_popup() as pop_info:
                        await opener.click("#open-btn")
                    pop1 = await pop_info.value
                    await pop1.wait_for_load_state("load")
                    assert await pop1.opener() is opener
                    assert pop1 in ctx.pages
                    assert await pop1.locator("#pop").inner_text() == "popup-one"
                    assert await pop1.evaluate(
                        "async () => [window.__ctxInit, navigator.language, await ctxMul(3, 5)]"
                    ) == ["shared", "fr-FR", 15]
                    assert await pop1.evaluate("() => fetch('/api').then((r) => r.text())") == "from-ctx-route"
                    await pop1.close()
                    assert pop1 not in ctx.pages

                    async with opener.expect_popup(lambda p2: not p2.is_closed()) as pop2_info:
                        await opener.click("#blank-link")
                    pop2 = await pop2_info.value
                    await pop2.wait_for_load_state("load")
                    assert await pop2.locator("#pop2").inner_text() == "popup-two"
                    assert len(seen_popups) == 2
                    await pop2.close()

                    await opener.goto("/frames-host")
                    for _ in range(100):
                        fs = opener.frames
                        if any(f.name == "same-fr" and f.url.endswith("/same-child") for f in fs) and any(
                            f.name == "cross-fr" and f.url.endswith("/cross-child") for f in fs
                        ):
                            break
                        await asyncio.sleep(0.03)
                    else:
                        raise AssertionError("frames did not load")

                    main = opener.main_frame
                    assert len(opener.frames) == 3
                    assert len(main.child_frames) == 2
                    same = opener.frame("same-fr")
                    cross = opener.frame(url=re.compile(r"cross-child"))
                    assert same is not None and cross is not None
                    assert same.parent_frame is main
                    assert cross.parent_frame is main
                    assert not same.is_detached()
                    assert not cross.is_detached()

                    await same.locator("#sb").click()
                    assert await same.locator("#sb").inner_text() == "clicked-same"
                    assert await same.title() == "SameTitle"
                    await cross.locator("#cb").click()
                    assert await cross.locator("#cb").inner_text() == "clicked-cross"
                    assert await cross.title() == "CrossTitle"
                    assert "clicked-cross" in await cross.content()
                    assert await cross.evaluate("(x) => x + 10", 5) == 15

                    await same.goto("/same-next")
                    assert await same.locator("#sn").inner_text() == "navigated-same"

                    await opener.evaluate("() => document.getElementById('same').remove()")
                    for _ in range(50):
                        if same.is_detached():
                            break
                        await asyncio.sleep(0.02)
                    assert same.is_detached()
            finally:
                await browser.close()

    asyncio.run(run_async())

    with sync_playwright() as p:
        browser = p.chromium.launch(max_browsers=1)
        try:
            with browser.new_context(base_url=base) as ctx:
                ctx.add_cookies([{"name": "sync_pre", "value": "ok", "url": base}])
                assert any(c["name"] == "sync_pre" for c in ctx.storage_state()["cookies"])
                opener = ctx.new_page()
                opener.goto("/opener-page")
                with opener.expect_popup() as info:
                    opener.click("#open-btn")
                pop = info.value
                pop.wait_for_load_state("load")
                assert pop.opener() is opener
                assert pop in ctx.pages
                pop.close()

                opener.goto("/frames-host")
                for _ in range(100):
                    if any(f.name == "cross-fr" and f.url.endswith("/cross-child") for f in opener.frames):
                        break
                    time.sleep(0.03)
                else:
                    raise AssertionError("sync frames did not load")
                cross = opener.frame("cross-fr")
                assert cross is not None
                cross.locator("#cb").click()
                assert cross.locator("#cb").inner_text() == "clicked-cross"
                assert cross.evaluate("(x) => x * 2", 7) == 14
        finally:
            browser.close()
