"""Per-page video recording via CDP ``Page.startScreencast`` (WebM via ffmpeg or MJPEG-in-AVI fallback)."""

from __future__ import annotations

import asyncio
import base64
import os
import shutil
import struct
import subprocess
import uuid
from collections.abc import Awaitable, Callable, Mapping
from contextlib import suppress
from dataclasses import dataclass
from os import PathLike
from pathlib import Path
from typing import Any

from ._errors import Error

_SCREENCAP_SETTLE_S = 0.025
_FFMPEG_TIMEOUT_S = 60.0

# Valid 2x2 black baseline JPEG used only if a page closes before any frame can be captured.
_FALLBACK_JPEG = bytes.fromhex(
    "ffd8ffe000104a46494600010100000100010000ffdb00430006040506050406060506070706080a100a0a09090a14"
    "0e0f0c1017141818171416161a1d251f1a1b231c1616202c20232627292a29191f2d302d283025282928ffdb004301"
    "0707070a080a130a0a13281a161a282828282828282828282828282828282828282828282828282828282828282828"
    "2828282828282828282828282828282828ffc0001108000200020301220002110103"
    "1101ffc4001f0000010501010101010100000000000000000102030405060708090a0bffc400b51000020103030204"
    "03050504040000017d01020300041105122131410613516107227114328191a1082342b1c11552d1f0243362728209"
    "0a161718191a25262728292a3435363738393a434445464748494a535455565758595a636465666768696a73747576"
    "7778797a838485868788898a92939495969798999aa2a3a4a5a6a7a8a9aab2b3b4b5b6b7b8b9bac2c3c4c5c6c7c8c9"
    "cad2d3d4d5d6d7d8d9dae1e2e3e4e5e6e7e8e9eaf1f2f3f4f5f6f7f8f9faffc4001f01000301010101010101010100"
    "00000000000102030405060708090a0bffc400b5110002010204040304070504040001027700010203110405213106"
    "1241510761711322328108144291a1b1c109233352f0156272d10a162434e125f11718191a262728292a3536373839"
    "3a434445464748494a535455565758595a636465666768696a737475767778797a82838485868788898a9293949596"
    "9798999aa2a3a4a5a6a7a8a9aab2b3b4b5b6b7b8b9bac2c3c4c5c6c7c8c9cad2d3d4d5d6d7d8d9dae2e3e4e5e6e7e8"
    "e9eaf2f3f4f5f6f7f8f9faffda000c03010002110311003f00f9528a28a00fffd9"
)


@dataclass
class _RecordedFrame:
    jpeg: bytes
    timestamp: float | None
    width: int | None
    height: int | None


def normalize_record_video(
    record_video_dir: str | PathLike[str] | None,
    record_video_size: Mapping[str, int] | None,
) -> tuple[Path, tuple[int, int] | None] | None:
    if record_video_dir is None:
        if record_video_size is not None:
            raise ValueError("record_video_size requires record_video_dir")
        return None
    dir_str = os.fspath(record_video_dir)
    if not isinstance(dir_str, str) or not dir_str:
        raise ValueError("record_video_dir must be a non-empty path")
    size_tuple: tuple[int, int] | None = None
    if record_video_size is not None:
        if not isinstance(record_video_size, Mapping):
            raise TypeError("record_video_size must be a mapping with 'width' and 'height'")
        unknown = set(record_video_size.keys()) - {"width", "height"}
        if unknown:
            raise ValueError(f"record_video_size: unsupported keys {sorted(unknown)}")
        w = record_video_size.get("width")
        h = record_video_size.get("height")
        if (
            isinstance(w, bool)
            or isinstance(h, bool)
            or not isinstance(w, int)
            or not isinstance(h, int)
            or w <= 0
            or h <= 0
        ):
            raise ValueError("record_video_size: width and height must be positive integers")
        size_tuple = (w, h)
    return Path(dir_str), size_tuple


