"""ADR-173 filesystem acceptance against an explicitly selected real candidate.

Run with KKERNEL pointing to this checkout's gate-built executable and with
the Python blake3 package installed. No binary fallback and no skip path.
Each test owns a scratch daemon; none uses the user's daemon or blob root.
"""

from __future__ import annotations

import ast
import base64
import json
import os
from pathlib import Path
import re
import select
import shutil
import signal
import socket
import struct
import subprocess
import tempfile
import time

from blake3 import blake3
import pytest

from khive.dsl import MAX_OPS_INPUT_LEN
from khive.errors import RequestRejected
from khive.ops import encode, op
from khive.transport import MAX_FRAME_BYTES, Session, SocketTransport


REPO = Path(__file__).resolve().parents[2]
OBJECT_BYTES = 64 * 1024 * 1024
NO_SWEEP_SECS = 86_400


def _rust_constant(relative_path: str, name: str) -> int:
    source = (REPO / relative_path).read_text()
    found = re.search(rf"\bconst {name}\s*:\s*(?:u64|usize)\s*=\s*([^;]+);", source)
    assert found, f"missing live Rust constant {name} in {relative_path}"

    def integer(node):
        if isinstance(node, ast.Constant) and type(node.value) is int:
            return node.value
        if isinstance(node, ast.BinOp) and isinstance(node.op, ast.Mult):
            return integer(node.left) * integer(node.right)
        if isinstance(node, ast.BinOp) and isinstance(node.op, ast.Add):
            return integer(node.left) + integer(node.right)
        if isinstance(node, ast.BinOp) and isinstance(node.op, ast.Sub):
            return integer(node.left) - integer(node.right)
        raise AssertionError(f"inspect changed Rust constant expression: {found.group(1)}")

    return integer(ast.parse(found.group(1).strip(), mode="eval").body)


def _b64(payload: bytes) -> str:
    return base64.b64encode(payload).decode("ascii")


def _entry(client: Session, verb: str, **args) -> dict:
    results = client.request(encode([op(verb, **args)]))
    assert len(results) == 1 and results[0]["tool"] == verb, results
    return results[0]


def _one(client: Session, verb: str, **args) -> dict:
    entry = _entry(client, verb, **args)
    assert entry["ok"], entry
    return entry["result"]


def _refused(client: Session, verb: str, *, contains: str, **args) -> dict:
    entry = _entry(client, verb, **args)
    assert not entry["ok"], f"unexpectedly accepted {verb}: {entry}"
    assert contains in entry["error"]["message"].lower(), entry
    return entry["error"]


def _begin(client: Session, size: int, **args) -> dict:
    result = _one(client, "blob.begin", size=size, **args)
    assert set(result) == {"upload_id", "part_limit", "next_index"}, result
    assert re.fullmatch(r"[0-9a-f]{32}", result["upload_id"]), result
    assert result["next_index"] == 0 and result["part_limit"] > 0, result
    return result


def _part(client: Session, upload_id: str, index: int, payload: bytes) -> dict:
    return _one(client, "blob.put_part", upload_id=upload_id, index=index, bytes=_b64(payload))


def _upload(client: Session, payload: bytes) -> dict:
    begin = _begin(client, len(payload))
    for index, offset in enumerate(range(0, len(payload), begin["part_limit"])):
        chunk = payload[offset : offset + begin["part_limit"]]
        assert _part(client, begin["upload_id"], index, chunk) == {
            "next_index": index + 1,
            "received_bytes": offset + len(chunk),
        }
    committed = _one(client, "blob.commit", upload_id=begin["upload_id"])
    assert committed == {"content_ref": blake3(payload).hexdigest(), "size": len(payload)}
    return committed


