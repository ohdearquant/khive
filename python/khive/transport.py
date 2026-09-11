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
from urllib.parse import urlsplit

from pydantic import ValidationError

from .dsl import render_dsl
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
    OperationError,
    ProtocolMismatch,
    RequestRejected,
    TransportError,
    raise_for_status,
)
from .models import OpResult
from .models import OpError, RecallOutcome
from .ops import encode, op

PROTOCOL_VERSION = 6
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


_LOOPBACK_HOSTS = {"127.0.0.1", "::1", "localhost"}


def _check_base_url_security(base_url: str, allow_insecure: bool) -> None:
    """Refuse a plain `http://` base URL that is not loopback.

    An API key sent in an `Authorization` header over plain HTTP to a
    non-loopback host is a credential leak to every hop in between. Loopback
    (a local dev server, or the offline fakes this package's own tests run
    against) is exempt; anything else needs `https://` or an explicit
    `allow_insecure=True`.
    """
    parsed = urlsplit(base_url)
    if parsed.scheme != "http" or allow_insecure:
        return
    host = (parsed.hostname or "").lower()
    if host in _LOOPBACK_HOSTS:
        return
    raise ValueError(
        f"refusing a {parsed.scheme}:// base URL with non-loopback host {host!r}; "
        "pass allow_insecure=True to allow this"
    )


def _cloud_config_id(base_url: str) -> str:
    return "http:" + base_url.rstrip("/")


def _render_ops_field(ops_field: str) -> str:
    """Turn `frame["ops"]` into the DSL text the cloud parser accepts.

    `frame["ops"]` is always a JSON document (`Session`/`ops.encode` build
    it for both transports): the client's internal `[{"tool","args"}]`
    array, which is rendered; or, when a caller handed `raw()` DSL text
    instead of op dicts, a JSON string (or a list mixing DSL strings and op
    dicts), whose DSL text passes through untouched."""
    if not ops_field:
        return ""
    try:
        parsed = json.loads(ops_field)
    except ValueError as exc:
        raise TransportError(f"malformed ops payload: {exc}") from exc
    if not parsed:
        return ""
    if isinstance(parsed, dict):
        parsed = [parsed]
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


def _is_minimal_aborted_entry(entry: Any) -> bool:
    """Whether `entry` is the cloud's minimal aborted-chain-entry shape:
    `{"ok": false, "aborted": true}`, with no `tool` — an op that was never
    dispatched because an earlier op in the same chain failed. `OpResult`
    requires `tool`, so this shape needs its own admission rule rather than
    going through `OpResult.model_validate` like an ordinary entry."""
    return (
        isinstance(entry, dict)
        and entry.get("ok") is False
        and entry.get("aborted") is True
        and "tool" not in entry
    )


def _validate_envelope_results(envelope: dict[str, Any], url: str) -> dict[str, Any]:
    """Reject an envelope whose result entries do not match `OpResult`.

    Runs after `_stringify_op_errors`, so a per-op error is already the
    plain string `OpResult.error` expects, not khive-cloud's
    `{"code","message"}` object — a top-level-only check would otherwise let
    e.g. `{"results": [42]}` or an entry missing `ok`/`tool` reach the caller
    as a successful response.

    A minimal aborted entry (see `_is_minimal_aborted_entry`) is admitted
    without going through `OpResult`, but normalized in place to carry
    `tool: ""` first — `models.py` stays untouched, so this is the seam that
    keeps every entry (aborted or not) satisfying `OpResult.tool: str`
    exactly as the socket transport's daemon-native aborted entries already
    do, giving both transports the same caller-visible object.
    """
    for index, entry in enumerate(envelope["results"]):
        if _is_minimal_aborted_entry(entry):
            entry["tool"] = ""
            continue
        try:
            OpResult.model_validate(entry)
        except ValidationError as exc:
            raise TransportError(
                f"response from {url} has a malformed result at index {index}: {exc}"
            ) from exc
    return envelope


