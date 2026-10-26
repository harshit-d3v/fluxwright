"""The blocking API, shaped like Playwright's ``playwright.sync_api``::

    from fluxwright.sync_api import sync_playwright

    with sync_playwright() as p:
        browser = p.chromium.launch(max_browsers=4)
        page = browser.new_page()
        page.goto("https://example.com")
        print(page.get_by_role("heading").text_content())
        browser.close()

Every call runs on one event loop in a background thread, so the objects work from any thread:
a thread pool of jobs sharing one browser is fine. Route handlers run on worker threads; event
listeners run one at a time, in order, on a thread of their own.
"""

from __future__ import annotations

import asyncio
import concurrent.futures
import logging
import threading
from contextlib import contextmanager
from os import PathLike
from pathlib import Path
from typing import Any, Generic, TypeVar
from collections.abc import Awaitable, Callable, Iterator

from . import _native
from . import async_api as _a
from ._errors import Error, TimeoutError
from ._native import ConsoleMessage
from ._types import FloatRect, Geolocation, ProxySettings, StorageState, URLMatch, ViewportSize

__all__ = [
    "Browser",
    "BrowserContext",
    "BrowserType",
    "ConsoleMessage",
    "Dialog",
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
    "Response",
    "Route",
    "StorageState",
    "TimeoutError",
    "ViewportSize",
    "chromium",
    "sync_playwright",
]

_log = logging.getLogger("fluxwright")
T = TypeVar("T")

_lock = threading.Lock()
_loop: asyncio.AbstractEventLoop | None = None
_thread: threading.Thread | None = None
# Event listeners run here, one at a time and in order. Route handlers use the loop's default
# pool, so one waiting on the page cannot hold up another request.
_listeners = concurrent.futures.ThreadPoolExecutor(1, thread_name_prefix="fluxwright-listeners")


def _event_loop() -> asyncio.AbstractEventLoop:
    global _loop, _thread
    with _lock:
        if _loop is None:
            _loop = asyncio.new_event_loop()
            _thread = threading.Thread(target=_loop.run_forever, name="fluxwright", daemon=True)
            _thread.start()
        return _loop


def _submit(fn: Callable[..., Awaitable[T]], *args: Any, **kwargs: Any) -> concurrent.futures.Future[T]:
    """Calls ``fn`` on the event loop's thread and awaits what it returns there."""
    if threading.current_thread() is _thread:
        raise Error("the sync API can't be called on fluxwright's event loop thread; use fluxwright.async_api there")

    async def call() -> T:
        return await fn(*args, **kwargs)

    return asyncio.run_coroutine_threadsafe(call(), _event_loop())


async def _on_loop(fn: Callable[..., T], *args: Any) -> T:
    """Calls a plain function on the event loop's thread, for the ones that start tasks there."""
    return fn(*args)


def _wait(future: concurrent.futures.Future[T]) -> T:
    # A short timed wait, so Ctrl+C lands within a moment on Windows too, and stops the call.
    try:
        while not future.done():
            concurrent.futures.wait([future], timeout=0.2)
    except BaseException:
        future.cancel()
        raise
    return future.result()


def _run(fn: Callable[..., Awaitable[T]], *args: Any, **kwargs: Any) -> T:
    return _wait(_submit(fn, *args, **kwargs))


async def _await(awaitable: Awaitable[T]) -> T:
    return await awaitable


def _to_sync(arg: Any) -> Any:
    """An async API object as its blocking counterpart, for a listener or predicate."""
    if isinstance(arg, _a.Response):
        return Response(arg)
    if isinstance(arg, _a.Request):
        return Request(arg)
    if isinstance(arg, _a.Dialog):
        return Dialog(arg)
    return arg


def _listen(handler: Callable[[Any], Any], arg: Any) -> None:
    try:
        handler(arg)
    except Exception:
        _log.exception("a page.on listener failed")


