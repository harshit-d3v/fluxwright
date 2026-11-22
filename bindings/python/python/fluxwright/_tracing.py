from __future__ import annotations

import asyncio
import base64
from collections.abc import Awaitable, Callable, Mapping
from contextlib import suppress
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import sys
import time
import traceback
from typing import Any
import weakref
import zipfile

try:
    from . import _native
except ImportError:  # pragma: no cover
    _native = None  # type: ignore[assignment]
from ._errors import Error

_SNAPSHOT_TIMEOUT = 0.5

_SNAPSHOT_JS = r"""(() => {
  try {
    const t0 = performance.now();
    const doc = document;
    const root = doc && doc.documentElement;
    if (!root) return null;
    const MAX_NODES = 4000;
    const MAX_TEXT = 16384;
    let nodeCount = 0;
    let textRemaining = MAX_TEXT;
    const ser = (n) => {
      if (!n || nodeCount >= MAX_NODES) return null;
      const nt = n.nodeType;
      if (nt === 3) {
        const v = n.nodeValue;
        if (!v || textRemaining <= 0) return null;
        nodeCount++;
        if (v.length > textRemaining) {
          const s = v.slice(0, textRemaining);
          textRemaining = 0;
          return s;
        }
        textRemaining -= v.length;
        return v;
      }
      if (nt !== 1) return null;
      nodeCount++;
      const tag = n.tagName || '';
      const upper = tag.toUpperCase();
      if (upper === 'SCRIPT') return ['SCRIPT', {}];
      const attrs = {};
      const rawAttrs = n.attributes;
      if (rawAttrs) {
        for (let i = 0; i < rawAttrs.length; i++) {
          const a = rawAttrs[i];
          if (a.name === '__proto__') continue;
          attrs[a.name] = a.value;
        }
      }
      if (upper === 'INPUT') {
        const t = (n.type || '').toLowerCase();
        if (t === 'checkbox' || t === 'radio') {
          attrs.__playwright_checked__ = Boolean(n.checked);
        } else if (t !== 'file') {
          attrs.__playwright_value__ = n.value != null ? String(n.value) : '';
        }
      } else if (upper === 'TEXTAREA' || upper === 'SELECT') {
        attrs.__playwright_value__ = n.value != null ? String(n.value) : '';
      } else if (upper === 'OPTION') {
        attrs.__playwright_selected__ = Boolean(n.selected);
      }
      const out = [tag, attrs];
      const ch = n.childNodes;
      if (ch) {
        for (let i = 0; i < ch.length; i++) {
          if (nodeCount >= MAX_NODES) break;
          const s = ser(ch[i]);
          if (s !== null) out.push(s);
        }
      }
      return out;
    };
    const html = ser(root);
    if (!html) return null;
    return {
      frameUrl: String(doc.URL || (doc.location && doc.location.href) || ''),
      doctype: doc.doctype ? String(doc.doctype.name || '') : '',
      html,
      viewport: {
        width: Number(window.innerWidth) || 0,
        height: Number(window.innerHeight) || 0
      },
      collectionTime: Math.max(0, performance.now() - t0)
    };
  } catch (_) {
    return null;
  }
})()"""


def _to_camel_method(name: str) -> str:
    if name == "wait_for_url":
        return "waitForURL"
    if "_" not in name:
        return name
    parts = name.split("_")
    return parts[0] + "".join(p.capitalize() for p in parts[1:] if p)


def _page_key(page: Any) -> str:
    target_id_fn = getattr(page, "_target_id", None)
    if callable(target_id_fn):
        tid = target_id_fn()
        if isinstance(tid, str) and tid:
            return tid
    return f"obj:{id(page)}"


def _request_key(page_id: str, req: Any) -> tuple[str, str]:
    native = getattr(req, "_native", None)
    req_id = getattr(native, "id", None) if native is not None else getattr(req, "id", None)
    if isinstance(req_id, str) and req_id:
        return (page_id, req_id)
    return (page_id, f"obj:{id(req)}")


def _write_trace_zip(
    dest_path: str,
    trace_body: str,
    net_body: str,
    resources: dict[str, bytes],
) -> None:
    out = Path(dest_path)
    out.parent.mkdir(parents=True, exist_ok=True)
    with zipfile.ZipFile(out, "w", compression=zipfile.ZIP_STORED) as zf:
        zf.writestr("trace.trace", trace_body.encode("utf-8"))
        zf.writestr("trace.network", net_body.encode("utf-8"))
        for res_key in sorted(resources.keys()):
            zf.writestr(res_key, resources[res_key])


