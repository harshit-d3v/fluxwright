import re
from os import PathLike
from pathlib import Path
from typing import Any, Literal
from collections.abc import Awaitable, Mapping, Sequence

from ._errors import Error
from ._types import FilePayload, FloatRect, PdfMargins, Position

_Path = str | PathLike[str]

class Engine:
    @staticmethod
    def launch(
        *,
        max_browsers: int | None = None,
        executable_path: _Path | None = None,
        headless: bool | None = None,
        queue_timeout: float | None = None,
        memory_ceiling_mb: int | None = None,
        max_concurrency_per_host: int | None = None,
        min_delay_per_host: float | None = None,
        args: Sequence[str] | None = None,
        ignore_default_args: bool | Sequence[str] | None = None,
        channel: str | None = None,
        prewarm_contexts: int | None = None,
    ) -> Awaitable[Engine]: ...
    @staticmethod
    def connect_over_cdp(endpoint_url: str) -> Awaitable[Engine]: ...
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
        is_mobile: bool | None = None,
        has_touch: bool | None = None,
        extra_http_headers: dict[str, str] | None = None,
        ignore_https_errors: bool | None = None,
        java_script_enabled: bool | None = None,
        offline: bool | None = None,
        base_url: str | None = None,
        http_credentials: Mapping[str, str] | None = None,
        bypass_csp: bool | None = None,
        storage_state: Mapping[str, Any] | _Path | None = None,
        record_har_path: _Path | None = None,
        record_har_omit_content: bool | None = None,
        record_har_content: str | None = None,
        record_har_mode: str | None = None,
        record_har_url_filter: str | None = None,
        resource_preset: str | None = None,
        block_resources: Sequence[str] | None = None,
        block_url_patterns: Sequence[str] | None = None,
    ) -> Awaitable[Lease]: ...
    def new_context(
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
        is_mobile: bool | None = None,
        has_touch: bool | None = None,
        extra_http_headers: dict[str, str] | None = None,
        ignore_https_errors: bool | None = None,
        java_script_enabled: bool | None = None,
        offline: bool | None = None,
        base_url: str | None = None,
        http_credentials: Mapping[str, str] | None = None,
        bypass_csp: bool | None = None,
        storage_state: Mapping[str, Any] | _Path | None = None,
        record_har_path: _Path | None = None,
        record_har_omit_content: bool | None = None,
        record_har_content: str | None = None,
        record_har_mode: str | None = None,
        record_har_url_filter: str | None = None,
        resource_preset: str | None = None,
        block_resources: Sequence[str] | None = None,
        block_url_patterns: Sequence[str] | None = None,
    ) -> Awaitable[Context]: ...
    def close(self) -> Awaitable[None]: ...


class ApiResponse:
    url: str
    status: int
    status_text: str
    headers: list[tuple[str, str]]
    @property
    def body(self) -> bytes: ...
    def dispose(self) -> None: ...


class Context:
    def new_page(self) -> Awaitable[Lease]: ...
    def cookies(self, urls: Sequence[str] | None = None) -> Awaitable[list[dict[str, Any]]]: ...
    def add_cookies(self, cookies: list[dict[str, Any]]) -> Awaitable[None]: ...
    def clear_cookies(self) -> Awaitable[None]: ...
    def replace_cookies(self, cookies: list[dict[str, Any]]) -> Awaitable[None]: ...
    def api_fetch(
        self,
        url: str,
        *,
        method: str | None = None,
        headers: dict[str, str] | None = None,
        params: Sequence[tuple[str, str]] | None = None,
        data: str | bytes | None = None,
        fail_on_status_code: bool = False,
        max_redirects: int | None = None,
        timeout: float | None = None,
    ) -> Awaitable[ApiResponse]: ...
    def add_init_script(self, source: str) -> Awaitable[None]: ...
    def expose_binding(self, name: str) -> Awaitable[None]: ...
    def binding_calls(self) -> BindingStream: ...
    def intercept(self) -> Awaitable[RouteStream]: ...
    def storage_state(
        self, *, path: _Path | None = None, indexed_db: bool = False
    ) -> Awaitable[dict[str, Any]]: ...
    def _start_har_recording(
        self,
        path: _Path,
        *,
        omit_content: bool | None = None,
        content: str | None = None,
        mode: str | None = None,
        url_filter: str | None = None,
    ) -> Awaitable[None]: ...
    def is_closed(self) -> bool: ...
    def close(self) -> Awaitable[None]: ...


