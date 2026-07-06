#!/usr/bin/env python3
"""Concurrency load-harness driver — the acceptance-gate measurement substrate.

Drives many concurrent agent connections across many tenant namespaces against
ONE warm daemon, mixed read/write, and observes the nine gate dimensions
across four channels:

  1. worker-stderr scrape       — fallback events logged by each front-end
  2. client-measured latency    — per-op round-trip timing
  3. client op-results/readback — write outcomes + attribution readback
  4. daemon-frame snapshot      — the one frame-dependent, probe-gated channel

Two embedder modes:
  --mode real   uses the installed/production `kkernel` binary (real
                embedder, Metal-backed on macOS). Acquires the machine-wide
                Metal-GPU serialization lock before touching the daemon.
  --mode bench  uses a `kkernel-bench` binary built with the bench-embedder
                cargo feature (FNV-1a hash, no GPU, no lock needed). Must be
                built separately; see the runbook.

Oracle channel (daemon-frame snapshot): the driver speaks the daemon's Unix
socket protocol directly to read gauges that are not yet exposed by any
merged wire field. It PROBES for support first (send a request carrying a
`metrics_only` field the current server doesn't know about yet, see if the
response grows a `metrics` key) and degrades cleanly to "PENDING" when the
field is not yet honored, rather than erroring or hanging. It never asserts a
dimension's threshold — it *observes* and reports.

This script never claims a dimension "passed". The smoke result (exit code)
only reflects whether the concurrency plumbing itself ran without crashing.

Safety:
  - A fresh temp DB is created per run; the live ~/.khive/khive.db is never
    touched (guarded the same way as bench_pipeline_daemon.py).
  - All daemon/front-end processes are terminated on exit; the scratch
    directory is removed unless --keep is passed.

Pack posture:
  - The hermetic reduced smoke runs the 7-pack default; the `session` pack is
    omitted because its lazily-applied mirror schema fails bootstrap recall on
    a single-file scratch DB and its background writes would confound the
    reduced-scale gauges. Pass the full production set via `--packs` for the
    acceptance run against a real multi-pack config.
"""

from __future__ import annotations

import argparse
import contextlib
import fcntl
import json
import os
import pathlib
import random
import socket as socketlib
import struct
import subprocess
import sys
import tempfile
import time
import uuid
from concurrent.futures import ThreadPoolExecutor
from concurrent.futures import TimeoutError as FutureTimeoutError

sys.path.insert(0, str(pathlib.Path(__file__).parent))
import bench_pipeline_daemon as bpd  # noqa: E402  (reuse framing/lifecycle helpers)

REPO_ROOT = pathlib.Path(__file__).parent.parent.parent

# ── Wire protocol constants (mirrors crates/khive-runtime/src/daemon.rs) ──────
PROTOCOL_VERSION = 3
# Pack posture for the spawned scratch daemon (feeds KHIVE_PACKS). Pinned explicitly here rather
# than reused from bpd._DEFAULT_PACKS so the config_id/registry surface is stated, not inherited.
#
# The hermetic reduced smoke excludes `session` (the 8th production-default pack). Its mirror
# applies schema lazily and runs periodic warm ticks against a background backend; on a single-file
# scratch DB that path fails bootstrap recall (`fts_notes` vtable construction), and its background
# writes would also confound the reduced-scale WAL / write-queue gauges this harness reads. The full
# acceptance run against a real multi-pack config opts session back in via `--packs` (see below).
DEFAULT_PACKS = "kg,gtd,memory,brain,comm,schedule,knowledge"
PRODUCTION_PACKS = "kg,gtd,memory,brain,comm,schedule,knowledge,session"

# ── Metal GPU serialization (machine-wide convention; real-embedder mode only)
METAL_GPU_LOCK_PATH = os.environ.get("METAL_GPU_LOCK_PATH", "/tmp/lion-metal-gpu-test.lock")
METAL_GPU_LOCK_TIMEOUT_S = float(os.environ.get("METAL_GPU_LOCK_TIMEOUT_S", "1800"))  # 30 min

_assert_not_live_db = bpd._assert_not_live_db
_read_pid_file = bpd._read_pid_file
_pid_alive = bpd._pid_alive
_argv_is_khive_daemon = bpd._argv_is_khive_daemon
_pct = bpd._pct


