"""Built-in lazy ``pytest`` plugin for Fluxwright (registered under ``pytest11``).

The built-in fixtures (``playwright``, ``browser_type``, ``browser``, ``context``, ``page``)
target ``fluxwright.sync_api`` with zero extra pytest dependencies. For ``fluxwright.async_api``
tests, use ``async_playwright()`` directly (e.g. with ``asyncio.run(...)`` or ``pytest-asyncio``).
"""

from __future__ import annotations

import hashlib
import inspect
import os
import re
import shutil
import tempfile
from collections.abc import Iterator
from contextlib import suppress
from pathlib import Path
from typing import Any

import pytest

from ._expect import clear_soft_errors, format_soft_errors
from .sync_api import Browser, BrowserContext, BrowserType, Page, Playwright, sync_playwright

_CONTEXT_STATE_ATTR = "_fluxwright_context_state"
_WINDOWS_RESERVED_NAMES = frozenset(
    {"CON", "PRN", "AUX", "NUL"}
    | {f"COM{i}" for i in range(1, 10)}
    | {f"LPT{i}" for i in range(1, 10)}
)


def _safe_addoption(target: Any, *names: str, **kwargs: Any) -> None:
    try:
        target.addoption(*names, **kwargs)
    except ValueError:
        # Another plugin (e.g. pytest-base-url or pytest-playwright) already registered this option.
        pass


def _safe_addini(parser: pytest.Parser, name: str, help_text: str, *, default: str = "") -> None:
    try:
        parser.addini(name, help=help_text, default=default)
    except ValueError:
        pass


def pytest_addoption(parser: pytest.Parser) -> None:
    group = parser.getgroup("fluxwright", "Fluxwright browser automation")
    _safe_addoption(
        group,
        "--headed",
        action="store_true",
        default=False,
        help="Run Fluxwright browsers in headed mode.",
    )
    _safe_addoption(
        group,
        "--base-url",
        action="store",
        default=None,
        help="Base URL for relative page.goto() and URL assertions.",
    )
    _safe_addini(parser, "base_url", "Base URL for relative page.goto() calls.", default="")
    _safe_addoption(
        group,
        "--fluxwright-max-browsers",
        action="store",
        type=int,
        default=None,
        help="Maximum pooled Chromium processes per pytest session.",
    )
    _safe_addoption(
        group,
        "--fluxwright-output",
        action="store",
        default="test-results",
        help="Directory for test failure artifacts (screenshots, traces, videos).",
    )
    _safe_addoption(
        group,
        "--fluxwright-screenshot",
        action="store",
        choices=("off", "on", "only-on-failure"),
        default="only-on-failure",
        help="Whether to capture screenshots after each test.",
    )
    _safe_addoption(
        group,
        "--fluxwright-tracing",
        action="store",
        choices=("off", "on", "retain-on-failure"),
        default="off",
        help="Whether to record Playwright-compatible trace .zip archives when context.tracing exists.",
    )
    _safe_addoption(
        group,
        "--fluxwright-video",
        action="store",
        choices=("off", "on", "retain-on-failure"),
        default="off",
        help="Whether to record page videos when record_video_dir / page.video exists.",
    )
    _safe_addoption(
        group,
        "--fluxwright-update-snapshots",
        action="store_true",
        default=False,
        help="Update visual regression golden baselines (sets FLUXWRIGHT_UPDATE_SNAPSHOTS=1).",
    )


def pytest_configure(config: pytest.Config) -> None:
    if config.getoption("--fluxwright-update-snapshots", default=False):
        config._fluxwright_prev_update_snapshots = os.environ.get("FLUXWRIGHT_UPDATE_SNAPSHOTS")  # type: ignore[attr-defined]
        os.environ["FLUXWRIGHT_UPDATE_SNAPSHOTS"] = "1"


def pytest_unconfigure(config: pytest.Config) -> None:
    if hasattr(config, "_fluxwright_prev_update_snapshots"):
        prev = config._fluxwright_prev_update_snapshots
        if prev is None:
            os.environ.pop("FLUXWRIGHT_UPDATE_SNAPSHOTS", None)
        else:
            os.environ["FLUXWRIGHT_UPDATE_SNAPSHOTS"] = prev


@pytest.hookimpl(tryfirst=True)
def pytest_runtest_setup(item: pytest.Item) -> None:
    for when in ("setup", "call", "teardown"):
        attr = f"_fluxwright_rep_{when}"
        if hasattr(item, attr):
            delattr(item, attr)
    clear_soft_errors()


