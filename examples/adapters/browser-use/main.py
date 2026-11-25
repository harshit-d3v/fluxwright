"""Browser-Use adapter backed by a Fluxwright managed cloud session."""

from __future__ import annotations

import asyncio
from collections import Counter
import os
from urllib.parse import quote, urljoin

from browser_use import Agent, BrowserSession, ChatOpenAI
import httpx


def require_env(name: str) -> str:
    value = os.getenv(name, "").strip()
    if not value:
        raise RuntimeError(f"{name} environment variable is required")
    return value


async def fetch_replay_summary(
    client: httpx.AsyncClient,
    server_url: str,
    session_id: str,
) -> None:
    resp = await client.get(f"{server_url}/v1/sessions/{session_id}/replay")
    resp.raise_for_status()
    replay = resp.json()
    counts = Counter(event.get("kind", "unknown") for event in replay.get("events", []))
    print(
        f"Replay summary ({session_id}): status={replay.get('status')} "
        f"url={replay.get('url')} total_events={replay.get('total_events', 0)} "
        f"by_kind={dict(counts)}"
    )


async def close_session(
    client: httpx.AsyncClient,
    server_url: str,
    session_id: str,
) -> None:
    resp = await client.delete(f"{server_url}/v1/sessions/{session_id}")
    if resp.status_code != 404:
        resp.raise_for_status()


async def main() -> None:
    server_url = os.getenv("FLUXWRIGHT_SERVER_URL", "http://127.0.0.1:7331").rstrip("/")
    token = require_env("FLUXWRIGHT_SERVER_TOKEN")
    model = os.getenv("OPENAI_MODEL", "gpt-4o")
    task = os.getenv(
        "TASK",
        "Go to https://example.com and report the main heading and destination URL of its link.",
    )
    use_query_token = os.getenv("FLUXWRIGHT_CDP_QUERY_TOKEN", "").lower() in {"1", "true", "yes"}

    headers = {"Authorization": f"Bearer {token}"}
    async with httpx.AsyncClient(headers=headers, timeout=30.0) as client:
        create_resp = await client.post(
            f"{server_url}/v1/sessions",
            json={
                "ttl_secs": 300,
                "keep_alive": False,
                "live_view_mode": "interactive",
                "record_frames": True,
            },
        )
        create_resp.raise_for_status()
        session = create_resp.json()

        session_id: str = session["id"]
        cdp_ws_url: str = session["cdp_ws_url"]
        live_view_url = urljoin(f"{server_url}/", session["live_view_url"].lstrip("/"))
        print(f"Session created: {session_id}")
        print(f"Live View URL:   {live_view_url}")

        # Pass Bearer auth via handshake header by default, or ?token= when
        # `fluxwright serve --allow-query-token` is enabled.
        if use_query_token:
            sep = "&" if "?" in cdp_ws_url else "?"
            browser_session = BrowserSession(
                cdp_url=f"{cdp_ws_url}{sep}token={quote(token, safe='')}",
            )
        else:
            browser_session = BrowserSession(cdp_url=cdp_ws_url, headers=headers)

        try:
            agent = Agent(
                task=task,
                llm=ChatOpenAI(model=model),
                browser_session=browser_session,
            )
            history = await agent.run()
            print(f"Agent result:    {history.final_result()}")
        finally:
            try:
                await browser_session.stop()
            except Exception as exc:
                print(f"Warning: failed to stop BrowserSession cleanly: {exc}")

            try:
                await fetch_replay_summary(client, server_url, session_id)
            except Exception as exc:
                print(f"Warning: failed to fetch session replay: {exc}")

            await close_session(client, server_url, session_id)
            print(f"Session closed:  {session_id}")


if __name__ == "__main__":
    asyncio.run(main())