def _jpeg_dimensions(data: bytes) -> tuple[int, int] | None:
    if len(data) < 4 or data[0] != 0xFF or data[1] != 0xD8:
        return None
    i = 2
    n = len(data)
    while i + 4 <= n:
        if data[i] != 0xFF:
            i += 1
            continue
        while i < n and data[i] == 0xFF:
            i += 1
        if i >= n:
            break
        marker = data[i]
        i += 1
        if marker in (0xD8, 0xD9, 0x00, 0x01) or (0xD0 <= marker <= 0xD7):
            continue
        if i + 2 > n:
            break
        seg_len = (data[i] << 8) | data[i + 1]
        if seg_len < 2 or i + seg_len > n:
            break
        if marker in (0xC0, 0xC1, 0xC2, 0xC3) and seg_len >= 7:
            height = (data[i + 3] << 8) | data[i + 4]
            width = (data[i + 5] << 8) | data[i + 6]
            if width > 0 and height > 0:
                return (width, height)
        i += seg_len
    return None


def _header_ci(headers: Mapping[str, Any] | None, target: str) -> str | None:
    if not headers:
        return None
    lower = target.lower()
    for k, v in headers.items():
        if str(k).lower() == lower:
            return str(v)
    return None


def _check_opt_str(label: str, name: str, value: Any) -> str | None:
    if value is None:
        return None
    if not isinstance(value, str):
        raise TypeError(f"{label}: {name} must be a str")
    return value


def _check_opt_bool(label: str, name: str, value: Any) -> bool | None:
    if value is None:
        return None
    if not isinstance(value, bool):
        raise TypeError(f"{label}: {name} must be a bool")
    return value


def _check_opt_path(label: str, value: Any) -> str | None:
    if value is None:
        return None
    if not isinstance(value, (str, os.PathLike)):
        raise TypeError(f"{label}: path must be a str or os.PathLike")
    fs_path = os.fspath(value)
    if not isinstance(fs_path, str) or not fs_path:
        raise TypeError(f"{label}: path must be a non-empty string")
    return fs_path


def _build_resource_snapshot(entry: dict[str, Any], end_mono_ms: float) -> dict[str, Any]:
    start_ms = entry["start_mono_ms"]
    resp_ms = entry["response_mono_ms"]
    total_ms = max(0.0, end_mono_ms - start_ms)
    wait_ms = max(0.0, resp_ms - start_ms) if resp_ms is not None else total_ms
    receive_ms = max(0.0, total_ms - wait_ms)

    req_headers_map = entry["req_headers"] or {}
    resp_headers_map = entry["resp_headers"] or {}
    req_headers = [{"name": str(k), "value": str(v)} for k, v in req_headers_map.items()]
    resp_headers = [{"name": str(k), "value": str(v)} for k, v in resp_headers_map.items()]
    post_data: str | None = entry["post_data"]

    req_obj: dict[str, Any] = {
        "method": entry["method"],
        "url": entry["url"],
        "httpVersion": "HTTP/1.1",
        "cookies": [],
        "headers": req_headers,
        "queryString": [],
        "headersSize": -1,
        "bodySize": len(post_data.encode("utf-8")) if post_data is not None else 0,
    }
    if post_data is not None:
        req_obj["postData"] = {
            "mimeType": _header_ci(req_headers_map, "content-type") or "application/octet-stream",
            "text": post_data,
        }

    resp_obj: dict[str, Any] = {
        "status": entry["status"],
        "statusText": entry["status_text"],
        "httpVersion": "HTTP/1.1",
        "cookies": [],
        "headers": resp_headers,
        "content": {"size": 0, "mimeType": entry["mime_type"]},
        "redirectURL": _header_ci(resp_headers_map, "location") or "",
        "headersSize": -1,
        "bodySize": -1,
    }
    if entry["failed_text"] is not None:
        resp_obj["_failureText"] = entry["failed_text"]

    return {
        "type": "resource-snapshot",
        "snapshot": {
            "pageref": entry["page_id"],
            "startedDateTime": entry["started_date_time"],
            "time": total_ms,
            "_monotonicTime": start_ms,
            "request": req_obj,
            "response": resp_obj,
            "cache": {},
            "timings": {"send": 0, "wait": wait_ms, "receive": receive_ms},
        },
    }


def _iso_utc_now() -> str:
    return datetime.now(timezone.utc).isoformat().replace("+00:00", "Z")


