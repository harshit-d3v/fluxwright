"""Typed synchronous and asynchronous HTTP SDK clients for ``fluxwright serve``."""

from __future__ import annotations

import asyncio
import base64
import json
import math
import time
from collections.abc import Mapping
from typing import Any
from urllib.error import HTTPError, URLError
from urllib.parse import quote, urlparse
from urllib.request import Request, urlopen

from ._errors import Error, TimeoutError as FluxwrightTimeoutError

__all__ = ["AsyncFluxwrightClient", "FluxwrightApiError", "FluxwrightClient"]

_CRAWL_TERMINAL_STATUSES = frozenset({"completed", "failed", "cancelled"})
_JOB_TERMINAL_STATUSES = frozenset({"succeeded", "failed", "cancelled"})


class FluxwrightApiError(Error):
    """Raised when a ``fluxwright serve`` HTTP endpoint returns a non-2xx response."""

    def __init__(
        self,
        message: str,
        *,
        status: int,
        code: str | None = None,
        body: Any = None,
    ) -> None:
        super().__init__(message, name="FluxwrightApiError")
        self.status = status
        self.code = code
        self.body = body


def _validate_non_empty_str(value: str, name: str) -> str:
    if not isinstance(value, str) or not value.strip():
        raise ValueError(f"FluxwrightClient: {name} must be a non-empty string")
    return value.strip()


def _validate_positive_float(value: float, name: str) -> float:
    if isinstance(value, bool) or not isinstance(value, (int, float)) or not math.isfinite(value) or value <= 0:
        raise ValueError(f"FluxwrightClient: {name} must be a positive finite number")
    return float(value)


def _normalize_base_url(base_url: str) -> str:
    raw = _validate_non_empty_str(base_url, "base_url")
    parsed = urlparse(raw)
    if parsed.scheme not in ("http", "https") or not parsed.netloc:
        raise ValueError(f"FluxwrightClient: base_url must be a valid http:// or https:// URL: {raw}")
    return raw.rstrip("/")


def _encode_segment(value: str, name: str) -> str:
    return quote(_validate_non_empty_str(value, name), safe="")


def _to_base64(content: bytes | bytearray | memoryview | str) -> str:
    if isinstance(content, str):
        raw = content.encode("utf-8")
    elif isinstance(content, (bytes, bytearray)):
        raw = bytes(content)
    elif isinstance(content, memoryview):
        raw = content.tobytes()
    else:
        raise TypeError("FluxwrightClient.upload_session_file: content must be bytes, bytearray, memoryview, or str")
    return base64.b64encode(raw).decode("ascii")


def _parse_api_error(status: int, reason: str, raw_bytes: bytes) -> FluxwrightApiError:
    raw_text = raw_bytes.decode("utf-8", errors="replace")
    parsed: Any = None
    if raw_text:
        try:
            parsed = json.loads(raw_text)
        except ValueError:
            parsed = None

    err_obj = parsed.get("error", parsed) if isinstance(parsed, dict) else None
    code = err_obj.get("code") if isinstance(err_obj, dict) and isinstance(err_obj.get("code"), str) else None
    msg = err_obj.get("message") if isinstance(err_obj, dict) and isinstance(err_obj.get("message"), str) else None
    if not msg:
        msg = raw_text.strip() or (f"HTTP {status} {reason}".strip() if reason else f"HTTP {status}")
    body = parsed if parsed is not None else (raw_text or None)
    return FluxwrightApiError(msg, status=status, code=code, body=body)


