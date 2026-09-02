"""Streamable-HTTP MCP helpers for khive-cloud.

khive-cloud mounts one MCP tool, `request`, at `{base_url}/mcp` over the
streamable-HTTP transport, gated by the same `Authorization: ApiKey <key>`
header as the REST endpoint. A bad key fails during `session.initialize()`
rather than as a normal tool error, and the MCP SDK's anyio-based transport
wraps that failure in one or more nested `ExceptionGroup`s — every entry
point here unwraps to the leaf exception before turning it into a
`khive` error.

`mcp` and `httpx` are optional dependencies (the `cloud` extra); this module
is only imported when a caller actually reaches for cloud/MCP functionality,
so a socket-only install never needs either package.
"""

from __future__ import annotations

import asyncio
import json
from collections.abc import AsyncIterator
from contextlib import asynccontextmanager
from typing import Any

from .errors import AuthError, KhiveError


def _root_cause(exc: BaseException) -> BaseException:
    """Descend nested ExceptionGroups / __cause__ chains to the leaf exception."""
    cur: BaseException = exc
    seen: set[int] = set()
    while id(cur) not in seen:
        seen.add(id(cur))
        subs = getattr(cur, "exceptions", None)
        if subs:
            cur = subs[0]
            continue
        cause = getattr(cur, "__cause__", None)
        if cause is not None and cause is not cur:
            cur = cause
            continue
        break
    return cur


def _as_khive_error(exc: Exception, url: str) -> KhiveError:
    root = _root_cause(exc)
    message = str(root)
    if "401" in message or "Unauthorized" in message or "403" in message or "Forbidden" in message:
        return AuthError(401, message, None, url)
    return KhiveError(message)


@asynccontextmanager
async def mcp_session(base_url: str, api_key: str) -> AsyncIterator[Any]:
    """Async context manager yielding an initialized `mcp.ClientSession`.

    Usage: ``async with mcp_session(url, key) as session: ...``
    """
    import httpx
    from mcp import ClientSession
    from mcp.client.streamable_http import streamable_http_client

    url = base_url.rstrip("/") + "/mcp"
    headers = {"Authorization": f"ApiKey {api_key}"}
    try:
        async with httpx.AsyncClient(headers=headers) as http_client:
            transport = streamable_http_client(url, http_client=http_client)
            async with (
                transport as (read, write, _get_session_id),
                ClientSession(read, write) as session,
            ):
                await session.initialize()
                yield session
    except KhiveError:
        raise
    except Exception as exc:
        raise _as_khive_error(exc, url) from exc


async def alist_tool_names(base_url: str, api_key: str) -> list[str]:
    """`tools/list`, returning just the tool names."""
    async with mcp_session(base_url, api_key) as session:
        result = await session.list_tools()
        return [tool.name for tool in result.tools]


async def acall_request(base_url: str, api_key: str, ops: str) -> Any:
    """Call the `request` tool with the given ops DSL and parse its JSON reply."""
    async with mcp_session(base_url, api_key) as session:
        result = await session.call_tool("request", {"ops": ops})
        if result.isError:
            text = result.content[0].text if result.content else "unknown MCP error"
            raise KhiveError(text)
        text = result.content[0].text if result.content else "null"
        return json.loads(text)


def _run_sync(make_coro: Any) -> Any:
    # `make_coro` is a zero-arg thunk, not an already-built coroutine: building
    # the coroutine only after confirming there is no running loop means the
    # "wrong context" path never leaves an unawaited coroutine behind.
    try:
        asyncio.get_running_loop()
    except RuntimeError:
        return asyncio.run(make_coro())
    raise RuntimeError(
        "this sync helper cannot be called from inside a running event loop — "
        "use mcp_session/alist_tool_names/acall_request directly instead"
    )


def mcp_list_tools(base_url: str, api_key: str) -> list[str]:
    """Sync convenience over `alist_tool_names` (`asyncio.run` under the hood)."""
    return _run_sync(lambda: alist_tool_names(base_url, api_key))


def mcp_request(base_url: str, api_key: str, ops: str) -> Any:
    """Sync convenience over `acall_request` (`asyncio.run` under the hood)."""
    return _run_sync(lambda: acall_request(base_url, api_key, ops))
