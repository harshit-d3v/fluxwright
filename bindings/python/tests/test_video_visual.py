"""Tests for video recording (``record_video_dir`` / ``page.video``) and visual regression (``to_have_screenshot``)."""

from __future__ import annotations

import asyncio
import os
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

import pytest

from fluxwright import async_api, sync_api
from fluxwright._video import encode_mjpeg_avi, jpeg_dimensions
from fluxwright._visual import compare_images, decode_png, encode_png
from fluxwright.async_api import Video as AsyncVideo
from fluxwright.async_api import async_playwright, expect as async_expect
from fluxwright.sync_api import Video as SyncVideo
from fluxwright.sync_api import expect as sync_expect, sync_playwright

FIXTURE_HTML = b"""<!doctype html>
<style>
  body { margin: 0; background: #ffffff; }
  #box { position: relative; width: 80px; height: 80px; background: rgb(255, 0, 0); }
  #dot { position: absolute; top: 0; left: 0; width: 2px; height: 2px; background: rgb(255, 0, 0); }
  @keyframes pulse {
    0% { background: rgb(255, 0, 0); }
    50% { background: rgb(0, 255, 0); }
    100% { background: rgb(0, 0, 255); }
  }
  #anim { width: 40px; height: 40px; background: rgb(255, 0, 0); animation: pulse 0.2s linear infinite; }
</style>
<div id="box"><div id="dot"></div></div>
<div id="anim"></div>
"""


class _Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(200)
        self.send_header("Content-Type", "text/html")
        self.send_header("Content-Length", str(len(FIXTURE_HTML)))
        self.end_headers()
        self.wfile.write(FIXTURE_HTML)

    def log_message(self, *args):
        pass


