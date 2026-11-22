"""End-to-end tests for ``context.tracing`` across async and sync Python APIs."""

from __future__ import annotations

import asyncio
from collections.abc import Iterator
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
from pathlib import Path
import re
import threading
import time
import zipfile

import pytest

from fluxwright import async_api, sync_api
from fluxwright._errors import Error
from fluxwright._tracing import AsyncTracing as RawAsyncTracing
from fluxwright.async_api import Locator as AsyncLocator
from fluxwright.async_api import Tracing as AsyncTracing
from fluxwright.async_api import async_playwright
from fluxwright.async_api import expect as async_expect
from fluxwright.sync_api import Tracing as SyncTracing
from fluxwright.sync_api import sync_playwright

_HTML = b"""<!doctype html>
<html>
  <body style="margin:0;padding:16px;background:#fafafa">
    <h1 id="heading">Trace Demo</h1>
    <input id="name" type="text" />
    <select id="sel">
      <option value="a">Alpha</option>
      <option value="b">Beta</option>
    </select>
    <button id="btn" onclick="document.getElementById('out').textContent='clicked'; console.log('btn-clicked')">
      Go
    </button>
    <button id="err-btn" onclick="setTimeout(() => { throw new Error('trace-boom') }, 0)">Boom</button>
    <div id="out">idle</div>
    <script>console.log('page-loaded')</script>
  </body>
</html>
"""


class _Handler(BaseHTTPRequestHandler):
    def do_GET(self) -> None:
        if self.path == "/api/ping":
            body = b'{"ok":true}'
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        self.send_response(200)
        self.send_header("Content-Type", "text/html; charset=utf-8")
        self.send_header("Content-Length", str(len(_HTML)))
        self.end_headers()
        self.wfile.write(_HTML)

    def log_message(self, format: str, *args: object) -> None:
        return


