"""Transports for the khive client.

The daemon's native wire (crates/khive-runtime/src/daemon.rs) is a Unix
socket at `~/.khive/khived.sock` (override: `KHIVE_SOCKET`) speaking
one request per connection: a 4-byte big-endian length prefix followed by a
JSON-encoded `DaemonRequestFrame`, answered by one length-prefixed
JSON-encoded `DaemonResponseFrame`. Frames are capped at 8 MiB in both
directions. Admission is peer-uid: the daemon only serves connections from
its own uid, so there is no credential in the frame — `namespace` and
`actor_id` are attribution inputs, not authentication.

`Transport` is the seam a remote (HTTP) implementation plugs into:
everything above it — models, ops, the client facade — is
transport-agnostic. `SocketTransport` talks to a local daemon;
`HttpTransport` (below) talks to a khive-cloud deployment over
`POST /v1/request`.

Handshake: on first use the client sends a `metrics_only` frame (the one
request the daemon answers regardless of `config_id`) to learn the daemon's
protocol version and `served_config_id`, then adopts that config id for the
session. A pure client has no local engine config of its own, so coherence
is daemon-defined; a `version_mismatch` is a hard error naming both sides.
"""

from __future__ import annotations

import json
import os
import socket
import struct
from abc import ABC, abstractmethod
from pathlib import Path
from typing import Any, Self

from .dsl import render_dsl
from .errors import (
    ConfigMismatch,
    FrameTooLarge,
    ProtocolMismatch,
    RequestRejected,
    TransportError,
    raise_for_status,
)

PROTOCOL_VERSION = 4
MAX_FRAME_BYTES = 8 * 1024 * 1024


def default_socket_path() -> Path:
    env = os.environ.get("KHIVE_SOCKET", "")
    if env:
        return Path(env)
    return Path.home() / ".khive" / "khived.sock"


class Transport(ABC):
    """One round-trip: a request frame dict in, a response frame dict out."""

    @abstractmethod
    def round_trip(self, frame: dict[str, Any], timeout: float) -> dict[str, Any]: ...


class SocketTransport(Transport):
    def __init__(self, path: str | Path | None = None) -> None:
        self.path = Path(path) if path is not None else default_socket_path()

    def round_trip(self, frame: dict[str, Any], timeout: float) -> dict[str, Any]:
        payload = json.dumps(frame).encode("utf-8")
        if len(payload) > MAX_FRAME_BYTES:
            raise FrameTooLarge(f"request frame is {len(payload)} bytes; cap is {MAX_FRAME_BYTES}")
        try:
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as sock:
                sock.settimeout(timeout)
                sock.connect(str(self.path))
                sock.sendall(struct.pack(">I", len(payload)) + payload)
                raw = self._read_frame(sock)
        except (TimeoutError, OSError) as exc:
            raise TransportError(f"khived at {self.path}: {exc}") from exc
        try:
            return json.loads(raw.decode("utf-8"))
        except ValueError as exc:
            raise TransportError(f"undecodable response frame ({len(raw)} bytes)") from exc

    def _read_frame(self, sock: socket.socket) -> bytes:
        header = self._read_exact(sock, 4)
        (length,) = struct.unpack(">I", header)
        if length > MAX_FRAME_BYTES:
            raise FrameTooLarge(f"response frame of {length} bytes exceeds {MAX_FRAME_BYTES}")
        return self._read_exact(sock, length)

    @staticmethod
    def _read_exact(sock: socket.socket, n: int) -> bytes:
        buf = bytearray()
        while len(buf) < n:
            chunk = sock.recv(n - len(buf))
            if not chunk:
                raise TransportError(f"connection closed after {len(buf)} of {n} bytes")
            buf.extend(chunk)
        return bytes(buf)


def _cloud_config_id(base_url: str) -> str:
    return "http:" + base_url.rstrip("/")


def _render_ops_field(ops_field: str) -> str:
    """Decode the client's internal `[{"tool","args"}]` JSON string and
    render it as DSL text — `frame["ops"]` always arrives in that JSON form
    (`Session`/`ops.encode` build it for both transports), and the cloud
    parser only accepts DSL text (see the module docstring)."""
    if not ops_field:
        return ""
    try:
        parsed = json.loads(ops_field)
    except ValueError as exc:
        raise TransportError(f"malformed ops payload: {exc}") from exc
    if not parsed:
        return ""
    return render_dsl(parsed)


def _parse_json_body(response: Any) -> Any:
    try:
        return response.json()
    except ValueError as exc:
        raise TransportError(f"malformed JSON body from {response.url}: {exc}") from exc


