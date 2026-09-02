"""khive.mcp against a fake khive-cloud MCP server (offline, streamable HTTP)."""

from __future__ import annotations

import asyncio
import logging
from contextlib import asynccontextmanager

import pytest

pytest.importorskip("mcp")

import khive.mcp as mcp_module
from khive import AuthError, KhiveError
from khive.mcp import (
    _TRANSPORT_LOGGER,
    _AcceptedTerminationFilter,
    _as_khive_error,
    _check_base_url_security,
    mcp_list_tools,
    mcp_request,
    mcp_session,
)


def test_mcp_list_tools(mcp_server, api_key):
    assert mcp_list_tools(mcp_server.url, api_key) == ["request"]


def test_mcp_request_round_trip(mcp_server, api_key):
    result = mcp_request(mcp_server.url, api_key, "stats()")
    assert result["summary"]["succeeded"] == 1
    assert result["results"][0]["result"] == {"entities": 1, "edges": 0, "notes": 0}


def test_mcp_wrong_key_raises_auth_error(mcp_server):
    with pytest.raises(AuthError) as exc_info:
        mcp_request(mcp_server.url, "wrong-key", "stats()")
    assert exc_info.value.status == 401


def test_mcp_session_refuses_non_loopback_http_without_allow_insecure(monkeypatch):
    def _must_not_connect(*args, **kwargs):
        raise AssertionError("must not construct an httpx client before the URL guard runs")

    monkeypatch.setattr("httpx.AsyncClient", _must_not_connect)

    async def _inner():
        async with mcp_session("http://example.test", "key"):
            pass

    with pytest.raises(ValueError, match="http.*example.test"):
        asyncio.run(_inner())


def test_mcp_session_allow_insecure_bypasses_url_guard(monkeypatch):
    built: list[bool] = []

    class _FakeAsyncClient:
        def __init__(self, *args, **kwargs) -> None:
            built.append(True)

        async def aclose(self) -> None:
            pass

    monkeypatch.setattr("httpx.AsyncClient", _FakeAsyncClient)

    async def _inner():
        # The URL guard is bypassed; the fake client has no real transport
        # underneath, so `streamable_http_client`/`session.initialize()` fail
        # downstream and surface as the translated client error — that
        # failure proves the guard did not block it.
        with pytest.raises(KhiveError):
            async with mcp_session("http://example.test", "key", allow_insecure=True):
                pass

    asyncio.run(_inner())
    assert built, "the client must have been constructed once the guard was bypassed"


def test_mcp_session_https_scheme_bypasses_guard_without_allow_insecure():
    # `_check_base_url_security` exempts every `https://` URL regardless of
    # host — verified directly, since standing up a real TLS endpoint here
    # would require network access this suite does not have.
    _check_base_url_security("https://example.test", False)


def test_mcp_session_loopback_http_admitted_without_allow_insecure(mcp_server, api_key):
    # `mcp_server.url` is `http://127.0.0.1:<port>` and every other MCP test
    # in this file calls it with `allow_insecure` left at its default
    # (`False`); this test names that guard behavior explicitly.
    result = mcp_request(mcp_server.url, api_key, "stats()")
    assert result["summary"]["succeeded"] == 1


def test_mcp_session_context_manager(mcp_server, api_key):
    async def _inner():
        async with mcp_session(mcp_server.url, api_key) as session:
            tools = await session.list_tools()
            assert [t.name for t in tools.tools] == ["request"]

    asyncio.run(_inner())


def test_body_exception_inside_mcp_session_propagates_unchanged(mcp_server, api_key):
    class _Boom(Exception):
        pass

    async def _inner():
        async with mcp_session(mcp_server.url, api_key):
            raise _Boom("caller failure")

    with pytest.raises(_Boom):
        asyncio.run(_inner())


def test_cancelled_error_inside_mcp_session_propagates_unchanged(mcp_server, api_key):
    async def _inner():
        async with mcp_session(mcp_server.url, api_key):
            raise asyncio.CancelledError("caller cancellation")

    with pytest.raises(asyncio.CancelledError):
        asyncio.run(_inner())


def test_alist_tool_names_translates_call_time_failure(monkeypatch):
    class _FailingSession:
        async def list_tools(self):
            raise RuntimeError("network blip")

    @asynccontextmanager
    async def _fake_session(base_url, api_key, *, allow_insecure=False):
        yield _FailingSession()

    monkeypatch.setattr(mcp_module, "mcp_session", _fake_session)
    with pytest.raises(KhiveError):
        asyncio.run(mcp_module.alist_tool_names("http://example.test", "key"))


def test_acall_request_translates_call_time_failure(monkeypatch):
    class _FailingSession:
        async def call_tool(self, name, args):
            raise RuntimeError("network blip")

    @asynccontextmanager
    async def _fake_session(base_url, api_key, *, allow_insecure=False):
        yield _FailingSession()

    monkeypatch.setattr(mcp_module, "mcp_session", _fake_session)
    with pytest.raises(KhiveError):
        asyncio.run(mcp_module.acall_request("http://example.test", "key", "stats()"))


def test_as_khive_error_preserves_403_status():
    httpx = pytest.importorskip("httpx")

    request = httpx.Request("POST", "http://example.test/mcp")
    response = httpx.Response(403, request=request, text="forbidden")
    exc = httpx.HTTPStatusError("403 Forbidden", request=request, response=response)
    err = _as_khive_error(exc, "http://example.test/mcp")
    assert isinstance(err, AuthError)
    assert err.status == 403


def test_sync_call_inside_running_loop_raises_runtime_error(mcp_server, api_key):
    async def _inner():
        with pytest.raises(RuntimeError):
            mcp_request(mcp_server.url, api_key, "stats()")

    asyncio.run(_inner())


def _termination_record(status: str) -> logging.LogRecord:
    return logging.LogRecord(
        _TRANSPORT_LOGGER,
        logging.WARNING,
        __file__,
        0,
        "Session termination failed: %s",
        (status,),
        None,
    )


def test_accepted_termination_filter_drops_2xx_and_keeps_the_rest():
    flt = _AcceptedTerminationFilter()
    assert not flt.filter(_termination_record("202"))
    assert not flt.filter(_termination_record("200"))
    assert flt.filter(_termination_record("500"))
    assert flt.filter(_termination_record("405"))
    other = logging.LogRecord(
        _TRANSPORT_LOGGER, logging.WARNING, __file__, 0, "other %s", ("202",), None
    )
    assert flt.filter(other)


def test_accepted_termination_is_silent_inside_mcp_session_and_filter_is_removed_after(
    mcp_server, api_key, caplog
):
    transport_log = logging.getLogger(_TRANSPORT_LOGGER)
    before = list(transport_log.filters)

    async def _inner():
        async with mcp_session(mcp_server.url, api_key):
            with caplog.at_level(logging.WARNING, logger=_TRANSPORT_LOGGER):
                transport_log.warning("Session termination failed: %s", 202)
                transport_log.warning("Session termination failed: %s", 500)

    asyncio.run(_inner())
    messages = [r.getMessage() for r in caplog.records]
    assert "Session termination failed: 202" not in messages
    assert "Session termination failed: 500" in messages
    assert list(transport_log.filters) == before
    # Outside a session the SDK's warning is untouched.
    with caplog.at_level(logging.WARNING, logger=_TRANSPORT_LOGGER):
        transport_log.warning("Session termination failed: %s", 202)
    assert [r.getMessage() for r in caplog.records].count("Session termination failed: 202") == 1