class _Daemon:
    def __init__(self, binary: str, root: Path, idle: int, sweep: int):
        self.binary, self.root, self.idle, self.sweep = binary, root, idle, sweep
        self.socket = root / "khived.sock"
        self.blobs = root / "blobs"
        self.config = root / "khive.toml"
        self.config.write_text(
            '[storage.blob]\nbackend = "fs"\n'
            f"root = {json.dumps(str(self.blobs))}\nfloor_bytes = 0\n"
        )
        self.env = {key: value for key, value in os.environ.items() if not key.startswith("KHIVE_")}
        self.env.update(
            KHIVE_SOCKET=str(self.socket),
            KHIVE_PID=str(root / "khived.pid"),
            KHIVE_LOCK=str(root / "khived.recovery.lock"),
            KHIVE_RECOVERER_LOCK=str(root / "khived.recoverer.lock"),
            KHIVE_PACKS="kg,blob",
            KHIVE_BLOB_UPLOAD_IDLE_SECS=str(idle),
            KHIVE_BLOB_UPLOAD_SWEEP_INTERVAL_SECS=str(sweep),
        )
        self.process = None
        self.starts = 0

    def client(self) -> Session:
        return Session(SocketTransport(self.socket), actor_id="test:blob:owner", timeout=60)

    def args(self) -> list[str]:
        return ["--config", str(self.config), "--db", str(self.root / "scratch.db"), "--no-embed"]

    def start(self):
        assert self.process is None
        self.starts += 1
        self.stderr = self.root / f"daemon-{self.starts}.stderr"
        with self.stderr.open("wb") as stderr:
            self.process = subprocess.Popen(
                [self.binary, "mcp", "--daemon", *self.args()],
                env=self.env,
                cwd=self.root,
                stdout=subprocess.DEVNULL,
                stderr=stderr,
            )
        self.started_at = time.monotonic()
        deadline = self.started_at + 45
        last_error = None
        while time.monotonic() < deadline:
            self.assert_alive()
            if self.socket.exists():
                try:
                    ready = Session(SocketTransport(self.socket), timeout=2)
                    _one(ready, "stats")
                    return self
                except Exception as exc:  # readiness includes the audit daemon
                    last_error = exc
            time.sleep(0.1)
        raise AssertionError(f"scratch daemon never became ready: {last_error}; {self.logs()}")

    def logs(self) -> str:
        return self.stderr.read_text(errors="replace")[-4000:]

    def assert_alive(self):
        assert self.process is not None and self.process.poll() is None, self.logs()

    def stop(self):
        if self.process is None:
            return
        if self.process.poll() is None:
            self.process.send_signal(signal.SIGTERM)
            try:
                self.process.wait(timeout=15)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=10)
        self.process = None

    def staging(self) -> set[Path]:
        path = self.blobs / ".uploads"
        return set(path.iterdir()) if path.exists() else set()

    def stage(self, begin: dict) -> Path:
        path = self.blobs / ".uploads" / begin["upload_id"]
        assert path.is_file(), (path, self.staging())
        return path

    def object(self, content_ref: str) -> Path:
        return self.blobs / content_ref[:2] / content_ref[2:4] / content_ref

    def objects(self) -> set[Path]:
        return {
            path for path in self.blobs.glob("*/*/*")
            if re.fullmatch(r"[0-9a-f]{64}", path.name) and path.is_file()
        }

    def wait_removed(self, stage: Path):
        deadline = time.monotonic() + self.idle + 3 * self.sweep + 5
        while stage.exists() and time.monotonic() < deadline:
            self.assert_alive()
            time.sleep(0.05)
        assert not stage.exists(), f"daemon did not expire {stage}: {self.logs()}"
        self.assert_alive()


@pytest.fixture
def upload_daemon_factory():
    selected = os.environ.get("KKERNEL")
    assert selected, "set KKERNEL to this checkout's gate-built candidate; no installed fallback"
    binary = Path(selected).resolve()
    assert binary.is_file() and os.access(binary, os.X_OK), binary
    daemons = []

    def create(*, idle=3600, sweep=NO_SWEEP_SECS, max_active=None, max_per_actor=None):
        # /tmp keeps both the main and derived events AF_UNIX paths below macOS's cap.
        root = Path(tempfile.mkdtemp(prefix="blob-wire-", dir="/tmp"))
        daemon = _Daemon(str(binary), root, idle, sweep)
        if max_active is not None:
            daemon.env["KHIVE_BLOB_UPLOAD_MAX_ACTIVE"] = str(max_active)
        if max_per_actor is not None:
            daemon.env["KHIVE_BLOB_UPLOAD_MAX_PER_ACTOR"] = str(max_per_actor)
        daemons.append(daemon)
        return daemon.start()

    yield create
    for daemon in reversed(daemons):
        daemon.stop()
        shutil.rmtree(daemon.root, ignore_errors=True)