@pytest.hookimpl(hookwrapper=True)
def pytest_runtest_call(item: pytest.Item) -> Iterator[None]:
    clear_soft_errors()
    outcome = yield
    errors = clear_soft_errors()
    if outcome.excinfo is None and errors:
        try:
            raise format_soft_errors(errors)
        except AssertionError as exc:
            outcome.force_exception((type(exc), exc, exc.__traceback__))


@pytest.hookimpl(tryfirst=True, hookwrapper=True)
def pytest_runtest_makereport(item: pytest.Item, call: pytest.CallInfo[Any]) -> Iterator[None]:
    outcome = yield
    rep = outcome.get_result()
    setattr(item, f"_fluxwright_rep_{rep.when}", rep)


def _sanitize_nodeid(nodeid: str) -> str:
    cleaned = re.sub(r"[^A-Za-z0-9._-]+", "-", nodeid)
    cleaned = cleaned.strip("- .")
    modified = cleaned != nodeid
    if not cleaned or cleaned in (".", "..") or set(cleaned) <= {"."}:
        cleaned = "test"
        modified = True
    stem = cleaned.split(".", 1)[0].upper()
    if stem in _WINDOWS_RESERVED_NAMES:
        cleaned = f"_{cleaned}"
        modified = True
    digest = hashlib.sha1(nodeid.encode("utf-8")).hexdigest()[:8]
    if len(cleaned) > 120 or modified:
        prefix = cleaned[:111].rstrip("- .") or "test"
        return f"{prefix}-{digest}"
    return cleaned


def _test_failed(node: Any) -> bool:
    for when in ("setup", "call"):
        rep = getattr(node, f"_fluxwright_rep_{when}", None)
        if rep is not None and getattr(rep, "failed", False):
            return True
    return False


def _get_video_handle(page_obj: Page) -> Any:
    video_attr = getattr(page_obj, "video", None)
    if video_attr is None:
        return None
    if callable(video_attr):
        with suppress(Exception):
            return video_attr()
        return None
    return video_attr


def _video_identity(video_obj: Any) -> int:
    return id(getattr(video_obj, "_impl", video_obj))


def _capture_screenshot(page_obj: Page, target_path: Path) -> bool:
    try:
        if page_obj.is_closed():
            return False
    except Exception:
        return False
    try:
        target_path.parent.mkdir(parents=True, exist_ok=True)
        page_obj.screenshot(path=target_path)
        return True
    except Exception:
        return False


def _save_or_discard_video(
    video_obj: Any,
    *,
    keep: bool,
    artifact_dir: Path,
    index: int,
    delete_source: bool = True,
) -> Path | None:
    if video_obj is None:
        return None
    src_path: Path | None = None
    path_fn = getattr(video_obj, "path", None)
    if callable(path_fn):
        with suppress(Exception):
            raw_path = path_fn()
            if raw_path:
                src_path = Path(str(raw_path))
    if keep:
        suffix = src_path.suffix if (src_path is not None and src_path.suffix) else ".webm"
        filename = f"video{suffix}" if index == 0 else f"video-{index + 1}{suffix}"
        dest = artifact_dir / filename
        artifact_dir.mkdir(parents=True, exist_ok=True)
        save_fn = getattr(video_obj, "save_as", None) or getattr(video_obj, "saveAs", None)
        if callable(save_fn):
            with suppress(Exception):
                save_fn(str(dest))
                if delete_source:
                    delete_fn = getattr(video_obj, "delete", None)
                    if callable(delete_fn):
                        with suppress(Exception):
                            delete_fn()
        return src_path
    if delete_source:
        delete_fn = getattr(video_obj, "delete", None)
        if callable(delete_fn):
            with suppress(Exception):
                delete_fn()
    return src_path


def _resolve_base_url(pytestconfig: pytest.Config) -> str | None:
    opt = pytestconfig.getoption("--base-url", default=None)
    if opt:
        return str(opt)
    with suppress(ValueError):
        ini = pytestconfig.getini("base_url")
        if ini:
            return str(ini)
    env_url = os.environ.get("FLUXWRIGHT_BASE_URL") or os.environ.get("PYTEST_BASE_URL")
    return env_url or None