class FrameInfo:
    id: str
    parent_id: str | None
    name: str
    url: str

class Lease: ...

class Page:
    def __new__(cls, lease: Lease) -> Page: ...
    @property
    def url(self) -> str: ...
    def goto(self, url: str, *, wait_until: str | None = None, timeout: float | None = None) -> Awaitable[None]:
        """Navigates; ``wait_until`` is ``load`` (the default), ``domcontentloaded``,
        ``networkidle`` or ``commit``. ``timeout`` is in milliseconds; 0 for none."""
    def reload(self, *, wait_until: str | None = None, timeout: float | None = None) -> Awaitable[None]: ...
    def go_back(self, *, wait_until: str | None = None, timeout: float | None = None) -> Awaitable[None]: ...
    def go_forward(self, *, wait_until: str | None = None, timeout: float | None = None) -> Awaitable[None]: ...
    def set_content(
        self, html: str, *, wait_until: str | None = None, timeout: float | None = None
    ) -> Awaitable[None]: ...
    def wait_for_load_state(
        self,
        state: Literal["load", "domcontentloaded", "networkidle"] | None = None,
        *,
        timeout: float | None = None,
    ) -> Awaitable[None]: ...
    def wait_for_function(
        self,
        expression: str,
        arg: Any = None,
        *,
        timeout: float | None = None,
        polling: float | Literal["raf"] | None = None,
    ) -> Awaitable[Any]: ...
    def _navigation_timeout(self) -> float: ...
    def _base_url(self) -> str | None: ...
    def _wait_for_url_change(self, after_url: str, *, timeout: float | None = None) -> Awaitable[str]: ...
    def set_default_timeout(self, timeout: float) -> None:
        """The timeout in milliseconds for this page's calls from now on, navigation included;
        0 for none. A call's own ``timeout`` wins."""
    def set_default_navigation_timeout(self, timeout: float) -> None:
        """The timeout in milliseconds for this page's ``goto`` from now on; 0 for none."""
    def title(self) -> Awaitable[str]: ...
    def content(self) -> Awaitable[str]: ...
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
    ) -> Awaitable[None]:
        """Scrolls into view and waits until the element is enabled, stable and not covered.
        ``timeout`` is in milliseconds, for this call only; 0 waits as long as it takes."""
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
    ) -> Awaitable[None]: ...
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
    ) -> Awaitable[None]: ...
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
    ) -> Awaitable[None]: ...
    def focus(self, selector: str, *, strict: bool | None = None, timeout: float | None = None) -> Awaitable[None]: ...
    def blur(self, selector: str, *, strict: bool | None = None, timeout: float | None = None) -> Awaitable[None]: ...
    def scroll_into_view_if_needed(
        self, selector: str, *, strict: bool | None = None, timeout: float | None = None
    ) -> Awaitable[None]: ...
    def drag_and_drop(
        self,
        source: str,
        target: str,
        *,
        force: bool | None = None,
        strict: bool | None = None,
        timeout: float | None = None,
    ) -> Awaitable[None]: ...
    def fill(
        self, selector: str, value: str, *, strict: bool | None = None, timeout: float | None = None
    ) -> Awaitable[None]: ...
    def clear(
        self, selector: str, *, force: bool | None = None, strict: bool | None = None, timeout: float | None = None
    ) -> Awaitable[None]: ...
    def press(
        self, selector: str, key: str, *, delay: float | None = None, strict: bool | None = None, timeout: float | None = None
    ) -> Awaitable[None]: ...
    def press_sequentially(
        self, selector: str, text: str, *, delay: float | None = None, strict: bool | None = None, timeout: float | None = None
    ) -> Awaitable[None]: ...
    def type(
        self, selector: str, text: str, *, delay: float | None = None, strict: bool | None = None, timeout: float | None = None
    ) -> Awaitable[None]: ...
    def check(
        self,
        selector: str,
        *,
        position: Position | None = None,
        force: bool | None = None,
        trial: bool | None = None,
        strict: bool | None = None,
        timeout: float | None = None,
    ) -> Awaitable[None]: ...
    def uncheck(
        self,
        selector: str,
        *,
        position: Position | None = None,
        force: bool | None = None,
        trial: bool | None = None,
        strict: bool | None = None,
        timeout: float | None = None,
    ) -> Awaitable[None]: ...
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
    ) -> Awaitable[None]: ...
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
    ) -> Awaitable[list[str]]: ...
    def set_input_files(
        self,
        selector: str,
        files: str | PathLike[str] | FilePayload | Sequence[str | PathLike[str] | FilePayload],
        *,
        strict: bool | None = None,
        timeout: float | None = None,
    ) -> Awaitable[None]: ...
    def wait_for_selector(
        self,
        selector: str,
        *,
        state: Literal["attached", "detached", "visible", "hidden"] | None = None,
        strict: bool | None = None,
        timeout: float | None = None,
    ) -> Awaitable[None]:
        """Waits until ``state``: visible by default."""
    def text_content(
        self, selector: str, *, strict: bool | None = None, timeout: float | None = None
    ) -> Awaitable[str | None]: ...
    def inner_text(
        self, selector: str, *, strict: bool | None = None, timeout: float | None = None
    ) -> Awaitable[str]: ...
    def inner_html(
        self, selector: str, *, strict: bool | None = None, timeout: float | None = None
    ) -> Awaitable[str]: ...
    def input_value(
        self, selector: str, *, strict: bool | None = None, timeout: float | None = None
    ) -> Awaitable[str]: ...
    def get_attribute(
        self, selector: str, name: str, *, strict: bool | None = None, timeout: float | None = None
    ) -> Awaitable[str | None]: ...
    def is_visible(
        self, selector: str, *, strict: bool | None = None, timeout: float | None = None
    ) -> Awaitable[bool]: ...
    def is_hidden(
        self, selector: str, *, strict: bool | None = None, timeout: float | None = None
    ) -> Awaitable[bool]: ...
    def is_enabled(
        self, selector: str, *, strict: bool | None = None, timeout: float | None = None
    ) -> Awaitable[bool]: ...
    def is_disabled(
        self, selector: str, *, strict: bool | None = None, timeout: float | None = None
    ) -> Awaitable[bool]: ...
    def is_checked(
        self, selector: str, *, strict: bool | None = None, timeout: float | None = None
    ) -> Awaitable[bool]: ...
    def is_editable(
        self, selector: str, *, strict: bool | None = None, timeout: float | None = None
    ) -> Awaitable[bool]: ...
    def evaluate(self, expression: str, arg: Any = None) -> Awaitable[Any]:
        """Runs JavaScript in the page and returns its JSON result, awaiting promises. A function
        (``"(x) => x * 2"``) is called with ``arg``; anything else is evaluated as is."""
    def screenshot(
        self,
        *,
        path: _Path | None = None,
        full_page: bool | None = None,
        type: Literal["png", "jpeg"] | None = None,
        quality: int | None = None,
        clip: FloatRect | None = None,
        omit_background: bool | None = None,
        timeout: float | None = None,
    ) -> Awaitable[bytes]:
        """PNG or JPEG; with ``path``, also written there (extension picks type if omitted)."""
    def pdf(
        self,
        *,
        path: _Path | None = None,
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
    ) -> Awaitable[bytes]:
        """The page printed to PDF (headless only); with ``path``, also written there."""
    def set_viewport_size(self, viewport_size: Mapping[str, int]) -> Awaitable[None]: ...
    def storage_state(
        self, *, path: _Path | None = None, indexed_db: bool = False
    ) -> Awaitable[dict[str, Any]]:
        """Every cookie in this page's context plus localStorage (and optionally IndexedDB) of the current origin, in
        Playwright's format; with ``path``, also saved as JSON for ``new_page(storage_state=path)``."""
    def console_messages(self) -> Awaitable[list[ConsoleMessage]]:
        """What the page, its popups and iframes logged so far (the last 1000)."""
    def page_errors(self) -> Awaitable[list[Error]]:
        """The exceptions nothing caught so far (the last 1000)."""
    def wait_for_download(self, *, timeout: float | None = None) -> Awaitable[Download]:
        """The next download this page finished (ones that finished earlier queue up), within
        ``timeout`` milliseconds (default 30 s)."""
    def _clear_downloads(self) -> Awaitable[None]: ...
    def close(self) -> Awaitable[None]: ...
    def is_closed(self) -> bool:
        """Whether ``close()`` was called (or is under way)."""
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
    def get_by_text(self, text: str, *, exact: bool | None = None) -> Locator: ...
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
    ) -> Locator: ...
    def get_by_label(self, text: str, *, exact: bool | None = None) -> Locator: ...
    def get_by_placeholder(self, text: str, *, exact: bool | None = None) -> Locator: ...
    def get_by_alt_text(self, text: str, *, exact: bool | None = None) -> Locator: ...
    def get_by_title(self, text: str, *, exact: bool | None = None) -> Locator: ...
    def get_by_test_id(self, test_id: str) -> Locator: ...
    def frame_locator(self, selector: str) -> FrameLocator: ...
    def _logs(self) -> LogStream: ...
    def _network(self) -> NetworkStream: ...
    def _dialogs(self) -> DialogStream: ...
    def _file_choosers(self) -> FileChooserStream: ...
    def _binding_calls(self) -> BindingStream: ...
    def _add_init_script(self, source: str) -> Awaitable[None]: ...
    def _expose_binding(self, name: str) -> Awaitable[None]: ...
    def _add_script_tag(
        self, *, url: str | None = None, content: str | None = None, script_type: str | None = None
    ) -> Awaitable[None]: ...
    def _add_style_tag(self, *, url: str | None = None, content: str | None = None) -> Awaitable[None]: ...
    def _mouse_move(self, x: float, y: float, *, steps: int | None = None) -> Awaitable[None]: ...
    def _mouse_down(
        self, *, button: Literal["left", "right", "middle"] | None = None, click_count: int | None = None
    ) -> Awaitable[None]: ...
    def _mouse_up(
        self, *, button: Literal["left", "right", "middle"] | None = None, click_count: int | None = None
    ) -> Awaitable[None]: ...
    def _mouse_click(
        self,
        x: float,
        y: float,
        *,
        delay: float | None = None,
        button: Literal["left", "right", "middle"] | None = None,
        click_count: int | None = None,
    ) -> Awaitable[None]: ...
    def _mouse_dblclick(
        self,
        x: float,
        y: float,
        *,
        delay: float | None = None,
        button: Literal["left", "right", "middle"] | None = None,
    ) -> Awaitable[None]: ...
    def _mouse_wheel(self, delta_x: float, delta_y: float) -> Awaitable[None]: ...
    def _keyboard_down(self, key: str) -> Awaitable[None]: ...
    def _keyboard_up(self, key: str) -> Awaitable[None]: ...
    def _keyboard_press(self, key: str, *, delay: float | None = None) -> Awaitable[None]: ...
    def _keyboard_type(self, text: str, *, delay: float | None = None) -> Awaitable[None]: ...
    def _keyboard_insert_text(self, text: str) -> Awaitable[None]: ...
    def _touchscreen_tap(self, x: float, y: float) -> Awaitable[None]: ...
    def _intercept(self) -> Awaitable[RouteStream]: ...
    def _emulate_media(self, *, media: str | None = None, color_scheme: str | None = None) -> Awaitable[None]: ...
    def _add_cookies(self, cookies: list[dict[str, Any]]) -> Awaitable[None]: ...
    def _clear_cookies(self) -> Awaitable[None]: ...
    def _api_fetch(
        self,
        url: str,
        *,
        method: str | None = None,
        headers: dict[str, str] | None = None,
        params: Sequence[tuple[str, str]] | None = None,
        data: str | bytes | None = None,
        fail_on_status_code: bool = False,
        max_redirects: int | None = None,
        timeout: float | None = None,
    ) -> Awaitable[ApiResponse]: ...
    def bring_to_front(self) -> Awaitable[None]: ...
    def _target_id(self) -> str: ...
    def _opener_target_id(self) -> str | None: ...
    def _popups(self) -> PopupStream: ...
    def _main_frame_id(self) -> str: ...
    def _frames(self) -> list[FrameInfo]: ...
    def _frame_goto(
        self,
        frame_id: str,
        url: str,
        *,
        wait_until: Literal["load", "domcontentloaded", "networkidle", "commit"] | None = None,
        timeout: float | None = None,
    ) -> Awaitable[None]: ...
    def _frame_evaluate(self, frame_id: str, expression: str, arg: Any = None) -> Awaitable[Any]: ...
    def _frame_by_id(self, frame_id: str) -> FrameLocator: ...
    def _new_cdp_session(self, frame_id: str | None = None) -> Awaitable[CdpSession]: ...
    def _websockets(self) -> WebSocketStream: ...
    def _workers(self) -> list[WorkerInfo]: ...
    def _worker_evaluate(self, worker_id: str, expression: str, arg: Any = None) -> Awaitable[Any]: ...
    def _worker_events(self) -> WorkerStream: ...
    def _start_har_recording(
        self,
        path: _Path,
        *,
        omit_content: bool | None = None,
        content: str | None = None,
        mode: str | None = None,
        url_filter: str | None = None,
    ) -> Awaitable[None]: ...