def jpeg_dimensions(data: bytes) -> tuple[int, int] | None:
    if len(data) < 4 or data[0] != 0xFF or data[1] != 0xD8:
        return None
    i = 2
    n = len(data)
    while i + 3 < n:
        if data[i] != 0xFF:
            i += 1
            continue
        while i < n and data[i] == 0xFF:
            i += 1
        if i >= n:
            break
        marker = data[i]
        i += 1
        if marker in (0x00, 0x01) or 0xD0 <= marker <= 0xD9:
            continue
        if i + 1 >= n:
            break
        seg_len = int.from_bytes(data[i : i + 2], "big")
        if seg_len < 2 or i + seg_len > n:
            break
        if marker in (0xC0, 0xC1, 0xC2, 0xC3, 0xC5, 0xC6, 0xC7, 0xC9, 0xCA, 0xCB, 0xCD, 0xCE, 0xCF) and seg_len >= 7:
            h = int.from_bytes(data[i + 3 : i + 5], "big")
            w = int.from_bytes(data[i + 5 : i + 7], "big")
            if w > 0 and h > 0:
                return (w, h)
        i += seg_len
    return None


def _compute_fps(frames: list[_RecordedFrame]) -> int:
    ts = [f.timestamp for f in frames if f.timestamp is not None]
    if len(ts) < 2 or ts[-1] <= ts[0]:
        return 10
    duration = max(0.1, ts[-1] - ts[0])
    return max(1, min(30, round(len(frames) / duration)))


def encode_mjpeg_avi(frames: list[bytes], fps: int, width: int, height: int) -> bytes:
    safe_fps = max(1, int(fps))
    w = max(1, int(width))
    h = max(1, int(height))
    num_frames = len(frames)
    max_frame_size = max((len(f) for f in frames), default=0)
    microsec_per_frame = 1_000_000 // safe_fps

    avih = struct.pack(
        "<10I16s",
        microsec_per_frame,
        (max_frame_size * safe_fps) & 0xFFFFFFFF,
        0,
        0x10,  # AVIF_HASINDEX
        num_frames,
        0,
        1,
        max_frame_size,
        w,
        h,
        b"\x00" * 16,
    )
    strh = struct.pack(
        "<4s4sIHH8I4h",
        b"vids",
        b"MJPG",
        0,
        0,
        0,
        0,
        1,
        safe_fps,
        0,
        num_frames,
        max_frame_size,
        0xFFFFFFFF,
        0,
        0,
        0,
        min(0x7FFF, w),
        min(0x7FFF, h),
    )
    strf = struct.pack(
        "<IiiHH4sIiiII",
        40,
        w,
        h,
        1,
        24,
        b"MJPG",
        (w * h * 3) & 0xFFFFFFFF,
        0,
        0,
        0,
        0,
    )

    strl_payload_len = 4 + (8 + len(strh)) + (8 + len(strf))
    hdrl_payload_len = 4 + (8 + len(avih)) + (8 + strl_payload_len)

    movi_payload_len = 4
    offsets: list[tuple[int, int]] = []
    for f in frames:
        offsets.append((movi_payload_len, len(f)))
        movi_payload_len += 8 + len(f) + (len(f) & 1)

    idx1_len = num_frames * 16
    riff_payload_len = 4 + (8 + hdrl_payload_len) + (8 + movi_payload_len) + (8 + idx1_len)

    parts: list[bytes] = [
        b"RIFF",
        struct.pack("<I", riff_payload_len),
        b"AVI ",
        b"LIST",
        struct.pack("<I", hdrl_payload_len),
        b"hdrl",
        b"avih",
        struct.pack("<I", len(avih)),
        avih,
        b"LIST",
        struct.pack("<I", strl_payload_len),
        b"strl",
        b"strh",
        struct.pack("<I", len(strh)),
        strh,
        b"strf",
        struct.pack("<I", len(strf)),
        strf,
        b"LIST",
        struct.pack("<I", movi_payload_len),
        b"movi",
    ]
    for f in frames:
        parts.append(b"00dc")
        parts.append(struct.pack("<I", len(f)))
        parts.append(f)
        if len(f) & 1:
            parts.append(b"\x00")

    parts.append(b"idx1")
    parts.append(struct.pack("<I", idx1_len))
    for offset, sz in offsets:
        parts.append(struct.pack("<4sIII", b"00dc", 0x10, offset, sz))

    return b"".join(parts)


