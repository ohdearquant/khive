"""Error taxonomy for the khive Python client.

Three distinct failure planes, kept as distinct exception types because a
caller retries them differently:

- transport: the daemon was unreachable or the connection broke mid-frame.
  Retryable after checking the daemon is up.
- protocol: the client and daemon disagree about the wire (version or config).
  Never retryable without upgrading one side; the message names both.
- operation: the daemon executed the request and an op inside it failed
  (invalid input, unknown id, storage refusal). Carried per-op so a batch can
  partially succeed exactly the way the server reports it.

`HttpTransport` (khive-cloud) adds a fourth plane, HTTP status: a non-2xx
response means the request never reached op dispatch at all, distinct from
an `OperationError`/`BatchError`, which mean it did dispatch and one op
inside it failed.
"""

from __future__ import annotations

import json as _json
from typing import Any


class KhiveError(Exception):
    """Base class for every error this package raises."""


class TransportError(KhiveError):
    """The daemon socket could not be reached or the frame exchange broke."""


class FrameTooLarge(TransportError):
    """A request or response frame exceeds the daemon's 8 MiB frame cap."""


class HttpError(TransportError):
    """A non-2xx HTTP response from a khive-cloud deployment.

    The API key never appears here: only status/body/url are carried.
    """

    def __init__(self, status: int, body: str, json: Any | None, url: str) -> None:
        self.status = status
        self.body = body
        self.json = json
        self.url = url
        super().__init__(f"HTTP {status} from {url}: {body}")


class AuthError(HttpError):
    """401 (missing/invalid API key) or 403 (out-of-scope operation)."""


class RateLimited(HttpError):
    """429 — the whole-request rate limit was hit."""


class BadRequest(HttpError):
    """400, 413, or another 4xx — malformed body, bad DSL, oversized payload."""


class ServerError(HttpError):
    """5xx — the server faulted."""


def raise_for_status(status: int, body_text: str, url: str) -> None:
    """Raise the matching `HttpError` subclass for a non-2xx status.

    Shared by the sync transport, the async transport, and the MCP helper
    error path so all three classify HTTP failures identically.
    """
    if 200 <= status < 300:
        return
    try:
        parsed = _json.loads(body_text)
    except ValueError:
        parsed = None
    if status in (401, 403):
        raise AuthError(status, body_text, parsed, url)
    if status == 429:
        raise RateLimited(status, body_text, parsed, url)
    if status >= 500:
        raise ServerError(status, body_text, parsed, url)
    raise BadRequest(status, body_text, parsed, url)


class ProtocolMismatch(KhiveError):
    """Client and daemon wire versions differ; upgrading one side is required."""

    def __init__(self, client_version: int, daemon_version: int, detail: str = "") -> None:
        self.client_version = client_version
        self.daemon_version = daemon_version
        super().__init__(
            f"daemon speaks protocol v{daemon_version}, client speaks v{client_version}"
            + (f": {detail}" if detail else "")
        )


class ConfigMismatch(KhiveError):
    """The daemon refused to serve under the config identity the client sent."""


class RequestRejected(KhiveError):
    """The daemon answered the frame with ok=false before dispatching ops."""


class OperationError(KhiveError):
    """A single op inside a request failed server-side."""

    def __init__(self, tool: str, message: str) -> None:
        self.tool = tool
        super().__init__(f"{tool}: {message}")


def http_op_error_code(error: Any) -> str | None:
    """Recover the `code` from a per-op error, cloud or already-stringified.

    khive-cloud embeds per-op failures as `{"code": "...", "message": "..."}`
    inside an HTTP 200 envelope; `HttpTransport` flattens that to the string
    `OpResult.error: str | None` expects (`"<code>: <message>"`) so results
    validate against the existing model unchanged. This is the inverse: given
    either shape, return the code.
    """
    if isinstance(error, dict):
        return error.get("code")
    if isinstance(error, str) and ": " in error:
        return error.split(": ", 1)[0]
    return None


class BatchError(KhiveError):
    """One or more ops in a batch failed; successes are preserved.

    `results` holds every per-op outcome in request order (dict per op);
    `failures` is the failed subset as (index, tool, error) tuples.
    """

    def __init__(self, results: list[dict], failures: list[tuple[int, str, str]]) -> None:
        self.results = results
        self.failures = failures
        summary = "; ".join(f"[{i}] {tool}: {err}" for i, tool, err in failures[:3])
        more = f" (+{len(failures) - 3} more)" if len(failures) > 3 else ""
        super().__init__(f"{len(failures)} of {len(results)} ops failed: {summary}{more}")
