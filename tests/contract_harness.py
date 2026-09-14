"""Private stores and bounded child processes for the Python contract suites."""

from __future__ import annotations

import json
import os
from pathlib import Path
import selectors
import subprocess
import tempfile
import time
from typing import Mapping, Sequence

ACTOR = "lambda:contract-test"


class OwnedContractStore:
    """One function's private config, HOME and file-backed databases."""

    def __init__(self, *, actor: str | None = ACTOR,
                 visible_namespaces: Sequence[str] = (), topology: str = ""):
        self._temporary = tempfile.TemporaryDirectory(prefix="khive-contract-")
        self.root = Path(self._temporary.name)
        self.home = self.root / "home"
        self.home.mkdir(mode=0o700)
        self.db = self.root / "contract.db"
        self.config = self.root / "contract.toml"
        self.actor = actor
        self.write_config(visible_namespaces=visible_namespaces, topology=topology)

    def write_config(self, *, visible_namespaces: Sequence[str] = (), topology: str = ""):
        actor = f"id = {json.dumps(self.actor)}\n" if self.actor is not None else ""
        self.config.write_text(
            f"{topology}\n[actor]\n{actor}"
            f"visible_namespaces = {json.dumps(list(visible_namespaces))}\n\n"
            f"[gate]\ngranted_actors = [{json.dumps(ACTOR)}]\ngrant_unattributed = false\n",
            encoding="utf-8",
        )

    def child_env(self, source: Mapping[str, str] | None = None) -> dict[str, str]:
        source = os.environ if source is None else source
        env = {key: value for key, value in source.items() if not key.startswith("KHIVE_")}
        env.update(HOME=str(self.home), KHIVE_CONFIG=str(self.config), KHIVE_NO_DAEMON="1")
        if self.actor is not None:
            env["KHIVE_ACTOR"] = self.actor
        return env

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        self.close()

    def close(self):
        self._temporary.cleanup()


def reap_child(proc: subprocess.Popen, *, timeout: float = 5, force: bool = False):
    """Finish this owned child before closing its pipes or releasing its handle."""
    if force and proc.poll() is None:
        proc.kill()
    if proc.stdin is not None and not proc.stdin.closed:
        proc.stdin.close()
    try:
        proc.wait(timeout=timeout)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait(timeout=timeout)
    for pipe in (proc.stdout, proc.stderr):
        if pipe is not None:
            pipe.close()


class JsonRpcTransport:
    """Nonblocking pipes with one absolute deadline for each JSON-RPC exchange.

    Partial lines, notifications and unrelated IDs consume the same deadline.
    stderr is drained alongside stdout so diagnostic output cannot park a child.
    """

    def __init__(self, proc: subprocess.Popen, timeout: float):
        if timeout <= 0:
            raise ValueError("timeout must be positive")
        self.proc, self.timeout = proc, timeout
        self.selector = selectors.DefaultSelector()
        self.buffer = bytearray()
        self.stderr = bytearray()
        self.deadline = 0.0
        self.expected_id = None
        for pipe, name in ((proc.stdout, "stdout"), (proc.stderr, "stderr")):
            os.set_blocking(pipe.fileno(), False)
            self.selector.register(pipe, selectors.EVENT_READ, name)
        os.set_blocking(proc.stdin.fileno(), False)

    def _remaining(self):
        remaining = self.deadline - time.monotonic()
        if remaining <= 0:
            raise TimeoutError(f"MCP exchange exceeded {self.timeout:g}s")
        return remaining

    def _pump(self):
        events = self.selector.select(self._remaining())
        for key, _ in events:
            if key.data == "stdin":
                continue
            chunk = os.read(key.fd, 65536)
            if not chunk:
                self.selector.unregister(key.fileobj)
                if key.data == "stdout":
                    raise EOFError("MCP server closed stdout unexpectedly")
            elif key.data == "stderr":
                self.stderr.extend(chunk)
                del self.stderr[:-4096]
            else:
                self.buffer.extend(chunk)

    def send(self, message: dict):
        self.deadline = time.monotonic() + self.timeout
        self.expected_id = message.get("id")
        data = memoryview((json.dumps(message) + "\n").encode())
        pipe = self.proc.stdin
        self.selector.register(pipe, selectors.EVENT_WRITE, "stdin")
        try:
            while data:
                self._remaining()
                try:
                    written = os.write(pipe.fileno(), data)
                    data = data[written:]
                except BlockingIOError:
                    self._pump()
        except BaseException:
            self.close(force=True)
            raise
        finally:
            if self.selector.get_map() and pipe in self.selector.get_map():
                self.selector.unregister(pipe)

    def response(self, expected_id=None):
        expected_id = self.expected_id if expected_id is None else expected_id
        try:
            while True:
                self._remaining()
                if b"\n" not in self.buffer:
                    self._pump()
                    continue
                line, _, rest = self.buffer.partition(b"\n")
                self.buffer = bytearray(rest)
                message = json.loads(line)
                if not isinstance(message, dict):
                    raise ValueError("MCP response must be an object")
                if "id" in message and message["id"] == expected_id:
                    return message
        except BaseException:
            self.close(force=True)
            raise

    def close(self, *, force: bool = False):
        reap_child(self.proc, timeout=self.timeout, force=force)
        self.selector.close()


def attach_transport(proc: subprocess.Popen, timeout: float = 10) -> JsonRpcTransport:
    transport = JsonRpcTransport(proc, timeout)
    proc.contract_transport = transport
    return transport
