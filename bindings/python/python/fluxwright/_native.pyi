from os import PathLike
from pathlib import Path
from typing import Any
from collections.abc import Awaitable, Mapping

from ._errors import Error
from ._types import FloatRect

_Path = str | PathLike[str]

class Engine:
    @staticmethod
    def launch(
        *,
        max_browsers: int | None = None,
        executable_path: _Path | None = None,
        headless: bool | None = None,
        queue_timeout: float | None = None,
    ) -> Awaitable[Engine]: ...
    def new_page(
        self,
        *,
        proxy: Mapping[str, Any] | None = None,
        user_agent: str | None = None,
        locale: str | None = None,
        timezone_id: str | None = None,
        geolocation: Mapping[str, Any] | None = None,
        permissions: list[str] | None = None,
        viewport: Mapping[str, Any] | None = None,
        device_scale_factor: float | None = None,
        color_scheme: str | None = None,
        storage_state: Mapping[str, Any] | _Path | None = None,
    ) -> Awaitable[Lease]: ...
    def close(self) -> Awaitable[None]: ...

class Lease: ...

class Page:
    def __new__(cls, lease: Lease) -> Page: ...
    def goto(self, url: str, *, wait_until: str | None = None) -> Awaitable[None]:
        """Navigates; ``wait_until`` is ``load`` (the default), ``domcontentloaded``,
        ``networkidle`` or ``commit``."""
    def title(self) -> Awaitable[str]: ...
    def content(self) -> Awaitable[str]: ...
    def click(self, selector: str) -> Awaitable[None]:
        """Scrolls into view and waits until the element is enabled, stable and not covered."""
    def fill(self, selector: str, value: str) -> Awaitable[None]: ...
    def wait_for_selector(self, selector: str) -> Awaitable[None]:
        """Waits until visible."""
    def evaluate(self, expression: str, arg: Any = None) -> Awaitable[Any]:
        """Runs JavaScript in the page and returns its JSON result, awaiting promises. A function
        (``"(x) => x * 2"``) is called with ``arg``; anything else is evaluated as is."""
    def screenshot(self, *, path: _Path | None = None, full_page: bool | None = None) -> Awaitable[bytes]:
        """PNG; with ``path``, also written there."""
    def set_viewport_size(self, viewport_size: Mapping[str, int]) -> Awaitable[None]: ...
    def storage_state(self, *, path: _Path | None = None) -> Awaitable[dict[str, Any]]:
        """Every cookie in this page's context plus localStorage of the current origin, in
        Playwright's format; with ``path``, also saved as JSON for ``new_page(storage_state=path)``."""
    def console_messages(self) -> Awaitable[list[ConsoleMessage]]:
        """What the page, its popups and iframes logged so far (the last 1000)."""
    def page_errors(self) -> Awaitable[list[Error]]:
        """The exceptions nothing caught so far (the last 1000)."""
    def wait_for_download(self, *, timeout: float | None = None) -> Awaitable[Download]:
        """The next download this page finished (ones that finished earlier queue up), within
        ``timeout`` milliseconds (default 30 s)."""
    def close(self) -> Awaitable[None]: ...
    def locator(self, selector: str) -> Locator:
        """CSS, ``text=``, or ``role=`` selector."""
    def get_by_text(self, text: str, *, exact: bool | None = None) -> Locator: ...
    def get_by_role(self, role: str, *, name: str | None = None, exact: bool | None = None) -> Locator: ...
    def get_by_label(self, text: str, *, exact: bool | None = None) -> Locator: ...
    def get_by_placeholder(self, text: str, *, exact: bool | None = None) -> Locator: ...
    def get_by_test_id(self, test_id: str) -> Locator: ...
    def frame_locator(self, selector: str) -> FrameLocator: ...
    def _logs(self) -> LogStream: ...
    def _intercept(self) -> Awaitable[RouteStream]: ...

class Locator:
    """Lazy, like Playwright's: every action runs the query again."""

    def click(self) -> Awaitable[None]: ...
    def fill(self, value: str) -> Awaitable[None]: ...
    def wait_for(self) -> Awaitable[None]:
        """Waits until visible."""
    def text_content(self) -> Awaitable[str | None]: ...
    def screenshot(self, *, path: _Path | None = None) -> Awaitable[bytes]:
        """PNG of the element, once it is visible and still; with ``path``, also written there."""
    def bounding_box(self) -> Awaitable[FloatRect | None]:
        """``{x, y, width, height}`` relative to the viewport, without scrolling; ``None`` when
        the element is not visible."""
    def evaluate(self, expression: str, arg: Any = None) -> Awaitable[Any]:
        """Calls the JavaScript function ``expression`` with the element and ``arg``."""
    def nth(self, index: int) -> Locator:
        """The ``index``-th match (0-based; negative counts from the end)."""
    @property
    def first(self) -> Locator: ...
    @property
    def last(self) -> Locator: ...
    def filter(self, *, has_text: str | None = None) -> Locator:
        """Keeps matches containing ``has_text`` (case-insensitive substring)."""
    def locator(self, selector: str) -> Locator:
        """Searches inside this locator's matches."""
    def get_by_text(self, text: str, *, exact: bool | None = None) -> Locator: ...
    def get_by_role(self, role: str, *, name: str | None = None, exact: bool | None = None) -> Locator: ...
    def get_by_label(self, text: str, *, exact: bool | None = None) -> Locator: ...
    def get_by_placeholder(self, text: str, *, exact: bool | None = None) -> Locator: ...
    def get_by_test_id(self, test_id: str) -> Locator: ...

class FrameLocator:
    """An iframe, same- or cross-origin, to find elements in."""

    def locator(self, selector: str) -> Locator: ...
    def get_by_text(self, text: str, *, exact: bool | None = None) -> Locator: ...
    def get_by_role(self, role: str, *, name: str | None = None, exact: bool | None = None) -> Locator: ...
    def get_by_label(self, text: str, *, exact: bool | None = None) -> Locator: ...
    def get_by_placeholder(self, text: str, *, exact: bool | None = None) -> Locator: ...
    def get_by_test_id(self, test_id: str) -> Locator: ...
    def frame_locator(self, selector: str) -> FrameLocator:
        """A nested iframe inside this one."""

class Route:
    url: str
    method: str
    headers: dict[str, str]
    post_data: str | None
    resource_type: str
    def fulfill(
        self,
        *,
        status: int | None = None,
        headers: dict[str, str] | None = None,
        body: str | bytes | None = None,
    ) -> Awaitable[None]: ...
    def continue_(
        self,
        *,
        url: str | None = None,
        method: str | None = None,
        headers: dict[str, str] | None = None,
        post_data: str | bytes | None = None,
    ) -> Awaitable[None]: ...
    def abort(self, error_code: str | None = None) -> Awaitable[None]: ...

class RouteStream:
    def next(self) -> Awaitable[Route | None]: ...

class LogStream:
    def next(self) -> Awaitable[ConsoleMessage | Error | None]: ...

class ConsoleMessage:
    type: str
    """``log``, ``error``, ``warning``, ``info``, ``debug``, ..."""
    text: str

class Download:
    """A file the page downloaded; deleted when the page closes, so ``save_as`` it first."""

    url: str
    suggested_filename: str
    """From ``Content-Disposition`` or the URL."""
    def path(self) -> Awaitable[Path]: ...
    def save_as(self, path: _Path) -> Awaitable[None]:
        """Copies the file to ``path``, creating missing folders."""
    def failure(self) -> Awaitable[str | None]: ...
