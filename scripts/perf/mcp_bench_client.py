#!/usr/bin/env python3
"""Shared stdio-MCP / daemon client plumbing for the perf scripts (stdlib-only).

Extracted from `bench_pipeline_daemon.py` (the stdio MCP handshake/`tools/call`
layer, daemon-engagement proof, live-DB safety guard, and process teardown)
and from `bench_load_harness.py` (the raw daemon-socket frame protocol used
by its metrics-snapshot oracle channel), per the benchmark-overhaul DESIGN.md
PR 2 slice ("one reusable real-MCP client and daemon lifecycle module").

Both scripts import this module instead of re-implementing the same wire
protocol twice. This is a pure extraction: no CLI, no output shape, and no
existing script's observable behavior changes as a result of this module
existing. `bench_pipeline_daemon.py` and `bench_load_harness.py` re-export
the names their own module-level code already referenced (e.g.
`bench_pipeline_daemon._call_verb`), so neither script's internals had to
change beyond the import.

Three layers live here:

  1. Stdio MCP transport: JSON-RPC `initialize`/`tools/call` framing over a
     subprocess's stdin/stdout pipes (`handshake`, `call_request`,
     `call_verb`).
  2. Daemon-engagement proof: positively confirms a spawned front-end
     routed traffic through a live `kkernel mcp --daemon` child bound to the
     expected bench socket/DB, per the Coverage definition's rule that a
     local-dispatch fallback is a failed scenario, not a sample
     (`assert_daemon_engaged`, `assert_no_daemon_spawned`).
  3. Raw daemon-socket framing: speaks the daemon's length-prefixed Unix
     socket protocol directly (`raw_daemon_roundtrip`, `base_daemon_frame`),
     used for the F10 diagnostic frame floor and the `metrics_only` gauge
     snapshot. This bypasses the stdio front-end entirely, so it is a lower
     attribution layer, not a Coverage-definition-compliant E2E measurement
     (see `measure_concurrent_frames`'s docstring).

`measure_concurrent_frames` and its two convenience wrappers
(`measure_probe_only_floor`, `measure_stats_dispatch_floor`) are new in this
module: deadline-aware concurrent raw-frame requests with timeout censoring,
the F10 "raw frame control" and "MCP `stats()` floor" pieces from
DESIGN.md's PR 2 slice that do not require PR 1's flagship manifest/schema
files. They return a plain percentile/timeout dict, not the versioned
`FlagshipRecord`/`Distribution` contract (that schema is owned by PR 1's
`schemas/flagship-result-v1.json`).
"""

from __future__ import annotations

import concurrent.futures
import json
import os
import pathlib
import signal
import socket as socketlib
import struct
import subprocess
import sys
import time

# ── Wire protocol constants (mirrors crates/khive-runtime/src/daemon.rs) ──────

PROTOCOL_VERSION = 3

# Default production pack set (must match RuntimeConfig::default().packs in
# crates/khive-runtime/src/config.rs so config_id agrees between front-end
# and daemon child).
DEFAULT_PACKS = "kg,gtd,memory,brain,comm,schedule,knowledge,session,code,workspace"

# ── Live-DB safety guard ──────────────────────────────────────────────────────

LIVE_DB_PATHS = frozenset([
    os.path.expanduser("~/.khive/khive.db"),
    os.path.expanduser("~/.khive/khive-graph.db"),
])


def assert_not_live_db(path):
    """Exit the process if `path` resolves to a live production DB location."""
    resolved = str(pathlib.Path(path).resolve())
    for live in LIVE_DB_PATHS:
        live_resolved = str(pathlib.Path(live).resolve())
        if resolved == live_resolved or resolved.startswith(str(pathlib.Path(live).parent.resolve())):
            print(f"FATAL: bench DB path {path!r} resolves to live DB location. Aborting.", file=sys.stderr)
            sys.exit(2)


# ── Stdio MCP transport ───────────────────────────────────────────────────────

_request_id = 0


def _next_id():
    global _request_id
    _request_id += 1
    return _request_id


