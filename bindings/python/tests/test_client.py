"""Tests for the synchronous and asynchronous FluxwrightClient HTTP SDKs."""

from __future__ import annotations

import asyncio
import base64
import json
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any
from urllib.parse import unquote

import pytest

import fluxwright
from fluxwright import AsyncFluxwrightClient, Error, FluxwrightApiError, FluxwrightClient

EXPECTED_TOKEN = "py-test-secret-token-1234567890abcdef"


class _MockState:
    def __init__(self) -> None:
        self.lock = threading.Lock()
        self.calls: list[dict[str, Any]] = []
        self.files: dict[str, bytes] = {}
        self.crawl_polls: dict[str, int] = {}
        self.job_polls: dict[str, int] = {}


def _make_handler(state: _MockState) -> type[BaseHTTPRequestHandler]:
    class Handler(BaseHTTPRequestHandler):
        def _send_json(self, status: int, payload: Any) -> None:
            raw = json.dumps(payload).encode("utf-8")
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(raw)))
            self.end_headers()
            self.wfile.write(raw)

        def _handle(self, method: str) -> None:
            length = int(self.headers.get("Content-Length", "0"))
            raw_body = self.rfile.read(length) if length > 0 else b""
            body = json.loads(raw_body.decode("utf-8")) if raw_body else None
            auth = self.headers.get("Authorization")

            with state.lock:
                state.calls.append({"method": method, "path": self.path, "auth": auth, "body": body})

            if method == "GET" and self.path == "/healthz":
                self._send_json(200, {"status": "ok"})
                return
            if method == "GET" and self.path == "/openapi.json":
                self._send_json(200, {"openapi": "3.1.0", "info": {"title": "Fluxwright HTTP API"}})
                return

            if auth != f"Bearer {EXPECTED_TOKEN}":
                self._send_json(401, {"error": {"code": "unauthorized", "message": "a bearer token is required"}})
                return

            if method == "GET" and self.path == "/v1/stats":
                self._send_json(200, {"active_pages": 2, "memory_ceiling_mb": 1024})
                return
            if method == "GET" and self.path == "/metrics":
                text = b"# TYPE fluxwright_jobs_total counter\nfluxwright_jobs_total 9\n"
                self.send_response(200)
                self.send_header("Content-Type", "text/plain; version=0.0.4; charset=utf-8")
                self.send_header("Content-Length", str(len(text)))
                self.end_headers()
                self.wfile.write(text)
                return
            if method == "POST" and self.path == "/v1/scrape":
                self._send_json(
                    200,
                    {
                        "url": body["url"],
                        "title": "Scraped",
                        "duration_ms": 11,
                        "engine": "http",
                        "markdown": "# Hello",
                    },
                )
                return
            if method == "POST" and self.path == "/v1/map":
                self._send_json(
                    200,
                    {
                        "url": body["url"],
                        "count": 2,
                        "links": ["https://example.com/", "https://example.com/about"],
                        "duration_ms": 5,
                        "engine": "http",
                    },
                )
                return
            if method == "POST" and self.path == "/v1/crawl":
                with state.lock:
                    state.crawl_polls["crawl-1"] = 0
                self._send_json(
                    202,
                    {
                        "id": "crawl-1",
                        "status": "queued",
                        "seed_url": body["url"],
                        "discovered": 1,
                        "completed": 0,
                        "failed": 0,
                        "pages": [],
                    },
                )
                return
            if method == "GET" and self.path == "/v1/crawl/crawl-1":
                with state.lock:
                    n = state.crawl_polls.get("crawl-1", 0) + 1
                    state.crawl_polls["crawl-1"] = n
                status = "completed" if n >= 2 else "running"
                pages = [{"url": "https://example.com", "depth": 0, "status": "ok"}] if status == "completed" else []
                self._send_json(200, {"id": "crawl-1", "status": status, "pages": pages})
                return
            if method == "DELETE" and self.path == "/v1/crawl/crawl-1":
                self._send_json(200, {"id": "crawl-1", "status": "cancelled"})
                return
            if method == "POST" and self.path == "/v1/jobs":
                with state.lock:
                    state.job_polls["job-1"] = 0
                self._send_json(202, {"id": "job-1", "status": "queued", "results": None})
                return
            if method == "GET" and self.path == "/v1/jobs/job-1":
                with state.lock:
                    n = state.job_polls.get("job-1", 0) + 1
                    state.job_polls["job-1"] = n
                status = "succeeded" if n >= 2 else "running"
                results = ["done"] if status == "succeeded" else None
                self._send_json(200, {"id": "job-1", "status": status, "results": results})
                return
            if method == "DELETE" and self.path == "/v1/jobs/job-1":
                self._send_json(200, {"id": "job-1", "status": "cancelled"})
                return
            if method == "POST" and self.path == "/v1/sessions":
                self._send_json(201, {"id": "sess-1", "status": "active", "ttl_secs": body.get("ttl_secs", 300)})
                return
            if method == "GET" and self.path == "/v1/sessions":
                self._send_json(200, {"sessions": [{"id": "sess-1", "status": "active"}]})
                return
            if method == "GET" and self.path == "/v1/sessions/sess-1":
                self._send_json(200, {"id": "sess-1", "status": "active"})
                return
            if method == "POST" and self.path == "/v1/sessions/sess-1/extend":
                self._send_json(200, {"id": "sess-1", "status": "active", "ttl_secs": body.get("ttl_secs", 300)})
                return
            if method == "DELETE" and self.path == "/v1/sessions/sess-1":
                self._send_json(200, {"id": "sess-1", "status": "closed"})
                return
            if method == "GET" and self.path == "/v1/sessions/sess-1/files":
                with state.lock:
                    files = [{"name": k, "size": len(v)} for k, v in state.files.items()]
                self._send_json(200, {"files": files})
                return
            if method == "POST" and self.path == "/v1/sessions/sess-1/files":
                raw = base64.b64decode(body["content_base64"])
                with state.lock:
                    state.files[body["name"]] = raw
                self._send_json(201, {"name": body["name"], "size": len(raw)})
                return
            if method == "GET" and self.path.startswith("/v1/sessions/sess-1/files/"):
                name = unquote(self.path[len("/v1/sessions/sess-1/files/") :])
                with state.lock:
                    raw = state.files.get(name)
                if raw is None:
                    self._send_json(404, {"error": {"code": "file_not_found", "message": f"file '{name}' not found"}})
                    return
                self.send_response(200)
                self.send_header("Content-Type", "application/octet-stream")
                self.send_header("Content-Length", str(len(raw)))
                self.end_headers()
                self.wfile.write(raw)
                return
            if method == "DELETE" and self.path.startswith("/v1/sessions/sess-1/files/"):
                name = unquote(self.path[len("/v1/sessions/sess-1/files/") :])
                with state.lock:
                    existed = state.files.pop(name, None) is not None
                if not existed:
                    self._send_json(404, {"error": {"code": "file_not_found", "message": f"file '{name}' not found"}})
                    return
                self._send_json(200, {"deleted": True, "name": name})
                return

            self._send_json(404, {"error": {"code": "not_found", "message": "endpoint not found"}})

        def do_GET(self) -> None:
            self._handle("GET")

        def do_POST(self) -> None:
            self._handle("POST")

        def do_DELETE(self) -> None:
            self._handle("DELETE")

        def log_message(self, *args: object) -> None:
            pass

    return Handler


