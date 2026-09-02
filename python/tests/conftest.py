"""Boots a scratch khived for the test session, and fake khive-cloud servers.

Isolation is by construction: a per-session temp directory holds the
database, socket, and pid file, all pointed at via `KHIVE_SOCKET` /
`KHIVE_PID` / `--db`. The suite never touches `~/.khive`. Requires a
`kkernel` binary on PATH (or `KKERNEL` env override).

The `rest_server`/`mcp_server` fixtures below fake khive-cloud's two
transports offline: a real REST endpoint over `http.server`, and a real MCP
`request` tool over a streamable-HTTP `uvicorn` server — both gated by the
same `Authorization: ApiKey <key>` scheme as the live deployment. Neither
import their optional dependency (`uvicorn`, `mcp`'s `FastMCP`) at module
scope, so collecting this file never requires the `cloud` extra; only
actually requesting `mcp_server` does.
"""

from __future__ import annotations

import json
import os
import shutil
import signal
import socket
import subprocess
import tempfile
import threading
import time
from collections.abc import Iterator
from dataclasses import dataclass
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any

import pytest

API_KEY = "test-key-123"


def _dispatch_ops(ops: str) -> tuple[int, dict[str, Any]]:
    """A tiny fake op-dispatcher mirroring khive-cloud's envelope shape.

    Parses the JSON array form `[{"tool": ..., "args": {...}}, ...]` (the
    only encoding this client emits) and answers a handful of well-known
    tool names; `rate_limited`/`boom` simulate whole-request HTTP failures
    (429/500) rather than a per-op error, since those are transport-level,
    not dispatch-level, on the real server.
    """
    try:
        parsed = json.loads(ops) if ops else None
    except ValueError:
        parsed = None
    if not isinstance(parsed, list) or not parsed:
        return 400, {"error": f"unparseable ops: {ops!r}"}
    results: list[dict[str, Any]] = []
    for entry in parsed:
        tool = entry.get("tool") if isinstance(entry, dict) else None
        if tool == "stats":
            results.append(
                {"ok": True, "tool": "stats", "result": {"entities": 1, "edges": 0, "notes": 0}}
            )
        elif tool == "whoami":
            results.append({"ok": True, "tool": "whoami", "result": {"namespace": "local"}})
        elif tool == "search":
            results.append({"ok": True, "tool": "search", "result": {"items": []}})
        elif tool == "nope":
            results.append(
                {
                    "ok": False,
                    "tool": "nope",
                    "error": {"code": "verb_not_found", "message": "unknown verb 'nope'"},
                }
            )
        elif tool == "later":
            results.append({"ok": False, "tool": "later", "aborted": True})
        elif tool == "rate_limited":
            return 429, {"error": "rate limit exceeded"}
        elif tool == "boom":
            return 500, {"error": "internal error"}
        else:
            results.append(
                {
                    "ok": False,
                    "tool": tool,
                    "error": {"code": "verb_not_found", "message": f"unknown verb {tool!r}"},
                }
            )
    succeeded = sum(1 for r in results if r.get("ok"))
    failed = sum(1 for r in results if not r.get("ok") and not r.get("aborted"))
    aborted = sum(1 for r in results if r.get("aborted"))
    envelope = {
        "results": results,
        "summary": {
            "total": len(results),
            "succeeded": succeeded,
            "failed": failed,
            "aborted": aborted,
        },
    }
    return 200, envelope


class _RestHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, format: str, *args: object) -> None:
        pass  # keep test output quiet

    def _authorized(self) -> bool:
        return self.headers.get("Authorization") == f"ApiKey {API_KEY}"

    def _send_json(self, status: int, payload: dict) -> None:
        body = json.dumps(payload).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self) -> None:
        if self.path == "/health":
            self._send_json(200, {"status": "ok"})
            return
        self._send_json(404, {"error": "not found"})

    def do_POST(self) -> None:
        if self.path != "/v1/request":
            self._send_json(404, {"error": "not found"})
            return
        if not self._authorized():
            self._send_json(401, {"error": "unauthorized"})
            return
        length = int(self.headers.get("Content-Length", "0"))
        raw = self.rfile.read(length) if length else b"{}"
        try:
            body = json.loads(raw)
        except ValueError:
            self._send_json(400, {"error": "malformed JSON body"})
            return
        status, payload = _dispatch_ops(body.get("ops", ""))
        self._send_json(status, payload)


@dataclass
class RestServer:
    _server: ThreadingHTTPServer
    _thread: threading.Thread

    @property
    def url(self) -> str:
        port = self._server.server_address[1]
        return f"http://127.0.0.1:{port}"

    def stop(self) -> None:
        self._server.shutdown()
        self._thread.join(timeout=5)