def send(proc, method, params=None):
    """Write one JSON-RPC request line to `proc`'s stdin."""
    msg = {"jsonrpc": "2.0", "id": _next_id(), "method": method}
    if params is not None:
        msg["params"] = params
    proc.stdin.write((json.dumps(msg) + "\n").encode())
    proc.stdin.flush()


def recv(proc):
    """Read and decode one JSON-RPC response line from `proc`'s stdout."""
    line = proc.stdout.readline()
    if not line:
        raise RuntimeError("MCP binary closed stdout unexpectedly")
    return json.loads(line)


def call_request(proc, ops_string):
    """Send one `request(ops=ops_string)` `tools/call` and return the decoded body."""
    send(proc, "tools/call", {"name": "request", "arguments": {"ops": ops_string}})
    resp = recv(proc)
    if "error" in resp:
        raise RuntimeError(f"MCP RPC error: {resp['error']}")
    result = resp.get("result", {})
    if result.get("isError"):
        content = result.get("content", [])
        text = content[0]["text"] if content else "(no text)"
        raise RuntimeError(f"request returned protocol error: {text}")
    content = result.get("content", [])
    text = content[0]["text"] if content else ""
    return json.loads(text) if text else None


def call_verb(proc, verb, args):
    """Call a single verb through `request` and return its unwrapped result."""
    ops = json.dumps([{"tool": verb, "args": args}])
    body = call_request(proc, ops)
    if body is None:
        raise RuntimeError(f"Empty response for verb {verb}")
    results = body.get("results") or []
    if not results:
        raise RuntimeError(f"No results in response for verb {verb}: {body}")
    first = results[0]
    if not first.get("ok", False):
        raise RuntimeError(f"Verb {verb} failed: {first.get('error', '<no error>')}")
    return first.get("result")


def handshake(proc, client_name="bench-pipeline", client_version="1.0.0"):
    """Perform the MCP `initialize` / `notifications/initialized` round trip."""
    send(proc, "initialize", {
        "protocolVersion": "2024-11-05",
        "capabilities": {},
        "clientInfo": {"name": client_name, "version": client_version},
    })
    init = recv(proc)
    if "error" in init:
        raise RuntimeError(f"initialize failed: {init['error']}")
    notify = {"jsonrpc": "2.0", "method": "notifications/initialized"}
    proc.stdin.write((json.dumps(notify) + "\n").encode())
    proc.stdin.flush()


# ── Daemon-engagement proof ────────────────────────────────────────────────────

def read_pid_file(pid_path):
    """Return (pid:int, raw:str) from pid file, or (None, None) if absent/bad."""
    try:
        raw = pathlib.Path(pid_path).read_text().strip()
        return int(raw), raw
    except Exception:
        return None, None


def pid_alive(pid):
    """Return True if the process is alive (signal 0)."""
    try:
        os.kill(pid, 0)
        return True
    except (ProcessLookupError, PermissionError):
        return False


def argv_is_khive_daemon(pid):
    """Return True if ps shows this PID running kkernel (or kkernel-bench) mcp --daemon."""
    try:
        out = subprocess.check_output(
            ["ps", "-p", str(pid), "-o", "args="],
            stderr=subprocess.DEVNULL,
        ).decode().strip()
        tokens = out.split()
        if not tokens:
            return False
        basename = os.path.basename(tokens[0])
        # Accept kkernel (production) or kkernel-bench (bench binary, which spawns
        # itself as the daemon via current_exe() in spawn_daemon()).
        if basename not in ("kkernel", "kkernel-bench"):
            return False
        rest = tokens[1:]
        return "mcp" in rest and "--daemon" in rest
    except Exception:
        return False


def lsof_has_bench_db(pid, bench_db):
    """Return True if the process has bench_db open (best-effort, skips if lsof absent)."""
    try:
        out = subprocess.check_output(
            ["lsof", "-p", str(pid)],
            stderr=subprocess.DEVNULL,
        ).decode()
        bench_db_real = str(pathlib.Path(bench_db).resolve())
        return bench_db_real in out
    except FileNotFoundError:
        print("[SKIP] lsof not available, skipping open-file sub-check", flush=True)
        return None  # None means skipped, not False
    except Exception:
        return False


