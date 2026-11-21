"""The blocking API, shaped like Playwright's ``playwright.sync_api``::

    from fluxwright.sync_api import sync_playwright

    with sync_playwright() as p:
        browser = p.chromium.launch(max_browsers=4)
        page = browser.new_page()
        page.goto("https://example.com")
        print(page.get_by_role("heading").text_content())
        browser.close()

Every call runs on one event loop in a background thread, so the objects work from any thread:
a thread pool of jobs sharing one browser is fine. Route handlers run on worker threads; each
page's event listeners run one at a time, in order, on a thread of that page's own.
"""

from __future__ import annotations

import asyncio
import concurrent.futures
import logging
import re
import threading
import weakref
from contextlib import contextmanager, suppress
from os import PathLike
from pathlib import Path
from typing import Any, Generic, Literal, TypeVar
from collections.abc import Awaitable, Callable, Iterator, Mapping, Sequence

from . import _native
from . import async_api as _a
from ._errors import Error, TimeoutError
from ._native import ConsoleMessage
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
    "WebSocket",
    "Worker",
    "chromium",
    "sync_playwright",
]

_log = logging.getLogger("fluxwright")
T = TypeVar("T")

_lock = threading.Lock()
_loop: asyncio.AbstractEventLoop | None = None
_thread: threading.Thread | None = None


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


_sync_pages: weakref.WeakKeyDictionary[_a.Page, Page] = weakref.WeakKeyDictionary()
_sync_websockets: weakref.WeakKeyDictionary[_a.WebSocket, WebSocket] = weakref.WeakKeyDictionary()
_sync_workers: weakref.WeakKeyDictionary[_a.Worker, Worker] = weakref.WeakKeyDictionary()


def _wrap_page(impl: _a.Page) -> Page:
    page = _sync_pages.get(impl)
    return page if page is not None else Page(impl)


def _page_listener_thread(async_page: _a.Page | None) -> concurrent.futures.ThreadPoolExecutor | None:
    if async_page is None:
        return None
    sync_page = _sync_pages.get(async_page)
    return sync_page._listener_thread if sync_page is not None else None


def _wrap_websocket(impl: _a.WebSocket) -> WebSocket:
    ws = _sync_websockets.get(impl)
    return ws if ws is not None else WebSocket(impl)


def _wrap_worker(impl: _a.Worker) -> Worker:
    w = _sync_workers.get(impl)
    return w if w is not None else Worker(impl)


def _to_sync(arg: Any) -> Any:
    """An async API object as its blocking counterpart, for a listener or predicate."""
    if isinstance(arg, _a.Page):
        return _wrap_page(arg)
    if isinstance(arg, _a.WebSocket):
        return _wrap_websocket(arg)
    if isinstance(arg, _a.Worker):
        return _wrap_worker(arg)
    if isinstance(arg, _a.Response):
        return Response(arg)
    if isinstance(arg, _a.Request):
        return Request(arg)
    if isinstance(arg, _a.Dialog):
        return Dialog(arg)
    if isinstance(arg, _a.FileChooser):
        return FileChooser(arg)
    return arg