@pytest.fixture(scope="module")
def base() -> Iterator[str]:
    server = ThreadingHTTPServer(("127.0.0.1", 0), _Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield f"http://127.0.0.1:{server.server_port}"
    finally:
        server.shutdown()
        server.server_close()


def _parse_ndjson(raw: bytes) -> list[dict]:
    text = raw.decode("utf-8").strip()
    if not text:
        return []
    return [json.loads(line) for line in text.splitlines()]


def _assert_valid_trace_zip(
    zip_path: Path,
    *,
    expected_title: str,
    expected_name: str | None = None,
    expected_apis: tuple[str, ...] = (),
    allow_in_flight: bool = False,
) -> tuple[list[dict], list[dict]]:
    raw_bytes = zip_path.read_bytes()
    assert raw_bytes[:4] == b"PK\x03\x04"

    with zipfile.ZipFile(zip_path, "r") as zf:
        assert zf.testzip() is None
        names = set(zf.namelist())
        assert "trace.trace" in names
        assert "trace.network" in names

        trace_events = _parse_ndjson(zf.read("trace.trace"))
        net_events = _parse_ndjson(zf.read("trace.network"))

        ctx_opts = next((e for e in trace_events if e.get("type") == "context-options"), None)
        assert ctx_opts is not None
        assert ctx_opts.get("sdkLanguage") == "python"
        assert ctx_opts.get("title") == expected_title
        if expected_name is not None:
            assert ctx_opts.get("contextId") == expected_name

        befores = [e for e in trace_events if e.get("type") == "before"]
        afters = [e for e in trace_events if e.get("type") == "after"]
        if not allow_in_flight:
            assert len(afters) >= len(befores)
        for api in expected_apis:
            assert any(e.get("apiName") == api for e in befores), f"missing {api} in trace.trace"

        frames = [e for e in trace_events if e.get("type") == "screencast-frame"]
        assert len(frames) >= 1
        for frame in frames:
            sha1_name = frame["sha1"]
            assert re.fullmatch(r"[0-9a-f]{40}\.jpeg", sha1_name)
            res_key = f"resources/{sha1_name}"
            assert res_key in names
            jpeg = zf.read(res_key)
            assert jpeg[:3] == b"\xff\xd8\xff"
            assert f"{hashlib.sha1(jpeg).hexdigest()}.jpeg" == sha1_name

    return trace_events, net_events


def test_exports() -> None:
    assert "Tracing" in async_api.__all__
    assert "Tracing" in sync_api.__all__
    assert async_api.Tracing is AsyncTracing
    assert sync_api.Tracing is SyncTracing


async def _run_async_tracing(base: str, tmp_path: Path) -> None:
    async with async_playwright() as p:
        browser = await p.chromium.launch(headless=True)
        try:
            ctx = await browser.new_context()
            assert isinstance(ctx.tracing, AsyncTracing)

            with pytest.raises(Error, match="not been started"):
                await ctx.tracing.stop(path=tmp_path / "never.zip")
            with pytest.raises(Error, match="Must start tracing"):
                await ctx.tracing.start_chunk(title="bad")
            with pytest.raises(Error, match="Must start tracing"):
                await ctx.tracing.stop_chunk(path=tmp_path / "bad.zip")
            with pytest.raises(TypeError):
                await ctx.tracing.start(screenshots="yes")  # type: ignore[arg-type]
            await ctx.tracing.stop()

            pre_page = await ctx.new_page()
            await pre_page.goto(base)

            trace_zip = tmp_path / "nested" / "async-trace.zip"
            await ctx.tracing.start(
                name="py-async",
                title="Async Trace",
                screenshots=True,
                snapshots=True,
            )
            with pytest.raises(Error, match="already started"):
                await ctx.tracing.start()
            with pytest.raises(TypeError):
                await ctx.tracing.stop(path="")

            await pre_page.fill("#name", "fluxwright")
            await pre_page.click("#btn")
            await pre_page.evaluate("() => fetch('/api/ping').then((r) => r.json())")
            await pre_page.click("#err-btn")
            await asyncio.sleep(0.08)

            sub_page = await ctx.new_page()
            await sub_page.goto(f"{base}/second")
            await sub_page.close()

            await ctx.tracing.stop(path=trace_zip)

            trace_events, net_events = _assert_valid_trace_zip(
                trace_zip,
                expected_title="Async Trace",
                expected_name="py-async",
                expected_apis=("page.fill", "page.click", "page.goto"),
            )
            assert any(e.get("type") == "console" and "btn-clicked" in e.get("text", "") for e in trace_events)
            assert any(
                e.get("type") == "event"
                and e.get("method") == "pageError"
                and "trace-boom" in e["params"]["error"]["error"]["message"]
                for e in trace_events
            )
            assert any(
                e.get("type") == "resource-snapshot"
                and "/api/ping" in e["snapshot"]["request"]["url"]
                and e["snapshot"]["response"]["status"] == 200
                for e in net_events
            )

            # Chunking lifecycle
            chunk1 = tmp_path / "async-chunk1.zip"
            chunk2 = tmp_path / "async-chunk2.zip"
            await ctx.tracing.start(title="Chunk Zero", screenshots=True)
            await pre_page.click("#btn")
            await ctx.tracing.stop_chunk(path=chunk1)
            with pytest.raises(Error, match="Must start a chunk"):
                await ctx.tracing.stop_chunk(path=chunk2)
            await ctx.tracing.start_chunk(name="part-2", title="Chunk Two")
            await pre_page.goto(f"{base}/chunk2")
            await ctx.tracing.stop_chunk(path=chunk2)
            await ctx.tracing.stop()

            c1, _ = _assert_valid_trace_zip(chunk1, expected_title="Chunk Zero", expected_apis=("page.click",))
            c2, _ = _assert_valid_trace_zip(chunk2, expected_title="Chunk Two", expected_apis=("page.goto",))
            assert not any(e.get("type") == "before" and e.get("apiName") == "page.click" for e in c2)
            assert any(e.get("type") == "before" and e.get("apiName") == "page.click" for e in c1)

            await ctx.close()
        finally:
            await browser.close()


def test_async_tracing(base: str, tmp_path: Path) -> None:
    asyncio.run(_run_async_tracing(base, tmp_path))


def test_sync_tracing(base: str, tmp_path: Path) -> None:
    with sync_playwright() as p:
        browser = p.chromium.launch(headless=True)
        try:
            ctx = browser.new_context()
            assert isinstance(ctx.tracing, SyncTracing)

            with pytest.raises(Error, match="not been started"):
                ctx.tracing.stop(path=tmp_path / "never.zip")
            ctx.tracing.stop()

            page = ctx.new_page()
            trace_zip = tmp_path / "sync-trace.zip"
            ctx.tracing.start(name="py-sync", title="Sync Trace", screenshots=True, snapshots=True)
            page.goto(base)
            page.locator("#name").fill("sync-flux")
            page.click("#btn")
            page.evaluate("() => fetch('/api/ping').then((r) => r.json())")
            time.sleep(0.05)
            ctx.tracing.stop(path=trace_zip)

            _, net_events = _assert_valid_trace_zip(
                trace_zip,
                expected_title="Sync Trace",
                expected_name="py-sync",
                expected_apis=("page.goto", "locator.fill", "page.click"),
            )
            assert any(
                e.get("type") == "resource-snapshot"
                and "/api/ping" in e["snapshot"]["request"]["url"]
                and e["snapshot"]["response"]["status"] == 200
                for e in net_events
            )

            c1 = tmp_path / "sync-c1.zip"
            c2 = tmp_path / "sync-c2.zip"
            ctx.tracing.start(title="Sync C1", screenshots=True)
            page.click("#btn")
            ctx.tracing.stop_chunk(path=c1)
            ctx.tracing.start_chunk(title="Sync C2")
            page.goto(f"{base}/c2")
            ctx.tracing.stop_chunk(path=c2)
            ctx.tracing.stop()

            _assert_valid_trace_zip(c1, expected_title="Sync C1", expected_apis=("page.click",))
            _assert_valid_trace_zip(c2, expected_title="Sync C2", expected_apis=("page.goto",))

            # Sync tracing.start(sources=True), camelCase methods, selector params, and no double-tracing.
            sync_loc_zip = tmp_path / "sync-loc.zip"
            ctx.tracing.start(title="Sync Loc", screenshots=True, snapshots=True, sources=True)
            page.locator("#sel").select_option("b")
            page.set_content("<p id='sc'>hello</p>")
            ctx.tracing.stop(path=sync_loc_zip)

            sync_events, _ = _assert_valid_trace_zip(
                sync_loc_zip,
                expected_title="Sync Loc",
                expected_apis=("locator.select_option", "page.set_content"),
            )
            sel_befores = [
                e for e in sync_events if e.get("type") == "before" and e.get("apiName") == "locator.select_option"
            ]
            assert len(sel_befores) == 1
            assert sel_befores[0]["class"] == "Locator"
            assert sel_befores[0]["method"] == "selectOption"
            assert "#sel" in sel_befores[0]["params"].get("selector", "")
            sc_before = next(
                e for e in sync_events if e.get("type") == "before" and e.get("apiName") == "page.set_content"
            )
            assert sc_before["method"] == "setContent"

            ctx.close()
        finally:
            browser.close()


async def _run_async_locator_and_audit_tracing(base: str, tmp_path: Path) -> None:
    async with async_playwright() as p:
        browser = await p.chromium.launch(headless=True)
        try:
            ctx = await browser.new_context()
            page = await ctx.new_page()

            # Wrapped async Locator works with expect() and traces all actions + camelCase + kwargs + error stack.
            loc_zip = tmp_path / "async-locator-trace.zip"
            await ctx.tracing.start(
                title="Async Locator Audit",
                screenshots=True,
                snapshots=True,
                sources=True,
            )
            await page.goto(url=base)
            await page.wait_for_url(f"{base}**")

            name_loc = page.locator("#name").first
            assert isinstance(name_loc, AsyncLocator)
            await name_loc.fill("async-traced")
            await name_loc.press("Enter")
            await page.locator("#sel").select_option("b")
            await page.get_by_role("button", name="Go").click()
            await async_expect(page.locator("#out")).to_have_text("clicked")
            await page.locator("#btn").dispatch_event("click")
            await page.click(selector="#btn")

            with pytest.raises(Error):
                await page.locator("#does-not-exist").click(timeout=100)

            await ctx.tracing.stop(path=loc_zip)

            events, _ = _assert_valid_trace_zip(
                loc_zip,
                expected_title="Async Locator Audit",
                expected_apis=(
                    "page.goto",
                    "page.wait_for_url",
                    "locator.fill",
                    "locator.press",
                    "locator.select_option",
                    "locator.click",
                    "locator.dispatch_event",
                    "page.click",
                ),
            )
            befores = [e for e in events if e.get("type") == "before"]
            afters_by_id = {e["callId"]: e for e in events if e.get("type") == "after"}

            # Kwargs inspection on page.goto(url=...) and page.click(selector=...)
            goto_ev = next(e for e in befores if e["apiName"] == "page.goto")
            assert goto_ev["params"].get("url") == base
            page_click_ev = next(e for e in befores if e["apiName"] == "page.click")
            assert page_click_ev["params"].get("selector") == "#btn"

            # wait_for_url must NOT double-trace a nested wait_for_load_state
            wfu_ev = next(e for e in befores if e["apiName"] == "page.wait_for_url")
            assert wfu_ev["method"] == "waitForURL"
            assert not any(e["apiName"] == "page.wait_for_load_state" for e in befores)

            # Async Locator actions traced with class="Locator", camelCase method, and params["selector"]
            loc_sel_ev = next(e for e in befores if e["apiName"] == "locator.select_option")
            assert loc_sel_ev["class"] == "Locator"
            assert loc_sel_ev["method"] == "selectOption"
            assert "#sel" in loc_sel_ev["params"].get("selector", "")

            loc_disp_ev = next(e for e in befores if e["apiName"] == "locator.dispatch_event")
            assert loc_disp_ev["class"] == "Locator"
            assert loc_disp_ev["method"] == "dispatchEvent"
            assert "#btn" in loc_disp_ev["params"].get("selector", "")

            # Failed locator action records name, message, and stack in after["error"]
            failed_before = next(
                e
                for e in befores
                if e["apiName"] == "locator.click" and "#does-not-exist" in e["params"].get("selector", "")
            )
            failed_after = afters_by_id[failed_before["callId"]]
            assert "error" in failed_after
            assert failed_after["error"].get("name")
            assert failed_after["error"].get("message")
            assert "Traceback" in failed_after["error"].get("stack", "")

            # stop(path=...) after stop_chunk() raises AND tears down cleanly so start() works again.
            c1 = tmp_path / "c1.zip"
            await ctx.tracing.start(title="Teardown Check", screenshots=True)
            await page.locator("#btn").click()
            await ctx.tracing.stop_chunk(path=c1)
            with pytest.raises(Error, match="No active trace chunk"):
                await ctx.tracing.stop(path=tmp_path / "never-written.zip")
            assert not ctx.tracing._is_active()
            # Must be able to start again without "already started".
            await ctx.tracing.start(title="Restarted", screenshots=False)
            await ctx.tracing.stop()

            await ctx.close()
        finally:
            await browser.close()


def test_async_locator_tracing_and_lifecycle(base: str, tmp_path: Path) -> None:
    asyncio.run(_run_async_locator_and_audit_tracing(base, tmp_path))


def test_tracing_unit_cross_chunk_isolation_leak_and_multipage_fallback(tmp_path: Path) -> None:
    """Unit tests for _chunk_seq isolation, failed write preservation, CDP leak cleanup, and multi-page fallback."""
    _MINI_JPEG = bytes.fromhex(
        "ffd8ffe000104a46494600010100000100010000ffdb00430006040506050406060506070706080a100a0a09090a14"
        "0e0f0c1017141818171416161a1d251f1a1b231c1616202c20232627292a29191f2d302d283025282928ffdb004301"
        "0707070a080a130a0a13281a161a282828282828282828282828282828282828282828282828282828282828282828"
        "28282828282828282828282828ffc00011080002000203012200021101031101ffc4001f0000010501010101010100"
        "000000000000000102030405060708090a0bffc400b5100002010303020403050504040000017d0102030004110512"
        "2131410613516107227114328191a1082342b1c11552d1f02433627282090a161718191a25262728292a3435363738"
        "393a434445464748494a535455565758595a636465666768696a737475767778797a838485868788898a9293949596"
        "9798999aa2a3a4a5a6a7a8a9aab2b3b4b5b6b7b8b9bac2c3c4c5c6c7c8c9cad2d3d4d5d6d7d8d9dae1e2e3e4e5e6e7"
        "e8e9eaf1f2f3f4f5f6f7f8f9faffc4001f0100030101010101010101010000000000000102030405060708090a0bff"
        "c400b51100020102040403040705040400010277000102031104052131061241510761711322328108144291a1b1c1"
        "09233352f0156272d10a162434e125f11718191a262728292a35363738393a434445464748494a535455565758595a"
        "636465666768696a737475767778797a82838485868788898a92939495969798999aa2a3a4a5a6a7a8a9aab2b3b4b5"
        "b6b7b8b9bac2c3c4c5c6c7c8c9cad2d3d4d5d6d7d8d9dae2e3e4e5e6e7e8e9eaf2f3f4f5f6f7f8f9faffda000c0301"
        "0002110311003f00f9528a28a00fffd9"
    )

    class _FakePage:
        def __init__(self, tid: str) -> None:
            self._tid = tid
            self.listeners: dict[str, list] = {}
            self.screenshot_calls = 0

        def _target_id(self) -> str:
            return self._tid

        def is_closed(self) -> bool:
            return False

        def on(self, ev: str, fn: object) -> None:
            self.listeners.setdefault(ev, []).append(fn)

        def remove_listener(self, ev: str, fn: object) -> None:
            if ev in self.listeners and fn in self.listeners[ev]:
                self.listeners[ev].remove(fn)

        async def screenshot(self, **kwargs: object) -> bytes:
            self.screenshot_calls += 1
            return _MINI_JPEG

    class _FailStartCdp:
        def __init__(self) -> None:
            self.detached = False
            self.listeners: list = []

        def on(self, ev: str, fn: object) -> None:
            self.listeners.append(fn)

        def remove_listener(self, ev: str, fn: object) -> None:
            if fn in self.listeners:
                self.listeners.remove(fn)

        async def send(self, method: str, params: object = None) -> dict:
            if method == "Page.startScreencast":
                raise RuntimeError("screencast boom")
            return {}

        async def detach(self) -> None:
            self.detached = True

    class _FakeCtx:
        def __init__(self, pages: list[_FakePage], cdp_factory: object) -> None:
            self.pages = pages
            self._cdp_factory = cdp_factory

        def _is_closed(self) -> bool:
            return False

        async def new_cdp_session(self, page: _FakePage) -> object:
            return self._cdp_factory(page)  # type: ignore[operator]

    async def _run() -> None:
        # 1. Cross-chunk action isolation + failed write preservation + buffer clearing.
        p1 = _FakePage("target-1")
        p2 = _FakePage("target-2")
        cdps: list[_FailStartCdp] = []

        def _mk_cdp(_page: _FakePage) -> _FailStartCdp:
            c = _FailStartCdp()
            cdps.append(c)
            return c

        ctx = _FakeCtx([p1, p2], _mk_cdp)
        tracer = RawAsyncTracing(ctx)
        await tracer.start(title="Chunk A", screenshots=True, sources=True)
        # Watcher keys use stable _target_id(), NOT id(page).
        assert "target-1" in tracer._watchers
        assert "target-2" in tracer._watchers
        # Failed Page.startScreencast detached both CDP sessions (no leak!).
        assert len(cdps) == 2 and all(c.detached for c in cdps)

        # In-flight action crossing stop_chunk -> start_chunk must NOT emit orphaned after in chunk 2.
        started_ev = asyncio.Event()
        release_ev = asyncio.Event()

        async def _slow_action() -> str:
            started_ev.set()
            await release_ev.wait()
            return "ok"

        inflight = asyncio.create_task(
            tracer._run_action(p1, "Page", "slow_op", "page.slow_op", {}, _slow_action)
        )
        await started_ev.wait()

        # Failed write (target is an existing directory) preserves chunk for retry.
        bad_target = tmp_path / "is-a-dir"
        bad_target.mkdir()
        with pytest.raises(OSError):
            await tracer.stop_chunk(path=bad_target)
        assert tracer._chunk_active is True
        assert len(tracer._trace_lines) >= 2

        chunk1 = tmp_path / "unit-c1.zip"
        await tracer.stop_chunk(path=chunk1)
        # Buffers cleared after successful stop_chunk write.
        assert tracer._trace_lines == []
        assert tracer._network_lines == []
        assert tracer._resources == {}

        # Multi-page fallback screenshot captured BOTH p1 and p2 via page.screenshot() when cdp is None.
        assert p1.screenshot_calls >= 1
        assert p2.screenshot_calls >= 1
        c1_events, _ = _assert_valid_trace_zip(chunk1, expected_title="Chunk A", allow_in_flight=True)
        frame_pages = {e["pageId"] for e in c1_events if e.get("type") == "screencast-frame"}
        assert len(frame_pages) == 2

        await tracer.start_chunk(title="Chunk B")
        release_ev.set()
        assert await inflight == "ok"
        chunk2 = tmp_path / "unit-c2.zip"
        await tracer.stop_chunk(path=chunk2)
        c2_events, _ = _assert_valid_trace_zip(chunk2, expected_title="Chunk B")
        # No orphaned "after" for call@1 in Chunk B!
        assert [e for e in c2_events if e.get("type") == "after"] == []

        await tracer.stop()

    asyncio.run(_run())
