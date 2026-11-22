"""Zero-dependency pure-Python PNG decoder/encoder and pixel differ for ``to_have_screenshot``."""

from __future__ import annotations

import os
import struct
import zlib
from collections.abc import Sequence
from dataclasses import dataclass
from os import PathLike
from pathlib import Path

_PNG_SIGNATURE = b"\x89PNG\r\n\x1a\n"

DISABLE_ANIMATIONS_PAGE_JS = """() => {
  if (!document.getElementById('__fw_disable_animations')) {
    const style = document.createElement('style');
    style.id = '__fw_disable_animations';
    style.textContent = '*, *::before, *::after { animation-duration: 0s !important; animation-delay: 0s !important; animation-iteration-count: 1 !important; animation-play-state: paused !important; transition-duration: 0s !important; transition-delay: 0s !important; scroll-behavior: auto !important; caret-color: transparent !important; }';
    (document.head || document.documentElement).appendChild(style);
  }
}"""

RESTORE_ANIMATIONS_PAGE_JS = """() => {
  const el = document.getElementById('__fw_disable_animations');
  if (el) {
    el.remove();
    return true;
  }
  return false;
}"""

DISABLE_ANIMATIONS_LOCATOR_JS = """(el) => {
  const doc = (el && el.ownerDocument) || document;
  if (!doc.getElementById('__fw_disable_animations')) {
    const style = doc.createElement('style');
    style.id = '__fw_disable_animations';
    style.textContent = '*, *::before, *::after { animation-duration: 0s !important; animation-delay: 0s !important; animation-iteration-count: 1 !important; animation-play-state: paused !important; transition-duration: 0s !important; transition-delay: 0s !important; scroll-behavior: auto !important; caret-color: transparent !important; }';
    (doc.head || doc.documentElement).appendChild(style);
  }
}"""

RESTORE_ANIMATIONS_LOCATOR_JS = """(el) => {
  const doc = (el && el.ownerDocument) || document;
  const s = doc.getElementById('__fw_disable_animations');
  if (s) s.remove();
}"""


@dataclass
class RgbaImage:
    width: int
    height: int
    data: bytes


@dataclass
class VisualDiff:
    expected_size: tuple[int, int]
    actual_size: tuple[int, int]
    total_pixels: int
    diff_pixels: int
    diff_ratio: float
    diff_png: bytes | None

    def is_within_tolerance(
        self,
        *,
        max_diff_pixels: int | None,
        max_diff_pixel_ratio: float | None,
    ) -> bool:
        if self.expected_size != self.actual_size:
            return False
        if max_diff_pixels is None and max_diff_pixel_ratio is None:
            return self.diff_pixels == 0
        if max_diff_pixels is not None and self.diff_pixels > max_diff_pixels:
            return False
        if max_diff_pixel_ratio is not None and self.diff_ratio > max_diff_pixel_ratio:
            return False
        return True


_VALID_BIT_DEPTHS: dict[int, tuple[int, ...]] = {
    0: (1, 2, 4, 8, 16),
    2: (8, 16),
    3: (1, 2, 4, 8),
    4: (8, 16),
    6: (8, 16),
}


