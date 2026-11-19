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
import math
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
from urllib.parse import urljoin, urlparse
from collections.abc import AsyncIterator, Awaitable, Callable, Sequence

from . import _native
from ._errors import Error, TimeoutError
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
    "Browser",
    "BrowserContext",
    "BrowserType",
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
    "Mouse",
    "Page",
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
        self._bindings: dict[str, Callable[..., Any]] = {}
        self._serving_bindings = False
        self._tasks: set[asyncio.Future[Any]] = set()
        self._expects: set[asyncio.Future[Any]] = set()
        self._context_ref: weakref.ref[BrowserContext] | None = None
        self._frame_cache: weakref.WeakValueDictionary[str, Frame] = weakref.WeakValueDictionary()
        _pages_by_target[self._target_id()] = self

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

    async def close(self) -> None:
        """Closes the page and rejects any pending ``expect_*`` waiters."""
        for future in list(self._expects):
            if not future.done():
                future.set_exception(Error("page closed"))
        _pages_by_target.pop(self._target_id(), None)
        ctx = self._context_ref() if self._context_ref is not None else None
        if ctx is not None:
            ctx._remove_page(self)
        self._routes.clear()
        self._listeners.clear()
        self._bindings.clear()
        await super().close()

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
        elif event in _NETWORK_EVENTS and (self._network_task is None or self._network_task.done()):
            self._network_task = asyncio.ensure_future(_serve_network(self._network(), weakref.ref(self), self._tasks))
            _keep(self._tasks, self._network_task)
        self._listeners.setdefault(event, []).append((added, call))


_NETWORK_EVENTS = ("request", "response", "requestfinished", "requestfailed")
_EVENTS = ("console", "pageerror", *_NETWORK_EVENTS, "dialog", "filechooser", "popup")


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


async def _serve_popups(stream: _native.PopupStream, page: PageRef, tasks: set[asyncio.Future[Any]]) -> None:
    while (lease := await stream.next()) is not None:
        popup = Page(lease)
        opener = page()
        if opener is not None:
            ctx = opener._context_ref() if opener._context_ref is not None else None
            if ctx is not None:
                ctx._adopt_popup(popup)
            _emit(opener._listeners, "popup", popup, tasks)


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
        )
        return Page(lease)

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
    ) -> BrowserContext:
        """A browser context with ``new_page``'s options, as Playwright's ``new_context``; see ``BrowserContext``."""
        ctx = await self._engine.new_context(
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
        )
        return BrowserContext(self, ctx)

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

    async def storage_state(self, *, path: str | PathLike[str] | None = None) -> StorageState:
        """The cookies and localStorage of this context, usable before or after ``new_page()``."""
        if self._closed:
            raise Error("browser_context.storage_state: the context is closed")
        pages = self.pages
        if pages:
            return await pages[0].storage_state(path=path)
        return await self._ctx.storage_state(path=path)

    async def cookies(self) -> list[dict[str, Any]]:
        """Every cookie in this context."""
        if self._closed:
            raise Error("browser_context.cookies: the context is closed")
        return await self._ctx.cookies()

    async def add_cookies(self, cookies: list[dict[str, Any]]) -> None:
        """Adds cookies to this context."""
        if self._closed:
            raise Error("browser_context.add_cookies: the context is closed")
        await self._ctx.add_cookies(cookies)

    async def clear_cookies(self) -> None:
        """Clears every cookie in this context."""
        if self._closed:
            raise Error("browser_context.clear_cookies: the context is closed")
        await self._ctx.clear_cookies()

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
        engine = await _native.Engine.launch(
            max_browsers=max_browsers,
            executable_path=executable_path,
            headless=headless,
            queue_timeout=queue_timeout,
            memory_ceiling_mb=memory_ceiling_mb,
            max_concurrency_per_host=max_concurrency_per_host,
            min_delay_per_host=min_delay_per_host,
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
