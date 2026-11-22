"""Playwright-style auto-retrying ``expect()`` assertions for async and sync APIs."""

from __future__ import annotations

import asyncio
import contextvars
import math
import re
import time
from collections.abc import Awaitable, Callable, Iterator, Mapping, Sequence
from contextlib import contextmanager, suppress
from os import PathLike
from pathlib import Path
from typing import TYPE_CHECKING, Any, Literal, overload
from urllib.parse import urljoin

try:
    from . import _native
except ImportError:  # pragma: no cover
    _native = None  # type: ignore[assignment]
from ._errors import Error
from ._types import FloatRect
from ._visual import (
    DISABLE_ANIMATIONS_LOCATOR_JS,
    DISABLE_ANIMATIONS_PAGE_JS,
    RESTORE_ANIMATIONS_LOCATOR_JS,
    RESTORE_ANIMATIONS_PAGE_JS,
    RgbaImage,
    artifact_paths,
    compare_images,
    decode_png,
    should_update_snapshots,
)

if TYPE_CHECKING:
    from .async_api import Locator as AsyncLocator
    from .async_api import Page as AsyncPage
    from .sync_api import Locator as SyncLocator
    from .sync_api import Page as SyncPage

DEFAULT_TIMEOUT_MS = 5000.0
POLL_INTERVAL_S = 0.05
MIN_PROBE_TIMEOUT_MS = 50.0
MAX_PROBE_TIMEOUT_MS = 250.0

_ProbeKind = Literal["value", "not-attached", "transient"]
_ProbeResult = tuple[_ProbeKind, bool, str]

_soft_errors_var: contextvars.ContextVar[list[AssertionError] | None] = contextvars.ContextVar(
    "fluxwright_soft_errors", default=None
)
_default_soft_errors: list[AssertionError] = []


def _get_soft_errors_store() -> list[AssertionError]:
    scoped = _soft_errors_var.get()
    return _default_soft_errors if scoped is None else scoped


def get_soft_errors() -> list[AssertionError]:
    """Returns a copy of soft assertion errors recorded in the current scope."""
    return list(_get_soft_errors_store())


def clear_soft_errors() -> list[AssertionError]:
    """Clears and returns all soft assertion errors recorded in the current scope."""
    store = _get_soft_errors_store()
    errors = list(store)
    store.clear()
    return errors


def format_soft_errors(errors: Sequence[AssertionError]) -> AssertionError:
    if len(errors) == 1:
        return errors[0]
    lines = [f"{len(errors)} soft assertion failures:"]
    for idx, err in enumerate(errors, 1):
        lines.append(f"  {idx}. {err}")
    return AssertionError("\n".join(lines))


def assert_no_soft_errors() -> None:
    """Consumes recorded soft assertion errors and raises if any failed."""
    errors = clear_soft_errors()
    if errors:
        raise format_soft_errors(errors)


def _record_soft_error(exc: AssertionError, message: str | None = None) -> None:
    err = AssertionError(f"{message}\n{exc}") if message else exc
    _get_soft_errors_store().append(err)


@contextmanager
def soft_assertions() -> Iterator[list[AssertionError]]:
    """Scopes ``expect.soft`` failures to this block and raises on exit if any failed."""
    bucket: list[AssertionError] = []
    token = _soft_errors_var.set(bucket)
    try:
        yield bucket
    finally:
        _soft_errors_var.reset(token)
    if bucket:
        raise format_soft_errors(bucket)


def _normalize_ws(text: str) -> str:
    return " ".join(text.split())


def _validate_pattern(expected: str | re.Pattern[str], method: str) -> None:
    if isinstance(expected, re.Pattern):
        if isinstance(expected.pattern, bytes):
            raise TypeError(f"{method}: expected a str or re.Pattern[str], got re.Pattern[bytes]")
        return
    if not isinstance(expected, str):
        raise TypeError(f"{method}: expected a str or re.Pattern[str], got {type(expected).__name__}")


def _validate_timeout(timeout: float | None, default_ms: float) -> float:
    if timeout is None:
        return default_ms
    if (
        isinstance(timeout, bool)
        or not isinstance(timeout, (int, float))
        or math.isnan(timeout)
        or math.isinf(timeout)
        or timeout < 0
    ):
        raise ValueError("timeout must be a non-negative finite number")
    return float(timeout)


def _match_pattern(
    actual: str,
    expected: str | re.Pattern[str],
    *,
    normalize: bool = False,
    substring: bool = False,
) -> bool:
    actual_val = _normalize_ws(actual) if normalize else actual
    if isinstance(expected, re.Pattern):
        return expected.search(actual_val) is not None
    expected_val = _normalize_ws(expected) if normalize else expected
    return (expected_val in actual_val) if substring else (actual_val == expected_val)


def _format_expected(expected: str | re.Pattern[str], *, normalize: bool = False) -> str:
    if isinstance(expected, re.Pattern):
        return f"/{expected.pattern}/"
    return repr(_normalize_ws(expected) if normalize else expected)


def _probe_timeout(remaining_ms: float | None) -> float:
    if remaining_ms is None:
        return MAX_PROBE_TIMEOUT_MS
    return max(MIN_PROBE_TIMEOUT_MS, min(remaining_ms, MAX_PROBE_TIMEOUT_MS))


def _is_fatal_error(exc: Exception) -> bool:
    return bool(
        re.search(
            r"page (is )?closed|browser process exited|session .* is gone|target crashed|"
            r"invalid selector:|devtools websocket:|engine is shut down|"
            r"browser context is closed|target closed|session with given id not found",
            str(exc),
            re.IGNORECASE,
        )
    )


def _resolve_url(page: Any, expected: str | re.Pattern[str]) -> str | re.Pattern[str]:
    if not isinstance(expected, str):
        return expected
    base_url = page._base_url()
    if base_url is not None and not re.match(r"^[a-zA-Z][a-zA-Z0-9+.-]*:", expected):
        return urljoin(base_url, expected)
    return expected