@pytest.fixture(scope="session")
def base_url(pytestconfig: pytest.Config) -> str | None:
    """Base URL configured via ``--base-url``, ini ``base_url``, ``FLUXWRIGHT_BASE_URL``, or ``PYTEST_BASE_URL``."""
    return _resolve_base_url(pytestconfig)


@pytest.fixture(scope="session")
def browser_type_launch_args(pytestconfig: pytest.Config) -> dict[str, Any]:
    """Launch arguments for ``browser_type.launch(...)``."""
    args: dict[str, Any] = {
        "headless": not bool(pytestconfig.getoption("--headed", default=False)),
    }
    max_browsers = pytestconfig.getoption("--fluxwright-max-browsers", default=None)
    if max_browsers is not None:
        args["max_browsers"] = int(max_browsers)
    return args


@pytest.fixture(scope="session")
def browser_context_args() -> dict[str, Any]:
    """Extra keyword arguments passed to ``browser.new_context(...)``."""
    return {}


@pytest.fixture(scope="session")
def playwright() -> Iterator[Playwright]:
    """Session-scoped lazy ``sync_playwright`` instance."""
    pw = sync_playwright().start()
    try:
        yield pw
    finally:
        pw.stop()


@pytest.fixture(scope="session")
def browser_type(playwright: Playwright) -> BrowserType:
    """Session-scoped Chromium ``BrowserType``."""
    return playwright.chromium


@pytest.fixture(scope="session")
def browser(browser_type: BrowserType, browser_type_launch_args: dict[str, Any]) -> Iterator[Browser]:
    """Session-scoped pooled ``Browser`` shared across tests."""
    b = browser_type.launch(**browser_type_launch_args)
    try:
        yield b
    finally:
        b.close()


def _context_lifecycle(
    browser: Browser,
    browser_context_args: dict[str, Any],
    base_url: str | None,
    pytestconfig: pytest.Config,
    request: pytest.FixtureRequest,
) -> Iterator[BrowserContext]:
    output_root = Path(str(pytestconfig.getoption("--fluxwright-output", default="test-results")))
    screenshot_mode = str(pytestconfig.getoption("--fluxwright-screenshot", default="only-on-failure"))
    tracing_mode = str(pytestconfig.getoption("--fluxwright-tracing", default="off"))
    video_mode = str(pytestconfig.getoption("--fluxwright-video", default="off"))
    artifact_dir = output_root / _sanitize_nodeid(request.node.nodeid)

    ctx_kwargs: dict[str, Any] = dict(browser_context_args)
    if base_url and "base_url" not in ctx_kwargs:
        ctx_kwargs["base_url"] = base_url

    video_tmp_dir: Path | None = None
    ctx: BrowserContext | None = None
    tracing: Any = None
    tracing_started = False
    ctx_state: dict[str, Any] = {
        "captured_pages": set(),
        "videos": [],
        "video_ids": set(),
        "shot_index": 1,
        "artifact_dir": artifact_dir,
        "screenshot_mode": screenshot_mode,
    }

    def _add_video(video_obj: Any) -> None:
        if video_obj is None:
            return
        vid = _video_identity(video_obj)
        if vid not in ctx_state["video_ids"]:
            ctx_state["video_ids"].add(vid)
            ctx_state["videos"].append(video_obj)

    try:
        if video_mode in ("on", "retain-on-failure"):
            try:
                params = inspect.signature(browser.new_context).parameters
            except (TypeError, ValueError):
                params = {}
            if "record_video_dir" in params and "record_video_dir" not in ctx_kwargs:
                video_tmp_dir = Path(tempfile.mkdtemp(prefix="fw-video-"))
                ctx_kwargs["record_video_dir"] = str(video_tmp_dir)

        ctx = browser.new_context(**ctx_kwargs)

        tracing = getattr(ctx, "tracing", None)
        if (
            tracing is not None
            and callable(getattr(tracing, "start", None))
            and callable(getattr(tracing, "stop", None))
            and tracing_mode in ("on", "retain-on-failure")
        ):
            tracing.start(screenshots=True, snapshots=True)
            tracing_started = True

        setattr(request.node, _CONTEXT_STATE_ATTR, ctx_state)
        yield ctx
    finally:
        if ctx is not None:
            failed = _test_failed(request.node)
            should_shot = screenshot_mode == "on" or (screenshot_mode == "only-on-failure" and failed)

            for pg in list(ctx.pages):
                if id(pg) in ctx_state["captured_pages"]:
                    continue
                _add_video(_get_video_handle(pg))
                if should_shot:
                    idx = ctx_state["shot_index"]
                    shot_name = f"test-failed-{idx}.png" if failed else f"test-finished-{idx}.png"
                    if _capture_screenshot(pg, artifact_dir / shot_name):
                        ctx_state["shot_index"] += 1
                ctx_state["captured_pages"].add(id(pg))

            all_videos_fn = getattr(ctx, "_all_video_handles", None)
            if callable(all_videos_fn):
                with suppress(Exception):
                    for v in all_videos_fn():
                        _add_video(v)

            if tracing is not None and tracing_started:
                keep_trace = tracing_mode == "on" or (tracing_mode == "retain-on-failure" and failed)
                with suppress(Exception):
                    if keep_trace:
                        artifact_dir.mkdir(parents=True, exist_ok=True)
                        tracing.stop(path=str(artifact_dir / "trace.zip"))
                    else:
                        tracing.stop()

            with suppress(Exception):
                ctx.close()

            keep_video = video_mode == "on" or (video_mode == "retain-on-failure" and failed)
            managed_videos = video_tmp_dir is not None
            seen_sources: set[Path] = set()
            video_idx = 0
            if managed_videos or keep_video:
                for video_obj in ctx_state["videos"]:
                    src = _save_or_discard_video(
                        video_obj,
                        keep=keep_video,
                        artifact_dir=artifact_dir,
                        index=video_idx,
                        delete_source=managed_videos,
                    )
                    if src is not None:
                        with suppress(OSError):
                            seen_sources.add(src.resolve())
                    if keep_video:
                        video_idx += 1

            if video_tmp_dir is not None and keep_video and video_tmp_dir.exists():
                extras = [
                    p
                    for p in video_tmp_dir.iterdir()
                    if p.is_file() and p.suffix.lower() in (".webm", ".avi")
                ]
                extras.sort(key=lambda p: (p.stat().st_mtime, p.name))
                for extra in extras:
                    with suppress(OSError):
                        resolved = extra.resolve()
                        if resolved in seen_sources:
                            continue
                        artifact_dir.mkdir(parents=True, exist_ok=True)
                        filename = f"video{extra.suffix}" if video_idx == 0 else f"video-{video_idx + 1}{extra.suffix}"
                        shutil.copyfile(extra, artifact_dir / filename)
                        seen_sources.add(resolved)
                        video_idx += 1

        if hasattr(request.node, _CONTEXT_STATE_ATTR):
            delattr(request.node, _CONTEXT_STATE_ATTR)
        if video_tmp_dir is not None:
            shutil.rmtree(video_tmp_dir, ignore_errors=True)


