"""The asyncio API, shaped like Playwright's ``playwright.async_api``::

    import asyncio
    from fluxwright.async_api import async_playwright

    async def main():
        async with async_playwright() as p:
            browser = await p.chromium.launch(max_browsers=4)
            page = await browser.new_page()
            await page.goto("https://example.com")
            print(await page.get_by_role("heading").text_content())
            await browser.close()

    asyncio.run(main())

Each ``new_page`` is a fresh browser context on a pooled Chrome. Close pages when a job is done.
"""

from __future__ import annotations

import asyncio
import base64
import inspect
import json as _json
import logging
import math
import os
import re
import time
import weakref
from contextlib import asynccontextmanager, suppress
from os import PathLike
from pathlib import Path
from typing import (
    Any,
    Generic,
    Literal,
    TypeVar,
)
from urllib.parse import parse_qsl, urlencode, urljoin, urlparse
from collections.abc import AsyncIterator, Awaitable, Callable, Mapping, Sequence

from . import _native
from ._errors import Error, TimeoutError
from ._expect import LocatorAssertions, PageAssertions, expect
from ._native import ConsoleMessage, Download, FrameLocator, Locator
from ._types import (
    FilePayload,
    FloatRect,
    Geolocation,
    HttpCredentials,
    PdfMargins,
    Position,
    ProxySettings,
    StorageState,
    URLMatch,
    ViewportSize,
)

__all__ = [
    "APIRequestContext",
    "APIResponse",
    "Browser",
    "BrowserContext",
    "BrowserType",
    "CDPSession",
    "ConsoleMessage",
    "Dialog",
    "Download",
    "Error",
    "EventInfo",
    "FileChooser",
    "FilePayload",
    "FloatRect",
    "Frame",
    "FrameLocator",
    "Geolocation",
    "HttpCredentials",
    "Keyboard",
    "Locator",
    "LocatorAssertions",
    "Mouse",
    "Page",
    "PageAssertions",
    "PdfMargins",
    "Playwright",
    "PlaywrightContextManager",
    "Position",
    "ProxySettings",
    "Request",
    "Response",
    "Route",
    "StorageState",
    "TimeoutError",
    "Touchscreen",
    "ViewportSize",
    "WebSocket",
    "Worker",
    "async_playwright",
    "chromium",
    "expect",
]

_log = logging.getLogger("fluxwright")
T = TypeVar("T")


def _url_matcher(pattern: URLMatch) -> Callable[[str], bool]:
    if callable(pattern):
        return lambda url: bool(pattern(url))
    if isinstance(pattern, re.Pattern):
        return lambda url: pattern.search(url) is not None
    regex, i, in_group = "", 0, False
    while i < len(pattern):
        c = pattern[i]
        if c == "*":
            deep = pattern[i + 1 : i + 2] == "*"
            regex += ".*" if deep else "[^/]*"
            i += 2 if deep else 1
            continue
        if c == "{":
            in_group, regex = True, regex + "(?:"
        elif c == "}" and in_group:
            in_group, regex = False, regex + ")"
        elif c == "," and in_group:
            regex += "|"
        else:
            regex += re.escape(c)
        i += 1
    compiled = re.compile(regex)
    return lambda url: compiled.fullmatch(url) is not None


def _takes_request(handler: Callable[..., Any]) -> bool:
    """Whether a route handler takes ``(route, request)`` rather than ``(route)``, as Playwright
    decides it."""
    try:
        params = inspect.signature(handler).parameters.values()
    except (TypeError, ValueError):
        return True
    positional = [p for p in params if p.kind in (p.POSITIONAL_ONLY, p.POSITIONAL_OR_KEYWORD)]
    return len(positional) >= 2 or any(p.kind == p.VAR_POSITIONAL for p in params)


def _settled(result: Any = None, error: BaseException | None = None) -> asyncio.Future[Any]:
    future = asyncio.get_running_loop().create_future()
    if error is None:
        future.set_result(result)
    else:
        future.set_exception(error)
    return future


class Request:
    """A request, as route handlers and network events see it."""

    def __init__(self, native: _native.Route | _native.NetworkRequest, failure: str | None = None) -> None:
        self._native = native
        self._failure = failure

    @property
    def url(self) -> str:
        return self._native.url

    @property
    def method(self) -> str:
        return self._native.method

    @property
    def headers(self) -> dict[str, str]:
        return self._native.headers

    @property
    def post_data(self) -> str | None:
        return self._native.post_data

    @property
    def resource_type(self) -> str:
        """``document``, ``script``, ``stylesheet``, ``image``, ``xhr``, ..."""
        return self._native.resource_type

    @property
    def failure(self) -> str | None:
        """Why the request failed (``net::ERR_FAILED``, ...), for ``requestfailed``."""
        return self._failure

    def __repr__(self) -> str:
        return f"<Request method={self.method!r} url={self.url!r}>"


class Response:
    """A response, as ``page.on("response")`` and ``expect_response`` hand it over."""

    def __init__(self, native: _native.NetworkResponse, request: Request | None = None) -> None:
        self._native = native
        self._request = request or Request(native.request)

    @property
    def url(self) -> str:
        return self._native.url

    @property
    def status(self) -> int:
        return self._native.status

    @property
    def status_text(self) -> str:
        return self._native.status_text

    @property
    def ok(self) -> bool:
        """Status 200 to 299."""
        return 200 <= self.status <= 299

    @property
    def headers(self) -> dict[str, str]:
        """Names lower-cased."""
        return self._native.headers

    @property
    def request(self) -> Request:
        return self._request

    async def body(self) -> bytes:
        """The body, once the response finished loading."""
        return await self._native.body()

    async def text(self) -> str:
        return (await self.body()).decode("utf-8", errors="replace")

    async def json(self) -> Any:
        return _json.loads(await self.body())

    def __repr__(self) -> str:
        return f"<Response status={self.status} url={self.url!r}>"


class Dialog:
    """A JavaScript dialog, as ``page.on("dialog")`` hands it over. Answer it with ``accept`` or
    ``dismiss``; one no listener answered is dismissed, so the page never hangs on it."""

    def __init__(self, native: _native.Dialog) -> None:
        self._native = native
        self._answered = False

    @property
    def type(self) -> str:
        """``alert``, ``confirm``, ``prompt`` or ``beforeunload``."""
        return self._native.type

    @property
    def message(self) -> str:
        return self._native.message

    @property
    def default_value(self) -> str:
        """A prompt's default text."""
        return self._native.default_value

    async def accept(self, prompt_text: str | None = None) -> None:
        """``prompt_text`` is what a ``prompt`` returns."""
        self._answered = True
        await self._native.accept(prompt_text)

    async def dismiss(self) -> None:
        self._answered = True
        await self._native.dismiss()

    def __repr__(self) -> str:
        return f"<Dialog type={self.type!r} message={self.message!r}>"


class FileChooser:
    """A native file chooser dialog, intercepted via ``page.on('filechooser')`` or
    ``page.expect_file_chooser()``."""

    def __init__(self, native: _native.FileChooser) -> None:
        self._native = native

    def is_multiple(self) -> bool:
        return self._native.is_multiple()

    async def set_files(
        self,
        files: str | PathLike[str] | FilePayload | Sequence[str | PathLike[str] | FilePayload],
        *,
        timeout: float | None = None,
    ) -> None:
        await self._native.set_files(files, timeout=timeout)

    def __repr__(self) -> str:
        return f"<FileChooser is_multiple={self.is_multiple()}>"


class Mouse:
    """Low-level pointer controls in viewport CSS pixels, accessed via ``page.mouse``."""

    def __init__(self, page: Page) -> None:
        self._page = page

    async def move(self, x: float, y: float, *, steps: int | None = None) -> None:
        await self._page._mouse_move(x, y, steps=steps)

    async def down(
        self,
        *,
        button: Literal["left", "right", "middle"] | None = None,
        click_count: int | None = None,
    ) -> None:
        await self._page._mouse_down(button=button, click_count=click_count)

    async def up(
        self,
        *,
        button: Literal["left", "right", "middle"] | None = None,
        click_count: int | None = None,
    ) -> None:
        await self._page._mouse_up(button=button, click_count=click_count)

    async def click(
        self,
        x: float,
        y: float,
        *,
        delay: float | None = None,
        button: Literal["left", "right", "middle"] | None = None,
        click_count: int | None = None,
    ) -> None:
        await self._page._mouse_click(x, y, delay=delay, button=button, click_count=click_count)

    async def dblclick(
        self,
        x: float,
        y: float,
        *,
        delay: float | None = None,
        button: Literal["left", "right", "middle"] | None = None,
    ) -> None:
        await self._page._mouse_dblclick(x, y, delay=delay, button=button)

    async def wheel(self, delta_x: float, delta_y: float) -> None:
        await self._page._mouse_wheel(delta_x, delta_y)


class Keyboard:
    """Low-level keyboard controls, accessed via ``page.keyboard``."""

    def __init__(self, page: Page) -> None:
        self._page = page

    async def down(self, key: str) -> None:
        await self._page._keyboard_down(key)

    async def up(self, key: str) -> None:
        await self._page._keyboard_up(key)

    async def press(self, key: str, *, delay: float | None = None) -> None:
        await self._page._keyboard_press(key, delay=delay)

    async def type(self, text: str, *, delay: float | None = None) -> None:
        await self._page._keyboard_type(text, delay=delay)

    async def insert_text(self, text: str) -> None:
        await self._page._keyboard_insert_text(text)


class Touchscreen:
    """Low-level touchscreen controls in viewport CSS pixels, accessed via ``page.touchscreen``."""

    def __init__(self, page: Page) -> None:
        self._page = page

    async def tap(self, x: float, y: float) -> None:
        await self._page._touchscreen_tap(x, y)


def _save_storage_json_private(path: str | PathLike[str], data: dict[str, Any]) -> None:
    p = Path(path)
    p.parent.mkdir(parents=True, mode=0o700, exist_ok=True)
    flags = os.O_WRONLY | os.O_CREAT | os.O_TRUNC
    fd = os.open(p, flags, 0o600)
    fh = None
    try:
        if hasattr(os, "fchmod"):
            os.fchmod(fd, 0o600)
        fh = os.fdopen(fd, "w", encoding="utf-8")
        _json.dump(data, fh, indent=2)
    finally:
        if fh is not None:
            fh.close()
        else:
            with suppress(OSError):
                os.close(fd)


def _has_header_ci(headers: Mapping[str, str], target: str) -> bool:
    tl = target.lower()
    return any(k.lower() == tl for k in headers)


def _encode_query_params(
    params: Mapping[str, Any] | Sequence[tuple[str, Any]] | str | None,
) -> list[tuple[str, str]] | None:
    if params is None:
        return None
    if isinstance(params, str):
        raw = params[1:] if params.startswith("?") else params
        return [(k, v) for k, v in parse_qsl(raw, keep_blank_values=True)]
    if isinstance(params, Mapping):
        return [(str(k), str(v)) for k, v in params.items()]
    return [(str(k), str(v)) for k, v in params]