def _parse_envelope(response: Any) -> dict[str, Any]:
    payload = _parse_json_body(response)
    if not isinstance(payload, dict) or not isinstance(payload.get("results"), list):
        raise TransportError(
            f"response from {response.url} is not a request envelope: {str(payload)[:200]}"
        )
    return payload


def _stringify_op_errors(envelope: Any) -> Any:
    """Flatten khive-cloud's `{"code","message"}` per-op error objects to a
    string, in place, so each entry still validates against
    `OpResult.error: str | None` (`client.py` is unmodified — this is the
    wire-adapter's job, same as `client._edge_from_wire`)."""
    if not isinstance(envelope, dict):
        return envelope
    for entry in envelope.get("results", []):
        if not isinstance(entry, dict):
            continue
        err = entry.get("error")
        if isinstance(err, dict):
            code = err.get("code")
            message = err.get("message", str(err))
            entry["error"] = f"{code}: {message}" if code else str(message)
    return envelope


class HttpTransport(Transport):
    """Talks to a khive-cloud deployment over `POST {base_url}/v1/request`.

    The cloud has no local engine config to hand-shake against, so a
    `metrics_only` frame is answered without a POST: `served_config_id` is
    derived deterministically from the base URL (stable across calls, so
    `Session`'s config-coherence check is trivially satisfied) and `metrics`
    is `GET /health`'s body. Every other frame carries `ops` in the client's
    internal `[{"tool", "args"}]` JSON-array form (what `Session`/`ops.encode`
    build for both transports); this one decodes it and posts
    `{"ops": render_dsl(...)}` — the cloud's `POST /v1/request` only accepts
    the request DSL as one string, not that JSON array (see `khive.dsl`).
    `config_mismatch` never occurs on this transport since the config id is
    a pure function of the URL.

    The API key is sent only as the `Authorization` header — never logged,
    never in `repr`, never folded into an error message.
    """

    def __init__(self, base_url: str, api_key: str, *, timeout: float = 30.0) -> None:
        import httpx

        self._base_url = base_url.rstrip("/")
        self._client = httpx.Client(
            base_url=self._base_url,
            headers={"Authorization": f"ApiKey {api_key}"},
            timeout=timeout,
        )

    def round_trip(self, frame: dict[str, Any], timeout: float) -> dict[str, Any]:
        import httpx

        if frame.get("metrics_only"):
            try:
                response = self._client.get("/health", timeout=timeout)
            except httpx.HTTPError as exc:
                raise TransportError(f"khive-cloud at {self._base_url}: {exc}") from exc
            raise_for_status(response.status_code, response.text, str(response.url))
            return {
                "ok": True,
                "served_config_id": _cloud_config_id(self._base_url),
                "protocol_version": PROTOCOL_VERSION,
                "metrics": _parse_json_body(response),
            }
        return self._post(_render_ops_field(frame.get("ops", "")), timeout)

    def send_dsl(self, ops: str, *, timeout: float) -> dict[str, Any]:
        """Send an already-rendered DSL ops string verbatim.

        Used by the `khive-cloud` CLI's `exec` command, whose input is DSL
        text typed by the caller — not the client's internal ops-array form
        that `round_trip` decodes and re-renders.
        """
        return self._post(ops, timeout)

    def _post(self, dsl_body: str, timeout: float) -> dict[str, Any]:
        import httpx

        try:
            response = self._client.post("/v1/request", json={"ops": dsl_body}, timeout=timeout)
        except httpx.HTTPError as exc:
            raise TransportError(f"khive-cloud at {self._base_url}: {exc}") from exc
        raise_for_status(response.status_code, response.text, str(response.url))
        return {"ok": True, "result": _stringify_op_errors(_parse_envelope(response))}

    def close(self) -> None:
        self._client.close()

    def __enter__(self) -> Self:
        return self

    def __exit__(self, *exc_info: object) -> None:
        self.close()


