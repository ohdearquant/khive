"""Requests wait for the supervised events daemon before they dispatch.

A daemon with events forwarding commits every write's audit batch through the
events daemon it spawns after binding its own socket. These tests make that
child bind late (test-harness settings, honoured only under
KHIVE_TEST_HARNESS=1) and send the first write as soon as the main socket
exists, the way a client that just started the daemon does.

Run with KKERNEL pointing to this checkout's built executable. There is no
binary fallback and no skip path.
"""

from __future__ import annotations

import os
from pathlib import Path
import shutil
import signal
import socket
import subprocess
import tempfile
import time
import uuid

import pytest

from khive.errors import RequestRejected
from khive.ops import encode, op
from khive.transport import Session, SocketTransport


class _LateEventsDaemon:
    def __init__(self, binary: Path, root: Path, *, bind_delay_ms: int, window_ms: int | None):
        self.root = root
        self.socket = root / "khived.sock"
        self.db = root / "scratch.db"
        # The events database sits beside the main one, and its socket beside it.
        self.events_socket = root / "scratch.db.events.sock"
        config = root / "khive.toml"
        config.write_text('[storage.blob]\nbackend = "fs"\nfloor_bytes = 0\n')
        env = {key: value for key, value in os.environ.items() if not key.startswith("KHIVE_")}
        env.update(
            KHIVE_SOCKET=str(self.socket),
            KHIVE_PID=str(root / "khived.pid"),
            KHIVE_LOCK=str(root / "khived.recovery.lock"),
            KHIVE_RECOVERER_LOCK=str(root / "khived.recoverer.lock"),
            KHIVE_PACKS="kg",
            KHIVE_TEST_HARNESS="1",
            KHIVE_TEST_EVENTS_DAEMON_BIND_DELAY_MS=str(bind_delay_ms),
        )
        if window_ms is not None:
            env["KHIVE_TEST_EVENTS_READY_WINDOW_MS"] = str(window_ms)
        self.stderr = root / "daemon.stderr"
        with self.stderr.open("wb") as stderr:
            self.process = subprocess.Popen(
                [
                    str(binary),
                    "mcp",
                    "--daemon",
                    "--config",
                    str(config),
                    "--db",
                    str(self.db),
                    "--no-embed",
                ],
                env=env,
                cwd=root,
                stdout=subprocess.DEVNULL,
                stderr=stderr,
            )
        # Wait only until the main socket accepts a connection: bind creates the file just
        # before listen, so a connect in between is refused.
        deadline = time.monotonic() + 45
        while True:
            self.assert_alive()
            assert time.monotonic() < deadline, f"main socket never accepted: {self.logs()}"
            if self.socket.exists():
                with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as probe:
                    try:
                        probe.connect(str(self.socket))
                        break
                    except (ConnectionRefusedError, FileNotFoundError):
                        pass
            time.sleep(0.01)

    def client(self) -> Session:
        return Session(SocketTransport(self.socket), actor_id="test:events:barrier", timeout=60)

    def logs(self) -> str:
        return self.stderr.read_text(errors="replace")[-4000:]

    def assert_alive(self):
        assert self.process.poll() is None, self.logs()

    def stop(self):
        if self.process.poll() is None:
            self.process.send_signal(signal.SIGTERM)
            try:
                self.process.wait(timeout=15)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=10)


@pytest.fixture
def late_events_daemon(request):
    selected = os.environ.get("KKERNEL")
    assert selected, "set KKERNEL to this checkout's built executable; no installed fallback"
    binary = Path(selected).resolve()
    assert binary.is_file() and os.access(binary, os.X_OK), binary
    daemons = []
    # conftest.py attaches each root's daemon stderr to the report of a failed test.
    request.node.scratch_daemon_roots = []

    def create(*, bind_delay_ms: int, window_ms: int | None = None) -> _LateEventsDaemon:
        # /tmp keeps both the main and the derived events socket paths below macOS's cap.
        root = Path(tempfile.mkdtemp(prefix="events-barrier-", dir="/tmp"))
        request.node.scratch_daemon_roots.append(root)
        daemon = _LateEventsDaemon(binary, root, bind_delay_ms=bind_delay_ms, window_ms=window_ms)
        daemons.append(daemon)
        return daemon

    yield create
    for daemon in reversed(daemons):
        daemon.stop()
        shutil.rmtree(daemon.root, ignore_errors=True)


def _create_concept(client: Session, name: str) -> dict:
    results = client.request(encode([op("create", kind="concept", name=name)]))
    assert len(results) == 1 and results[0]["tool"] == "create", results
    return results[0]


def _find_concepts(client: Session, name: str) -> list[dict]:
    results = client.request(encode([op("list", kind="concept", limit=50)]))
    assert len(results) == 1 and results[0]["ok"], results
    payload = results[0]["result"]
    if isinstance(payload, dict):
        payload = payload.get("items", payload.get("entities", payload.get("results")))
    assert isinstance(payload, list), results
    return [item for item in payload if item.get("name") == name]


def test_first_write_after_the_main_socket_appears_waits_for_the_events_daemon(late_events_daemon):
    daemon = late_events_daemon(bind_delay_ms=3000)
    assert not daemon.events_socket.exists(), (
        "the write must be sent before the events daemon binds"
    )
    name = f"barrier-{uuid.uuid4().hex}"

    created = _create_concept(daemon.client(), name)

    assert created["ok"], (created, daemon.logs())
    assert daemon.events_socket.exists(), "the write was answered before the events daemon bound"
    assert [item["id"] for item in _find_concepts(daemon.client(), name)] == [
        created["result"]["id"]
    ]


def test_write_is_refused_uncommitted_when_the_events_daemon_misses_the_startup_window(
    late_events_daemon,
):
    daemon = late_events_daemon(bind_delay_ms=4000, window_ms=300)
    assert not daemon.events_socket.exists(), (
        "the write must be sent before the events daemon binds"
    )
    name = f"barrier-{uuid.uuid4().hex}"

    with pytest.raises(RequestRejected) as refused:
        _create_concept(daemon.client(), name)

    detail = refused.value.error_detail
    assert detail is not None, refused.value
    assert detail.code == "events_daemon_unavailable", detail
    assert detail.domain_disposition == "not_committed", detail
    assert detail.retryable is True, detail
    # The daemon may report the socket through a resolved directory (macOS
    # reaches /tmp as /private/tmp), so compare resolved paths and accept the
    # message naming either spelling.
    expected_socket = daemon.events_socket.resolve()
    assert Path(detail.model_extra["socket"]).resolve() == expected_socket, detail
    assert (
        str(expected_socket) in detail.message
        or str(daemon.events_socket) in detail.message
    ), detail

    # Once the late child answers, the gate reopens and requests dispatch again.
    deadline = time.monotonic() + 30
    while True:
        daemon.assert_alive()
        try:
            found = _find_concepts(daemon.client(), name)
            break
        except RequestRejected as exc:
            assert (
                exc.error_detail is not None
                and exc.error_detail.code == "events_daemon_unavailable"
            ), exc
            assert time.monotonic() < deadline, f"gate never reopened: {daemon.logs()}"
            time.sleep(0.2)
    assert found == [], "the refused write left a committed entity behind"
    retried = _create_concept(daemon.client(), name)
    assert retried["ok"], (retried, daemon.logs())
    # The same lookup finds the retried write, so the empty result above was an absence.
    assert [item["id"] for item in _find_concepts(daemon.client(), name)] == [
        retried["result"]["id"]
    ]