def _encode_multipart(multipart: Mapping[str, Any]) -> tuple[str, bytes]:
    def _clean(s: Any) -> str:
        return str(s).replace("\r", "").replace("\n", "").replace('"', "")

    boundary = f"----FluxwrightBoundary{os.urandom(8).hex()}"
    parts: list[bytes] = []
    for name, value in multipart.items():
        parts.append(f"--{boundary}\r\n".encode())
        if isinstance(value, Mapping) and "buffer" in value:
            filename = value.get("name") or "file"
            mime = value.get("mimeType") or "application/octet-stream"
            buf = value["buffer"]
            if isinstance(buf, str):
                buf = buf.encode("utf-8")
            elif not isinstance(buf, (bytes, bytearray)):
                buf = bytes(buf)
            parts.append(
                f'Content-Disposition: form-data; name="{_clean(name)}"; filename="{_clean(filename)}"\r\n'
                f"Content-Type: {_clean(mime)}\r\n\r\n".encode()
            )
            parts.append(bytes(buf))
            parts.append(b"\r\n")
        elif isinstance(value, (bytes, bytearray)):
            parts.append(
                f'Content-Disposition: form-data; name="{_clean(name)}"; filename="blob"\r\n'
                f"Content-Type: application/octet-stream\r\n\r\n".encode()
            )
            parts.append(bytes(value))
            parts.append(b"\r\n")
        else:
            parts.append(f'Content-Disposition: form-data; name="{_clean(name)}"\r\n\r\n'.encode())
            parts.append(str(value).encode("utf-8"))
            parts.append(b"\r\n")
    parts.append(f"--{boundary}--\r\n".encode())
    return f"multipart/form-data; boundary={boundary}", b"".join(parts)


def _cookie_field_matches(flt: str | re.Pattern[str], value: str, *, is_domain: bool = False) -> bool:
    if isinstance(flt, re.Pattern):
        return flt.search(value) is not None
    if not is_domain:
        return value == flt
    f = flt.lower()
    v = value.lower()
    return v == f if f.startswith(".") else (v == f or v == f".{f}")


class APIResponse:
    """HTTP response returned by ``APIRequestContext`` and ``route.fetch()``."""

    def __init__(self, native: _native.ApiResponse) -> None:
        self._native = native
        self._headers_cache: dict[str, str] | None = None

    @property
    def url(self) -> str:
        return self._native.url

    @property
    def status(self) -> int:
        return self._native.status

    @property
    def status_text(self) -> str:
        return self._native.status_text

    @property
    def ok(self) -> bool:
        return 200 <= self._native.status <= 299

    @property
    def headers(self) -> dict[str, str]:
        if self._headers_cache is None:
            merged: dict[str, str] = {}
            for k, v in self._native.headers:
                kl = k.lower()
                if kl in merged:
                    merged[kl] += ("\n" if kl == "set-cookie" else ", ") + v
                else:
                    merged[kl] = v
            self._headers_cache = merged
        return dict(self._headers_cache)

    def headers_array(self) -> list[dict[str, str]]:
        return [{"name": k, "value": v} for k, v in self._native.headers]

    async def body(self) -> bytes:
        return self._native.body

    async def text(self) -> str:
        return (await self.body()).decode("utf-8", errors="replace")

    async def json(self) -> Any:
        return _json.loads(await self.text())

    async def dispose(self) -> None:
        self._native.dispose()


class APIRequestContext:
    """HTTP client sharing cookies and defaults with a ``BrowserContext`` or ``Page``."""

    def __init__(
        self,
        sender: Callable[..., Awaitable[_native.ApiResponse]],
        cookies_fn: Callable[[], Awaitable[list[dict[str, Any]]]],
        is_closed_fn: Callable[[], bool],
    ) -> None:
        self._sender = sender
        self._cookies_fn = cookies_fn
        self._is_closed_fn = is_closed_fn
        self._disposed = False

    async def fetch(
        self,
        url: str,
        *,
        params: Mapping[str, Any] | Sequence[tuple[str, Any]] | str | None = None,
        method: str | None = None,
        headers: Mapping[str, str] | None = None,
        data: Any = None,
        form: Mapping[str, Any] | None = None,
        multipart: Mapping[str, Any] | None = None,
        timeout: float | None = None,
        fail_on_status_code: bool = False,
        max_redirects: int | None = None,
    ) -> APIResponse:
        if self._disposed or self._is_closed_fn():
            raise Error("api_request_context.fetch: target is closed or disposed")
        modes = sum(x is not None for x in (data, form, multipart))
        if modes > 1:
            raise TypeError("api_request_context.fetch: pass at most one of data, form, or multipart")
        sent_headers = {str(k): str(v) for k, v in (headers or {}).items()}
        body: str | bytes | None = None
        if data is not None:
            if isinstance(data, (bytes, bytearray)):
                body = bytes(data)
                if not _has_header_ci(sent_headers, "content-type"):
                    sent_headers["content-type"] = "application/octet-stream"
            elif isinstance(data, str):
                body = data
                if not _has_header_ci(sent_headers, "content-type"):
                    sent_headers["content-type"] = "application/octet-stream"
            else:
                body = _json.dumps(data)
                if not _has_header_ci(sent_headers, "content-type"):
                    sent_headers["content-type"] = "application/json"
        elif form is not None:
            body = urlencode({str(k): str(v) for k, v in form.items()})
            if not _has_header_ci(sent_headers, "content-type"):
                sent_headers["content-type"] = "application/x-www-form-urlencoded"
        elif multipart is not None:
            ctype, body = _encode_multipart(multipart)
            if not _has_header_ci(sent_headers, "content-type"):
                sent_headers["content-type"] = ctype

        native = await self._sender(
            str(url),
            method=method,
            headers=sent_headers or None,
            params=_encode_query_params(params),
            data=body,
            fail_on_status_code=bool(fail_on_status_code),
            max_redirects=max_redirects,
            timeout=timeout,
        )
        return APIResponse(native)

    async def get(self, url: str, **kwargs: Any) -> APIResponse:
        return await self.fetch(url, method="GET", **kwargs)

    async def post(self, url: str, **kwargs: Any) -> APIResponse:
        return await self.fetch(url, method="POST", **kwargs)

    async def put(self, url: str, **kwargs: Any) -> APIResponse:
        return await self.fetch(url, method="PUT", **kwargs)

    async def patch(self, url: str, **kwargs: Any) -> APIResponse:
        return await self.fetch(url, method="PATCH", **kwargs)

    async def delete(self, url: str, **kwargs: Any) -> APIResponse:
        return await self.fetch(url, method="DELETE", **kwargs)

    async def head(self, url: str, **kwargs: Any) -> APIResponse:
        return await self.fetch(url, method="HEAD", **kwargs)

    async def storage_state(self, *, path: str | PathLike[str] | None = None) -> StorageState:
        if self._disposed or self._is_closed_fn():
            raise Error("api_request_context.storage_state: target is closed or disposed")
        state: StorageState = {"cookies": await self._cookies_fn(), "origins": []}
        if path is not None:
            _save_storage_json_private(path, state)
        return state

    async def dispose(self) -> None:
        self._disposed = True


class Route:
    """How a route handler answers a request, as Playwright's ``Route``. Each request takes one
    answer; the methods return awaitables."""

    def __init__(self, native: _native.Route, request: Request) -> None:
        self._native = native
        self._request = request
        self._answer: asyncio.Future[None] | None = None
        self._fell_back = False
        # Set once the handler returned: the request went on, and a later answer is too late.
        self._returned = False

    @property
    def request(self) -> Request:
        return self._request

    async def fetch(
        self,
        *,
        url: str | None = None,
        method: str | None = None,
        headers: dict[str, str] | None = None,
        post_data: Any = None,
        max_redirects: int | None = None,
        timeout: float | None = None,
    ) -> APIResponse:
        if self._returned:
            raise Error("route: handler already returned")
        if self._answer is not None or self._fell_back:
            raise Error("route is already handled")
        encoded_post: str | bytes | None = None
        if post_data is not None:
            if isinstance(post_data, (bytes, bytearray)):
                encoded_post = bytes(post_data)
            elif isinstance(post_data, str):
                encoded_post = post_data
            else:
                encoded_post = _json.dumps(post_data)
        raw = await self._native.fetch(
            url=url,
            method=method,
            headers=headers,
            post_data=encoded_post,
            max_redirects=max_redirects,
            timeout=timeout,
        )
        return APIResponse(raw)

    def fulfill(
        self,
        *,
        status: int | None = None,
        headers: dict[str, str] | None = None,
        body: str | bytes | None = None,
        json: Any = None,
        path: str | PathLike[str] | None = None,
        content_type: str | None = None,
        response: APIResponse | None = None,
    ) -> Awaitable[None]:
        """A made-up response. ``json`` is serialized and sets the content type; ``path`` is a
        file to send."""

        async def answer() -> None:
            eff_status = status
            sent: dict[str, str] = {}
            data = body
            if response is not None:
                if eff_status is None:
                    eff_status = response.status
                skip = {"content-encoding", "content-length", "transfer-encoding", "connection"}
                for h in response.headers_array():
                    kl = h["name"].lower()
                    if kl in skip:
                        continue
                    if kl in sent:
                        sent[kl] += ("\n" if kl == "set-cookie" else ", ") + h["value"]
                    else:
                        sent[kl] = h["value"]
                if headers:
                    for k, v in headers.items():
                        for ek in [ek for ek in sent if ek.lower() == k.lower()]:
                            del sent[ek]
                        sent[k] = v
                if json is not None:
                    data = _json.dumps(json)
                    if not _has_header_ci(sent, "content-type"):
                        sent["content-type"] = "application/json"
                elif path is not None:
                    data = Path(path).read_bytes()
                elif data is None:
                    data = await response.body()
            else:
                sent = dict(headers or {})
                if json is not None:
                    data = _json.dumps(json)
                    sent.setdefault("content-type", "application/json")
                if path is not None:
                    data = Path(path).read_bytes()
            if content_type is not None:
                sent["content-type"] = content_type
            await self._native.fulfill(status=eff_status, headers=sent, body=data)

        return self._answer_with(answer)

    def continue_(
        self,
        *,
        url: str | None = None,
        method: str | None = None,
        headers: dict[str, str] | None = None,
        post_data: str | bytes | None = None,
    ) -> Awaitable[None]:
        """Sends the request on, changed if asked; ``headers`` replaces all of them."""
        return self._answer_with(
            lambda: self._native.continue_(url=url, method=method, headers=headers, post_data=post_data)
        )

    def abort(self, error_code: str | None = None) -> Awaitable[None]:
        """Fails the request with one of Playwright's codes: ``failed`` (the default),
        ``aborted``, ``blockedbyclient``, ``timedout``, ..."""
        return self._answer_with(lambda: self._native.abort(error_code))

    def fallback(self) -> Awaitable[None]:
        """Leaves the request to the next matching handler: the one added before this one."""
        if self._returned:
            return _late("route")
        if self._answer is not None:
            return _settled(error=Error("route is already handled"))
        self._fell_back = True
        return _settled()

    def _answer_with(self, start: Callable[[], Awaitable[None]]) -> Awaitable[None]:
        # The first answer wins. It starts now rather than when awaited, so an answer still under
        # way when the handler returns is waited for, not taken as no answer.
        if self._returned:
            return _late("route")
        if self._answer is not None or self._fell_back:
            return _settled(error=Error("route is already handled"))

        async def run() -> None:
            await start()

        self._answer = asyncio.ensure_future(run())
        return self._answer


