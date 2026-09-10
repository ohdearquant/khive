"""Expiry acceptance through a scratch daemon and a second OS client.

Set KKERNEL to the freshly built candidate. Run with zero skips.
"""
from __future__ import annotations

from datetime import datetime, timezone
import json
import os
import select
import sqlite3
import subprocess
import sys
import uuid

from khive.ops import encode, op
from khive.transport import Session, SocketTransport


WORKER = r'''
from datetime import datetime, timedelta, timezone
import json, os, sys
from khive.ops import encode, op
from khive.transport import Session, SocketTransport
client = Session(SocketTransport(sys.argv[1]), timeout=30)
client.handshake()
key = sys.argv[2]
def write(version, ttl):
    deadline = (datetime.now(timezone.utc) + timedelta(seconds=ttl)).isoformat(timespec="microseconds")
    result = client.request(encode([op("stream.batch", ops=[{"op":"write", "key":key, "kind":"head", "doc":{"expires_at":deadline}, "embed":False, "expected_version":version}])]))[0]
    assert result["ok"], result
    print(json.dumps({"pid":os.getpid(), "deadline":deadline, "write":result["result"]["results"][0]}), flush=True)
write(None, 1)
assert sys.stdin.readline().strip() == "renew"
write(1, 3600)
assert sys.stdin.readline().strip() == "done"
'''


def _receive(proc):
    assert select.select([proc.stdout], [], [], 60)[0], "peer did not respond"
    line = proc.stdout.readline()
    assert line, "peer closed without response"
    return json.loads(line)


def _counts(scratch):
    with sqlite3.connect(f"file:{scratch['root'] / 'scratch.db'}?mode=ro", uri=True) as db:
        return tuple(db.execute(sql).fetchone()[0] for sql in (
            "SELECT COUNT(*) FROM notes",
            "SELECT COUNT(*) FROM note_streams",
            "SELECT COUNT(*) FROM events WHERE kind != 'audit'",
        ))


def test_expiry_arm10_socket_expiry_and_intervening_writer(scratch_daemon):
    client = Session(SocketTransport(scratch_daemon["socket"]), timeout=30)
    client.handshake()
    key, stream = f"lease-{uuid.uuid4()}", f"expiry-{uuid.uuid4()}"
    proc = subprocess.Popen([sys.executable, "-c", WORKER, str(scratch_daemon["socket"]), key],
                            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    def request(verb, **args):
        return client.request(encode([op(verb, **args)]))[0]
    def publish(version):
        return request("stream.batch", atomic=True,
                       observed=[{"key":key, "kind":"head", "version":version, "live_until":"expires_at"}],
                       ops=[{"op":"append", "stream":stream, "record":1}])
    try:
        first = _receive(proc)
        assert first["pid"] != os.getpid()
        observed = request("get", key=key, kind="head")
        assert observed["ok"] and observed["result"]["version"] == 1
        live = publish(1)
        assert live["ok"] and live["result"]["committed"] is True, live
        # Peer remains alive holding the document; no writer renews it while
        # its deadline passes. The parent does not edit the deadline.
        deadline = datetime.fromisoformat(first["deadline"])
        import time
        time.sleep(max(0, (deadline - datetime.now(timezone.utc)).total_seconds()) + 0.02)
        assert proc.poll() is None
        before = _counts(scratch_daemon)
        expired = publish(1)
        assert not expired["ok"], expired
        error = expired["error"]
        assert error["kind"] == "conflict" and error["domain_disposition"] == "not_committed", expired
        details = error["details"]
        assert details["reason"] == "expired" and details["key"] == key, expired
        assert details["field"] == "expires_at" and json.loads(details["value"]) == first["deadline"]
        assert datetime.fromisoformat(details["now"].replace("Z", "+00:00")) > deadline
        assert _counts(scratch_daemon) == before
        assert request("stream.stat", stream=stream)["result"] == {"count":1, "head_seq":1}
        assert request("get", key=key, kind="head")["result"]["version"] == 1
        proc.stdin.write("renew\n")
        proc.stdin.flush()
        renewed = _receive(proc)
        assert renewed["write"]["version"] == 2
        before = _counts(scratch_daemon)
        stale = publish(1)
        assert not stale["ok"] and stale["error"]["details"]["reason"] == "version_conflict", stale
        assert stale["error"]["domain_disposition"] == "not_committed"
        assert _counts(scratch_daemon) == before
        assert publish(2)["ok"]
        assert request("stream.stat", stream=stream)["result"] == {"count":2, "head_seq":2}
        proc.stdin.write("done\n")
        proc.stdin.flush()
        _, stderr = proc.communicate(timeout=60)
        assert proc.returncode == 0, stderr
    finally:
        if proc.poll() is None:
            proc.kill()
            proc.wait()