def test_blob_upload_wire_64mib_external_blake3_ranged_roundtrip(upload_daemon_factory):
    daemon = upload_daemon_factory()
    client = daemon.client()
    block = bytes(range(256)) * 4096
    payload = block * 64
    assert len(payload) == OBJECT_BYTES
    expected = blake3(payload).hexdigest()
    begin = _begin(client, len(payload))
    part_count = 0
    for index, offset in enumerate(range(0, len(payload), begin["part_limit"])):
        chunk = payload[offset : offset + begin["part_limit"]]
        assert _part(client, begin["upload_id"], index, chunk) == {
            "next_index": index + 1,
            "received_bytes": offset + len(chunk),
        }
        part_count += 1
    assert part_count == (OBJECT_BYTES + begin["part_limit"] - 1) // begin["part_limit"]
    result = _one(client, "blob.commit", upload_id=begin["upload_id"])
    assert result == {"content_ref": expected, "size": OBJECT_BYTES}
    assert not daemon.staging()
    _refused(client, "blob.commit", upload_id=begin["upload_id"], contains="unknown upload")

    response_reserve = _rust_constant(
        "crates/khive-pack-blob/src/handlers.rs", "RESPONSE_ENVELOPE_RESERVE_BYTES"
    )
    read_limit = (MAX_FRAME_BYTES - response_reserve) * 3 // 4
    returned = blake3()
    ranges = 0
    for offset in range(0, OBJECT_BYTES, read_limit):
        length = min(read_limit, OBJECT_BYTES - offset)
        result = _one(client, "blob.get", content_ref=expected, range={"offset": offset, "length": length})
        raw = base64.b64decode(result["bytes"], validate=True)
        assert result["size"] == OBJECT_BYTES
        assert result["range"] == {"offset": offset, "length": length}
        assert raw == payload[offset : offset + length]
        returned.update(raw)
        ranges += 1
    assert returned.hexdigest() == expected
    print(f"external BLAKE3 {expected}; {part_count} upload parts; {ranges} ranged reads")


def test_blob_upload_wire_known_and_full_dedup_touch_one_object(upload_daemon_factory):
    daemon = upload_daemon_factory()
    client, payload = daemon.client(), b"ADR-173 dedup baseline" * 1024
    first = _upload(client, payload)
    path = daemon.object(first["content_ref"])
    assert daemon.objects() == {path}
    assert _one(client, "blob.begin", size=len(payload), content_ref=first["content_ref"]) == first
    assert not daemon.staging()
    before = path.stat().st_mtime_ns
    time.sleep(1.05)
    assert _upload(client, payload) == first
    assert path.stat().st_mtime_ns > before
    assert daemon.objects() == {path} and not daemon.staging()
    before = path.stat().st_mtime_ns
    time.sleep(1.05)
    assert _one(client, "blob.put", bytes=_b64(payload)) == first
    assert path.stat().st_mtime_ns > before
    assert daemon.objects() == {path}


def test_blob_upload_wire_wrong_index_preserves_sequential_progress(upload_daemon_factory):
    daemon = upload_daemon_factory()
    client = daemon.client()
    begin = _begin(client, 2)
    stage = daemon.stage(begin)
    for index in (1, 9):
        error = _refused(client, "blob.put_part", upload_id=begin["upload_id"], index=index,
                         bytes=_b64(b"x"), contains="invalid input")
        assert "index" in error["message"].lower()
        assert stage.stat().st_size == 0
    assert _part(client, begin["upload_id"], 0, b"a") == {"next_index": 1, "received_bytes": 1}
    _refused(client, "blob.put_part", upload_id=begin["upload_id"], index=2,
             bytes=_b64(b"x"), contains="invalid input")
    assert stage.read_bytes() == b"a"
    assert _part(client, begin["upload_id"], 1, b"b") == {"next_index": 2, "received_bytes": 2}
    assert _one(client, "blob.commit", upload_id=begin["upload_id"]) == {
        "content_ref": blake3(b"ab").hexdigest(), "size": 2,
    }