def _late(what: str) -> asyncio.Future[None]:
    """An answer after its handler returned: the request already went on. It resolves, with a
    warning, rather than raising where nothing awaits it."""
    _log.warning(
        "a %s was answered after its handler returned, and the answer is ignored: "
        "await it inside the handler",
        what,
    )
    return _settled()


class _RouteEntry:
    __slots__ = ("url", "handler", "matches", "with_request")

    def __init__(self, url: URLMatch, handler: Callable[..., Any]) -> None:
        self.url = url
        self.handler = handler
        self.matches = _url_matcher(url)
        self.with_request = _takes_request(handler)


class EventInfo(Generic[T]):
    """What ``expect_download`` yields: ``await info.value`` is the download."""

    def __init__(self, future: asyncio.Future[T]) -> None:
        self._future = future

    @property
    def value(self) -> Awaitable[T]:
        return self._future

    def is_done(self) -> bool:
        return self._future.done()


@asynccontextmanager
async def _expect_emitter_event(
    target: Any,
    label: str,
    is_closed_fn: Callable[[], bool],
    expects: set[asyncio.Future[Any]],
    allowed: tuple[str, ...],
    event: str,
    predicate: Callable[[Any], bool] | None,
    timeout: float | None,
) -> AsyncIterator[EventInfo[Any]]:
    if event not in allowed:
        raise ValueError(f"{label}.expect_event: {event!r} is not supported ({', '.join(allowed)})")
    if is_closed_fn():
        raise Error(f"{label}.expect_event: target closed")
    future: asyncio.Future[Any] = asyncio.get_running_loop().create_future()
    expects.add(future)
    pred = predicate if predicate is not None else (lambda _: True)

    def listener(arg: Any) -> None:
        if future.done():
            return
        try:
            if pred(arg):
                future.set_result(arg)
        except Exception as e:
            future.set_exception(e)

    target.on(event, listener)
    try:
        yield EventInfo(future)
        limit = 30_000 if timeout is None else timeout
        try:
            await asyncio.wait_for(asyncio.shield(future), limit / 1000 if limit > 0 else None)
        except asyncio.TimeoutError:
            raise TimeoutError(f"{label}.expect_event({event!r}): timeout {limit:g}ms exceeded") from None
    finally:
        expects.discard(future)
        target.remove_listener(event, listener)


_WS_EVENTS = ("framesent", "framereceived", "socketerror", "close")
_WORKER_EVENTS = ("close", "console")


class WebSocket:
    """A WebSocket opened by a page, emitted via ``page.on('websocket')``."""

    def __init__(self, page: Page, url: str) -> None:
        self._page = weakref.ref(page)
        self._url = url
        self._closed = False
        self._listeners: dict[str, list[tuple[Listener, Listener]]] = {}
        self._expects: set[asyncio.Future[Any]] = set()
        self._tasks = page._tasks

    @property
    def url(self) -> str:
        return self._url

    def is_closed(self) -> bool:
        return self._closed

    def _emit(self, event: str, arg: Any) -> None:
        _emit(self._listeners, event, arg, self._tasks)

    def _mark_closed(self) -> None:
        if self._closed:
            return
        self._closed = True
        _emit(self._listeners, "close", self, self._tasks)
        for future in list(self._expects):
            if not future.done():
                future.set_exception(Error("websocket closed"))

    def on(self, event: str, handler: Listener) -> None:
        if event not in _WS_EVENTS:
            raise ValueError(f"web_socket.on: {event!r} is not supported ({', '.join(_WS_EVENTS)})")
        self._listeners.setdefault(event, []).append((handler, handler))

    def once(self, event: str, handler: Listener) -> None:
        if event not in _WS_EVENTS:
            raise ValueError(f"web_socket.once: {event!r} is not supported ({', '.join(_WS_EVENTS)})")
        listeners = self._listeners

        def call(arg: Any) -> Any:
            with suppress(ValueError):
                listeners[event].remove((handler, call))
            return handler(arg)

        self._listeners.setdefault(event, []).append((handler, call))

    def remove_listener(self, event: str, handler: Listener) -> None:
        _remove_listener(self._listeners, event, handler)

    off = remove_listener

    @asynccontextmanager
    async def expect_event(
        self,
        event: str,
        predicate: Callable[[Any], bool] | None = None,
        *,
        timeout: float | None = None,
    ) -> AsyncIterator[EventInfo[Any]]:
        async with _expect_emitter_event(
            self,
            "web_socket",
            self.is_closed,
            self._expects,
            _WS_EVENTS,
            event,
            predicate,
            timeout,
        ) as info:
            yield info

    def __repr__(self) -> str:
        return f"<WebSocket url={self._url!r}>"


class Worker:
    """A dedicated Web Worker running on a page."""

    def __init__(self, page: Page, worker_id: str, url: str) -> None:
        self._page = weakref.ref(page)
        self._id = worker_id
        self._url = url
        self._closed = False
        self._listeners: dict[str, list[tuple[Listener, Listener]]] = {}
        self._expects: set[asyncio.Future[Any]] = set()
        self._tasks = page._tasks

    @property
    def url(self) -> str:
        return self._url

    async def evaluate(self, expression: str, arg: Any = None) -> Any:
        p = self._page()
        if self._closed or p is None or p.is_closed():
            raise Error("worker.evaluate: worker is closed")
        return await p._worker_evaluate(self._id, expression, arg)

    def _emit(self, event: str, arg: Any) -> None:
        _emit(self._listeners, event, arg, self._tasks)

    def _mark_closed(self) -> None:
        if self._closed:
            return
        self._closed = True
        _emit(self._listeners, "close", self, self._tasks)
        for future in list(self._expects):
            if not future.done():
                future.set_exception(Error("worker closed"))

    def on(self, event: str, handler: Listener) -> None:
        if event not in _WORKER_EVENTS:
            raise ValueError(f"worker.on: {event!r} is not supported ({', '.join(_WORKER_EVENTS)})")
        if (p := self._page()) is not None and not p.is_closed():
            p._ensure_worker_subscription()
        self._listeners.setdefault(event, []).append((handler, handler))

    def once(self, event: str, handler: Listener) -> None:
        if event not in _WORKER_EVENTS:
            raise ValueError(f"worker.once: {event!r} is not supported ({', '.join(_WORKER_EVENTS)})")
        if (p := self._page()) is not None and not p.is_closed():
            p._ensure_worker_subscription()
        listeners = self._listeners

        def call(arg: Any) -> Any:
            with suppress(ValueError):
                listeners[event].remove((handler, call))
            return handler(arg)

        self._listeners.setdefault(event, []).append((handler, call))

    def remove_listener(self, event: str, handler: Listener) -> None:
        _remove_listener(self._listeners, event, handler)

    off = remove_listener

    @asynccontextmanager
    async def expect_event(
        self,
        event: str,
        predicate: Callable[[Any], bool] | None = None,
        *,
        timeout: float | None = None,
    ) -> AsyncIterator[EventInfo[Any]]:
        async with _expect_emitter_event(
            self,
            "worker",
            lambda: self._closed,
            self._expects,
            _WORKER_EVENTS,
            event,
            predicate,
            timeout,
        ) as info:
            yield info

    def __repr__(self) -> str:
        return f"<Worker url={self._url!r}>"


def _strip_fragment(url: str) -> str:
    idx = url.find("#")
    return url if idx == -1 else url[:idx]


def _header_list_ci(headers: Sequence[Mapping[str, Any]] | None, name: str) -> str | None:
    lower = name.lower()
    for h in headers or ():
        if str(h.get("name", "")).lower() == lower:
            v = h.get("value")
            return str(v) if v is not None else None
    return None


def _create_har_matcher(har_json: Mapping[str, Any]) -> Callable[[Request], dict[str, Any] | None]:
    raw_entries = (har_json.get("log") or {}).get("entries")
    entries: list[Any] = raw_entries if isinstance(raw_entries, list) else []
    served = [False] * len(entries)
    skip = {"connection", "content-encoding", "content-length", "transfer-encoding"}

    def match(request: Request) -> dict[str, Any] | None:
        target_url = _strip_fragment(request.url)
        target_method = request.method.upper()
        req_headers = request.headers
        req_post = request.post_data
        max_score = -1
        candidates: list[int] = []
        for idx, entry in enumerate(entries):
            if not isinstance(entry, Mapping):
                continue
            ereq = entry.get("request")
            if not isinstance(ereq, Mapping):
                continue
            eurl = ereq.get("url")
            if not isinstance(eurl, str) or _strip_fragment(eurl) != target_url:
                continue
            if str(ereq.get("method") or "GET").upper() != target_method:
                continue
            score = 0
            epost = ereq.get("postData")
            if req_post is not None and isinstance(epost, Mapping) and epost.get("text") == req_post:
                score += 100
            for name in ("content-type", "accept"):
                hv = _header_list_ci(ereq.get("headers"), name)
                if hv is not None and req_headers.get(name, "").lower() == hv.lower():
                    score += 10
            if score > max_score:
                max_score = score
                candidates = [idx]
            elif score == max_score:
                candidates.append(idx)
        if not candidates:
            return None
        chosen = next((i for i in candidates if not served[i]), None)
        if chosen is None:
            chosen = candidates[-1]
        else:
            served[chosen] = True
        chosen_entry = entries[chosen]
        eresp = chosen_entry.get("response") if isinstance(chosen_entry, Mapping) else None
        if not isinstance(eresp, Mapping):
            eresp = {}
        headers: dict[str, str] = {}
        for h in eresp.get("headers") or ():
            if not isinstance(h, Mapping):
                continue
            k = str(h.get("name", "")).lower()
            if not k or k in skip:
                continue
            val = "" if h.get("value") is None else str(h["value"])
            if k in headers:
                headers[k] += ("\n" if k == "set-cookie" else ", ") + val
            else:
                headers[k] = val
        body: str | bytes | None = None
        content = eresp.get("content")
        if isinstance(content, Mapping) and content.get("text") is not None:
            text = str(content["text"])
            if str(content.get("encoding") or "").lower() == "base64":
                body = base64.b64decode(text)
            else:
                body = text
        raw_status = eresp.get("status")
        status = int(raw_status) if raw_status else 200
        return {"status": status, "headers": headers, "body": body}

    return match


def _filter_har_file(path: Path, regex: re.Pattern[str]) -> None:
    har = _json.loads(path.read_text(encoding="utf-8"))
    log = har.get("log") if isinstance(har, dict) else None
    if isinstance(log, dict) and isinstance(log.get("entries"), list):
        log["entries"] = [
            e
            for e in log["entries"]
            if isinstance(e, Mapping)
            and isinstance((e.get("request") or {}).get("url"), str)
            and regex.search(e["request"]["url"]) is not None
        ]
        _save_storage_json_private(path, har)


async def _flush_har_filters(filters: list[tuple[Path, re.Pattern[str]]]) -> None:
    pending = list(filters)
    filters.clear()
    for path, regex in pending:
        if path.exists():
            _filter_har_file(path, regex)


