"""khive.mcp against a fake khive-cloud MCP server (offline, streamable HTTP)."""

from __future__ import annotations

import asyncio

import pytest

pytest.importorskip("mcp")

from khive import AuthError
from khive.mcp import mcp_list_tools, mcp_request, mcp_session


def test_mcp_list_tools(mcp_server, api_key):
    assert mcp_list_tools(mcp_server.url, api_key) == ["request"]


def test_mcp_request_round_trip(mcp_server, api_key):
    result = mcp_request(mcp_server.url, api_key, '[{"tool": "stats", "args": {}}]')
    assert result["summary"]["succeeded"] == 1
    assert result["results"][0]["result"] == {"entities": 1, "edges": 0, "notes": 0}


def test_mcp_wrong_key_raises_auth_error(mcp_server):
    with pytest.raises(AuthError):
        mcp_request(mcp_server.url, "wrong-key", '[{"tool": "stats", "args": {}}]')


def test_mcp_session_context_manager(mcp_server, api_key):
    async def _inner():
        async with mcp_session(mcp_server.url, api_key) as session:
            tools = await session.list_tools()
            assert [t.name for t in tools.tools] == ["request"]

    asyncio.run(_inner())


def test_sync_call_inside_running_loop_raises_runtime_error(mcp_server, api_key):
    async def _inner():
        with pytest.raises(RuntimeError):
            mcp_request(mcp_server.url, api_key, '[{"tool": "stats", "args": {}}]')

    asyncio.run(_inner())
