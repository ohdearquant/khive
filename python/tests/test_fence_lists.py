"""Ordered fences survive sequential daemon owners of one scratch store.

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
def fence_daemon(scratch, label):
    binary = os.environ.get("KKERNEL")
    assert binary, "set KKERNEL to this checkout's freshly built binary"
    root = Path(scratch["root"])
    socket = root / f"{label}.sock"
    database = root / "fences.db"
    env = os.environ.copy()
    env.update(KHIVE_SOCKET=str(socket), KHIVE_PID=str(root / f"{label}.pid"),
               KHIVE_LOCK=str(root / f"{label}.lock"),
               KHIVE_RECOVERER_LOCK=str(root / f"{label}.recoverer.lock"))
    command = [binary, "mcp", "--daemon", "--config", str(root / "khive.toml"),
               "--db", str(database)]
    stderr_path = root / f"{label}.stderr"
    with stderr_path.open("wb") as stderr:
        process = subprocess.Popen(command, cwd=root, env=env,
                                   stdout=subprocess.DEVNULL, stderr=stderr)
        try:
            client = Session(SocketTransport(socket), actor_id="fence-test", timeout=5)
            deadline = time.monotonic() + 30
            last = None
            while time.monotonic() < deadline:
                if process.poll() is not None:
                    pytest.fail(f"{label} daemon exited {process.returncode}: "
                                f"{stderr_path.read_text()}")
                if socket.exists():
                    try:
                        one(client, "stats")
                        break
                    except Exception as error:
                        last = error
                time.sleep(0.1)
            else:
                pytest.fail(f"{label} daemon not ready: {last}")
            assert process.pid != os.getpid()
            yield client, process.pid
        finally:
            if process.poll() is None:
                process.send_signal(signal.SIGTERM)
            try:
                process.wait(timeout=15)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()


def assert_second_boot_refused(scratch, holder_pid):
    """A competing HOME/socket cannot serve the same opened database."""
    binary = os.environ.get("KKERNEL")
    assert binary, "set KKERNEL to this checkout's freshly built binary"
    root = Path(scratch["root"])
    env = os.environ.copy()
    env.update(KHIVE_SOCKET=str(root / "fence-contender.sock"),
               KHIVE_PID=str(root / "fence-contender.pid"),
               KHIVE_LOCK=str(root / "fence-contender.lock"),
               KHIVE_RECOVERER_LOCK=str(root / "fence-contender.recoverer.lock"))
    contender = subprocess.run(
        [binary, "mcp", "--daemon", "--config", str(root / "khive.toml"),
         "--db", str(root / "fences.db")],
        cwd=root, env=env, capture_output=True, text=True, timeout=15,
    )
    assert contender.returncode != 0, "a second daemon must not serve the held store"
    assert f"pid {holder_pid}" in contender.stderr, contender.stderr
    assert str(root / "fences.db") in contender.stderr, contender.stderr


def test_ordered_fences_two_daemons_stale_and_missing(scratch_daemon):
    prefix = f"fences/{uuid.uuid4()}"
    fences = [{"key": f"{prefix}/{i}", "kind": "head", "expected_version": 1}
              for i in range(2)]
    with fence_daemon(scratch_daemon, "fence-first") as (first, holder_pid):
        leases = [one(first, "create", kind="head", key=f"{prefix}/{i}", content="{}")
                  for i in range(2)]
        target = one(first, "create", kind="head", content="{}")
        assert_second_boot_refused(scratch_daemon, holder_pid)

    # The second owner reads the first owner's durable rows after shutdown;
    # concurrent daemons on this database are refused by the store claim.
    with fence_daemon(scratch_daemon, "fence-second") as (second, _):
        one(second, "stream.append", stream=prefix, record="first", fence=fences)
        one(second, "update", id=target["id"], content='{"ok":true}', fence=fences[:1])
        target = one(second, "get", id=target["id"])
        assert target["version"] == 2
        renewed = one(second, "update", id=leases[1]["id"], expected_version=1,
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
            after = one(second, "get", id=original["id"])
            assert (after["version"], after["content"]) == (original["version"], original["content"])
