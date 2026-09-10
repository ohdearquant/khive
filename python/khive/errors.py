"""Error taxonomy for the khive Python client.

Three distinct failure planes, kept as distinct exception types because a
caller retries them differently:

- transport: the daemon was unreachable or the connection broke mid-frame.
  A broken exchange can leave the domain outcome unknown; it permits no replay.
- protocol: the client and daemon disagree about the wire (version or config).
  Never retryable without upgrading one side; the message names both.
- operation: the daemon executed the request and an op inside it failed
  (invalid input, unknown id, storage refusal). Carried per-op so a batch can
  partially succeed exactly the way the server reports it.
"""

from __future__ import annotations

from .models import OpError


class KhiveError(Exception):
    """Base class for every error this package raises."""

    def __init__(self, *args: object, error_detail: OpError | None = None) -> None:
        self.error_detail = error_detail
        super().__init__(*args)


class TransportError(KhiveError):
    """The daemon socket could not be reached or the frame exchange broke."""


class FrameTooLarge(TransportError):
    """A request or response frame exceeds the daemon's 8 MiB frame cap."""


class ProtocolMismatch(KhiveError):
    """Client and daemon wire versions differ; upgrading one side is required."""

    def __init__(
        self,
        client_version: int,
        daemon_version: int,
        detail: str = "",
        error_detail: OpError | None = None,
    ) -> None:
        self.client_version = client_version
        self.daemon_version = daemon_version
        super().__init__(
            f"daemon speaks protocol v{daemon_version}, client speaks v{client_version}"
            + (f": {detail}" if detail else ""),
            error_detail=error_detail,
        )


class ConfigMismatch(KhiveError):
    """The daemon refused to serve under the config identity the client sent."""


class RequestRejected(KhiveError):
    """The daemon answered with ok=false; error_detail describes the domain outcome."""


class OperationError(KhiveError):
    """A single op inside a request failed server-side."""

    def __init__(self, tool: str, message: OpError | str) -> None:
        self.tool = tool
        self.error = message
        super().__init__(f"{tool}: {message}")


class BatchError(KhiveError):
    """One or more ops in a batch failed; successes are preserved.

    `results` holds every per-op outcome in request order (dict per op);
    `failures` is the failed subset as (index, tool, error) tuples.
    """

    def __init__(
        self, results: list[dict], failures: list[tuple[int, str, OpError | str | None]]
    ) -> None:
        self.results = results
        self.failures = failures
        summary = "; ".join(f"[{i}] {tool}: {err}" for i, tool, err in failures[:3])
        more = f" (+{len(failures) - 3} more)" if len(failures) > 3 else ""
        super().__init__(f"{len(failures)} of {len(results)} ops failed: {summary}{more}")