def _try_mux_webm(
    out_path: Path,
    frames: list[_RecordedFrame],
    fps: int,
    size: tuple[int, int] | None,
    first_dims: tuple[int, int] | None,
) -> bool:
    cmd = [
        "ffmpeg",
        "-y",
        "-loglevel",
        "error",
        "-f",
        "image2pipe",
        "-vcodec",
        "mjpeg",
        "-framerate",
        str(fps),
        "-i",
        "-",
    ]
    has_mixed = first_dims is not None and any(
        (d := jpeg_dimensions(f.jpeg)) is not None and d != first_dims for f in frames
    )
    scale_target = size or (first_dims if has_mixed else None)
    if scale_target is not None:
        w, h = scale_target
        cmd.extend(["-vf", f"scale={w}:{h}:force_original_aspect_ratio=decrease,pad={w}:{h}:(ow-iw)/2:(oh-ih)/2"])
    cmd.extend(["-c:v", "libvpx", "-b:v", "1M", "-pix_fmt", "yuv420p", "-an", str(out_path)])

    payload = b"".join(f.jpeg for f in frames)
    try:
        proc = subprocess.run(
            cmd,
            input=payload,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            check=False,
            timeout=_FFMPEG_TIMEOUT_S,
        )
    except (OSError, subprocess.TimeoutExpired):
        return False
    if proc.returncode != 0 or not out_path.exists():
        return False
    try:
        with out_path.open("rb") as fh:
            head = fh.read(4)
        return head == b"\x1a\x45\xdf\xa3"
    except OSError:
        return False


def _mux_frames(
    video_dir: Path,
    file_id: str,
    size: tuple[int, int] | None,
    frames: list[_RecordedFrame],
) -> Path:
    video_dir.mkdir(parents=True, exist_ok=True)
    if not frames:
        frames = [_RecordedFrame(jpeg=_FALLBACK_JPEG, timestamp=None, width=2, height=2)]
    fps = _compute_fps(frames)
    f0 = frames[0]
    first_dims = jpeg_dimensions(f0.jpeg) or ((f0.width, f0.height) if f0.width and f0.height else None)
    width, height = first_dims or size or (800, 600)

    webm_path = video_dir / f"{file_id}.webm"
    if _try_mux_webm(webm_path, frames, fps, size, first_dims):
        return webm_path
    with suppress(OSError):
        webm_path.unlink(missing_ok=True)

    avi_path = video_dir / f"{file_id}.avi"
    avi_bytes = encode_mjpeg_avi([f.jpeg for f in frames], fps, width, height)
    avi_path.write_bytes(avi_bytes)
    return avi_path


class Video:
    """Handle to a page's recorded video (`page.video`)."""

    def __init__(self, recorder: VideoRecorder) -> None:
        self._recorder = recorder

    async def path(self) -> Path:
        """Waits for recording to finish (on page/context close) and returns the video path."""
        return await self._recorder.wait_for_finished()

    async def save_as(self, path: str | PathLike[str]) -> None:
        """Copies the recorded video to ``path``, creating missing parent directories."""
        dest_str = os.fspath(path)
        if not isinstance(dest_str, str) or not dest_str:
            raise ValueError("video.save_as: path must be non-empty")
        src = await self.path()
        dest = Path(dest_str)

        def _copy() -> None:
            dest.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(src, dest)

        await asyncio.to_thread(_copy)

    async def delete(self) -> None:
        """Deletes the recorded video file if it exists."""
        src = await self.path()
        await asyncio.to_thread(src.unlink, missing_ok=True)


