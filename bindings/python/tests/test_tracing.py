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
from fluxwright.async_api import Tracing as AsyncTracing
from fluxwright.async_api import async_playwright
from fluxwright.sync_api import Tracing as SyncTracing
from fluxwright.sync_api import sync_playwright

_HTML = b"""<!doctype html>
<html>
  <body style="margin:0;padding:16px;background:#fafafa">
    <h1 id="heading">Trace Demo</h1>
    <input id="name" type="text" />
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

            ctx.close()
        finally:
            browser.close()