def _split_har_url_filter(
    har_path: str | PathLike[str] | None,
    url_filter: str | re.Pattern[str] | None,
) -> tuple[str | None, tuple[Path, re.Pattern[str]] | None]:
    if url_filter is None:
        return None, None
    if isinstance(url_filter, str):
        return url_filter, None
    if isinstance(url_filter, re.Pattern):
        pending = (Path(har_path), url_filter) if har_path is not None else None
        return None, pending
    raise TypeError("record_har_url_filter must be a str or re.Pattern")


async def _apply_route_from_har(
    target: Any,
    label: str,
    start_update_fn: Callable[..., Awaitable[None]],
    pending_filters: list[tuple[Path, re.Pattern[str]]],
    har: str | PathLike[str],
    *,
    url: URLMatch | None = None,
    not_found: Literal["abort", "fallback"] | None = None,
    update: bool | None = None,
    update_mode: Literal["full", "minimal"] | None = None,
    update_content: Literal["embed", "omit"] | None = None,
) -> None:
    if not isinstance(har, (str, PathLike)) or not str(har):
        raise TypeError(f"{label}.route_from_har: har must be a non-empty file path")
    if url is not None and not isinstance(url, (str, re.Pattern)) and not callable(url):
        raise TypeError(f"{label}.route_from_har: url must be a str, re.Pattern, or callable")
    if not_found not in (None, "abort", "fallback"):
        raise ValueError(f"{label}.route_from_har: not_found must be 'abort' or 'fallback', not {not_found!r}")
    if update_mode not in (None, "full", "minimal"):
        raise ValueError(f"{label}.route_from_har: update_mode must be 'full' or 'minimal', not {update_mode!r}")
    if update_content not in (None, "embed", "omit"):
        raise ValueError(f"{label}.route_from_har: update_content must be 'embed' or 'omit', not {update_content!r}")
    har_path = Path(har)
    if update:
        if callable(url):
            raise TypeError(f"{label}.route_from_har: url cannot be a callable when update=True")
        await start_update_fn(
            har_path,
            content=update_content or "embed",
            mode=update_mode or "minimal",
            url_filter=url if isinstance(url, str) else None,
        )
        if isinstance(url, re.Pattern):
            pending_filters.append((har_path, url))
        return
    har_json = _json.loads(har_path.read_text(encoding="utf-8"))
    match = _create_har_matcher(har_json)
    fallback = not_found == "fallback"

    async def handler(route: Route, request: Request) -> None:
        hit = match(request)
        if hit is not None:
            await route.fulfill(status=hit["status"], headers=hit["headers"], body=hit["body"])
            return
        if fallback:
            await route.fallback()
        else:
            await route.abort("failed")

    await target.route("**/*" if url is None else url, handler)


Listener = Callable[[Any], Any]
_UNSET: Any = object()
_pages_by_target: weakref.WeakValueDictionary[str, Page] = weakref.WeakValueDictionary()


class Frame:
    """An imperative handle to a main frame or iframe on a page."""

    def __init__(
        self,
        page: Page,
        frame_id: str,
        parent_id: str | None,
        name: str,
        url: str,
    ) -> None:
        self._page = page
        self._id = frame_id
        self._parent_id = parent_id
        self._name = name
        self._url = url

    def _update(self, info: _native.FrameInfo) -> None:
        self._parent_id = info.parent_id
        self._name = info.name
        self._url = info.url

    def _info(self) -> _native.FrameInfo | None:
        for f in self._page._frames():
            if f.id == self._id:
                self._update(f)
                return f
        return None

    def _root(self) -> FrameLocator:
        return self._page._frame_by_id(self._id)

    @property
    def page(self) -> Page:
        return self._page

    @property
    def url(self) -> str:
        self._info()
        return self._url

    @property
    def name(self) -> str:
        self._info()
        return self._name

    def is_detached(self) -> bool:
        return self._page.is_closed() or self._info() is None

    @property
    def parent_frame(self) -> Frame | None:
        info = self._info()
        pid = info.parent_id if info is not None else self._parent_id
        if pid is None:
            return None
        return self._page._get_frame_by_id(pid)

    @property
    def child_frames(self) -> list[Frame]:
        return [self._page._get_frame(f) for f in self._page._frames() if f.parent_id == self._id]

    async def goto(
        self,
        url: str,
        *,
        wait_until: Literal["load", "domcontentloaded", "networkidle", "commit"] | None = None,
        timeout: float | None = None,
    ) -> None:
        await self._page._frame_goto(self._id, url, wait_until=wait_until, timeout=timeout)

    async def evaluate(self, expression: str, arg: Any = None) -> Any:
        return await self._page._frame_evaluate(self._id, expression, arg)

    async def title(self) -> str:
        return str(await self._page._frame_evaluate(self._id, "document.title"))

    async def content(self) -> str:
        return str(
            await self._page._frame_evaluate(
                self._id,
                "document.documentElement ? document.documentElement.outerHTML : ''",
            )
        )

    def locator(
        self,
        selector: str,
        *,
        has: Locator | None = None,
        has_not: Locator | None = None,
        has_text: str | re.Pattern[str] | None = None,
        has_not_text: str | re.Pattern[str] | None = None,
    ) -> Locator:
        return self._root().locator(
            selector,
            has=has,
            has_not=has_not,
            has_text=has_text,
            has_not_text=has_not_text,
        )

    def get_by_text(self, text: str, *, exact: bool | None = None) -> Locator:
        return self._root().get_by_text(text, exact=exact)

    def get_by_role(
        self,
        role: str,
        *,
        checked: bool | None = None,
        disabled: bool | None = None,
        exact: bool | None = None,
        expanded: bool | None = None,
        include_hidden: bool | None = None,
        level: int | None = None,
        name: str | re.Pattern[str] | None = None,
        pressed: bool | None = None,
        selected: bool | None = None,
    ) -> Locator:
        return self._root().get_by_role(
            role,
            checked=checked,
            disabled=disabled,
            exact=exact,
            expanded=expanded,
            include_hidden=include_hidden,
            level=level,
            name=name,
            pressed=pressed,
            selected=selected,
        )

    def get_by_label(self, text: str, *, exact: bool | None = None) -> Locator:
        return self._root().get_by_label(text, exact=exact)

    def get_by_placeholder(self, text: str, *, exact: bool | None = None) -> Locator:
        return self._root().get_by_placeholder(text, exact=exact)

    def get_by_alt_text(self, text: str, *, exact: bool | None = None) -> Locator:
        return self._root().get_by_alt_text(text, exact=exact)

    def get_by_title(self, text: str, *, exact: bool | None = None) -> Locator:
        return self._root().get_by_title(text, exact=exact)

    def get_by_test_id(self, test_id: str) -> Locator:
        return self._root().get_by_test_id(test_id)

    def frame_locator(self, selector: str) -> FrameLocator:
        return self._root().frame_locator(selector)


