"""Tests for Playwright-style auto-retrying ``expect()`` assertions across async and sync APIs."""

import asyncio
import re
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest

from fluxwright import async_api, sync_api
from fluxwright._expect import (
    DEFAULT_TIMEOUT_MS,
    LocatorAssertions as RawLocatorAssertions,
    PageAssertions as RawPageAssertions,
    _is_fatal_error,
    _resolve_url,
)
from fluxwright.async_api import (
    LocatorAssertions as AsyncLocatorAssertions,
    PageAssertions as AsyncPageAssertions,
    async_playwright,
    expect as async_expect,
)
from fluxwright.sync_api import (
    LocatorAssertions as SyncLocatorAssertions,
    PageAssertions as SyncPageAssertions,
    expect as sync_expect,
    sync_playwright,
)

FIXTURE_HTML = b"""<!doctype html>
<title>Start Title</title>
<div id="vis" style="display:none">Hidden</div>
<div id="hide">Visible</div>
<button id="btn" disabled>Go</button>
<input id="chk" type="checkbox">
<input id="edt" readonly value="before">
<p id="txt">  Loading   data  </p>
<div id="cls" class="pill idle">Idle</div>
<ul id="items"><li>One</li></ul>
<div class="dup">First</div><div class="dup">Second</div>
<div id="swap-host"><span id="swap">NotAnInput</span></div>
"""

SCHEDULE_MUTATIONS_JS = """() => {
  setTimeout(() => {
    document.title = 'Updated   Title';
    history.pushState({}, '', '/done');
    document.getElementById('vis').style.display = 'block';
    document.getElementById('hide').style.display = 'none';
    const btn = document.getElementById('btn');
    btn.disabled = false;
    btn.setAttribute('data-status', 'ready-42');
    document.getElementById('chk').checked = true;
    const edt = document.getElementById('edt');
    edt.readOnly = false;
    edt.value = 'after';
    document.getElementById('txt').textContent = '  Loaded\\n  42   items  ';
    document.getElementById('cls').className = 'pill active primary';
    const li = document.createElement('li');
    li.textContent = 'Two';
    document.getElementById('items').appendChild(li);
    const late = document.createElement('div');
    late.id = 'late';
    late.style.display = 'none';
    document.body.appendChild(late);
    const dups = document.querySelectorAll('.dup');
    if (dups.length > 1) dups[1].remove();
    const inp = document.createElement('input');
    inp.id = 'swap';
    inp.value = 'swapped';
    document.getElementById('swap-host').replaceChildren(inp);
  }, 120);
}"""


class _Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(200)
        self.send_header("Content-Type", "text/html")
        if self.path == "/done":
            body = b"<title>Done Title</title><p id='done'>Finished</p>"
        else:
            body = FIXTURE_HTML
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass


