"""The native session, CLI and MCP return the same complete plan object."""

from __future__ import annotations

import json
import os
from pathlib import Path
import selectors
import subprocess
import time

import pytest

from conftest import _kkernel
from khive import Session, SocketTransport


TIMEOUT_SECONDS = 30.0
MAX_STDIO_BYTES = 8 * 1024 * 1024
CHAIN = (
    'create(kind="note", content="sample") | '
    'update(id=$prev.id, properties={"nested": '
    '[$prev.properties.owner, {"source": $prev.tags[0]}]}) | '
    'get(id=$prev.id)'
)


def _scratch_environment(scratch):
    root = scratch["root"]
    env = os.environ.copy()
    env.pop("KHIVE_NO_DAEMON", None)
    env.update({
        "KHIVE_SOCKET": str(scratch["socket"]),
        "KHIVE_PID": str(root / "khived.pid"),
        "KHIVE_LOCK": str(root / "khived.recovery.lock"),
        "KHIVE_RECOVERER_LOCK": str(root / "khived.recoverer.lock"),
        "KHIVE_DB": str(root / "scratch.db"),
        "KHIVE_CONFIG": str(root / "khive.toml"),
    })
    return env


def _cli_plan(binary, scratch, ops):
    root = scratch["root"]
    completed = subprocess.run(
        [
            binary, "exec", "--plan", "--db", str(root / "scratch.db"),
            "--config", str(root / "khive.toml"), ops,
        ],
        env=_scratch_environment(scratch),
        cwd=root,
        capture_output=True,
        text=True,
        timeout=TIMEOUT_SECONDS,
        check=False,
    )
    assert completed.returncode == 0, completed.stderr
    return json.loads(completed.stdout)


class StdioMcp:
    def __init__(self, process):
        self.process = process
        self.buffer = bytearray()
        self.sequence = 0
        self.selector = selectors.DefaultSelector()
        self.selector.register(process.stdout, selectors.EVENT_READ)
        os.set_blocking(process.stdout.fileno(), False)

    def close(self):
        self.selector.close()

    def send(self, message):
        self.process.stdin.write((json.dumps(message) + "\n").encode())
        self.process.stdin.flush()

    def notify(self, method):
        self.send({"jsonrpc": "2.0", "method": method})

    def request(self, method, params):
        self.sequence += 1
        request_id = self.sequence
        self.send({"jsonrpc": "2.0", "id": request_id, "method": method, "params": params})
        deadline = time.monotonic() + TIMEOUT_SECONDS
        while True:
            while b"\n" in self.buffer:
                line, _, remaining = self.buffer.partition(b"\n")
                self.buffer = bytearray(remaining)
                message = json.loads(line)
                if message.get("id") == request_id:
                    assert "error" not in message, message.get("error")
                    return message["result"]
            remaining_seconds = deadline - time.monotonic()
            assert remaining_seconds > 0, f"MCP {method} timed out"
            assert self.selector.select(remaining_seconds), f"MCP {method} timed out"
            chunk = os.read(self.process.stdout.fileno(), 65536)
            assert chunk, f"MCP closed stdout during {method}"
            self.buffer.extend(chunk)
            assert len(self.buffer) <= MAX_STDIO_BYTES, "MCP response exceeds the byte budget"


def _stop_process(process):
    try:
        process.stdin.close()
    except BrokenPipeError:
        pass
    try:
        process.wait(timeout=5)
    except subprocess.TimeoutExpired:
        process.terminate()
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=5)
    finally:
        process.stdout.close()


def _mcp_plan(binary, scratch, ops, case):
    root: Path = scratch["root"]
    stderr_path = root / f"plan-mcp-{case}.stderr"
    with stderr_path.open("wb") as stderr:
        process = subprocess.Popen(
            [
                binary, "mcp", "--db", str(root / "scratch.db"),
                "--config", str(root / "khive.toml"),
            ],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=stderr,
            env=_scratch_environment(scratch),
            cwd=root,
        )
        connection = None
        try:
            connection = StdioMcp(process)
            connection.request("initialize", {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "plan-surface-test", "version": "1.0"},
            })
            connection.notify("notifications/initialized")
            result = connection.request("tools/call", {
                "name": "request",
                "arguments": {"ops": ops, "plan": True},
            })
            assert result.get("isError") is not True, result
            text = [entry["text"] for entry in result["content"] if entry["type"] == "text"]
            assert len(text) == 1, result
            return json.loads(text[0])
        except Exception as error:
            stderr.flush()
            diagnostic = stderr_path.read_text(errors="replace")[-2000:]
            raise AssertionError(
                f"MCP plan exchange failed: {error}; stderr: {diagnostic}"
            ) from error
        finally:
            if connection is not None:
                connection.close()
            _stop_process(process)


@pytest.mark.parametrize(
    ("case", "ops"),
    [
        ("nested-chain", CHAIN),
        ("parse-error", "create("),
        ("unknown-verb", "unregistered.plan_example()"),
    ],
    ids=["nested-chain", "parse-error", "unknown-verb"],
)
def test_plan_objects_match_across_all_three_surfaces(scratch_daemon, case, ops):
    binary = _kkernel()
    assert binary is not None, "scratch fixture already selected a kernel binary"
    session = Session(SocketTransport(scratch_daemon["socket"]), timeout=TIMEOUT_SECONDS)
    native = session.plan(ops)
    cli = _cli_plan(binary, scratch_daemon, ops)
    mcp = _mcp_plan(binary, scratch_daemon, ops, case)

    assert native == cli == mcp
    assert set(native["limits"]) >= {"max_ops", "max_depth", "max_input_len"}
    if case == "nested-chain":
        assert native["parsed"] is True
        assert native["mode"] == "chain"
        assert native["stage_count"] == 3
        assert native["stages"][1]["args"]["properties"] == {
            "nested": ["$prev.properties.owner", {"source": "$prev.tags[0]"}],
        }
        assert set(native["stages"][1]["prev_refs"]) == {"id", "properties.owner", "tags.[0]"}
    elif case == "parse-error":
        assert native["parsed"] is False
        assert isinstance(native["error"], str)
        assert "stages" not in native
    else:
        assert native["parsed"] is True
        assert native["stage_count"] == 1
        assert native["stages"][0]["known"] is False
        assert native["stages"][0]["pack"] is None