class Page(_native.Page):
    """A browser tab in a fresh browser context, leased from the engine.

    Close it when the job is done, in a ``finally``. A page you drop is freed when Python
    collects it, even if its listeners or route handlers refer to it.
    """

    def __init__(self, lease: _native.Lease) -> None:
        self._routes: list[_RouteEntry] = []
        self._routing = False
        self._route_lock = asyncio.Lock()
        self._listeners: dict[str, list[tuple[Listener, Listener]]] = {}
        self._logging = False
        # Ends once no network listener is left; the next one starts another (see _serve_network).
        self._network_task: asyncio.Future[Any] | None = None
        self._answering_dialogs = False
        self._serving_file_choosers = False
        self._serving_popups = False
        self._serving_websockets = False
        self._websockets_live: dict[str, WebSocket] = {}
        self._serving_workers = False
        self._worker_map: dict[str, Worker] = {}
        self._pending_har_filters: list[tuple[Path, re.Pattern[str]]] = []
        self._bindings: dict[str, Callable[..., Any]] = {}
        self._serving_bindings = False
        self._tasks: set[asyncio.Future[Any]] = set()
        self._expects: set[asyncio.Future[Any]] = set()
        self._context_ref: weakref.ref[BrowserContext] | None = None
        self._request: APIRequestContext | None = None
        self._frame_cache: weakref.WeakValueDictionary[str, Frame] = weakref.WeakValueDictionary()
        _pages_by_target[self._target_id()] = self

    @property
    def request(self) -> APIRequestContext:
        ctx = self._context_ref() if self._context_ref is not None else None
        if ctx is not None:
            return ctx.request
        if self._request is None:
            async def _cookies() -> list[dict[str, Any]]:
                st = await self.storage_state()
                return list(st.get("cookies", []))

            self._request = APIRequestContext(self._api_fetch, _cookies, self.is_closed)
        return self._request

    @property
    def mouse(self) -> Mouse:
        return Mouse(self)

    @property
    def keyboard(self) -> Keyboard:
        return Keyboard(self)

    @property
    def touchscreen(self) -> Touchscreen:
        return Touchscreen(self)

    async def wait_for_url(
        self,
        url: URLMatch,
        *,
        timeout: float | None = None,
        wait_until: Literal["load", "domcontentloaded", "networkidle", "commit"] | None = None,
    ) -> None:
        if wait_until is not None and wait_until not in ("load", "domcontentloaded", "networkidle", "commit"):
            raise ValueError(
                f"wait_until must be load, domcontentloaded, networkidle or commit, not {wait_until!r}"
            )
        if timeout is not None and (
            isinstance(timeout, bool)
            or not isinstance(timeout, (int, float))
            or math.isnan(timeout)
            or timeout < 0
        ):
            raise ValueError("timeout must be a non-negative number")
        target: URLMatch = url
        if isinstance(target, str):
            base_url = self._base_url()
            if base_url is not None and not re.match(r"^[a-zA-Z][a-zA-Z0-9+.-]*:", target):
                if target.startswith("/"):
                    parsed = urlparse(base_url)
                    target = f"{parsed.scheme}://{parsed.netloc}{target}"
                elif not target.startswith("*") and "{" not in target:
                    target = urljoin(base_url, target)
        elif not isinstance(target, re.Pattern) and not callable(target):
            raise TypeError("page.wait_for_url: url must be a str, re.Pattern or callable")
        matches = _url_matcher(target)
        state = wait_until or "load"
        nav_timeout = self._navigation_timeout() if timeout is None else float(timeout)
        unlimited = nav_timeout == 0 or nav_timeout >= 365 * 24 * 3600 * 1000
        deadline = None if unlimited else time.monotonic() + (nav_timeout / 1000.0)
        while True:
            current = self.url
            if matches(current):
                if state == "commit":
                    return
                rem = None if deadline is None else max(1.0, (deadline - time.monotonic()) * 1000.0)
                await self.wait_for_load_state(state, timeout=rem)
                return
            if deadline is not None:
                rem = (deadline - time.monotonic()) * 1000.0
                if rem <= 0:
                    raise TimeoutError(f"page.wait_for_url: timeout exceeded (current: {current})")
            else:
                rem = None
            await self._wait_for_url_change(current, timeout=rem)

    async def route(self, url: URLMatch, handler: Callable[..., Any]) -> None:
        """Hands matching requests of this page, its popups and iframes to ``handler(route)``
        or ``handler(route, request)``, as Playwright's ``page.route``. The handler added last
        runs first; an unanswered request continues. Call it before navigating."""
        # A call made while another is turning interception on waits for it, so no call returns
        # before requests are handed over.
        async with self._route_lock:
            if not self._routing:
                stream = await self._intercept()
                ref = weakref.ref(self)
                _keep(self._tasks, asyncio.ensure_future(_serve_routes(stream, lambda: _routes_of(ref), self._tasks)))
                self._routing = True
            self._routes.append(_RouteEntry(url, handler))

    async def unroute(self, url: URLMatch, handler: Callable[..., Any] | None = None) -> None:
        """Removes the handlers added for ``url`` (only ``handler``, when given)."""
        self._routes[:] = [r for r in self._routes if not (r.url == url and (handler is None or r.handler == handler))]

    async def route_from_har(
        self,
        har: str | PathLike[str],
        *,
        url: URLMatch | None = None,
        not_found: Literal["abort", "fallback"] | None = None,
        update: bool | None = None,
        update_mode: Literal["full", "minimal"] | None = None,
        update_content: Literal["embed", "omit"] | None = None,
    ) -> None:
        """Serves matching requests from a HAR 1.2 file, or records into ``har`` when ``update=True``."""
        if self.is_closed():
            raise Error("page.route_from_har: page closed")
        await _apply_route_from_har(
            self,
            "page",
            self._start_har_recording,
            self._pending_har_filters,
            har,
            url=url,
            not_found=not_found,
            update=update,
            update_mode=update_mode,
            update_content=update_content,
        )

    async def add_init_script(
        self, script: str | None = None, *, path: str | PathLike[str] | None = None
    ) -> None:
        """Adds a script evaluated whenever the page navigates or reloads."""
        source = _resolve_init_script("page.add_init_script", script, path)
        await self._add_init_script(source)

    async def expose_function(self, name: str, callback: Callable[..., Any]) -> None:
        """Exposes ``callback`` on ``window.<name>``; calls from the page return a Promise."""
        if not isinstance(name, str) or not name:
            raise TypeError("page.expose_function: name must be a non-empty str")
        if not callable(callback):
            raise TypeError("page.expose_function: callback must be callable")
        if name in self._bindings:
            raise Error(f'Function "{name}" has been already registered')
        if not self._serving_bindings:
            _keep(
                self._tasks,
                asyncio.ensure_future(_serve_bindings(self._binding_calls(), weakref.ref(self), self._tasks)),
            )
            self._serving_bindings = True
        self._bindings[name] = callback
        try:
            await self._expose_binding(name)
        except BaseException:
            if self._bindings.get(name) is callback:
                del self._bindings[name]
            raise

    async def add_script_tag(
        self,
        *,
        url: str | None = None,
        path: str | PathLike[str] | None = None,
        content: str | None = None,
        type: str | None = None,
    ) -> None:
        """Adds a ``<script>`` tag into the page with the given ``url``, ``path`` or ``content``."""
        if (url is not None) + (path is not None) + (content is not None) != 1:
            raise ValueError("page.add_script_tag: provide exactly one of url, path, or content")
        text = Path(path).read_text(encoding="utf-8") if path is not None else content
        await self._add_script_tag(url=url, content=text, script_type=type)

    async def add_style_tag(
        self,
        *,
        url: str | None = None,
        path: str | PathLike[str] | None = None,
        content: str | None = None,
    ) -> None:
        """Adds a ``<style>`` or ``<link rel="stylesheet">`` tag into the page."""
        if (url is not None) + (path is not None) + (content is not None) != 1:
            raise ValueError("page.add_style_tag: provide exactly one of url, path, or content")
        text = Path(path).read_text(encoding="utf-8") if path is not None else content
        await self._add_style_tag(url=url, content=text)

    def on(self, event: str, handler: Listener) -> None:
        """Calls ``handler`` from now on with each ``"console"`` message (a ``ConsoleMessage``),
        ``"pageerror"`` (an ``Error``), ``"request"``, ``"requestfinished"`` and
        ``"requestfailed"`` (a ``Request``), ``"response"`` (a ``Response``) or ``"dialog"`` (a
        ``Dialog``, to accept or dismiss). Calls of a coroutine function run as tasks."""
        self._add_listener("on", event, handler, handler)

    def once(self, event: str, handler: Listener) -> None:
        """``on``, for the next event only."""
        listeners = self._listeners

        def call(arg: Any) -> Any:
            # This registration only: an ``on`` of the same handler stays.
            with suppress(ValueError):
                listeners[event].remove((handler, call))
            return handler(arg)

        self._add_listener("once", event, handler, call)

    def remove_listener(self, event: str, handler: Listener) -> None:
        _remove_listener(self._listeners, event, handler)

    off = remove_listener

    @asynccontextmanager
    async def expect_popup(
        self,
        predicate: Callable[[Page], bool] | None = None,
        *,
        timeout: float | None = None,
    ) -> AsyncIterator[EventInfo[Page]]:
        """Waits for a popup page opened by the code in the ``async with`` block."""
        async with self._expect(
            "popup",
            predicate if predicate is not None else (lambda _: True),
            timeout,
            label="popup",
        ) as info:
            yield info

    async def opener(self) -> Page | None:
        tid = self._opener_target_id()
        if tid is None:
            return None
        p = _pages_by_target.get(tid)
        return p if p is not None and not p.is_closed() else None

    def _get_frame(self, info: _native.FrameInfo) -> Frame:
        f = self._frame_cache.get(info.id)
        if f is None:
            f = Frame(self, info.id, info.parent_id, info.name, info.url)
            self._frame_cache[info.id] = f
        else:
            f._update(info)
        return f

    def _get_frame_by_id(self, frame_id: str) -> Frame | None:
        for f in self._frames():
            if f.id == frame_id:
                return self._get_frame(f)
        return self._frame_cache.get(frame_id)

    @property
    def frames(self) -> list[Frame]:
        return [self._get_frame(f) for f in self._frames()]

    @property
    def main_frame(self) -> Frame:
        mid = self._main_frame_id()
        found = self._get_frame_by_id(mid)
        if found is not None:
            return found
        f = self._frame_cache.get(mid)
        if f is None:
            f = Frame(self, mid, None, "", self.url)
            self._frame_cache[mid] = f
        return f

    def frame(self, name: str | None = None, *, url: URLMatch | None = None) -> Frame | None:
        if (name is not None) == (url is not None):
            raise TypeError("page.frame: provide either name or url, not both")
        if name is not None:
            return next((f for f in self.frames if f.name == name), None)
        assert url is not None
        matches = _url_matcher(url)
        return next((f for f in self.frames if matches(f.url)), None)

    @asynccontextmanager
    async def expect_file_chooser(
        self,
        predicate: Callable[[FileChooser], bool] | None = None,
        *,
        timeout: float | None = None,
    ) -> AsyncIterator[EventInfo[FileChooser]]:
        """Waits for the native file chooser opened by the code in the ``async with`` block."""
        async with self._expect(
            "filechooser",
            predicate if predicate is not None else (lambda _: True),
            timeout,
            label="file_chooser",
        ) as info:
            yield info

    @asynccontextmanager
    async def expect_download(self, timeout: float | None = None) -> AsyncIterator[EventInfo[Download]]:
        """Waits for the download the code in the ``async with`` block starts (``timeout`` in
        milliseconds, default 30 s), as Playwright's ``page.expect_download``::

            async with page.expect_download() as info:
                await page.get_by_text("Export").click()
            download = await info.value
        """
        # Only a download the block starts: drop ones that finished before it.
        await self._clear_downloads()
        future = asyncio.ensure_future(self.wait_for_download(timeout=timeout))
        try:
            yield EventInfo(future)
        except BaseException:
            future.cancel()
            raise
        await future

    @asynccontextmanager
    async def expect_response(
        self, url_or_predicate: URLMatch | Callable[[Response], bool], *, timeout: float | None = None
    ) -> AsyncIterator[EventInfo[Response]]:
        """Waits for the first response from now on whose URL matches (a glob or regex, as in
        ``route``) or that the predicate accepts (``timeout`` in milliseconds, default 30 s; 0
        waits forever), as Playwright's ``page.expect_response``::

            async with page.expect_response("**/api/items") as info:
                await page.get_by_text("Load").click()
            items = await (await info.value).json()
        """
        async with self._expect("response", url_or_predicate, timeout) as info:
            yield info

    @asynccontextmanager
    async def expect_request(
        self, url_or_predicate: URLMatch | Callable[[Request], bool], *, timeout: float | None = None
    ) -> AsyncIterator[EventInfo[Request]]:
        """``expect_response``, for requests."""
        async with self._expect("request", url_or_predicate, timeout) as info:
            yield info

    @asynccontextmanager
    async def expect_websocket(
        self,
        predicate: Callable[[WebSocket], bool] | None = None,
        *,
        timeout: float | None = None,
    ) -> AsyncIterator[EventInfo[WebSocket]]:
        """Waits for a WebSocket opened by the code in the ``async with`` block."""
        async with self._expect(
            "websocket",
            predicate if predicate is not None else (lambda _: True),
            timeout,
            label="websocket",
        ) as info:
            yield info

    @asynccontextmanager
    async def expect_worker(
        self,
        predicate: Callable[[Worker], bool] | None = None,
        *,
        timeout: float | None = None,
    ) -> AsyncIterator[EventInfo[Worker]]:
        """Waits for a dedicated Web Worker created by the code in the ``async with`` block."""
        async with self._expect(
            "worker",
            predicate if predicate is not None else (lambda _: True),
            timeout,
            label="worker",
        ) as info:
            yield info

    @asynccontextmanager
    async def expect_event(
        self,
        event: str,
        predicate: Callable[[Any], bool] | None = None,
        *,
        timeout: float | None = None,
    ) -> AsyncIterator[EventInfo[Any]]:
        """Waits for ``event`` to fire on this page."""
        async with self._expect(
            event,
            predicate if predicate is not None else (lambda _: True),
            timeout,
            label="event",
        ) as info:
            yield info

    @property
    def workers(self) -> list[Worker]:
        """Dedicated Web Workers currently active on this page."""
        if not self.is_closed():
            self._ensure_worker_subscription()
        out: list[Worker] = []
        for info in self._workers():
            w = self._worker_map.get(info.id)
            if w is None:
                w = Worker(self, info.id, info.url)
                self._worker_map[info.id] = w
            out.append(w)
        return out

    async def close(self) -> None:
        """Closes the page and rejects any pending ``expect_*`` waiters."""
        for future in list(self._expects):
            if not future.done():
                future.set_exception(Error("page closed"))
        for ws in list(self._websockets_live.values()):
            ws._mark_closed()
        self._websockets_live.clear()
        for w in list(self._worker_map.values()):
            w._mark_closed()
        self._worker_map.clear()
        _pages_by_target.pop(self._target_id(), None)
        ctx = self._context_ref() if self._context_ref is not None else None
        if ctx is not None:
            ctx._remove_page(self)
        self._routes.clear()
        self._listeners.clear()
        self._bindings.clear()
        await super().close()
        await _flush_har_filters(self._pending_har_filters)

    async def emulate_media(
        self,
        *,
        media: Literal["screen", "print"] | None = _UNSET,
        color_scheme: Literal["light", "dark", "no-preference"] | None = _UNSET,
    ) -> None:
        """Changes CSS ``media`` and/or ``prefers-color-scheme``; ``None`` resets either."""
        m = None if media is _UNSET else ("" if media is None else media)
        cs = None if color_scheme is _UNSET else ("" if color_scheme is None else color_scheme)
        await self._emulate_media(media=m, color_scheme=cs)

    @asynccontextmanager
    async def _expect(
        self,
        event: str,
        url_or_predicate: Any,
        timeout: float | None,
        *,
        label: str | None = None,
    ) -> AsyncIterator[EventInfo[Any]]:
        if self.is_closed():
            raise Error("page closed")
        future: asyncio.Future[Any] = asyncio.get_running_loop().create_future()
        self._expects.add(future)
        if callable(url_or_predicate):
            matches = url_or_predicate
        else:
            url = _url_matcher(url_or_predicate)

            def matches(r: Any) -> bool:
                return url(r.url)

        def listener(arg: Any) -> None:
            if future.done():
                return
            try:
                if matches(arg):
                    future.set_result(arg)
            except Exception as e:
                future.set_exception(e)

        self.on(event, listener)
        try:
            yield EventInfo(future)
            limit = 30_000 if timeout is None else timeout
            try:
                await asyncio.wait_for(asyncio.shield(future), limit / 1000 if limit > 0 else None)
            except asyncio.TimeoutError:
                raise TimeoutError(f"page.expect_{label or event}: timeout {limit:g}ms exceeded") from None
        finally:
            self._expects.discard(future)
            self.remove_listener(event, listener)

    def _ensure_popup_subscription(self) -> None:
        if not self._serving_popups:
            _keep(self._tasks, asyncio.ensure_future(_serve_popups(self._popups(), weakref.ref(self), self._tasks)))
            self._serving_popups = True

    def _ensure_worker_subscription(self) -> None:
        if self._serving_workers:
            return
        self._serving_workers = True
        for info in self._workers():
            if info.id not in self._worker_map:
                self._worker_map[info.id] = Worker(self, info.id, info.url)
        _keep(self._tasks, asyncio.ensure_future(_serve_workers(self._worker_events(), weakref.ref(self), self._tasks)))

    def _add_listener(self, method: str, event: str, added: Listener, call: Listener) -> None:
        if event not in _EVENTS:
            raise ValueError(f"page.{method}: {event!r} is not supported ({', '.join(_EVENTS)})")
        # Each source is subscribed here, so nothing after this call is missed.
        if event in ("console", "pageerror") and not self._logging:
            _keep(self._tasks, asyncio.ensure_future(_serve_logs(self._logs(), weakref.ref(self), self._tasks)))
            self._logging = True
        elif event == "dialog" and not self._answering_dialogs:
            _keep(self._tasks, asyncio.ensure_future(_serve_dialogs(self._dialogs(), weakref.ref(self))))
            self._answering_dialogs = True
        elif event == "filechooser" and not self._serving_file_choosers:
            _keep(
                self._tasks,
                asyncio.ensure_future(_serve_file_choosers(self._file_choosers(), weakref.ref(self), self._tasks)),
            )
            self._serving_file_choosers = True
        elif event == "popup":
            self._ensure_popup_subscription()
        elif event == "websocket" and not self._serving_websockets:
            _keep(
                self._tasks,
                asyncio.ensure_future(_serve_websockets(self._websockets(), weakref.ref(self), self._tasks)),
            )
            self._serving_websockets = True
        elif event == "worker":
            self._ensure_worker_subscription()
        elif event in _NETWORK_EVENTS and (self._network_task is None or self._network_task.done()):
            self._network_task = asyncio.ensure_future(_serve_network(self._network(), weakref.ref(self), self._tasks))
            _keep(self._tasks, self._network_task)
        self._listeners.setdefault(event, []).append((added, call))


