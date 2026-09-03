"""Boots a scratch khived for the test session.

Isolation is by construction: a per-session temp directory holds the
database, socket, and pid file, all pointed at via `KHIVE_SOCKET` /
`KHIVE_PID` / `--db`. The suite never touches `~/.khive`. Requires a
`kkernel` binary on PATH (or `KKERNEL` env override).
"""

from __future__ import annotations

import os
import shutil
import signal
import subprocess
import tempfile
import time
from pathlib import Path

import pytest


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