class Locator:
    """Lazy, like Playwright's: every action runs the query again."""

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
    ) -> Awaitable[None]:
        """``timeout`` is in milliseconds, for this call only; 0 waits as long as it takes."""
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
    ) -> Awaitable[None]: ...
    def hover(
        self,
        *,
        position: Position | None = None,
        modifiers: Sequence[Literal["Alt", "Control", "Meta", "Shift", "ControlOrMeta"]] | None = None,
        force: bool | None = None,
        trial: bool | None = None,
        timeout: float | None = None,
    ) -> Awaitable[None]: ...
    def tap(
        self,
        *,
        position: Position | None = None,
        modifiers: Sequence[Literal["Alt", "Control", "Meta", "Shift", "ControlOrMeta"]] | None = None,
        force: bool | None = None,
        trial: bool | None = None,
        timeout: float | None = None,
    ) -> Awaitable[None]: ...
    def focus(self, *, timeout: float | None = None) -> Awaitable[None]: ...
    def blur(self, *, timeout: float | None = None) -> Awaitable[None]: ...
    def scroll_into_view_if_needed(self, *, timeout: float | None = None) -> Awaitable[None]: ...
    def drag_to(self, target: Locator, *, force: bool | None = None, timeout: float | None = None) -> Awaitable[None]: ...
    def fill(self, value: str, *, timeout: float | None = None) -> Awaitable[None]: ...
    def clear(self, *, force: bool | None = None, timeout: float | None = None) -> Awaitable[None]: ...
    def press(self, key: str, *, delay: float | None = None, timeout: float | None = None) -> Awaitable[None]: ...
    def press_sequentially(
        self, text: str, *, delay: float | None = None, timeout: float | None = None
    ) -> Awaitable[None]: ...
    def type(self, text: str, *, delay: float | None = None, timeout: float | None = None) -> Awaitable[None]: ...
    def check(
        self,
        *,
        position: Position | None = None,
        force: bool | None = None,
        trial: bool | None = None,
        timeout: float | None = None,
    ) -> Awaitable[None]: ...
    def uncheck(
        self,
        *,
        position: Position | None = None,
        force: bool | None = None,
        trial: bool | None = None,
        timeout: float | None = None,
    ) -> Awaitable[None]: ...
    def set_checked(
        self,
        checked: bool,
        *,
        position: Position | None = None,
        force: bool | None = None,
        trial: bool | None = None,
        timeout: float | None = None,
    ) -> Awaitable[None]: ...
    def select_option(
        self,
        value: str | Sequence[str] | Mapping[str, Any] | Sequence[Mapping[str, Any]] | None = None,
        *,
        index: int | Sequence[int] | None = None,
        label: str | Sequence[str] | None = None,
        element: Any = None,
        force: bool | None = None,
        timeout: float | None = None,
    ) -> Awaitable[list[str]]: ...
    def set_input_files(
        self,
        files: str | PathLike[str] | FilePayload | Sequence[str | PathLike[str] | FilePayload],
        *,
        timeout: float | None = None,
    ) -> Awaitable[None]: ...
    def wait_for(
        self,
        *,
        state: Literal["attached", "detached", "visible", "hidden"] | None = None,
        timeout: float | None = None,
    ) -> Awaitable[None]:
        """Waits until ``state``: visible by default."""
    def text_content(self, *, timeout: float | None = None) -> Awaitable[str | None]: ...
    def inner_text(self, *, timeout: float | None = None) -> Awaitable[str]: ...
    def inner_html(self, *, timeout: float | None = None) -> Awaitable[str]: ...
    def input_value(self, *, timeout: float | None = None) -> Awaitable[str]: ...
    def get_attribute(self, name: str, *, timeout: float | None = None) -> Awaitable[str | None]: ...
    def count(self) -> Awaitable[int]: ...
    def all(self) -> Awaitable[list[Locator]]: ...
    def all_text_contents(self) -> Awaitable[list[str]]: ...
    def all_inner_texts(self) -> Awaitable[list[str]]: ...
    def is_visible(self, *, timeout: float | None = None) -> Awaitable[bool]: ...
    def is_hidden(self, *, timeout: float | None = None) -> Awaitable[bool]: ...
    def is_enabled(self, *, timeout: float | None = None) -> Awaitable[bool]: ...
    def is_disabled(self, *, timeout: float | None = None) -> Awaitable[bool]: ...
    def is_checked(self, *, timeout: float | None = None) -> Awaitable[bool]: ...
    def is_editable(self, *, timeout: float | None = None) -> Awaitable[bool]: ...
    def screenshot(
        self,
        *,
        path: _Path | None = None,
        type: Literal["png", "jpeg"] | None = None,
        quality: int | None = None,
        omit_background: bool | None = None,
        timeout: float | None = None,
    ) -> Awaitable[bytes]:
        """PNG or JPEG of the element, once it is visible and still; with ``path``, also written there."""
    def bounding_box(self, *, timeout: float | None = None) -> Awaitable[FloatRect | None]:
        """``{x, y, width, height}`` relative to the viewport, without scrolling; ``None`` when
        the element is not visible."""
    def evaluate(self, expression: str, arg: Any = None, *, timeout: float | None = None) -> Awaitable[Any]:
        """Calls the JavaScript function ``expression`` with the element and ``arg``."""
    def nth(self, index: int) -> Locator:
        """The ``index``-th match (0-based; negative counts from the end)."""
    @property
    def first(self) -> Locator: ...
    @property
    def last(self) -> Locator: ...
    def filter(
        self,
        *,
        has: Locator | None = None,
        has_not: Locator | None = None,
        has_text: str | re.Pattern[str] | None = None,
        has_not_text: str | re.Pattern[str] | None = None,
    ) -> Locator:
        """Narrow matching elements."""
    def and_(self, locator: Locator) -> Locator: ...
    def or_(self, locator: Locator) -> Locator: ...
    def locator(
        self,
        selector: str,
        *,
        has: Locator | None = None,
        has_not: Locator | None = None,
        has_text: str | re.Pattern[str] | None = None,
        has_not_text: str | re.Pattern[str] | None = None,
    ) -> Locator:
        """Searches inside this locator's matches."""
    def get_by_text(self, text: str, *, exact: bool | None = None) -> Locator: ...
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
    ) -> Locator: ...
    def get_by_label(self, text: str, *, exact: bool | None = None) -> Locator: ...
    def get_by_placeholder(self, text: str, *, exact: bool | None = None) -> Locator: ...
    def get_by_alt_text(self, text: str, *, exact: bool | None = None) -> Locator: ...
    def get_by_title(self, text: str, *, exact: bool | None = None) -> Locator: ...
    def get_by_test_id(self, test_id: str) -> Locator: ...