@pytest.fixture
def mock_server():
    state = _MockState()
    server = ThreadingHTTPServer(("127.0.0.1", 0), _make_handler(state))
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    yield f"http://127.0.0.1:{server.server_address[1]}", state
    server.shutdown()
    server.server_close()


def test_exports_and_validation() -> None:
    for name in ("FluxwrightClient", "AsyncFluxwrightClient", "FluxwrightApiError"):
        assert name in fluxwright.__all__
    assert issubclass(FluxwrightApiError, Error)

    with pytest.raises(ValueError):
        FluxwrightClient("", EXPECTED_TOKEN)
    with pytest.raises(ValueError):
        FluxwrightClient("ftp://example.com", EXPECTED_TOKEN)
    with pytest.raises(ValueError):
        FluxwrightClient("http://127.0.0.1:8080", "   ")
    with pytest.raises(ValueError):
        FluxwrightClient("http://127.0.0.1:8080", EXPECTED_TOKEN, timeout=0)

    client = FluxwrightClient("http://127.0.0.1:8080///", EXPECTED_TOKEN)
    assert client.base_url == "http://127.0.0.1:8080"
    assert EXPECTED_TOKEN not in repr(client)


def test_sync_client_all_endpoints_and_errors(mock_server) -> None:
    base_url, state = mock_server

    with FluxwrightClient(f"{base_url}/", EXPECTED_TOKEN, timeout=5.0) as client:
        assert client.healthz() == {"status": "ok"}
        assert client.openapi()["openapi"] == "3.1.0"
        assert client.stats()["memory_ceiling_mb"] == 1024
        assert "fluxwright_jobs_total 9" in client.metrics()

        assert client.scrape({"url": "https://example.com", "formats": ["markdown"]})["markdown"] == "# Hello"
        assert client.map({"url": "https://example.com"})["count"] == 2

        started_crawl = client.start_crawl({"url": "https://example.com"})
        assert started_crawl["status"] == "queued"
        assert client.get_crawl("crawl-1")["status"] == "running"
        assert client.cancel_crawl("crawl-1")["status"] == "cancelled"

        crawled = client.crawl({"url": "https://example.com"}, poll_interval=0.01, timeout=2.0)
        assert crawled["status"] == "completed"
        assert len(crawled["pages"]) == 1

        submitted_job = client.submit_job({"steps": [{"action": "title"}]})
        assert submitted_job["status"] == "queued"
        assert client.get_job("job-1")["status"] == "running"
        assert client.cancel_job("job-1")["status"] == "cancelled"

        job_done = client.run_job({"steps": [{"action": "title"}]}, poll_interval=0.01, timeout=2.0)
        assert job_done["status"] == "succeeded"
        assert job_done["results"] == ["done"]

        sess = client.create_session({"ttl_secs": 600})
        assert sess["id"] == "sess-1"
        assert len(client.list_sessions()["sessions"]) == 1
        assert client.get_session("sess-1")["status"] == "active"
        assert client.extend_session("sess-1", 900)["ttl_secs"] == 900

        bin_data = b"\x00\x01\x02\xff\xfe"
        assert client.upload_session_file("sess-1", "report 1.bin", bin_data)["size"] == len(bin_data)
        assert client.upload_session_file("sess-1", "note.txt", "hello world")["size"] == 11
        assert len(client.list_session_files("sess-1")["files"]) == 2
        assert client.download_session_file("sess-1", "report 1.bin") == bin_data
        assert client.download_session_file("sess-1", "note.txt") == b"hello world"
        assert client.delete_session_file("sess-1", "note.txt")["deleted"] is True
        assert client.delete_session("sess-1")["status"] == "closed"

        with pytest.raises(FluxwrightApiError) as missing_exc:
            client.download_session_file("sess-1", "missing.bin")
        assert missing_exc.value.status == 404
        assert missing_exc.value.code == "file_not_found"

    assert len(state.calls) >= 20
    assert all(call["auth"] == f"Bearer {EXPECTED_TOKEN}" for call in state.calls)

    with FluxwrightClient(base_url, "wrong-token") as bad_client, pytest.raises(FluxwrightApiError) as auth_exc:
        bad_client.stats()
    assert auth_exc.value.status == 401
    assert auth_exc.value.code == "unauthorized"
    assert auth_exc.value.body == {"error": {"code": "unauthorized", "message": "a bearer token is required"}}


