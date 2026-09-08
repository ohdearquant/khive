"""Transports for the khive client.

The daemon's native wire (crates/khive-runtime/src/daemon.rs) is a Unix
socket at `~/.khive/khived.sock` (override: `KHIVE_SOCKET`) speaking
one request per connection: a 4-byte big-endian length prefix followed by a
JSON-encoded `DaemonRequestFrame`, answered by one length-prefixed
JSON-encoded `DaemonResponseFrame`. Frames are capped at 8 MiB in both
directions. Admission is peer-uid: the daemon only serves connections from
its own uid, so there is no credential in the frame — `namespace` and
`actor_id` are attribution inputs, not authentication.

`Transport` is the seam a remote (HTTP) implementation will plug into
later: everything above it — models, ops, the client facade — is
transport-agnostic. Only `SocketTransport` exists today.

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
from typing import Any

from .envelope import (
    _decode_json_text,
    _envelope_from_payload,
    _plan_from_payload,
    _validate_envelope_results,
    _validate_frame_error_detail,
)
from .errors import (
    ConfigMismatch,
    FrameTooLarge,
    ProtocolMismatch,
    RequestRejected,
    TransportError,
)
from .models import OpError

PROTOCOL_VERSION = 5
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
        return self._handshake(self._base_frame())

    def _handshake(self, frame: dict[str, Any]) -> str:
        response = self.transport.round_trip(frame | {"metrics_only": True}, self.timeout)
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
                raise ConfigMismatch(
                    str(response.get("error")),
                    error_detail=_validate_frame_error_detail(response, "daemon"),
                )
        if not response.get("ok"):
            raise RequestRejected(
                str(response.get("error")),
                error_detail=_validate_frame_error_detail(response, "daemon"),
            )
        raw = response.get("result")
        parsed = _decode_json_text(raw, "daemon") if isinstance(raw, str) else raw
        envelope = _envelope_from_payload(parsed, "daemon")
        return _validate_envelope_results(envelope, "daemon")["results"]

    def plan(self, ops: str, *, timeout: float | None = None) -> dict[str, Any]:
        """Parse ops without dispatch; return a plan, including parsed=false errors.

        A successful parse reports syntax and catalog information, not permission
        to execute the operations or evidence that their references will resolve.
        """
        if self._config_id is None:
            self._handshake(self._plan_frame())
        frame = self._plan_frame() | {"ops": ops, "plan": True}
        response = self.transport.round_trip(frame, timeout or self.timeout)
        self._check_version(response)
        if response.get("config_mismatch"):
            self._handshake(self._plan_frame())
            frame["config_id"] = self._config_id
            response = self.transport.round_trip(frame, timeout or self.timeout)
            self._check_version(response)
            if response.get("config_mismatch"):
                raise ConfigMismatch(
                    str(response.get("error")),
                    error_detail=_validate_frame_error_detail(response, "daemon"),
                )
        if not response.get("ok"):
            raise RequestRejected(
                str(response.get("error")),
                error_detail=_validate_frame_error_detail(response, "daemon"),
            )
        raw = response.get("result")
        parsed = _decode_json_text(raw, "daemon") if isinstance(raw, str) else raw
        return _plan_from_payload(parsed, "daemon")

    def _plan_frame(self) -> dict[str, Any]:
        return {
            "ops": "",
            # Required by the frame codec; the plan path never resolves identity.
            "namespace": "",
            "config_id": self._config_id or "",
            "protocol_version": PROTOCOL_VERSION,
        }

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
            message = str(response.get("error") or "")
            detail = _validate_frame_error_detail(response, "daemon")
            fields = detail.model_dump(exclude_unset=True) if detail is not None else {}
            fields.pop("domain_result", None)
            fields.update(
                kind="protocol",
                code="version_mismatch",
                message=message,
                domain_disposition="unknown",
            )
            raise ProtocolMismatch(
                PROTOCOL_VERSION,
                int(response.get("daemon_protocol_version") or 0),
                message,
                OpError.model_validate(fields),
            )