def _unfilter_pass(
    raw: memoryview,
    offset: int,
    width: int,
    height: int,
    bpp: int,
    channels: int,
    bit_depth: int,
) -> tuple[bytearray, int, int]:
    row_bytes = (width * channels * bit_depth + 7) // 8
    needed = height * (1 + row_bytes)
    if len(raw) - offset < needed:
        raise ValueError(
            f"decode_png: truncated image data (need {needed} bytes at offset {offset}, have {len(raw) - offset})"
        )
    recon = bytearray(height * row_bytes)
    for y in range(height):
        flt = raw[offset]
        offset += 1
        row_out = y * row_bytes
        prev_out = (y - 1) * row_bytes
        if flt == 0:
            recon[row_out : row_out + row_bytes] = raw[offset : offset + row_bytes]
            offset += row_bytes
            continue
        if flt == 1:
            for i in range(row_bytes):
                a = recon[row_out + i - bpp] if i >= bpp else 0
                recon[row_out + i] = (raw[offset + i] + a) & 0xFF
        elif flt == 2:
            if y == 0:
                recon[row_out : row_out + row_bytes] = raw[offset : offset + row_bytes]
            else:
                for i in range(row_bytes):
                    recon[row_out + i] = (raw[offset + i] + recon[prev_out + i]) & 0xFF
        elif flt == 3:
            for i in range(row_bytes):
                a = recon[row_out + i - bpp] if i >= bpp else 0
                b = recon[prev_out + i] if y > 0 else 0
                recon[row_out + i] = (raw[offset + i] + ((a + b) >> 1)) & 0xFF
        elif flt == 4:
            for i in range(row_bytes):
                a = recon[row_out + i - bpp] if i >= bpp else 0
                b = recon[prev_out + i] if y > 0 else 0
                c = recon[prev_out + i - bpp] if (y > 0 and i >= bpp) else 0
                p = a + b - c
                pa = abs(p - a)
                pb = abs(p - b)
                pc = abs(p - c)
                pr = a if (pa <= pb and pa <= pc) else (b if pb <= pc else c)
                recon[row_out + i] = (raw[offset + i] + pr) & 0xFF
        else:
            raise ValueError(f"decode_png: unsupported PNG filter type {flt}")
        offset += row_bytes
    return recon, offset, row_bytes