async def _poll_until(
    *,
    target_label: str,
    matcher: str,
    expected_desc: str,
    negated: bool,
    timeout_ms: float,
    probe: Callable[[float], Awaitable[_ProbeResult]],
) -> None:
    unlimited = timeout_ms == 0.0
    deadline = None if unlimited else time.monotonic() + (timeout_ms / 1000.0)
    last_known_desc: str | None = None
    while True:
        remaining_ms = None if deadline is None else max(0.0, (deadline - time.monotonic()) * 1000.0)
        try:
            kind, matched, actual_desc = await probe(_probe_timeout(remaining_ms))
        except Exception as exc:
            if _is_fatal_error(exc):
                raise
            kind, matched, actual_desc = "transient", False, f"error: {exc}"

        if kind == "value":
            ok = (not matched) if negated else matched
            if ok:
                return
            last_known_desc = actual_desc
        elif kind == "not-attached":
            actual_desc = "element not found (count: 0)"
            last_known_desc = actual_desc
        elif last_known_desc is not None:
            actual_desc = last_known_desc

        if deadline is not None:
            rem_s = deadline - time.monotonic()
            if rem_s <= 0:
                exp_text = f"not {expected_desc}" if negated else expected_desc
                raise AssertionError(
                    f"{target_label}.{matcher}: Timed out {timeout_ms:g}ms waiting for assertion.\n"
                    f"Expected: {exp_text}\n"
                    f"Actual: {actual_desc}"
                )
            await asyncio.sleep(min(POLL_INTERVAL_S, rem_s))
        else:
            await asyncio.sleep(POLL_INTERVAL_S)


