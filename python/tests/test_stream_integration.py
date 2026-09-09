"""Ordered streams through two OS clients, the Python client, and CLI exec.

Acceptance runs must set KKERNEL to the binary built from this checkout and
must report zero skips. The scratch_daemon fixture owns all stores and sockets.
"""
from __future__ import annotations

import json
import os
import subprocess
import sys
import uuid

from khive.ops import encode, op
from khive.transport import Session, SocketTransport


def session(scratch_daemon):
    return Session(SocketTransport(scratch_daemon["socket"]))


def one(client, verb, **args):
    result = client.request(encode([op(verb, **args)]))[0]
    assert result["ok"], result
    return result["result"]


def test_stream_two_os_processes_append_fifty_each(scratch_daemon):
    stream = f"concurrent-{uuid.uuid4()}"
    worker = r'''
import json, os, sys
from khive.ops import encode, op
from khive.transport import Session, SocketTransport
client = Session(SocketTransport(sys.argv[1]), timeout=30)
client.handshake()
sys.stdin.readline()
seqs = []
for n in range(50):
    result = client.request(encode([op("stream.append", stream=sys.argv[2], record={"writer": sys.argv[3], "n": n})]))[0]
    assert result["ok"], result
    seqs.append(result["result"]["seq"])
print(json.dumps({"pid": os.getpid(), "seqs": seqs}))
'''
    procs = [subprocess.Popen([sys.executable, "-c", worker, str(scratch_daemon["socket"]), stream, str(index)], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True) for index in range(2)]
    outputs = []
    try:
        for proc in procs:
            proc.stdin.write("go\n")
            proc.stdin.flush()
        for proc in procs:
            stdout, stderr = proc.communicate(timeout=240)
            assert proc.returncode == 0, stderr
            outputs.append(json.loads(stdout))
    finally:
        for proc in procs:
            if proc.poll() is None:
                proc.kill()
                proc.wait()
    assert len({item["pid"] for item in outputs}) == 2
    assert all(len(item["seqs"]) == 50 for item in outputs)
    assert sorted(seq for item in outputs for seq in item["seqs"]) == list(range(1, 101))
    page = one(session(scratch_daemon), "stream.read", stream=stream)
    assert [entry["seq"] for entry in page["entries"]] == list(range(1, 101))
    assert {(entry["record"]["writer"], entry["record"]["n"]) for entry in page["entries"]} == {(str(w), n) for w in range(2) for n in range(50)}
    assert page["head_seq"] == 100 and page["next_after"] is None


def test_stream_python_cli_same_result_and_error_objects(scratch_daemon):
    binary = os.environ.get("KKERNEL")
    assert binary, "set KKERNEL to this checkout's freshly built binary"
    client = session(scratch_daemon)
    stream = f"transport-{uuid.uuid4()}"
    appended = one(client, "stream.append", stream=stream, record=None, expected_seq=1)
    assert appended["seq"] == 1
    uuid.UUID(appended["id"])
    env = os.environ.copy()
    env["KHIVE_SOCKET"] = str(scratch_daemon["socket"])
    env["KHIVE_PID"] = str(scratch_daemon["root"] / "khived.pid")
    env.pop("KHIVE_NO_DAEMON", None)
    for verb, args in [
        ("stream.read", {"stream": stream}),
        ("stream.stat", {"stream": stream}),
        ("stream.append", {"stream": stream, "record": None, "expected_seq": 1}),
        ("stream.append", {"stream": stream, "record": None, "fence": None}),
    ]:
        ops = encode([op(verb, **args)])
        expected = client.request(ops)[0]
        proc = subprocess.run([binary, "exec", ops, "--config", str(scratch_daemon["root"] / "khive.toml"), "--db", str(scratch_daemon["root"] / "scratch.db"), "--presentation", "verbose", "--output-format", "json"], env=env, cwd=scratch_daemon["root"], capture_output=True, text=True, timeout=60)
        envelope = json.loads(proc.stdout)
        actual = envelope["results"][0]
        assert actual["ok"] == expected["ok"], (actual, expected, proc.stderr)
        field = "result" if expected["ok"] else "error"
        assert actual[field] == expected[field]
    assert one(client, "stream.read", stream=stream)["entries"][0]["record"] is None
