"""Tests for video recording (``record_video_dir`` / ``page.video``) and visual regression (``to_have_screenshot``)."""

from __future__ import annotations

import asyncio
import os
import struct
import subprocess
import threading
import time
import zlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

import pytest

from fluxwright import async_api, sync_api
from fluxwright._expect import _run_to_have_screenshot, _validate_screenshot_options
from fluxwright._video import (
    _RecordedFrame,
    _mux_frames,
    encode_mjpeg_avi,
    jpeg_dimensions,
)
from fluxwright._visual import (
    RgbaImage,
    _make_chunk,
    _PNG_SIGNATURE,
    artifact_paths,
    compare_images,
    decode_png,
    encode_png,
    should_update_snapshots,
)
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
        if self.path == "/popup":
            body = b"<!doctype html><title>Popup</title><div id='p' style='width:60px;height:60px;background:blue'>Pop</div>"
        elif self.path == "/self-close":
            body = b"<!doctype html><title>Closing</title><script>setTimeout(() => window.close(), 30)</script>"
        elif self.path == "/opener":
            body = (
                b"<!doctype html><title>Opener</title>"
                b"<button id='open-pop' onclick=\"window.open('/popup', '_blank')\">Open</button>"
                b"<button id='open-close' onclick=\"window.open('/self-close', '_blank')\">OpenClose</button>"
            )
        else:
            body = FIXTURE_HTML
        self.send_response(200)
        self.send_header("Content-Type", "text/html")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

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
                await async_expect(vpage.locator("#box")).to_have_screenshot(box_shot, timeout=1000)
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
                await async_expect(vpage.locator("#box")).to_have_screenshot(box_shot, threshold=0.0, timeout=1000)

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
                await async_expect(vpage.locator("#box")).to_have_screenshot(box_shot, timeout=1000)
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
                await async_expect(vpage).to_have_screenshot(page_shot, timeout=1000)

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
                sync_expect(vpage.locator("#box")).to_have_screenshot(shot, timeout=1000)
            assert (snap_dir / "sync_box-actual.png").exists()
            assert (snap_dir / "sync_box-diff.png").exists()
            vpage.close()
        finally:
            browser.close()


