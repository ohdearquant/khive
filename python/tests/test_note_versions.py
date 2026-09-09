"""Versioned heads through independent socket and strict CLI processes.

Acceptance runs must set KKERNEL to the binary built from this checkout.
All processes use the scratch_daemon fixture's isolated database and config.
"""
from __future__ import annotations

import json
import os
import selectors
import subprocess
import sys
import uuid

import pytest

from khive import Khive, OperationError, op
from khive.ops import encode
from khive.transport import Session, SocketTransport


WORKER = r'''
import json, os, subprocess, sys
from pathlib import Path
from khive.ops import encode, op
from khive.transport import Session, SocketTransport
surface, binary, root, socket, spec = sys.argv[1:]
spec = json.loads(spec)
client = Session(SocketTransport(socket), actor_id="version-test", timeout=60)
def request(verb, args):
    ops = encode([op(verb, **args)])
    if surface == "socket":
        return client.request(ops)[0], None
    env = os.environ.copy()
    env["KHIVE_SOCKET"] = socket
    env["KHIVE_PID"] = str(Path(root) / "khived.pid")
    env["KHIVE_LOCK"] = str(Path(root) / "khived.recovery.lock")
    env["KHIVE_RECOVERER_LOCK"] = str(Path(root) / "khived.recoverer.lock")
    env.pop("KHIVE_NO_DAEMON", None)
    command = [binary, "exec", ops, "--strict", "--actor", "version-test",
        "--config", str(Path(root) / "khive.toml"),
        "--db", str(Path(root) / "scratch.db"), "--presentation", "verbose", "--output-format", "json"]
    result = subprocess.run(command, env=env, cwd=root, capture_output=True, text=True, timeout=60)
    assert result.stdout.strip(), (result.returncode, result.stderr)
    response = json.loads(result.stdout)["results"][0]
    assert (result.returncode == 0) == response["ok"], (result.returncode, response, result.stderr)
    return response, result.returncode
if "read_id" in spec:
    before, _ = request("get", {"id": spec["read_id"]})
    assert before["ok"], before
    revision = before["result"]["version"]
    assert type(revision) is int
else:
    revision = None
print(json.dumps({"pid": os.getpid(), "version": revision}), flush=True)
assert sys.stdin.readline() == "go\n"
response, rc = request(spec["verb"], spec["args"])
print(json.dumps({"response": response, "returncode": rc}), flush=True)
'''


def client(scratch):
    return Session(SocketTransport(scratch["socket"]), actor_id="version-test")


def one(session, verb, **args):
    result = session.request(encode([op(verb, **args)]))[0]
    assert result["ok"], result
    return result["result"]


def race(scratch, surface, specs):
    binary = os.environ.get("KKERNEL")
    assert binary, "set KKERNEL to this checkout's freshly built binary"
    processes = []
    try:
        for spec in specs:
            processes.append(subprocess.Popen(
                [sys.executable, "-c", WORKER, surface, binary, str(scratch["root"]),
                 str(scratch["socket"]), json.dumps(spec)],
                stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
            ))
        ready = []
        with selectors.DefaultSelector() as selector:
            for process in processes:
                selector.register(process.stdout, selectors.EVENT_READ)
            while selector.get_map():
                events = selector.select(timeout=90)
                assert events, "workers did not reach the read-before-write barrier"
                for key, _ in events:
                    line = key.fileobj.readline()
                    assert line, "worker exited before reaching the barrier"
                    ready.append(json.loads(line))
                    selector.unregister(key.fileobj)
        assert len({item["pid"] for item in ready}) == 2
        for process in processes:
            process.stdin.write("go\n")
            process.stdin.flush()
        results = []
        for process in processes:
            stdout, stderr = process.communicate(timeout=90)
            assert process.returncode == 0, stderr
            results.append(json.loads(stdout))
        return ready, results
    finally:
        for process in processes:
            if process.poll() is None:
                process.kill()
            process.wait()