def assert_daemon_engaged(sock_path, pid_path, bench_db, label="main"):
    """Assert all three daemon-engagement checks pass. Exit non-zero on failure."""
    errors = []

    # Check 1: socket file exists
    if not pathlib.Path(sock_path).exists():
        errors.append(
            f"[DAEMON-CHECK-{label}] FAIL: bench socket {sock_path!r} does not exist. "
            "The front-end must have silently fallen back to local dispatch."
        )
    else:
        print(f"[DAEMON-CHECK-{label}] PASS: bench socket {sock_path!r} exists", flush=True)

    # Check 2: PID file exists, PID is alive, and argv is kkernel mcp --daemon
    pid, _ = read_pid_file(pid_path)
    if pid is None:
        errors.append(
            f"[DAEMON-CHECK-{label}] FAIL: bench PID file {pid_path!r} absent or unreadable."
        )
    elif not pid_alive(pid):
        errors.append(
            f"[DAEMON-CHECK-{label}] FAIL: PID {pid} from {pid_path!r} is not alive."
        )
    elif not argv_is_khive_daemon(pid):
        try:
            argv_out = subprocess.check_output(
                ["ps", "-p", str(pid), "-o", "args="], stderr=subprocess.DEVNULL
            ).decode().strip()
        except Exception:
            argv_out = "<ps failed>"
        errors.append(
            f"[DAEMON-CHECK-{label}] FAIL: PID {pid} is alive but argv does not match "
            f"'kkernel mcp --daemon'. Got: {argv_out!r}"
        )
    else:
        print(
            f"[DAEMON-CHECK-{label}] PASS: PID {pid} is a live kkernel mcp --daemon process",
            flush=True,
        )

        # Check 3: that daemon has bench.db open (best-effort)
        db_open = lsof_has_bench_db(pid, bench_db)
        if db_open is None:
            pass  # skipped
        elif db_open:
            print(
                f"[DAEMON-CHECK-{label}] PASS: PID {pid} has {bench_db!r} open (lsof confirmed)",
                flush=True,
            )
        else:
            errors.append(
                f"[DAEMON-CHECK-{label}] FAIL: PID {pid} is 'kkernel mcp --daemon' but does NOT "
                f"have {bench_db!r} open. It may be attached to the live DB instead."
            )

    if errors:
        for msg in errors:
            print(msg, file=sys.stderr, flush=True)
        print(
            f"FATAL: daemon-engagement assertions failed ({label} run). "
            "The gate CANNOT rely on this run's P@K. Exiting.",
            file=sys.stderr,
            flush=True,
        )
        sys.exit(1)


def assert_no_daemon_spawned(sock_path, label="nodaemon"):
    """Assert no bench daemon was spawned (KHIVE_NO_DAEMON control run)."""
    if pathlib.Path(sock_path).exists():
        print(
            f"[DAEMON-CHECK-{label}] FAIL: bench socket {sock_path!r} exists when "
            "KHIVE_NO_DAEMON=1 was set: something spawned a daemon unexpectedly.",
            file=sys.stderr,
            flush=True,
        )
        sys.exit(1)
    print(
        f"[DAEMON-CHECK-{label}] PASS: no bench socket with KHIVE_NO_DAEMON=1 (correct)",
        flush=True,
    )


def teardown_daemon(pid_path, tmpdir):
    """SIGTERM the bench daemon (if any), then rmtree the tmpdir."""
    import shutil
    pid, _ = read_pid_file(pid_path)
    if pid is not None and pid_alive(pid) and argv_is_khive_daemon(pid):
        try:
            os.kill(pid, signal.SIGTERM)
            # Give it a moment to exit cleanly.
            for _ in range(20):
                time.sleep(0.1)
                if not pid_alive(pid):
                    break
        except Exception:
            pass
    try:
        shutil.rmtree(tmpdir, ignore_errors=True)
    except Exception:
        pass


