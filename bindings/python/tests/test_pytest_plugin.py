"""Tests for the built-in lazy ``pytest-fluxwright`` plugin."""

from __future__ import annotations

import json
import os
import subprocess
import sys
import threading
import zipfile
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
    traces = list(out_dir.rglob("*.zip"))
    assert len(traces) == 1, f"expected 1 failure trace, found {traces}"
    assert traces[0].read_bytes()[:4] == b"PK\x03\x04"
    # Verify page stayed open until tracing.stop() so trace.zip includes >= 1 screenshot frame!
    with zipfile.ZipFile(traces[0], "r") as zf:
        trace_lines = [
            json.loads(line)
            for line in zf.read("trace.trace").decode("utf-8").splitlines()
            if line.strip()
        ]
        frames = [e for e in trace_lines if e.get("type") == "screencast-frame"]
        assert len(frames) >= 1, f"expected >= 1 screencast-frame in pytest trace.zip: {trace_lines}"
        assert f"resources/{frames[0]['sha1']}" in set(zf.namelist())
    videos = list(out_dir.rglob("*.avi")) + list(out_dir.rglob("*.webm"))
    assert len(videos) == 1, f"expected 1 failure video, found {videos}"
    assert videos[0].read_bytes()[:4] in (b"RIFF", b"\x1a\x45\xdf\xa3")

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

    # When delete_source=False (user-configured record_video_dir), delete() is NEVER called.
    prev_deletes = len(deleted_videos)
    pytest_plugin._save_or_discard_video(v_drop, keep=False, artifact_dir=artifact_dir, index=0, delete_source=False)
    pytest_plugin._save_or_discard_video(v_keep, keep=True, artifact_dir=artifact_dir, index=2, delete_source=False)
    assert len(deleted_videos) == prev_deletes


def test_plugin_sanitize_nodeid_rep_reset_and_base_url_env(monkeypatch: pytest.MonkeyPatch) -> None:
    s1 = pytest_plugin._sanitize_nodeid("test_a.py::test_fn[a/b]")
    s2 = pytest_plugin._sanitize_nodeid("test_a.py::test_fn[a:b]")
    assert s1 != s2

    for bad_dot in (".", "..", "...", ""):
        res = pytest_plugin._sanitize_nodeid(bad_dot)
        assert res not in ("", ".", "..")
        assert not res.endswith((".", " "))

    trailing = pytest_plugin._sanitize_nodeid("test_dots...   ")
    assert not trailing.endswith((".", " "))

    for reserved in ("CON", "PRN", "AUX", "NUL", "COM1", "COM9", "LPT1", "LPT9", "nul.py", "CON.txt"):
        res = pytest_plugin._sanitize_nodeid(reserved)
        assert res.split(".", 1)[0].upper() not in pytest_plugin._WINDOWS_RESERVED_NAMES
        assert res.startswith("_")

    # pytest_runtest_setup clears stale _fluxwright_rep_* attributes (e.g. reruns).
    class _DummyItem:
        pass

    item = _DummyItem()
    item._fluxwright_rep_setup = object()  # type: ignore[attr-defined]
    item._fluxwright_rep_call = object()  # type: ignore[attr-defined]
    item._fluxwright_rep_teardown = object()  # type: ignore[attr-defined]
    pytest_plugin.pytest_runtest_setup(item)  # type: ignore[arg-type]
    assert not hasattr(item, "_fluxwright_rep_setup")
    assert not hasattr(item, "_fluxwright_rep_call")
    assert not hasattr(item, "_fluxwright_rep_teardown")

    # base_url falls back to PYTEST_BASE_URL when --base-url, ini, and FLUXWRIGHT_BASE_URL are unset.
    class _DummyConfig:
        def getoption(self, name: str, default: object = None) -> object:
            return default

        def getini(self, name: str) -> str:
            return ""

    monkeypatch.delenv("FLUXWRIGHT_BASE_URL", raising=False)
    monkeypatch.setenv("PYTEST_BASE_URL", "http://127.0.0.1:9999")
    assert pytest_plugin._resolve_base_url(_DummyConfig()) == "http://127.0.0.1:9999"  # type: ignore[arg-type]