class FluxwrightClient:
    """Synchronous HTTP client for the ``fluxwright serve`` REST API."""

    def __init__(self, base_url: str, token: str, timeout: float = 30.0) -> None:
        self._base_url = _normalize_base_url(base_url)
        self._token = _validate_non_empty_str(token, "token")
        self._timeout = _validate_positive_float(timeout, "timeout")

    @property
    def base_url(self) -> str:
        return self._base_url

    def __repr__(self) -> str:
        return f"FluxwrightClient(base_url={self._base_url!r}, timeout={self._timeout!r})"

    def __enter__(self) -> FluxwrightClient:
        return self

    def __exit__(self, *args: object) -> None:
        self.close()

    def close(self) -> None:
        """Close the client (no-op for stdlib urllib, provided for context manager symmetry)."""

    def _request(
        self,
        method: str,
        path: str,
        *,
        body: Mapping[str, Any] | None = None,
        response_type: str = "json",
        timeout: float | None = None,
    ) -> Any:
        effective_timeout = self._timeout if timeout is None else _validate_positive_float(timeout, "timeout")
        url = f"{self._base_url}{path}"
        headers: dict[str, str] = {"Authorization": f"Bearer {self._token}"}
        if response_type == "json":
            headers["Accept"] = "application/json"
        elif response_type == "text":
            headers["Accept"] = "text/plain"

        data: bytes | None = None
        if body is not None:
            headers["Content-Type"] = "application/json"
            data = json.dumps(dict(body)).encode("utf-8")

        req = Request(url, data=data, headers=headers, method=method)
        try:
            with urlopen(req, timeout=effective_timeout) as resp:
                raw = resp.read()
        except HTTPError as exc:
            err_body = exc.read()
            raise _parse_api_error(exc.code, getattr(exc, "reason", ""), err_body) from exc
        except TimeoutError as exc:
            raise FluxwrightTimeoutError(
                f"FluxwrightClient: {method} {path} timed out after {effective_timeout}s"
            ) from exc
        except URLError as exc:
            if isinstance(exc.reason, TimeoutError):
                raise FluxwrightTimeoutError(
                    f"FluxwrightClient: {method} {path} timed out after {effective_timeout}s"
                ) from exc
            raise Error(f"FluxwrightClient: {method} {path} failed: {exc.reason}") from exc

        if response_type == "bytes":
            return raw
        if response_type == "text":
            return raw.decode("utf-8")
        if not raw:
            return {}
        return json.loads(raw.decode("utf-8"))

    def healthz(self) -> dict[str, Any]:
        return self._request("GET", "/healthz")

    def openapi(self) -> dict[str, Any]:
        return self._request("GET", "/openapi.json")

    def stats(self) -> dict[str, Any]:
        return self._request("GET", "/v1/stats")

    def metrics(self) -> str:
        return self._request("GET", "/metrics", response_type="text")

    def scrape(self, request: Mapping[str, Any]) -> dict[str, Any]:
        if not isinstance(request, Mapping):
            raise TypeError("FluxwrightClient.scrape: request must be a mapping")
        return self._request("POST", "/v1/scrape", body=request)

    def map(self, request: Mapping[str, Any]) -> dict[str, Any]:
        if not isinstance(request, Mapping):
            raise TypeError("FluxwrightClient.map: request must be a mapping")
        return self._request("POST", "/v1/map", body=request)

    def start_crawl(self, request: Mapping[str, Any]) -> dict[str, Any]:
        if not isinstance(request, Mapping):
            raise TypeError("FluxwrightClient.start_crawl: request must be a mapping")
        return self._request("POST", "/v1/crawl", body=request)

    def get_crawl(self, crawl_id: str) -> dict[str, Any]:
        return self._request("GET", f"/v1/crawl/{_encode_segment(crawl_id, 'crawl_id')}")

    def cancel_crawl(self, crawl_id: str) -> dict[str, Any]:
        return self._request("DELETE", f"/v1/crawl/{_encode_segment(crawl_id, 'crawl_id')}")

    def crawl(
        self,
        request: Mapping[str, Any],
        *,
        poll_interval: float = 0.25,
        timeout: float | None = None,
    ) -> dict[str, Any]:
        interval = _validate_positive_float(poll_interval, "poll_interval")
        total_timeout = self._timeout if timeout is None else _validate_positive_float(timeout, "timeout")
        deadline = time.monotonic() + total_timeout

        snapshot = self.start_crawl(request)
        while snapshot.get("status") not in _CRAWL_TERMINAL_STATUSES:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise FluxwrightTimeoutError(
                    f"FluxwrightClient.crawl: crawl {snapshot.get('id', '')} timed out after {total_timeout}s"
                )
            time.sleep(min(interval, remaining))
            req_remaining = deadline - time.monotonic()
            if req_remaining <= 0:
                raise FluxwrightTimeoutError(
                    f"FluxwrightClient.crawl: crawl {snapshot.get('id', '')} timed out after {total_timeout}s"
                )
            snapshot = self._request(
                "GET",
                f"/v1/crawl/{_encode_segment(str(snapshot['id']), 'crawl_id')}",
                timeout=min(self._timeout, req_remaining),
            )
        return snapshot

    def submit_job(self, request: Mapping[str, Any]) -> dict[str, Any]:
        if not isinstance(request, Mapping):
            raise TypeError("FluxwrightClient.submit_job: request must be a mapping")
        return self._request("POST", "/v1/jobs", body=request)

    def get_job(self, job_id: str) -> dict[str, Any]:
        return self._request("GET", f"/v1/jobs/{_encode_segment(job_id, 'job_id')}")

    def cancel_job(self, job_id: str) -> dict[str, Any]:
        return self._request("DELETE", f"/v1/jobs/{_encode_segment(job_id, 'job_id')}")

    def run_job(
        self,
        request: Mapping[str, Any],
        *,
        poll_interval: float = 0.2,
        timeout: float | None = None,
    ) -> dict[str, Any]:
        interval = _validate_positive_float(poll_interval, "poll_interval")
        total_timeout = self._timeout if timeout is None else _validate_positive_float(timeout, "timeout")
        deadline = time.monotonic() + total_timeout

        job = self.submit_job(request)
        while job.get("status") not in _JOB_TERMINAL_STATUSES:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise FluxwrightTimeoutError(
                    f"FluxwrightClient.run_job: job {job.get('id', '')} timed out after {total_timeout}s"
                )
            time.sleep(min(interval, remaining))
            req_remaining = deadline - time.monotonic()
            if req_remaining <= 0:
                raise FluxwrightTimeoutError(
                    f"FluxwrightClient.run_job: job {job.get('id', '')} timed out after {total_timeout}s"
                )
            job = self._request(
                "GET",
                f"/v1/jobs/{_encode_segment(str(job['id']), 'job_id')}",
                timeout=min(self._timeout, req_remaining),
            )
        return job

    def create_session(self, request: Mapping[str, Any] | None = None) -> dict[str, Any]:
        payload: Mapping[str, Any] = {} if request is None else request
        if not isinstance(payload, Mapping):
            raise TypeError("FluxwrightClient.create_session: request must be a mapping")
        return self._request("POST", "/v1/sessions", body=payload)

    def list_sessions(self) -> dict[str, Any]:
        return self._request("GET", "/v1/sessions")

    def get_session(self, session_id: str) -> dict[str, Any]:
        return self._request("GET", f"/v1/sessions/{_encode_segment(session_id, 'session_id')}")

    def extend_session(self, session_id: str, ttl_secs: int | None = None) -> dict[str, Any]:
        body: dict[str, Any] = {}
        if ttl_secs is not None:
            if isinstance(ttl_secs, bool) or not isinstance(ttl_secs, int) or ttl_secs <= 0:
                raise ValueError("FluxwrightClient.extend_session: ttl_secs must be a positive integer")
            body["ttl_secs"] = ttl_secs
        return self._request("POST", f"/v1/sessions/{_encode_segment(session_id, 'session_id')}/extend", body=body)

    def delete_session(self, session_id: str) -> dict[str, Any]:
        return self._request("DELETE", f"/v1/sessions/{_encode_segment(session_id, 'session_id')}")

    def list_session_files(self, session_id: str) -> dict[str, Any]:
        return self._request("GET", f"/v1/sessions/{_encode_segment(session_id, 'session_id')}/files")

    def upload_session_file(
        self,
        session_id: str,
        name: str,
        content: bytes | bytearray | memoryview | str,
    ) -> dict[str, Any]:
        file_name = _validate_non_empty_str(name, "name")
        payload = {"name": file_name, "content_base64": _to_base64(content)}
        return self._request("POST", f"/v1/sessions/{_encode_segment(session_id, 'session_id')}/files", body=payload)

    def download_session_file(self, session_id: str, name: str) -> bytes:
        seg_id = _encode_segment(session_id, "session_id")
        seg_name = _encode_segment(name, "name")
        return self._request("GET", f"/v1/sessions/{seg_id}/files/{seg_name}", response_type="bytes")

    def delete_session_file(self, session_id: str, name: str) -> dict[str, Any]:
        seg_id = _encode_segment(session_id, "session_id")
        seg_name = _encode_segment(name, "name")
        return self._request("DELETE", f"/v1/sessions/{seg_id}/files/{seg_name}")