def test_visual_codec_alpha_validation_and_paths(tmp_path: Path, monkeypatch: pytest.MonkeyPatch):
    # 1. Alpha-channel blind spot: opaque #C0C0C0 vs transparent (0,0,0,0) must differ at threshold=0.0.
    opaque_gray = RgbaImage(2, 2, bytes([192, 192, 192, 255] * 4))
    transparent = RgbaImage(2, 2, bytes([0, 0, 0, 0] * 4))
    diff = compare_images(opaque_gray, transparent, threshold=0.0, render_diff=False)
    assert diff.diff_pixels == 4

    opaque_black = RgbaImage(2, 2, bytes([0, 0, 0, 255] * 4))
    assert compare_images(opaque_black, transparent, threshold=0.0).diff_pixels == 4
    opaque_white = RgbaImage(2, 2, bytes([255, 255, 255, 255] * 4))
    assert compare_images(opaque_white, transparent, threshold=0.0).diff_pixels == 4
    # Two fully transparent pixels with different hidden RGB values are visually identical.
    transparent_red = RgbaImage(2, 2, bytes([255, 0, 0, 0] * 4))
    assert compare_images(transparent, transparent_red, threshold=0.0).diff_pixels == 0

    # 2. PNG IHDR validation, missing PLTE for type 3, truncated IDAT, and grayscale tRNS without masking.
    def _build_png(ihdr_fields: tuple[int, int, int, int, int, int, int], raw_data: bytes, extra: bytes = b"") -> bytes:
        ihdr = struct.pack(">IIBBBBB", *ihdr_fields)
        return (
            _PNG_SIGNATURE
            + _make_chunk(b"IHDR", ihdr)
            + extra
            + _make_chunk(b"IDAT", zlib.compress(raw_data))
            + _make_chunk(b"IEND", b"")
        )

    # Zero width/height rejected.
    with pytest.raises(ValueError, match="dimensions"):
        decode_png(_build_png((0, 4, 8, 6, 0, 0, 0), b""))
    # Invalid (color_type, bit_depth) rejected (RGB 4-bit is invalid per PNG spec).
    with pytest.raises(ValueError, match="color_type"):
        decode_png(_build_png((2, 2, 4, 2, 0, 0, 0), b"\x00" * 10))
    # Indexed type 3 without PLTE rejected.
    with pytest.raises(ValueError, match="PLTE"):
        decode_png(_build_png((2, 2, 8, 3, 0, 0, 0), b"\x00\x00\x00\x00\x00\x00"))
    # Truncated IDAT rejected cleanly (no silent bytearray resize or IndexError).
    with pytest.raises(ValueError, match="truncated"):
        decode_png(_build_png((4, 4, 8, 6, 0, 0, 0), b"\x00\x01\x02"))
    # Grayscale 8-bit tRNS value 0x0100 (256) must NOT mask to 0 and turn black pixel transparent.
    gray_trns_png = _build_png(
        (2, 1, 8, 0, 0, 0, 0),
        b"\x00\x00\xff",
        extra=_make_chunk(b"tRNS", struct.pack(">H", 256)),
    )
    decoded_gray = decode_png(gray_trns_png)
    assert decoded_gray.data[:4] == bytes([0, 0, 0, 255])

    # 3. artifact_paths rejects '.', '..', traversal, empty/trailing slash, and bytes.
    for bad in (".", "..", "../escape.png", "a/../b.png", "", "   ", "/", "sub/", "sub/.", ["a", "..", "b.png"], ["."]):
        with pytest.raises(ValueError):
            artifact_paths(bad)  # type: ignore[arg-type]
    with pytest.raises(TypeError):
        artifact_paths(b"box.png")  # type: ignore[arg-type]

    # 4. should_update_snapshots: explicit False overrides FLUXWRIGHT_UPDATE_SNAPSHOTS=1.
    monkeypatch.setenv("FLUXWRIGHT_UPDATE_SNAPSHOTS", "1")
    assert should_update_snapshots(None) is True
    assert should_update_snapshots(True) is True
    assert should_update_snapshots(False) is False

    # 5. Strict bool & clip validation in _validate_screenshot_options.
    for kw in ({"update": "false"}, {"omit_background": "false"}, {"full_page": "false"}):
        with pytest.raises(TypeError):
            _validate_screenshot_options(
                "test",
                threshold=None,
                max_diff_pixels=None,
                max_diff_pixel_ratio=None,
                animations=None,
                timeout=None,
                default_timeout=1000.0,
                omit_background=kw.get("omit_background"),  # type: ignore[arg-type]
                full_page=kw.get("full_page"),  # type: ignore[arg-type]
                clip=None,
                update=kw.get("update"),  # type: ignore[arg-type]
            )
    for bad_clip in (
        {"x": 0, "y": 0, "width": 0, "height": 10},
        {"x": 0, "y": 0, "width": 10},
        {"x": 0, "y": 0, "width": 10, "height": 10, "extra": 1},
    ):
        with pytest.raises(ValueError):
            _validate_screenshot_options(
                "test",
                threshold=None,
                max_diff_pixels=None,
                max_diff_pixel_ratio=None,
                animations=None,
                timeout=None,
                default_timeout=1000.0,
                omit_background=None,
                full_page=None,
                clip=bad_clip,  # type: ignore[arg-type]
                update=None,
            )

    # 6. _run_to_have_screenshot: timeout=0 still requires 2 matching frames, and OSError fails immediately.
    png_a = encode_png(2, 2, bytes([255, 0, 0, 255] * 4))
    png_b = encode_png(2, 2, bytes([0, 0, 255, 255] * 4))
    seq = [png_a, png_b, png_b]
    calls = 0

    async def _capture(_limit: float) -> bytes:
        nonlocal calls
        idx = min(calls, len(seq) - 1)
        calls += 1
        return seq[idx]

    async def _noop(*_args: object) -> None:
        return None

    target_zero = tmp_path / "zero-timeout.png"
    opts_zero = _validate_screenshot_options(
        "test",
        threshold=0.0,
        max_diff_pixels=None,
        max_diff_pixel_ratio=None,
        animations="allow",
        timeout=0,
        default_timeout=1000.0,
        omit_background=None,
        full_page=None,
        clip=None,
        update=True,
    )
    asyncio.run(
        _run_to_have_screenshot(
            target_label="test",
            name=target_zero,
            opts=opts_zero,
            is_closed=lambda: False,
            disable_animations=_noop,
            restore_animations=_noop,
            capture_shot=_capture,
        )
    )
    assert calls == 3
    assert target_zero.read_bytes() == png_b

    # OSError when writing baseline fails immediately instead of spinning to timeout.
    unwritable = tmp_path / "dir-as-baseline"
    unwritable.mkdir()
    opts_oserr = _validate_screenshot_options(
        "test",
        threshold=0.0,
        max_diff_pixels=None,
        max_diff_pixel_ratio=None,
        animations="allow",
        timeout=5000,
        default_timeout=5000.0,
        omit_background=None,
        full_page=None,
        clip=None,
        update=True,
    )
    started = time.monotonic()
    with pytest.raises(OSError):
        asyncio.run(
            _run_to_have_screenshot(
                target_label="test",
                name=unwritable,
                opts=opts_oserr,
                is_closed=lambda: False,
                disable_animations=_noop,
                restore_animations=_noop,
                capture_shot=lambda _l: asyncio.sleep(0, result=png_b),
            )
        )
    assert time.monotonic() - started < 2.0