@pytest.fixture
def context(
    browser: Browser,
    browser_context_args: dict[str, Any],
    base_url: str | None,
    pytestconfig: pytest.Config,
    request: pytest.FixtureRequest,
) -> Iterator[BrowserContext]:
    """Per-test isolated ``BrowserContext`` with failure screenshot, trace, and video hooks."""
    yield from _context_lifecycle(browser, browser_context_args, base_url, pytestconfig, request)


@pytest.fixture
def page(context: BrowserContext, request: pytest.FixtureRequest) -> Iterator[Page]:
    """Per-test ``Page`` bound to the isolated ``context``."""
    pg = context.new_page()
    try:
        yield pg
    finally:
        failed = _test_failed(request.node)
        ctx_state = getattr(request.node, _CONTEXT_STATE_ATTR, None)
        if isinstance(ctx_state, dict):
            video_obj = _get_video_handle(pg)
            if video_obj is not None:
                vid = _video_identity(video_obj)
                video_ids = ctx_state.setdefault("video_ids", set())
                if vid not in video_ids:
                    video_ids.add(vid)
                    ctx_state["videos"].append(video_obj)
            screenshot_mode = ctx_state.get("screenshot_mode", "only-on-failure")
            should_shot = screenshot_mode == "on" or (screenshot_mode == "only-on-failure" and failed)
            if should_shot:
                idx = ctx_state["shot_index"]
                shot_name = f"test-failed-{idx}.png" if failed else f"test-finished-{idx}.png"
                if _capture_screenshot(pg, ctx_state["artifact_dir"] / shot_name):
                    ctx_state["shot_index"] += 1
            ctx_state["captured_pages"].add(id(pg))
        else:
            with suppress(Exception):
                if not pg.is_closed():
                    pg.close()
