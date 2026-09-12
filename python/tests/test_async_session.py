"""AsyncSession over the local daemon socket: framing, handshake, and refusals.

Two kinds of coverage here, because the change has two halves.

`AsyncSocketTransport` is new code with no sync twin to lean on, so it is
exercised against a real `asyncio` unix socket server speaking the daemon's
framing. A stub cannot cover length prefixes, partial reads, or the cap.

`AsyncSession` shares every classification decision with `Session` through
`_SessionCore`. The tests that matter there are the ones that would fail if
someone later gave the awaited path its own copy: the two drivers must reach
the same verdict on the same response bytes.

No async pytest plugin is a dev dependency, so each test drives its coroutine
with a bare `asyncio.run`, the same pattern as `test_async_http_transport.py`.
"""

from __future__ import annotations

import asyncio
import json
import struct
from copy import deepcopy

import pytest

from khive import AsyncSession, AsyncSocketTransport, Session
from khive.errors import ConfigMismatch, FrameTooLarge, ProtocolMismatch, RequestRejected
from khive.transport import MAX_FRAME_BYTES, PROTOCOL_VERSION


def _run(coro):
    return asyncio.run(coro)


RESULTS = {"results": [{"ok": True, "tool": "stats"}]}


class AsyncDaemonStub:
    """The awaited shape of `test_session_plan.py`'s DaemonStub."""

    def __init__(self, *, version=PROTOCOL_VERSION, mismatches=0, rejection=None):
        self.version = version
        self.mismatches = mismatches
        self.rejection = rejection
        self.frames = []
        self.timeouts = []

    async def round_trip(self, frame, timeout):
        self.frames.append(deepcopy(frame))
        self.timeouts.append(timeout)
        if frame["protocol_version"] != self.version:
            return {
                "ok": False,
                "version_mismatch": True,
                "daemon_protocol_version": self.version,
                "error": "protocol version mismatch",
            }
        if frame.get("metrics_only"):
            return {"ok": True, "served_config_id": "catalog-config", "metrics": {"status": "ok"}}
        if self.mismatches:
            self.mismatches -= 1
            return {"ok": False, "config_mismatch": True, "error": "config changed"}
        if self.rejection is not None:
            return {"ok": False, "error": self.rejection}
        return {"ok": True, "served_config_id": "catalog-config", "result": json.dumps(RESULTS)}


class SyncMirror:
    """`AsyncDaemonStub`'s answers, delivered synchronously.

    Same responses, same order, so a divergence between the two sessions can
    only come from the sessions themselves.
    """

    def __init__(self, **kwargs):
        self._inner = AsyncDaemonStub(**kwargs)

    def round_trip(self, frame, timeout):
        return asyncio.run(self._inner.round_trip(frame, timeout))


# -- the shared core, asserted structurally --------------------------------


def test_both_sessions_share_one_copy_of_every_classification():
    # Identity, not equality: if either class grows its own override these stop
    # being the same function and this fails, which is the whole point of the
    # split. A behavioural test alone would pass a duplicated-but-identical copy
    # and go on passing until the copies drifted.
    assert Session._decode_results is AsyncSession._decode_results
    assert Session._request_frame is AsyncSession._request_frame
    assert Session._base_frame is AsyncSession._base_frame
    assert Session._check_version is AsyncSession._check_version


@pytest.mark.parametrize(
    "kwargs,expected",
    [
        ({"rejection": "refused by policy"}, RequestRejected),
        ({"mismatches": 5}, ConfigMismatch),
        ({"version": PROTOCOL_VERSION + 1}, ProtocolMismatch),
    ],
)
def test_both_drivers_raise_the_same_class_on_the_same_response(kwargs, expected):
    with pytest.raises(expected):
        Session(SyncMirror(**kwargs)).request("stats()")

    async def _inner():
        await AsyncSession(AsyncDaemonStub(**kwargs)).arequest("stats()")

    with pytest.raises(expected):
        _run(_inner())


def test_one_rehandshake_then_the_results(  # a mismatch that clears is not an error
):
    stub = AsyncDaemonStub(mismatches=1)

    async def _inner():
        session = AsyncSession(stub, actor_id="test-client", namespace="project")
        return await session.arequest("stats()")

    assert _run(_inner()) == RESULTS["results"]
    # handshake, refused request, re-handshake, accepted request: exactly one retry.
    assert [bool(f.get("metrics_only")) for f in stub.frames] == [True, False, True, False]


def test_ametrics_reads_without_adopting_a_config():
    stub = AsyncDaemonStub()

    async def _inner():
        return await AsyncSession(stub).ametrics()

    assert _run(_inner()) == {"status": "ok"}


def test_async_context_manager_closes_without_error():
    async def _inner():
        async with AsyncSession(AsyncDaemonStub()) as session:
            return await session.arequest("stats()")

    assert _run(_inner()) == RESULTS["results"]


# -- the new transport, against a real socket -------------------------------


class FakeDaemon:
    """One length-prefixed request per connection, exactly like khived."""

    def __init__(self, path, responder):
        self.path = str(path)
        self.responder = responder
        self.requests = []

    async def __aenter__(self):
        self._server = await asyncio.start_unix_server(self._serve, self.path)
        return self

    async def __aexit__(self, *exc_info):
        self._server.close()
        await self._server.wait_closed()

    async def _serve(self, reader, writer):
        header = await reader.readexactly(4)
        (length,) = struct.unpack(">I", header)
        body = await reader.readexactly(length)
        frame = json.loads(body.decode("utf-8"))
        self.requests.append(frame)
        payload = json.dumps(self.responder(frame)).encode("utf-8")
        writer.write(struct.pack(">I", len(payload)) + payload)
        await writer.drain()
        writer.close()


def test_transport_round_trips_over_a_real_unix_socket(tmp_path):
    sock = tmp_path / "khived.sock"

    def responder(frame):
        return {"ok": True, "served_config_id": "socket-config", "echoed": frame["ops"]}

    async def _inner():
        async with FakeDaemon(sock, responder) as daemon:
            transport = AsyncSocketTransport(sock)
            response = await transport.round_trip(
                {"ops": "stats()", "protocol_version": PROTOCOL_VERSION}, timeout=5.0
            )
            return response, daemon.requests

    response, requests = _run(_inner())
    assert response["echoed"] == "stats()"
    assert requests[0]["ops"] == "stats()"


def test_session_drives_the_real_socket_end_to_end(tmp_path):
    sock = tmp_path / "khived.sock"

    def responder(frame):
        if frame.get("metrics_only"):
            return {"ok": True, "served_config_id": "socket-config"}
        return {"ok": True, "served_config_id": "socket-config", "result": json.dumps(RESULTS)}

    async def _inner():
        async with FakeDaemon(sock, responder):
            session = AsyncSession(AsyncSocketTransport(sock))
            return await session.arequest("stats()")

    assert _run(_inner()) == RESULTS["results"]


def test_oversized_request_frame_is_refused_before_any_connection(tmp_path):
    # No server is started: the cap must reject before the socket is touched, so
    # a connection error here would mean the check runs too late.
    transport = AsyncSocketTransport(tmp_path / "absent.sock")

    async def _inner():
        await transport.round_trip({"ops": "x" * (MAX_FRAME_BYTES + 1)}, timeout=5.0)

    with pytest.raises(FrameTooLarge):
        _run(_inner())