class FrameLocator:
    """An iframe, same- or cross-origin, to find elements in."""

    def locator(
        self,
        selector: str,
        *,
        has: Locator | None = None,
        has_not: Locator | None = None,
        has_text: str | re.Pattern[str] | None = None,
        has_not_text: str | re.Pattern[str] | None = None,
    ) -> Locator: ...
    def get_by_text(self, text: str, *, exact: bool | None = None) -> Locator: ...
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
    ) -> Locator: ...
    def get_by_label(self, text: str, *, exact: bool | None = None) -> Locator: ...
    def get_by_placeholder(self, text: str, *, exact: bool | None = None) -> Locator: ...
    def get_by_alt_text(self, text: str, *, exact: bool | None = None) -> Locator: ...
    def get_by_title(self, text: str, *, exact: bool | None = None) -> Locator: ...
    def get_by_test_id(self, test_id: str) -> Locator: ...
    def frame_locator(self, selector: str) -> FrameLocator:
        """A nested iframe inside this one."""

class Route:
    url: str
    method: str
    headers: dict[str, str]
    post_data: str | None
    resource_type: str
    def fetch(
        self,
        *,
        url: str | None = None,
        method: str | None = None,
        headers: dict[str, str] | None = None,
        post_data: str | bytes | None = None,
        max_redirects: int | None = None,
        timeout: float | None = None,
    ) -> Awaitable[ApiResponse]: ...
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