@pytest.mark.parametrize("surface", ["socket", "cli"])
def test_note_version_cross_process_cas_and_lost_ack(scratch_daemon, surface):
    session = client(scratch_daemon)
    note = one(session, "create", kind="head", key=f"cas/{uuid.uuid4()}", content="{}")
    assert type(note["version"]) is int and note["version"] == 1
    specs = [{"read_id": note["id"], "verb": "update", "args": {
        "id": note["id"], "expected_version": 1, "content": json.dumps({"writer": i}),
    }} for i in range(2)]
    ready, outcomes = race(scratch_daemon, surface, specs)
    assert [item["version"] for item in ready] == [1, 1]
    success = [item["response"] for item in outcomes if item["response"]["ok"]]
    refused = [item["response"] for item in outcomes if not item["response"]["ok"]]
    assert len(success) == len(refused) == 1, outcomes
    assert success[0]["result"]["version"] == 2
    details = refused[0]["error"]["details"]
    assert details["reason"] == "version_conflict"
    assert details["expected_version"] == "1" and details["current_version"] == "2"
    final = one(session, "get", id=note["id"])
    assert final["version"] == 2 and final["content"] == success[0]["result"]["content"]
    winner = next(i for i, outcome in enumerate(outcomes) if outcome["response"]["ok"])
    retry = session.request(encode([op("update", **specs[winner]["args"])]))[0]
    assert retry["error"]["details"]["reason"] == "version_conflict"
    assert one(session, "get", id=note["id"]) == final


@pytest.mark.parametrize("surface", ["socket", "cli"])
def test_note_version_cross_process_key_claim(scratch_daemon, surface):
    session = client(scratch_daemon)
    key = f"claim/{uuid.uuid4()}"
    specs = [{"verb": "create", "args": {"kind": "head", "key": key,
              "content": json.dumps({"writer": i})}} for i in range(2)]
    _, outcomes = race(scratch_daemon, surface, specs)
    success = [item["response"] for item in outcomes if item["response"]["ok"]]
    refused = [item["response"] for item in outcomes if not item["response"]["ok"]]
    assert len(success) == len(refused) == 1, outcomes
    winner = success[0]["result"]
    assert winner["version"] == 1
    details = refused[0]["error"]["details"]
    assert details["reason"] == "key_conflict" and details["key"] == key
    assert details["existing_id"] == winner["id"]
    stored = one(session, "get", kind="note", key=key, note_kind="head")
    assert stored["id"] == winner["id"] and stored["version"] == 1
    page = one(session, "list", kind="note", note_kind="head", key_prefix=key)
    assert [item["id"] for item in page["notes"]] == [winner["id"]]


@pytest.mark.parametrize("surface", ["socket", "cli"])
def test_note_version_cross_process_stale_and_missing_fence(scratch_daemon, surface):
    session = client(scratch_daemon)
    key = f"fence/{uuid.uuid4()}"
    fence = one(session, "create", kind="head", key=key, content="{}")
    target = one(session, "create", kind="head", content="{}", fence={
        "key": key, "kind": "head", "expected_version": 1,
    })
    fence = one(session, "update", id=fence["id"], expected_version=1, content='{"epoch":2}')
    specs = [{"verb": "update", "args": {"id": target["id"], "content": '{"bad":true}',
              "fence": {"key": name, "kind": "head", "expected_version": 1}}}
             for name in [key, f"missing/{uuid.uuid4()}"]]
    _, outcomes = race(scratch_daemon, surface, specs)
    for outcome in outcomes:
        result = outcome["response"]
        assert not result["ok"] and result["error"]["details"]["reason"] == "fence_conflict"
    assert one(session, "get", id=fence["id"])["version"] == fence["version"]
    assert one(session, "get", id=target["id"])["version"] == target["version"]
    assert one(session, "get", id=target["id"])["content"] == target["content"]


def test_note_version_typed_creation_rejects_stale_fence(scratch_daemon):
    db = Khive(socket_path=str(scratch_daemon["socket"]), actor_id="version-test")
    with pytest.raises(OperationError) as raised:
        db.notes.create(kind="head", subject="", content="{}", embed=False,
                        fence={"key": f"missing/{uuid.uuid4()}", "kind": "head", "expected_version": 1})
    assert raised.value.error.details["reason"] == "fence_conflict"