class _Finders:
    """The ways to find elements that pages, locators and frames share."""

    _impl: Any

    def locator(self, selector: str) -> Locator:
        """CSS, ``text=``, or ``role=`` selector."""
        return Locator(self._impl.locator(selector))

    def get_by_text(self, text: str, *, exact: bool | None = None) -> Locator:
        return Locator(self._impl.get_by_text(text, exact=exact))

    def get_by_role(self, role: str, *, name: str | None = None, exact: bool | None = None) -> Locator:
        return Locator(self._impl.get_by_role(role, name=name, exact=exact))

    def get_by_label(self, text: str, *, exact: bool | None = None) -> Locator:
        return Locator(self._impl.get_by_label(text, exact=exact))

    def get_by_placeholder(self, text: str, *, exact: bool | None = None) -> Locator:
        return Locator(self._impl.get_by_placeholder(text, exact=exact))

    def get_by_test_id(self, test_id: str) -> Locator:
        return Locator(self._impl.get_by_test_id(test_id))


class Locator(_Finders):
    """Lazy, like Playwright's: every action runs the query again."""

    _impl: _native.Locator

    def __init__(self, impl: _native.Locator) -> None:
        self._impl = impl

    def __repr__(self) -> str:
        return repr(self._impl)

    def click(self) -> None:
        _run(self._impl.click)

    def fill(self, value: str) -> None:
        _run(self._impl.fill, value)

    def wait_for(self) -> None:
        """Waits until visible."""
        _run(self._impl.wait_for)

    def text_content(self) -> str | None:
        return _run(self._impl.text_content)

    def bounding_box(self) -> FloatRect | None:
        """Relative to the viewport, without scrolling; ``None`` when the element is not visible."""
        return _run(self._impl.bounding_box)

    def screenshot(self, *, path: str | PathLike[str] | None = None) -> bytes:
        """PNG of the element, once it is visible and still; with ``path``, also written there."""
        return _run(self._impl.screenshot, path=path)

    def evaluate(self, expression: str, arg: Any = None) -> Any:
        """Calls the JavaScript function ``expression`` with the element and ``arg``."""
        return _run(self._impl.evaluate, expression, arg)

    def nth(self, index: int) -> Locator:
        """The ``index``-th match (0-based; negative counts from the end)."""
        return Locator(self._impl.nth(index))

    @property
    def first(self) -> Locator:
        return Locator(self._impl.first)

    @property
    def last(self) -> Locator:
        return Locator(self._impl.last)

    def filter(self, *, has_text: str | None = None) -> Locator:
        """Keeps matches containing ``has_text`` (case-insensitive substring)."""
        return Locator(self._impl.filter(has_text=has_text))


class FrameLocator(_Finders):
    """An iframe, same- or cross-origin, to find elements in."""

    _impl: _native.FrameLocator

    def __init__(self, impl: _native.FrameLocator) -> None:
        self._impl = impl

    def frame_locator(self, selector: str) -> FrameLocator:
        """A nested iframe inside this one."""
        return FrameLocator(self._impl.frame_locator(selector))


class Download:
    """A file the page downloaded; deleted when the page closes, so ``save_as`` it first."""

    def __init__(self, impl: _native.Download) -> None:
        self._impl = impl

    def __repr__(self) -> str:
        return repr(self._impl)

    @property
    def url(self) -> str:
        return self._impl.url

    @property
    def suggested_filename(self) -> str:
        """From ``Content-Disposition`` or the URL."""
        return self._impl.suggested_filename

    def path(self) -> Path:
        return Path(_run(self._impl.path))

    def save_as(self, path: str | PathLike[str]) -> None:
        """Copies the file to ``path``, creating missing folders."""
        _run(self._impl.save_as, path)

    def failure(self) -> str | None:
        return _run(self._impl.failure)


class Request:
    """A request as a route handler sees it."""

    def __init__(self, impl: _a.Request) -> None:
        self._impl = impl

    def __repr__(self) -> str:
        return repr(self._impl)

    @property
    def url(self) -> str:
        return self._impl.url

    @property
    def method(self) -> str:
        return self._impl.method

    @property
    def headers(self) -> dict[str, str]:
        return self._impl.headers

    @property
    def post_data(self) -> str | None:
        return self._impl.post_data

    @property
    def resource_type(self) -> str:
        return self._impl.resource_type

    @property
    def failure(self) -> str | None:
        """Why the request failed (``net::ERR_FAILED``, ...), for ``requestfailed``."""
        return self._impl.failure