@pytest.fixture(scope="module")
def base():
    server = ThreadingHTTPServer(("127.0.0.1", 0), _Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    yield f"http://127.0.0.1:{server.server_address[1]}/"
    server.shutdown()


def _assert_valid_video_header(data: bytes) -> None:
    assert len(data) > 16, f"video too small: {len(data)}"
    is_webm = data.startswith(b"\x1a\x45\xdf\xa3")
    is_avi = len(data) >= 12 and data[0:4] == b"RIFF" and data[8:12] == b"AVI "
    assert is_webm or is_avi, f"unexpected video header: {data[:16].hex()}"


def test_exports_and_pure_codecs():
    assert "Video" in async_api.__all__
    assert "Video" in sync_api.__all__

    fake_jpeg = bytes([0xFF, 0xD8, 0xFF, 0xC0, 0, 11, 8, 0, 2, 0, 3, 1, 1, 17, 0, 0xFF, 0xD9])
    assert jpeg_dimensions(fake_jpeg) == (3, 2)
    avi = encode_mjpeg_avi([fake_jpeg, fake_jpeg], 10, 320, 240)
    assert avi[0:4] == b"RIFF"
    assert avi[8:12] == b"AVI "
    assert int.from_bytes(avi[4:8], "little") + 8 == len(avi)
    assert b"MJPG" in avi
    assert b"00dc" in avi
    assert b"idx1" in avi

    red = bytes([255, 0, 0, 255] * 16)
    blue = bytes([0, 0, 255, 255] * 16)
    red_png = encode_png(4, 4, red)
    blue_png = encode_png(4, 4, blue)
    dec_red = decode_png(red_png)
    dec_blue = decode_png(blue_png)
    assert (dec_red.width, dec_red.height) == (4, 4)
    assert dec_red.data == red

    diff = compare_images(dec_red, dec_blue, threshold=0.2, render_diff=True)
    assert diff.diff_pixels == 16
    assert diff.diff_ratio == pytest.approx(1.0)
    assert diff.diff_png is not None
    diff_img = decode_png(diff.diff_png)
    assert diff_img.data[:4] == bytes([255, 0, 255, 255])


def test_async_video_and_visual(base: str, tmp_path: Path):
    asyncio.run(_run_async(base, tmp_path))


async def _run_async(base: str, tmp_path: Path) -> None:
    video_dir = tmp_path / "async_videos"
    snap_dir = tmp_path / "async_snaps"

    async with async_playwright() as p:
        browser = await p.chromium.launch(max_browsers=1)
        try:
            plain = await browser.new_page()
            assert plain.video is None
            await plain.close()

            # Video via browser.new_context(record_video_dir=..., record_video_size=...).
            ctx = await browser.new_context(
                viewport={"width": 640, "height": 480},
                record_video_dir=video_dir,
                record_video_size={"width": 640, "height": 480},
            )
            page = await ctx.new_page()
            video = page.video
            assert isinstance(video, AsyncVideo)
            await page.goto(base)
            await page.evaluate("() => setTimeout(() => { document.getElementById('box').style.background = 'rgb(0,0,255)'; }, 40)")
            await asyncio.sleep(0.15)
            await page.close()

            recorded_path = await video.path()
            assert recorded_path.parent == video_dir
            raw = recorded_path.read_bytes()
            _assert_valid_video_header(raw)

            saved = tmp_path / "exports" / "async_copy.vid"
            await video.save_as(saved)
            assert saved.read_bytes() == raw

            await video.delete()
            assert not recorded_path.exists()
            await video.delete()  # idempotent
            await ctx.close()

            # Visual regression tests.
            vpage = await browser.new_page(viewport={"width": 320, "height": 240})
            await vpage.goto(base)

            box_shot = snap_dir / "box.png"
            box_actual = snap_dir / "box-actual.png"
            box_diff = snap_dir / "box-diff.png"
            env_shot = snap_dir / "env-box.png"
            anim_shot = snap_dir / "anim.png"
            page_shot = snap_dir / "page.png"

            with pytest.raises(AssertionError, match="Baseline snapshot does not exist"):
                await async_expect(vpage.locator("#box")).to_have_screenshot(box_shot, timeout=250)
            assert not box_shot.exists()
            assert box_actual.exists()

            await async_expect(vpage.locator("#box")).to_have_screenshot(box_shot, update=True)
            assert box_shot.exists()
            await async_expect(vpage.locator("#box")).to_have_screenshot(box_shot)

            prev_env = os.environ.get("FLUXWRIGHT_UPDATE_SNAPSHOTS")
            os.environ["FLUXWRIGHT_UPDATE_SNAPSHOTS"] = "1"
            try:
                await async_expect(vpage.locator("#box")).to_have_screenshot(env_shot)
            finally:
                if prev_env is None:
                    os.environ.pop("FLUXWRIGHT_UPDATE_SNAPSHOTS", None)
                else:
                    os.environ["FLUXWRIGHT_UPDATE_SNAPSHOTS"] = prev_env
            assert env_shot.exists()

            # Delayed restoration passes via polling.
            await vpage.evaluate(
                "() => { document.getElementById('box').style.background = 'rgb(0, 255, 0)'; "
                "setTimeout(() => { document.getElementById('box').style.background = 'rgb(255, 0, 0)'; }, 120); }"
            )
            await async_expect(vpage.locator("#box")).to_have_screenshot(box_shot, timeout=3000)

            # Threshold pass vs strict threshold=0.0 fail.
            await vpage.evaluate("() => { document.getElementById('box').style.background = 'rgb(225, 0, 0)'; }")
            await async_expect(vpage.locator("#box")).to_have_screenshot(box_shot)
            with pytest.raises(AssertionError, match="Screenshot mismatch"):
                await async_expect(vpage.locator("#box")).to_have_screenshot(box_shot, threshold=0.0, timeout=250)

            # max_diff_pixels & max_diff_pixel_ratio.
            await vpage.evaluate(
                "() => { document.getElementById('box').style.background = 'rgb(255, 0, 0)'; "
                "document.getElementById('dot').style.background = 'rgb(0, 0, 255)'; }"
            )
            await async_expect(vpage.locator("#box")).to_have_screenshot(box_shot, max_diff_pixels=16)
            await async_expect(vpage.locator("#box")).to_have_screenshot(box_shot, max_diff_pixel_ratio=0.01)

            box_actual.unlink(missing_ok=True)
            box_diff.unlink(missing_ok=True)
            with pytest.raises(AssertionError) as exc_info:
                await async_expect(vpage.locator("#box")).to_have_screenshot(box_shot, timeout=250)
            msg = str(exc_info.value)
            assert "pixels differ" in msg
            assert "ratio " in msg
            assert "box-actual.png" in msg
            assert "box-diff.png" in msg
            assert box_actual.exists()
            assert box_diff.exists()
            diff_img = decode_png(box_diff.read_bytes())
            assert any(
                diff_img.data[i : i + 4] == b"\xff\x00\xff\xff"
                for i in range(0, len(diff_img.data), 4)
            )

            # animations='disabled' pauses CSS animations.
            await async_expect(vpage.locator("#anim")).to_have_screenshot(anim_shot, animations="disabled", update=True)
            await asyncio.sleep(0.08)
            await async_expect(vpage.locator("#anim")).to_have_screenshot(anim_shot)

            # Page-level to_have_screenshot.
            await async_expect(vpage).to_have_screenshot(page_shot, update=True)
            await async_expect(vpage).to_have_screenshot(page_shot)
            await vpage.evaluate("() => { document.body.style.background = '#000000'; }")
            with pytest.raises(AssertionError, match="page-diff.png"):
                await async_expect(vpage).to_have_screenshot(page_shot, timeout=250)

            await vpage.close()
        finally:
            await browser.close()


def test_sync_video_and_visual(base: str, tmp_path: Path):
    video_dir = tmp_path / "sync_videos"
    snap_dir = tmp_path / "sync_snaps"

    with sync_playwright() as p:
        browser = p.chromium.launch(max_browsers=1)
        try:
            ctx = browser.new_context(
                viewport={"width": 480, "height": 360},
                record_video_dir=video_dir,
                record_video_size={"width": 480, "height": 360},
            )
            page = ctx.new_page()
            video = page.video
            assert isinstance(video, SyncVideo)
            page.goto(base)
            time.sleep(0.1)
            page.close()

            vpath = video.path()
            assert vpath.exists()
            _assert_valid_video_header(vpath.read_bytes())
            dest = tmp_path / "sync_saved.vid"
            video.save_as(dest)
            assert dest.exists()
            video.delete()
            assert not vpath.exists()
            ctx.close()

            vpage = browser.new_page(viewport={"width": 320, "height": 240})
            vpage.goto(base)
            shot = snap_dir / "sync_box.png"
            sync_expect(vpage.locator("#box")).to_have_screenshot(shot, update=True)
            sync_expect(vpage.locator("#box")).to_have_screenshot(shot)
            sync_expect(vpage).to_have_screenshot(snap_dir / "sync_page.png", update=True)
            sync_expect(vpage).to_have_screenshot(snap_dir / "sync_page.png")

            vpage.evaluate("() => { document.getElementById('box').style.background = 'rgb(0, 0, 255)'; }")
            with pytest.raises(AssertionError, match="sync_box-diff.png"):
                sync_expect(vpage.locator("#box")).to_have_screenshot(shot, timeout=250)
            assert (snap_dir / "sync_box-actual.png").exists()
            assert (snap_dir / "sync_box-diff.png").exists()
            vpage.close()
        finally:
            browser.close()