def test_blob_upload_wire_identical_tail_retry_does_not_append_or_touch(upload_daemon_factory):
    daemon = upload_daemon_factory()
    client = daemon.client()
    begin = _begin(client, 2)
    first = _part(client, begin["upload_id"], 0, b"a")
    stage = daemon.stage(begin)
    before = stage.stat()
    assert _part(client, begin["upload_id"], 0, b"a") == first
    assert stage.read_bytes() == b"a" and stage.stat().st_mtime_ns == before.st_mtime_ns
    assert _part(client, begin["upload_id"], 1, b"b") == {"next_index": 2, "received_bytes": 2}
    assert _one(client, "blob.commit", upload_id=begin["upload_id"]) == {
        "content_ref": blake3(b"ab").hexdigest(), "size": 2,
    }


@pytest.mark.parametrize("retry", [b"aa", b"b"], ids=["different_length", "same_length_different_bytes"])
def test_blob_upload_wire_tail_mismatch_aborts(upload_daemon_factory, retry):
    daemon = upload_daemon_factory()
    client = daemon.client()
    begin = _begin(client, 3)
    _part(client, begin["upload_id"], 0, b"a")
    stage = daemon.stage(begin)
    _refused(client, "blob.put_part", upload_id=begin["upload_id"], index=0,
             bytes=_b64(retry), contains="invalid input")
    assert not stage.exists() and not daemon.staging()
    _refused(client, "blob.commit", upload_id=begin["upload_id"], contains="unknown upload")


def test_blob_upload_wire_begin_above_object_ceiling_creates_no_stage(upload_daemon_factory):
    daemon = upload_daemon_factory()
    assert _rust_constant("crates/khive-pack-blob/src/handlers.rs", "MAX_OBJECT_BYTES") == OBJECT_BYTES
    _refused(daemon.client(), "blob.begin", size=OBJECT_BYTES + 1, contains="invalid input")
    assert not daemon.staging() and not daemon.objects()


def test_blob_upload_wire_total_active_cap_refuses_and_abort_releases_slot(upload_daemon_factory):
    # Keep the per-actor ceiling above the attempted third upload so it cannot
    # mask removal of the total-cap guard in the mutation control.
    daemon = upload_daemon_factory(max_active=2, max_per_actor=3)
    client = daemon.client()
    active = [_begin(client, 0) for _ in range(2)]
    stages = {daemon.stage(begin) for begin in active}
    assert len(stages) == 2 and daemon.staging() == stages
    assert all(stage.stat().st_size == 0 for stage in stages)
    _refused(client, "blob.begin", size=0, contains="total active-upload ceiling of 2 reached")
    assert daemon.staging() == stages and not daemon.objects()

    _one(client, "blob.abort", upload_id=active[0]["upload_id"])
    remaining = daemon.stage(active[1])
    assert daemon.staging() == {remaining}
    replacement = _begin(client, 0)
    assert replacement["upload_id"] not in {begin["upload_id"] for begin in active}
    assert daemon.staging() == {remaining, daemon.stage(replacement)}
    for begin in (active[1], replacement):
        _one(client, "blob.abort", upload_id=begin["upload_id"])
    assert not daemon.staging() and not daemon.objects()


def test_blob_upload_wire_crossing_declared_size_aborts(upload_daemon_factory):
    daemon = upload_daemon_factory()
    client = daemon.client()
    begin = _begin(client, 1)
    stage = daemon.stage(begin)
    _refused(client, "blob.put_part", upload_id=begin["upload_id"], index=0,
             bytes=_b64(b"ab"), contains="invalid input")
    assert not stage.exists() and not daemon.objects()
    _refused(client, "blob.commit", upload_id=begin["upload_id"], contains="unknown upload")