# ── Metal GPU advisory-lock helper ────────────────────────────────────────────


def _lsof_lock_holder(path: pathlib.Path) -> str | None:
    try:
        out = subprocess.check_output(
            ["lsof", "-t", str(path)], stderr=subprocess.DEVNULL
        ).decode().strip()
        return out.replace("\n", ",") or None
    except Exception:
        return None


def acquire_metal_gpu_lock(timeout_s: float = METAL_GPU_LOCK_TIMEOUT_S):
    """Acquire the exclusive advisory flock serializing Metal-GPU-driving
    processes on this machine. Bounded wait; fails LOUD (raises SystemExit)
    rather than hanging or silently proceeding under contention — concurrent
    GPU work corrupts both timing and numerics for every party involved.

    Only called in --mode real. Bench-embedder mode never touches the GPU and
    must not take this lock.
    """
    path = pathlib.Path(METAL_GPU_LOCK_PATH)
    path.parent.mkdir(parents=True, exist_ok=True)
    fh = open(path, "w")  # noqa: SIM115 — handle must outlive this function (flock lifetime)
    deadline = time.monotonic() + timeout_s
    waited = False
    while True:
        try:
            fcntl.flock(fh.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
            if waited:
                print(f"[gpu-lock] acquired {path} after waiting", flush=True)
            return fh
        except BlockingIOError:
            waited = True
            if time.monotonic() >= deadline:
                holder = _lsof_lock_holder(path)
                fh.close()
                raise SystemExit(
                    f"FATAL: could not acquire exclusive Metal-GPU lock {path} "
                    f"within {timeout_s:.0f}s. Another GPU-driving process appears "
                    f"to hold it{f' (pid(s): {holder})' if holder else ''}. "
                    "Refusing to proceed — never bypass this lock."
                ) from None
            time.sleep(2.0)


def release_metal_gpu_lock(fh) -> None:
    if fh is None:
        return
    try:
        fcntl.flock(fh.fileno(), fcntl.LOCK_UN)
    except Exception:
        pass
    fh.close()


# ── Raw daemon-socket framing (oracle probe channel only) ─────────────────────
#
# Every other channel goes through the existing front-end binary (stdio MCP).
# This is the one place we speak the daemon's Unix-socket wire protocol
# directly in Python, because the oracle channel needs a `metrics_only` frame
# field that the stdio/MCP surface has no verb for (it isn't merged yet).


def _recv_exact(sock: socketlib.socket, n: int) -> bytes:
    buf = b""
    while len(buf) < n:
        chunk = sock.recv(n - len(buf))
        if not chunk:
            raise RuntimeError("daemon socket closed mid-frame")
        buf += chunk
    return buf


def _raw_daemon_roundtrip(sock_path: str, frame: dict, timeout_s: float = 5.0) -> dict:
    s = socketlib.socket(socketlib.AF_UNIX, socketlib.SOCK_STREAM)
    s.settimeout(timeout_s)
    try:
        s.connect(sock_path)
        payload = json.dumps(frame).encode()
        s.sendall(struct.pack(">I", len(payload)) + payload)
        len_buf = _recv_exact(s, 4)
        (length,) = struct.unpack(">I", len_buf)
        raw = _recv_exact(s, length)
        return json.loads(raw)
    finally:
        s.close()


def _base_daemon_frame(ops: str, config_id: str, probe_only: bool) -> dict:
    """Build a DaemonRequestFrame JSON payload. `presentation` /
    `presentation_per_op` / `namespace` have no serde default on the Rust
    struct (unlike the rest), so they must always be present in the wire
    payload or the daemon silently drops the connection.
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
        "format": None,
        "format_per_op": None,
        "from_wire": False,
    }


def probe_oracle_channel(sock_path: str) -> dict:
    """Two-step probe against the daemon-frame snapshot channel.

    Step 1 — config_id discovery: send an intentionally WRONG config_id as a
    probe_only frame. The daemon computes and echoes `served_config_id` on
    EVERY response, including a `config_mismatch` one, so this harvests the
    daemon's real config_id without reimplementing its Rust-side computation
    in Python.

    Step 2 — metrics support probe: resend with the correct config_id and an
    extra `metrics_only: true` field (the field name the not-yet-merged
    metrics-frame PR is expected to define). Today this field is unknown to
    the server and silently ignored (serde ignores unrecognized JSON keys by
    default), so the request just dispatches `ops` normally and the response
    carries no `metrics` key — that absence IS the "PENDING" signal. Once the
    metrics PR merges, a `metrics` key appearing in the response flips this to
    "LIVE" with no code change required here.

    Never raises: any failure degrades to PENDING with the reason recorded.
    """
    try:
        resp1 = _raw_daemon_roundtrip(
            sock_path, _base_daemon_frame("", "__loadharness_discovery_probe__", True)
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

    metrics_frame = _base_daemon_frame("stats()", real_config_id, False)
    metrics_frame["metrics_only"] = True
    try:
        resp2 = _raw_daemon_roundtrip(sock_path, metrics_frame)
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
            "no `metrics` key in response — this daemon predates the metrics-frame "
            "PR (metrics_only was ignored as an unknown field)"
        ),
    }


# ── generic nested-value lookup (attribution readback, dim 8) ────────────────


def _find_value_anywhere(obj, key: str):
    """DFS for the first occurrence of `key` in a nested dict/list, returning
    its value. Used for the attribution readback check so the harness does
    not hardcode one exact response shape.
    """
    if isinstance(obj, dict):
        if key in obj:
            return obj[key]
        for v in obj.values():
            found = _find_value_anywhere(v, key)
            if found is not None:
                return found
    elif isinstance(obj, list):
        for item in obj:
            found = _find_value_anywhere(item, key)
            if found is not None:
                return found
    return None


# ── worker op menu ─────────────────────────────────────────────────────────────

_RECALL_PHRASES = [
    "concurrency load harness daemon socket frame perf driver",
    "namespace attribution actor write stamp tenant readback",
    "write queue backpressure watchdog timeout typed error",
    "WAL checkpoint pages floor pin oldest transaction age",
    "brain slot resolve profile compose knowledge domain",
]

_COMPOSE_PHRASES = [
    # auto-compose requires >= 10 words per query (khive-pack-knowledge validation)
    "concurrency load harness tenant alternation namespace attribution write backpressure gauge",
    "warm daemon socket frame protocol version config identity coherence check",
    "write ahead log checkpoint floor pin threshold oldest transaction age gauge",
]


class OpOutcome:
    __slots__ = ("op", "ok", "latency_us", "error")

    def __init__(self, op, ok, latency_us, error=None):
        self.op = op
        self.ok = ok
        self.latency_us = latency_us
        self.error = error


def _timed_call(proc, verb, args) -> OpOutcome:
    t0 = time.perf_counter_ns()
    try:
        bpd._call_verb(proc, verb, args)
        elapsed_us = (time.perf_counter_ns() - t0) // 1000
        return OpOutcome(verb, True, elapsed_us)
    except Exception as exc:
        elapsed_us = (time.perf_counter_ns() - t0) // 1000
        return OpOutcome(verb, False, elapsed_us, str(exc))


def _op_recall(proc, tenant, i):
    return _timed_call(proc, "memory.recall", {"query": random.choice(_RECALL_PHRASES), "limit": 5})


def _op_knowledge_search(proc, tenant, i):
    return _timed_call(proc, "knowledge.search", {"query": random.choice(_RECALL_PHRASES), "limit": 5})


def _op_knowledge_compose(proc, tenant, i):
    return _timed_call(
        proc, "knowledge.compose", {"query": random.choice(_COMPOSE_PHRASES), "max_tokens": 1500}
    )


def _op_remember(proc, tenant, i):
    return _timed_call(
        proc,
        "memory.remember",
        {
            "content": f"loadharness probe note tenant={tenant} i={i} id={uuid.uuid4().hex[:8]}",
            "memory_type": "episodic",
            "salience": 0.3,
            "decay_factor": 0.02,
        },
    )


def _op_create_entity(proc, tenant, i):
    return _timed_call(
        proc,
        "create",
        {
            "kind": "entity",
            "entity_kind": "concept",
            "name": f"loadharness-concept-tenant{tenant}-{i}-{uuid.uuid4().hex[:8]}",
            "description": "load-harness synthetic concept for concurrency probing",
        },
    )


# weighted read-heavy op menu (recall-dominant), steady write fraction
_OP_MENU = [
    (_op_recall, 0.40),
    (_op_knowledge_search, 0.15),
    (_op_knowledge_compose, 0.10),
    (_op_remember, 0.15),
    (_op_create_entity, 0.10),
]
_OP_MENU_WEIGHTS_SUM = sum(w for _, w in _OP_MENU)


def _pick_op():
    r = random.uniform(0, _OP_MENU_WEIGHTS_SUM)
    upto = 0.0
    for fn, w in _OP_MENU:
        upto += w
        if r <= upto:
            return fn
    return _OP_MENU[-1][0]


def _attribution_probe(proc, tenant) -> dict:
    """dim-8 attribution readback: send a message as this tenant's connection,
    read it back, and check the DURABLY STORED record attributes it consistently.

    This does NOT assert a specific expected actor STRING. Empirically (this
    worktree, ADR-096's `serve.rs` explicit-namespace fill rule,
    `resolved.actor_id = Some(ns)` whenever an explicit non-local `--namespace`
    is passed): a front-end's own `actor_id` collapses to its `--namespace`
    value, and a per-session `KHIVE_ACTOR` env var (the mechanism this
    harness's spec recommends for actor pinning) is NOT consulted in that case
    — tier 1 (CLI namespace) short-circuits before tier 3 (`KHIVE_ACTOR`) is
    ever reached. So `tenant_N_actor` (what a naive reading of the spec would
    expect) never appears; `tenant_N` does. That is a real, worth-flagging
    finding about the current attribution-pinning mechanism, not a harness
    bug — see the runbook / PR notes.

    What IS meaningfully checkable without depending on that mechanism's exact
    string convention: (a) the value echoed at send time durably persists
    unchanged in the stored record (write-then-read consistency), and (b)
    distinct tenants get distinct attributed identities (no cross-tenant
    collapse/bleed). The caller aggregates (b) across all tenants.
    """
    probe_id = uuid.uuid4().hex[:12]
    try:
        send_result = bpd._call_verb(
            proc,
            "comm.send",
            {
                "to": f"loadharness-sink-{tenant}",
                "content": f"loadharness attribution probe {probe_id}",
                "subject": "loadharness-probe",
            },
        )
    except Exception as exc:
        return {"tenant": tenant, "status": "send-error", "detail": str(exc)}

    full_id = send_result.get("full_id") if isinstance(send_result, dict) else None
    if not full_id:
        return {"tenant": tenant, "status": "send-error", "detail": f"no full_id in send result: {send_result}"}
    sent_from = send_result.get("from")

    try:
        readback = bpd._call_verb(proc, "get", {"id": full_id})
    except Exception as exc:
        return {"tenant": tenant, "status": "readback-error", "detail": str(exc)}

    stored_from_actor = _find_value_anywhere(readback, "from_actor")

    if stored_from_actor is None:
        return {"tenant": tenant, "status": "no-from_actor-in-readback", "sent_from": sent_from}
    if stored_from_actor != sent_from:
        return {
            "tenant": tenant,
            "status": "readback-inconsistent",
            "sent_from": sent_from,
            "stored_from_actor": stored_from_actor,
        }
    return {"tenant": tenant, "status": "consistent", "attributed_actor": stored_from_actor}


# ── worker lifecycle ──────────────────────────────────────────────────────────


class WorkerResult:
    def __init__(self, tenant, idx):
        self.tenant = tenant
        self.idx = idx
        self.outcomes: list[OpOutcome] = []
        self.attribution: dict | None = None
        self.crashed: str | None = None
        self.stderr_path: str | None = None


def _spawn_worker_proc(binary, tenant, base_env, log_level, stderr_path):
    env = {**base_env, "KHIVE_ACTOR": f"tenant_{tenant}_actor"}
    with open(stderr_path, "wb") as stderr_fh:
        proc = subprocess.Popen(
            [binary, "mcp", "--namespace", f"tenant_{tenant}", "--log", log_level],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=stderr_fh,
            env=env,
        )
    # child keeps its own dup'd fd; we scrape the file after this handle closes
    return proc


def _run_worker(binary, tenant, idx, base_env, log_level, tmpdir, ops_per_worker, is_leader, proc_registry):
    result = WorkerResult(tenant, idx)
    stderr_path = os.path.join(tmpdir, f"worker-t{tenant}-w{idx}.stderr.log")
    result.stderr_path = stderr_path
    proc = None
    try:
        proc = _spawn_worker_proc(binary, tenant, base_env, log_level, stderr_path)
        proc_registry[(tenant, idx)] = proc  # so a future-timeout can kill a wedged read
        bpd._handshake(proc)

        if is_leader:
            result.attribution = _attribution_probe(proc, tenant)

        for i in range(ops_per_worker):
            fn = _pick_op()
            result.outcomes.append(fn(proc, tenant, i))
    except Exception as exc:
        result.crashed = repr(exc)
    finally:
        if proc is not None:
            with contextlib.suppress(Exception):
                proc.stdin.close()
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait()
    return result


# ── main driver ────────────────────────────────────────────────────────────────


def _resolve_binary(mode: str) -> str:
    if mode == "bench":
        binary = os.environ.get(
            "KKERNEL_BENCH_BINARY", str(REPO_ROOT / "crates" / "target" / "release" / "kkernel-bench")
        )
        if not pathlib.Path(binary).exists():
            print(f"FATAL: bench binary not found at {binary!r}. Build with:", file=sys.stderr)
            print("  cd crates && cargo build --release -p kkernel --features bench-embedder", file=sys.stderr)
            print("  cp crates/target/release/kkernel crates/target/release/kkernel-bench", file=sys.stderr)
            sys.exit(2)
        return binary

    binary = os.environ.get("KKERNEL_BINARY")
    if binary:
        return binary
    cargo_bin = pathlib.Path.home() / ".cargo" / "bin" / "kkernel"
    if cargo_bin.exists():
        return str(cargo_bin)
    release_bin = REPO_ROOT / "crates" / "target" / "release" / "kkernel"
    if release_bin.exists():
        return str(release_bin)
    print(
        "FATAL: no real kkernel binary found. Set KKERNEL_BINARY, install to "
        "~/.cargo/bin/kkernel, or build with: cd crates && cargo build --release -p kkernel",
        file=sys.stderr,
    )
    sys.exit(2)


def _fallback_lines(stderr_path: str) -> list[str]:
    try:
        with open(stderr_path, errors="replace") as f:
            return [line for line in f if "daemon_fallback" in line]
    except Exception:
        return []


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--mode", choices=["real", "bench"], default="real")
    ap.add_argument("--workers", type=int, default=100)
    ap.add_argument("--tenants", type=int, default=20)
    ap.add_argument("--ops-per-worker", type=int, default=20)
    ap.add_argument("--worker-timeout", type=float, default=120.0)
    ap.add_argument("--log-level", default="warn")
    ap.add_argument(
        "--packs",
        default=DEFAULT_PACKS,
        help=(
            "comma-separated KHIVE_PACKS for the spawned daemon (default: the 7-pack hermetic "
            f"reduced-smoke set). Full production posture is '{PRODUCTION_PACKS}'; pass it "
            "explicitly for the acceptance run against a real multi-pack config (the hermetic "
            "single-file smoke omits `session` — see the module docstring)."
        ),
    )
    ap.add_argument("--keep", action="store_true", help="do not tear down the scratch daemon/dir on exit")
    ap.add_argument("--report", default=None, help="optional path to also write the JSON report")
    args = ap.parse_args()

    if args.workers % args.tenants != 0:
        print(
            f"FATAL: --workers ({args.workers}) must be an exact multiple of --tenants ({args.tenants})",
            file=sys.stderr,
        )
        return 2
    workers_per_tenant = args.workers // args.tenants

    binary = _resolve_binary(args.mode)
    print(f"Using {args.mode}-embedder binary: {binary}", flush=True)

    gpu_lock_fh = None
    if args.mode == "real":
        print(f"Acquiring Metal-GPU lock {METAL_GPU_LOCK_PATH} (bounded {METAL_GPU_LOCK_TIMEOUT_S:.0f}s)...", flush=True)
        gpu_lock_fh = acquire_metal_gpu_lock()
        print("[gpu-lock] acquired", flush=True)

    tmpdir = tempfile.mkdtemp(prefix="khive-loadharness-")
    db_path = os.path.join(tmpdir, "loadharness.db")
    sock_path = os.path.join(tmpdir, "khived.sock")
    pid_path_file = os.path.join(tmpdir, "khived.pid")
    lock_path = os.path.join(tmpdir, "khived.recovery.lock")
    _assert_not_live_db(db_path)

    # An empty, explicit --config file makes every spawned process hermetic:
    # it stops khive's project-anchored config discovery (walking up from cwd
    # looking for .khive/config.toml) from picking up an ambient project
    # config. That matters twice over: an ambient config declaring multiple
    # [[backends]] makes --db/KHIVE_DB ambiguous (hard error), and an ambient
    # [actor] id would outrank (and silently defeat) this harness's per-tenant
    # KHIVE_ACTOR pinning (config-file actor id is tier 2, KHIVE_ACTOR is only
    # the tier-3 fallback). Empty file means "define nothing" — normal env-var
    # driven defaults apply, no ambiguity.
    scratch_config_path = os.path.join(tmpdir, "loadharness-config.toml")
    pathlib.Path(scratch_config_path).write_text("")

    base_env = {**os.environ}
    base_env["KHIVE_DB"] = db_path
    base_env["KHIVE_SOCKET"] = sock_path
    base_env["KHIVE_PID"] = pid_path_file
    base_env["KHIVE_LOCK"] = lock_path
    base_env["KHIVE_PACKS"] = args.packs
    base_env["KHIVE_DAEMON_STRICT"] = "1"
    base_env["KHIVE_WRITE_QUEUE"] = "1"
    base_env["KHIVE_CONFIG"] = scratch_config_path
    base_env.pop("KHIVE_NO_DAEMON", None)

    report: dict = {
        "meta": {
            "mode": args.mode,
            "workers": args.workers,
            "tenants": args.tenants,
            "workers_per_tenant": workers_per_tenant,
            "ops_per_worker": args.ops_per_worker,
            "git_sha": bpd._git_sha(),
            "started_at": bpd._iso_now(),
            "db_path": db_path,
            "run_posture": "KHIVE_DAEMON_STRICT=1 KHIVE_WRITE_QUEUE=1",
        },
        "smoke_result": "FAIL",
        "smoke_errors": [],
    }

    bootstrap_proc = None
    worker_procs: dict = {}
    try:
        # ── bootstrap: bring up the shared warm daemon once, verify engagement
        print("Spawning bootstrap front-end to warm the shared daemon...", flush=True)
        bootstrap_proc = subprocess.Popen(
            [binary, "mcp", "--log", args.log_level],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=base_env,
        )
        bpd._handshake(bootstrap_proc)
        bpd._call_verb(
            bootstrap_proc,
            "memory.remember",
            {"content": "loadharness bootstrap warmup note", "memory_type": "episodic", "salience": 0.3, "decay_factor": 0.02},
        )
        bpd._call_verb(bootstrap_proc, "memory.recall", {"query": "loadharness bootstrap warmup", "limit": 3})

        deadline = time.time() + 5.0
        while not pathlib.Path(sock_path).exists() and time.time() < deadline:
            time.sleep(0.1)
        bpd.assert_daemon_engaged(sock_path, pid_path_file, db_path, label="loadharness-bootstrap")
        print("[ok] bootstrap daemon engagement confirmed", flush=True)

        try:
            bootstrap_proc.stdin.close()
            bootstrap_proc.wait(timeout=5)
        except Exception:
            bootstrap_proc.kill()
            bootstrap_proc.wait()
        bootstrap_proc = None

        # ── oracle probe: t0 sample (before load) ──
        print("Probing oracle (daemon-frame) channel at t0...", flush=True)
        oracle_t0 = probe_oracle_channel(sock_path)
        report["oracle_probe_t0"] = oracle_t0
        print(f"[oracle] t0: {oracle_t0['oracle']} — {oracle_t0['detail']}", flush=True)

        # ── fan out workers ──
        print(
            f"Spawning {args.workers} workers across {args.tenants} tenants "
            f"({workers_per_tenant}/tenant), {args.ops_per_worker} ops/worker...",
            flush=True,
        )
        jobs = []
        for tenant in range(args.tenants):
            for idx in range(workers_per_tenant):
                jobs.append((tenant, idx))

        worker_results: list[WorkerResult] = []
        with ThreadPoolExecutor(max_workers=args.workers) as pool:
            futures = {
                pool.submit(
                    _run_worker,
                    binary,
                    tenant,
                    idx,
                    base_env,
                    args.log_level,
                    tmpdir,
                    args.ops_per_worker,
                    idx == 0,
                    worker_procs,
                ): (tenant, idx)
                for tenant, idx in jobs
            }
            for fut in futures:
                tenant, idx = futures[fut]
                try:
                    worker_results.append(fut.result(timeout=args.worker_timeout))
                except FutureTimeoutError:
                    # Kill this worker's front-end so its wedged stdout read returns and the
                    # thread exits — otherwise ThreadPoolExecutor.__exit__ (shutdown(wait=True))
                    # blocks forever on the hung worker and never reaches teardown, leaking the
                    # scratch daemon + the Metal GPU lock.
                    wedged = worker_procs.get((tenant, idx))
                    if wedged is not None:
                        with contextlib.suppress(Exception):
                            wedged.kill()
                    hung = WorkerResult(tenant, idx)
                    hung.crashed = f"worker exceeded --worker-timeout={args.worker_timeout}s (possible silent hang)"
                    worker_results.append(hung)

        # ── oracle probe: post-load sample ──
        oracle_post = probe_oracle_channel(sock_path)
        report["oracle_probe_post_load"] = oracle_post
        print(f"[oracle] post-load: {oracle_post['oracle']} — {oracle_post['detail']}", flush=True)

        # ── aggregate ──
        crashed = [r for r in worker_results if r.crashed]
        report["smoke_errors"] = [f"tenant={r.tenant} worker={r.idx}: {r.crashed}" for r in crashed]

        all_outcomes: list[OpOutcome] = []
        for r in worker_results:
            all_outcomes.extend(r.outcomes)

        latencies_by_op: dict[str, list[int]] = {}
        error_texts: list[str] = []
        for o in all_outcomes:
            latencies_by_op.setdefault(o.op, []).append(o.latency_us)
            if not o.ok and o.error:
                error_texts.append(o.error)

        def _percentiles(us_list):
            us_list = sorted(us_list)
            return {
                "n": len(us_list),
                "p50_us": _pct(us_list, 0.5),
                "p95_us": _pct(us_list, 0.95),
                "p99_us": _pct(us_list, 0.99),
            }

        recall_latencies = latencies_by_op.get("memory.recall", [])
        compose_latencies = latencies_by_op.get("knowledge.compose", []) + latencies_by_op.get(
            "knowledge.search", []
        )

        # fallback scrape (dim 1) — across every worker's own front-end stderr
        fallback_lines: list[str] = []
        for r in worker_results:
            if r.stderr_path:
                fallback_lines.extend(_fallback_lines(r.stderr_path))
        reason_breakdown = {"config_mismatch": 0, "namespace_mismatch": 0, "other": 0}
        for line in fallback_lines:
            if 'reason="config_mismatch"' in line or "reason=config_mismatch" in line:
                reason_breakdown["config_mismatch"] += 1
            elif 'reason="namespace_mismatch"' in line or "reason=namespace_mismatch" in line:
                reason_breakdown["namespace_mismatch"] += 1
            else:
                reason_breakdown["other"] += 1

        # write-backpressure error categorization (dims 6, 7)
        write_queue_full_count = sum(1 for e in error_texts if "write queue full" in e)
        sqlite_busy_count = sum(1 for e in error_texts if "SQLITE_BUSY" in e or "database is locked" in e)

        # attribution (dim 8)
        attributions = [r.attribution for r in worker_results if r.attribution is not None]
        attr_consistent = sum(1 for a in attributions if a.get("status") == "consistent")
        attr_inconsistent = sum(1 for a in attributions if a.get("status") == "readback-inconsistent")
        attr_errored = sum(
            1 for a in attributions if a.get("status") in ("send-error", "readback-error", "no-from_actor-in-readback")
        )
        attributed_actors = [a["attributed_actor"] for a in attributions if a.get("status") == "consistent"]
        attr_distinct_count = len(set(attributed_actors))

        report["op_counts"] = {op: len(v) for op, v in latencies_by_op.items()}
        report["op_error_counts"] = {
            op: sum(1 for o in all_outcomes if o.op == op and not o.ok) for op in latencies_by_op
        }
        report["dimensions"] = {
            "1_fallback": {
                "channel": "worker-stderr-scrape",
                "daemon_fallback_lines": len(fallback_lines),
                "reason_breakdown": reason_breakdown,
                "note": "grep for literal 'daemon_fallback' event across every worker front-end's stderr; "
                "STRICT=1 elevates config/namespace-mismatch fallbacks to error-level but the substring "
                "is the same either way",
            },
            "2_recall_latency": {
                "channel": "client-measured",
                **_percentiles(recall_latencies),
                "note": "meaningful cold-spike read requires --mode real; bench-embedder has no cold init to spike",
            },
            "3_embed_cold_start": {
                "channel": "daemon-log-scrape",
                "status": "not-implemented-this-round",
                "note": "no confirmed embedder-init log-event text found in this worktree during recon; "
                "only indirect corroboration available via dim-2's latency shape (no direct probe here)",
            },
            "4_wal_floor": {"channel": "daemon-frame (oracle)", "status": oracle_post["oracle"], "t0": oracle_t0, "post_load": oracle_post},
            "5_wal_pin": {"channel": "daemon-frame (oracle)", "status": oracle_post["oracle"]},
            "6_write_backpressure": {
                "channel": "client-op-results",
                "sqlite_busy_or_locked_count": sqlite_busy_count,
                "note": "should be 0 under KHIVE_WRITE_QUEUE=1; a nonzero count means the write-queue is "
                "not absorbing write-write contention",
            },
            "7_backpressure_surfaced": {
                "channel": "client-op-results + daemon-frame (oracle)",
                "write_queue_full_typed_errors": write_queue_full_count,
                "oracle_status": oracle_post["oracle"],
            },
            "8_attribution": {
                "channel": "client-readback",
                "checked": len(attributions),
                "consistent_write_then_read": attr_consistent,
                "inconsistent_write_then_read": attr_inconsistent,
                "errored": attr_errored,
                "distinct_attributed_actors": attr_distinct_count,
                "expected_distinct_actors": args.tenants,
                "note": "does NOT assert a specific actor-string convention (see docstring on "
                "_attribution_probe for a real finding: the KHIVE_ACTOR-per-session pinning this "
                "harness's spec recommends is superseded by ADR-096's explicit-namespace "
                "fill rule in this worktree, so the attributed actor equals the namespace string, "
                "not '<namespace>_actor'). This checks write-then-read consistency and that "
                "distinct tenants get distinct attributed identities (no cross-tenant collapse).",
                "detail": attributions,
            },
            "9_brain_slot_throughput": {
                "channel": "client-measured",
                **_percentiles(compose_latencies),
            },
        }

        report["smoke_result"] = "PASS" if not crashed else "FAIL"
        report["meta"]["finished_at"] = bpd._iso_now()
        return 0 if report["smoke_result"] == "PASS" else 1

    except SystemExit:
        raise
    except Exception as exc:
        report["smoke_errors"].append(f"driver-level exception: {exc!r}")
        report["smoke_result"] = "FAIL"
        return 1
    finally:
        if bootstrap_proc is not None:
            try:
                bootstrap_proc.stdin.close()
                bootstrap_proc.wait(timeout=5)
            except Exception:
                bootstrap_proc.kill()
                bootstrap_proc.wait()

        for _wp in worker_procs.values():
            with contextlib.suppress(Exception):
                if _wp.poll() is None:
                    _wp.kill()

        print(json.dumps(report, indent=2, default=str))
        if args.report:
            try:
                pathlib.Path(args.report).write_text(json.dumps(report, indent=2, default=str))
            except Exception as exc:
                print(f"NOTE: failed to write --report copy: {exc!r}", file=sys.stderr)

        if args.keep:
            print(f"--keep set: leaving scratch dir + daemon running at {tmpdir}", flush=True)
        else:
            bpd._teardown_daemon(pid_path_file, tmpdir)

        if gpu_lock_fh is not None:
            release_metal_gpu_lock(gpu_lock_fh)


if __name__ == "__main__":
    sys.exit(main())