class LocatorAssertions:
    """Auto-retrying assertions for an async ``Locator``."""

    def __init__(
        self,
        locator: AsyncLocator | _native.Locator,
        default_timeout: float = DEFAULT_TIMEOUT_MS,
        *,
        soft: bool = False,
        message: str | None = None,
    ) -> None:
        self._locator = locator
        impl = getattr(locator, "_impl", None)
        self._raw_locator: Any = impl if (_native is not None and isinstance(impl, _native.Locator)) else locator
        self._default_timeout = default_timeout
        self._soft = soft
        self._message = message

    def _owner_page(self) -> Any:
        fn = getattr(self._locator, "_owner_page", None)
        if callable(fn):
            return fn()
        return None

    def _target(self) -> str:
        return f"expect({self._locator!r})"

    async def _run_checked(self, coro: Awaitable[None]) -> None:
        try:
            await coro
        except AssertionError as exc:
            if self._soft:
                _record_soft_error(exc, self._message)
                return
            if self._message:
                raise AssertionError(f"{self._message}\n{exc}") from None
            raise

    async def _ensure_attached(self, limit_ms: float) -> _ProbeResult | None:
        try:
            n = await asyncio.wait_for(self._raw_locator.count(), timeout=limit_ms / 1000.0)
        except asyncio.TimeoutError:
            return ("transient", False, "timed out waiting for element count")
        except Exception as exc:
            if _is_fatal_error(exc):
                raise
            return ("transient", False, f"error: {exc}")
        if n == 0:
            return ("not-attached", False, "element not found (count: 0)")
        return None

    async def _bool_check(
        self,
        *,
        matcher: str,
        expected: str,
        true_label: str,
        false_label: str,
        require_attached: bool,
        negated: bool,
        timeout: float | None,
        call: Callable[[float], Awaitable[bool]],
    ) -> None:
        timeout_ms = _validate_timeout(timeout, self._default_timeout)

        async def probe(limit_ms: float) -> _ProbeResult:
            if require_attached:
                early = await self._ensure_attached(limit_ms)
                if early is not None:
                    return early
            val = await call(limit_ms)
            return ("value", val, true_label if val else false_label)

        await self._run_checked(
            _poll_until(
                target_label=self._target(),
                matcher=matcher,
                expected_desc=expected,
                negated=negated,
                timeout_ms=timeout_ms,
                probe=probe,
            )
        )

    async def _attached_check(self, *, matcher: str, negated: bool, timeout: float | None) -> None:
        timeout_ms = _validate_timeout(timeout, self._default_timeout)

        async def probe(limit_ms: float) -> _ProbeResult:
            n = await asyncio.wait_for(self._raw_locator.count(), timeout=limit_ms / 1000.0)
            if n == 0:
                return ("value", False, "detached (count: 0)")
            if n == 1:
                return ("value", True, "attached")
            await self._raw_locator.is_visible(timeout=limit_ms)
            return ("value", True, f"attached (count: {n})")

        await self._run_checked(
            _poll_until(
                target_label=self._target(),
                matcher=matcher,
                expected_desc="attached",
                negated=negated,
                timeout_ms=timeout_ms,
                probe=probe,
            )
        )

    async def _checked_check(
        self,
        *,
        matcher: str,
        negated: bool,
        checked: bool | None,
        timeout: float | None,
    ) -> None:
        if checked is not None and not isinstance(checked, bool):
            raise TypeError(f"expect(locator).{matcher}: checked must be a bool, got {type(checked).__name__}")
        want = True if checked is None else checked
        timeout_ms = _validate_timeout(timeout, self._default_timeout)

        async def probe(limit_ms: float) -> _ProbeResult:
            early = await self._ensure_attached(limit_ms)
            if early is not None:
                return early
            val = await self._raw_locator.is_checked(timeout=limit_ms)
            return ("value", val is want, "checked" if val else "unchecked")

        await self._run_checked(
            _poll_until(
                target_label=self._target(),
                matcher=matcher,
                expected_desc="checked" if want else "unchecked",
                negated=negated,
                timeout_ms=timeout_ms,
                probe=probe,
            )
        )

    async def _text_check(
        self,
        expected: str | re.Pattern[str],
        *,
        matcher: str,
        substring: bool,
        negated: bool,
        timeout: float | None,
    ) -> None:
        _validate_pattern(expected, f"expect(locator).{matcher}")
        timeout_ms = _validate_timeout(timeout, self._default_timeout)
        expected_desc = (
            f"substring {_format_expected(expected, normalize=True)}"
            if substring
            else _format_expected(expected, normalize=True)
        )

        async def probe(limit_ms: float) -> _ProbeResult:
            early = await self._ensure_attached(limit_ms)
            if early is not None:
                return early
            raw = await self._raw_locator.text_content(timeout=limit_ms) or ""
            actual_norm = _normalize_ws(raw)
            return (
                "value",
                _match_pattern(raw, expected, normalize=True, substring=substring),
                repr(actual_norm),
            )

        await self._run_checked(
            _poll_until(
                target_label=self._target(),
                matcher=matcher,
                expected_desc=expected_desc,
                negated=negated,
                timeout_ms=timeout_ms,
                probe=probe,
            )
        )

    async def _value_check(
        self,
        expected: str | re.Pattern[str],
        *,
        matcher: str,
        negated: bool,
        timeout: float | None,
    ) -> None:
        _validate_pattern(expected, f"expect(locator).{matcher}")
        timeout_ms = _validate_timeout(timeout, self._default_timeout)

        async def probe(limit_ms: float) -> _ProbeResult:
            early = await self._ensure_attached(limit_ms)
            if early is not None:
                return early
            val = await self._raw_locator.input_value(timeout=limit_ms)
            return ("value", _match_pattern(val, expected), repr(val))

        await self._run_checked(
            _poll_until(
                target_label=self._target(),
                matcher=matcher,
                expected_desc=_format_expected(expected),
                negated=negated,
                timeout_ms=timeout_ms,
                probe=probe,
            )
        )

    async def _attribute_check(
        self,
        name: str,
        expected: str | re.Pattern[str],
        *,
        matcher: str,
        negated: bool,
        timeout: float | None,
    ) -> None:
        if not isinstance(name, str) or not name:
            raise ValueError(f"expect(locator).{matcher}: name must be a non-empty str")
        _validate_pattern(expected, f"expect(locator).{matcher}")
        timeout_ms = _validate_timeout(timeout, self._default_timeout)

        async def probe(limit_ms: float) -> _ProbeResult:
            early = await self._ensure_attached(limit_ms)
            if early is not None:
                return early
            val = await self._raw_locator.get_attribute(name, timeout=limit_ms)
            if val is None:
                return ("value", False, f"{name}=<absent>")
            return ("value", _match_pattern(val, expected), f"{name}={val!r}")

        await self._run_checked(
            _poll_until(
                target_label=self._target(),
                matcher=matcher,
                expected_desc=f"{name}={_format_expected(expected)}",
                negated=negated,
                timeout_ms=timeout_ms,
                probe=probe,
            )
        )

    async def _class_check(
        self,
        expected: str | re.Pattern[str],
        *,
        matcher: str,
        negated: bool,
        timeout: float | None,
    ) -> None:
        _validate_pattern(expected, f"expect(locator).{matcher}")
        timeout_ms = _validate_timeout(timeout, self._default_timeout)

        async def probe(limit_ms: float) -> _ProbeResult:
            early = await self._ensure_attached(limit_ms)
            if early is not None:
                return early
            cls = await self._raw_locator.get_attribute("class", timeout=limit_ms) or ""
            return ("value", _match_pattern(cls, expected), repr(cls))

        await self._run_checked(
            _poll_until(
                target_label=self._target(),
                matcher=matcher,
                expected_desc=_format_expected(expected),
                negated=negated,
                timeout_ms=timeout_ms,
                probe=probe,
            )
        )

    async def _count_check(
        self,
        count: int,
        *,
        matcher: str,
        negated: bool,
        timeout: float | None,
    ) -> None:
        if isinstance(count, bool) or not isinstance(count, int) or count < 0:
            raise ValueError(f"expect(locator).{matcher}: count must be a non-negative int")
        timeout_ms = _validate_timeout(timeout, self._default_timeout)

        async def probe(limit_ms: float) -> _ProbeResult:
            n = await asyncio.wait_for(self._raw_locator.count(), timeout=limit_ms / 1000.0)
            return ("value", n == count, str(n))

        await self._run_checked(
            _poll_until(
                target_label=self._target(),
                matcher=matcher,
                expected_desc=str(count),
                negated=negated,
                timeout_ms=timeout_ms,
                probe=probe,
            )
        )

    async def to_be_visible(self, timeout: float | None = None) -> None:
        await self._bool_check(
            matcher="to_be_visible",
            expected="visible",
            true_label="visible",
            false_label="hidden",
            require_attached=False,
            negated=False,
            timeout=timeout,
            call=lambda lim: self._raw_locator.is_visible(timeout=lim),
        )

    async def not_to_be_visible(self, timeout: float | None = None) -> None:
        await self._bool_check(
            matcher="not_to_be_visible",
            expected="visible",
            true_label="visible",
            false_label="hidden",
            require_attached=False,
            negated=True,
            timeout=timeout,
            call=lambda lim: self._raw_locator.is_visible(timeout=lim),
        )

    async def to_be_hidden(self, timeout: float | None = None) -> None:
        await self._bool_check(
            matcher="to_be_hidden",
            expected="hidden",
            true_label="hidden",
            false_label="visible",
            require_attached=False,
            negated=False,
            timeout=timeout,
            call=lambda lim: self._raw_locator.is_hidden(timeout=lim),
        )

    async def not_to_be_hidden(self, timeout: float | None = None) -> None:
        await self._bool_check(
            matcher="not_to_be_hidden",
            expected="hidden",
            true_label="hidden",
            false_label="visible",
            require_attached=False,
            negated=True,
            timeout=timeout,
            call=lambda lim: self._raw_locator.is_hidden(timeout=lim),
        )

    async def to_be_attached(self, timeout: float | None = None) -> None:
        await self._attached_check(matcher="to_be_attached", negated=False, timeout=timeout)

    async def not_to_be_attached(self, timeout: float | None = None) -> None:
        await self._attached_check(matcher="not_to_be_attached", negated=True, timeout=timeout)

    async def to_be_enabled(self, timeout: float | None = None) -> None:
        await self._bool_check(
            matcher="to_be_enabled",
            expected="enabled",
            true_label="enabled",
            false_label="disabled",
            require_attached=True,
            negated=False,
            timeout=timeout,
            call=lambda lim: self._raw_locator.is_enabled(timeout=lim),
        )

    async def not_to_be_enabled(self, timeout: float | None = None) -> None:
        await self._bool_check(
            matcher="not_to_be_enabled",
            expected="enabled",
            true_label="enabled",
            false_label="disabled",
            require_attached=True,
            negated=True,
            timeout=timeout,
            call=lambda lim: self._raw_locator.is_enabled(timeout=lim),
        )

    async def to_be_disabled(self, timeout: float | None = None) -> None:
        await self._bool_check(
            matcher="to_be_disabled",
            expected="disabled",
            true_label="disabled",
            false_label="enabled",
            require_attached=True,
            negated=False,
            timeout=timeout,
            call=lambda lim: self._raw_locator.is_disabled(timeout=lim),
        )

    async def not_to_be_disabled(self, timeout: float | None = None) -> None:
        await self._bool_check(
            matcher="not_to_be_disabled",
            expected="disabled",
            true_label="disabled",
            false_label="enabled",
            require_attached=True,
            negated=True,
            timeout=timeout,
            call=lambda lim: self._raw_locator.is_disabled(timeout=lim),
        )

    async def to_be_checked(self, timeout: float | None = None, *, checked: bool | None = None) -> None:
        await self._checked_check(matcher="to_be_checked", negated=False, checked=checked, timeout=timeout)

    async def not_to_be_checked(self, timeout: float | None = None) -> None:
        await self._checked_check(matcher="not_to_be_checked", negated=True, checked=None, timeout=timeout)

    async def to_be_editable(self, timeout: float | None = None) -> None:
        await self._bool_check(
            matcher="to_be_editable",
            expected="editable",
            true_label="editable",
            false_label="not editable",
            require_attached=True,
            negated=False,
            timeout=timeout,
            call=lambda lim: self._raw_locator.is_editable(timeout=lim),
        )

    async def not_to_be_editable(self, timeout: float | None = None) -> None:
        await self._bool_check(
            matcher="not_to_be_editable",
            expected="editable",
            true_label="editable",
            false_label="not editable",
            require_attached=True,
            negated=True,
            timeout=timeout,
            call=lambda lim: self._raw_locator.is_editable(timeout=lim),
        )

    async def to_have_text(self, expected: str | re.Pattern[str], timeout: float | None = None) -> None:
        await self._text_check(expected, matcher="to_have_text", substring=False, negated=False, timeout=timeout)

    async def not_to_have_text(self, expected: str | re.Pattern[str], timeout: float | None = None) -> None:
        await self._text_check(expected, matcher="not_to_have_text", substring=False, negated=True, timeout=timeout)

    async def to_contain_text(self, expected: str | re.Pattern[str], timeout: float | None = None) -> None:
        await self._text_check(expected, matcher="to_contain_text", substring=True, negated=False, timeout=timeout)

    async def not_to_contain_text(self, expected: str | re.Pattern[str], timeout: float | None = None) -> None:
        await self._text_check(expected, matcher="not_to_contain_text", substring=True, negated=True, timeout=timeout)

    async def to_have_value(self, expected: str | re.Pattern[str], timeout: float | None = None) -> None:
        await self._value_check(expected, matcher="to_have_value", negated=False, timeout=timeout)

    async def not_to_have_value(self, expected: str | re.Pattern[str], timeout: float | None = None) -> None:
        await self._value_check(expected, matcher="not_to_have_value", negated=True, timeout=timeout)

    async def to_have_attribute(
        self, name: str, expected: str | re.Pattern[str], timeout: float | None = None
    ) -> None:
        await self._attribute_check(name, expected, matcher="to_have_attribute", negated=False, timeout=timeout)

    async def not_to_have_attribute(
        self, name: str, expected: str | re.Pattern[str], timeout: float | None = None
    ) -> None:
        await self._attribute_check(name, expected, matcher="not_to_have_attribute", negated=True, timeout=timeout)

    async def to_have_class(self, expected: str | re.Pattern[str], timeout: float | None = None) -> None:
        await self._class_check(expected, matcher="to_have_class", negated=False, timeout=timeout)

    async def not_to_have_class(self, expected: str | re.Pattern[str], timeout: float | None = None) -> None:
        await self._class_check(expected, matcher="not_to_have_class", negated=True, timeout=timeout)

    async def to_have_count(self, count: int, timeout: float | None = None) -> None:
        await self._count_check(count, matcher="to_have_count", negated=False, timeout=timeout)

    async def not_to_have_count(self, count: int, timeout: float | None = None) -> None:
        await self._count_check(count, matcher="not_to_have_count", negated=True, timeout=timeout)

    async def to_have_screenshot(
        self,
        name: str | PathLike[str] | Sequence[str],
        *,
        threshold: float | None = None,
        max_diff_pixels: int | None = None,
        max_diff_pixel_ratio: float | None = None,
        animations: Literal["disabled", "allow"] | None = None,
        timeout: float | None = None,
        omit_background: bool | None = None,
        update: bool | None = None,
    ) -> None:
        opts = _validate_screenshot_options(
            f"{self._target()}.to_have_screenshot",
            threshold=threshold,
            max_diff_pixels=max_diff_pixels,
            max_diff_pixel_ratio=max_diff_pixel_ratio,
            animations=animations,
            timeout=timeout,
            default_timeout=self._default_timeout,
            omit_background=omit_background,
            full_page=None,
            clip=None,
            update=update,
        )

        def _is_closed() -> bool:
            page = self._owner_page()
            return page is not None and bool(page.is_closed())

        async def _disable(limit_ms: float) -> None:
            with suppress(Exception):
                await self._raw_locator.evaluate(DISABLE_ANIMATIONS_LOCATOR_JS, None, timeout=min(limit_ms, 500.0))

        async def _restore() -> None:
            removed = False
            page = self._owner_page()
            if page is not None and not page.is_closed():
                with suppress(Exception):
                    if _native is not None and isinstance(page, _native.Page):
                        removed = bool(await _native.Page.evaluate(page, RESTORE_ANIMATIONS_PAGE_JS))
                    else:
                        removed = bool(await page.evaluate(RESTORE_ANIMATIONS_PAGE_JS))
            if not removed:
                with suppress(Exception):
                    await self._raw_locator.evaluate(RESTORE_ANIMATIONS_LOCATOR_JS, None, timeout=250.0)

        await self._run_checked(
            _run_to_have_screenshot(
                target_label=self._target(),
                name=name,
                opts=opts,
                is_closed=_is_closed,
                disable_animations=_disable,
                restore_animations=_restore,
                capture_shot=lambda limit_ms: self._raw_locator.screenshot(
                    omit_background=opts["omit_background"],
                    timeout=limit_ms,
                ),
            )
        )