def _read_sample(row: memoryview, pixel_idx: int, sample_idx: int, channels: int, bit_depth: int) -> int:
    idx = pixel_idx * channels + sample_idx
    if bit_depth == 8:
        return row[idx]
    if bit_depth == 16:
        return row[idx * 2]
    spb = 8 // bit_depth
    byte_val = row[idx // spb]
    shift = 8 - bit_depth * ((idx % spb) + 1)
    mask = (1 << bit_depth) - 1
    raw = (byte_val >> shift) & mask
    return round((raw * 255) / mask)


def _write_rgba_pixels(
    recon: bytearray,
    row_bytes: int,
    width: int,
    height: int,
    color_type: int,
    bit_depth: int,
    plte: bytes | None,
    trns: bytes | None,
    out: bytearray,
    x_start: int,
    y_start: int,
    x_step: int,
    y_step: int,
    img_width: int,
) -> None:
    channels = 1 if color_type in (0, 3) else (3 if color_type == 2 else (2 if color_type == 4 else 4))
    mv = memoryview(recon)

    # Fast paths for standard non-interlaced 8-bit RGBA and RGB screenshots.
    if x_start == 0 and y_start == 0 and x_step == 1 and y_step == 1:
        if color_type == 6 and bit_depth == 8:
            out[:] = recon[: width * height * 4]
            return
        if color_type == 2 and bit_depth == 8 and trns is None:
            out[0::4] = recon[0::3]
            out[1::4] = recon[1::3]
            out[2::4] = recon[2::3]
            out[3::4] = b"\xff" * (width * height)
            return

    for py in range(height):
        row = mv[py * row_bytes : (py + 1) * row_bytes]
        dst_y = y_start + py * y_step
        for px in range(width):
            dst_x = x_start + px * x_step
            dst_idx = (dst_y * img_width + dst_x) * 4
            if color_type == 6 and bit_depth == 8:
                s = px * 4
                out[dst_idx : dst_idx + 4] = row[s : s + 4]
            elif color_type == 2 and bit_depth == 8:
                s = px * 3
                r, g, b = row[s], row[s + 1], row[s + 2]
                a = (
                    0
                    if (
                        trns
                        and len(trns) >= 6
                        and r == int.from_bytes(trns[0:2], "big")
                        and g == int.from_bytes(trns[2:4], "big")
                        and b == int.from_bytes(trns[4:6], "big")
                    )
                    else 255
                )
                out[dst_idx] = r
                out[dst_idx + 1] = g
                out[dst_idx + 2] = b
                out[dst_idx + 3] = a
            elif color_type == 3:
                if bit_depth == 8:
                    p_idx = row[px]
                else:
                    spb = 8 // bit_depth
                    byte_val = row[px // spb]
                    shift = 8 - bit_depth * ((px % spb) + 1)
                    p_idx = (byte_val >> shift) & ((1 << bit_depth) - 1)
                p_off = p_idx * 3
                if plte and p_off + 2 < len(plte):
                    out[dst_idx] = plte[p_off]
                    out[dst_idx + 1] = plte[p_off + 1]
                    out[dst_idx + 2] = plte[p_off + 2]
                else:
                    out[dst_idx] = out[dst_idx + 1] = out[dst_idx + 2] = 0
                out[dst_idx + 3] = trns[p_idx] if (trns and p_idx < len(trns)) else 255
            elif color_type == 0:
                g = _read_sample(row, px, 0, channels, bit_depth)
                a = 255
                if trns and len(trns) >= 2:
                    t_val = int.from_bytes(trns[:2], "big")
                    if bit_depth == 16:
                        raw_sample = int.from_bytes(row[px * 2 : px * 2 + 2], "big")
                    elif bit_depth == 8:
                        raw_sample = row[px]
                    else:
                        raw_mask = (1 << bit_depth) - 1
                        spb = 8 // bit_depth
                        raw_sample = (row[px // spb] >> (8 - bit_depth * ((px % spb) + 1))) & raw_mask
                    if raw_sample == t_val:
                        a = 0
                out[dst_idx] = out[dst_idx + 1] = out[dst_idx + 2] = g
                out[dst_idx + 3] = a
            elif color_type == 4:
                g = _read_sample(row, px, 0, channels, bit_depth)
                a = _read_sample(row, px, 1, channels, bit_depth)
                out[dst_idx] = out[dst_idx + 1] = out[dst_idx + 2] = g
                out[dst_idx + 3] = a
            elif color_type == 2:
                r = _read_sample(row, px, 0, channels, bit_depth)
                g = _read_sample(row, px, 1, channels, bit_depth)
                b = _read_sample(row, px, 2, channels, bit_depth)
                a = 255
                if trns and len(trns) >= 6 and bit_depth == 16:
                    if (
                        row[px * 6 : px * 6 + 2] == trns[0:2]
                        and row[px * 6 + 2 : px * 6 + 4] == trns[2:4]
                        and row[px * 6 + 4 : px * 6 + 6] == trns[4:6]
                    ):
                        a = 0
                out[dst_idx] = r
                out[dst_idx + 1] = g
                out[dst_idx + 2] = b
                out[dst_idx + 3] = a
            elif color_type == 6:
                out[dst_idx] = _read_sample(row, px, 0, channels, bit_depth)
                out[dst_idx + 1] = _read_sample(row, px, 1, channels, bit_depth)
                out[dst_idx + 2] = _read_sample(row, px, 2, channels, bit_depth)
                out[dst_idx + 3] = _read_sample(row, px, 3, channels, bit_depth)


def decode_png(data: bytes) -> RgbaImage:
    if not isinstance(data, (bytes, bytearray)) or len(data) < 8 or data[:8] != _PNG_SIGNATURE:
        raise ValueError("decode_png: invalid PNG signature")
    pos = 8
    n = len(data)
    ihdr: tuple[int, int, int, int, int, int, int] | None = None
    plte: bytes | None = None
    trns: bytes | None = None
    idat_parts: list[bytes] = []

    while pos + 12 <= n:
        length = int.from_bytes(data[pos : pos + 4], "big")
        ctype = data[pos + 4 : pos + 8]
        d_start = pos + 8
        d_end = d_start + length
        if d_end + 4 > n:
            raise ValueError(f"decode_png: truncated chunk {ctype!r}")
        chunk = bytes(data[d_start:d_end])
        if ctype == b"IHDR":
            if length != 13:
                raise ValueError("decode_png: invalid IHDR length")
            ihdr = struct.unpack(">IIBBBBB", chunk)
        elif ctype == b"PLTE":
            plte = chunk
        elif ctype == b"tRNS":
            trns = chunk
        elif ctype == b"IDAT":
            idat_parts.append(chunk)
        elif ctype == b"IEND":
            break
        pos = d_end + 4

    if ihdr is None:
        raise ValueError("decode_png: missing IHDR chunk")
    width, height, bit_depth, color_type, compression, flt_method, interlace = ihdr
    if width <= 0 or height <= 0 or width > 0x7FFFFFFF or height > 0x7FFFFFFF:
        raise ValueError(f"decode_png: invalid image dimensions {width}x{height}")
    if compression != 0 or flt_method != 0:
        raise ValueError("decode_png: unsupported compression or filter method")
    valid_depths = _VALID_BIT_DEPTHS.get(color_type)
    if valid_depths is None or bit_depth not in valid_depths:
        raise ValueError(f"decode_png: invalid (color_type={color_type}, bit_depth={bit_depth})")
    if color_type == 3 and (plte is None or len(plte) == 0 or len(plte) % 3 != 0):
        raise ValueError("decode_png: indexed PNG requires a valid PLTE chunk")
    if not idat_parts:
        raise ValueError("decode_png: missing IDAT chunk")

    channels = 1 if color_type in (0, 3) else (3 if color_type == 2 else (2 if color_type == 4 else 4))
    bpp = max(1, (channels * bit_depth + 7) // 8)
    try:
        raw = memoryview(zlib.decompress(b"".join(idat_parts)))
    except zlib.error as exc:
        raise ValueError(f"decode_png: invalid zlib stream: {exc}") from None
    rgba = bytearray(width * height * 4)

    if interlace == 0:
        recon, _, row_bytes = _unfilter_pass(raw, 0, width, height, bpp, channels, bit_depth)
        _write_rgba_pixels(
            recon, row_bytes, width, height, color_type, bit_depth, plte, trns, rgba, 0, 0, 1, 1, width
        )
    elif interlace == 1:
        x_starts = (0, 4, 0, 2, 0, 1, 0)
        y_starts = (0, 0, 4, 0, 2, 0, 1)
        x_steps = (8, 8, 4, 4, 2, 2, 1)
        y_steps = (8, 8, 8, 4, 4, 2, 2)
        offset = 0
        for p in range(7):
            pw = (width - x_starts[p] + x_steps[p] - 1) // x_steps[p]
            ph = (height - y_starts[p] + y_steps[p] - 1) // y_steps[p]
            if pw <= 0 or ph <= 0:
                continue
            recon, offset, row_bytes = _unfilter_pass(raw, offset, pw, ph, bpp, channels, bit_depth)
            _write_rgba_pixels(
                recon,
                row_bytes,
                pw,
                ph,
                color_type,
                bit_depth,
                plte,
                trns,
                rgba,
                x_starts[p],
                y_starts[p],
                x_steps[p],
                y_steps[p],
                width,
            )
    else:
        raise ValueError(f"decode_png: unsupported interlace method {interlace}")

    return RgbaImage(width=width, height=height, data=bytes(rgba))


def _make_chunk(ctype: bytes, data: bytes) -> bytes:
    crc = zlib.crc32(ctype + data) & 0xFFFFFFFF
    return struct.pack(">I", len(data)) + ctype + data + struct.pack(">I", crc)


def encode_png(width: int, height: int, rgba: bytes) -> bytes:
    ihdr = struct.pack(">IIBBBBB", width, height, 8, 6, 0, 0, 0)
    stride = width * 4
    mv = memoryview(rgba)
    raw = b"".join(b"\x00" + bytes(mv[y * stride : (y + 1) * stride]) for y in range(height))
    idat = zlib.compress(raw)
    return _PNG_SIGNATURE + _make_chunk(b"IHDR", ihdr) + _make_chunk(b"IDAT", idat) + _make_chunk(b"IEND", b"")


def _yiq_rgb_delta_sq(dr: float, dg: float, db: float) -> float:
    if dr == 0.0 and dg == 0.0 and db == 0.0:
        return 0.0
    y = dr * 0.29889531 + dg * 0.58662247 + db * 0.11448223
    i = dr * 0.59597799 - dg * 0.27417610 - db * 0.32180189
    q = dr * 0.21147017 - dg * 0.52261711 + db * 0.31114694
    return 0.5053 * y * y + 0.299 * i * i + 0.1957 * q * q


def _yiq_delta_sq(r1: int, g1: int, b1: int, a1: int, r2: int, g2: int, b2: int, a2: int) -> float:
    if a1 == 255 and a2 == 255:
        return _yiq_rgb_delta_sq(float(r1 - r2), float(g1 - g2), float(b1 - b2))
    a1f = a1 / 255.0
    a2f = a2 / 255.0
    pr1, pg1, pb1 = r1 * a1f, g1 * a1f, b1 * a1f
    pr2, pg2, pb2 = r2 * a2f, g2 * a2f, b2 * a2f
    dr_black = pr1 - pr2
    dg_black = pg1 - pg2
    db_black = pb1 - pb2
    d_black = _yiq_rgb_delta_sq(dr_black, dg_black, db_black)
    inv_diff = 255.0 * (a2f - a1f)
    d_white = _yiq_rgb_delta_sq(dr_black + inv_diff, dg_black + inv_diff, db_black + inv_diff)
    return d_black if d_black >= d_white else d_white


def compare_images(
    expected: RgbaImage,
    actual: RgbaImage,
    *,
    threshold: float = 0.2,
    render_diff: bool = False,
) -> VisualDiff:
    max_delta = 35215.0 * threshold * threshold
    out_w = max(expected.width, actual.width)
    out_h = max(expected.height, actual.height)
    total_pixels = out_w * out_h

    if expected.width == actual.width and expected.height == actual.height and expected.data == actual.data:
        return VisualDiff(
            expected_size=(expected.width, expected.height),
            actual_size=(actual.width, actual.height),
            total_pixels=total_pixels,
            diff_pixels=0,
            diff_ratio=0.0,
            diff_png=None,
        )

    diff_pixels = 0
    diff_rgba = bytearray(out_w * out_h * 4) if render_diff else None
    e_mv = memoryview(expected.data)
    a_mv = memoryview(actual.data)
    same_dims = expected.width == actual.width and expected.height == actual.height

    for y in range(out_h):
        if same_dims:
            row_start = y * out_w * 4
            row_end = row_start + out_w * 4
            if e_mv[row_start:row_end] == a_mv[row_start:row_end]:
                if diff_rgba is not None:
                    for off in range(row_start, row_end, 4):
                        gray = 0.299 * a_mv[off] + 0.587 * a_mv[off + 1] + 0.114 * a_mv[off + 2]
                        faded = max(0, min(255, round(255.0 + (gray - 255.0) * 0.25)))
                        diff_rgba[off] = diff_rgba[off + 1] = diff_rgba[off + 2] = faded
                        diff_rgba[off + 3] = 255
                continue

        in_exp_y = y < expected.height
        in_act_y = y < actual.height
        for x in range(out_w):
            out_idx = (y * out_w + x) * 4
            if not (in_exp_y and in_act_y and x < expected.width and x < actual.width):
                diff_pixels += 1
                if diff_rgba is not None:
                    diff_rgba[out_idx] = 255
                    diff_rgba[out_idx + 1] = 0
                    diff_rgba[out_idx + 2] = 255
                    diff_rgba[out_idx + 3] = 255
                continue
            e_idx = (y * expected.width + x) * 4
            a_idx = (y * actual.width + x) * 4
            r1, g1, b1, a1 = e_mv[e_idx], e_mv[e_idx + 1], e_mv[e_idx + 2], e_mv[e_idx + 3]
            r2, g2, b2, a2 = a_mv[a_idx], a_mv[a_idx + 1], a_mv[a_idx + 2], a_mv[a_idx + 3]
            if r1 == r2 and g1 == g2 and b1 == b2 and a1 == a2:
                if diff_rgba is not None:
                    gray = 0.299 * r2 + 0.587 * g2 + 0.114 * b2
                    faded = max(0, min(255, round(255.0 + (gray - 255.0) * 0.25)))
                    diff_rgba[out_idx] = diff_rgba[out_idx + 1] = diff_rgba[out_idx + 2] = faded
                    diff_rgba[out_idx + 3] = 255
                continue
            if _yiq_delta_sq(r1, g1, b1, a1, r2, g2, b2, a2) > max_delta:
                diff_pixels += 1
                if diff_rgba is not None:
                    diff_rgba[out_idx] = 255
                    diff_rgba[out_idx + 1] = 0
                    diff_rgba[out_idx + 2] = 255
                    diff_rgba[out_idx + 3] = 255
            elif diff_rgba is not None:
                gray = 0.299 * r2 + 0.587 * g2 + 0.114 * b2
                faded = max(0, min(255, round(255.0 + (gray - 255.0) * 0.25)))
                diff_rgba[out_idx] = diff_rgba[out_idx + 1] = diff_rgba[out_idx + 2] = faded
                diff_rgba[out_idx + 3] = 255

    diff_ratio = 0.0 if total_pixels == 0 else (diff_pixels / total_pixels)
    diff_png = (
        encode_png(out_w, out_h, bytes(diff_rgba))
        if (diff_rgba is not None and (diff_pixels > 0 or not same_dims))
        else None
    )
    return VisualDiff(
        expected_size=(expected.width, expected.height),
        actual_size=(actual.width, actual.height),
        total_pixels=total_pixels,
        diff_pixels=diff_pixels,
        diff_ratio=diff_ratio,
        diff_png=diff_png,
    )


def _has_parent_traversal(raw: str) -> bool:
    return any(part == ".." for part in raw.replace("\\", "/").split("/"))


def artifact_paths(name: str | PathLike[str] | Sequence[str]) -> tuple[Path, Path, Path]:
    if isinstance(name, (bytes, bytearray)):
        raise TypeError("to_have_screenshot: path must be a str, PathLike, or Sequence[str]")
    if isinstance(name, (str, PathLike)):
        s = os.fspath(name)
        if not isinstance(s, str) or not s or not s.strip():
            raise ValueError("to_have_screenshot: path must be non-empty")
        if s.endswith(("/", "\\")) or s == "." or s.replace("\\", "/").endswith("/."):
            raise ValueError("to_have_screenshot: path must include a filename")
        if _has_parent_traversal(s):
            raise ValueError("to_have_screenshot: path must not contain '..' segments")
        baseline = Path(s)
    elif isinstance(name, Sequence):
        parts: list[str] = []
        for seg in name:
            if not isinstance(seg, (str, PathLike)):
                raise TypeError("to_have_screenshot: path segments must be str or PathLike")
            seg_s = os.fspath(seg)
            if not isinstance(seg_s, str) or not seg_s or not seg_s.strip():
                raise ValueError("to_have_screenshot: path segments must be non-empty")
            if _has_parent_traversal(seg_s):
                raise ValueError("to_have_screenshot: path must not contain '..' segments")
            parts.append(seg_s)
        if not parts:
            raise ValueError("to_have_screenshot: path segments must be non-empty")
        if parts[-1].endswith(("/", "\\")) or parts[-1] == "." or parts[-1].replace("\\", "/").endswith("/."):
            raise ValueError("to_have_screenshot: path must include a filename")
        baseline = Path(*parts)
    else:
        raise TypeError("to_have_screenshot: path must be a str, PathLike, or Sequence[str]")

    if ".." in baseline.parts or not baseline.name or baseline.name in (".", "..") or not baseline.name.strip():
        raise ValueError(f"to_have_screenshot: invalid filename in path {baseline!r}")

    stem = baseline.stem or "screenshot"
    return (
        baseline,
        baseline.with_name(f"{stem}-actual.png"),
        baseline.with_name(f"{stem}-diff.png"),
    )


def should_update_snapshots(explicit_update: bool | None) -> bool:
    if explicit_update is not None:
        return bool(explicit_update)
    val = os.environ.get("FLUXWRIGHT_UPDATE_SNAPSHOTS", "").strip().lower()
    return val in ("1", "true", "yes", "all")
