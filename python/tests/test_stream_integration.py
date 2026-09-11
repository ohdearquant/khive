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
        assert proc.stdout.strip(), (proc.returncode, proc.stderr)
        envelope = json.loads(proc.stdout)
        actual = envelope["results"][0]
        assert actual["ok"] == expected["ok"], (actual, expected, proc.stderr)
        field = "result" if expected["ok"] else "error"
        assert actual[field] == expected[field]
    assert one(client, "stream.read", stream=stream)["entries"][0]["record"] is None


BATCH_WORKER = r'''
import json, os, sys
from khive.ops import encode, op
from khive.transport import Session, SocketTransport
client = Session(SocketTransport(sys.argv[1]), timeout=30)
client.handshake()
sys.stdin.readline()
atomic = sys.argv[4] == "atomic"
batches = []
for n in range(int(sys.argv[5])):
    ops = [{"op": "append", "stream": sys.argv[2], "record": {"writer": sys.argv[3], "batch": n, "i": i}} for i in range(3)]
    result = client.request(encode([op("stream.batch", ops=ops, atomic=atomic)]))[0]
    assert result["ok"], result
    batches.append([m["seq"] for m in result["result"]["results"]])
print(json.dumps({"pid": os.getpid(), "batches": batches}))
'''


def _two_processes_batching(scratch_daemon, stream, mode, repeats):
    procs = [subprocess.Popen([sys.executable, "-c", BATCH_WORKER, str(scratch_daemon["socket"]), stream, str(index), mode, str(repeats)], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True) for index in range(2)]
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
    return outputs


def test_stream_batch_two_processes_atomic_members_are_consecutive(scratch_daemon):
    # Amendment 1 acceptance 6: each batch's appends are consecutive within
    # the batch and the union of numbers is dense.
    stream = f"batch-atomic-{uuid.uuid4()}"
    outputs = _two_processes_batching(scratch_daemon, stream, "atomic", 20)
    all_seqs = []
    for item in outputs:
        for batch in item["batches"]:
            assert batch == list(range(batch[0], batch[0] + 3)), batch
            all_seqs.extend(batch)
    assert sorted(all_seqs) == list(range(1, 121))
    assert one(session(scratch_daemon), "stream.stat", stream=stream) == {"count": 120, "head_seq": 120}


def test_stream_batch_two_processes_per_member_increase_and_interleave(scratch_daemon):
    # Amendment 1 acceptance 7: numbers increase in list order, the union is
    # dense, adjacency is not asserted, and the control requires at least one
    # repeat in which the other process's number fell between two members.
    stream = f"batch-member-{uuid.uuid4()}"
    outputs = _two_processes_batching(scratch_daemon, stream, "per_member", 40)
    all_seqs, interleaved = [], 0
    for item in outputs:
        for batch in item["batches"]:
            assert batch == sorted(batch) and len(set(batch)) == 3, batch
            interleaved += batch[2] - batch[0] > 2
            all_seqs.extend(batch)
    assert sorted(all_seqs) == list(range(1, 241))
    assert interleaved >= 1, "control: no repeat interleaved, so the arm did not exercise what per-member mode permits"
    print(f"interleaved batches: {interleaved} of {len(all_seqs) // 3}")


def _without_per_call_fields(result):
    return [{k: v for k, v in member.items() if k not in {"seq", "id", "created_at", "updated_at"}} for member in result["results"]]


def test_stream_batch_python_cli_same_result_and_error_objects(scratch_daemon):
    binary = os.environ.get("KKERNEL")
    assert binary, "set KKERNEL to this checkout's freshly built binary"
    client = session(scratch_daemon)
    stream = f"batch-transport-{uuid.uuid4()}"
    env = os.environ.copy()
    env["KHIVE_SOCKET"] = str(scratch_daemon["socket"])
    env["KHIVE_PID"] = str(scratch_daemon["root"] / "khived.pid")
    env.pop("KHIVE_NO_DAEMON", None)
    # Each side gets its own keyed-write key: a write member creates the key on
    # its first run, so replaying one string on both transports would compare a
    # creation against a key conflict instead of two equal creations.
    for make_args in [
        lambda: {"ops": [{"op": "append", "stream": stream, "record": None}, {"op": "nope"}, {"op": "write", "key": f"h-{uuid.uuid4()}", "kind": "head", "doc": {}}]},
        lambda: {"ops": [{"op": "append", "stream": stream, "record": None, "expected_seq": 99}], "atomic": True},
        lambda: {"ops": [{"op": "append", "stream": stream, "record": None}], "fence": {"key": "k", "kind": "head", "expected_version": 1}},
    ]:
        expected = client.request(encode([op("stream.batch", **make_args())]))[0]
        ops = encode([op("stream.batch", **make_args())])
        proc = subprocess.run([binary, "exec", ops, "--config", str(scratch_daemon["root"] / "khive.toml"), "--db", str(scratch_daemon["root"] / "scratch.db"), "--presentation", "verbose", "--output-format", "json"], env=env, cwd=scratch_daemon["root"], capture_output=True, text=True, timeout=60)
        actual = json.loads(proc.stdout)["results"][0]
        assert actual["ok"] == expected["ok"], (actual, expected, proc.stderr)
        if expected["ok"]:
            assert actual["result"]["committed"] is True and expected["result"]["committed"] is True
            assert _without_per_call_fields(actual["result"]) == _without_per_call_fields(expected["result"])
        else:
            assert actual["error"] == expected["error"]