_NETWORK_EVENTS = ("request", "response", "requestfinished", "requestfailed")
_EVENTS = ("console", "pageerror", *_NETWORK_EVENTS, "dialog", "filechooser", "popup", "websocket", "worker")


# The loops below run until the page closes (the network one also once nobody listens). They reach
# the page only through a weak reference, between events, so a page nobody holds is collected and
# its slot goes back to the pool even when its handlers refer to it (as Node's WeakRef does).

PageRef = Callable[[], "Page | None"]


def _listeners_of(page: PageRef) -> dict[str, list[tuple[Listener, Listener]]]:
    p = page()
    return p._listeners if p is not None else {}


def _keep(tasks: set[asyncio.Future[Any]], future: asyncio.Future[Any]) -> None:
    """Holds a background task until it ends, and logs it if it failed."""
    tasks.add(future)

    def forget(done: asyncio.Future[Any]) -> None:
        tasks.discard(done)
        if not done.cancelled() and done.exception() is not None:
            _log.error("a fluxwright background task failed", exc_info=done.exception())

    future.add_done_callback(forget)


def _remove_listener(listeners: dict[str, list[tuple[Listener, Listener]]], event: str, handler: Listener) -> None:
    entries = listeners.get(event, [])
    for i, (added, _) in enumerate(entries):
        if added == handler:
            del entries[i]
            return


def _emit(
    listeners: dict[str, list[tuple[Listener, Listener]]], event: str, arg: Any, tasks: set[asyncio.Future[Any]]
) -> None:
    for _, call in list(listeners.get(event, ())):
        try:
            result = call(arg)
            if inspect.isawaitable(result):
                _keep(tasks, asyncio.ensure_future(result))
        except Exception:
            _log.exception("a page.on(%r) listener failed", event)


# No loop keeps a listener, route entry or page in a local across ``await stream.next()``: that
# would hold the page for as long as the stream is quiet.


async def _serve_logs(stream: _native.LogStream, page: PageRef, tasks: set[asyncio.Future[Any]]) -> None:
    while (entry := await stream.next()) is not None:
        _emit(_listeners_of(page), "pageerror" if isinstance(entry, Error) else "console", entry, tasks)


# Requests kept for their later events; ones that never finish (a long poll) go first, as in Node.
_MAX_REQUESTS = 1000


async def _serve_network(stream: _native.NetworkStream, page: PageRef, tasks: set[asyncio.Future[Any]]) -> None:
    # One Request per request, as in Playwright: its request, response, requestfinished and
    # requestfailed events, and response.request, hand over the same object.
    requests: dict[str, Request] = {}
    while (e := await stream.next()) is not None:
        request = requests.get(e.request.id)
        if request is None:
            request = requests[e.request.id] = Request(e.request)
            if len(requests) > _MAX_REQUESTS:
                del requests[next(iter(requests))]
        if e.event in ("requestfinished", "requestfailed"):
            requests.pop(e.request.id, None)
        if e.event == "requestfailed":
            request._failure = e.failure
        arg = Response(e.response, request) if e.response is not None else request
        if not _emit_network(_listeners_of(page), e.event, arg, tasks):
            return  # nobody listens: the next listener starts another stream


def _emit_network(
    listeners: dict[str, list[tuple[Listener, Listener]]], event: str, arg: Any, tasks: set[asyncio.Future[Any]]
) -> bool:
    """Emits ``event``; whether any network listener is left."""
    _emit(listeners, event, arg, tasks)
    return any(listeners.get(e) for e in _NETWORK_EVENTS)


async def _serve_dialogs(stream: _native.DialogStream, page: PageRef) -> None:
    while (native := await stream.next()) is not None:
        await _answer_dialog(_listeners_of(page).get("dialog", []), Dialog(native))


async def _serve_file_choosers(
    stream: _native.FileChooserStream,
    page: PageRef,
    tasks: set[asyncio.Future[Any]],
) -> None:
    while (native := await stream.next()) is not None:
        _emit(_listeners_of(page), "filechooser", FileChooser(native), tasks)


def _resolve_init_script(method: str, script: str | None, path: str | PathLike[str] | None) -> str:
    if (script is not None) + (path is not None) != 1:
        raise ValueError(f"{method}: provide either script or path")
    if path is not None:
        return Path(path).read_text(encoding="utf-8")
    if not isinstance(script, str):
        raise TypeError(f"{method}: script must be a str")
    return script


class CDPSession:
    """Raw Chrome DevTools Protocol session attached via ``context.new_cdp_session(page)``."""

    def __init__(self, impl: _native.CdpSession) -> None:
        self._impl = impl
        self._listeners: dict[str, list[tuple[Listener, Listener]]] = {}
        self._serving = False
        self._detached = False
        self._tasks: set[asyncio.Future[Any]] = set()

    async def send(self, method: str, params: dict[str, Any] | None = None) -> Any:
        if self._detached:
            raise Error("cdp_session.send: session is detached")
        if not isinstance(method, str) or not method:
            raise TypeError("cdp_session.send: method must be a non-empty str")
        return await self._impl.send(method, params)

    def _ensure_events(self) -> None:
        if not self._serving and not self._detached:
            _keep(
                self._tasks,
                asyncio.ensure_future(_serve_cdp_events(self._impl._events(), weakref.ref(self), self._tasks)),
            )
            self._serving = True

    def on(self, event: str, handler: Listener) -> None:
        self._ensure_events()
        self._listeners.setdefault(event, []).append((handler, handler))

    def once(self, event: str, handler: Listener) -> None:
        self._ensure_events()
        listeners = self._listeners

        def call(arg: Any) -> Any:
            with suppress(ValueError):
                listeners[event].remove((handler, call))
            return handler(arg)

        self._listeners.setdefault(event, []).append((handler, call))

    def remove_listener(self, event: str, handler: Listener) -> None:
        _remove_listener(self._listeners, event, handler)

    off = remove_listener

    async def detach(self) -> None:
        if self._detached:
            return
        self._detached = True
        self._listeners.clear()
        await self._impl.detach()


async def _serve_cdp_events(
    stream: _native.CdpEventStream, session: weakref.ref[CDPSession], tasks: set[asyncio.Future[Any]]
) -> None:
    while (ev := await stream.next()) is not None:
        s = session()
        if s is None or s._detached:
            return
        params = _json.loads(ev.params_json) if ev.params_json else {}
        _emit(s._listeners, ev.method, params, tasks)


async def _serve_popups(stream: _native.PopupStream, page: PageRef, tasks: set[asyncio.Future[Any]]) -> None:
    while (lease := await stream.next()) is not None:
        popup = Page(lease)
        opener = page()
        if opener is not None:
            ctx = opener._context_ref() if opener._context_ref is not None else None
            if ctx is not None:
                ctx._adopt_popup(popup)
            _emit(opener._listeners, "popup", popup, tasks)


async def _serve_websockets(
    stream: _native.WebSocketStream, page: PageRef, tasks: set[asyncio.Future[Any]]
) -> None:
    while (ev := await stream.next()) is not None:
        p = page()
        if p is None:
            return
        if ev.event == "created":
            ws = WebSocket(p, ev.url or "")
            p._websockets_live[ev.request_id] = ws
            _emit(p._listeners, "websocket", ws, tasks)
        else:
            ws = p._websockets_live.get(ev.request_id)
            if ws is None:
                continue
            if ev.event in ("framesent", "framereceived"):
                payload = ev.payload_bytes if ev.payload_bytes is not None else (ev.payload_text or "")
                ws._emit(ev.event, payload)
            elif ev.event == "socketerror":
                ws._emit("socketerror", ev.error_message or "")
            elif ev.event == "close":
                p._websockets_live.pop(ev.request_id, None)
                ws._mark_closed()