# ── Percentile ────────────────────────────────────────────────────────────────

def pct(sorted_list, p):
    """Nearest-rank percentile over an already-sorted list (empty -> 0.0)."""
    if not sorted_list:
        return 0.0
    idx = min(int(len(sorted_list) * p), len(sorted_list) - 1)
    return sorted_list[idx]


# ── Raw daemon-socket framing (F10 diagnostic / metrics-snapshot channel) ────
#
# Every other function in this module goes through the front-end's stdio MCP
# surface. This section speaks the daemon's length-prefixed Unix-socket wire
# protocol directly, for two reasons neither the stdio surface nor a single
# front-end subprocess can provide:
#   - a `metrics_only` gauge-snapshot request (no MCP verb wraps this frame
#     field; it is a read-only measurement surface, not a product verb), and
#   - genuinely concurrent overlapping requests (one stdio subprocess serves
#     one synchronous request/response pipe; overlapping raw socket
#     connections do not have that limitation).


def recv_exact(sock: socketlib.socket, n: int, deadline: float | None = None) -> bytes:
    """Read exactly `n` bytes from `sock`.

    If `deadline` (a `time.monotonic()` timestamp) is given, each individual
    `recv()` call is bounded by the REMAINING budget rather than a fixed
    per-call timeout. `socket.settimeout` governs one blocking call at a
    time, so a socket that drip-feeds a response in small pieces would
    otherwise get a fresh full timeout window on every partial read and
    could hold the caller open arbitrarily long past the caller's actual
    deadline.
    """
    buf = b""
    while len(buf) < n:
        if deadline is not None:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise socketlib.timeout("whole-request deadline exceeded while assembling frame")
            sock.settimeout(remaining)
        chunk = sock.recv(n - len(buf))
        if not chunk:
            raise RuntimeError("daemon socket closed mid-frame")
        buf += chunk
    return buf


def raw_daemon_roundtrip(sock_path: str, frame: dict, timeout_s: float = 5.0) -> dict:
    """Send one length-prefixed JSON frame directly over the daemon's Unix
    socket and return the decoded response frame. Raises `socket.timeout` if
    the complete round trip (connect, send, and every partial recv needed to
    assemble the frame) does not finish within `timeout_s` of this call's
    start. The budget is a single absolute deadline for the whole request,
    not a per-socket-operation timeout: `recv_exact` is handed the
    remaining time on every call so a slow, drip-feeding peer cannot reset
    the clock on each partial read.
    """
    deadline = time.monotonic() + timeout_s
    s = socketlib.socket(socketlib.AF_UNIX, socketlib.SOCK_STREAM)
    try:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise socketlib.timeout("whole-request deadline exceeded before connect")
        s.settimeout(remaining)
        s.connect(sock_path)

        payload = json.dumps(frame).encode()
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise socketlib.timeout("whole-request deadline exceeded before send")
        s.settimeout(remaining)
        s.sendall(struct.pack(">I", len(payload)) + payload)

        len_buf = recv_exact(s, 4, deadline=deadline)
        (length,) = struct.unpack(">I", len_buf)
        raw = recv_exact(s, length, deadline=deadline)
        return json.loads(raw)
    finally:
        s.close()


def base_daemon_frame(
    ops: str,
    config_id: str,
    probe_only: bool = False,
    metrics_only: bool = False,
    request_id: int | None = None,
) -> dict:
    """Build a DaemonRequestFrame JSON payload. `presentation` /
    `presentation_per_op` / `namespace` have no serde default on the Rust
    struct (unlike the rest), so they must always be present in the wire
    payload or the daemon silently drops the connection.

    `request_id` (khive#948) is the caller's own process-local monotonic
    counter value for this request, echoed back on the response and stamped
    into the dispatch's audit event so `join_request_ids` below can pair a
    client-side sample with its server-side audit row. `None` (the default)
    sends no id, matching a pre-#948 caller.
    """
    return {
        "ops": ops,
        "presentation": None,
        "presentation_per_op": None,
        "namespace": "local",
        "actor_id": None,
        "visible_namespaces": [],
        "config_id": config_id,
        "protocol_version": PROTOCOL_VERSION,
        "probe_only": probe_only,
        "metrics_only": metrics_only,
        "format": None,
        "format_per_op": None,
        "from_wire": False,
        "request_id": request_id,
    }