class Response:
    """A response, as ``page.on("response")`` and ``expect_response`` hand it over."""

    def __init__(self, impl: _a.Response) -> None:
        self._impl = impl

    def __repr__(self) -> str:
        return repr(self._impl)

    @property
    def url(self) -> str:
        return self._impl.url

    @property
    def status(self) -> int:
        return self._impl.status

    @property
    def status_text(self) -> str:
        return self._impl.status_text

    @property
    def ok(self) -> bool:
        """Status 200 to 299."""
        return self._impl.ok

    @property
    def headers(self) -> dict[str, str]:
        """Names lower-cased."""
        return self._impl.headers

    @property
    def request(self) -> Request:
        return Request(self._impl.request)

    def body(self) -> bytes:
        """The body, once the response finished loading."""
        return _run(self._impl.body)

    def text(self) -> str:
        return _run(self._impl.text)

    def json(self) -> Any:
        return _run(self._impl.json)


class Dialog:
    """A JavaScript dialog, as ``page.on("dialog")`` hands it over. Answer it with ``accept`` or
    ``dismiss``; one no listener answered is dismissed, so the page never hangs on it."""

    def __init__(self, impl: _a.Dialog) -> None:
        self._impl = impl

    def __repr__(self) -> str:
        return repr(self._impl)

    @property
    def type(self) -> str:
        """``alert``, ``confirm``, ``prompt`` or ``beforeunload``."""
        return self._impl.type

    @property
    def message(self) -> str:
        return self._impl.message

    @property
    def default_value(self) -> str:
        return self._impl.default_value

    def accept(self, prompt_text: str | None = None) -> None:
        """``prompt_text`` is what a ``prompt`` returns."""
        _run(self._impl.accept, prompt_text)

    def dismiss(self) -> None:
        _run(self._impl.dismiss)


class Route:
    """How a route handler answers a request, as Playwright's ``Route``. Each request takes one
    answer."""

    def __init__(self, impl: _a.Route) -> None:
        self._impl = impl

    @property
    def request(self) -> Request:
        return Request(self._impl.request)

    def fulfill(
        self,
        *,
        status: int | None = None,
        headers: dict[str, str] | None = None,
        body: str | bytes | None = None,
        json: Any = None,
        path: str | PathLike[str] | None = None,
        content_type: str | None = None,
    ) -> None:
        """A made-up response. ``json`` is serialized and sets the content type; ``path`` is a
        file to send."""
        _run(
            self._impl.fulfill,
            status=status,
            headers=headers,
            body=body,
            json=json,
            path=path,
            content_type=content_type,
        )

    def continue_(
        self,
        *,
        url: str | None = None,
        method: str | None = None,
        headers: dict[str, str] | None = None,
        post_data: str | bytes | None = None,
    ) -> None:
        """Sends the request on, changed if asked; ``headers`` replaces all of them."""
        _run(self._impl.continue_, url=url, method=method, headers=headers, post_data=post_data)

    def abort(self, error_code: str | None = None) -> None:
        """Fails the request: ``failed`` (the default), ``aborted``, ``blockedbyclient``, ..."""
        _run(self._impl.abort, error_code)

    def fallback(self) -> None:
        """Leaves the request to the next matching handler: the one added before this one."""
        _run(self._impl.fallback)


class EventInfo(Generic[T]):
    """What ``expect_download`` yields: ``info.value`` is the download."""

    def __init__(self, future: concurrent.futures.Future[Any], wrap: Callable[[Any], T]) -> None:
        self._future = future
        self._wrap = wrap

    @property
    def value(self) -> T:
        return self._wrap(_wait(self._future))

    def is_done(self) -> bool:
        return self._future.done()