async def _serve_workers(
    stream: _native.WorkerStream, page: PageRef, tasks: set[asyncio.Future[Any]]
) -> None:
    while (ev := await stream.next()) is not None:
        p = page()
        if p is None:
            return
        if ev.event == "created":
            w = p._worker_map.get(ev.id)
            if w is None:
                w = Worker(p, ev.id, ev.url or "")
                p._worker_map[ev.id] = w
            _emit(p._listeners, "worker", w, tasks)
        elif ev.event == "console":
            w = p._worker_map.get(ev.id)
            if w is not None and ev.message is not None:
                w._emit("console", ev.message)
        elif ev.event == "close":
            w = p._worker_map.pop(ev.id, None)
            if w is not None:
                w._mark_closed()


async def _serve_bindings(
    stream: _native.BindingStream,
    owner: Callable[[], Any],
    tasks: set[asyncio.Future[Any]],
) -> None:
    while (call := await stream.next()) is not None:
        _keep(tasks, asyncio.ensure_future(_invoke_binding(call, owner)))


async def _invoke_binding(call: _native.BindingCall, owner: Callable[[], Any]) -> None:
    obj = owner()
    cb = obj._bindings.get(call.name) if obj is not None else None
    if cb is None:
        with suppress(Error):
            await call.reject(f'Function "{call.name}" is not exposed')
        return
    try:
        args = _json.loads(call.args_json)
        result = cb(*args)
        if inspect.isawaitable(result):
            result = await result
        encoded = _json.dumps(result)
    except Exception as exc:
        with suppress(Error):
            await call.reject(str(exc))
        return
    with suppress(Error):
        await call.resolve(encoded)


async def _answer_dialog(listeners: list[tuple[Listener, Listener]], dialog: Dialog) -> None:
    # Every listener runs and is awaited; a dialog none of them answered is dismissed, so the
    # page never hangs on it.
    pending = []
    for _, call in list(listeners):
        try:
            result = call(dialog)
            if inspect.isawaitable(result):
                pending.append(result)
        except Exception:
            _log.exception("a page.on('dialog') listener failed")
    for outcome in await asyncio.gather(*pending, return_exceptions=True):
        if isinstance(outcome, BaseException):
            _log.error("a page.on('dialog') listener failed", exc_info=outcome)
    if not dialog._answered:
        # As with no listener at all: beforeunload is accepted (dismissing it would cancel
        # the navigation), the rest dismissed.
        with suppress(Error):
            await (dialog.accept() if dialog.type == "beforeunload" else dialog.dismiss())


def _ctx_routes_of(ctx: weakref.ref[BrowserContext]) -> list[_RouteEntry]:
    c = ctx()
    return list(c._routes) if c is not None else []


def _routes_of(page: PageRef) -> list[_RouteEntry]:
    p = page()
    if p is None:
        return []
    ctx = p._context_ref() if p._context_ref is not None else None
    ctx_routes = list(ctx._routes) if ctx is not None else []
    return ctx_routes + list(p._routes)


async def _serve_routes(
    stream: _native.RouteStream, routes_of: Callable[[], list[_RouteEntry]], tasks: set[asyncio.Future[Any]]
) -> None:
    while (native := await stream.next()) is not None:
        _keep(tasks, asyncio.ensure_future(_dispatch(native, routes_of())))


async def _dispatch(native: _native.Route, routes: list[_RouteEntry]) -> None:
    request = Request(native)
    answered = False
    try:
        for entry in reversed(list(routes)):
            try:
                if not entry.matches(native.url):
                    continue
            except Exception:
                _log.exception("a page.route URL matcher failed")
                continue
            route = Route(native, request)
            try:
                try:
                    result = entry.handler(route, request) if entry.with_request else entry.handler(route)
                    if inspect.isawaitable(result):
                        await result
                finally:
                    route._returned = True
                if route._fell_back and route._answer is None:
                    continue
                if route._answer is not None:
                    await route._answer
                    answered = True
            except Exception:
                _log.exception("a route handler failed; the request continues")
            break
    finally:
        # No answer, or one that failed: let the request through rather than hang the page. One
        # that failed after taking the request is refused here, harmlessly (the engine continues
        # it).
        if not answered:
            with suppress(Error):
                await native.continue_()


class Browser:
    """The engine: a pool of Chrome processes that pages are leased from."""

    def __init__(self, engine: _native.Engine) -> None:
        self._engine = engine
        self._closed = False

    async def new_page(
        self,
        *,
        proxy: ProxySettings | None = None,
        user_agent: str | None = None,
        locale: str | None = None,
        timezone_id: str | None = None,
        geolocation: Geolocation | None = None,
        permissions: list[str] | None = None,
        viewport: ViewportSize | None = None,
        device_scale_factor: float | None = None,
        color_scheme: str | None = None,
        is_mobile: bool | None = None,
        has_touch: bool | None = None,
        extra_http_headers: dict[str, str] | None = None,
        ignore_https_errors: bool | None = None,
        java_script_enabled: bool | None = None,
        offline: bool | None = None,
        base_url: str | None = None,
        http_credentials: HttpCredentials | None = None,
        bypass_csp: bool | None = None,
        storage_state: StorageState | str | PathLike[str] | None = None,
        record_har_path: str | PathLike[str] | None = None,
        record_har_omit_content: bool | None = None,
        record_har_content: Literal["embed", "omit"] | None = None,
        record_har_mode: Literal["full", "minimal"] | None = None,
        record_har_url_filter: str | re.Pattern[str] | None = None,
        resource_preset: Literal["scraping", "minimal", "no-media"] | None = None,
        block_resources: Sequence[Literal["image", "font", "media", "stylesheet", "script"]] | None = None,
        block_url_patterns: Sequence[str] | None = None,
    ) -> Page:
        """A page in a fresh browser context; every option applies to this page only, with
        Playwright's names. ``locale`` is BCP 47 (``de-DE``), ``timezone_id`` IANA
        (``Asia/Tokyo``), ``color_scheme`` ``light``, ``dark`` or ``no-preference``;
        ``geolocation`` also needs ``permissions=["geolocation"]``; ``storage_state`` is what
        ``page.storage_state()`` returned, or the JSON file it saved."""
        native_url_filter, pending_filter = _split_har_url_filter(record_har_path, record_har_url_filter)
        lease = await self._engine.new_page(
            proxy=proxy,
            user_agent=user_agent,
            locale=locale,
            timezone_id=timezone_id,
            geolocation=geolocation,
            permissions=permissions,
            viewport=viewport,
            device_scale_factor=device_scale_factor,
            color_scheme=color_scheme,
            is_mobile=is_mobile,
            has_touch=has_touch,
            extra_http_headers=extra_http_headers,
            ignore_https_errors=ignore_https_errors,
            java_script_enabled=java_script_enabled,
            offline=offline,
            base_url=base_url,
            http_credentials=http_credentials,
            bypass_csp=bypass_csp,
            storage_state=storage_state,
            record_har_path=record_har_path,
            record_har_omit_content=record_har_omit_content,
            record_har_content=record_har_content,
            record_har_mode=record_har_mode,
            record_har_url_filter=native_url_filter,
            resource_preset=resource_preset,
            block_resources=list(block_resources) if block_resources is not None else None,
            block_url_patterns=list(block_url_patterns) if block_url_patterns is not None else None,
        )
        page = Page(lease)
        if pending_filter is not None:
            page._pending_har_filters.append(pending_filter)
        return page

    async def new_context(
        self,
        *,
        proxy: ProxySettings | None = None,
        user_agent: str | None = None,
        locale: str | None = None,
        timezone_id: str | None = None,
        geolocation: Geolocation | None = None,
        permissions: list[str] | None = None,
        viewport: ViewportSize | None = None,
        device_scale_factor: float | None = None,
        color_scheme: str | None = None,
        is_mobile: bool | None = None,
        has_touch: bool | None = None,
        extra_http_headers: dict[str, str] | None = None,
        ignore_https_errors: bool | None = None,
        java_script_enabled: bool | None = None,
        offline: bool | None = None,
        base_url: str | None = None,
        http_credentials: HttpCredentials | None = None,
        bypass_csp: bool | None = None,
        storage_state: StorageState | str | PathLike[str] | None = None,
        record_har_path: str | PathLike[str] | None = None,
        record_har_omit_content: bool | None = None,
        record_har_content: Literal["embed", "omit"] | None = None,
        record_har_mode: Literal["full", "minimal"] | None = None,
        record_har_url_filter: str | re.Pattern[str] | None = None,
        resource_preset: Literal["scraping", "minimal", "no-media"] | None = None,
        block_resources: Sequence[Literal["image", "font", "media", "stylesheet", "script"]] | None = None,
        block_url_patterns: Sequence[str] | None = None,
    ) -> BrowserContext:
        """A browser context with ``new_page``'s options, as Playwright's ``new_context``; see ``BrowserContext``."""
        native_url_filter, pending_filter = _split_har_url_filter(record_har_path, record_har_url_filter)
        native_ctx = await self._engine.new_context(
            proxy=proxy,
            user_agent=user_agent,
            locale=locale,
            timezone_id=timezone_id,
            geolocation=geolocation,
            permissions=permissions,
            viewport=viewport,
            device_scale_factor=device_scale_factor,
            color_scheme=color_scheme,
            is_mobile=is_mobile,
            has_touch=has_touch,
            extra_http_headers=extra_http_headers,
            ignore_https_errors=ignore_https_errors,
            java_script_enabled=java_script_enabled,
            offline=offline,
            base_url=base_url,
            http_credentials=http_credentials,
            bypass_csp=bypass_csp,
            storage_state=storage_state,
            record_har_path=record_har_path,
            record_har_omit_content=record_har_omit_content,
            record_har_content=record_har_content,
            record_har_mode=record_har_mode,
            record_har_url_filter=native_url_filter,
            resource_preset=resource_preset,
            block_resources=list(block_resources) if block_resources is not None else None,
            block_url_patterns=list(block_url_patterns) if block_url_patterns is not None else None,
        )
        ctx = BrowserContext(self, native_ctx)
        if pending_filter is not None:
            ctx._pending_har_filters.append(pending_filter)
        return ctx

    async def close(self) -> None:
        """Shuts the engine down, and with it every page and Chrome process it started."""
        if not self._closed:
            self._closed = True
            await self._engine.close()

    async def __aenter__(self) -> Browser:
        return self

    async def __aexit__(self, *exc: Any) -> None:
        await self.close()