# ── Client-side request_id join (khive#948 design note §4) ──────────────────


def join_request_ids(samples: list[dict], audit_events: list[dict]) -> list[dict]:
    """Join client-side per-request samples to server-side audit events by
    `request_id`, per the khive#948 design note's §4 client-side join
    procedure.

    `samples`: one dict per request the harness sent, each carrying at least
    `request_id` (the value the harness generated and put on the frame) and
    `client_send_ts`/`client_recv_ts` (or any other client-measured fields —
    this function does not read them, only passes them through unchanged).

    `audit_events`: the persisted audit rows read back for the run's
    namespace/verb/time window (e.g. via `brain.event_counts` or a direct
    event-store query). Each event's `resource.request_id` (if present) is
    the correlation key; `duration_us` is the server-side dispatch time to
    pull across for the client/server latency decomposition.

    Returns one dict per input sample, `{**sample, "server_duration_us":
    int | None, "join_status": str}`:
      - `"joined"`: exactly one audit event carried this `request_id`;
        `server_duration_us` is that event's `duration_us`.
      - `"no_audit_row"`: no audit event in the window carried this
        `request_id` (talking to a pre-#948 daemon, or the row hasn't landed
        yet / fell outside the query window). `server_duration_us` is `None`.
      - `"duplicate_request_id"`: MORE THAN ONE audit event in the window
        carries this `request_id`. Binding sign-off condition (design note
        §4 item 7): a process-local counter restarts at the same values
        across harness restarts, so two runs inside one query window can
        collide on ids. This is the never-pick-a-row guard — silently
        joining to either candidate row would be exactly the heuristic join
        the benchmark Amendment 1 §3 forbids. `server_duration_us` is `None`
        even though duration data technically exists on the ambiguous rows:
        it is unattributable, not merely absent.

    A sample with no `request_id` at all (the harness sent the request
    without one) is not joinable by definition and is returned with
    `"no_audit_row"` — the same "unavailable" outcome as a request_id that
    matched nothing, since neither case has an unambiguous server-side row.
    """
    matches_by_id: dict[int, list[dict]] = {}
    for event in audit_events:
        rid = (event.get("resource") or {}).get("request_id")
        if rid is None:
            continue
        matches_by_id.setdefault(rid, []).append(event)

    joined = []
    for sample in samples:
        rid = sample.get("request_id")
        matches = matches_by_id.get(rid, []) if rid is not None else []
        if rid is None or not matches:
            status = "no_audit_row"
            server_duration_us = None
        elif len(matches) > 1:
            status = "duplicate_request_id"
            server_duration_us = None
        else:
            status = "joined"
            server_duration_us = matches[0].get("duration_us")
        joined.append({**sample, "server_duration_us": server_duration_us, "join_status": status})
    return joined