@pytest.fixture(scope="module")
def base():
    server = ThreadingHTTPServer(("127.0.0.1", 0), _Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    yield f"http://127.0.0.1:{server.server_address[1]}"
    server.shutdown()


def test_exports():
    for name in ("expect", "PageAssertions", "LocatorAssertions"):
        assert name in async_api.__all__
        assert name in sync_api.__all__


def test_async_expect(base):
    asyncio.run(_run_async_expect(base))


async def _run_async_expect(base: str) -> None:
    with pytest.raises(TypeError):
        async_expect(object())  # type: ignore[arg-type]

    async with async_playwright() as p:
        browser = await p.chromium.launch(max_browsers=1)
        try:
            page = await browser.new_page(base_url=base)
            page.set_default_timeout(10_000)
            await page.goto("/start")

            page_assert = async_expect(page)
            loc_assert = async_expect(page.locator("#vis"))
            assert isinstance(page_assert, AsyncPageAssertions)
            assert isinstance(loc_assert, AsyncLocatorAssertions)

            # Immediate states before delayed DOM update.
            await loc_assert.to_be_hidden()
            await async_expect(page.locator("#btn")).to_be_disabled()
            await async_expect(page.locator("#chk")).not_to_be_checked()
            await async_expect(page.locator("#edt")).not_to_be_editable()
            await async_expect(page.locator("#late")).not_to_be_attached()
            await page_assert.not_to_have_url("/done")

            await page.evaluate(SCHEDULE_MUTATIONS_JS)

            # Delayed page title & URL.
            await page_assert.to_have_title("Updated Title")
            await page_assert.to_have_title(re.compile(r"Updated\s+Title"))
            await page_assert.not_to_have_title("Start Title")
            await page_assert.to_have_url("/done")
            await page_assert.to_have_url(f"{base}/done")
            await page_assert.to_have_url(re.compile(r"/done$"))

            # Delayed locator states & attributes.
            await async_expect(page.locator("#vis")).to_be_visible()
            await async_expect(page.locator("#vis")).not_to_be_hidden()
            await async_expect(page.locator("#hide")).to_be_hidden()
            await async_expect(page.locator("#hide")).not_to_be_visible()
            await async_expect(page.locator("#late")).to_be_attached()
            await async_expect(page.locator("#late")).to_be_hidden()

            await async_expect(page.locator("#btn")).to_be_enabled()
            await async_expect(page.locator("#btn")).not_to_be_disabled()
            await async_expect(page.locator("#btn")).to_have_attribute("data-status", "ready-42")
            await async_expect(page.locator("#btn")).to_have_attribute("data-status", re.compile(r"^ready-\d+$"))
            await async_expect(page.locator("#btn")).not_to_have_attribute("data-status", "loading")
            await async_expect(page.locator("#btn")).not_to_have_attribute("data-missing", "any")

            await async_expect(page.locator("#chk")).to_be_checked()
            await async_expect(page.locator("#edt")).to_be_editable()
            await async_expect(page.locator("#edt")).to_have_value("after")
            await async_expect(page.locator("#edt")).to_have_value(re.compile(r"^aft"))
            await async_expect(page.locator("#edt")).not_to_have_value("before")

            await async_expect(page.locator("#txt")).to_have_text("Loaded 42 items")
            await async_expect(page.locator("#txt")).to_have_text(re.compile(r"Loaded \d+ items"))
            await async_expect(page.locator("#txt")).to_contain_text("42 items")
            await async_expect(page.locator("#txt")).to_contain_text(re.compile(r"\d+ items"))
            await async_expect(page.locator("#txt")).not_to_have_text("Loading data")
            await async_expect(page.locator("#txt")).not_to_contain_text("Loading")

            await async_expect(page.locator("#cls")).to_have_class("pill active primary")
            await async_expect(page.locator("#cls")).to_have_class(re.compile(r"\bactive\b"))
            await async_expect(page.locator("#cls")).not_to_have_class("pill idle")

            await async_expect(page.locator("#items li")).to_have_count(2)
            await async_expect(page.locator("#items li")).not_to_have_count(1)
            await async_expect(page.locator("#never")).to_have_count(0)

            # Transient strict-mode violation and wrong-tag element are ignored until resolved.
            await async_expect(page.locator(".dup")).to_have_text("First")
            await async_expect(page.locator("#swap")).to_have_value("swapped")

            # Delayed re-disable exercises positive to_be_disabled + not_to_be_enabled.
            await page.evaluate("() => setTimeout(() => { document.getElementById('btn').disabled = true; }, 100)")
            await async_expect(page.locator("#btn")).to_be_disabled()
            await async_expect(page.locator("#btn")).not_to_be_enabled()

            # Timeout diagnostics and prompt failure.
            started = time.monotonic()
            with pytest.raises(AssertionError) as exc_info:
                await async_expect(page.locator("#txt")).to_have_text("Never", timeout=600)
            assert time.monotonic() - started < 3.0
            msg = str(exc_info.value)
            assert "to_have_text" in msg
            assert "Expected: 'Never'" in msg
            assert "Actual: 'Loaded 42 items'" in msg

            with pytest.raises(AssertionError, match="element not found"):
                await async_expect(page.locator("#missing")).not_to_have_text("anything", timeout=600)

            with pytest.raises(AssertionError, match="to_have_url"):
                await async_expect(page).to_have_url("/never", timeout=150)

            await page.close()
        finally:
            await browser.close()


def test_sync_expect(base):
    with pytest.raises(TypeError):
        sync_expect(object())  # type: ignore[arg-type]

    with sync_playwright() as p:
        browser = p.chromium.launch(max_browsers=1)
        try:
            page = browser.new_page(base_url=base)
            page.set_default_timeout(10_000)
            page.goto("/start")

            page_assert = sync_expect(page)
            loc_assert = sync_expect(page.locator("#vis"))
            assert isinstance(page_assert, SyncPageAssertions)
            assert isinstance(loc_assert, SyncLocatorAssertions)

            loc_assert.to_be_hidden()
            sync_expect(page.locator("#btn")).to_be_disabled()
            sync_expect(page.locator("#chk")).not_to_be_checked()
            sync_expect(page.locator("#edt")).not_to_be_editable()
            sync_expect(page.locator("#late")).not_to_be_attached()
            page_assert.not_to_have_url("/done")

            page.evaluate(SCHEDULE_MUTATIONS_JS)

            page_assert.to_have_title("Updated Title")
            page_assert.to_have_title(re.compile(r"Updated\s+Title"))
            page_assert.not_to_have_title("Start Title")
            page_assert.to_have_url("/done")
            page_assert.to_have_url(re.compile(r"/done$"))

            sync_expect(page.locator("#vis")).to_be_visible()
            sync_expect(page.locator("#vis")).not_to_be_hidden()
            sync_expect(page.locator("#hide")).to_be_hidden()
            sync_expect(page.locator("#hide")).not_to_be_visible()
            sync_expect(page.locator("#late")).to_be_attached()

            sync_expect(page.locator("#btn")).to_be_enabled()
            sync_expect(page.locator("#btn")).not_to_be_disabled()
            sync_expect(page.locator("#btn")).to_have_attribute("data-status", "ready-42")
            sync_expect(page.locator("#btn")).not_to_have_attribute("data-status", "loading")

            sync_expect(page.locator("#chk")).to_be_checked()
            sync_expect(page.locator("#edt")).to_be_editable()
            sync_expect(page.locator("#edt")).to_have_value("after")
            sync_expect(page.locator("#edt")).not_to_have_value("before")

            sync_expect(page.locator("#txt")).to_have_text("Loaded 42 items")
            sync_expect(page.locator("#txt")).to_contain_text("42 items")
            sync_expect(page.locator("#txt")).not_to_have_text("Loading data")
            sync_expect(page.locator("#txt")).not_to_contain_text("Loading")

            sync_expect(page.locator("#cls")).to_have_class("pill active primary")
            sync_expect(page.locator("#cls")).not_to_have_class("pill idle")

            sync_expect(page.locator("#items li")).to_have_count(2)
            sync_expect(page.locator("#items li")).not_to_have_count(1)
            sync_expect(page.locator("#never")).to_have_count(0)

            sync_expect(page.locator(".dup")).to_have_text("First")
            sync_expect(page.locator("#swap")).to_have_value("swapped")

            page.evaluate("() => setTimeout(() => { document.getElementById('btn').disabled = true; }, 100)")
            sync_expect(page.locator("#btn")).to_be_disabled()
            sync_expect(page.locator("#btn")).not_to_be_enabled()

            short_expect = sync_expect.configure(timeout=200)
            started = time.monotonic()
            with pytest.raises(AssertionError) as exc_info:
                short_expect(page.locator("#txt")).to_have_text("Never")
            assert time.monotonic() - started < 3.0
            assert "200ms" in str(exc_info.value)

            with pytest.raises(AssertionError, match="element not found"):
                sync_expect(page.locator("#missing")).not_to_have_text("x", timeout=2000)

            # Bytes regex rejected, strict bool on checked.
            with pytest.raises(TypeError, match="bytes"):
                sync_expect(page.locator("#txt")).to_have_text(re.compile(b"Loaded"))  # type: ignore[arg-type]
            with pytest.raises(TypeError, match="bool"):
                sync_expect(page.locator("#chk")).to_be_checked(checked="false")  # type: ignore[arg-type]

            # expect.set_options(timeout=...) updates default timeout in place.
            try:
                sync_expect.set_options(timeout=150)
                started = time.monotonic()
                with pytest.raises(AssertionError, match="150ms"):
                    sync_expect(page.locator("#txt")).to_have_text("Never")
                assert time.monotonic() - started < 3.0
            finally:
                sync_expect.set_options(timeout=DEFAULT_TIMEOUT_MS)

            # expect.soft accumulates failures without stopping execution.
            sync_expect.clear_soft_errors()
            sync_expect.soft(page.locator("#txt")).to_have_text("Wrong One", timeout=100)
            sync_expect.soft(page).to_have_title("Wrong Title", timeout=100)
            sync_expect.soft(page.locator("#txt")).to_have_text("Loaded 42 items", timeout=1000)
            assert len(sync_expect.soft_errors()) == 2
            with pytest.raises(AssertionError) as soft_exc:
                sync_expect.assert_no_soft_errors()
            assert "Wrong One" in str(soft_exc.value)
            assert "Wrong Title" in str(soft_exc.value)
            assert sync_expect.soft_errors() == []

            page.close()
        finally:
            browser.close()


def test_expect_unit_url_fatal_bounded_probes_and_async_soft():
    # 1. _resolve_url handles protocol-relative //host/path via urljoin.
    class _BasePage:
        def _base_url(self) -> str:
            return "http://127.0.0.1:8080/app/index.html"

    bp = _BasePage()
    assert _resolve_url(bp, "/done") == "http://127.0.0.1:8080/done"
    assert _resolve_url(bp, "//other.example/path") == "http://other.example/path"
    assert _resolve_url(bp, "next") == "http://127.0.0.1:8080/app/next"
    assert _resolve_url(bp, "https://abs.example/x") == "https://abs.example/x"

    # 2. Expanded _is_fatal_error patterns.
    for msg in (
        "devtools websocket: connection reset",
        "engine is shut down",
        "browser context is closed",
        "Target closed",
        "Session with given id not found",
        "page is closed",
    ):
        assert _is_fatal_error(RuntimeError(msg)), f"expected fatal: {msg}"
    assert not _is_fatal_error(RuntimeError("element is not visible"))

    # 3. count() and title() probes are bounded by limit_ms (short timeout never hangs 30s).
    class _SlowLocator:
        async def count(self) -> int:
            await asyncio.sleep(5.0)
            return 0

    class _SlowPage:
        def is_closed(self) -> bool:
            return False

        async def title(self) -> str:
            await asyncio.sleep(5.0)
            return "never"

    async def _run_unit() -> None:
        t0 = time.monotonic()
        with pytest.raises(AssertionError):
            await RawLocatorAssertions(_SlowLocator()).to_have_count(1, timeout=150)  # type: ignore[arg-type]
        assert time.monotonic() - t0 < 1.5

        t1 = time.monotonic()
        with pytest.raises(AssertionError):
            await RawPageAssertions(_SlowPage()).to_have_title("ok", timeout=150)  # type: ignore[arg-type]
        assert time.monotonic() - t1 < 1.5

        # Bytes regex rejected on async assertions too.
        with pytest.raises(TypeError, match="bytes"):
            await RawPageAssertions(_SlowPage()).to_have_title(re.compile(b"ok"))  # type: ignore[arg-type]
        with pytest.raises(TypeError, match="bool"):
            await RawLocatorAssertions(_SlowLocator()).to_be_checked(checked="false")  # type: ignore[arg-type]

        # Async expect.set_options + expect.soft with fake assertion objects.
        try:
            async_expect.set_options(timeout=120)
            assert async_expect._default_timeout == 120.0
        finally:
            async_expect.set_options(timeout=DEFAULT_TIMEOUT_MS)

        async_expect.clear_soft_errors()
        await RawLocatorAssertions(_SlowLocator(), soft=True, message="soft-count").to_have_count(1, timeout=100)  # type: ignore[arg-type]
        await RawPageAssertions(_SlowPage(), soft=True, message="soft-title").to_have_title("x", timeout=100)  # type: ignore[arg-type]
        assert len(async_expect.soft_errors()) == 2
        with pytest.raises(AssertionError) as soft_info:
            async_expect.assert_no_soft_errors()
        assert "soft-count" in str(soft_info.value)
        assert "soft-title" in str(soft_info.value)
        assert async_expect.soft_errors() == []

    asyncio.run(_run_unit())