def test_async_client_all_endpoints_and_errors(mock_server) -> None:
    base_url, state = mock_server

    async def _run() -> None:
        async with AsyncFluxwrightClient(base_url, EXPECTED_TOKEN, timeout=5.0) as client:
            assert EXPECTED_TOKEN not in repr(client)
            assert (await client.healthz()) == {"status": "ok"}
            assert (await client.openapi())["openapi"] == "3.1.0"
            assert (await client.stats())["active_pages"] == 2
            assert "fluxwright_jobs_total 9" in (await client.metrics())
            assert (await client.scrape({"url": "https://example.com"}))["title"] == "Scraped"
            assert (await client.map({"url": "https://example.com"}))["count"] == 2

            crawled = await client.crawl({"url": "https://example.com"}, poll_interval=0.01, timeout=2.0)
            assert crawled["status"] == "completed"
            assert (await client.cancel_crawl("crawl-1"))["status"] == "cancelled"

            job = await client.run_job({"steps": [{"action": "title"}]}, poll_interval=0.01, timeout=2.0)
            assert job["status"] == "succeeded"
            assert (await client.cancel_job("job-1"))["status"] == "cancelled"

            sess = await client.create_session()
            assert sess["id"] == "sess-1"
            assert len((await client.list_sessions())["sessions"]) == 1
            assert (await client.get_session("sess-1"))["status"] == "active"
            assert (await client.extend_session("sess-1", 450))["ttl_secs"] == 450

            payload = bytearray(b"async-bytes-\xff")
            assert (await client.upload_session_file("sess-1", "async.bin", payload))["size"] == len(payload)
            assert len((await client.list_session_files("sess-1"))["files"]) == 1
            assert (await client.download_session_file("sess-1", "async.bin")) == bytes(payload)
            assert (await client.delete_session_file("sess-1", "async.bin"))["deleted"] is True
            assert (await client.delete_session("sess-1"))["status"] == "closed"

        async with AsyncFluxwrightClient(base_url, "wrong-token") as bad_client:
            with pytest.raises(FluxwrightApiError) as exc_info:
                await bad_client.stats()
            assert exc_info.value.status == 401
            assert exc_info.value.code == "unauthorized"

    asyncio.run(_run())
    assert any(call["auth"] == f"Bearer {EXPECTED_TOKEN}" for call in state.calls)
