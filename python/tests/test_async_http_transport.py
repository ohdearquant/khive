"""AsyncHttpTransport against a fake khive-cloud REST endpoint (offline, no daemon).

Mirrors `test_http_transport.py`'s coverage for the sync transport — the two
implementations duplicate their GET/POST, header, timeout, and error paths,
so they need independent regression coverage rather than relying on the sync
tests alone. No async pytest plugin is a dev dependency, so each test drives
its coroutine with a bare `asyncio.run` (same pattern as `test_mcp.py`).
"""

from __future__ import annotations

import asyncio
import json

import pytest

httpx = pytest.importorskip("httpx")

from test_http_transport import _malformed_server

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


def test_op_error_entry_flattened(rest_server, api_key):
    async def _inner():
        transport = AsyncHttpTransport(rest_server.url, api_key)
        try:
            response = await transport.round_trip({"ops": encode([op("nope")])}, timeout=5.0)
            entry = response["result"]["results"][0]
            assert entry["ok"] is False
            assert "verb_not_found" in entry["error"]
        finally:
            await transport.aclose()

    _run(_inner())


def test_non_dict_result_entry_raises_transport_error():
    async def _inner():
        body = json.dumps({"results": [42]}).encode()
        with _malformed_server(body) as url:
            transport = AsyncHttpTransport(url, "key")
            try:
                with pytest.raises(TransportError):
                    await transport.round_trip({"ops": encode([op("stats")])}, timeout=5.0)
            finally:
                await transport.aclose()

    _run(_inner())


def test_chained_abort_returns_minimal_entry(rest_server, api_key):
    async def _inner():
        transport = AsyncHttpTransport(rest_server.url, api_key)
        try:
            response = await transport.round_trip(
                {"ops": json.dumps("nope() | stats()")}, timeout=5.0
            )
        finally:
            await transport.aclose()
        return response["result"]["results"]

    results = _run(_inner())
    assert results[0]["ok"] is False and results[0]["tool"] == "nope"
    assert results[1] == {"ok": False, "aborted": True, "tool": ""}


def test_malformed_error_object_bad_code_type_raises_transport_error():
    async def _inner():
        body = json.dumps(
            {"results": [{"ok": False, "tool": "x", "error": {"code": [], "message": {}}}]}
        ).encode()
        with _malformed_server(body) as url:
            transport = AsyncHttpTransport(url, "key")
            try:
                with pytest.raises(TransportError):
                    await transport.round_trip({"ops": encode([op("x")])}, timeout=5.0)
            finally:
                await transport.aclose()

    _run(_inner())


def test_malformed_error_object_missing_message_raises_transport_error():
    async def _inner():
        body = json.dumps(
            {"results": [{"ok": False, "tool": "x", "error": {"code": "boom"}}]}
        ).encode()
        with _malformed_server(body) as url:
            transport = AsyncHttpTransport(url, "key")
            try:
                with pytest.raises(TransportError):
                    await transport.round_trip({"ops": encode([op("x")])}, timeout=5.0)
            finally:
                await transport.aclose()

    _run(_inner())


def test_non_json_2xx_body_raises_transport_error():
    async def _inner():
        with _malformed_server(b"not json at all") as url:
            transport = AsyncHttpTransport(url, "key")
            try:
                with pytest.raises(TransportError):
                    await transport.round_trip({"ops": encode([op("stats")])}, timeout=5.0)
            finally:
                await transport.aclose()

    _run(_inner())


def test_json_2xx_body_missing_results_raises_transport_error():
    async def _inner():
        body = json.dumps({"ok": True}).encode()
        with _malformed_server(body) as url:
            transport = AsyncHttpTransport(url, "key")
            try:
                with pytest.raises(TransportError):
                    await transport.round_trip({"ops": encode([op("stats")])}, timeout=5.0)
            finally:
                await transport.aclose()

    _run(_inner())


def test_json_2xx_body_that_is_a_list_raises_transport_error():
    async def _inner():
        body = json.dumps(["not", "an", "envelope"]).encode()
        with _malformed_server(body) as url:
            transport = AsyncHttpTransport(url, "key")
            try:
                with pytest.raises(TransportError):
                    await transport.round_trip({"ops": encode([op("stats")])}, timeout=5.0)
            finally:
                await transport.aclose()

    _run(_inner())


def test_non_loopback_http_base_url_refused():
    with pytest.raises(ValueError, match="http.*example.test"):
        AsyncHttpTransport("http://example.test", "key")


def test_loopback_http_base_url_admitted(rest_server, api_key):
    async def _inner():
        transport = AsyncHttpTransport(rest_server.url, api_key)
        try:
            response = await transport.round_trip({"ops": encode([op("stats")])}, timeout=5.0)
            assert response["result"]["results"][0]["result"] == {
                "entities": 1,
                "edges": 0,
                "notes": 0,
            }
        finally:
            await transport.aclose()

    _run(_inner())


def test_allow_insecure_admits_non_loopback_http_base_url():
    transport = AsyncHttpTransport("http://example.test", "key", allow_insecure=True)
    assert transport._base_url == "http://example.test"