class BrowserContext:
    """A browser context, as Playwright's: holds multiple pages and popups sharing cookies,
    storage, init scripts, exposed functions and routes.
    """

    def __init__(self, browser: Browser, ctx: _native.Context) -> None:
        self._browser = browser
        self._ctx = ctx
        self._closed = False
        self._pages: list[Page] = []
        self._routes: list[_RouteEntry] = []
        self._routing = False
        self._route_lock = asyncio.Lock()
        self._bindings: dict[str, Callable[..., Any]] = {}
        self._serving_bindings = False
        self._tasks: set[asyncio.Future[Any]] = set()
        self._pending_har_filters: list[tuple[Path, re.Pattern[str]]] = []
        self._request: APIRequestContext | None = None

    @property
    def request(self) -> APIRequestContext:
        if self._request is None:
            self._request = APIRequestContext(
                self._ctx.api_fetch,
                self._ctx.cookies,
                lambda: self._closed,
            )
        return self._request

    @property
    def browser(self) -> Browser:
        return self._browser

    @property
    def pages(self) -> list[Page]:
        self._pages = [p for p in self._pages if not p.is_closed()]
        return list(self._pages)

    def _adopt_page(self, page: Page) -> None:
        if page not in self._pages:
            self._pages.append(page)
        page._context_ref = weakref.ref(self)
        if not page.is_closed():
            page._ensure_popup_subscription()

    def _adopt_popup(self, popup: Page) -> None:
        if self._closed:
            return
        self._adopt_page(popup)

    def _remove_page(self, page: Page) -> None:
        with suppress(ValueError):
            self._pages.remove(page)

    async def new_page(self) -> Page:
        if self._closed:
            raise Error("browser_context.new_page: the context is closed")
        lease = await self._ctx.new_page()
        page = Page(lease)
        if self._closed:
            await page.close()
            raise Error("browser_context.new_page: the context is closed")
        self._adopt_page(page)
        return page

    async def route(self, url: URLMatch, handler: Callable[..., Any]) -> None:
        """``page.route`` for all pages and popups in this context; added before ``new_page()``, it
        applies from the first request."""
        if self._closed:
            raise Error("browser_context.route: the context is closed")
        async with self._route_lock:
            if not self._routing:
                stream = await self._ctx.intercept()
                ref = weakref.ref(self)
                _keep(
                    self._tasks,
                    asyncio.ensure_future(_serve_routes(stream, lambda: _ctx_routes_of(ref), self._tasks)),
                )
                self._routing = True
            self._routes.append(_RouteEntry(url, handler))

    async def unroute(self, url: URLMatch, handler: Callable[..., Any] | None = None) -> None:
        self._routes[:] = [r for r in self._routes if not (r.url == url and (handler is None or r.handler == handler))]

    async def route_from_har(
        self,
        har: str | PathLike[str],
        *,
        url: URLMatch | None = None,
        not_found: Literal["abort", "fallback"] | None = None,
        update: bool | None = None,
        update_mode: Literal["full", "minimal"] | None = None,
        update_content: Literal["embed", "omit"] | None = None,
    ) -> None:
        """Serves matching requests on this context from a HAR 1.2 file, or records when ``update=True``."""
        if self._closed:
            raise Error("browser_context.route_from_har: the context is closed")
        await _apply_route_from_har(
            self,
            "browser_context",
            self._ctx._start_har_recording,
            self._pending_har_filters,
            har,
            url=url,
            not_found=not_found,
            update=update,
            update_mode=update_mode,
            update_content=update_content,
        )

    async def add_init_script(
        self, script: str | None = None, *, path: str | PathLike[str] | None = None
    ) -> None:
        """``page.add_init_script`` for all pages and popups in this context."""
        if self._closed:
            raise Error("browser_context.add_init_script: the context is closed")
        source = _resolve_init_script("browser_context.add_init_script", script, path)
        await self._ctx.add_init_script(source)

    async def expose_function(self, name: str, callback: Callable[..., Any]) -> None:
        """``page.expose_function`` for all pages and popups in this context."""
        if self._closed:
            raise Error("browser_context.expose_function: the context is closed")
        if not isinstance(name, str) or not name:
            raise TypeError("browser_context.expose_function: name must be a non-empty str")
        if not callable(callback):
            raise TypeError("browser_context.expose_function: callback must be callable")
        if name in self._bindings:
            raise Error(f'Function "{name}" has been already registered')
        if not self._serving_bindings:
            _keep(
                self._tasks,
                asyncio.ensure_future(_serve_bindings(self._ctx.binding_calls(), weakref.ref(self), self._tasks)),
            )
            self._serving_bindings = True
        self._bindings[name] = callback
        try:
            await self._ctx.expose_binding(name)
        except BaseException:
            if self._bindings.get(name) is callback:
                del self._bindings[name]
            raise

    async def storage_state(
        self, *, path: str | PathLike[str] | None = None, indexed_db: bool = False
    ) -> StorageState:
        """The cookies, localStorage, and optionally IndexedDB of this context, usable before or after ``new_page()``."""
        if self._closed:
            raise Error("browser_context.storage_state: the context is closed")
        pages = self.pages
        if not pages:
            return await self._ctx.storage_state(path=path, indexed_db=indexed_db)
        if len(pages) == 1:
            return await pages[0].storage_state(path=path, indexed_db=indexed_db)
        by_origin: dict[str, dict[str, Any]] = {}
        cookies: list[dict[str, Any]] = []
        for idx, page_item in enumerate(pages):
            st = await page_item.storage_state(indexed_db=indexed_db)
            if idx == 0:
                cookies = list(st.get("cookies", []))
            for origin in st.get("origins", []):
                by_origin[origin["origin"]] = origin
        merged: StorageState = {"cookies": cookies, "origins": list(by_origin.values())}
        if path is not None:
            _save_storage_json_private(path, merged)
        return merged

    async def cookies(self, urls: str | Sequence[str] | None = None) -> list[dict[str, Any]]:
        """Cookies in this context (all, or matching ``urls``)."""
        if self._closed:
            raise Error("browser_context.cookies: the context is closed")
        if urls is None:
            return await self._ctx.cookies()
        url_list = [urls] if isinstance(urls, str) else list(urls)
        return await self._ctx.cookies(url_list)

    async def add_cookies(self, cookies: list[dict[str, Any]]) -> None:
        """Adds cookies to this context."""
        if self._closed:
            raise Error("browser_context.add_cookies: the context is closed")
        await self._ctx.add_cookies(cookies)

    async def clear_cookies(
        self,
        *,
        name: str | re.Pattern[str] | None = None,
        domain: str | re.Pattern[str] | None = None,
        path: str | re.Pattern[str] | None = None,
    ) -> None:
        """Clears cookies in this context, optionally filtered by name, domain, and/or path."""
        if self._closed:
            raise Error("browser_context.clear_cookies: the context is closed")
        if name is None and domain is None and path is None:
            await self._ctx.clear_cookies()
            return
        all_cookies = await self._ctx.cookies()
        keep = [
            c
            for c in all_cookies
            if not (
                (name is None or _cookie_field_matches(name, c["name"]))
                and (domain is None or _cookie_field_matches(domain, c["domain"], is_domain=True))
                and (path is None or _cookie_field_matches(path, c["path"]))
            )
        ]
        if len(keep) != len(all_cookies):
            await self._ctx.replace_cookies(keep)

    async def new_cdp_session(self, page: Page | Frame) -> CDPSession:
        """Attaches a raw Chrome DevTools Protocol session to ``page`` or an out-of-process ``Frame``."""
        if self._closed:
            raise Error("browser_context.new_cdp_session: the context is closed")
        if isinstance(page, Frame):
            target_page, frame_id = page.page, page._id
        elif isinstance(page, Page):
            target_page, frame_id = page, None
        else:
            raise TypeError("browser_context.new_cdp_session: argument must be a Page or Frame")
        ctx = target_page._context_ref() if target_page._context_ref is not None else None
        if ctx is not self:
            raise Error("browser_context.new_cdp_session: page does not belong to this context")
        return CDPSession(await target_page._new_cdp_session(frame_id))

    async def close(self) -> None:
        """Closes this context and all of its pages and popups."""
        if self._closed:
            return
        self._closed = True
        for p in list(self._pages):
            if not p.is_closed():
                with suppress(Error):
                    await p.close()
        self._pages.clear()
        with suppress(Error):
            await self._ctx.close()
        await _flush_har_filters(self._pending_har_filters)

    async def __aenter__(self) -> BrowserContext:
        return self

    async def __aexit__(self, *exc: Any) -> None:
        await self.close()


class BrowserType:
    """``chromium``: starts the engine."""

    name = "chromium"

    def __init__(self, launched: list[Browser] | None = None) -> None:
        self._launched = launched

    async def launch(
        self,
        *,
        headless: bool | None = None,
        executable_path: str | PathLike[str] | None = None,
        max_browsers: int | None = None,
        queue_timeout: float | None = None,
        memory_ceiling_mb: int | None = None,
        max_concurrency_per_host: int | None = None,
        min_delay_per_host: float | None = None,
        args: Sequence[str] | None = None,
        ignore_default_args: bool | Sequence[str] | None = None,
        channel: str | None = None,
        prewarm_contexts: int | None = None,
    ) -> Browser:
        """Starts the engine in this process: up to ``max_browsers`` Chrome processes (default
        one per 4 CPUs, 8 pages each), headless unless ``headless=False``. New pages wait while
        all Chrome processes together use more than ``memory_ceiling_mb`` (default 75% of the
        memory free at launch). Headless uses chrome-headless-shell when it is installed, as
        Playwright does; ``executable_path`` or ``FLUXWRIGHT_CHROMIUM`` picks the binary. When
        every slot is busy, ``new_page`` waits its turn however long the queue is; with
        ``queue_timeout`` (milliseconds) it raises ``TimeoutError`` after that long instead,
        counting a browser start when one is needed. For polite crawling,
        ``max_concurrency_per_host`` caps the pages on one host at once across the fleet and
        ``min_delay_per_host`` (milliseconds) spaces the ``goto`` calls to it: a ``goto`` waits
        its turn."""
        if ignore_default_args is not None and not isinstance(ignore_default_args, bool):
            ignore_default_args = list(ignore_default_args)
        engine = await _native.Engine.launch(
            max_browsers=max_browsers,
            executable_path=executable_path,
            headless=headless,
            queue_timeout=queue_timeout,
            memory_ceiling_mb=memory_ceiling_mb,
            max_concurrency_per_host=max_concurrency_per_host,
            min_delay_per_host=min_delay_per_host,
            args=list(args) if args is not None else None,
            ignore_default_args=ignore_default_args,
            channel=channel,
            prewarm_contexts=prewarm_contexts,
        )
        browser = Browser(engine)
        if self._launched is not None:
            self._launched.append(browser)
        return browser

    async def connect_over_cdp(self, endpoint_url: str) -> Browser:
        """Connects to an existing browser over the Chrome DevTools Protocol (``ws://...`` or ``http://...``)."""
        engine = await _native.Engine.connect_over_cdp(endpoint_url)
        browser = Browser(engine)
        if self._launched is not None:
            self._launched.append(browser)
        return browser


class Playwright:
    """What ``async_playwright()`` gives: ``p.chromium``. Browsers launched through it close
    when it stops."""

    def __init__(self) -> None:
        self._browsers: list[Browser] = []
        self.chromium = BrowserType(self._browsers)

    async def stop(self) -> None:
        await asyncio.gather(*(b.close() for b in self._browsers), return_exceptions=True)


class PlaywrightContextManager:
    def __init__(self) -> None:
        self._playwright: Playwright | None = None

    async def start(self) -> Playwright:
        self._playwright = Playwright()
        return self._playwright

    async def __aenter__(self) -> Playwright:
        return await self.start()

    async def __aexit__(self, *exc: Any) -> None:
        if self._playwright is not None:
            await self._playwright.stop()


def async_playwright() -> PlaywrightContextManager:
    """Playwright's entry point, so a Playwright script runs after changing only its import."""
    return PlaywrightContextManager()


#: Launches the engine directly: ``browser = await chromium.launch()``.
chromium = BrowserType()