def probe_metrics_snapshot(sock_path: str) -> dict:
    """Two-step probe against the daemon's `metrics_only` gauge-snapshot frame.

    Step 1, config_id discovery: send an intentionally WRONG config_id as a
    probe_only frame. The daemon computes and echoes `served_config_id` on
    EVERY response, including a `config_mismatch` one, so this harvests the
    daemon's real config_id without reimplementing its Rust-side computation
    in Python.

    Step 2, metrics snapshot: resend with the correct config_id and
    `metrics_only: true`. A daemon that predates the `metrics_only` field
    (`crates/khive-runtime/src/daemon.rs`) ignores the unknown key and
    dispatches normally, so the response carries no `metrics` key. That
    absence is the "PENDING" signal. A daemon that supports it returns
    `metrics` populated with a `MetricsSnapshot` and never reaches ops
    dispatch. That presence is the "LIVE" signal.

    Never raises: any failure degrades to PENDING with the reason recorded.
    """
    try:
        resp1 = raw_daemon_roundtrip(
            sock_path, base_daemon_frame("", "__bench_discovery_probe__", probe_only=True)
        )
    except Exception as exc:
        return {"oracle": "PENDING", "config_id": None, "detail": f"discovery round-trip failed: {exc!r}"}

    real_config_id = resp1.get("served_config_id")
    if not real_config_id:
        return {
            "oracle": "PENDING",
            "config_id": None,
            "detail": f"no served_config_id in discovery response: {resp1}",
        }

    metrics_frame = base_daemon_frame("stats()", real_config_id, probe_only=False, metrics_only=True)
    try:
        resp2 = raw_daemon_roundtrip(sock_path, metrics_frame)
    except Exception as exc:
        return {
            "oracle": "PENDING",
            "config_id": real_config_id,
            "detail": f"metrics probe round-trip failed: {exc!r}",
        }

    if resp2.get("config_mismatch"):
        return {
            "oracle": "PENDING",
            "config_id": real_config_id,
            "detail": f"config_mismatch on metrics probe (unexpected race): {resp2}",
        }
    if resp2.get("metrics") is not None:
        return {
            "oracle": "LIVE",
            "config_id": real_config_id,
            "detail": "response carries a populated `metrics` key",
            "metrics": resp2["metrics"],
        }
    return {
        "oracle": "PENDING",
        "config_id": real_config_id,
        "detail": (
            "no `metrics` key in response, this daemon predates the metrics_only field"
        ),
    }


# ── F10 concurrent frame floor: deadline-aware, timeout-censored ────────────

def _classify_bad_response(resp: dict, frame: dict) -> str | None:
    """Return an `errors_by_code` label if `resp` must be rejected as a
    sample, or `None` if it is a genuine successful dispatch.

    Per the Coverage definition (item 3), a daemon fallback or config
    mismatch is a failed scenario, never an interchangeable latency sample:
    the daemon deliberately returns `ok: false, config_mismatch: true`
    WITHOUT dispatching when the frame's `config_id` disagrees with its own
    (`crates/khive-runtime/src/daemon.rs` around 799-810), so a stale or
    mismatched daemon would otherwise produce a plausible-but-wrong
    fast-rejection latency. `metrics_only` frames are rejected outright: they
    are a daemon-metrics gauge probe, answered before the config_id check
    (namespace/config-agnostic read), never a dispatch measurement, so their
    elapsed time must never enter the success latency population regardless
    of what `served_config_id` they carry.
    """
    if frame.get("metrics_only"):
        return "metrics_only"
    if resp.get("config_mismatch"):
        return "config_mismatch"
    if resp.get("version_mismatch"):
        return "version_mismatch"
    if not resp.get("ok"):
        return "not_ok"
    if resp.get("served_config_id") != frame.get("config_id"):
        return "served_config_mismatch"
    return None


