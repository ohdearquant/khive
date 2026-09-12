"""Ordered fences across two isolated daemon processes sharing one scratch store.

Set KKERNEL to the binary built from this checkout. No production stores are used.
"""
from __future__ import annotations

import contextlib
import os
from pathlib import Path
import signal
import subprocess
import time
import uuid

import pytest

from khive import op
from khive.ops import encode
from khive.transport import Session, SocketTransport


def request(session, verb, **args):
    return session.request(encode([op(verb, **args)]))[0]


def one(session, verb, **args):
    response = request(session, verb, **args)
    assert response["ok"], response
    return response["result"]


@contextlib.contextmanager
def second_daemon(scratch):
    binary = os.environ.get("KKERNEL")
    assert binary, "set KKERNEL to this checkout's freshly built binary"
    root = Path(scratch["root"])
    socket = root / "peer.sock"
    env = os.environ.copy()
    env.update(KHIVE_SOCKET=str(socket), KHIVE_PID=str(root / "peer.pid"),
               KHIVE_LOCK=str(root / "peer.lock"),
               KHIVE_RECOVERER_LOCK=str(root / "peer.recoverer.lock"))
    command = [binary, "mcp", "--daemon", "--config", str(root / "khive.toml"),
               "--db", str(root / "scratch.db")]
    with (root / "peer.stderr").open("wb") as stderr:
        process = subprocess.Popen(command, cwd=root, env=env,
                                   stdout=subprocess.DEVNULL, stderr=stderr)
        try:
            client = Session(SocketTransport(socket), actor_id="fence-test", timeout=5)
            deadline = time.monotonic() + 30
            last = None
            while time.monotonic() < deadline:
                if process.poll() is not None:
                    pytest.fail(f"second daemon exited {process.returncode}: "
                                f"{(root / 'peer.stderr').read_text()}")
                if socket.exists():
                    try:
                        one(client, "stats")
                        break
                    except Exception as error:
                        last = error
                time.sleep(0.1)
            else:
                pytest.fail(f"second daemon not ready: {last}")
            assert process.pid != os.getpid()
            yield client
        finally:
            if process.poll() is None:
                process.send_signal(signal.SIGTERM)
            try:
                process.wait(timeout=15)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()


def test_ordered_fences_two_daemons_stale_and_missing(scratch_daemon):
    first = Session(SocketTransport(scratch_daemon["socket"]), actor_id="fence-test")
    prefix = f"fences/{uuid.uuid4()}"
    leases = [one(first, "create", kind="head", key=f"{prefix}/{i}", content="{}")
              for i in range(2)]
    fences = [{"key": f"{prefix}/{i}", "kind": "head", "expected_version": 1}
              for i in range(2)]
    target = one(first, "create", kind="head", content="{}")
    with second_daemon(scratch_daemon) as second:
        one(second, "stream.append", stream=prefix, record="first", fence=fences)
        one(second, "update", id=target["id"], content='{"ok":true}', fence=fences[:1])
        target = one(second, "get", id=target["id"])
        assert target["version"] == 2
        renewed = one(first, "update", id=leases[1]["id"], expected_version=1,
                      content='{"renewed":true}')
        assert renewed["version"] == 2
        for atomic in [True, False]:
            stream = f"{prefix}/batch/{atomic}"
            response = request(second, "stream.batch", atomic=atomic, ops=[
                {"op": "append", "stream": stream, "record": "before"},
                {"op": "append", "stream": stream, "record": "refused", "fence": fences},
                {"op": "append", "stream": stream, "record": "after"},
            ])
            expected = {"reason": "fence_conflict", "key": fences[1]["key"],
                        "expected_version": "1", "current_version": "2", "index": "1"}
            if atomic:
                expected["member"] = "1"
                assert not response["ok"], response
                error = response["error"]
            else:
                assert response["ok"], response
                result = response["result"]
                assert result["committed"] is True
                assert result["results"][0]["seq"] == 1
                assert result["results"][2]["seq"] == 2
                error = result["results"][1]
            assert error["details"] == expected
            assert error["domain_disposition"] == "not_committed"
            assert one(second, "stream.stat", stream=stream)["head_seq"] == (0 if atomic else 2)
        for key, current in [(fences[1]["key"], "2"), (f"{prefix}/missing", None)]:
            stale = [fences[0], {**fences[1], "key": key}]
            for verb, args in [
                ("update", {"id": target["id"], "content": '{"bad":true}'}),
                ("create", {"kind": "head", "content": "{}"}),
                ("stream.append", {"stream": prefix, "record": "bad"}),
            ]:
                response = request(second, verb, **args, fence=stale)
                assert not response["ok"], response
                error = response["error"]
                expected = {"reason": "fence_conflict", "key": key,
                            "expected_version": "1", "index": "1"}
                if current is not None:
                    expected["current_version"] = current
                assert error["details"] == expected
                assert error["domain_disposition"] == "not_committed"
            assert one(second, "stream.stat", stream=prefix)["head_seq"] == 1
            after = one(second, "get", id=target["id"])
            assert (after["version"], after["content"]) == (2, target["content"])
        for original in [leases[0], renewed]:
            after = one(first, "get", id=original["id"])
            assert (after["version"], after["content"]) == (original["version"], original["content"])