class Page(_Finders):
    """A browser tab in a fresh browser context, leased from the engine. Close it when the job is
    done."""

    _impl: _a.Page

    def __init__(self, impl: _a.Page) -> None:
        self._impl = impl
        self._route_bridges: list[tuple[URLMatch, Callable[..., Any], Callable[..., Any]]] = []
        self._listener_bridges: list[tuple[str, Callable[[Any], Any], Callable[[Any], Any]]] = []

    def goto(self, url: str, *, wait_until: str | None = None) -> None:
        """Navigates; ``wait_until`` is ``load`` (the default), ``domcontentloaded``,
        ``networkidle`` or ``commit``."""
        _run(self._impl.goto, url, wait_until=wait_until)

    def title(self) -> str:
        return _run(self._impl.title)

    def content(self) -> str:
        return _run(self._impl.content)

    def click(self, selector: str) -> None:
        """Scrolls into view and waits until the element is enabled, stable and not covered."""
        _run(self._impl.click, selector)

    def fill(self, selector: str, value: str) -> None:
        _run(self._impl.fill, selector, value)

    def wait_for_selector(self, selector: str) -> None:
        """Waits until visible."""
        _run(self._impl.wait_for_selector, selector)

    def evaluate(self, expression: str, arg: Any = None) -> Any:
        """Runs JavaScript in the page and returns its JSON result, awaiting promises. A function
        (``"(x) => x * 2"``) is called with ``arg``; anything else is evaluated as is."""
        return _run(self._impl.evaluate, expression, arg)

    def screenshot(self, *, path: str | PathLike[str] | None = None, full_page: bool | None = None) -> bytes:
        """PNG; with ``path``, also written there."""
        return _run(self._impl.screenshot, path=path, full_page=full_page)

    def set_viewport_size(self, viewport_size: ViewportSize) -> None:
        _run(self._impl.set_viewport_size, viewport_size)

    def storage_state(self, *, path: str | PathLike[str] | None = None) -> StorageState:
        """Every cookie in this page's context plus localStorage of the current origin; with
        ``path``, also saved as JSON for ``new_page(storage_state=path)``."""
        return _run(self._impl.storage_state, path=path)

    def console_messages(self) -> list[ConsoleMessage]:
        """What the page, its popups and iframes logged so far (the last 1000)."""
        return _run(self._impl.console_messages)

    def page_errors(self) -> list[Error]:
        """The exceptions nothing caught so far (the last 1000)."""
        return _run(self._impl.page_errors)

    def frame_locator(self, selector: str) -> FrameLocator:
        return FrameLocator(self._impl.frame_locator(selector))

    def route(self, url: URLMatch, handler: Callable[..., Any]) -> None:
        """Hands matching requests to ``handler(route)`` or ``handler(route, request)``, on a
        worker thread, as Playwright's ``page.route``. The handler added last runs first; an
        unanswered request continues. Call it before navigating."""
        loop = _event_loop()
        with_request = _a._takes_request(handler)

        def bridge(route: _a.Route, request: _a.Request) -> Any:
            args = (Route(route), Request(request)) if with_request else (Route(route),)
            return loop.run_in_executor(None, lambda: handler(*args))

        self._route_bridges.append((url, handler, bridge))
        _run(self._impl.route, url, bridge)

    def unroute(self, url: URLMatch, handler: Callable[..., Any] | None = None) -> None:
        """Removes the handlers added for ``url`` (only ``handler``, when given)."""
        for entry in list(self._route_bridges):
            if entry[0] == url and (handler is None or entry[1] == handler):
                self._route_bridges.remove(entry)
                _run(self._impl.unroute, url, entry[2])

    def on(self, event: str, handler: Callable[[Any], Any]) -> None:
        """Calls ``handler`` from now on with each ``"console"`` message (a ``ConsoleMessage``),
        ``"pageerror"`` (an ``Error``), ``"request"``, ``"requestfinished"`` and
        ``"requestfailed"`` (a ``Request``), ``"response"`` (a ``Response``) or ``"dialog"`` (a
        ``Dialog``, to accept or dismiss)."""
        self._listen("on", event, handler)

    def once(self, event: str, handler: Callable[[Any], Any]) -> None:
        """``on``, for the next event only."""
        self._listen("once", event, handler)

    def remove_listener(self, event: str, handler: Callable[[Any], Any]) -> None:
        for entry in list(self._listener_bridges):
            if entry[0] == event and entry[1] == handler:
                self._listener_bridges.remove(entry)
                _run(_on_loop, self._impl.remove_listener, event, entry[2])
                return

    off = remove_listener

    @contextmanager
    def expect_download(self, timeout: float | None = None) -> Iterator[EventInfo[Download]]:
        """Waits for the download the code in the ``with`` block starts (``timeout`` in
        milliseconds, default 30 s), as Playwright's ``page.expect_download``::

            with page.expect_download() as info:
                page.get_by_text("Export").click()
            download = info.value
        """
        future = _submit(self._impl.wait_for_download, timeout=timeout)
        info = EventInfo(future, Download)
        try:
            yield info
        except BaseException:
            future.cancel()
            raise
        _wait(future)  # as Playwright does: a failed or late download raises here

    @contextmanager
    def expect_response(
        self, url_or_predicate: URLMatch | Callable[[Response], bool], *, timeout: float | None = None
    ) -> Iterator[EventInfo[Response]]:
        """Waits for the first response from now on whose URL matches (a glob or regex, as in
        ``route``) or that the predicate accepts (``timeout`` in milliseconds, default 30 s), as
        Playwright's ``page.expect_response``::

            with page.expect_response("**/api/items") as info:
                page.get_by_text("Load").click()
            items = info.value.json()
        """
        with self._expect(self._impl.expect_response, url_or_predicate, timeout, Response) as info:
            yield info

    @contextmanager
    def expect_request(
        self, url_or_predicate: URLMatch | Callable[[Request], bool], *, timeout: float | None = None
    ) -> Iterator[EventInfo[Request]]:
        """``expect_response``, for requests."""
        with self._expect(self._impl.expect_request, url_or_predicate, timeout, Request) as info:
            yield info

    @contextmanager
    def _expect(
        self, start: Callable[..., Any], url_or_predicate: Any, timeout: float | None, wrap: Callable[[Any], T]
    ) -> Iterator[EventInfo[T]]:
        if callable(url_or_predicate):
            predicate = url_or_predicate

            def matcher(r: Any) -> bool:
                return bool(predicate(_to_sync(r)))
        else:
            matcher = url_or_predicate
        manager = start(matcher, timeout=timeout)
        info = _run(manager.__aenter__)
        future = asyncio.run_coroutine_threadsafe(_await(info.value), _event_loop())
        try:
            yield EventInfo(future, wrap)
        except BaseException as e:
            try:
                _run(manager.__aexit__, type(e), e, e.__traceback__)
            finally:
                future.cancel()
            raise
        _run(manager.__aexit__, None, None, None)

    def wait_for_download(self, *, timeout: float | None = None) -> Download:
        """The next download this page finished; ones that finished earlier queue up."""
        return Download(_run(self._impl.wait_for_download, timeout=timeout))

    def close(self) -> None:
        _run(self._impl.close)

    def is_closed(self) -> bool:
        return self._impl.is_closed()

    def _listen(self, method: str, event: str, handler: Callable[[Any], Any]) -> None:
        loop = _event_loop()

        # Dialog listeners run on the loop's own pool: on the one-at-a-time listener thread, a
        # slow listener on another page held up the answer, and the click that opened the dialog
        # timed out. Each page's dialogs still come one at a time.
        executor = None if event == "dialog" else _listeners

        def bridge(arg: Any) -> Any:
            # A future, so the dialog loop waits for the handler's answer.
            return loop.run_in_executor(executor, _listen, handler, _to_sync(arg))

        _run(_on_loop, getattr(self._impl, method), event, bridge)
        self._listener_bridges.append((event, handler, bridge))