def test_plugin_user_video_dir_preserved_secondary_pages_and_soft_expect(
    tmp_path: Path, plugin_server: str
) -> None:
    # 1. User-configured record_video_dir must NEVER be deleted on passing tests.
    user_vid_dir = tmp_path / "user-videos"
    user_proj = tmp_path / "user_vid_proj"
    user_proj.mkdir()
    (user_proj / "conftest.py").write_text(
        f"""
import pytest

@pytest.fixture(scope="session")
def browser_context_args():
    return {{"record_video_dir": {str(user_vid_dir)!r}}}
""",
        encoding="utf-8",
    )
    (user_proj / "test_user_vid.py").write_text(
        """
def test_pass_keeps_user_video(page):
    page.goto("/hello")
    assert page.title() == "Plugin Hello"
""",
        encoding="utf-8",
    )
    res_user = subprocess.run(
        [
            sys.executable,
            "-m",
            "pytest",
            "-q",
            "-o",
            "addopts=",
            str(user_proj / "test_user_vid.py"),
            f"--base-url={plugin_server}",
            "--fluxwright-max-browsers=1",
            "--fluxwright-video=retain-on-failure",
        ],
        cwd=str(user_proj),
        capture_output=True,
        text=True,
        check=False,
    )
    assert res_user.returncode == 0, res_user.stdout + res_user.stderr
    user_vids = list(user_vid_dir.glob("*.avi")) + list(user_vid_dir.glob("*.webm"))
    assert len(user_vids) == 1, f"user record_video_dir video should be preserved, found {user_vids}"

    # 2. Early-closed secondary page retains video on failure, and expect.soft fails the test cleanly.
    multi_out = tmp_path / "multi-artifacts"
    multi_file = tmp_path / "test_multi_and_soft.py"
    multi_file.write_text(
        """
from fluxwright.sync_api import expect

def test_early_closed_page_and_soft_fail(context, page):
    page.goto("/hello")
    p2 = context.new_page()
    p2.goto("/")
    p2.close()
    expect.soft(page.locator("#hdr")).to_have_text("Soft Wrong 1", timeout=100)
    expect.soft(page.locator("#hdr")).to_have_text("Soft Wrong 2", timeout=100)
""",
        encoding="utf-8",
    )
    res_multi = subprocess.run(
        [
            sys.executable,
            "-m",
            "pytest",
            "-q",
            "-o",
            "addopts=",
            str(multi_file),
            f"--base-url={plugin_server}",
            "--fluxwright-max-browsers=1",
            f"--fluxwright-output={multi_out}",
            "--fluxwright-screenshot=only-on-failure",
            "--fluxwright-video=retain-on-failure",
        ],
        cwd=str(tmp_path),
        capture_output=True,
        text=True,
        check=False,
    )
    assert res_multi.returncode != 0
    assert "Soft Wrong 1" in res_multi.stdout
    assert "Soft Wrong 2" in res_multi.stdout
    pngs = list(multi_out.rglob("*.png"))
    assert len(pngs) == 1
    videos = list(multi_out.rglob("*.avi")) + list(multi_out.rglob("*.webm"))
    assert len(videos) >= 2, f"expected >= 2 videos (main + early-closed p2), found {videos}"


def test_plugin_context_setup_failure_cleans_tempdir_and_context(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    created_dirs: list[Path] = []
    orig_mkdtemp = pytest_plugin.tempfile.mkdtemp

    def _track_mkdtemp(*args: object, **kwargs: object) -> str:
        d = orig_mkdtemp(*args, **kwargs)
        created_dirs.append(Path(d))
        return d

    monkeypatch.setattr(pytest_plugin.tempfile, "mkdtemp", _track_mkdtemp)

    class _BoomTracing:
        def start(self, **kwargs: object) -> None:
            raise RuntimeError("tracing start boom")

        def stop(self, **kwargs: object) -> None:
            return None

    class _FakeCtx:
        def __init__(self) -> None:
            self.pages: list = []
            self.tracing = _BoomTracing()
            self.closed = False

        def close(self) -> None:
            self.closed = True

    fake_ctx = _FakeCtx()

    class _FakeBrowser:
        def new_context(self, *, record_video_dir: str | None = None, **kwargs: object) -> _FakeCtx:
            return fake_ctx

    class _FakeConfig:
        def getoption(self, name: str, default: object = None) -> object:
            return {
                "--fluxwright-output": str(tmp_path / "out"),
                "--fluxwright-screenshot": "only-on-failure",
                "--fluxwright-tracing": "on",
                "--fluxwright-video": "on",
            }.get(name, default)

    class _FakeNode:
        nodeid = "test_setup_leak.py::test_boom"

    class _FakeRequest:
        node = _FakeNode()

    gen = pytest_plugin._context_lifecycle(
        _FakeBrowser(),  # type: ignore[arg-type]
        {},
        None,
        _FakeConfig(),  # type: ignore[arg-type]
        _FakeRequest(),  # type: ignore[arg-type]
    )
    with pytest.raises(RuntimeError, match="tracing start boom"):
        next(gen)
    assert fake_ctx.closed is True
    assert len(created_dirs) == 1
    assert not created_dirs[0].exists()