def test_blob_upload_wire_part_limit_formula_exact_and_plus_one(upload_daemon_factory):
    daemon = upload_daemon_factory()
    client = daemon.client()
    ops_cap = _rust_constant("crates/khive-request/src/types.rs", "MAX_OPS_INPUT_LEN")
    frame_cap = _rust_constant("crates/khive-runtime/src/daemon.rs", "MAX_FRAME_BYTES")
    reserve = _rust_constant("crates/khive-pack-blob/src/handlers.rs", "REQUEST_RESERVE")
    assert (ops_cap, frame_cap, reserve) == (MAX_OPS_INPUT_LEN, MAX_FRAME_BYTES, 8192)
    limit = (min(ops_cap, frame_cap) - reserve) * 3 // 4
    begin = _begin(client, limit)
    assert begin["part_limit"] == limit
    payload = b"x" * limit
    ops = encode([op("blob.put_part", upload_id=begin["upload_id"], index=0, bytes=_b64(payload))])
    client.handshake()
    serialized = json.dumps(client._request_frame(ops)).encode()
    assert len(ops.encode()) < ops_cap
    assert len(serialized) - len(ops.encode()) < frame_cap - ops_cap
    assert len(serialized) < frame_cap
    assert client.request(ops)[0]["result"] == {"next_index": 1, "received_bytes": limit}
    assert _one(client, "blob.commit", upload_id=begin["upload_id"]) == {
        "content_ref": blake3(payload).hexdigest(), "size": limit,
    }

    over = _begin(client, limit + 1)
    ops = encode([op("blob.put_part", upload_id=over["upload_id"], index=0, bytes=_b64(payload + b"x"))])
    assert len(ops.encode()) < ops_cap
    entry = client.request(ops)[0]
    assert not entry["ok"] and "invalid input" in entry["error"]["message"].lower(), entry
    assert "decoded length" in entry["error"]["message"].lower(), entry
    assert str(limit + 1) in entry["error"]["message"], entry
    assert str(limit) in entry["error"]["message"], entry
    stage = daemon.stage(over)
    assert stage.stat().st_size == 0
    assert _part(client, over["upload_id"], 0, b"x") == {"next_index": 1, "received_bytes": 1}
    assert stage.read_bytes() == b"x"
    _one(client, "blob.abort", upload_id=over["upload_id"])


def test_blob_upload_wire_oversized_ops_is_parser_refusal(upload_daemon_factory):
    daemon = upload_daemon_factory()
    client = daemon.client()
    begin = _begin(client, 1)
    stage = daemon.stage(begin)
    ops = encode([op("blob.put_part", upload_id=begin["upload_id"], index=0,
                     bytes=_b64(b"x" * MAX_OPS_INPUT_LEN))])
    assert MAX_OPS_INPUT_LEN < len(ops.encode()) < MAX_FRAME_BYTES
    with pytest.raises(RequestRejected) as rejected:
        client.request(ops)
    assert str(MAX_OPS_INPUT_LEN) in str(rejected.value)
    assert stage.stat().st_size == 0
    assert _part(client, begin["upload_id"], 0, b"a") == {"next_index": 1, "received_bytes": 1}