class Browser:
    """The engine: a pool of Chrome processes that pages are leased from."""

    def __init__(self, impl: _a.Browser) -> None:
        self._impl = impl

    def new_page(
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
        Playwright's names."""
        return Page(
            _run(
                self._impl.new_page,
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
        )

    def new_context(
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
    ) -> BrowserContext:
        """A browser context with ``new_page``'s options, as Playwright's ``new_context``. Each
        context holds one page; see ``BrowserContext``."""
        options = dict(
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
        return BrowserContext(self, options)

    def close(self) -> None:
        """Shuts the engine down, and with it every page and Chrome process it started."""
        _run(self._impl.close)

    def __enter__(self) -> Browser:
        return self

    def __exit__(self, *exc: Any) -> None:
        self.close()


class BrowserContext:
    """A browser context, as Playwright's: the options its page opens with.

    Each context holds one page, so one job's cookies and storage never reach another:
    ``new_page()`` opens it and a second call raises ``Error``. To share a login across pages,
    pass one page's ``storage_state()`` to the next ``new_context``.
    """

    def __init__(self, browser: Browser, options: dict[str, Any]) -> None:
        self._browser = browser
        self._options = options
        self._page: Page | None = None
        self._lock = threading.Lock()
        self._opening = False
        self._closed = False
        self._routes: list[tuple[URLMatch, Callable[..., Any]]] = []

    @property
    def browser(self) -> Browser:
        return self._browser

    @property
    def pages(self) -> list[Page]:
        return [self._page] if self._page is not None and not self._page.is_closed() else []

    def new_page(self) -> Page:
        with self._lock:
            if self._closed:
                raise Error("browser_context.new_page: the context is closed")
            if self._page is not None or self._opening:
                raise Error(f"browser_context.new_page: {_a.ONE_PAGE}")
            self._opening = True
        try:
            page = self._browser.new_page(**self._options)
            try:
                for url, handler in self._routes:
                    page.route(url, handler)
                # close() from another thread while the page was coming: it would otherwise stay
                # open on a closed context. Checked and stored under the lock close() takes.
                with self._lock:
                    if not self._closed:
                        self._page = page
                if self._page is not page:
                    raise Error("browser_context.new_page: the context is closed")
            except BaseException:
                page.close()
                raise
            return page
        finally:
            self._opening = False

    def route(self, url: URLMatch, handler: Callable[..., Any]) -> None:
        """``page.route`` for this context's page; added before ``new_page()``, it applies from
        the page's first request."""
        self._routes.append((url, handler))
        if self._page is not None:
            self._page.route(url, handler)

    def unroute(self, url: URLMatch, handler: Callable[..., Any] | None = None) -> None:
        self._routes = [r for r in self._routes if not (r[0] == url and (handler is None or r[1] == handler))]
        if self._page is not None:
            self._page.unroute(url, handler)

    def storage_state(self, *, path: str | PathLike[str] | None = None) -> StorageState:
        """The cookies and localStorage of this context's page, as ``page.storage_state``."""
        if self._page is None:
            raise Error("browser_context.storage_state: call new_page() first")
        return self._page.storage_state(path=path)

    def cookies(self) -> list[dict[str, Any]]:
        """Every cookie in this context."""
        cookies: list[dict[str, Any]] = self.storage_state()["cookies"]
        return cookies

    def close(self) -> None:
        """Closes the context's page; the context opens no other."""
        with self._lock:
            self._closed = True
            page = self._page
        if page is not None:
            page.close()

    def __enter__(self) -> BrowserContext:
        return self

    def __exit__(self, *exc: Any) -> None:
        self.close()


class BrowserType:
    """``chromium``: starts the engine."""

    name = "chromium"

    def __init__(self, impl: _a.BrowserType) -> None:
        self._impl = impl

    def launch(
        self,
        *,
        headless: bool | None = None,
        executable_path: str | PathLike[str] | None = None,
        max_browsers: int | None = None,
        queue_timeout: float | None = None,
        memory_ceiling_mb: int | None = None,
    ) -> Browser:
        """Starts the engine in this process: up to ``max_browsers`` Chrome processes (default
        one per 4 CPUs, 8 pages each), headless unless ``headless=False``. New pages wait while
        all Chrome processes together use more than ``memory_ceiling_mb`` (default 75% of the
        memory free at launch). When every slot is busy, ``new_page`` waits its turn however long
        the queue is; with ``queue_timeout`` (milliseconds) it raises ``TimeoutError`` after that
        long instead, counting a browser start when one is needed."""
        return Browser(
            _run(
                self._impl.launch,
                headless=headless,
                executable_path=executable_path,
                max_browsers=max_browsers,
                queue_timeout=queue_timeout,
                memory_ceiling_mb=memory_ceiling_mb,
            )
        )


class Playwright:
    """What ``sync_playwright()`` gives: ``p.chromium``. Browsers launched through it close
    when it stops."""

    def __init__(self) -> None:
        self._impl = _a.Playwright()
        self.chromium = BrowserType(self._impl.chromium)

    def stop(self) -> None:
        _run(self._impl.stop)


class PlaywrightContextManager:
    def __init__(self) -> None:
        self._playwright: Playwright | None = None

    def start(self) -> Playwright:
        self._playwright = Playwright()
        return self._playwright

    def __enter__(self) -> Playwright:
        return self.start()

    def __exit__(self, *exc: Any) -> None:
        if self._playwright is not None:
            self._playwright.stop()


def sync_playwright() -> PlaywrightContextManager:
    """Playwright's entry point, so a Playwright script runs after changing only its import."""
    return PlaywrightContextManager()


#: Launches the engine directly: ``browser = chromium.launch()``.
chromium = BrowserType(_a.chromium)
