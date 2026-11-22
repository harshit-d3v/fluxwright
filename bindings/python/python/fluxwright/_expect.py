"""Playwright-style auto-retrying ``expect()`` assertions for async and sync APIs."""

from __future__ import annotations

import asyncio
import math
import re
import time
from collections.abc import Awaitable, Callable
from typing import TYPE_CHECKING, Any, Literal, overload
from urllib.parse import urljoin, urlparse

from . import _native
from ._errors import Error

if TYPE_CHECKING:
    from .async_api import Page as AsyncPage
    from .sync_api import Locator as SyncLocator
    from .sync_api import Page as SyncPage

DEFAULT_TIMEOUT_MS = 5000.0
POLL_INTERVAL_S = 0.05
MIN_PROBE_TIMEOUT_MS = 50.0
MAX_PROBE_TIMEOUT_MS = 250.0

_ProbeKind = Literal["value", "not-attached", "transient"]
_ProbeResult = tuple[_ProbeKind, bool, str]


def _normalize_ws(text: str) -> str:
    return " ".join(text.split())


def _validate_pattern(expected: str | re.Pattern[str], method: str) -> None:
    if not isinstance(expected, (str, re.Pattern)):
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
            r"page (is )?closed|browser process exited|session .* is gone|target crashed|invalid selector:",
            str(exc),
            re.IGNORECASE,
        )
    )