def test_blob_upload_wire_actual_oversized_frame_is_rejected_before_dispatch(upload_daemon_factory):
    daemon = upload_daemon_factory()
    client = daemon.client()
    begin = _begin(client, OBJECT_BYTES)
    stage = daemon.stage(begin)
    payload = b"f" * begin["part_limit"]
    ops = encode([op("blob.put_part", upload_id=begin["upload_id"], index=0, bytes=_b64(payload))])
    assert len(ops.encode()) < MAX_OPS_INPUT_LEN
    client.handshake()
    frame = client._request_frame(ops)
    base_size = len(json.dumps(frame).encode())
    # Every entry is a valid namespace. With the frame guard removed, identity
    # validation cannot hide a dispatch behind an invalid padding string.
    frame["visible_namespaces"] = ["local"] * ((MAX_FRAME_BYTES - base_size) // 9 + 3)
    serialized = json.dumps(frame).encode()
    assert len(serialized) - len(ops.encode()) > MAX_FRAME_BYTES - MAX_OPS_INPUT_LEN
    assert len(serialized) > MAX_FRAME_BYTES
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as wire:
        wire.settimeout(10)
        wire.connect(str(daemon.socket))
        try:
            wire.sendall(struct.pack(">I", len(serialized)) + serialized)
        except (BrokenPipeError, ConnectionResetError):
            pass  # the daemon can close as soon as it reads the actual frame's prefix
        try:
            assert wire.recv(4) == b"", "oversized frame unexpectedly received a response"
        except ConnectionResetError:
            pass
    daemon.assert_alive()
    assert stage.stat().st_size == 0
    assert _part(client, begin["upload_id"], 0, payload) == {
        "next_index": 1, "received_bytes": len(payload),
    }
    print(f"offered serialized frame of {len(serialized)} bytes; cap {MAX_FRAME_BYTES}; no dispatch")


def test_blob_upload_wire_abort_removes_stage_and_record(upload_daemon_factory):
    daemon = upload_daemon_factory()
    client = daemon.client()
    begin = _begin(client, 2)
    _part(client, begin["upload_id"], 0, b"a")
    stage = daemon.stage(begin)
    _one(client, "blob.abort", upload_id=begin["upload_id"])
    assert not stage.exists() and not daemon.staging() and not daemon.objects()
    _refused(client, "blob.commit", upload_id=begin["upload_id"], contains="unknown upload")


@pytest.mark.parametrize("verb", ["blob.put_part", "blob.commit"], ids=["put_part", "commit"])
def test_blob_upload_wire_verb_expiry_without_sweeper(upload_daemon_factory, verb):
    daemon = upload_daemon_factory(idle=1, sweep=NO_SWEEP_SECS)
    client = daemon.client()
    begin = _begin(client, 2 if verb == "blob.put_part" else 1)
    _part(client, begin["upload_id"], 0, b"a")
    stage = daemon.stage(begin)
    time.sleep(daemon.idle + 0.2)
    assert time.monotonic() - daemon.started_at < daemon.sweep
    assert stage.read_bytes() == b"a", "sweep ran before the verb-only expiry assertion"
    args = {"index": 1, "bytes": _b64(b"b")} if verb == "blob.put_part" else {}
    _refused(client, verb, upload_id=begin["upload_id"], contains="unknown upload", **args)
    assert not stage.exists() and not daemon.staging()
    _refused(client, "blob.commit", upload_id=begin["upload_id"], contains="unknown upload")


def test_blob_upload_wire_daemon_expiry_without_verbs_keeps_committed_object(upload_daemon_factory):
    daemon = upload_daemon_factory(idle=2, sweep=1)
    client = daemon.client()
    committed = _one(client, "blob.put", bytes=_b64(b"committed control"))
    permanent = daemon.object(committed["content_ref"])
    begin = _begin(client, 2)
    _part(client, begin["upload_id"], 0, b"a")
    stage = daemon.stage(begin)
    assert stage.read_bytes() == b"a" and permanent.read_bytes() == b"committed control"
    daemon.wait_removed(stage)
    assert permanent.read_bytes() == b"committed control"
    assert not daemon.staging()
    _refused(client, "blob.commit", upload_id=begin["upload_id"], contains="unknown upload")


def test_blob_upload_wire_restart_orphan_sweep_and_begin_again(upload_daemon_factory):
    daemon = upload_daemon_factory(idle=2, sweep=1)
    client = daemon.client()
    begin = _begin(client, 2)
    _part(client, begin["upload_id"], 0, b"a")
    stage = daemon.stage(begin)
    first_pid = daemon.process.pid
    daemon.stop()
    assert stage.read_bytes() == b"a", "shutdown must leave the orphan for restart sweep evidence"
    daemon.start()
    assert daemon.process.pid != first_pid
    client = daemon.client()
    _refused(client, "blob.commit", upload_id=begin["upload_id"], contains="unknown upload")
    daemon.wait_removed(stage)
    assert not daemon.staging()
    result = _upload(client, b"ab")
    assert base64.b64decode(_one(client, "blob.get", content_ref=result["content_ref"])["bytes"]) == b"ab"


def _mcp_response(process, request_id: int) -> dict:
    deadline = time.monotonic() + 45
    while time.monotonic() < deadline:
        assert select.select([process.stdout], [], [], max(0, deadline - time.monotonic()))[0], (
            "MCP child response timed out"
        )
        line = process.stdout.readline()
        assert line, "MCP child closed stdout before its response"
        result = json.loads(line)
        if result.get("id") == request_id:
            return result
    raise AssertionError("MCP child did not return the requested response id")


def test_blob_upload_wire_daemon_owns_expiry_after_mcp_client_exit(upload_daemon_factory):
    daemon = upload_daemon_factory(idle=20, sweep=1)
    stderr_path = daemon.root / "client.stderr"
    with stderr_path.open("wb") as stderr:
        child = subprocess.Popen(
            [daemon.binary, "mcp", *daemon.args()], env=daemon.env, cwd=daemon.root,
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=stderr, bufsize=0,
        )

    def send(message):
        child.stdin.write((json.dumps(message) + "\n").encode())
        child.stdin.flush()

    try:
        send({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": "2024-11-05", "capabilities": {},
            "clientInfo": {"name": "blob-upload-owner-control", "version": "1"},
        }})
        assert "result" in _mcp_response(child, 1)
        send({"jsonrpc": "2.0", "method": "notifications/initialized"})
        send({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {
            "name": "request", "arguments": {
                "ops": encode([op("blob.begin", size=1)]), "format": "json", "presentation": "verbose",
            },
        }})
        response = _mcp_response(child, 2)
        assert "error" not in response and not response["result"].get("isError"), response
        envelope = json.loads(response["result"]["content"][0]["text"])
        entry = envelope["results"][0]
        assert entry["ok"], entry
        begin = entry["result"]
        stage = daemon.stage(begin)
        # The daemon can append this capability before the MCP client exits;
        # that rules out a private upload record in the client's serve process.
        assert _part(daemon.client(), begin["upload_id"], 0, b"a") == {
            "next_index": 1, "received_bytes": 1,
        }
        child.stdin.close()
        assert child.wait(timeout=15) == 0, stderr_path.read_text(errors="replace")
        assert child.pid != daemon.process.pid and stage.read_bytes() == b"a"
        # No RPC or upload verb may run from here to the removal assertion:
        # verb-side expiry would otherwise hide an inert daemon sweeper.
        # Fixture deletion runs only after this test returns (or fails).
        daemon.wait_removed(stage)
        assert child.poll() == 0 and not daemon.staging()
    finally:
        if child.poll() is None:
            child.kill()
            child.wait(timeout=10)


def test_blob_upload_wire_expected_hash_mismatch_discards_stage(upload_daemon_factory):
    daemon = upload_daemon_factory()
    client = daemon.client()
    begin = _begin(client, 1, content_ref=blake3(b"different").hexdigest())
    _part(client, begin["upload_id"], 0, b"a")
    stage = daemon.stage(begin)
    _refused(client, "blob.commit", upload_id=begin["upload_id"], contains="invalid input")
    assert not stage.exists() and not daemon.objects()
    _refused(client, "blob.commit", upload_id=begin["upload_id"], contains="unknown upload")


def test_blob_upload_wire_empty_object_and_empty_part(upload_daemon_factory):
    daemon = upload_daemon_factory()
    client = daemon.client()
    begin = _begin(client, 0)
    assert _part(client, begin["upload_id"], 0, b"") == {"next_index": 1, "received_bytes": 0}
    expected = {"content_ref": blake3(b"").hexdigest(), "size": 0}
    assert _one(client, "blob.commit", upload_id=begin["upload_id"]) == expected
    assert _one(client, "blob.begin", size=0, content_ref=expected["content_ref"]) == expected
    assert not daemon.staging()