class AsyncTracing:
    """Records Playwright Trace Viewer-compatible ``.zip`` bundles for a ``BrowserContext``."""

    def __init__(self, context: Any) -> None:
        self._context_ref = weakref.ref(context)
        self._lock: asyncio.Lock | None = None
        self._active = False
        self._chunk_active = False
        self._session_gen = 0
        self._chunk_seq = 0
        self._screenshots = False
        self._snapshots = False
        self._sources = False
        self._name: str | None = None
        self._title: str | None = None
        self._origin_perf = 0.0
        self._origin_wall = 0
        self._next_call_seq = 0
        self._next_page_seq = 0
        self._page_ids: dict[str, str] = {}
        self._trace_lines: list[str] = []
        self._network_lines: list[str] = []
        self._resources: dict[str, bytes] = {}
        self._chunk_frames = 0
        self._in_flight_net: dict[tuple[str, str], dict[str, Any]] = {}
        self._watchers: dict[str, dict[str, Any]] = {}
        self._pending_page_tasks: set[asyncio.Task[Any]] = set()

    def _get_lock(self) -> asyncio.Lock:
        if self._lock is None:
            self._lock = asyncio.Lock()
        return self._lock

    def _mono_ms(self) -> float:
        return (time.perf_counter() - self._origin_perf) * 1000.0

    def _ensure_page_id(self, page: Any) -> str:
        key = _page_key(page)
        page_id = self._page_ids.get(key)
        if page_id is None:
            self._next_page_seq += 1
            page_id = f"page@{self._next_page_seq}"
            self._page_ids[key] = page_id
        return page_id

    def _push_context_options(self) -> None:
        entry: dict[str, Any] = {
            "type": "context-options",
            "version": 6,
            "origin": "library",
            "browserName": "chromium",
            "platform": sys.platform,
            "wallTime": self._origin_wall,
            "monotonicTime": 0,
            "sdkLanguage": "python",
            "options": {
                "screenshots": self._screenshots,
                "snapshots": self._snapshots,
                "sources": self._sources,
            },
        }
        if self._title is not None:
            entry["title"] = self._title
        if self._name is not None:
            entry["contextId"] = self._name
        self._trace_lines.append(json.dumps(entry, separators=(",", ":")))

    def _reset_chunk(self, name: str | None, title: str | None) -> None:
        self._chunk_seq += 1
        self._name = name
        self._title = title
        self._trace_lines = []
        self._network_lines = []
        self._resources = {}
        self._in_flight_net.clear()
        self._chunk_frames = 0
        self._chunk_active = True
        self._push_context_options()
        ctx = self._context_ref()
        now_ms = self._mono_ms()
        if ctx is not None:
            for page in ctx.pages:
                page_id = self._ensure_page_id(page)
                self._trace_lines.append(
                    json.dumps(
                        {
                            "type": "event",
                            "time": now_ms,
                            "class": "BrowserContext",
                            "method": "page",
                            "params": {"pageId": page_id},
                            "pageId": page_id,
                        },
                        separators=(",", ":"),
                    )
                )

    def _snapshot_network_lines(self) -> list[str]:
        end_ms = self._mono_ms()
        lines = list(self._network_lines)
        if self._chunk_active:
            for item in self._in_flight_net.values():
                snap = _build_resource_snapshot(item, end_ms)
                lines.append(json.dumps(snap, separators=(",", ":")))
        return lines

    def _record_jpeg_frame(
        self,
        page_id: str,
        data: bytes,
        width: int | None = None,
        height: int | None = None,
        frame_swap_wall_time: float | None = None,
    ) -> None:
        if not self._active or not self._chunk_active or not data:
            return
        dims = _jpeg_dimensions(data)
        w = width if width and width > 0 else (dims[0] if dims else 1280)
        h = height if height and height > 0 else (dims[1] if dims else 720)
        digest = hashlib.sha1(data).hexdigest()
        sha1_name = f"{digest}.jpeg"
        res_key = f"resources/{sha1_name}"
        if res_key not in self._resources:
            self._resources[res_key] = data
        self._chunk_frames += 1
        self._trace_lines.append(
            json.dumps(
                {
                    "type": "screencast-frame",
                    "pageId": page_id,
                    "sha1": sha1_name,
                    "width": w,
                    "height": h,
                    "timestamp": self._mono_ms(),
                    "frameSwapWallTime": (
                        frame_swap_wall_time if frame_swap_wall_time is not None else int(time.time() * 1000)
                    ),
                },
                separators=(",", ":"),
            )
        )

    async def _attach_page(self, page: Any) -> None:
        page_key = _page_key(page)
        if not self._active or page.is_closed() or page_key in self._watchers:
            return
        ctx = self._context_ref()
        if ctx is None:
            return
        gen = self._session_gen
        page_id = self._ensure_page_id(page)
        self_ref = weakref.ref(self)
        page_ref = weakref.ref(page)

        def on_console(msg: Any) -> None:
            t = self_ref()
            if t is None or not t._active or not t._chunk_active or t._session_gen != gen:
                return
            t._trace_lines.append(
                json.dumps(
                    {
                        "type": "console",
                        "messageType": getattr(msg, "type", "log") or "log",
                        "text": getattr(msg, "text", "") or "",
                        "args": [],
                        "location": {"url": "", "lineNumber": 0, "columnNumber": 0},
                        "pageId": page_id,
                        "time": t._mono_ms(),
                    },
                    separators=(",", ":"),
                )
            )

        def on_pageerror(err: Any) -> None:
            t = self_ref()
            if t is None or not t._active or not t._chunk_active or t._session_gen != gen:
                return
            t._trace_lines.append(
                json.dumps(
                    {
                        "type": "event",
                        "time": t._mono_ms(),
                        "class": "BrowserContext",
                        "method": "pageError",
                        "params": {
                            "error": {
                                "error": {
                                    "name": getattr(err, "name", None) or "Error",
                                    "message": getattr(err, "message", None) or str(err),
                                    "stack": getattr(err, "stack", "") or "",
                                }
                            }
                        },
                        "pageId": page_id,
                    },
                    separators=(",", ":"),
                )
            )

        def on_request(req: Any) -> None:
            t = self_ref()
            if t is None or not t._active or not t._chunk_active or t._session_gen != gen:
                return
            t._in_flight_net[_request_key(page_id, req)] = {
                "page_id": page_id,
                "started_date_time": _iso_utc_now(),
                "start_mono_ms": t._mono_ms(),
                "method": req.method,
                "url": req.url,
                "req_headers": dict(req.headers or {}),
                "post_data": req.post_data,
                "response_mono_ms": None,
                "status": 0,
                "status_text": "",
                "resp_headers": {},
                "mime_type": "",
                "failed_text": None,
            }

        def on_response(res: Any) -> None:
            t = self_ref()
            if t is None or not t._active or not t._chunk_active or t._session_gen != gen:
                return
            req = res.request
            req_key = _request_key(page_id, req)
            now_ms = t._mono_ms()
            item = t._in_flight_net.get(req_key)
            if item is None:
                item = {
                    "page_id": page_id,
                    "started_date_time": _iso_utc_now(),
                    "start_mono_ms": now_ms,
                    "method": req.method,
                    "url": req.url,
                    "req_headers": dict(req.headers or {}),
                    "post_data": req.post_data,
                    "response_mono_ms": None,
                    "status": 0,
                    "status_text": "",
                    "resp_headers": {},
                    "mime_type": "",
                    "failed_text": None,
                }
                t._in_flight_net[req_key] = item
            resp_headers = dict(res.headers or {})
            ct = _header_ci(resp_headers, "content-type") or ""
            item["response_mono_ms"] = now_ms
            item["status"] = res.status
            item["status_text"] = res.status_text
            item["resp_headers"] = resp_headers
            item["mime_type"] = ct.split(";")[0].strip()

        def on_finished(req: Any) -> None:
            t = self_ref()
            if t is None or not t._active or not t._chunk_active or t._session_gen != gen:
                return
            now_ms = t._mono_ms()
            item = t._in_flight_net.pop(_request_key(page_id, req), None)
            if item is None:
                item = {
                    "page_id": page_id,
                    "started_date_time": _iso_utc_now(),
                    "start_mono_ms": now_ms,
                    "method": req.method,
                    "url": req.url,
                    "req_headers": dict(req.headers or {}),
                    "post_data": req.post_data,
                    "response_mono_ms": None,
                    "status": 0,
                    "status_text": "",
                    "resp_headers": {},
                    "mime_type": "",
                    "failed_text": None,
                }
            t._network_lines.append(
                json.dumps(_build_resource_snapshot(item, now_ms), separators=(",", ":"))
            )

        def on_failed(req: Any) -> None:
            t = self_ref()
            if t is None or not t._active or not t._chunk_active or t._session_gen != gen:
                return
            now_ms = t._mono_ms()
            item = t._in_flight_net.pop(_request_key(page_id, req), None)
            if item is None:
                item = {
                    "page_id": page_id,
                    "started_date_time": _iso_utc_now(),
                    "start_mono_ms": now_ms,
                    "method": req.method,
                    "url": req.url,
                    "req_headers": dict(req.headers or {}),
                    "post_data": req.post_data,
                    "response_mono_ms": None,
                    "status": 0,
                    "status_text": "",
                    "resp_headers": {},
                    "mime_type": "",
                    "failed_text": None,
                }
            item["failed_text"] = req.failure or "failed"
            t._network_lines.append(
                json.dumps(_build_resource_snapshot(item, now_ms), separators=(",", ":"))
            )

        page.on("console", on_console)
        page.on("pageerror", on_pageerror)
        page.on("request", on_request)
        page.on("response", on_response)
        page.on("requestfinished", on_finished)
        page.on("requestfailed", on_failed)

        def cleanup() -> None:
            p = page_ref()
            if p is None:
                return
            p.remove_listener("console", on_console)
            p.remove_listener("pageerror", on_pageerror)
            p.remove_listener("request", on_request)
            p.remove_listener("response", on_response)
            p.remove_listener("requestfinished", on_finished)
            p.remove_listener("requestfailed", on_failed)

        watcher: dict[str, Any] = {
            "page_ref": page_ref,
            "page_id": page_id,
            "cdp": None,
            "cdp_ready": None,
            "screencast_started": False,
            "cleanup": cleanup,
            "cleanup_cdp": None,
        }
        self._watchers[page_key] = watcher

        def _is_stale() -> bool:
            return (
                not self._active
                or self._session_gen != gen
                or page.is_closed()
                or self._watchers.get(page_key) is not watcher
            )

        if (self._screenshots or self._snapshots) and not page.is_closed():
            cdp: Any = None
            cleanup_cdp: Callable[[], None] | None = None
            screencast_started = False

            async def _attach_cdp() -> None:
                nonlocal cdp, cleanup_cdp, screencast_started
                try:
                    cdp = await ctx.new_cdp_session(page)
                    if _is_stale():
                        with suppress(Exception):
                            await cdp.detach()
                        return

                    if self._screenshots:

                        async def on_frame(ev: dict[str, Any]) -> None:
                            t = self_ref()
                            data_b64 = ev.get("data") if isinstance(ev, dict) else None
                            if (
                                t is not None
                                and t._session_gen == gen
                                and isinstance(data_b64, str)
                                and data_b64
                            ):
                                with suppress(Exception):
                                    raw = base64.b64decode(data_b64)
                                    meta = ev.get("metadata") or {}
                                    ts = meta.get("timestamp")
                                    swap_wall = (
                                        int(ts * 1000)
                                        if isinstance(ts, (int, float))
                                        else int(time.time() * 1000)
                                    )
                                    t._record_jpeg_frame(
                                        page_id,
                                        raw,
                                        meta.get("deviceWidth"),
                                        meta.get("deviceHeight"),
                                        swap_wall,
                                    )
                            session_id = ev.get("sessionId") if isinstance(ev, dict) else None
                            if session_id is not None:
                                with suppress(Exception):
                                    await cdp.send(
                                        "Page.screencastFrameAck", {"sessionId": session_id}
                                    )

                        cdp.on("Page.screencastFrame", on_frame)
                        cleanup_cdp = lambda: cdp.remove_listener("Page.screencastFrame", on_frame)  # noqa: E731
                        with suppress(Exception):
                            await cdp.send("Page.enable")
                        if _is_stale():
                            cleanup_cdp()
                            with suppress(Exception):
                                await cdp.detach()
                            return
                        await cdp.send(
                            "Page.startScreencast",
                            {
                                "format": "jpeg",
                                "quality": 80,
                                "maxWidth": 1280,
                                "maxHeight": 720,
                                "everyNthFrame": 1,
                            },
                        )
                        screencast_started = True
                        if _is_stale():
                            cleanup_cdp()
                            with suppress(Exception):
                                await cdp.send("Page.stopScreencast")
                            with suppress(Exception):
                                await cdp.detach()
                            return

                    watcher["cdp"] = cdp
                    watcher["screencast_started"] = screencast_started
                    watcher["cleanup_cdp"] = cleanup_cdp
                except Exception:
                    if cleanup_cdp is not None:
                        with suppress(Exception):
                            cleanup_cdp()
                    if cdp is not None:
                        if screencast_started:
                            with suppress(Exception):
                                await cdp.send("Page.stopScreencast")
                        with suppress(Exception):
                            await cdp.detach()
                finally:
                    watcher["cdp_ready"] = None

            attach_task = asyncio.ensure_future(_attach_cdp())
            watcher["cdp_ready"] = attach_task
            await attach_task

    async def _detach_page(self, page_key: str) -> None:
        watcher = self._watchers.pop(page_key, None)
        if watcher is None:
            return
        watcher["cleanup"]()
        if watcher["cleanup_cdp"] is not None:
            watcher["cleanup_cdp"]()
        cdp = watcher["cdp"]
        if cdp is not None:
            if watcher.get("screencast_started"):
                with suppress(Exception):
                    await cdp.send("Page.stopScreencast")
            with suppress(Exception):
                await cdp.detach()

    def _is_active(self) -> bool:
        return self._active

    def _track_page_created(self, page: Any) -> asyncio.Task[Any]:
        task = asyncio.ensure_future(self._on_page_created(page))
        self._pending_page_tasks.add(task)
        task.add_done_callback(self._pending_page_tasks.discard)
        return task

    async def _drain_pending_page_tasks(self) -> None:
        while self._pending_page_tasks:
            pending = [t for t in self._pending_page_tasks if not t.done()]
            if not pending:
                self._pending_page_tasks.clear()
                break
            await asyncio.gather(*pending, return_exceptions=True)

    async def _on_page_created(self, page: Any) -> None:
        task = asyncio.current_task()
        added = False
        if task is not None and task not in self._pending_page_tasks:
            self._pending_page_tasks.add(task)
            added = True
        try:
            if not self._active:
                return
            page_id = self._ensure_page_id(page)
            if self._chunk_active:
                self._trace_lines.append(
                    json.dumps(
                        {
                            "type": "event",
                            "time": self._mono_ms(),
                            "class": "BrowserContext",
                            "method": "page",
                            "params": {"pageId": page_id},
                            "pageId": page_id,
                        },
                        separators=(",", ":"),
                    )
                )
            await self._attach_page(page)
        finally:
            if added and task is not None:
                self._pending_page_tasks.discard(task)

    async def _on_page_closing(self, page: Any) -> None:
        if not self._active:
            return
        page_key = _page_key(page)
        if self._chunk_active:
            page_id = self._ensure_page_id(page)
            self._trace_lines.append(
                json.dumps(
                    {
                        "type": "event",
                        "time": self._mono_ms(),
                        "class": "Page",
                        "method": "close",
                        "params": {},
                        "pageId": page_id,
                    },
                    separators=(",", ":"),
                )
            )
        await self._detach_page(page_key)
        self._page_ids.pop(page_key, None)

    async def _dispose(self) -> None:
        if not self._active and not self._watchers and not self._pending_page_tasks:
            return
        self._session_gen += 1
        self._active = False
        self._chunk_active = False
        await self._drain_pending_page_tasks()
        for key in list(self._watchers.keys()):
            await self._detach_page(key)
        self._trace_lines.clear()
        self._network_lines.clear()
        self._resources.clear()
        self._in_flight_net.clear()
        self._page_ids.clear()
        self._chunk_frames = 0

    async def _capture_snapshot(
        self,
        page: Any,
        page_id: str,
        call_id: str,
        phase: str,
        chunk_seq: int,
        session_gen: int,
    ) -> dict[str, Any] | None:
        if (
            not self._active
            or not self._chunk_active
            or not self._snapshots
            or self._chunk_seq != chunk_seq
            or self._session_gen != session_gen
        ):
            return None
        if page is None or page.is_closed():
            return None
        page_key = _page_key(page)
        watcher = self._watchers.get(page_key)
        if watcher is None:
            return None
        if watcher.get("cdp") is None and watcher.get("cdp_ready") is not None:
            with suppress(Exception):
                await asyncio.wait_for(asyncio.shield(watcher["cdp_ready"]), timeout=_SNAPSHOT_TIMEOUT)
            watcher = self._watchers.get(page_key)
            if watcher is None:
                return None
        cdp = watcher.get("cdp")
        if cdp is None:
            return None
        if (
            not self._active
            or not self._chunk_active
            or self._chunk_seq != chunk_seq
            or self._session_gen != session_gen
        ):
            return None
        try:
            res = await asyncio.wait_for(
                cdp.send(
                    "Runtime.evaluate",
                    {"expression": _SNAPSHOT_JS, "returnByValue": True},
                ),
                timeout=_SNAPSHOT_TIMEOUT,
            )
        except Exception:
            return None
        if not isinstance(res, dict) or res.get("exceptionDetails"):
            return None
        val = (res.get("result") or {}).get("value")
        if not isinstance(val, dict) or not isinstance(val.get("html"), list):
            return None
        if (
            not self._active
            or not self._chunk_active
            or self._chunk_seq != chunk_seq
            or self._session_gen != session_gen
        ):
            return None
        vp = val.get("viewport")
        if isinstance(vp, dict):
            w = vp.get("width")
            h = vp.get("height")
            viewport = {
                "width": w if isinstance(w, (int, float)) else 0,
                "height": h if isinstance(h, (int, float)) else 0,
            }
        else:
            viewport = {"width": 0, "height": 0}
        coll = val.get("collectionTime")
        collection_time = float(coll) if isinstance(coll, (int, float)) and coll >= 0 else 0.0
        frame_url = val.get("frameUrl")
        doctype = val.get("doctype")
        return {
            "callId": call_id,
            "snapshotName": f"{phase}@{call_id}",
            "pageId": page_id,
            "frameId": "main",
            "frameUrl": frame_url if isinstance(frame_url, str) else "",
            "doctype": doctype if isinstance(doctype, str) else "",
            "html": val["html"],
            "viewport": viewport,
            "timestamp": self._mono_ms(),
            "collectionTime": collection_time,
            "resourceOverrides": [],
            "isMainFrame": True,
        }

    def _append_after(
        self,
        call_id: str,
        chunk_seq: int,
        session_gen: int,
        error_info: dict[str, str] | None,
        after_snapshot_name: str | None,
    ) -> None:
        if (
            self._active
            and self._chunk_active
            and self._chunk_seq == chunk_seq
            and self._session_gen == session_gen
        ):
            after: dict[str, Any] = {
                "type": "after",
                "callId": call_id,
                "endTime": self._mono_ms(),
            }
            if after_snapshot_name is not None:
                after["afterSnapshot"] = after_snapshot_name
            if error_info is not None:
                after["error"] = error_info
            self._trace_lines.append(json.dumps(after, separators=(",", ":")))

    async def _maybe_capture_after_snapshot(
        self,
        page: Any,
        page_id: str,
        call_id: str,
        chunk_seq: int,
        session_gen: int,
    ) -> str | None:
        if (
            not self._snapshots
            or not self._active
            or not self._chunk_active
            or self._chunk_seq != chunk_seq
            or self._session_gen != session_gen
        ):
            return None
        snap = await self._capture_snapshot(page, page_id, call_id, "after", chunk_seq, session_gen)
        if (
            snap is not None
            and self._active
            and self._chunk_active
            and self._chunk_seq == chunk_seq
            and self._session_gen == session_gen
        ):
            self._trace_lines.append(
                json.dumps({"type": "frame-snapshot", "snapshot": snap}, separators=(",", ":"))
            )
            return str(snap["snapshotName"])
        return None

    async def _run_action(
        self,
        page: Any,
        class_name: str,
        method_name: str,
        api_name: str,
        params: dict[str, Any] | None,
        fn: Callable[[], Awaitable[Any]],
    ) -> Any:
        if not self._active or not self._chunk_active:
            return await fn()
        self._next_call_seq += 1
        call_id = f"call@{self._next_call_seq}"
        chunk_seq = self._chunk_seq
        session_gen = self._session_gen
        camel_method = _to_camel_method(method_name)
        page_id = self._ensure_page_id(page)

        before_snapshot_name: str | None = None
        if self._snapshots:
            snap = await self._capture_snapshot(page, page_id, call_id, "before", chunk_seq, session_gen)
            if (
                not self._active
                or not self._chunk_active
                or self._chunk_seq != chunk_seq
                or self._session_gen != session_gen
            ):
                return await fn()
            if snap is not None:
                before_snapshot_name = snap["snapshotName"]
                self._trace_lines.append(
                    json.dumps({"type": "frame-snapshot", "snapshot": snap}, separators=(",", ":"))
                )

        start_time = self._mono_ms()
        before_ev: dict[str, Any] = {
            "type": "before",
            "callId": call_id,
            "startTime": start_time,
            "class": class_name,
            "method": camel_method,
            "apiName": api_name,
            "params": params or {},
            "pageId": page_id,
            "stepId": call_id,
        }
        if before_snapshot_name is not None:
            before_ev["beforeSnapshot"] = before_snapshot_name
        self._trace_lines.append(json.dumps(before_ev, separators=(",", ":")))

        after_snapshot_name: str | None = None
        try:
            result = await fn()
        except BaseException as exc:
            cancelled = isinstance(exc, asyncio.CancelledError)
            error_info = {
                "name": getattr(exc, "name", None) or type(exc).__name__,
                "message": str(exc),
                "stack": "".join(traceback.format_exception(type(exc), exc, exc.__traceback__)),
            }
            if not cancelled:
                with suppress(asyncio.CancelledError):
                    after_snapshot_name = await self._maybe_capture_after_snapshot(
                        page, page_id, call_id, chunk_seq, session_gen
                    )
            self._append_after(call_id, chunk_seq, session_gen, error_info, after_snapshot_name)
            raise

        try:
            after_snapshot_name = await self._maybe_capture_after_snapshot(
                page, page_id, call_id, chunk_seq, session_gen
            )
        except asyncio.CancelledError:
            self._append_after(call_id, chunk_seq, session_gen, None, None)
            raise
        self._append_after(call_id, chunk_seq, session_gen, None, after_snapshot_name)
        return result

    async def _finalize_chunk(self, dest_path: str | None) -> None:
        await self._drain_pending_page_tasks()
        ctx = self._context_ref()
        open_pages = [p for p in (ctx.pages if ctx is not None else []) if not p.is_closed()]
        if dest_path is not None and self._screenshots and self._chunk_frames == 0 and open_pages:
            deadline = time.perf_counter() + 0.25
            while time.perf_counter() < deadline and self._chunk_frames == 0:
                await asyncio.sleep(0.02)
            if self._chunk_frames == 0:
                for page in open_pages:
                    if page.is_closed():
                        continue
                    watcher = self._watchers.get(_page_key(page))
                    cdp = watcher.get("cdp") if watcher is not None else None
                    page_id = self._ensure_page_id(page)
                    raw: bytes | None = None
                    if cdp is not None:
                        with suppress(Exception):
                            res = await cdp.send("Page.captureScreenshot", {"format": "jpeg", "quality": 80})
                            data_b64 = res.get("data") if isinstance(res, dict) else None
                            if isinstance(data_b64, str) and data_b64:
                                raw = base64.b64decode(data_b64)
                    if not raw:
                        with suppress(Exception):
                            if _native is not None and isinstance(page, _native.Page):
                                raw = await _native.Page.screenshot(page, type="jpeg", quality=80)
                            else:
                                raw = await page.screenshot(type="jpeg", quality=80)
                    if raw:
                        self._record_jpeg_frame(page_id, raw)

        await asyncio.sleep(0.02)

        if dest_path is not None:
            net_lines = self._snapshot_network_lines()
            trace_body = ("\n".join(self._trace_lines) + "\n") if self._trace_lines else ""
            net_body = ("\n".join(net_lines) + "\n") if net_lines else ""
            resources_copy = dict(self._resources)
            await asyncio.to_thread(_write_trace_zip, dest_path, trace_body, net_body, resources_copy)

        self._chunk_active = False
        self._trace_lines.clear()
        self._network_lines.clear()
        self._resources.clear()
        self._in_flight_net.clear()
        self._chunk_frames = 0

    async def start(
        self,
        *,
        name: str | None = None,
        title: str | None = None,
        screenshots: bool | None = None,
        snapshots: bool | None = None,
        sources: bool | None = None,
    ) -> None:
        """Begins recording a Playwright-compatible trace for every page in this context."""
        opt_name = _check_opt_str("tracing.start", "name", name)
        opt_title = _check_opt_str("tracing.start", "title", title)
        opt_screenshots = _check_opt_bool("tracing.start", "screenshots", screenshots)
        opt_snapshots = _check_opt_bool("tracing.start", "snapshots", snapshots)
        opt_sources = _check_opt_bool("tracing.start", "sources", sources)
        async with self._get_lock():
            ctx = self._context_ref()
            if ctx is None or ctx._is_closed():
                raise Error("tracing.start: browser context is closed")
            if self._active:
                raise Error("Tracing has been already started")
            self._session_gen += 1
            self._active = True
            self._screenshots = bool(opt_screenshots)
            self._snapshots = bool(opt_snapshots)
            self._sources = bool(opt_sources)
            self._origin_perf = time.perf_counter()
            self._origin_wall = int(time.time() * 1000)
            self._reset_chunk(opt_name, opt_title)
            for page in ctx.pages:
                await self._attach_page(page)

    async def start_chunk(self, *, name: str | None = None, title: str | None = None) -> None:
        """Starts a new trace chunk after an earlier ``stop_chunk()``."""
        opt_name = _check_opt_str("tracing.start_chunk", "name", name)
        opt_title = _check_opt_str("tracing.start_chunk", "title", title)
        async with self._get_lock():
            ctx = self._context_ref()
            if ctx is None or ctx._is_closed():
                raise Error("tracing.start_chunk: browser context is closed")
            if not self._active:
                raise Error("Must start tracing before starting a new chunk")
            self._reset_chunk(opt_name, opt_title)

    async def stop_chunk(self, *, path: str | os.PathLike[str] | None = None) -> None:
        """Stops the active trace chunk and optionally writes it to ``path``."""
        dest_path = _check_opt_path("tracing.stop_chunk", path)
        async with self._get_lock():
            if not self._active:
                raise Error("Must start tracing before stopping")
            if not self._chunk_active:
                raise Error("Must start a chunk before stopping")
            await self._finalize_chunk(dest_path)

    async def stop(self, *, path: str | os.PathLike[str] | None = None) -> None:
        """Stops tracing and optionally saves the active trace chunk to ``path``."""
        dest_path = _check_opt_path("tracing.stop", path)
        async with self._get_lock():
            if not self._active:
                if dest_path is not None:
                    raise Error("Tracing has not been started")
                return
            try:
                if dest_path is not None and not self._chunk_active:
                    raise Error("No active trace chunk; call start_chunk() before stop(path=...)")
                if self._chunk_active:
                    await self._finalize_chunk(dest_path)
            finally:
                await self._dispose()


class SyncTracing:
    """Synchronous wrapper around ``AsyncTracing`` exposed as ``context.tracing``."""

    def __init__(self, impl: AsyncTracing, run: Callable[..., Any]) -> None:
        self._impl = impl
        self._run = run

    def start(
        self,
        *,
        name: str | None = None,
        title: str | None = None,
        screenshots: bool | None = None,
        snapshots: bool | None = None,
        sources: bool | None = None,
    ) -> None:
        self._run(
            self._impl.start,
            name=name,
            title=title,
            screenshots=screenshots,
            snapshots=snapshots,
            sources=sources,
        )

    def start_chunk(self, *, name: str | None = None, title: str | None = None) -> None:
        self._run(self._impl.start_chunk, name=name, title=title)

    def stop_chunk(self, *, path: str | os.PathLike[str] | None = None) -> None:
        self._run(self._impl.stop_chunk, path=path)

    def stop(self, *, path: str | os.PathLike[str] | None = None) -> None:
        self._run(self._impl.stop, path=path)