def test_video_mux_fallback_timeout_and_avi_dims(tmp_path: Path, monkeypatch: pytest.MonkeyPatch):
    # Force ffmpeg to time out -> must fall back cleanly to MJPEG-AVI and prefer first_dims over size.
    def _timeout_run(*args: object, **kwargs: object) -> object:
        assert kwargs.get("timeout") == 60.0
        raise subprocess.TimeoutExpired(cmd="ffmpeg", timeout=60.0)

    monkeypatch.setattr(subprocess, "run", _timeout_run)
    # 3x2 JPEG while requested size is 640x480: AVI header must use actual 3x2 frame dims.
    fake_jpeg = bytes([0xFF, 0xD8, 0xFF, 0xC0, 0, 11, 8, 0, 2, 0, 3, 1, 1, 17, 0, 0xFF, 0xD9])
    out = _mux_frames(tmp_path, "dimcheck", (640, 480), [_RecordedFrame(fake_jpeg, 1.0, None, None)])
    assert out.suffix == ".avi"
    raw = out.read_bytes()
    # avih starts after RIFF(12) + LIST hdrl(12) + avih header(8) = byte 32; width at +32, height at +36.
    avih_pos = raw.index(b"avih") + 8
    avi_w, avi_h = struct.unpack_from("<II", raw, avih_pos + 32)
    assert (avi_w, avi_h) == (3, 2)


async def _run_async_video_popup_self_close_and_visual_audit(base: str, tmp_path: Path) -> None:
    video_dir = tmp_path / "audit_videos"
    snap_dir = tmp_path / "audit_snaps"

    async with async_playwright() as p:
        browser = await p.chromium.launch(max_browsers=1)
        try:
            # 1. Popup video is IMMEDIATELY non-None on popup arrival, and self-closing popup never deadlocks.
            ctx = await browser.new_context(
                viewport={"width": 480, "height": 360},
                record_video_dir=video_dir,
            )
            opener = await ctx.new_page()
            await opener.goto(f"{base}opener")

            async with opener.expect_popup() as pop_info:
                await opener.click("#open-pop")
            popup = await pop_info.value
            # Immediately non-None right on arrival (no race!).
            assert popup.video is not None
            pop_video = popup.video
            await popup.wait_for_selector("#p")
            await popup.close()
            pop_path = await asyncio.wait_for(pop_video.path(), timeout=10.0)
            assert pop_path.exists()
            assert pop_video._recorder._frames == []
            _assert_valid_video_header(pop_path.read_bytes())

            # 2. Self-closing popup via window.close(): must NOT deadlock on path() or ctx.close()!
            async with opener.expect_popup() as close_info:
                await opener.click("#open-close")
            self_closing = await close_info.value
            assert self_closing.video is not None
            sc_video = self_closing.video
            # Wait briefly for window.close() to run and access ctx.pages (pruning closed page).
            await asyncio.sleep(0.25)
            _ = ctx.pages
            await ctx.close()
            sc_path = await asyncio.wait_for(sc_video.path(), timeout=10.0)
            assert sc_path.exists()
            _assert_valid_video_header(sc_path.read_bytes())

            # 3. update=False overrides FLUXWRIGHT_UPDATE_SNAPSHOTS=1, and detached locator cleans animation style.
            vpage = await browser.new_page(viewport={"width": 320, "height": 240})
            await vpage.goto(base)
            prev_env = os.environ.get("FLUXWRIGHT_UPDATE_SNAPSHOTS")
            os.environ["FLUXWRIGHT_UPDATE_SNAPSHOTS"] = "1"
            try:
                missing_shot = snap_dir / "must-not-create.png"
                with pytest.raises(AssertionError, match="Baseline snapshot does not exist"):
                    await async_expect(vpage.locator("#box")).to_have_screenshot(
                        missing_shot, update=False, timeout=800
                    )
                assert not missing_shot.exists()
            finally:
                if prev_env is None:
                    os.environ.pop("FLUXWRIGHT_UPDATE_SNAPSHOTS", None)
                else:
                    os.environ["FLUXWRIGHT_UPDATE_SNAPSHOTS"] = prev_env

            # Detach #box mid-assertion and confirm <style id="__fw_disable_animations"> is cleaned up.
            detach_shot = snap_dir / "detach-baseline.png"
            await async_expect(vpage.locator("#box")).to_have_screenshot(detach_shot, update=True)
            await vpage.evaluate(
                "() => { document.getElementById('box').style.background = 'rgb(0, 255, 0)'; "
                "setTimeout(() => { const b = document.getElementById('box'); if (b) b.remove(); }, 80); }"
            )
            with pytest.raises(AssertionError):
                await async_expect(vpage.locator("#box")).to_have_screenshot(
                    detach_shot, animations="disabled", timeout=350
                )
            has_style = await vpage.evaluate("() => Boolean(document.getElementById('__fw_disable_animations'))")
            assert has_style is False

            await vpage.close()
        finally:
            await browser.close()


def test_async_popup_video_self_close_and_visual_audit(base: str, tmp_path: Path):
    asyncio.run(_run_async_video_popup_self_close_and_visual_audit(base, tmp_path))