def _stringify_op_errors(envelope: Any, url: str) -> Any:
    """Flatten khive-cloud's `{"code","message"}` per-op error objects to a
    string, in place, so each entry still validates against
    `OpResult.error: str | None` (`client.py` is unmodified — this is the
    wire-adapter's job, same as `client._edge_from_wire`).

    Validates the error object's shape first: `code`, when present, and
    `message` must both be strings — anything else is a malformed cloud
    entry, not a value to flatten and pass along.
    """
    if not isinstance(envelope, dict):
        return envelope
    for index, entry in enumerate(envelope.get("results", [])):
        if not isinstance(entry, dict):
            continue
        err = entry.get("error")
        if not isinstance(err, dict):
            continue
        code = err.get("code")
        if code is not None and not isinstance(code, str):
            raise TransportError(
                f"response from {url} has a malformed error object at index {index}: "
                f"'code' must be a string, got {type(code).__name__}"
            )
        message = err.get("message")
        if not isinstance(message, str):
            raise TransportError(
                f"response from {url} has a malformed error object at index {index}: "
                f"'message' must be a string, got {type(message).__name__}"
            )
        entry["error"] = f"{code}: {message}" if code else message
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

    `frame`'s identity fields (`Session._base_frame`'s `namespace`,
    `actor_id`, `visible_namespaces`) are never put on the wire: khive-cloud
    resolves the principal from the API key alone, so those fields only
    matter to `SocketTransport`'s local daemon.

    By default a plain `http://` base URL is refused unless its host is
    loopback (`127.0.0.1`, `::1`, `localhost`) — an API key sent over plain
    HTTP to anything else leaks to every hop in between. Pass
    `allow_insecure=True` to talk to a non-loopback host over `http://`
    anyway.
    """

    def __init__(
        self,
        base_url: str,
        api_key: str,
        *,
        timeout: float = 30.0,
        allow_insecure: bool = False,
    ) -> None:
        import httpx

        _check_base_url_security(base_url, allow_insecure)
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

        For callers that already hold DSL text (a script, a notebook, a
        REPL). `round_trip` is the other path: it decodes the client's
        internal ops-array form and renders it.
        """
        return self._post(ops, timeout)

    def _post(self, dsl_body: str, timeout: float) -> dict[str, Any]:
        import httpx

        try:
            response = self._client.post("/v1/request", json={"ops": dsl_body}, timeout=timeout)
        except httpx.HTTPError as exc:
            raise TransportError(f"khive-cloud at {self._base_url}: {exc}") from exc
        raise_for_status(response.status_code, response.text, str(response.url))
        envelope = _stringify_op_errors(_parse_envelope(response), str(response.url))
        _validate_envelope_results(envelope, str(response.url))
        return {"ok": True, "result": envelope}

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

    `frame`'s identity fields (`Session._base_frame`'s `namespace`,
    `actor_id`, `visible_namespaces`) are never put on the wire: khive-cloud
    resolves the principal from the API key alone, so those fields only
    matter to `SocketTransport`'s local daemon.

    By default a plain `http://` base URL is refused unless its host is
    loopback (`127.0.0.1`, `::1`, `localhost`); pass `allow_insecure=True`
    to talk to a non-loopback host over `http://` anyway.
    """

    def __init__(
        self,
        base_url: str,
        api_key: str,
        *,
        timeout: float = 30.0,
        allow_insecure: bool = False,
    ) -> None:
        import httpx

        _check_base_url_security(base_url, allow_insecure)
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
        envelope = _stringify_op_errors(_parse_envelope(response), str(response.url))
        _validate_envelope_results(envelope, str(response.url))
        return {"ok": True, "result": envelope}

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
        namespace: str | None = None,
        actor_id: str | None = None,
        visible_namespaces: list[str] | None = None,
        timeout: float = 30.0,
    ) -> None:
        """`namespace` is this session's DEFAULT namespace for every op it sends.

        It used to default to `"local"` and reach only the request frame, which
        the registry does not read as an op argument, so a session constructed
        with a namespace sent every op unscoped and silently answered about the
        caller's own namespace instead. The value now falls through to each op
        that does not name one of its own, which is what the argument reads as.
        An explicit per-op `namespace=` still wins. `None` means "send no
        namespace argument", the behaviour of a session that names none.
        """
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

    def remember(
        self,
        content: str,
        *,
        key: str | None = None,
        memory_type: str | None = None,
        salience: float | None = None,
        decay_factor: float | None = None,
        source_id: str | None = None,
        tags: list[str] | None = None,
        embedding_model: str | None = None,
        namespace: str | None = None,
        timeout: float | None = None,
    ) -> dict[str, Any]:
        """Return one raw memory.remember outcome, including unchanged per-op errors.

        Recovery remains caller-controlled: this method does not infer a write
        namespace or retry an operation error, including a key conflict.
        """
        results = self.request(
            encode(
                [
                    op(
                        "memory.remember",
                        content=content,
                        key=key,
                        memory_type=memory_type,
                        salience=salience,
                        decay_factor=decay_factor,
                        source_id=source_id,
                        tags=tags,
                        embedding_model=embedding_model,
                        namespace=self._op_namespace(namespace),
                    )
                ]
            ),
            timeout=timeout,
        )
        if len(results) != 1 or results[0]["tool"] != "memory.remember":
            raise TransportError("response from daemon is not a single memory.remember result")
        return results[0]

    def send(
        self,
        to: str,
        content: str,
        *,
        idempotency_key: str | None = None,
        subject: str | None = None,
        thread_id: str | None = None,
        tags: list[str] | None = None,
        self_send: bool | None = None,
        namespace: str | None = None,
        timeout: float | None = None,
    ) -> dict[str, Any]:
        """Return one raw comm.send outcome; operation recovery is caller-controlled.

        Reuse the same key, actor, namespace and request after a lost response.
        Successful replay returns the original pair IDs; conflicts and uncertain
        outcomes remain unchanged per-operation errors, without automatic retry.
        """
        results = self.request(
            encode(
                [
                    op(
                        "comm.send",
                        to=to,
                        content=content,
                        idempotency_key=idempotency_key,
                        subject=subject,
                        thread_id=thread_id,
                        tags=tags,
                        self_send=self_send,
                        namespace=self._op_namespace(namespace),
                    )
                ]
            ),
            timeout=timeout,
        )
        if len(results) != 1 or results[0]["tool"] != "comm.send":
            raise TransportError("response from daemon is not a single comm.send result")
        return results[0]

    def reply(
        self,
        id: str,
        content: str,
        *,
        idempotency_key: str | None = None,
        tags: list[str] | None = None,
        namespace: str | None = None,
        timeout: float | None = None,
    ) -> dict[str, Any]:
        """Return one raw comm.reply outcome without retrying operation errors.

        A key identifies the resolved original message as well as the reply
        payload. Successful replay does not mark the original read again.
        """
        results = self.request(
            encode(
                [
                    op(
                        "comm.reply",
                        id=id,
                        content=content,
                        idempotency_key=idempotency_key,
                        tags=tags,
                        namespace=self._op_namespace(namespace),
                    )
                ]
            ),
            timeout=timeout,
        )
        if len(results) != 1 or results[0]["tool"] != "comm.reply":
            raise TransportError("response from daemon is not a single comm.reply result")
        return results[0]

    def recall(
        self,
        query: str,
        *,
        limit: int | None = None,
        top_k: int | None = None,
        min_score: float | None = None,
        score_floor: float | None = None,
        min_salience: float | None = None,
        memory_type: str | None = None,
        created_after: str | None = None,
        created_before: str | None = None,
        tags: list[str] | None = None,
        tag_mode: str | None = None,
        exclude_tags: list[str] | None = None,
        include_source_id: bool | None = None,
        full_content: bool | None = None,
        profile_id: str | None = None,
        embedding_model: str | None = None,
        namespace: str | None = None,
        timeout: float | None = None,
    ) -> RecallOutcome:
        """Run one memory.recall and return its typed outcome.

        The outcome keeps the server's response class (clean, degraded,
        budget-capped, or both, empty or not) instead of flattening to rows;
        see `RecallOutcome`. A failed op raises `OperationError` with the
        server's error object unchanged.
        """
        results = self.request(
            encode(
                [
                    op(
                        "memory.recall",
                        query=query,
                        limit=limit,
                        top_k=top_k,
                        min_score=min_score,
                        score_floor=score_floor,
                        min_salience=min_salience,
                        memory_type=memory_type,
                        created_after=created_after,
                        created_before=created_before,
                        tags=tags,
                        tag_mode=tag_mode,
                        exclude_tags=exclude_tags,
                        include_source_id=include_source_id,
                        full_content=full_content,
                        profile_id=profile_id,
                        embedding_model=embedding_model,
                        namespace=self._op_namespace(namespace),
                    )
                ]
            ),
            timeout=timeout,
        )
        if len(results) != 1 or results[0]["tool"] != "memory.recall":
            raise TransportError("response from daemon is not a single memory.recall result")
        entry = results[0]
        if not entry.get("ok"):
            raise OperationError("memory.recall", entry.get("error") or "unknown error")
        try:
            return RecallOutcome.from_result(entry.get("result"))
        except (TypeError, ValueError) as exc:
            raise TransportError(f"response from daemon: {exc}") from exc

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

    def _op_namespace(self, namespace: str | None) -> str | None:
        """Resolve one op's namespace: the op's own if it named one, else the
        session's default, else None so no argument is sent at all."""
        return namespace if namespace is not None else self.namespace

    def _base_frame(self) -> dict[str, Any]:
        return {
            "ops": "",
            # Verbose passes canonical JSON through unchanged: full ISO-8601
            # timestamps, no humanized fields ("0s ago"), no redundancy
            # pre-pass. The compact/agent renderings are for humans and
            # agents reading text; a typed client needs the machine contract.
            "presentation": "verbose",
            "format": "json",
            # The frame's namespace is an identity field, not a scope: it has
            # always been a string here, so an unset session keeps sending
            # "local" and the wire contract is unchanged by the default move.
            "namespace": self.namespace or "local",
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