def _resolve_url(page: Any, expected: str | re.Pattern[str]) -> str | re.Pattern[str]:
    if not isinstance(expected, str):
        return expected
    base_url = page._base_url()
    if base_url is not None and not re.match(r"^[a-zA-Z][a-zA-Z0-9+.-]*:", expected):
        if expected.startswith("/"):
            parsed = urlparse(base_url)
            return f"{parsed.scheme}://{parsed.netloc}{expected}"
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
        elif kind == "not-attached":
            actual_desc = "element not found (count: 0)"

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

    def __init__(self, locator: _native.Locator, default_timeout: float = DEFAULT_TIMEOUT_MS) -> None:
        self._locator = locator
        self._default_timeout = default_timeout

    def _target(self) -> str:
        return f"expect({self._locator!r})"

    async def _ensure_attached(self) -> _ProbeResult | None:
        try:
            n = await self._locator.count()
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
                early = await self._ensure_attached()
                if early is not None:
                    return early
            val = await call(limit_ms)
            return ("value", val, true_label if val else false_label)

        await _poll_until(
            target_label=self._target(),
            matcher=matcher,
            expected_desc=expected,
            negated=negated,
            timeout_ms=timeout_ms,
            probe=probe,
        )

    async def _attached_check(self, *, matcher: str, negated: bool, timeout: float | None) -> None:
        timeout_ms = _validate_timeout(timeout, self._default_timeout)

        async def probe(limit_ms: float) -> _ProbeResult:
            n = await self._locator.count()
            if n == 0:
                return ("value", False, "detached (count: 0)")
            if n == 1:
                return ("value", True, "attached")
            await self._locator.is_visible(timeout=limit_ms)
            return ("value", True, f"attached (count: {n})")

        await _poll_until(
            target_label=self._target(),
            matcher=matcher,
            expected_desc="attached",
            negated=negated,
            timeout_ms=timeout_ms,
            probe=probe,
        )

    async def _checked_check(
        self,
        *,
        matcher: str,
        negated: bool,
        checked: bool | None,
        timeout: float | None,
    ) -> None:
        want = True if checked is None else bool(checked)
        timeout_ms = _validate_timeout(timeout, self._default_timeout)

        async def probe(limit_ms: float) -> _ProbeResult:
            early = await self._ensure_attached()
            if early is not None:
                return early
            val = await self._locator.is_checked(timeout=limit_ms)
            return ("value", val is want, "checked" if val else "unchecked")

        await _poll_until(
            target_label=self._target(),
            matcher=matcher,
            expected_desc="checked" if want else "unchecked",
            negated=negated,
            timeout_ms=timeout_ms,
            probe=probe,
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
            early = await self._ensure_attached()
            if early is not None:
                return early
            raw = await self._locator.text_content(timeout=limit_ms) or ""
            actual_norm = _normalize_ws(raw)
            return (
                "value",
                _match_pattern(raw, expected, normalize=True, substring=substring),
                repr(actual_norm),
            )

        await _poll_until(
            target_label=self._target(),
            matcher=matcher,
            expected_desc=expected_desc,
            negated=negated,
            timeout_ms=timeout_ms,
            probe=probe,
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
            early = await self._ensure_attached()
            if early is not None:
                return early
            val = await self._locator.input_value(timeout=limit_ms)
            return ("value", _match_pattern(val, expected), repr(val))

        await _poll_until(
            target_label=self._target(),
            matcher=matcher,
            expected_desc=_format_expected(expected),
            negated=negated,
            timeout_ms=timeout_ms,
            probe=probe,
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
            early = await self._ensure_attached()
            if early is not None:
                return early
            val = await self._locator.get_attribute(name, timeout=limit_ms)
            if val is None:
                return ("value", False, f"{name}=<absent>")
            return ("value", _match_pattern(val, expected), f"{name}={val!r}")

        await _poll_until(
            target_label=self._target(),
            matcher=matcher,
            expected_desc=f"{name}={_format_expected(expected)}",
            negated=negated,
            timeout_ms=timeout_ms,
            probe=probe,
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
            early = await self._ensure_attached()
            if early is not None:
                return early
            cls = await self._locator.get_attribute("class", timeout=limit_ms) or ""
            return ("value", _match_pattern(cls, expected), repr(cls))

        await _poll_until(
            target_label=self._target(),
            matcher=matcher,
            expected_desc=_format_expected(expected),
            negated=negated,
            timeout_ms=timeout_ms,
            probe=probe,
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

        async def probe(_limit_ms: float) -> _ProbeResult:
            n = await self._locator.count()
            return ("value", n == count, str(n))

        await _poll_until(
            target_label=self._target(),
            matcher=matcher,
            expected_desc=str(count),
            negated=negated,
            timeout_ms=timeout_ms,
            probe=probe,
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
            call=lambda lim: self._locator.is_visible(timeout=lim),
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
            call=lambda lim: self._locator.is_visible(timeout=lim),
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
            call=lambda lim: self._locator.is_hidden(timeout=lim),
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
            call=lambda lim: self._locator.is_hidden(timeout=lim),
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
            call=lambda lim: self._locator.is_enabled(timeout=lim),
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
            call=lambda lim: self._locator.is_enabled(timeout=lim),
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
            call=lambda lim: self._locator.is_disabled(timeout=lim),
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
            call=lambda lim: self._locator.is_disabled(timeout=lim),
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
            call=lambda lim: self._locator.is_editable(timeout=lim),
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
            call=lambda lim: self._locator.is_editable(timeout=lim),
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


class PageAssertions:
    """Auto-retrying assertions for an async ``Page``."""

    def __init__(self, page: AsyncPage, default_timeout: float = DEFAULT_TIMEOUT_MS) -> None:
        self._page = page
        self._default_timeout = default_timeout

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

        async def probe(_limit_ms: float) -> _ProbeResult:
            if self._page.is_closed():
                raise Error("page is closed")
            raw = await self._page.title()
            actual_norm = _normalize_ws(raw)
            return ("value", _match_pattern(raw, expected, normalize=True), repr(actual_norm))

        await _poll_until(
            target_label="expect(page)",
            matcher=matcher,
            expected_desc=_format_expected(expected, normalize=True),
            negated=negated,
            timeout_ms=timeout_ms,
            probe=probe,
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

        await _poll_until(
            target_label="expect(page)",
            matcher=matcher,
            expected_desc=_format_expected(target),
            negated=negated,
            timeout_ms=timeout_ms,
            probe=probe,
        )

    async def to_have_title(self, expected: str | re.Pattern[str], timeout: float | None = None) -> None:
        await self._title_check(expected, matcher="to_have_title", negated=False, timeout=timeout)

    async def not_to_have_title(self, expected: str | re.Pattern[str], timeout: float | None = None) -> None:
        await self._title_check(expected, matcher="not_to_have_title", negated=True, timeout=timeout)

    async def to_have_url(self, expected: str | re.Pattern[str], timeout: float | None = None) -> None:
        await self._url_check(expected, matcher="to_have_url", negated=False, timeout=timeout)

    async def not_to_have_url(self, expected: str | re.Pattern[str], timeout: float | None = None) -> None:
        await self._url_check(expected, matcher="not_to_have_url", negated=True, timeout=timeout)


class _AsyncExpect:
    def __init__(self, default_timeout: float = DEFAULT_TIMEOUT_MS) -> None:
        self._default_timeout = default_timeout

    @overload
    def __call__(self, actual: AsyncPage) -> PageAssertions: ...

    @overload
    def __call__(self, actual: _native.Locator) -> LocatorAssertions: ...

    def __call__(self, actual: AsyncPage | _native.Locator) -> PageAssertions | LocatorAssertions:
        if isinstance(actual, _native.Page):
            return PageAssertions(actual, self._default_timeout)  # type: ignore[arg-type]
        if isinstance(actual, _native.Locator):
            return LocatorAssertions(actual, self._default_timeout)
        raise TypeError(f"expect: expected a Page or Locator, got {type(actual).__name__}")

    def configure(self, *, timeout: float | None = None) -> _AsyncExpect:
        next_timeout = _validate_timeout(timeout, self._default_timeout)
        return _AsyncExpect(next_timeout)


expect = _AsyncExpect()


class SyncLocatorAssertions:
    """Auto-retrying assertions for a sync ``Locator``."""

    def __init__(
        self,
        impl: LocatorAssertions,
        run_fn: Callable[..., Any],
    ) -> None:
        self._impl = impl
        self._run = run_fn

    def to_be_visible(self, timeout: float | None = None) -> None:
        self._run(self._impl.to_be_visible, timeout=timeout)

    def not_to_be_visible(self, timeout: float | None = None) -> None:
        self._run(self._impl.not_to_be_visible, timeout=timeout)

    def to_be_hidden(self, timeout: float | None = None) -> None:
        self._run(self._impl.to_be_hidden, timeout=timeout)

    def not_to_be_hidden(self, timeout: float | None = None) -> None:
        self._run(self._impl.not_to_be_hidden, timeout=timeout)

    def to_be_attached(self, timeout: float | None = None) -> None:
        self._run(self._impl.to_be_attached, timeout=timeout)

    def not_to_be_attached(self, timeout: float | None = None) -> None:
        self._run(self._impl.not_to_be_attached, timeout=timeout)

    def to_be_enabled(self, timeout: float | None = None) -> None:
        self._run(self._impl.to_be_enabled, timeout=timeout)

    def not_to_be_enabled(self, timeout: float | None = None) -> None:
        self._run(self._impl.not_to_be_enabled, timeout=timeout)

    def to_be_disabled(self, timeout: float | None = None) -> None:
        self._run(self._impl.to_be_disabled, timeout=timeout)

    def not_to_be_disabled(self, timeout: float | None = None) -> None:
        self._run(self._impl.not_to_be_disabled, timeout=timeout)

    def to_be_checked(self, timeout: float | None = None, *, checked: bool | None = None) -> None:
        self._run(self._impl.to_be_checked, timeout=timeout, checked=checked)

    def not_to_be_checked(self, timeout: float | None = None) -> None:
        self._run(self._impl.not_to_be_checked, timeout=timeout)

    def to_be_editable(self, timeout: float | None = None) -> None:
        self._run(self._impl.to_be_editable, timeout=timeout)

    def not_to_be_editable(self, timeout: float | None = None) -> None:
        self._run(self._impl.not_to_be_editable, timeout=timeout)

    def to_have_text(self, expected: str | re.Pattern[str], timeout: float | None = None) -> None:
        self._run(self._impl.to_have_text, expected, timeout=timeout)

    def not_to_have_text(self, expected: str | re.Pattern[str], timeout: float | None = None) -> None:
        self._run(self._impl.not_to_have_text, expected, timeout=timeout)

    def to_contain_text(self, expected: str | re.Pattern[str], timeout: float | None = None) -> None:
        self._run(self._impl.to_contain_text, expected, timeout=timeout)

    def not_to_contain_text(self, expected: str | re.Pattern[str], timeout: float | None = None) -> None:
        self._run(self._impl.not_to_contain_text, expected, timeout=timeout)

    def to_have_value(self, expected: str | re.Pattern[str], timeout: float | None = None) -> None:
        self._run(self._impl.to_have_value, expected, timeout=timeout)

    def not_to_have_value(self, expected: str | re.Pattern[str], timeout: float | None = None) -> None:
        self._run(self._impl.not_to_have_value, expected, timeout=timeout)

    def to_have_attribute(
        self, name: str, expected: str | re.Pattern[str], timeout: float | None = None
    ) -> None:
        self._run(self._impl.to_have_attribute, name, expected, timeout=timeout)

    def not_to_have_attribute(
        self, name: str, expected: str | re.Pattern[str], timeout: float | None = None
    ) -> None:
        self._run(self._impl.not_to_have_attribute, name, expected, timeout=timeout)

    def to_have_class(self, expected: str | re.Pattern[str], timeout: float | None = None) -> None:
        self._run(self._impl.to_have_class, expected, timeout=timeout)

    def not_to_have_class(self, expected: str | re.Pattern[str], timeout: float | None = None) -> None:
        self._run(self._impl.not_to_have_class, expected, timeout=timeout)

    def to_have_count(self, count: int, timeout: float | None = None) -> None:
        self._run(self._impl.to_have_count, count, timeout=timeout)

    def not_to_have_count(self, count: int, timeout: float | None = None) -> None:
        self._run(self._impl.not_to_have_count, count, timeout=timeout)


class SyncPageAssertions:
    """Auto-retrying assertions for a sync ``Page``."""

    def __init__(
        self,
        impl: PageAssertions,
        run_fn: Callable[..., Any],
    ) -> None:
        self._impl = impl
        self._run = run_fn

    def to_have_title(self, expected: str | re.Pattern[str], timeout: float | None = None) -> None:
        self._run(self._impl.to_have_title, expected, timeout=timeout)

    def not_to_have_title(self, expected: str | re.Pattern[str], timeout: float | None = None) -> None:
        self._run(self._impl.not_to_have_title, expected, timeout=timeout)

    def to_have_url(self, expected: str | re.Pattern[str], timeout: float | None = None) -> None:
        self._run(self._impl.to_have_url, expected, timeout=timeout)

    def not_to_have_url(self, expected: str | re.Pattern[str], timeout: float | None = None) -> None:
        self._run(self._impl.not_to_have_url, expected, timeout=timeout)


class SyncExpect:
    def __init__(
        self,
        page_cls: type[SyncPage],
        locator_cls: type[SyncLocator],
        page_assertions_cls: type[SyncPageAssertions],
        locator_assertions_cls: type[SyncLocatorAssertions],
        run_fn: Callable[..., Any],
        default_timeout: float = DEFAULT_TIMEOUT_MS,
    ) -> None:
        self._page_cls = page_cls
        self._locator_cls = locator_cls
        self._page_assertions_cls = page_assertions_cls
        self._locator_assertions_cls = locator_assertions_cls
        self._run = run_fn
        self._default_timeout = default_timeout

    @overload
    def __call__(self, actual: SyncPage) -> SyncPageAssertions: ...

    @overload
    def __call__(self, actual: SyncLocator) -> SyncLocatorAssertions: ...

    def __call__(self, actual: SyncPage | SyncLocator) -> SyncPageAssertions | SyncLocatorAssertions:
        if isinstance(actual, self._page_cls):
            return self._page_assertions_cls(PageAssertions(actual._impl, self._default_timeout), self._run)
        if isinstance(actual, self._locator_cls):
            return self._locator_assertions_cls(LocatorAssertions(actual._impl, self._default_timeout), self._run)
        raise TypeError(f"expect: expected a Page or Locator, got {type(actual).__name__}")

    def configure(self, *, timeout: float | None = None) -> SyncExpect:
        next_timeout = _validate_timeout(timeout, self._default_timeout)
        return SyncExpect(
            self._page_cls,
            self._locator_cls,
            self._page_assertions_cls,
            self._locator_assertions_cls,
            self._run,
            next_timeout,
        )