@pytest.fixture
def rest_server() -> Iterator[RestServer]:
    server = ThreadingHTTPServer(("127.0.0.1", 0), _RestHandler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    wrapper = RestServer(server, thread)
    try:
        yield wrapper
    finally:
        wrapper.stop()


@pytest.fixture
def api_key() -> str:
    return API_KEY


def _free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def _build_mcp_app(expected_key: str):
    from mcp.server.fastmcp import FastMCP
    from starlette.applications import Starlette
    from starlette.responses import JSONResponse

    app = FastMCP("khive-cloud-fake", stateless_http=True)

    @app.tool()
    def request(ops: str) -> str:
        """Fake `request` tool mirroring `_dispatch_ops`'s canned envelopes."""
        _status, payload = _dispatch_ops(ops)
        return json.dumps(payload)

    inner = app.streamable_http_app()

    class _AuthGate:
        def __init__(self, wrapped: Starlette) -> None:
            self._wrapped = wrapped
            self._expected = f"ApiKey {expected_key}".encode()

        async def __call__(self, scope, receive, send) -> None:
            if scope["type"] == "http":
                headers = dict(scope.get("headers") or [])
                if headers.get(b"authorization") != self._expected:
                    response = JSONResponse({"error": "unauthorized"}, status_code=401)
                    await response(scope, receive, send)
                    return
            await self._wrapped(scope, receive, send)

    return _AuthGate(inner)


@dataclass
class McpServer:
    port: int
    _server: object
    _thread: threading.Thread

    @property
    def url(self) -> str:
        return f"http://127.0.0.1:{self.port}"

    def stop(self) -> None:
        self._server.should_exit = True
        self._thread.join(timeout=5)


@pytest.fixture
def mcp_server() -> Iterator[McpServer]:
    uvicorn = pytest.importorskip("uvicorn")
    pytest.importorskip("mcp")

    app = _build_mcp_app(API_KEY)
    port = _free_port()
    config = uvicorn.Config(app, host="127.0.0.1", port=port, log_level="error", lifespan="on")
    server = uvicorn.Server(config)
    thread = threading.Thread(target=server.run, daemon=True)
    thread.start()

    for _ in range(200):
        if getattr(server, "started", False):
            break
        time.sleep(0.02)
    wrapper = McpServer(port, server, thread)
    try:
        yield wrapper
    finally:
        wrapper.stop()


def _kkernel() -> str | None:
    env = os.environ.get("KKERNEL")
    if env:
        return env
    return shutil.which("kkernel") or (
        str(Path.home() / ".cargo/bin/kkernel")
        if (Path.home() / ".cargo/bin/kkernel").exists()
        else None
    )


@pytest.fixture(scope="session")
def scratch_daemon():
    binary = _kkernel()
    if binary is None:
        pytest.skip("no kkernel binary found (set KKERNEL)")
    # Deliberately NOT pytest's tmp_path: macOS caps AF_UNIX socket paths at
    # ~104 bytes, and the derived events socket ("<db>.events.sock") under
    # pytest's /private/var/folders/... tree exceeds it — the events daemon
    # then cannot bind and every dispatch fails its audit commit.
    root = Path(tempfile.mkdtemp(prefix="khived-", dir="/tmp"))
    sock = root / "khived.sock"
    # An empty explicit config keeps the daemon off any user-level
    # ~/.khive/config.toml, whose declared backends both reject `--db`
    # overrides and would otherwise aim the scratch daemon at real stores.
    config = root / "khive.toml"
    config.write_text("")
    env = os.environ.copy()
    env["KHIVE_SOCKET"] = str(sock)
    env["KHIVE_PID"] = str(root / "khived.pid")
    proc = subprocess.Popen(
        [
            binary,
            "mcp",
            "--daemon",
            "--config",
            str(config),
            "--db",
            str(root / "scratch.db"),
        ],
        env=env,
        stdout=subprocess.DEVNULL,
        stderr=(root / "daemon.stderr").open("wb"),
        cwd=str(root),
    )
    # Readiness = a successful dispatch, not socket existence: the daemon
    # accepts its main socket before the supervised events daemon is taking
    # audit writes, and every dispatch fails until that lane is up.
    from khive import Khive

    deadline = time.monotonic() + 30
    ready = False
    last_error: Exception | None = None
    while time.monotonic() < deadline:
        if proc.poll() is not None:
            stderr = (root / "daemon.stderr").read_text(errors="replace")[-2000:]
            pytest.fail(f"scratch daemon exited rc={proc.returncode}: {stderr}")
        if sock.exists():
            try:
                Khive(socket_path=str(sock), timeout=5.0).stats()
                ready = True
                break
            except Exception as exc:  # noqa: BLE001 — retried until deadline
                last_error = exc
        time.sleep(0.2)
    if not ready:
        proc.kill()
        pytest.fail(f"scratch daemon never served a dispatch within 30s: {last_error}")
    yield {"socket": sock, "root": root}
    proc.send_signal(signal.SIGTERM)
    try:
        proc.wait(timeout=15)
    except subprocess.TimeoutExpired:
        proc.kill()
    shutil.rmtree(root, ignore_errors=True)
