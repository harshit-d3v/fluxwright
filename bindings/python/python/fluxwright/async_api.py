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
import inspect
import json as _json
import logging
import re
from contextlib import asynccontextmanager, suppress
from os import PathLike
from pathlib import Path
from typing import (
    Any,
    Generic,
    TypeVar,
)
from collections.abc import AsyncIterator, Awaitable, Callable

from . import _native
from ._errors import Error, TimeoutError
from ._native import ConsoleMessage, Download, FrameLocator, Locator
from ._types import FloatRect, Geolocation, ProxySettings, StorageState, URLMatch, ViewportSize

__all__ = [
    "Browser",
    "BrowserType",
    "ConsoleMessage",
    "Download",
    "Error",
    "EventInfo",
    "FloatRect",
    "FrameLocator",
    "Geolocation",
    "Locator",
    "Page",
    "Playwright",
    "PlaywrightContextManager",
    "ProxySettings",
    "Request",
    "Route",
    "StorageState",
    "TimeoutError",
    "ViewportSize",
    "async_playwright",
    "chromium",
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
    """A request as a route handler sees it."""

    def __init__(self, native: _native.Route) -> None:
        self._native = native

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

    def __repr__(self) -> str:
        return f"<Request method={self.method!r} url={self.url!r}>"


class Route:
    """How a route handler answers a request, as Playwright's ``Route``. Each request takes one
    answer; the methods return awaitables."""

    def __init__(self, native: _native.Route, request: Request) -> None:
        self._native = native
        self._request = request
        self._answer: asyncio.Future[None] | None = None
        self._fell_back = False

    @property
    def request(self) -> Request:
        return self._request

    def fulfill(
        self,
        *,
        status: int | None = None,
        headers: dict[str, str] | None = None,
        body: str | bytes | None = None,
        json: Any = None,
        path: str | PathLike[str] | None = None,
        content_type: str | None = None,
    ) -> Awaitable[None]:
        """A made-up response. ``json`` is serialized and sets the content type; ``path`` is a
        file to send."""

        async def answer() -> None:
            sent = dict(headers or {})
            data = body
            if json is not None:
                data = _json.dumps(json)
                sent.setdefault("content-type", "application/json")
            if path is not None:
                data = Path(path).read_bytes()
            if content_type is not None:
                sent["content-type"] = content_type
            await self._native.fulfill(status=status, headers=sent, body=data)

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
        if self._answer is not None:
            return _settled(error=Error("route is already handled"))
        self._fell_back = True
        return _settled()

    def _answer_with(self, start: Callable[[], Awaitable[None]]) -> Awaitable[None]:
        # The first answer wins. It starts now rather than when awaited, so an answer still under
        # way when the handler returns is waited for, not taken as no answer.
        if self._answer is not None or self._fell_back:
            return _settled(error=Error("route is already handled"))

        async def run() -> None:
            await start()

        self._answer = asyncio.ensure_future(run())
        return self._answer


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


Listener = Callable[[Any], Any]


class Page(_native.Page):
    """A browser tab in a fresh browser context, leased from the engine.

    Close it when the job is done; a page you forget is cleaned up when Python collects it.
    """

    def __init__(self, lease: _native.Lease) -> None:
        self._routes: list[_RouteEntry] = []
        self._routing = False
        self._route_lock = asyncio.Lock()
        self._listeners: dict[str, list[tuple[Listener, Listener]]] = {}
        self._logging = False
        self._tasks: set[asyncio.Future[Any]] = set()

    async def route(self, url: URLMatch, handler: Callable[..., Any]) -> None:
        """Hands matching requests of this page, its popups and iframes to ``handler(route)``
        or ``handler(route, request)``, as Playwright's ``page.route``. The handler added last
        runs first; an unanswered request continues. Call it before navigating."""
        # A call made while another is turning interception on waits for it, so no call returns
        # before requests are handed over.
        async with self._route_lock:
            if not self._routing:
                stream = await self._intercept()
                _keep(self._tasks, asyncio.ensure_future(_serve_routes(stream, self._routes, self._tasks)))
                self._routing = True
            self._routes.append(_RouteEntry(url, handler))

    async def unroute(self, url: URLMatch, handler: Callable[..., Any] | None = None) -> None:
        """Removes the handlers added for ``url`` (only ``handler``, when given)."""
        self._routes[:] = [r for r in self._routes if not (r.url == url and (handler is None or r.handler == handler))]

    def on(self, event: str, handler: Listener) -> None:
        """Calls ``handler`` with each ``"console"`` message (a ``ConsoleMessage``) or
        ``"pageerror"`` (an ``Error``) from now on. Calls of a coroutine function run as tasks."""
        self._add_listener("on", event, handler, handler)

    def once(self, event: str, handler: Listener) -> None:
        """``on``, for the next event only."""
        listeners = self._listeners

        def call(arg: Any) -> Any:
            _remove_listener(listeners, event, handler)
            return handler(arg)

        self._add_listener("once", event, handler, call)

    def remove_listener(self, event: str, handler: Listener) -> None:
        _remove_listener(self._listeners, event, handler)

    off = remove_listener

    @asynccontextmanager
    async def expect_download(self, timeout: float | None = None) -> AsyncIterator[EventInfo[Download]]:
        """Waits for the download the code in the ``async with`` block starts (``timeout`` in
        milliseconds, default 30 s), as Playwright's ``page.expect_download``::

            async with page.expect_download() as info:
                await page.get_by_text("Export").click()
            download = await info.value
        """
        future = asyncio.ensure_future(self.wait_for_download(timeout=timeout))
        try:
            yield EventInfo(future)
        except BaseException:
            future.cancel()
            raise
        await future

    def _add_listener(self, method: str, event: str, added: Listener, call: Listener) -> None:
        if event not in ("console", "pageerror"):
            raise ValueError(f"page.{method}: {event!r} is not supported (console, pageerror)")
        if not self._logging:
            # Subscribed here, so nothing logged after this call is missed.
            _keep(self._tasks, asyncio.ensure_future(_serve_logs(self._logs(), self._listeners, self._tasks)))
            self._logging = True
        self._listeners.setdefault(event, []).append((added, call))


# The loops below run until the page closes. They hold the page's handlers, never the page, so a
# page nobody holds is still collected and its slot goes back to the pool.


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


async def _serve_logs(
    stream: _native.LogStream,
    listeners: dict[str, list[tuple[Listener, Listener]]],
    tasks: set[asyncio.Future[Any]],
) -> None:
    while (entry := await stream.next()) is not None:
        event = "pageerror" if isinstance(entry, Error) else "console"
        for _, call in list(listeners.get(event, ())):
            try:
                result = call(entry)
                if inspect.isawaitable(result):
                    _keep(tasks, asyncio.ensure_future(result))
            except Exception:
                _log.exception("a page.on(%r) listener failed", event)


async def _serve_routes(
    stream: _native.RouteStream, routes: list[_RouteEntry], tasks: set[asyncio.Future[Any]]
) -> None:
    while (native := await stream.next()) is not None:
        _keep(tasks, asyncio.ensure_future(_dispatch(native, routes)))


async def _dispatch(native: _native.Route, routes: list[_RouteEntry]) -> None:
    request = Request(native)
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
                result = entry.handler(route, request) if entry.with_request else entry.handler(route)
                if inspect.isawaitable(result):
                    await result
                if route._fell_back and route._answer is None:
                    continue
                if route._answer is not None:
                    await route._answer
            except Exception:
                _log.exception("a route handler failed; the request continues")
            break
    finally:
        # No answer, or one that failed: let the request through rather than hang the page. A
        # request already answered refuses this, harmlessly.
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
        storage_state: StorageState | str | PathLike[str] | None = None,
    ) -> Page:
        """A page in a fresh browser context; every option applies to this page only, with
        Playwright's names. ``locale`` is BCP 47 (``de-DE``), ``timezone_id`` IANA
        (``Asia/Tokyo``), ``color_scheme`` ``light``, ``dark`` or ``no-preference``;
        ``geolocation`` also needs ``permissions=["geolocation"]``; ``storage_state`` is what
        ``page.storage_state()`` returned, or the JSON file it saved."""
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
            storage_state=storage_state,
        )
        return Page(lease)

    async def close(self) -> None:
        """Shuts the engine down, and with it every page and Chrome process it started."""
        if not self._closed:
            self._closed = True
            await self._engine.close()

    async def __aenter__(self) -> Browser:
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
    ) -> Browser:
        """Starts the engine in this process: up to ``max_browsers`` Chrome processes (default
        4), headless unless ``headless=False``. Headless uses chrome-headless-shell when it is
        installed, as Playwright does; ``executable_path`` or ``FLUXWRIGHT_CHROMIUM`` picks the
        binary. When every slot is busy, ``new_page`` waits its turn however long the queue is;
        with ``queue_timeout`` (milliseconds) it raises ``TimeoutError`` after that long instead."""
        engine = await _native.Engine.launch(
            max_browsers=max_browsers, executable_path=executable_path, headless=headless, queue_timeout=queue_timeout
        )
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