class AsyncHttpTransport:
    """Async twin of `HttpTransport`.

    Not a `Transport` subclass — `Transport.round_trip` is synchronous and
    `Session` drives it synchronously, so this is used directly by callers
    who are already inside an event loop rather than through `Session`.
    """

    def __init__(self, base_url: str, api_key: str, *, timeout: float = 30.0) -> None:
        import httpx

        self._base_url = base_url.rstrip("/")
        self._client = httpx.AsyncClient(
            base_url=self._base_url,
            headers={"Authorization": f"ApiKey {api_key}"},
            timeout=timeout,
        )

    async def round_trip(self, frame: dict[str, Any], timeout: float) -> dict[str, Any]:
        import httpx

        if frame.get("metrics_only"):
            try:
                response = await self._client.get("/health", timeout=timeout)
            except httpx.HTTPError as exc:
                raise TransportError(f"khive-cloud at {self._base_url}: {exc}") from exc
            raise_for_status(response.status_code, response.text, str(response.url))
            return {
                "ok": True,
                "served_config_id": _cloud_config_id(self._base_url),
                "protocol_version": PROTOCOL_VERSION,
                "metrics": _parse_json_body(response),
            }
        dsl_body = _render_ops_field(frame.get("ops", ""))
        try:
            response = await self._client.post(
                "/v1/request", json={"ops": dsl_body}, timeout=timeout
            )
        except httpx.HTTPError as exc:
            raise TransportError(f"khive-cloud at {self._base_url}: {exc}") from exc
        raise_for_status(response.status_code, response.text, str(response.url))
        return {"ok": True, "result": _stringify_op_errors(_parse_envelope(response))}

    async def aclose(self) -> None:
        await self._client.aclose()

    async def __aenter__(self) -> Self:
        return self

    async def __aexit__(self, *exc_info: object) -> None:
        await self.aclose()


class Session:
    """A configured lane to one daemon: transport + identity + adopted config.

    Performs the version/config handshake lazily on the first request and
    caches `config_id` for the connection's lifetime. If the daemon restarts
    under a different config, the next request comes back `config_mismatch`
    and the session re-handshakes once before failing.
    """

    def __init__(
        self,
        transport: Transport | None = None,
        *,
        namespace: str = "local",
        actor_id: str | None = None,
        visible_namespaces: list[str] | None = None,
        timeout: float = 30.0,
    ) -> None:
        self.transport = transport or SocketTransport()
        self.namespace = namespace
        self.actor_id = actor_id
        self.visible_namespaces = visible_namespaces or []
        self.timeout = timeout
        self._config_id: str | None = None

    # -- handshake ---------------------------------------------------------

    def handshake(self) -> str:
        response = self.transport.round_trip(
            self._base_frame() | {"metrics_only": True}, self.timeout
        )
        self._check_version(response)
        served = response.get("served_config_id")
        if not served:
            raise ConfigMismatch(
                "daemon did not report a config id; it predates this client's protocol"
            )
        self._config_id = served
        return served

    def metrics(self) -> dict[str, Any]:
        response = self.transport.round_trip(
            self._base_frame() | {"metrics_only": True}, self.timeout
        )
        self._check_version(response)
        return response.get("metrics") or {}

    # -- request path ------------------------------------------------------

    def request(self, ops_json: str, *, timeout: float | None = None) -> list[dict[str, Any]]:
        """Send one ops payload; return the per-op result list."""
        if self._config_id is None:
            self.handshake()
        frame = self._base_frame() | {
            "ops": ops_json,
            "config_id": self._config_id,
        }
        response = self.transport.round_trip(frame, timeout or self.timeout)
        self._check_version(response)
        if response.get("config_mismatch"):
            # One re-handshake: the daemon restarted under a new config.
            self.handshake()
            frame["config_id"] = self._config_id
            response = self.transport.round_trip(frame, timeout or self.timeout)
            self._check_version(response)
            if response.get("config_mismatch"):
                raise ConfigMismatch(str(response.get("error")))
        if not response.get("ok"):
            raise RequestRejected(str(response.get("error")))
        raw = response.get("result")
        parsed = json.loads(raw) if isinstance(raw, str) else raw
        if isinstance(parsed, dict) and "results" in parsed:
            return parsed["results"]
        raise TransportError(f"response result missing 'results': {str(parsed)[:200]}")

    def _base_frame(self) -> dict[str, Any]:
        return {
            "ops": "",
            # Verbose passes canonical JSON through unchanged: full ISO-8601
            # timestamps, no humanized fields ("0s ago"), no redundancy
            # pre-pass. The compact/agent renderings are for humans and
            # agents reading text; a typed client needs the machine contract.
            "presentation": "verbose",
            "format": "json",
            "namespace": self.namespace,
            "actor_id": self.actor_id,
            "visible_namespaces": self.visible_namespaces,
            "config_id": self._config_id or "",
            "protocol_version": PROTOCOL_VERSION,
            "from_wire": False,
        }

    @staticmethod
    def _check_version(response: dict[str, Any]) -> None:
        if response.get("version_mismatch"):
            raise ProtocolMismatch(
                PROTOCOL_VERSION,
                int(response.get("daemon_protocol_version") or 0),
                str(response.get("error") or ""),
            )