class VideoRecorder:
    """Active CDP screencast recorder attached to one async ``Page``."""

    def __init__(
        self,
        video_dir: Path,
        size: tuple[int, int] | None,
        session: Any,
        *,
        on_stopped: Callable[[VideoRecorder], None] | None = None,
    ) -> None:
        self._dir = video_dir
        self._size = size
        self._id = uuid.uuid4().hex
        self._session = session
        self._on_stopped = on_stopped
        self._frames: list[_RecordedFrame] = []
        self._ack_tasks: set[asyncio.Future[Any]] = set()
        self._done_future: asyncio.Future[Path] = asyncio.get_running_loop().create_future()
        self._stop_future: asyncio.Future[Path] | None = None
        self._start_task: asyncio.Task[None] | None = None
        self._stop_requested = False
        self._accepting_frames = True

    def _attach_session_close_hook(self, session: Any) -> None:
        on_close = getattr(session, "_on_close", None)
        if callable(on_close):
            on_close(self._on_session_closed)

    def _on_session_closed(self) -> None:
        if self._stop_future is None:
            with suppress(RuntimeError):
                self._trigger_stop()

    @classmethod
    async def start(
        cls,
        session: Any,
        video_dir: Path,
        size: tuple[int, int] | None,
        *,
        on_stopped: Callable[[VideoRecorder], None] | None = None,
    ) -> VideoRecorder:
        await asyncio.to_thread(video_dir.mkdir, parents=True, exist_ok=True)
        rec = cls(video_dir, size, session, on_stopped=on_stopped)
        session.on("Page.screencastFrame", rec._on_frame)
        try:
            with suppress(Exception):
                await session.send("Page.enable")
            params: dict[str, Any] = {
                "format": "jpeg",
                "quality": 80,
                "everyNthFrame": 1,
            }
            if size is not None:
                params["maxWidth"] = size[0]
                params["maxHeight"] = size[1]
            await session.send("Page.startScreencast", params)
        except BaseException:
            with suppress(Exception):
                session.remove_listener("Page.screencastFrame", rec._on_frame)
            with suppress(Exception):
                await session.detach()
            raise
        rec._attach_session_close_hook(session)
        return rec

    @classmethod
    def start_deferred(
        cls,
        session_factory: Callable[[], Awaitable[Any]],
        video_dir: Path,
        size: tuple[int, int] | None,
        *,
        is_closed: Callable[[], bool] | None = None,
        on_stopped: Callable[[VideoRecorder], None] | None = None,
    ) -> VideoRecorder:
        """Synchronously creates a recorder whose CDP screencast starts asynchronously.

        Used for popups so ``popup.video`` is immediately non-None before ``"popup"`` is emitted,
        and ``stop()`` chains cleanly behind startup even if the popup closes immediately.
        """
        rec = cls(video_dir, size, session=None, on_stopped=on_stopped)
        rec._start_task = asyncio.ensure_future(rec._startup(session_factory, is_closed))
        return rec

    async def _startup(
        self,
        session_factory: Callable[[], Awaitable[Any]],
        is_closed: Callable[[], bool] | None,
    ) -> None:
        session: Any = None
        attached_listener = False
        try:
            await asyncio.to_thread(self._dir.mkdir, parents=True, exist_ok=True)
            if self._stop_requested or (is_closed is not None and is_closed()):
                if is_closed is not None and is_closed():
                    self._trigger_stop()
                return
            session = await session_factory()
            if self._stop_requested or (is_closed is not None and is_closed()):
                with suppress(Exception):
                    await session.detach()
                if is_closed is not None and is_closed():
                    self._trigger_stop()
                return
            session.on("Page.screencastFrame", self._on_frame)
            attached_listener = True
            with suppress(Exception):
                await session.send("Page.enable")
            if self._stop_requested or (is_closed is not None and is_closed()):
                with suppress(Exception):
                    session.remove_listener("Page.screencastFrame", self._on_frame)
                with suppress(Exception):
                    await session.detach()
                if is_closed is not None and is_closed():
                    self._trigger_stop()
                return
            params: dict[str, Any] = {
                "format": "jpeg",
                "quality": 80,
                "everyNthFrame": 1,
            }
            if self._size is not None:
                params["maxWidth"] = self._size[0]
                params["maxHeight"] = self._size[1]
            await session.send("Page.startScreencast", params)
            self._session = session
            self._attach_session_close_hook(session)
            if is_closed is not None and is_closed():
                self._trigger_stop()
        except Exception:
            if session is not None:
                if attached_listener:
                    with suppress(Exception):
                        session.remove_listener("Page.screencastFrame", self._on_frame)
                with suppress(Exception):
                    await session.detach()
            if is_closed is not None and is_closed():
                self._trigger_stop()

    def _on_frame(self, params: dict[str, Any]) -> None:
        if not self._accepting_frames:
            return
        data_b64 = params.get("data")
        if isinstance(data_b64, str):
            with suppress(Exception):
                jpeg = base64.b64decode(data_b64)
                meta = params.get("metadata") or {}
                ts = meta.get("timestamp")
                dw = meta.get("deviceWidth")
                dh = meta.get("deviceHeight")
                self._frames.append(
                    _RecordedFrame(
                        jpeg=jpeg,
                        timestamp=float(ts) if isinstance(ts, (int, float)) else None,
                        width=max(1, round(float(dw))) if isinstance(dw, (int, float)) else None,
                        height=max(1, round(float(dh))) if isinstance(dh, (int, float)) else None,
                    )
                )
        ack_id = params.get("sessionId")
        if ack_id is not None and self._session is not None:
            fut = asyncio.ensure_future(self._ack(ack_id))
            self._ack_tasks.add(fut)
            fut.add_done_callback(self._ack_tasks.discard)

    async def _ack(self, session_id: Any) -> None:
        session = self._session
        if session is None:
            return
        with suppress(Exception):
            await session.send("Page.screencastFrameAck", {"sessionId": session_id})

    async def wait_for_finished(self) -> Path:
        return await asyncio.shield(self._done_future)

    def _trigger_stop(self) -> asyncio.Future[Path]:
        self._stop_requested = True
        if self._stop_future is None:
            fut = asyncio.ensure_future(self._finish())

            def _consume_exc(done: asyncio.Future[Path]) -> None:
                if not done.cancelled():
                    done.exception()

            fut.add_done_callback(_consume_exc)
            self._stop_future = fut
        return self._stop_future

    async def stop(self) -> Path:
        return await asyncio.shield(self._trigger_stop())

    async def _finish(self) -> Path:
        self._stop_requested = True
        try:
            if self._start_task is not None and not self._start_task.done():
                with suppress(Exception):
                    await self._start_task

            session = self._session
            if session is not None:
                with suppress(Exception):
                    await session.send("Page.stopScreencast")
                await asyncio.sleep(_SCREENCAP_SETTLE_S)
                self._accepting_frames = False
                with suppress(Exception):
                    session.remove_listener("Page.screencastFrame", self._on_frame)
                if self._ack_tasks:
                    for t in list(self._ack_tasks):
                        t.cancel()
                    self._ack_tasks.clear()

                if not self._frames:
                    with suppress(Exception):
                        shot = await session.send(
                            "Page.captureScreenshot", {"format": "jpeg", "quality": 80}
                        )
                        if isinstance(shot, dict) and isinstance(shot.get("data"), str):
                            self._frames.append(
                                _RecordedFrame(
                                    jpeg=base64.b64decode(shot["data"]),
                                    timestamp=None,
                                    width=self._size[0] if self._size else None,
                                    height=self._size[1] if self._size else None,
                                )
                            )
                with suppress(Exception):
                    await session.detach()
                self._session = None

            frames = list(self._frames)
            self._frames.clear()
            out_path = await asyncio.to_thread(_mux_frames, self._dir, self._id, self._size, frames)
            if not self._done_future.done():
                self._done_future.set_result(out_path)
            return out_path
        except BaseException as exc:
            if not self._done_future.done():
                self._done_future.set_exception(Error(str(exc)))
            raise
        finally:
            self._frames.clear()
            if self._on_stopped is not None:
                cb = self._on_stopped
                self._on_stopped = None
                with suppress(Exception):
                    cb(self)