class PageAssertions:
    """Auto-retrying assertions for an async ``Page``."""

    def __init__(
        self,
        page: AsyncPage,
        default_timeout: float = DEFAULT_TIMEOUT_MS,
        *,
        soft: bool = False,
        message: str | None = None,
    ) -> None:
        self._page = page
        self._default_timeout = default_timeout
        self._soft = soft
        self._message = message

    async def _run_checked(self, coro: Awaitable[None]) -> None:
        try:
            await coro
        except AssertionError as exc:
            if self._soft:
                _record_soft_error(exc, self._message)
                return
            if self._message:
                raise AssertionError(f"{self._message}\n{exc}") from None
            raise

    async def _title_check(
        self,
        expected: str | re.Pattern[str],
        *,
        matcher: str,
        negated: bool,
        timeout: float | None,
    ) -> None:
        _validate_pattern(expected, f"expect(page).{matcher}")
        timeout_ms = _validate_timeout(timeout, self._default_timeout)

        async def probe(limit_ms: float) -> _ProbeResult:
            if self._page.is_closed():
                raise Error("page is closed")
            raw = await asyncio.wait_for(self._page.title(), timeout=limit_ms / 1000.0)
            actual_norm = _normalize_ws(raw)
            return ("value", _match_pattern(raw, expected, normalize=True), repr(actual_norm))

        await self._run_checked(
            _poll_until(
                target_label="expect(page)",
                matcher=matcher,
                expected_desc=_format_expected(expected, normalize=True),
                negated=negated,
                timeout_ms=timeout_ms,
                probe=probe,
            )
        )

    async def _url_check(
        self,
        expected: str | re.Pattern[str],
        *,
        matcher: str,
        negated: bool,
        timeout: float | None,
    ) -> None:
        _validate_pattern(expected, f"expect(page).{matcher}")
        timeout_ms = _validate_timeout(timeout, self._default_timeout)
        target = _resolve_url(self._page, expected)

        async def probe(_limit_ms: float) -> _ProbeResult:
            if self._page.is_closed():
                raise Error("page is closed")
            actual = self._page.url
            return ("value", _match_pattern(actual, target), repr(actual))

        await self._run_checked(
            _poll_until(
                target_label="expect(page)",
                matcher=matcher,
                expected_desc=_format_expected(target),
                negated=negated,
                timeout_ms=timeout_ms,
                probe=probe,
            )
        )

    async def to_have_title(self, expected: str | re.Pattern[str], timeout: float | None = None) -> None:
        await self._title_check(expected, matcher="to_have_title", negated=False, timeout=timeout)

    async def not_to_have_title(self, expected: str | re.Pattern[str], timeout: float | None = None) -> None:
        await self._title_check(expected, matcher="not_to_have_title", negated=True, timeout=timeout)

    async def to_have_url(self, expected: str | re.Pattern[str], timeout: float | None = None) -> None:
        await self._url_check(expected, matcher="to_have_url", negated=False, timeout=timeout)

    async def not_to_have_url(self, expected: str | re.Pattern[str], timeout: float | None = None) -> None:
        await self._url_check(expected, matcher="not_to_have_url", negated=True, timeout=timeout)

    async def to_have_screenshot(
        self,
        name: str | PathLike[str] | Sequence[str],
        *,
        threshold: float | None = None,
        max_diff_pixels: int | None = None,
        max_diff_pixel_ratio: float | None = None,
        animations: Literal["disabled", "allow"] | None = None,
        timeout: float | None = None,
        full_page: bool | None = None,
        omit_background: bool | None = None,
        clip: FloatRect | None = None,
        update: bool | None = None,
    ) -> None:
        opts = _validate_screenshot_options(
            "expect(page).to_have_screenshot",
            threshold=threshold,
            max_diff_pixels=max_diff_pixels,
            max_diff_pixel_ratio=max_diff_pixel_ratio,
            animations=animations,
            timeout=timeout,
            default_timeout=self._default_timeout,
            omit_background=omit_background,
            full_page=full_page,
            clip=clip,
            update=update,
        )

        is_native_page = _native is not None and isinstance(self._page, _native.Page)

        async def _disable(_limit_ms: float) -> None:
            with suppress(Exception):
                if is_native_page:
                    await _native.Page.evaluate(self._page, DISABLE_ANIMATIONS_PAGE_JS)
                else:
                    await self._page.evaluate(DISABLE_ANIMATIONS_PAGE_JS)

        async def _restore() -> None:
            with suppress(Exception):
                if is_native_page:
                    await _native.Page.evaluate(self._page, RESTORE_ANIMATIONS_PAGE_JS)
                else:
                    await self._page.evaluate(RESTORE_ANIMATIONS_PAGE_JS)

        def _capture(limit_ms: float) -> Awaitable[bytes]:
            if is_native_page:
                return _native.Page.screenshot(
                    self._page,
                    full_page=opts["full_page"],
                    clip=opts["clip"],
                    omit_background=opts["omit_background"],
                    timeout=limit_ms,
                )
            return self._page.screenshot(
                full_page=opts["full_page"],
                clip=opts["clip"],
                omit_background=opts["omit_background"],
                timeout=limit_ms,
            )

        await self._run_checked(
            _run_to_have_screenshot(
                target_label="expect(page)",
                name=name,
                opts=opts,
                is_closed=self._page.is_closed,
                disable_animations=_disable,
                restore_animations=_restore,
                capture_shot=_capture,
            )
        )


