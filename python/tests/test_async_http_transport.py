"""AsyncHttpTransport against a fake khive-cloud REST endpoint (offline, no daemon).

Mirrors `test_http_transport.py`'s coverage for the sync transport — the two
implementations duplicate their GET/POST, header, timeout, and error paths,
so they need independent regression coverage rather than relying on the sync
tests alone. No async pytest plugin is a dev dependency, so each test drives
its coroutine with a bare `asyncio.run` (same pattern as `test_mcp.py`).
"""

from __future__ import annotations

import asyncio

import pytest

httpx = pytest.importorskip("httpx")

from khive import AsyncHttpTransport, AuthError, RateLimited, ServerError, TransportError
from khive.ops import encode, op


def _run(coro):
    return asyncio.run(coro)


def test_handshake_never_posts(rest_server, api_key):
    async def _inner():
        transport = AsyncHttpTransport(rest_server.url, api_key)
        try:
            response = await transport.round_trip({"ops": "", "metrics_only": True}, timeout=5.0)
            assert response["served_config_id"] == f"http:{rest_server.url}"
            assert response["metrics"] == {"status": "ok"}
        finally:
            await transport.aclose()

    _run(_inner())


def test_post_round_trip(rest_server, api_key):
    async def _inner():
        transport = AsyncHttpTransport(rest_server.url, api_key)
        try:
            response = await transport.round_trip({"ops": encode([op("stats")])}, timeout=5.0)
            envelope = response["result"]
            assert envelope["summary"]["succeeded"] == 1
            assert envelope["results"][0]["result"] == {"entities": 1, "edges": 0, "notes": 0}
        finally:
            await transport.aclose()

    _run(_inner())


def test_wrong_key_raises_auth_error(rest_server):
    async def _inner():
        transport = AsyncHttpTransport(rest_server.url, "wrong-key")
        try:
            with pytest.raises(AuthError) as exc_info:
                await transport.round_trip({"ops": encode([op("stats")])}, timeout=5.0)
            assert exc_info.value.status == 401
        finally:
            await transport.aclose()

    _run(_inner())


def test_rate_limited_raises_rate_limited(rest_server, api_key):
    async def _inner():
        transport = AsyncHttpTransport(rest_server.url, api_key)
        try:
            with pytest.raises(RateLimited) as exc_info:
                await transport.round_trip({"ops": encode([op("rate_limited")])}, timeout=5.0)
            assert exc_info.value.status == 429
        finally:
            await transport.aclose()

    _run(_inner())


def test_server_error_raises_server_error(rest_server, api_key):
    async def _inner():
        transport = AsyncHttpTransport(rest_server.url, api_key)
        try:
            with pytest.raises(ServerError) as exc_info:
                await transport.round_trip({"ops": encode([op("boom")])}, timeout=5.0)
            assert exc_info.value.status == 500
        finally:
            await transport.aclose()

    _run(_inner())


def test_connection_error_raises_transport_error():
    async def _inner():
        transport = AsyncHttpTransport("http://127.0.0.1:1", "key", timeout=2.0)
        try:
            with pytest.raises(TransportError):
                await transport.round_trip({"ops": encode([op("stats")])}, timeout=2.0)
        finally:
            await transport.aclose()

    _run(_inner())


def test_connection_error_on_metrics_raises_transport_error():
    async def _inner():
        transport = AsyncHttpTransport("http://127.0.0.1:1", "key", timeout=2.0)
        try:
            with pytest.raises(TransportError):
                await transport.round_trip({"ops": "", "metrics_only": True}, timeout=2.0)
        finally:
            await transport.aclose()

    _run(_inner())


def test_aclose_closes_the_underlying_client(rest_server, api_key):
    async def _inner():
        transport = AsyncHttpTransport(rest_server.url, api_key)
        await transport.round_trip({"ops": encode([op("stats")])}, timeout=5.0)
        await transport.aclose()
        assert transport._client.is_closed

    _run(_inner())
