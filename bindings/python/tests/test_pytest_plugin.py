"""Tests for the built-in lazy ``pytest-fluxwright`` plugin."""

from __future__ import annotations

import os
import subprocess
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

import pytest

from fluxwright import pytest_plugin
from fluxwright.sync_api import Browser, BrowserContext, BrowserType, Page, expect


class _Handler(BaseHTTPRequestHandler):
    def do_GET(self) -> None:
        self.send_response(200)
        self.send_header("Content-Type", "text/html; charset=utf-8")
        if self.path == "/hello":
            body = b"<title>Plugin Hello</title><h1 id='hdr'>Hello Plugin</h1>"
        else:
            body = b"<title>Plugin Home</title><p id='msg'>Ready</p>"
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args: object) -> None:
        pass


@pytest.fixture(scope="module")
def plugin_server() -> str:
    server = ThreadingHTTPServer(("127.0.0.1", 0), _Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    yield f"http://127.0.0.1:{server.server_address[1]}"
    server.shutdown()


_SEEN_BROWSER_IDS: list[int] = []


def test_plugin_fixtures_first_and_isolation(
    browser_type: BrowserType,
    browser: Browser,
    context: BrowserContext,
    page: Page,
    plugin_server: str,
) -> None:
    assert browser_type.name == "chromium"
    assert isinstance(browser, Browser)
    assert isinstance(context, BrowserContext)
    assert isinstance(page, Page)
    _SEEN_BROWSER_IDS.append(id(browser))

    page.goto(f"{plugin_server}/hello")
    expect(page).to_have_title("Plugin Hello")
    expect(page.locator("#hdr")).to_have_text("Hello Plugin")
    page.evaluate("() => localStorage.setItem('plugin_iso', 'test1')")


def test_plugin_session_browser_reuse_and_context_isolation(
    browser: Browser,
    context: BrowserContext,
    plugin_server: str,
) -> None:
    _SEEN_BROWSER_IDS.append(id(browser))
    assert len(set(_SEEN_BROWSER_IDS)) == 1
    page = context.new_page()
    page.goto(plugin_server)
    expect(page).to_have_title("Plugin Home")
    assert page.evaluate("() => localStorage.getItem('plugin_iso')") is None


def test_plugin_option_collision_guard_and_lazy_unit() -> None:
    class _DummyGroup:
        def __init__(self) -> None:
            self.seen: set[str] = {"--base-url", "--headed"}

        def addoption(self, *names: str, **kwargs: object) -> None:
            if any(n in self.seen for n in names):
                raise ValueError("option already added")
            self.seen.update(names)

    class _DummyParser:
        def __init__(self) -> None:
            self.group = _DummyGroup()
            self.inis: set[str] = {"base_url"}

        def getgroup(self, name: str, description: str = "") -> _DummyGroup:
            return self.group

        def addini(self, name: str, help: str = "", default: str = "") -> None:
            if name in self.inis:
                raise ValueError("ini already added")
            self.inis.add(name)

    parser = _DummyParser()
    # Must not raise when --base-url, --headed, or ini base_url already exist.
    pytest_plugin.pytest_addoption(parser)  # type: ignore[arg-type]
    assert "--fluxwright-output" in parser.group.seen
    assert "--fluxwright-screenshot" in parser.group.seen


def test_plugin_cli_help_lists_options(tmp_path: Path) -> None:
    res = subprocess.run(
        [sys.executable, "-m", "pytest", "--help"],
        cwd=str(tmp_path),
        capture_output=True,
        text=True,
        check=False,
    )
    assert res.returncode == 0, res.stderr
    for flag in (
        "--headed",
        "--fluxwright-max-browsers",
        "--fluxwright-output",
        "--fluxwright-screenshot",
        "--fluxwright-tracing",
        "--fluxwright-video",
        "--fluxwright-update-snapshots",
    ):
        assert flag in res.stdout, f"missing {flag} in pytest --help"


def test_plugin_lazy_no_browser_when_unrequested(tmp_path: Path) -> None:
    test_file = tmp_path / "test_lazy.py"
    test_file.write_text(
        """
import fluxwright.pytest_plugin as fw_plugin
import fluxwright.sync_api as sync_api

def test_no_fixtures(monkeypatch):
    def fail_start():
        raise AssertionError("sync_playwright should not start when fixtures are unused")
    monkeypatch.setattr(sync_api, "sync_playwright", fail_start)
    monkeypatch.setattr(fw_plugin, "sync_playwright", fail_start)
    assert 2 + 2 == 4
""",
        encoding="utf-8",
    )
    res = subprocess.run(
        [sys.executable, "-m", "pytest", "-q", "-o", "addopts=", str(test_file)],
        cwd=str(tmp_path),
        capture_output=True,
        text=True,
        check=False,
    )
    assert res.returncode == 0, res.stdout + res.stderr


def test_plugin_failure_screenshot_and_cli_flags(tmp_path: Path, plugin_server: str) -> None:
    out_dir = tmp_path / "artifacts"
    fail_file = tmp_path / "test_fail_shot.py"
    fail_file.write_text(
        """
from fluxwright.sync_api import expect

def test_intentional_failure(page):
    page.goto("/hello")
    expect(page.locator("#hdr")).to_have_text("Hello Plugin")
    assert False, "deliberate failure for screenshot capture"
""",
        encoding="utf-8",
    )

    fail_res = subprocess.run(
        [
            sys.executable,
            "-m",
            "pytest",
            "-q",
            "-o",
            "addopts=",
            str(fail_file),
            f"--base-url={plugin_server}",
            "--fluxwright-max-browsers=1",
            f"--fluxwright-output={out_dir}",
            "--fluxwright-screenshot=only-on-failure",
            "--fluxwright-tracing=retain-on-failure",
            "--fluxwright-video=retain-on-failure",
        ],
        cwd=str(tmp_path),
        capture_output=True,
        text=True,
        check=False,
    )
    assert fail_res.returncode != 0
    pngs = list(out_dir.rglob("*.png"))
    assert len(pngs) == 1, f"expected 1 failure screenshot, found {pngs}\n{fail_res.stdout}\n{fail_res.stderr}"
    assert pngs[0].read_bytes()[:4] == b"\x89PNG"

    # With --fluxwright-screenshot=off, no screenshot should be written.
    off_dir = tmp_path / "artifacts-off"
    off_res = subprocess.run(
        [
            sys.executable,
            "-m",
            "pytest",
            "-q",
            "-o",
            "addopts=",
            str(fail_file),
            f"--base-url={plugin_server}",
            "--fluxwright-max-browsers=1",
            f"--fluxwright-output={off_dir}",
            "--fluxwright-screenshot=off",
        ],
        cwd=str(tmp_path),
        capture_output=True,
        text=True,
        check=False,
    )
    assert off_res.returncode != 0
    assert list(off_dir.rglob("*.png")) == []

    # Verify --fluxwright-update-snapshots, --base-url, and overridden base_url work cleanly.
    pass_file = tmp_path / "test_flags_pass.py"
    pass_file.write_text(
        """
import os
from fluxwright.sync_api import expect

def test_base_url_and_update_snapshots(page, base_url):
    assert base_url
    assert os.environ.get("FLUXWRIGHT_UPDATE_SNAPSHOTS") == "1"
    page.goto("/hello")
    expect(page).to_have_title("Plugin Hello")
""",
        encoding="utf-8",
    )
    env = {k: v for k, v in os.environ.items() if k != "FLUXWRIGHT_UPDATE_SNAPSHOTS"}
    pass_res = subprocess.run(
        [
            sys.executable,
            "-m",
            "pytest",
            "-q",
            "-o",
            "addopts=",
            str(pass_file),
            f"--base-url={plugin_server}",
            "--fluxwright-max-browsers=1",
            "--fluxwright-update-snapshots",
        ],
        cwd=str(tmp_path),
        env=env,
        capture_output=True,
        text=True,
        check=False,
    )
    assert pass_res.returncode == 0, pass_res.stdout + pass_res.stderr


def test_plugin_feature_detection_hooks_with_mocks(tmp_path: Path) -> None:
    """Verify getattr hooks for tracing & video execute cleanly when E2/E3 APIs exist."""
    saved_videos: list[Path] = []
    deleted_videos: list[bool] = []

    class _FakeVideo:
        def __init__(self, keep_src: Path) -> None:
            self._src = keep_src

        def path(self) -> Path:
            return self._src

        def save_as(self, dest: str | Path) -> None:
            p = Path(dest)
            p.write_bytes(b"RIFF0000AVI ")
            saved_videos.append(p)

        def delete(self) -> None:
            deleted_videos.append(True)

    artifact_dir = tmp_path / "out"
    v_keep = _FakeVideo(tmp_path / "raw1.avi")
    pytest_plugin._save_or_discard_video(v_keep, keep=True, artifact_dir=artifact_dir, index=0)
    assert (artifact_dir / "video.avi").read_bytes() == b"RIFF0000AVI "
    assert len(saved_videos) == 1 and len(deleted_videos) == 1

    v_drop = _FakeVideo(tmp_path / "raw2.avi")
    pytest_plugin._save_or_discard_video(v_drop, keep=False, artifact_dir=artifact_dir, index=1)
    assert len(saved_videos) == 1 and len(deleted_videos) == 2

    class _FakePageWithProp:
        video = v_keep

    class _FakePageWithMethod:
        def video(self) -> _FakeVideo:
            return v_drop

    assert pytest_plugin._get_video_handle(_FakePageWithProp()) is v_keep  # type: ignore[arg-type]
    assert pytest_plugin._get_video_handle(_FakePageWithMethod()) is v_drop  # type: ignore[arg-type]