class AsyncFluxwrightClient:
    """Asynchronous HTTP client for ``fluxwright serve``, powered by ``asyncio.to_thread``."""

    def __init__(self, base_url: str, token: str, timeout: float = 30.0) -> None:
        self._sync = FluxwrightClient(base_url, token, timeout=timeout)

    @property
    def base_url(self) -> str:
        return self._sync.base_url

    def __repr__(self) -> str:
        return f"AsyncFluxwrightClient(base_url={self._sync.base_url!r}, timeout={self._sync._timeout!r})"

    async def __aenter__(self) -> AsyncFluxwrightClient:
        return self

    async def __aexit__(self, *args: object) -> None:
        await self.aclose()

    async def aclose(self) -> None:
        self._sync.close()

    async def healthz(self) -> dict[str, Any]:
        return await asyncio.to_thread(self._sync.healthz)

    async def openapi(self) -> dict[str, Any]:
        return await asyncio.to_thread(self._sync.openapi)

    async def stats(self) -> dict[str, Any]:
        return await asyncio.to_thread(self._sync.stats)

    async def metrics(self) -> str:
        return await asyncio.to_thread(self._sync.metrics)

    async def scrape(self, request: Mapping[str, Any]) -> dict[str, Any]:
        return await asyncio.to_thread(self._sync.scrape, request)

    async def map(self, request: Mapping[str, Any]) -> dict[str, Any]:
        return await asyncio.to_thread(self._sync.map, request)

    async def start_crawl(self, request: Mapping[str, Any]) -> dict[str, Any]:
        return await asyncio.to_thread(self._sync.start_crawl, request)

    async def get_crawl(self, crawl_id: str) -> dict[str, Any]:
        return await asyncio.to_thread(self._sync.get_crawl, crawl_id)

    async def cancel_crawl(self, crawl_id: str) -> dict[str, Any]:
        return await asyncio.to_thread(self._sync.cancel_crawl, crawl_id)

    async def crawl(
        self,
        request: Mapping[str, Any],
        *,
        poll_interval: float = 0.25,
        timeout: float | None = None,
    ) -> dict[str, Any]:
        interval = _validate_positive_float(poll_interval, "poll_interval")
        total_timeout = self._sync._timeout if timeout is None else _validate_positive_float(timeout, "timeout")
        deadline = time.monotonic() + total_timeout

        snapshot = await self.start_crawl(request)
        while snapshot.get("status") not in _CRAWL_TERMINAL_STATUSES:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise FluxwrightTimeoutError(
                    f"AsyncFluxwrightClient.crawl: crawl {snapshot.get('id', '')} timed out after {total_timeout}s"
                )
            await asyncio.sleep(min(interval, remaining))
            req_remaining = deadline - time.monotonic()
            if req_remaining <= 0:
                raise FluxwrightTimeoutError(
                    f"AsyncFluxwrightClient.crawl: crawl {snapshot.get('id', '')} timed out after {total_timeout}s"
                )
            snapshot = await asyncio.to_thread(
                self._sync._request,
                "GET",
                f"/v1/crawl/{_encode_segment(str(snapshot['id']), 'crawl_id')}",
                timeout=min(self._sync._timeout, req_remaining),
            )
        return snapshot

    async def submit_job(self, request: Mapping[str, Any]) -> dict[str, Any]:
        return await asyncio.to_thread(self._sync.submit_job, request)

    async def get_job(self, job_id: str) -> dict[str, Any]:
        return await asyncio.to_thread(self._sync.get_job, job_id)

    async def cancel_job(self, job_id: str) -> dict[str, Any]:
        return await asyncio.to_thread(self._sync.cancel_job, job_id)

    async def run_job(
        self,
        request: Mapping[str, Any],
        *,
        poll_interval: float = 0.2,
        timeout: float | None = None,
    ) -> dict[str, Any]:
        interval = _validate_positive_float(poll_interval, "poll_interval")
        total_timeout = self._sync._timeout if timeout is None else _validate_positive_float(timeout, "timeout")
        deadline = time.monotonic() + total_timeout

        job = await self.submit_job(request)
        while job.get("status") not in _JOB_TERMINAL_STATUSES:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise FluxwrightTimeoutError(
                    f"AsyncFluxwrightClient.run_job: job {job.get('id', '')} timed out after {total_timeout}s"
                )
            await asyncio.sleep(min(interval, remaining))
            req_remaining = deadline - time.monotonic()
            if req_remaining <= 0:
                raise FluxwrightTimeoutError(
                    f"AsyncFluxwrightClient.run_job: job {job.get('id', '')} timed out after {total_timeout}s"
                )
            job = await asyncio.to_thread(
                self._sync._request,
                "GET",
                f"/v1/jobs/{_encode_segment(str(job['id']), 'job_id')}",
                timeout=min(self._sync._timeout, req_remaining),
            )
        return job

    async def create_session(self, request: Mapping[str, Any] | None = None) -> dict[str, Any]:
        return await asyncio.to_thread(self._sync.create_session, request)

    async def list_sessions(self) -> dict[str, Any]:
        return await asyncio.to_thread(self._sync.list_sessions)

    async def get_session(self, session_id: str) -> dict[str, Any]:
        return await asyncio.to_thread(self._sync.get_session, session_id)

    async def extend_session(self, session_id: str, ttl_secs: int | None = None) -> dict[str, Any]:
        return await asyncio.to_thread(self._sync.extend_session, session_id, ttl_secs)

    async def delete_session(self, session_id: str) -> dict[str, Any]:
        return await asyncio.to_thread(self._sync.delete_session, session_id)

    async def list_session_files(self, session_id: str) -> dict[str, Any]:
        return await asyncio.to_thread(self._sync.list_session_files, session_id)

    async def upload_session_file(
        self,
        session_id: str,
        name: str,
        content: bytes | bytearray | memoryview | str,
    ) -> dict[str, Any]:
        return await asyncio.to_thread(self._sync.upload_session_file, session_id, name, content)

    async def download_session_file(self, session_id: str, name: str) -> bytes:
        return await asyncio.to_thread(self._sync.download_session_file, session_id, name)

    async def delete_session_file(self, session_id: str, name: str) -> dict[str, Any]:
        return await asyncio.to_thread(self._sync.delete_session_file, session_id, name)