@contextmanager
def _sync_expect(
    start: Callable[..., Any], url_or_predicate: Any, timeout: float | None, wrap: Callable[[Any], T]
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


def _bridge_route(
    bridges: list[tuple[URLMatch, Callable[..., Any], Callable[..., Any]]],
    async_target: Any,
    url: URLMatch,
    handler: Callable[..., Any],
) -> None:
    loop = _event_loop()
    with_request = _a._takes_request(handler)

    def bridge(route: _a.Route, request: _a.Request) -> Any:
        args = (Route(route), Request(request)) if with_request else (Route(route),)
        # The loop's default pool: a handler waiting on the page holds up no other request.
        return loop.run_in_executor(None, lambda: handler(*args))

    bridges.append((url, handler, bridge))
    _run(async_target.route, url, bridge)


def _unbridge_route(
    bridges: list[tuple[URLMatch, Callable[..., Any], Callable[..., Any]]],
    async_target: Any,
    url: URLMatch,
    handler: Callable[..., Any] | None = None,
) -> None:
    for entry in list(bridges):
        if entry[0] == url and (handler is None or entry[1] == handler):
            bridges.remove(entry)
            _run(async_target.unroute, url, entry[2])


def _bridge_expose(async_target: Any, label: str, name: str, callback: Callable[..., Any]) -> None:
    if not callable(callback):
        raise TypeError(f"{label}: callback must be callable")
    loop = _event_loop()

    def bridge(*args: Any) -> Any:
        return loop.run_in_executor(None, lambda: callback(*args))

    _run(async_target.expose_function, name, bridge)


def _listen(handler: Callable[[Any], Any], arg: Any) -> None:
    try:
        handler(arg)
    except Exception:
        _log.exception("a page.on listener failed")


def _native_loc(loc: Locator | Any) -> Any:
    return loc._impl if isinstance(loc, Locator) else loc


class _Finders:
    """The ways to find elements that pages, locators and frames share."""

    _impl: Any

    def locator(
        self,
        selector: str,
        *,
        has: Locator | None = None,
        has_not: Locator | None = None,
        has_text: str | re.Pattern[str] | None = None,
        has_not_text: str | re.Pattern[str] | None = None,
    ) -> Locator:
        """CSS, ``text=``, ``xpath=``, or ``role=`` selector."""
        return Locator(
            self._impl.locator(
                selector,
                has=_native_loc(has) if has is not None else None,
                has_not=_native_loc(has_not) if has_not is not None else None,
                has_text=has_text,
                has_not_text=has_not_text,
            )
        )

    def get_by_text(self, text: str, *, exact: bool | None = None) -> Locator:
        return Locator(self._impl.get_by_text(text, exact=exact))

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
        return Locator(
            self._impl.get_by_role(
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
        )

    def get_by_label(self, text: str, *, exact: bool | None = None) -> Locator:
        return Locator(self._impl.get_by_label(text, exact=exact))

    def get_by_placeholder(self, text: str, *, exact: bool | None = None) -> Locator:
        return Locator(self._impl.get_by_placeholder(text, exact=exact))

    def get_by_alt_text(self, text: str, *, exact: bool | None = None) -> Locator:
        return Locator(self._impl.get_by_alt_text(text, exact=exact))

    def get_by_title(self, text: str, *, exact: bool | None = None) -> Locator:
        return Locator(self._impl.get_by_title(text, exact=exact))

    def get_by_test_id(self, test_id: str) -> Locator:
        return Locator(self._impl.get_by_test_id(test_id))


class Locator(_Finders):
    """Lazy, like Playwright's: every action runs the query again."""

    _impl: _native.Locator

    def __init__(self, impl: _native.Locator) -> None:
        self._impl = impl

    def __repr__(self) -> str:
        return repr(self._impl)

    def click(
        self,
        *,
        button: Literal["left", "right", "middle"] | None = None,
        click_count: int | None = None,
        delay: float | None = None,
        position: Position | None = None,
        modifiers: Sequence[Literal["Alt", "Control", "Meta", "Shift", "ControlOrMeta"]] | None = None,
        force: bool | None = None,
        trial: bool | None = None,
        timeout: float | None = None,
    ) -> None:
        """``timeout`` is in milliseconds, for this call only; 0 waits as long as it takes."""
        _run(
            self._impl.click,
            button=button,
            click_count=click_count,
            delay=delay,
            position=position,
            modifiers=list(modifiers) if modifiers is not None else None,
            force=force,
            trial=trial,
            timeout=timeout,
        )

    def dblclick(
        self,
        *,
        button: Literal["left", "right", "middle"] | None = None,
        delay: float | None = None,
        position: Position | None = None,
        modifiers: Sequence[Literal["Alt", "Control", "Meta", "Shift", "ControlOrMeta"]] | None = None,
        force: bool | None = None,
        trial: bool | None = None,
        timeout: float | None = None,
    ) -> None:
        _run(
            self._impl.dblclick,
            button=button,
            delay=delay,
            position=position,
            modifiers=list(modifiers) if modifiers is not None else None,
            force=force,
            trial=trial,
            timeout=timeout,
        )

    def hover(
        self,
        *,
        position: Position | None = None,
        modifiers: Sequence[Literal["Alt", "Control", "Meta", "Shift", "ControlOrMeta"]] | None = None,
        force: bool | None = None,
        trial: bool | None = None,
        timeout: float | None = None,
    ) -> None:
        _run(
            self._impl.hover,
            position=position,
            modifiers=list(modifiers) if modifiers is not None else None,
            force=force,
            trial=trial,
            timeout=timeout,
        )

    def tap(
        self,
        *,
        position: Position | None = None,
        modifiers: Sequence[Literal["Alt", "Control", "Meta", "Shift", "ControlOrMeta"]] | None = None,
        force: bool | None = None,
        trial: bool | None = None,
        timeout: float | None = None,
    ) -> None:
        _run(
            self._impl.tap,
            position=position,
            modifiers=list(modifiers) if modifiers is not None else None,
            force=force,
            trial=trial,
            timeout=timeout,
        )

    def focus(self, *, timeout: float | None = None) -> None:
        _run(self._impl.focus, timeout=timeout)

    def blur(self, *, timeout: float | None = None) -> None:
        _run(self._impl.blur, timeout=timeout)

    def scroll_into_view_if_needed(self, *, timeout: float | None = None) -> None:
        _run(self._impl.scroll_into_view_if_needed, timeout=timeout)

    def drag_to(self, target: Locator, *, force: bool | None = None, timeout: float | None = None) -> None:
        _run(self._impl.drag_to, target._impl, force=force, timeout=timeout)

    def fill(self, value: str, *, timeout: float | None = None) -> None:
        _run(self._impl.fill, value, timeout=timeout)

    def clear(self, *, force: bool | None = None, timeout: float | None = None) -> None:
        _run(self._impl.clear, force=force, timeout=timeout)

    def press(self, key: str, *, delay: float | None = None, timeout: float | None = None) -> None:
        _run(self._impl.press, key, delay=delay, timeout=timeout)

    def press_sequentially(self, text: str, *, delay: float | None = None, timeout: float | None = None) -> None:
        _run(self._impl.press_sequentially, text, delay=delay, timeout=timeout)

    def type(self, text: str, *, delay: float | None = None, timeout: float | None = None) -> None:
        _run(self._impl.type, text, delay=delay, timeout=timeout)

    def check(
        self,
        *,
        position: Position | None = None,
        force: bool | None = None,
        trial: bool | None = None,
        timeout: float | None = None,
    ) -> None:
        _run(self._impl.check, position=position, force=force, trial=trial, timeout=timeout)

    def uncheck(
        self,
        *,
        position: Position | None = None,
        force: bool | None = None,
        trial: bool | None = None,
        timeout: float | None = None,
    ) -> None:
        _run(self._impl.uncheck, position=position, force=force, trial=trial, timeout=timeout)

    def set_checked(
        self,
        checked: bool,
        *,
        position: Position | None = None,
        force: bool | None = None,
        trial: bool | None = None,
        timeout: float | None = None,
    ) -> None:
        _run(self._impl.set_checked, checked, position=position, force=force, trial=trial, timeout=timeout)

    def select_option(
        self,
        value: str | Sequence[str] | Mapping[str, Any] | Sequence[Mapping[str, Any]] | None = None,
        *,
        index: int | Sequence[int] | None = None,
        label: str | Sequence[str] | None = None,
        element: Any = None,
        force: bool | None = None,
        timeout: float | None = None,
    ) -> list[str]:
        return _run(
            self._impl.select_option,
            value,
            index=index,
            label=label,
            element=element,
            force=force,
            timeout=timeout,
        )

    def set_input_files(
        self,
        files: str | PathLike[str] | FilePayload | Sequence[str | PathLike[str] | FilePayload],
        *,
        timeout: float | None = None,
    ) -> None:
        _run(self._impl.set_input_files, files, timeout=timeout)

    def wait_for(
        self,
        *,
        state: Literal["attached", "detached", "visible", "hidden"] | None = None,
        timeout: float | None = None,
    ) -> None:
        """Waits until ``state``: visible by default."""
        _run(self._impl.wait_for, state=state, timeout=timeout)

    def text_content(self, *, timeout: float | None = None) -> str | None:
        return _run(self._impl.text_content, timeout=timeout)

    def inner_text(self, *, timeout: float | None = None) -> str:
        return _run(self._impl.inner_text, timeout=timeout)

    def inner_html(self, *, timeout: float | None = None) -> str:
        return _run(self._impl.inner_html, timeout=timeout)

    def input_value(self, *, timeout: float | None = None) -> str:
        return _run(self._impl.input_value, timeout=timeout)

    def get_attribute(self, name: str, *, timeout: float | None = None) -> str | None:
        return _run(self._impl.get_attribute, name, timeout=timeout)

    def count(self) -> int:
        return _run(self._impl.count)

    def all(self) -> list[Locator]:
        return [Locator(item) for item in _run(self._impl.all)]

    def all_text_contents(self) -> list[str]:
        return _run(self._impl.all_text_contents)

    def all_inner_texts(self) -> list[str]:
        return _run(self._impl.all_inner_texts)

    def is_visible(self, *, timeout: float | None = None) -> bool:
        return _run(self._impl.is_visible, timeout=timeout)

    def is_hidden(self, *, timeout: float | None = None) -> bool:
        return _run(self._impl.is_hidden, timeout=timeout)

    def is_enabled(self, *, timeout: float | None = None) -> bool:
        return _run(self._impl.is_enabled, timeout=timeout)

    def is_disabled(self, *, timeout: float | None = None) -> bool:
        return _run(self._impl.is_disabled, timeout=timeout)

    def is_checked(self, *, timeout: float | None = None) -> bool:
        return _run(self._impl.is_checked, timeout=timeout)

    def is_editable(self, *, timeout: float | None = None) -> bool:
        return _run(self._impl.is_editable, timeout=timeout)

    def bounding_box(self, *, timeout: float | None = None) -> FloatRect | None:
        """Relative to the viewport, without scrolling; ``None`` when the element is not visible."""
        return _run(self._impl.bounding_box, timeout=timeout)

    def screenshot(
        self,
        *,
        path: str | PathLike[str] | None = None,
        type: Literal["png", "jpeg"] | None = None,
        quality: int | None = None,
        omit_background: bool | None = None,
        timeout: float | None = None,
    ) -> bytes:
        """PNG or JPEG of the element, once it is visible and still; with ``path``, also written there."""
        return _run(
            self._impl.screenshot,
            path=path,
            type=type,
            quality=quality,
            omit_background=omit_background,
            timeout=timeout,
        )

    def evaluate(self, expression: str, arg: Any = None, *, timeout: float | None = None) -> Any:
        """Calls the JavaScript function ``expression`` with the element and ``arg``."""
        return _run(self._impl.evaluate, expression, arg, timeout=timeout)

    def nth(self, index: int) -> Locator:
        """The ``index``-th match (0-based; negative counts from the end)."""
        return Locator(self._impl.nth(index))

    @property
    def first(self) -> Locator:
        return Locator(self._impl.first)

    @property
    def last(self) -> Locator:
        return Locator(self._impl.last)

    def filter(
        self,
        *,
        has: Locator | None = None,
        has_not: Locator | None = None,
        has_text: str | re.Pattern[str] | None = None,
        has_not_text: str | re.Pattern[str] | None = None,
    ) -> Locator:
        """Narrow matching elements."""
        return Locator(
            self._impl.filter(
                has=_native_loc(has) if has is not None else None,
                has_not=_native_loc(has_not) if has_not is not None else None,
                has_text=has_text,
                has_not_text=has_not_text,
            )
        )

    def and_(self, locator: Locator) -> Locator:
        return Locator(self._impl.and_(_native_loc(locator)))

    def or_(self, locator: Locator) -> Locator:
        return Locator(self._impl.or_(_native_loc(locator)))


class FrameLocator(_Finders):
    """An iframe, same- or cross-origin, to find elements in."""

    _impl: _native.FrameLocator

    def __init__(self, impl: _native.FrameLocator) -> None:
        self._impl = impl

    def frame_locator(self, selector: str) -> FrameLocator:
        """A nested iframe inside this one."""
        return FrameLocator(self._impl.frame_locator(selector))


class Frame(_Finders):
    """An imperative handle to a main frame or iframe on a page."""

    _impl: _a.Frame

    def __init__(self, impl: _a.Frame) -> None:
        self._impl = impl

    @property
    def page(self) -> Page:
        return _wrap_page(self._impl.page)

    @property
    def url(self) -> str:
        return self._impl.url

    @property
    def name(self) -> str:
        return self._impl.name

    def is_detached(self) -> bool:
        return self._impl.is_detached()

    @property
    def parent_frame(self) -> Frame | None:
        p = self._impl.parent_frame
        return Frame(p) if p is not None else None

    @property
    def child_frames(self) -> list[Frame]:
        return [Frame(f) for f in self._impl.child_frames]

    def goto(self, url: str, *, wait_until: str | None = None, timeout: float | None = None) -> None:
        _run(self._impl.goto, url, wait_until=wait_until, timeout=timeout)

    def evaluate(self, expression: str, arg: Any = None) -> Any:
        return _run(self._impl.evaluate, expression, arg)

    def title(self) -> str:
        return _run(self._impl.title)

    def content(self) -> str:
        return _run(self._impl.content)

    def frame_locator(self, selector: str) -> FrameLocator:
        return FrameLocator(self._impl.frame_locator(selector))


class CDPSession:
    """Raw Chrome DevTools Protocol session attached via ``context.new_cdp_session(page)``."""

    def __init__(self, impl: _a.CDPSession) -> None:
        self._impl = impl
        self._listener_bridges: list[tuple[str, Callable[[Any], Any], Callable[[Any], Any]]] = []
        self._listener_thread = concurrent.futures.ThreadPoolExecutor(1, thread_name_prefix="fluxwright-cdp-listeners")

    def send(self, method: str, params: dict[str, Any] | None = None) -> Any:
        return _run(self._impl.send, method, params)

    def on(self, event: str, handler: Callable[[Any], Any]) -> None:
        self._listen("on", event, handler)

    def once(self, event: str, handler: Callable[[Any], Any]) -> None:
        self._listen("once", event, handler)

    def remove_listener(self, event: str, handler: Callable[[Any], Any]) -> None:
        for entry in list(self._listener_bridges):
            if entry[0] == event and entry[1] == handler:
                with suppress(ValueError):
                    self._listener_bridges.remove(entry)
                _run(_on_loop, self._impl.remove_listener, event, entry[2])
                return

    off = remove_listener

    def detach(self) -> None:
        _run(self._impl.detach)
        self._listener_thread.shutdown(wait=False)

    def _listen(self, method: str, event: str, handler: Callable[[Any], Any]) -> None:
        loop = _event_loop()
        bridges = self._listener_bridges

        def bridge(arg: Any) -> Any:
            if method == "once":
                with suppress(ValueError):
                    bridges.remove(entry)
            try:
                return loop.run_in_executor(self._listener_thread, _listen, handler, arg)
            except RuntimeError:
                return None

        entry = (event, handler, bridge)
        bridges.append(entry)
        try:
            _run(_on_loop, getattr(self._impl, method), event, bridge)
        except BaseException:
            with suppress(ValueError):
                bridges.remove(entry)
            raise


class _SyncChildEmitter:
    """Shared listener bridging for sync ``WebSocket`` and ``Worker``."""

    _impl: Any
    _listener_bridges: list[tuple[str, Callable[[Any], Any], Callable[[Any], Any]]]

    def on(self, event: str, handler: Callable[[Any], Any]) -> None:
        self._listen("on", event, handler)

    def once(self, event: str, handler: Callable[[Any], Any]) -> None:
        self._listen("once", event, handler)

    def remove_listener(self, event: str, handler: Callable[[Any], Any]) -> None:
        for entry in list(self._listener_bridges):
            if entry[0] == event and entry[1] == handler:
                with suppress(ValueError):
                    self._listener_bridges.remove(entry)
                _run(_on_loop, self._impl.remove_listener, event, entry[2])
                return

    off = remove_listener

    def _listen(self, method: str, event: str, handler: Callable[[Any], Any]) -> None:
        loop = _event_loop()
        bridges = self._listener_bridges
        executor = _page_listener_thread(self._impl._page())

        def bridge(arg: Any) -> Any:
            if method == "once":
                with suppress(ValueError):
                    bridges.remove(entry)
            try:
                return loop.run_in_executor(executor, _listen, handler, _to_sync(arg))
            except RuntimeError:
                return None

        entry = (event, handler, bridge)
        bridges.append(entry)
        try:
            _run(_on_loop, getattr(self._impl, method), event, bridge)
        except BaseException:
            with suppress(ValueError):
                bridges.remove(entry)
            raise


class WebSocket(_SyncChildEmitter):
    """A WebSocket opened by a page, emitted via ``page.on("websocket")``."""

    _impl: _a.WebSocket

    def __init__(self, impl: _a.WebSocket) -> None:
        self._impl = impl
        _sync_websockets[impl] = self
        self._listener_bridges: list[tuple[str, Callable[[Any], Any], Callable[[Any], Any]]] = []

    def __repr__(self) -> str:
        return repr(self._impl)

    @property
    def url(self) -> str:
        return self._impl.url

    def is_closed(self) -> bool:
        return self._impl.is_closed()

    @contextmanager
    def expect_event(
        self,
        event: str,
        predicate: Callable[[Any], bool] | None = None,
        *,
        timeout: float | None = None,
    ) -> Iterator[EventInfo[Any]]:
        with _sync_expect(
            lambda p, timeout=None: self._impl.expect_event(event, p, timeout=timeout),
            predicate,
            timeout,
            _to_sync,
        ) as info:
            yield info


class Worker(_SyncChildEmitter):
    """A dedicated Web Worker running on a page."""

    _impl: _a.Worker

    def __init__(self, impl: _a.Worker) -> None:
        self._impl = impl
        _sync_workers[impl] = self
        self._listener_bridges: list[tuple[str, Callable[[Any], Any], Callable[[Any], Any]]] = []

    def __repr__(self) -> str:
        return repr(self._impl)

    @property
    def url(self) -> str:
        return self._impl.url

    def evaluate(self, expression: str, arg: Any = None) -> Any:
        return _run(self._impl.evaluate, expression, arg)

    @contextmanager
    def expect_event(
        self,
        event: str,
        predicate: Callable[[Any], bool] | None = None,
        *,
        timeout: float | None = None,
    ) -> Iterator[EventInfo[Any]]:
        with _sync_expect(
            lambda p, timeout=None: self._impl.expect_event(event, p, timeout=timeout),
            predicate,
            timeout,
            _to_sync,
        ) as info:
            yield info


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


class FileChooser:
    """A native file chooser dialog, intercepted via ``page.on("filechooser")`` or
    ``page.expect_file_chooser()``."""

    def __init__(self, impl: _a.FileChooser) -> None:
        self._impl = impl

    def __repr__(self) -> str:
        return repr(self._impl)

    def is_multiple(self) -> bool:
        return self._impl.is_multiple()

    def set_files(
        self,
        files: str | PathLike[str] | FilePayload | Sequence[str | PathLike[str] | FilePayload],
        *,
        timeout: float | None = None,
    ) -> None:
        _run(self._impl.set_files, files, timeout=timeout)


class Mouse:
    """Low-level pointer controls in viewport CSS pixels, accessed via ``page.mouse``."""

    def __init__(self, impl: _a.Mouse) -> None:
        self._impl = impl

    def move(self, x: float, y: float, *, steps: int | None = None) -> None:
        _run(self._impl.move, x, y, steps=steps)

    def down(
        self,
        *,
        button: Literal["left", "right", "middle"] | None = None,
        click_count: int | None = None,
    ) -> None:
        _run(self._impl.down, button=button, click_count=click_count)

    def up(
        self,
        *,
        button: Literal["left", "right", "middle"] | None = None,
        click_count: int | None = None,
    ) -> None:
        _run(self._impl.up, button=button, click_count=click_count)

    def click(
        self,
        x: float,
        y: float,
        *,
        delay: float | None = None,
        button: Literal["left", "right", "middle"] | None = None,
        click_count: int | None = None,
    ) -> None:
        _run(self._impl.click, x, y, delay=delay, button=button, click_count=click_count)

    def dblclick(
        self,
        x: float,
        y: float,
        *,
        delay: float | None = None,
        button: Literal["left", "right", "middle"] | None = None,
    ) -> None:
        _run(self._impl.dblclick, x, y, delay=delay, button=button)

    def wheel(self, delta_x: float, delta_y: float) -> None:
        _run(self._impl.wheel, delta_x, delta_y)


class Keyboard:
    """Low-level keyboard controls, accessed via ``page.keyboard``."""

    def __init__(self, impl: _a.Keyboard) -> None:
        self._impl = impl

    def down(self, key: str) -> None:
        _run(self._impl.down, key)

    def up(self, key: str) -> None:
        _run(self._impl.up, key)

    def press(self, key: str, *, delay: float | None = None) -> None:
        _run(self._impl.press, key, delay=delay)

    def type(self, text: str, *, delay: float | None = None) -> None:
        _run(self._impl.type, text, delay=delay)

    def insert_text(self, text: str) -> None:
        _run(self._impl.insert_text, text)


class Touchscreen:
    """Low-level touchscreen controls in viewport CSS pixels, accessed via ``page.touchscreen``."""

    def __init__(self, impl: _a.Touchscreen) -> None:
        self._impl = impl

    def tap(self, x: float, y: float) -> None:
        _run(self._impl.tap, x, y)


class APIResponse:
    """HTTP response returned by ``APIRequestContext`` and ``route.fetch()``."""

    def __init__(self, impl: _a.APIResponse) -> None:
        self._impl = impl

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
        return self._impl.ok

    @property
    def headers(self) -> dict[str, str]:
        return self._impl.headers

    def headers_array(self) -> list[dict[str, str]]:
        return self._impl.headers_array()

    def body(self) -> bytes:
        return _run(self._impl.body)

    def text(self) -> str:
        return _run(self._impl.text)

    def json(self) -> Any:
        return _run(self._impl.json)

    def dispose(self) -> None:
        _run(self._impl.dispose)


class APIRequestContext:
    """HTTP client sharing cookies and defaults with a ``BrowserContext`` or ``Page``."""

    def __init__(self, impl: _a.APIRequestContext) -> None:
        self._impl = impl

    def fetch(
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
        return APIResponse(
            _run(
                self._impl.fetch,
                url,
                params=params,
                method=method,
                headers=headers,
                data=data,
                form=form,
                multipart=multipart,
                timeout=timeout,
                fail_on_status_code=fail_on_status_code,
                max_redirects=max_redirects,
            )
        )

    def get(self, url: str, **kwargs: Any) -> APIResponse:
        return APIResponse(_run(self._impl.get, url, **kwargs))

    def post(self, url: str, **kwargs: Any) -> APIResponse:
        return APIResponse(_run(self._impl.post, url, **kwargs))

    def put(self, url: str, **kwargs: Any) -> APIResponse:
        return APIResponse(_run(self._impl.put, url, **kwargs))

    def patch(self, url: str, **kwargs: Any) -> APIResponse:
        return APIResponse(_run(self._impl.patch, url, **kwargs))

    def delete(self, url: str, **kwargs: Any) -> APIResponse:
        return APIResponse(_run(self._impl.delete, url, **kwargs))

    def head(self, url: str, **kwargs: Any) -> APIResponse:
        return APIResponse(_run(self._impl.head, url, **kwargs))

    def storage_state(self, *, path: str | PathLike[str] | None = None) -> StorageState:
        return _run(self._impl.storage_state, path=path)

    def dispose(self) -> None:
        _run(self._impl.dispose)


class Route:
    """How a route handler answers a request, as Playwright's ``Route``. Each request takes one
    answer."""

    def __init__(self, impl: _a.Route) -> None:
        self._impl = impl

    @property
    def request(self) -> Request:
        return Request(self._impl.request)

    def fetch(
        self,
        *,
        url: str | None = None,
        method: str | None = None,
        headers: dict[str, str] | None = None,
        post_data: Any = None,
        max_redirects: int | None = None,
        timeout: float | None = None,
    ) -> APIResponse:
        return APIResponse(
            _run(
                self._impl.fetch,
                url=url,
                method=method,
                headers=headers,
                post_data=post_data,
                max_redirects=max_redirects,
                timeout=timeout,
            )
        )

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
            response=response._impl if response is not None else None,
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
        _sync_pages[impl] = self
        self._route_bridges: list[tuple[URLMatch, Callable[..., Any], Callable[..., Any]]] = []
        self._listener_bridges: list[tuple[str, Callable[[Any], Any], Callable[[Any], Any]]] = []
        # This page's listeners run here, one at a time and in order, so a slow one on another
        # page never holds them up. The thread starts with the first event.
        self._listener_thread = concurrent.futures.ThreadPoolExecutor(1, thread_name_prefix="fluxwright-listeners")

    @property
    def url(self) -> str:
        return self._impl.url

    def goto(self, url: str, *, wait_until: str | None = None, timeout: float | None = None) -> None:
        """Navigates; ``wait_until`` is ``load`` (the default), ``domcontentloaded``,
        ``networkidle`` or ``commit``. ``timeout`` is in milliseconds; 0 for none."""
        _run(self._impl.goto, url, wait_until=wait_until, timeout=timeout)

    def reload(self, *, wait_until: str | None = None, timeout: float | None = None) -> None:
        _run(self._impl.reload, wait_until=wait_until, timeout=timeout)

    def go_back(self, *, wait_until: str | None = None, timeout: float | None = None) -> None:
        _run(self._impl.go_back, wait_until=wait_until, timeout=timeout)

    def go_forward(self, *, wait_until: str | None = None, timeout: float | None = None) -> None:
        _run(self._impl.go_forward, wait_until=wait_until, timeout=timeout)

    def set_content(self, html: str, *, wait_until: str | None = None, timeout: float | None = None) -> None:
        _run(self._impl.set_content, html, wait_until=wait_until, timeout=timeout)

    def wait_for_load_state(
        self,
        state: Literal["load", "domcontentloaded", "networkidle"] | None = None,
        *,
        timeout: float | None = None,
    ) -> None:
        _run(self._impl.wait_for_load_state, state, timeout=timeout)

    def wait_for_url(
        self,
        url: URLMatch,
        *,
        timeout: float | None = None,
        wait_until: Literal["load", "domcontentloaded", "networkidle", "commit"] | None = None,
    ) -> None:
        _run(self._impl.wait_for_url, url, timeout=timeout, wait_until=wait_until)

    def wait_for_function(
        self,
        expression: str,
        arg: Any = None,
        *,
        timeout: float | None = None,
        polling: float | Literal["raf"] | None = None,
    ) -> Any:
        return _run(self._impl.wait_for_function, expression, arg, timeout=timeout, polling=polling)

    def set_default_timeout(self, timeout: float) -> None:
        """The timeout in milliseconds for this page's calls from now on, navigation included;
        0 for none. A call's own ``timeout`` wins."""
        self._impl.set_default_timeout(timeout)

    def set_default_navigation_timeout(self, timeout: float) -> None:
        """The timeout in milliseconds for this page's ``goto`` from now on; 0 for none."""
        self._impl.set_default_navigation_timeout(timeout)

    def title(self) -> str:
        return _run(self._impl.title)

    def content(self) -> str:
        return _run(self._impl.content)

    @property
    def mouse(self) -> Mouse:
        return Mouse(self._impl.mouse)

    @property
    def keyboard(self) -> Keyboard:
        return Keyboard(self._impl.keyboard)

    @property
    def touchscreen(self) -> Touchscreen:
        return Touchscreen(self._impl.touchscreen)

    def click(
        self,
        selector: str,
        *,
        button: Literal["left", "right", "middle"] | None = None,
        click_count: int | None = None,
        delay: float | None = None,
        position: Position | None = None,
        modifiers: Sequence[Literal["Alt", "Control", "Meta", "Shift", "ControlOrMeta"]] | None = None,
        force: bool | None = None,
        trial: bool | None = None,
        strict: bool | None = None,
        timeout: float | None = None,
    ) -> None:
        """Scrolls into view and waits until the element is enabled, stable and not covered."""
        _run(
            self._impl.click,
            selector,
            button=button,
            click_count=click_count,
            delay=delay,
            position=position,
            modifiers=list(modifiers) if modifiers is not None else None,
            force=force,
            trial=trial,
            strict=strict,
            timeout=timeout,
        )

    def dblclick(
        self,
        selector: str,
        *,
        button: Literal["left", "right", "middle"] | None = None,
        delay: float | None = None,
        position: Position | None = None,
        modifiers: Sequence[Literal["Alt", "Control", "Meta", "Shift", "ControlOrMeta"]] | None = None,
        force: bool | None = None,
        trial: bool | None = None,
        strict: bool | None = None,
        timeout: float | None = None,
    ) -> None:
        _run(
            self._impl.dblclick,
            selector,
            button=button,
            delay=delay,
            position=position,
            modifiers=list(modifiers) if modifiers is not None else None,
            force=force,
            trial=trial,
            strict=strict,
            timeout=timeout,
        )

    def hover(
        self,
        selector: str,
        *,
        position: Position | None = None,
        modifiers: Sequence[Literal["Alt", "Control", "Meta", "Shift", "ControlOrMeta"]] | None = None,
        force: bool | None = None,
        trial: bool | None = None,
        strict: bool | None = None,
        timeout: float | None = None,
    ) -> None:
        _run(
            self._impl.hover,
            selector,
            position=position,
            modifiers=list(modifiers) if modifiers is not None else None,
            force=force,
            trial=trial,
            strict=strict,
            timeout=timeout,
        )

    def tap(
        self,
        selector: str,
        *,
        position: Position | None = None,
        modifiers: Sequence[Literal["Alt", "Control", "Meta", "Shift", "ControlOrMeta"]] | None = None,
        force: bool | None = None,
        trial: bool | None = None,
        strict: bool | None = None,
        timeout: float | None = None,
    ) -> None:
        _run(
            self._impl.tap,
            selector,
            position=position,
            modifiers=list(modifiers) if modifiers is not None else None,
            force=force,
            trial=trial,
            strict=strict,
            timeout=timeout,
        )

    def focus(self, selector: str, *, strict: bool | None = None, timeout: float | None = None) -> None:
        _run(self._impl.focus, selector, strict=strict, timeout=timeout)

    def blur(self, selector: str, *, strict: bool | None = None, timeout: float | None = None) -> None:
        _run(self._impl.blur, selector, strict=strict, timeout=timeout)

    def scroll_into_view_if_needed(
        self, selector: str, *, strict: bool | None = None, timeout: float | None = None
    ) -> None:
        _run(self._impl.scroll_into_view_if_needed, selector, strict=strict, timeout=timeout)

    def drag_and_drop(
        self,
        source: str,
        target: str,
        *,
        force: bool | None = None,
        strict: bool | None = None,
        timeout: float | None = None,
    ) -> None:
        _run(self._impl.drag_and_drop, source, target, force=force, strict=strict, timeout=timeout)

    def fill(self, selector: str, value: str, *, strict: bool | None = None, timeout: float | None = None) -> None:
        _run(self._impl.fill, selector, value, strict=strict, timeout=timeout)

    def clear(
        self, selector: str, *, force: bool | None = None, strict: bool | None = None, timeout: float | None = None
    ) -> None:
        _run(self._impl.clear, selector, force=force, strict=strict, timeout=timeout)

    def press(
        self,
        selector: str,
        key: str,
        *,
        delay: float | None = None,
        strict: bool | None = None,
        timeout: float | None = None,
    ) -> None:
        _run(self._impl.press, selector, key, delay=delay, strict=strict, timeout=timeout)

    def press_sequentially(
        self,
        selector: str,
        text: str,
        *,
        delay: float | None = None,
        strict: bool | None = None,
        timeout: float | None = None,
    ) -> None:
        _run(self._impl.press_sequentially, selector, text, delay=delay, strict=strict, timeout=timeout)

    def type(
        self,
        selector: str,
        text: str,
        *,
        delay: float | None = None,
        strict: bool | None = None,
        timeout: float | None = None,
    ) -> None:
        _run(self._impl.type, selector, text, delay=delay, strict=strict, timeout=timeout)

    def check(
        self,
        selector: str,
        *,
        position: Position | None = None,
        force: bool | None = None,
        trial: bool | None = None,
        strict: bool | None = None,
        timeout: float | None = None,
    ) -> None:
        _run(self._impl.check, selector, position=position, force=force, trial=trial, strict=strict, timeout=timeout)

    def uncheck(
        self,
        selector: str,
        *,
        position: Position | None = None,
        force: bool | None = None,
        trial: bool | None = None,
        strict: bool | None = None,
        timeout: float | None = None,
    ) -> None:
        _run(self._impl.uncheck, selector, position=position, force=force, trial=trial, strict=strict, timeout=timeout)

    def set_checked(
        self,
        selector: str,
        checked: bool,
        *,
        position: Position | None = None,
        force: bool | None = None,
        trial: bool | None = None,
        strict: bool | None = None,
        timeout: float | None = None,
    ) -> None:
        _run(
            self._impl.set_checked,
            selector,
            checked,
            position=position,
            force=force,
            trial=trial,
            strict=strict,
            timeout=timeout,
        )

    def select_option(
        self,
        selector: str,
        value: str | Sequence[str] | Mapping[str, Any] | Sequence[Mapping[str, Any]] | None = None,
        *,
        index: int | Sequence[int] | None = None,
        label: str | Sequence[str] | None = None,
        element: Any = None,
        force: bool | None = None,
        strict: bool | None = None,
        timeout: float | None = None,
    ) -> list[str]:
        return _run(
            self._impl.select_option,
            selector,
            value,
            index=index,
            label=label,
            element=element,
            force=force,
            strict=strict,
            timeout=timeout,
        )

    def set_input_files(
        self,
        selector: str,
        files: str | PathLike[str] | FilePayload | Sequence[str | PathLike[str] | FilePayload],
        *,
        strict: bool | None = None,
        timeout: float | None = None,
    ) -> None:
        _run(self._impl.set_input_files, selector, files, strict=strict, timeout=timeout)

    def wait_for_selector(
        self,
        selector: str,
        *,
        state: Literal["attached", "detached", "visible", "hidden"] | None = None,
        strict: bool | None = None,
        timeout: float | None = None,
    ) -> None:
        """Waits until ``state``: visible by default."""
        _run(self._impl.wait_for_selector, selector, state=state, strict=strict, timeout=timeout)

    def text_content(
        self, selector: str, *, strict: bool | None = None, timeout: float | None = None
    ) -> str | None:
        return _run(self._impl.text_content, selector, strict=strict, timeout=timeout)

    def inner_text(self, selector: str, *, strict: bool | None = None, timeout: float | None = None) -> str:
        return _run(self._impl.inner_text, selector, strict=strict, timeout=timeout)

    def inner_html(self, selector: str, *, strict: bool | None = None, timeout: float | None = None) -> str:
        return _run(self._impl.inner_html, selector, strict=strict, timeout=timeout)

    def input_value(self, selector: str, *, strict: bool | None = None, timeout: float | None = None) -> str:
        return _run(self._impl.input_value, selector, strict=strict, timeout=timeout)

    def get_attribute(
        self, selector: str, name: str, *, strict: bool | None = None, timeout: float | None = None
    ) -> str | None:
        return _run(self._impl.get_attribute, selector, name, strict=strict, timeout=timeout)

    def is_visible(self, selector: str, *, strict: bool | None = None, timeout: float | None = None) -> bool:
        return _run(self._impl.is_visible, selector, strict=strict, timeout=timeout)

    def is_hidden(self, selector: str, *, strict: bool | None = None, timeout: float | None = None) -> bool:
        return _run(self._impl.is_hidden, selector, strict=strict, timeout=timeout)

    def is_enabled(self, selector: str, *, strict: bool | None = None, timeout: float | None = None) -> bool:
        return _run(self._impl.is_enabled, selector, strict=strict, timeout=timeout)

    def is_disabled(self, selector: str, *, strict: bool | None = None, timeout: float | None = None) -> bool:
        return _run(self._impl.is_disabled, selector, strict=strict, timeout=timeout)

    def is_checked(self, selector: str, *, strict: bool | None = None, timeout: float | None = None) -> bool:
        return _run(self._impl.is_checked, selector, strict=strict, timeout=timeout)

    def is_editable(self, selector: str, *, strict: bool | None = None, timeout: float | None = None) -> bool:
        return _run(self._impl.is_editable, selector, strict=strict, timeout=timeout)

    def evaluate(self, expression: str, arg: Any = None) -> Any:
        """Runs JavaScript in the page and returns its JSON result, awaiting promises. A function
        (``"(x) => x * 2"``) is called with ``arg``; anything else is evaluated as is."""
        return _run(self._impl.evaluate, expression, arg)

    def screenshot(
        self,
        *,
        path: str | PathLike[str] | None = None,
        full_page: bool | None = None,
        type: Literal["png", "jpeg"] | None = None,
        quality: int | None = None,
        clip: FloatRect | None = None,
        omit_background: bool | None = None,
        timeout: float | None = None,
    ) -> bytes:
        """PNG or JPEG; with ``path``, also written there (extension picks type if omitted)."""
        return _run(
            self._impl.screenshot,
            path=path,
            full_page=full_page,
            type=type,
            quality=quality,
            clip=clip,
            omit_background=omit_background,
            timeout=timeout,
        )

    def pdf(
        self,
        *,
        path: str | PathLike[str] | None = None,
        format: str | None = None,
        width: str | float | None = None,
        height: str | float | None = None,
        margin: PdfMargins | None = None,
        landscape: bool | None = None,
        print_background: bool | None = None,
        scale: float | None = None,
        page_ranges: str | None = None,
        prefer_css_page_size: bool | None = None,
        display_header_footer: bool | None = None,
        header_template: str | None = None,
        footer_template: str | None = None,
        outline: bool | None = None,
        tagged: bool | None = None,
    ) -> bytes:
        """The page printed to PDF (headless only); with ``path``, also written there."""
        return _run(
            self._impl.pdf,
            path=path,
            format=format,
            width=width,
            height=height,
            margin=margin,
            landscape=landscape,
            print_background=print_background,
            scale=scale,
            page_ranges=page_ranges,
            prefer_css_page_size=prefer_css_page_size,
            display_header_footer=display_header_footer,
            header_template=header_template,
            footer_template=footer_template,
            outline=outline,
            tagged=tagged,
        )

    def set_viewport_size(self, viewport_size: ViewportSize) -> None:
        _run(self._impl.set_viewport_size, viewport_size)

    def emulate_media(
        self,
        *,
        media: Literal["screen", "print"] | None = _a._UNSET,
        color_scheme: Literal["light", "dark", "no-preference"] | None = _a._UNSET,
    ) -> None:
        """Changes CSS ``media`` and/or ``prefers-color-scheme``; ``None`` resets either."""
        _run(self._impl.emulate_media, media=media, color_scheme=color_scheme)

    def storage_state(
        self, *, path: str | PathLike[str] | None = None, indexed_db: bool = False
    ) -> StorageState:
        """Every cookie in this page's context plus localStorage (and optionally IndexedDB) of the current origin; with
        ``path``, also saved as JSON for ``new_page(storage_state=path)``."""
        return _run(self._impl.storage_state, path=path, indexed_db=indexed_db)

    @property
    def request(self) -> APIRequestContext:
        return APIRequestContext(self._impl.request)

    def console_messages(self) -> list[ConsoleMessage]:
        """What the page, its popups and iframes logged so far (the last 1000)."""
        return _run(self._impl.console_messages)

    def page_errors(self) -> list[Error]:
        """The exceptions nothing caught so far (the last 1000)."""
        return _run(self._impl.page_errors)

    def frame_locator(self, selector: str) -> FrameLocator:
        return FrameLocator(self._impl.frame_locator(selector))

    @property
    def frames(self) -> list[Frame]:
        return [Frame(f) for f in self._impl.frames]

    @property
    def main_frame(self) -> Frame:
        return Frame(self._impl.main_frame)

    def frame(self, name: str | None = None, *, url: URLMatch | None = None) -> Frame | None:
        f = self._impl.frame(name, url=url)
        return Frame(f) if f is not None else None

    def bring_to_front(self) -> None:
        _run(self._impl.bring_to_front)

    def opener(self) -> Page | None:
        p = _run(self._impl.opener)
        return _wrap_page(p) if p is not None else None

    @contextmanager
    def expect_popup(
        self,
        predicate: Callable[[Page], bool] | None = None,
        *,
        timeout: float | None = None,
    ) -> Iterator[EventInfo[Page]]:
        """Waits for a popup page opened by the code in the ``with`` block."""
        with self._expect(self._impl.expect_popup, predicate, timeout, _wrap_page) as info:
            yield info

    def route(self, url: URLMatch, handler: Callable[..., Any]) -> None:
        """Hands matching requests to ``handler(route)`` or ``handler(route, request)``, on a
        worker thread, as Playwright's ``page.route``. The handler added last runs first; an
        unanswered request continues. Call it before navigating."""
        _bridge_route(self._route_bridges, self._impl, url, handler)

    def unroute(self, url: URLMatch, handler: Callable[..., Any] | None = None) -> None:
        """Removes the handlers added for ``url`` (only ``handler``, when given)."""
        _unbridge_route(self._route_bridges, self._impl, url, handler)

    def route_from_har(
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
        _run(
            self._impl.route_from_har,
            har,
            url=url,
            not_found=not_found,
            update=update,
            update_mode=update_mode,
            update_content=update_content,
        )

    def add_init_script(self, script: str | None = None, *, path: str | PathLike[str] | None = None) -> None:
        """Adds a script evaluated whenever the page navigates or reloads."""
        _run(self._impl.add_init_script, script, path=path)

    def expose_function(self, name: str, callback: Callable[..., Any]) -> None:
        """Exposes ``callback`` on ``window.<name>``; runs on the worker pool so it can call the page."""
        _bridge_expose(self._impl, "page.expose_function", name, callback)

    def add_script_tag(
        self,
        *,
        url: str | None = None,
        path: str | PathLike[str] | None = None,
        content: str | None = None,
        type: str | None = None,
    ) -> None:
        """Adds a ``<script>`` tag into the page with the given ``url``, ``path`` or ``content``."""
        _run(self._impl.add_script_tag, url=url, path=path, content=content, type=type)

    def add_style_tag(
        self,
        *,
        url: str | None = None,
        path: str | PathLike[str] | None = None,
        content: str | None = None,
    ) -> None:
        """Adds a ``<style>`` or ``<link rel="stylesheet">`` tag into the page."""
        _run(self._impl.add_style_tag, url=url, path=path, content=content)

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
                with suppress(ValueError):  # a once that fired meanwhile removed itself
                    self._listener_bridges.remove(entry)
                _run(_on_loop, self._impl.remove_listener, event, entry[2])
                return

    off = remove_listener

    @contextmanager
    def expect_file_chooser(
        self,
        predicate: Callable[[FileChooser], bool] | None = None,
        *,
        timeout: float | None = None,
    ) -> Iterator[EventInfo[FileChooser]]:
        """Waits for the native file chooser opened by the code in the ``with`` block."""
        with self._expect(self._impl.expect_file_chooser, predicate, timeout, FileChooser) as info:
            yield info

    @contextmanager
    def expect_download(self, timeout: float | None = None) -> Iterator[EventInfo[Download]]:
        """Waits for the download the code in the ``with`` block starts (``timeout`` in
        milliseconds, default 30 s), as Playwright's ``page.expect_download``::

            with page.expect_download() as info:
                page.get_by_text("Export").click()
            download = info.value
        """
        _run(self._impl._clear_downloads)  # only a download the block starts
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
    def expect_websocket(
        self,
        predicate: Callable[[WebSocket], bool] | None = None,
        *,
        timeout: float | None = None,
    ) -> Iterator[EventInfo[WebSocket]]:
        """Waits for a WebSocket opened by the code in the ``with`` block."""
        with self._expect(self._impl.expect_websocket, predicate, timeout, _wrap_websocket) as info:
            yield info

    @contextmanager
    def expect_worker(
        self,
        predicate: Callable[[Worker], bool] | None = None,
        *,
        timeout: float | None = None,
    ) -> Iterator[EventInfo[Worker]]:
        """Waits for a dedicated Web Worker created by the code in the ``with`` block."""
        with self._expect(self._impl.expect_worker, predicate, timeout, _wrap_worker) as info:
            yield info

    @contextmanager
    def expect_event(
        self,
        event: str,
        predicate: Callable[[Any], bool] | None = None,
        *,
        timeout: float | None = None,
    ) -> Iterator[EventInfo[Any]]:
        """Waits for ``event`` to fire on this page."""
        with _sync_expect(
            lambda p, timeout=None: self._impl.expect_event(event, p, timeout=timeout),
            predicate,
            timeout,
            _to_sync,
        ) as info:
            yield info

    @property
    def workers(self) -> list[Worker]:
        """Dedicated Web Workers currently active on this page."""
        return [_wrap_worker(w) for w in _run(_on_loop, lambda: self._impl.workers)]

    @contextmanager
    def _expect(
        self, start: Callable[..., Any], url_or_predicate: Any, timeout: float | None, wrap: Callable[[Any], T]
    ) -> Iterator[EventInfo[T]]:
        with _sync_expect(start, url_or_predicate, timeout, wrap) as info:
            yield info

    def wait_for_download(self, *, timeout: float | None = None) -> Download:
        """The next download this page finished; ones that finished earlier queue up."""
        return Download(_run(self._impl.wait_for_download, timeout=timeout))

    def close(self) -> None:
        _run(self._impl.close)
        # Listener calls already queued still run; events after this are dropped (see _listen).
        self._listener_thread.shutdown(wait=False)

    def is_closed(self) -> bool:
        return self._impl.is_closed()

    def _listen(self, method: str, event: str, handler: Callable[[Any], Any]) -> None:
        loop = _event_loop()
        bridges = self._listener_bridges

        # Dialog listeners run on the loop's own pool: behind a slow listener on the one-at-a-time
        # thread, the answer came late and the click that opened the dialog timed out. Each page's
        # dialogs still come one at a time.
        executor = None if event == "dialog" else self._listener_thread

        def bridge(arg: Any) -> Any:
            if method == "once":
                with suppress(ValueError):  # spent: remove_listener must not find it
                    bridges.remove(entry)
            try:
                # A future, so the dialog loop waits for the handler's answer.
                return loop.run_in_executor(executor, _listen, handler, _to_sync(arg))
            except RuntimeError:  # the page closed, and its listener thread with it
                return None

        entry = (event, handler, bridge)
        bridges.append(entry)  # before the first event can fire it
        try:
            _run(_on_loop, getattr(self._impl, method), event, bridge)
        except BaseException:
            with suppress(ValueError):
                bridges.remove(entry)
            raise


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
                record_har_url_filter=record_har_url_filter,
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
    ) -> BrowserContext:
        """A browser context with ``new_page``'s options, as Playwright's ``new_context``; see ``BrowserContext``."""
        impl = _run(
            self._impl.new_context,
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
            record_har_url_filter=record_har_url_filter,
        )
        return BrowserContext(self, impl)

    def close(self) -> None:
        """Shuts the engine down, and with it every page and Chrome process it started."""
        _run(self._impl.close)

    def __enter__(self) -> Browser:
        return self

    def __exit__(self, *exc: Any) -> None:
        self.close()


class BrowserContext:
    """A browser context, as Playwright's: holds multiple pages and popups sharing cookies,
    storage, init scripts, exposed functions and routes.
    """

    def __init__(self, browser: Browser, impl: _a.BrowserContext) -> None:
        self._browser = browser
        self._impl = impl
        self._route_bridges: list[tuple[URLMatch, Callable[..., Any], Callable[..., Any]]] = []

    @property
    def browser(self) -> Browser:
        return self._browser

    @property
    def pages(self) -> list[Page]:
        return [_wrap_page(p) for p in self._impl.pages]

    def new_page(self) -> Page:
        return _wrap_page(_run(self._impl.new_page))

    def route(self, url: URLMatch, handler: Callable[..., Any]) -> None:
        """``page.route`` for all pages and popups in this context; added before ``new_page()``, it
        applies from the first request."""
        _bridge_route(self._route_bridges, self._impl, url, handler)

    def unroute(self, url: URLMatch, handler: Callable[..., Any] | None = None) -> None:
        _unbridge_route(self._route_bridges, self._impl, url, handler)

    def route_from_har(
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
        _run(
            self._impl.route_from_har,
            har,
            url=url,
            not_found=not_found,
            update=update,
            update_mode=update_mode,
            update_content=update_content,
        )

    def add_init_script(self, script: str | None = None, *, path: str | PathLike[str] | None = None) -> None:
        """``page.add_init_script`` for all pages and popups in this context."""
        _run(self._impl.add_init_script, script, path=path)

    def expose_function(self, name: str, callback: Callable[..., Any]) -> None:
        """``page.expose_function`` for all pages and popups in this context."""
        _bridge_expose(self._impl, "browser_context.expose_function", name, callback)

    @property
    def request(self) -> APIRequestContext:
        return APIRequestContext(self._impl.request)

    def storage_state(
        self, *, path: str | PathLike[str] | None = None, indexed_db: bool = False
    ) -> StorageState:
        """The cookies, localStorage, and optionally IndexedDB of this context, usable before or after ``new_page()``."""
        return _run(self._impl.storage_state, path=path, indexed_db=indexed_db)

    def cookies(self, urls: str | Sequence[str] | None = None) -> list[dict[str, Any]]:
        """Cookies in this context (all, or matching ``urls``)."""
        return _run(self._impl.cookies, urls)

    def add_cookies(self, cookies: list[dict[str, Any]]) -> None:
        """Adds cookies to this context."""
        _run(self._impl.add_cookies, cookies)

    def clear_cookies(
        self,
        *,
        name: str | re.Pattern[str] | None = None,
        domain: str | re.Pattern[str] | None = None,
        path: str | re.Pattern[str] | None = None,
    ) -> None:
        """Clears cookies in this context, optionally filtered by name, domain, and/or path."""
        _run(self._impl.clear_cookies, name=name, domain=domain, path=path)

    def new_cdp_session(self, page: Page | Frame) -> CDPSession:
        """Attaches a raw Chrome DevTools Protocol session to ``page`` or an out-of-process ``Frame``."""
        if not isinstance(page, (Page, Frame)):
            raise TypeError("browser_context.new_cdp_session: argument must be a Page or Frame")
        return CDPSession(_run(self._impl.new_cdp_session, page._impl))

    def close(self) -> None:
        """Closes this context and all of its pages and popups."""
        wrappers = [_sync_pages[p] for p in self._impl.pages if p in _sync_pages]
        _run(self._impl.close)
        for w in wrappers:
            w._listener_thread.shutdown(wait=False)

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
        max_concurrency_per_host: int | None = None,
        min_delay_per_host: float | None = None,
    ) -> Browser:
        """Starts the engine in this process: up to ``max_browsers`` Chrome processes (default
        one per 4 CPUs, 8 pages each), headless unless ``headless=False``. New pages wait while
        all Chrome processes together use more than ``memory_ceiling_mb`` (default 75% of the
        memory free at launch). When every slot is busy, ``new_page`` waits its turn however long
        the queue is; with ``queue_timeout`` (milliseconds) it raises ``TimeoutError`` after that
        long instead, counting a browser start when one is needed. ``max_concurrency_per_host``
        caps the pages on one host at once across the fleet and ``min_delay_per_host``
        (milliseconds) spaces the ``goto`` calls to it: a ``goto`` waits its turn."""
        return Browser(
            _run(
                self._impl.launch,
                headless=headless,
                executable_path=executable_path,
                max_browsers=max_browsers,
                queue_timeout=queue_timeout,
                memory_ceiling_mb=memory_ceiling_mb,
                max_concurrency_per_host=max_concurrency_per_host,
                min_delay_per_host=min_delay_per_host,
            )
        )

    def connect_over_cdp(self, endpoint_url: str) -> Browser:
        """Connects to an existing browser over the Chrome DevTools Protocol (``ws://...`` or ``http://...``)."""
        return Browser(_run(self._impl.connect_over_cdp, endpoint_url))


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