def _validate_clip(method: str, clip: Any) -> FloatRect | None:
    if clip is None:
        return None
    if not isinstance(clip, Mapping):
        raise TypeError(f"{method}: clip must be a mapping with x, y, width, height")
    expected_keys = {"x", "y", "width", "height"}
    keys = set(clip.keys())
    unknown = keys - expected_keys
    if unknown:
        raise ValueError(f"{method}: clip has unsupported keys {sorted(unknown)}")
    missing = expected_keys - keys
    if missing:
        raise ValueError(f"{method}: clip is missing required keys {sorted(missing)}")
    parsed: dict[str, float] = {}
    for k in ("x", "y", "width", "height"):
        v = clip[k]
        if isinstance(v, bool) or not isinstance(v, (int, float)) or math.isnan(v) or math.isinf(v):
            raise ValueError(f"{method}: clip.{k} must be a finite number")
        fv = float(v)
        if k in ("width", "height") and fv <= 0:
            raise ValueError(f"{method}: clip.{k} must be > 0")
        parsed[k] = fv
    return {
        "x": parsed["x"],
        "y": parsed["y"],
        "width": parsed["width"],
        "height": parsed["height"],
    }


def _validate_screenshot_options(
    method: str,
    *,
    threshold: float | None,
    max_diff_pixels: int | None,
    max_diff_pixel_ratio: float | None,
    animations: str | None,
    timeout: float | None,
    default_timeout: float,
    omit_background: bool | None,
    full_page: bool | None,
    clip: FloatRect | None,
    update: bool | None,
) -> dict[str, Any]:
    timeout_ms = _validate_timeout(timeout, default_timeout)
    if threshold is not None and (isinstance(threshold, bool) or not isinstance(threshold, (int, float))):
        raise ValueError(f"{method}: threshold must be a finite float in [0.0, 1.0]")
    thresh = 0.2 if threshold is None else float(threshold)
    if math.isnan(thresh) or math.isinf(thresh) or not (0.0 <= thresh <= 1.0):
        raise ValueError(f"{method}: threshold must be a finite float in [0.0, 1.0]")
    if max_diff_pixels is not None and (
        isinstance(max_diff_pixels, bool) or not isinstance(max_diff_pixels, int) or max_diff_pixels < 0
    ):
        raise ValueError(f"{method}: max_diff_pixels must be a non-negative int")
    if max_diff_pixel_ratio is not None:
        if (
            isinstance(max_diff_pixel_ratio, bool)
            or not isinstance(max_diff_pixel_ratio, (int, float))
            or math.isnan(max_diff_pixel_ratio)
            or math.isinf(max_diff_pixel_ratio)
            or not (0.0 <= max_diff_pixel_ratio <= 1.0)
        ):
            raise ValueError(f"{method}: max_diff_pixel_ratio must be a finite float in [0.0, 1.0]")
    anim = "disabled" if animations is None else animations
    if anim not in ("disabled", "allow"):
        raise ValueError(f"{method}: animations must be 'disabled' or 'allow', not {animations!r}")
    for opt_name, opt_val in (
        ("omit_background", omit_background),
        ("full_page", full_page),
        ("update", update),
    ):
        if opt_val is not None and not isinstance(opt_val, bool):
            raise TypeError(f"{method}: {opt_name} must be a bool, got {type(opt_val).__name__}")
    validated_clip = _validate_clip(method, clip)
    return {
        "timeout_ms": timeout_ms,
        "threshold": thresh,
        "max_diff_pixels": max_diff_pixels,
        "max_diff_pixel_ratio": float(max_diff_pixel_ratio) if max_diff_pixel_ratio is not None else None,
        "animations": anim,
        "omit_background": False if omit_background is None else omit_background,
        "full_page": False if full_page is None else full_page,
        "clip": validated_clip,
        "update": update,
    }