def measure_concurrent_frames(
    sock_path: str,
    frame: dict,
    attempts: int,
    concurrency: int,
    deadline_ms: float = 2000.0,
) -> dict:
    """Fire `attempts` raw daemon-frame round trips at `concurrency` parallel
    workers and return a percentile/timeout distribution.

    Each attempt opens its own AF_UNIX socket connection, so overlapping
    in-flight requests are genuinely concurrent (unlike a single stdio
    front-end subprocess, which serves one synchronous request/response pipe
    at a time). A response that does not complete within `deadline_ms` is
    right-censored: it is counted under `timed_out` and its elapsed time is
    NEVER inserted into the latency population, per the Methodology
    contract's settle/censoring rule (a deadline never substitutes into a
    percentile).

    A response that completes but fails `_classify_bad_response` (not
    `ok`, a `config_mismatch`/`version_mismatch` flag, or a
    `served_config_id` that disagrees with the frame's requested
    `config_id`) is likewise excluded from the latency population and
    counted under `errors_by_code` by its distinct code
    (`config_mismatch`, `version_mismatch`, `not_ok`,
    `served_config_mismatch`), per the Coverage definition's rule that a
    daemon fallback or config mismatch is a failed scenario, not an
    interchangeable sample. Any transport exception is counted under
    `errors_by_code` too, keyed by the exception's class name: the raw
    frame protocol has no server-assigned stable error code for transport
    failures, so the Python exception type is the best available
    attribution at that layer.

    This is the F10 diagnostic frame-floor measurement: it bypasses the
    stdio MCP front-end's `tools/call` JSON-RPC decode entirely, so per the
    Coverage definition it is a lower attribution layer, not a substitute
    for a full E2E scenario record (which requires the versioned
    `FlagshipRecord` contract and scenario runner from PR 1 / later PRs).

    Returns a plain dict (not the versioned Distribution schema from
    DESIGN.md's Typed interfaces section, which is owned by PR 1's
    `schemas/flagship-result-v1.json`):
      {attempts, successes, timed_out, errors_by_code,
       p50_us, p95_us, p99_us, max_us}
    """
    deadline_s = deadline_ms / 1000.0
    latencies_us: list[int] = []
    timed_out = 0
    errors_by_code: dict[str, int] = {}

    def _one(_i):
        t0 = time.perf_counter_ns()
        try:
            resp = raw_daemon_roundtrip(sock_path, frame, timeout_s=deadline_s)
            bad_code = _classify_bad_response(resp, frame)
            if bad_code is not None:
                return ("error", bad_code)
            return ("ok", (time.perf_counter_ns() - t0) // 1000)
        except socketlib.timeout:
            return ("timeout", None)
        except Exception as exc:  # classify, never propagate to the caller
            return ("error", type(exc).__name__)

    with concurrent.futures.ThreadPoolExecutor(max_workers=max(1, concurrency)) as pool:
        for kind, payload in pool.map(_one, range(attempts)):
            if kind == "ok":
                latencies_us.append(payload)
            elif kind == "timeout":
                timed_out += 1
            else:
                errors_by_code[payload] = errors_by_code.get(payload, 0) + 1

    latencies_us.sort()
    return {
        "attempts": attempts,
        "successes": len(latencies_us),
        "timed_out": timed_out,
        "errors_by_code": errors_by_code,
        "p50_us": pct(latencies_us, 0.5) if latencies_us else None,
        "p95_us": pct(latencies_us, 0.95) if latencies_us else None,
        "p99_us": pct(latencies_us, 0.99) if latencies_us else None,
        "max_us": latencies_us[-1] if latencies_us else None,
    }


def measure_probe_only_floor(
    sock_path: str,
    config_id: str,
    attempts: int = 100,
    concurrency: int = 8,
    deadline_ms: float = 2000.0,
) -> dict:
    """Concurrent `probe_only` frame round trips: the raw framing floor with
    no ops dispatch at all. Clearly a diagnostic non-MCP measurement (DESIGN.md
    F10 required target scenarios). The daemon returns an identity frame
    immediately after identity validation, without reaching the dispatcher.
    """
    frame = base_daemon_frame("", config_id, probe_only=True)
    return measure_concurrent_frames(sock_path, frame, attempts, concurrency, deadline_ms)


def measure_stats_dispatch_floor(
    sock_path: str,
    config_id: str,
    attempts: int = 100,
    concurrency: int = 8,
    deadline_ms: float = 2000.0,
) -> dict:
    """Concurrent raw-frame round trips carrying `ops="stats()"`: the daemon's
    full parse-plus-dispatch floor for its cheapest read verb. This still
    bypasses the stdio front-end's `tools/call` JSON-RPC decode, so it
    isolates daemon-side dispatch cost from stdio transport cost. It is not
    a Coverage-definition-compliant `request(stats())` E2E record on its own.
    """
    frame = base_daemon_frame("stats()", config_id, probe_only=False, metrics_only=False)
    return measure_concurrent_frames(sock_path, frame, attempts, concurrency, deadline_ms)
