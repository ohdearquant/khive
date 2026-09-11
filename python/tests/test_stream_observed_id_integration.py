"""ADR-174 A6 arm 9: identity across recreation by a second OS client.

Set KKERNEL to this checkout's freshly built candidate; acceptance requires zero skips.
"""
from __future__ import annotations

import json
import os
import sqlite3
import subprocess
import sys
import uuid

from khive.ops import encode, op
from khive.transport import Session, SocketTransport


RECREATE = r'''
import json, os, sys
from khive.ops import encode, op
from khive.transport import Session, SocketTransport
client = Session(SocketTransport(sys.argv[1]), timeout=30)
client.handshake()
def request(verb, **args):
    result = client.request(encode([op(verb, **args)]))[0]
    assert result["ok"], result
    return result["result"]
request("delete", id=sys.argv[3])
replacement = request("create", kind="head", key=sys.argv[2], content="{}")
assert replacement["id"] != sys.argv[3] and replacement["version"] == 1, replacement
print(json.dumps({"pid":os.getpid(), "replacement":replacement}))
'''


def _counts(scratch):
    with sqlite3.connect(f"file:{scratch['root'] / 'scratch.db'}?mode=ro", uri=True) as db:
        return tuple(db.execute(sql).fetchone()[0] for sql in (
            "SELECT COUNT(*) FROM notes",
            "SELECT COUNT(*) FROM note_streams",
            "SELECT COUNT(*) FROM fts_notes",
            "SELECT COUNT(*) FROM events WHERE kind != 'audit'",
        ))


def test_observed_id_arm9_socket_second_process_recreation(scratch_daemon):
    assert os.environ.get("KKERNEL"), "set KKERNEL to this checkout's freshly built binary"
    client = Session(SocketTransport(scratch_daemon["socket"]), timeout=30)
    client.handshake()

    def request(verb, **args):
        return client.request(encode([op(verb, **args)]))[0]

    def one(verb, **args):
        result = request(verb, **args)
        assert result["ok"], result
        return result["result"]

    for replaced in (False, True):
        suffix = str(uuid.uuid4())
        key, target = f"lease-{suffix}", f"target-{suffix}"
        streams = [f"identity-a-{suffix}", f"identity-b-{suffix}"]
        one("create", kind="head", key=key, content="{}")
        one("create", kind="head", key=target, content="{}")
        original = one("get", kind="head", key=key)
        assert original["version"] == 1
        if replaced:
            peer = subprocess.run([sys.executable, "-c", RECREATE, str(scratch_daemon["socket"]), key, original["id"]],
                                  capture_output=True, text=True, timeout=60)
            assert peer.returncode == 0, (peer.stdout, peer.stderr)
            result = json.loads(peer.stdout)
            assert result["pid"] != os.getpid()
            current = result["replacement"]
            assert one("get", kind="head", key=key)["id"] == current["id"]
        ops = [
            {"op":"append", "stream":streams[0], "record":1},
            {"op":"write", "key":target, "kind":"head", "expected_version":1, "doc":{"published":True}},
            {"op":"append", "stream":streams[1], "record":2},
        ]
        observed = {"key":key, "kind":"head", "version":1, "id":original["id"]}
        before = (_counts(scratch_daemon), [one("stream.stat", stream=s) for s in streams], one("get", kind="head", key=target))
        result = request("stream.batch", atomic=True, observed=[observed], ops=ops)
        if replaced:
            assert not result["ok"], result
            error = result["error"]
            assert error["kind"] == "conflict" and error["domain_disposition"] == "not_committed", result
            assert error["details"] == {"reason":"identity_conflict", "key":key, "kind":"head", "version":"1",
                                        "id":original["id"], "current_id":current["id"], "index":"0"}, result
            after = (_counts(scratch_daemon), [one("stream.stat", stream=s) for s in streams], one("get", kind="head", key=target))
            assert after == before
            # Arm 2 as a socket control too: the replacement's version suffices without id.
            del observed["id"]
            result = request("stream.batch", atomic=True, observed=[observed], ops=ops)
        assert result["ok"] and result["result"]["committed"] is True, result
        members = result["result"]["results"]
        assert len(members) == 3 and members[0]["seq"] == 1 and members[1]["version"] == 2 and members[2]["seq"] == 1
        assert [one("stream.stat", stream=s) for s in streams] == [{"count":1, "head_seq":1}] * 2
        written = one("get", kind="head", key=target)
        assert written["version"] == 2 and json.loads(written["content"]) == {"published":True}