def _shot_limit(remaining_ms: float | None) -> float:
    if remaining_ms is None:
        return 2000.0
    return max(100.0, min(remaining_ms, 2000.0))


async def _write_artifact(path: Path, data: bytes) -> None:
    def _write() -> None:
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(data)

    await asyncio.to_thread(_write)


async def _run_to_have_screenshot(
    *,
    target_label: str,
    name: str | PathLike[str] | Sequence[str],
    opts: dict[str, Any],
    is_closed: Callable[[], bool],
    disable_animations: Callable[[float], Awaitable[None]],
    restore_animations: Callable[[], Awaitable[None]],
    capture_shot: Callable[[float], Awaitable[bytes]],
) -> None:
    baseline_path, actual_path, diff_path = artifact_paths(name)
    timeout_ms: float = opts["timeout_ms"]
    disable_anim: bool = opts["animations"] == "disabled"
    unlimited = timeout_ms == 0.0

    try:
        if should_update_snapshots(opts["update"]):
            deadline = None if unlimited else time.monotonic() + (timeout_ms / 1000.0)
            prev_img: RgbaImage | None = None
            last_err: str | None = None
            attempts = 0
            while True:
                if is_closed():
                    raise Error("page is closed")
                rem_ms = None if deadline is None else max(0.0, (deadline - time.monotonic()) * 1000.0)
                limit = _shot_limit(rem_ms)
                if disable_anim:
                    await disable_animations(limit)
                stabilized_raw: bytes | None = None
                try:
                    raw = await capture_shot(limit)
                    attempts += 1
                    img = decode_png(raw)
                    if prev_img is not None:
                        diff = compare_images(prev_img, img, threshold=opts["threshold"], render_diff=False)
                        if diff.is_within_tolerance(
                            max_diff_pixels=opts["max_diff_pixels"],
                            max_diff_pixel_ratio=opts["max_diff_pixel_ratio"],
                        ):
                            stabilized_raw = raw
                    prev_img = img
                    last_err = None
                except Exception as exc:
                    if _is_fatal_error(exc):
                        raise
                    last_err = str(exc)

                if stabilized_raw is not None:
                    await _write_artifact(baseline_path, stabilized_raw)
                    return

                if deadline is not None:
                    rem_s = deadline - time.monotonic()
                    if rem_s <= 0:
                        if attempts == 1 and prev_img is not None:
                            final_raw: bytes | None = None
                            with suppress(Exception):
                                raw = await capture_shot(1000.0)
                                img = decode_png(raw)
                                diff = compare_images(prev_img, img, threshold=opts["threshold"], render_diff=False)
                                if diff.is_within_tolerance(
                                    max_diff_pixels=opts["max_diff_pixels"],
                                    max_diff_pixel_ratio=opts["max_diff_pixel_ratio"],
                                ):
                                    final_raw = raw
                            if final_raw is not None:
                                await _write_artifact(baseline_path, final_raw)
                                return
                        if attempts >= 2:
                            raise AssertionError(
                                f"{target_label}.to_have_screenshot: Timed out {timeout_ms:g}ms "
                                f"waiting for screenshot to stabilize for update at {baseline_path}"
                            )
                        raise AssertionError(
                            f"{target_label}.to_have_screenshot: Failed to capture snapshot "
                            f"({last_err or 'did not stabilize'})"
                        )
                    await asyncio.sleep(min(POLL_INTERVAL_S, rem_s))
                else:
                    await asyncio.sleep(POLL_INTERVAL_S)

        try:
            baseline_bytes = await asyncio.to_thread(baseline_path.read_bytes)
        except FileNotFoundError:
            if disable_anim:
                await disable_animations(500.0)
            with suppress(Exception):
                actual_raw = await capture_shot(2000.0)
                await _write_artifact(actual_path, actual_raw)
            raise AssertionError(
                f"{target_label}.to_have_screenshot: Baseline snapshot does not exist at {baseline_path}.\n"
                f"Run with FLUXWRIGHT_UPDATE_SNAPSHOTS=1 or update=True to create it. Actual saved to {actual_path}"
            ) from None

        baseline_img = decode_png(baseline_bytes)
        deadline = None if unlimited else time.monotonic() + (timeout_ms / 1000.0)
        last_mismatch: tuple[bytes, RgbaImage] | None = None
        last_err = None

        while True:
            if is_closed():
                raise Error("page is closed")
            rem_ms = None if deadline is None else max(0.0, (deadline - time.monotonic()) * 1000.0)
            limit = _shot_limit(rem_ms)
            if disable_anim:
                await disable_animations(limit)
            try:
                raw = await capture_shot(limit)
                actual_img = decode_png(raw)
                diff = compare_images(baseline_img, actual_img, threshold=opts["threshold"], render_diff=False)
                if diff.is_within_tolerance(
                    max_diff_pixels=opts["max_diff_pixels"],
                    max_diff_pixel_ratio=opts["max_diff_pixel_ratio"],
                ):
                    return
                last_mismatch = (raw, actual_img)
                last_err = None
            except Exception as exc:
                if _is_fatal_error(exc):
                    raise
                last_err = str(exc)

            if deadline is not None:
                rem_s = deadline - time.monotonic()
                if rem_s <= 0:
                    break
                await asyncio.sleep(min(POLL_INTERVAL_S, rem_s))
            else:
                await asyncio.sleep(POLL_INTERVAL_S)

        if last_mismatch is not None:
            actual_raw, actual_img = last_mismatch
            final_diff = compare_images(baseline_img, actual_img, threshold=opts["threshold"], render_diff=True)
            await _write_artifact(actual_path, actual_raw)
            if final_diff.diff_png is not None:
                await _write_artifact(diff_path, final_diff.diff_png)
            raise AssertionError(
                f"{target_label}.to_have_screenshot: Screenshot mismatch ({final_diff.diff_pixels} of "
                f"{final_diff.total_pixels} pixels differ, ratio {final_diff.diff_ratio:.6f}; "
                f"expected {final_diff.expected_size[0]}x{final_diff.expected_size[1]}, "
                f"actual {final_diff.actual_size[0]}x{final_diff.actual_size[1]}).\n"
                f"Baseline: {baseline_path}\nActual:   {actual_path}\nDiff:     {diff_path}"
            )

        raise AssertionError(
            f"{target_label}.to_have_screenshot: Timed out {timeout_ms:g}ms waiting for screenshot "
            f"({last_err or 'unknown error'})"
        )
    finally:
        if disable_anim and not is_closed():
            await restore_animations()