class NetworkRequest:
    id: str
    url: str
    method: str
    headers: dict[str, str]
    post_data: str | None
    resource_type: str

class NetworkResponse:
    url: str
    status: int
    status_text: str
    headers: dict[str, str]
    request: NetworkRequest
    def body(self, *, timeout: float | None = None) -> Awaitable[bytes]: ...

class NetworkEvent:
    event: str
    request: NetworkRequest
    response: NetworkResponse | None
    failure: str | None

class NetworkStream:
    def next(self) -> Awaitable[NetworkEvent | None]: ...

class Dialog:
    type: str
    message: str
    default_value: str
    def accept(self, prompt_text: str | None = None) -> Awaitable[None]: ...
    def dismiss(self) -> Awaitable[None]: ...

class DialogStream:
    def next(self) -> Awaitable[Dialog | None]: ...

class FileChooser:
    def is_multiple(self) -> bool: ...
    def set_files(
        self,
        files: str | PathLike[str] | FilePayload | Sequence[str | PathLike[str] | FilePayload],
        *,
        timeout: float | None = None,
    ) -> Awaitable[None]: ...

class FileChooserStream:
    def next(self) -> Awaitable[FileChooser | None]: ...

class BindingCall:
    @property
    def name(self) -> str: ...
    @property
    def args_json(self) -> str: ...
    def resolve(self, value_json: str) -> Awaitable[None]: ...
    def reject(self, message: str) -> Awaitable[None]: ...

class BindingStream:
    def next(self) -> Awaitable[BindingCall | None]: ...


class PopupStream:
    def next(self) -> Awaitable[Lease | None]: ...


class CdpSessionEvent:
    method: str
    params_json: str


class CdpEventStream:
    def next(self) -> Awaitable[CdpSessionEvent | None]: ...


class CdpSession:
    def send(self, method: str, params: Mapping[str, Any] | None = None) -> Awaitable[Any]: ...
    def _events(self) -> CdpEventStream: ...
    def detach(self) -> Awaitable[None]: ...


class WebSocketEvent:
    event: str
    request_id: str
    url: str | None
    payload_text: str | None
    payload_bytes: bytes | None
    error_message: str | None


class WebSocketStream:
    def next(self) -> Awaitable[WebSocketEvent | None]: ...


class WorkerInfo:
    id: str
    url: str


class WorkerEvent:
    event: str
    id: str
    url: str | None
    message: ConsoleMessage | None


class WorkerStream:
    def next(self) -> Awaitable[WorkerEvent | None]: ...
