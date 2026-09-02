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
import logging
from collections.abc import AsyncIterator
from contextlib import AsyncExitStack, asynccontextmanager
from typing import Any

from .errors import AuthError, KhiveError
from .transport import _check_base_url_security

_TRANSPORT_LOGGER = "mcp.client.streamable_http"


class _AcceptedTerminationFilter(logging.Filter):
    """Silence the SDK's warning for a session DELETE the server accepted.

    On session close the streamable-HTTP client sends `DELETE {base}/mcp`
    and logs `Session termination failed: <status>` for anything but 200 or
    204. khive-cloud acknowledges the DELETE with 202 Accepted, which is
    success, so that record is dropped; any non-2xx status still logs.
    """

    def filter(self, record: logging.LogRecord) -> bool:
        message = record.getMessage()
        if not message.startswith("Session termination failed: "):
            return True
        status = message.rsplit(" ", 1)[-1]
        return not (status.isdigit() and 200 <= int(status) < 300)


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
    import httpx

    root = _root_cause(exc)
    if isinstance(root, httpx.HTTPStatusError):
        status = root.response.status_code
        if status in (401, 403):
            return AuthError(status, str(root), None, url)
        return KhiveError(str(root))
    message = str(root)
    if "401" in message or "Unauthorized" in message:
        return AuthError(401, message, None, url)
    if "403" in message or "Forbidden" in message:
        return AuthError(403, message, None, url)
    return KhiveError(message)


@asynccontextmanager
async def mcp_session(
    base_url: str, api_key: str, *, allow_insecure: bool = False
) -> AsyncIterator[Any]:
    """Async context manager yielding an initialized `mcp.ClientSession`.

    Usage: ``async with mcp_session(url, key) as session: ...``

    By default a plain `http://` base URL is refused unless its host is
    loopback (`127.0.0.1`, `::1`, `localhost`) — the same guard
    `HttpTransport` applies, checked here before any client is built, since
    the `Authorization` header carrying the API key is identical either way.
    Pass `allow_insecure=True` to talk to a non-loopback host over `http://`
    anyway.

    Exception translation (transport/session failures → `khive` errors) is
    scoped to setup, before the yield: an exception raised inside the
    caller's ``async with`` body propagates unchanged instead of being
    rewritten as a generic `KhiveError` on generator exit.
    """
    _check_base_url_security(base_url, allow_insecure)
    import httpx
    from mcp import ClientSession
    from mcp.client.streamable_http import streamable_http_client

    url = base_url.rstrip("/") + "/mcp"
    headers = {"Authorization": f"ApiKey {api_key}"}
    transport_log = logging.getLogger(_TRANSPORT_LOGGER)
    termination_filter = _AcceptedTerminationFilter()
    transport_log.addFilter(termination_filter)
    stack = AsyncExitStack()
    try:
        try:
            http_client = await stack.enter_async_context(httpx.AsyncClient(headers=headers))
            transport = streamable_http_client(url, http_client=http_client)
            read, write, _get_session_id = await stack.enter_async_context(transport)
            session = await stack.enter_async_context(ClientSession(read, write))
            await session.initialize()
        except (Exception, asyncio.CancelledError) as exc:
            # A bad key (or any other setup-time transport failure) often does
            # not surface here: `session.initialize()` awaits a response that
            # never arrives, and only gets a `CancelledError` when the
            # streamable-HTTP transport's task group is torn down — the real
            # cause (e.g. an `httpx.HTTPStatusError`) is only raised by that
            # teardown. Close the stack now so that exception, if there is one,
            # is the one translated; it is strictly more informative than a
            # bare `CancelledError`.
            close_exc: BaseException | None = None
            try:
                await stack.aclose()
            except BaseException as inner:
                close_exc = inner
            if isinstance(exc, asyncio.CancelledError) and close_exc is not None:
                # The bare-cancellation case above: `exc` itself carries no
                # information, so the teardown failure IS the real cause.
                real_exc = close_exc
            else:
                # `exc` is already a typed setup failure (or a cancellation
                # that resolved on its own) — an unrelated cleanup failure
                # must not bury it, only annotate it.
                real_exc = exc
                if close_exc is not None and hasattr(real_exc, "add_note"):
                    real_exc.add_note(f"cleanup after setup failure also raised: {close_exc!r}")
            if isinstance(real_exc, KhiveError):
                raise real_exc
            raise _as_khive_error(real_exc, url) from real_exc
        try:
            yield session
        except BaseException as body_exc:
            try:
                await stack.aclose()
            except BaseException as close_exc:
                if close_exc is body_exc:
                    raise
                # Swallow a cleanup failure distinct from the exception being
                # unwound — `contextlib.suppress(Exception)` cannot do this
                # because cleanup can raise `asyncio.CancelledError` or a
                # `BaseExceptionGroup`, neither of which is an `Exception`,
                # and either would otherwise replace `body_exc` silently.
                if hasattr(body_exc, "add_note"):
                    body_exc.add_note(f"cleanup after body exception also raised: {close_exc!r}")
            raise
        else:
            await stack.aclose()
    finally:
        # The DELETE that the SDK logs about is sent by `stack.aclose()`,
        # so the filter stays until every close path above has run.
        transport_log.removeFilter(termination_filter)


async def alist_tool_names(
    base_url: str, api_key: str, *, allow_insecure: bool = False
) -> list[str]:
    """`tools/list`, returning just the tool names."""
    url = base_url.rstrip("/") + "/mcp"
    async with mcp_session(base_url, api_key, allow_insecure=allow_insecure) as session:
        try:
            result = await session.list_tools()
        except Exception as exc:
            raise _as_khive_error(exc, url) from exc
        return [tool.name for tool in result.tools]


async def acall_request(
    base_url: str, api_key: str, ops: str, *, allow_insecure: bool = False
) -> Any:
    """Call the `request` tool with the given ops DSL and parse its JSON reply."""
    url = base_url.rstrip("/") + "/mcp"
    async with mcp_session(base_url, api_key, allow_insecure=allow_insecure) as session:
        try:
            result = await session.call_tool("request", {"ops": ops})
        except Exception as exc:
            raise _as_khive_error(exc, url) from exc
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


def mcp_list_tools(base_url: str, api_key: str, *, allow_insecure: bool = False) -> list[str]:
    """Sync convenience over `alist_tool_names` (`asyncio.run` under the hood)."""
    return _run_sync(lambda: alist_tool_names(base_url, api_key, allow_insecure=allow_insecure))


def mcp_request(base_url: str, api_key: str, ops: str, *, allow_insecure: bool = False) -> Any:
    """Sync convenience over `acall_request` (`asyncio.run` under the hood)."""
    return _run_sync(
        lambda: acall_request(base_url, api_key, ops, allow_insecure=allow_insecure)
    )