class _AsyncExpect:
    def __init__(self, default_timeout: float = DEFAULT_TIMEOUT_MS, *, soft: bool = False) -> None:
        self._default_timeout = default_timeout
        self._soft = soft

    @overload
    def __call__(self, actual: AsyncPage, message: str | None = None) -> PageAssertions: ...

    @overload
    def __call__(self, actual: AsyncLocator | _native.Locator, message: str | None = None) -> LocatorAssertions: ...

    def __call__(
        self,
        actual: AsyncPage | AsyncLocator | _native.Locator,
        message: str | None = None,
    ) -> PageAssertions | LocatorAssertions:
        if _native is not None and isinstance(actual, _native.Page):
            return PageAssertions(actual, self._default_timeout, soft=self._soft, message=message)  # type: ignore[arg-type]
        if _native is not None and (
            isinstance(actual, _native.Locator) or isinstance(getattr(actual, "_impl", None), _native.Locator)
        ):
            return LocatorAssertions(actual, self._default_timeout, soft=self._soft, message=message)
        raise TypeError(f"expect: expected a Page or Locator, got {type(actual).__name__}")

    @property
    def soft(self) -> _AsyncExpect:
        return _AsyncExpect(self._default_timeout, soft=True)

    def configure(self, *, timeout: float | None = None, soft: bool | None = None) -> _AsyncExpect:
        next_timeout = _validate_timeout(timeout, self._default_timeout)
        if soft is not None and not isinstance(soft, bool):
            raise TypeError(f"expect.configure: soft must be a bool, got {type(soft).__name__}")
        next_soft = self._soft if soft is None else soft
        return _AsyncExpect(next_timeout, soft=next_soft)

    def set_options(self, timeout: float | None = None) -> None:
        """Updates the default assertion timeout (Playwright Python standard API)."""
        if timeout is not None:
            self._default_timeout = _validate_timeout(timeout, self._default_timeout)

    def soft_errors(self) -> list[AssertionError]:
        return get_soft_errors()

    def clear_soft_errors(self) -> list[AssertionError]:
        return clear_soft_errors()

    def assert_no_soft_errors(self) -> None:
        assert_no_soft_errors()


expect = _AsyncExpect()


class SyncLocatorAssertions:
    """Auto-retrying assertions for a sync ``Locator``."""

    def __init__(
        self,
        impl: LocatorAssertions,
        run_fn: Callable[..., Any],
        *,
        soft: bool = False,
        message: str | None = None,
    ) -> None:
        self._impl = impl
        self._run = run_fn
        self._soft = soft
        self._message = message

    def _exec(self, fn: Callable[..., Any], *args: Any, **kwargs: Any) -> None:
        try:
            self._run(fn, *args, **kwargs)
        except AssertionError as exc:
            if self._soft:
                _record_soft_error(exc, self._message)
                return
            if self._message:
                raise AssertionError(f"{self._message}\n{exc}") from None
            raise

    def to_be_visible(self, timeout: float | None = None) -> None:
        self._exec(self._impl.to_be_visible, timeout=timeout)

    def not_to_be_visible(self, timeout: float | None = None) -> None:
        self._exec(self._impl.not_to_be_visible, timeout=timeout)

    def to_be_hidden(self, timeout: float | None = None) -> None:
        self._exec(self._impl.to_be_hidden, timeout=timeout)

    def not_to_be_hidden(self, timeout: float | None = None) -> None:
        self._exec(self._impl.not_to_be_hidden, timeout=timeout)

    def to_be_attached(self, timeout: float | None = None) -> None:
        self._exec(self._impl.to_be_attached, timeout=timeout)

    def not_to_be_attached(self, timeout: float | None = None) -> None:
        self._exec(self._impl.not_to_be_attached, timeout=timeout)

    def to_be_enabled(self, timeout: float | None = None) -> None:
        self._exec(self._impl.to_be_enabled, timeout=timeout)

    def not_to_be_enabled(self, timeout: float | None = None) -> None:
        self._exec(self._impl.not_to_be_enabled, timeout=timeout)

    def to_be_disabled(self, timeout: float | None = None) -> None:
        self._exec(self._impl.to_be_disabled, timeout=timeout)

    def not_to_be_disabled(self, timeout: float | None = None) -> None:
        self._exec(self._impl.not_to_be_disabled, timeout=timeout)

    def to_be_checked(self, timeout: float | None = None, *, checked: bool | None = None) -> None:
        self._exec(self._impl.to_be_checked, timeout=timeout, checked=checked)

    def not_to_be_checked(self, timeout: float | None = None) -> None:
        self._exec(self._impl.not_to_be_checked, timeout=timeout)

    def to_be_editable(self, timeout: float | None = None) -> None:
        self._exec(self._impl.to_be_editable, timeout=timeout)

    def not_to_be_editable(self, timeout: float | None = None) -> None:
        self._exec(self._impl.not_to_be_editable, timeout=timeout)

    def to_have_text(self, expected: str | re.Pattern[str], timeout: float | None = None) -> None:
        self._exec(self._impl.to_have_text, expected, timeout=timeout)

    def not_to_have_text(self, expected: str | re.Pattern[str], timeout: float | None = None) -> None:
        self._exec(self._impl.not_to_have_text, expected, timeout=timeout)

    def to_contain_text(self, expected: str | re.Pattern[str], timeout: float | None = None) -> None:
        self._exec(self._impl.to_contain_text, expected, timeout=timeout)

    def not_to_contain_text(self, expected: str | re.Pattern[str], timeout: float | None = None) -> None:
        self._exec(self._impl.not_to_contain_text, expected, timeout=timeout)

    def to_have_value(self, expected: str | re.Pattern[str], timeout: float | None = None) -> None:
        self._exec(self._impl.to_have_value, expected, timeout=timeout)

    def not_to_have_value(self, expected: str | re.Pattern[str], timeout: float | None = None) -> None:
        self._exec(self._impl.not_to_have_value, expected, timeout=timeout)

    def to_have_attribute(
        self, name: str, expected: str | re.Pattern[str], timeout: float | None = None
    ) -> None:
        self._exec(self._impl.to_have_attribute, name, expected, timeout=timeout)

    def not_to_have_attribute(
        self, name: str, expected: str | re.Pattern[str], timeout: float | None = None
    ) -> None:
        self._exec(self._impl.not_to_have_attribute, name, expected, timeout=timeout)

    def to_have_class(self, expected: str | re.Pattern[str], timeout: float | None = None) -> None:
        self._exec(self._impl.to_have_class, expected, timeout=timeout)

    def not_to_have_class(self, expected: str | re.Pattern[str], timeout: float | None = None) -> None:
        self._exec(self._impl.not_to_have_class, expected, timeout=timeout)

    def to_have_count(self, count: int, timeout: float | None = None) -> None:
        self._exec(self._impl.to_have_count, count, timeout=timeout)

    def not_to_have_count(self, count: int, timeout: float | None = None) -> None:
        self._exec(self._impl.not_to_have_count, count, timeout=timeout)

    def to_have_screenshot(
        self,
        name: str | PathLike[str] | Sequence[str],
        *,
        threshold: float | None = None,
        max_diff_pixels: int | None = None,
        max_diff_pixel_ratio: float | None = None,
        animations: Literal["disabled", "allow"] | None = None,
        timeout: float | None = None,
        omit_background: bool | None = None,
        update: bool | None = None,
    ) -> None:
        self._exec(
            self._impl.to_have_screenshot,
            name,
            threshold=threshold,
            max_diff_pixels=max_diff_pixels,
            max_diff_pixel_ratio=max_diff_pixel_ratio,
            animations=animations,
            timeout=timeout,
            omit_background=omit_background,
            update=update,
        )


class SyncPageAssertions:
    """Auto-retrying assertions for a sync ``Page``."""

    def __init__(
        self,
        impl: PageAssertions,
        run_fn: Callable[..., Any],
        *,
        soft: bool = False,
        message: str | None = None,
    ) -> None:
        self._impl = impl
        self._run = run_fn
        self._soft = soft
        self._message = message

    def _exec(self, fn: Callable[..., Any], *args: Any, **kwargs: Any) -> None:
        try:
            self._run(fn, *args, **kwargs)
        except AssertionError as exc:
            if self._soft:
                _record_soft_error(exc, self._message)
                return
            if self._message:
                raise AssertionError(f"{self._message}\n{exc}") from None
            raise

    def to_have_title(self, expected: str | re.Pattern[str], timeout: float | None = None) -> None:
        self._exec(self._impl.to_have_title, expected, timeout=timeout)

    def not_to_have_title(self, expected: str | re.Pattern[str], timeout: float | None = None) -> None:
        self._exec(self._impl.not_to_have_title, expected, timeout=timeout)

    def to_have_url(self, expected: str | re.Pattern[str], timeout: float | None = None) -> None:
        self._exec(self._impl.to_have_url, expected, timeout=timeout)

    def not_to_have_url(self, expected: str | re.Pattern[str], timeout: float | None = None) -> None:
        self._exec(self._impl.not_to_have_url, expected, timeout=timeout)

    def to_have_screenshot(
        self,
        name: str | PathLike[str] | Sequence[str],
        *,
        threshold: float | None = None,
        max_diff_pixels: int | None = None,
        max_diff_pixel_ratio: float | None = None,
        animations: Literal["disabled", "allow"] | None = None,
        timeout: float | None = None,
        full_page: bool | None = None,
        omit_background: bool | None = None,
        clip: FloatRect | None = None,
        update: bool | None = None,
    ) -> None:
        self._exec(
            self._impl.to_have_screenshot,
            name,
            threshold=threshold,
            max_diff_pixels=max_diff_pixels,
            max_diff_pixel_ratio=max_diff_pixel_ratio,
            animations=animations,
            timeout=timeout,
            full_page=full_page,
            omit_background=omit_background,
            clip=clip,
            update=update,
        )


class SyncExpect:
    def __init__(
        self,
        page_cls: type[SyncPage],
        locator_cls: type[SyncLocator],
        page_assertions_cls: type[SyncPageAssertions],
        locator_assertions_cls: type[SyncLocatorAssertions],
        run_fn: Callable[..., Any],
        default_timeout: float = DEFAULT_TIMEOUT_MS,
        *,
        soft: bool = False,
    ) -> None:
        self._page_cls = page_cls
        self._locator_cls = locator_cls
        self._page_assertions_cls = page_assertions_cls
        self._locator_assertions_cls = locator_assertions_cls
        self._run = run_fn
        self._default_timeout = default_timeout
        self._soft = soft

    @overload
    def __call__(self, actual: SyncPage, message: str | None = None) -> SyncPageAssertions: ...

    @overload
    def __call__(self, actual: SyncLocator, message: str | None = None) -> SyncLocatorAssertions: ...

    def __call__(
        self,
        actual: SyncPage | SyncLocator,
        message: str | None = None,
    ) -> SyncPageAssertions | SyncLocatorAssertions:
        if isinstance(actual, self._page_cls):
            return self._page_assertions_cls(
                PageAssertions(actual._impl, self._default_timeout),
                self._run,
                soft=self._soft,
                message=message,
            )
        if isinstance(actual, self._locator_cls):
            return self._locator_assertions_cls(
                LocatorAssertions(actual._impl, self._default_timeout),
                self._run,
                soft=self._soft,
                message=message,
            )
        raise TypeError(f"expect: expected a Page or Locator, got {type(actual).__name__}")

    @property
    def soft(self) -> SyncExpect:
        return SyncExpect(
            self._page_cls,
            self._locator_cls,
            self._page_assertions_cls,
            self._locator_assertions_cls,
            self._run,
            self._default_timeout,
            soft=True,
        )

    def configure(self, *, timeout: float | None = None, soft: bool | None = None) -> SyncExpect:
        next_timeout = _validate_timeout(timeout, self._default_timeout)
        if soft is not None and not isinstance(soft, bool):
            raise TypeError(f"expect.configure: soft must be a bool, got {type(soft).__name__}")
        next_soft = self._soft if soft is None else soft
        return SyncExpect(
            self._page_cls,
            self._locator_cls,
            self._page_assertions_cls,
            self._locator_assertions_cls,
            self._run,
            next_timeout,
            soft=next_soft,
        )

    def set_options(self, timeout: float | None = None) -> None:
        """Updates the default assertion timeout (Playwright Python standard API)."""
        if timeout is not None:
            self._default_timeout = _validate_timeout(timeout, self._default_timeout)

    def soft_errors(self) -> list[AssertionError]:
        return get_soft_errors()

    def clear_soft_errors(self) -> list[AssertionError]:
        return clear_soft_errors()

    def assert_no_soft_errors(self) -> None:
        assert_no_soft_errors()
