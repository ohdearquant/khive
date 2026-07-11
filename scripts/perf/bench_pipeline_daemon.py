#!/usr/bin/env python3
"""Pipeline regression gate — drives the daemon path.

Ingest 600 deterministic notes (15 topic clusters x 40), run 15 recall
queries through the kkernel mcp binary over stdio (which forwards to the
warm daemon on first use), measure Precision@K and round-trip latency.

What this gate DOES verify:
  (a) Daemon-path engagement: the front-end binary spawns a warm daemon
      child, binds the bench socket, and routes all recall traffic through it.
  (b) Fused-pipeline mean Precision@K: mean P@K across 15 queries must be
      >= RECALL_FLOOR (0.70) when both legs are active (default fusion).
  (c) Per-leg structural retrieval: the vector leg (dense ANN — khive-vamana
      / khive-hnsw) and the keyword leg (FTS5 / bm25) are each exercised in
      isolation via fusion_strategy="vector_only" / "keyword_only". Each must
      meet its own floor (VECTOR_FLOOR / KEYWORD_FLOOR = 0.70) per-query.

What this gate does NOT verify:
  This harness uses a deterministic LEXICAL hash embedder (cargo feature
  bench-embedder / FNV-1a), not a real embedding model. It therefore does NOT
  measure semantic-quality or recall@K regression on real embeddings. That is
  covered separately by the banked 1M-scale Vamana benchmarks. The per-leg
  floors prove that each retrieval leg is structurally functional; they do not
  gate semantic ranking quality.

Safety:
  - A fresh temp DB is created per run; live ~/.khive/khive.db is never touched.
  - The bench binary is identified by the path passed via KKERNEL_BENCH_BINARY
    env var or discovered at crates/target/release/kkernel-bench. The live
    daemon keyed on ~/.khive/khive.db is untouched because config_id differs.
  - Both the front-end process and its bench daemon child are terminated on exit.

NOTE: P@K is NOT bit-identical run-to-run — downstream ANN candidate ordering
and over-fetch retry introduce small variance. The floor must keep healthy
headroom and must NOT be tightened toward the observed mean.
"""

import csv
import itertools
import json
import os
import pathlib
import signal
import subprocess
import sys
import tempfile
import time

# ── Tunables ──────────────────────────────────────────────────────────────────

TOP_K = 10
RECALL_FLOOR = 0.70
# Per-leg structural floors. Both legs score 1.000 (zero spread) on this corpus
# across all measured runs (3 clean runs, all 15 queries, both legs). Floors are
# set at 0.70 — same 30% headroom as the fused gate — so any total leg failure
# (i.e. a leg returning zero or wrong-topic hits) will drop P@K to 0.0 or near
# zero, well below the floor. Gating is per-query (not just per-mean) because
# the zero-spread observed means make per-query gating safe without false fails.
VECTOR_FLOOR = 0.70
KEYWORD_FLOOR = 0.70
N_REPS = 5  # repetitions per query for latency measurement

# ANN convergence settle step. After a 600-note ingest burst, the memory pack's
# background ANN rebuild (write-generation-checked install, ADR-107 sec 1) has
# not necessarily installed by the time the first post-ingest recall lands —
# that is the documented eventual-consistency contract this pipeline ships
# (stale-but-installed served immediately, hot-swapped on background completion).
# This benchmark is a read-after-write consumer of that contract: it must poll
# for real convergence, not sample mid-rebuild and misread the race as a
# structural vector-leg failure. Bounded generous deadline; fails loudly (not
# silently) if convergence never happens, since that would indicate an actual
# rebuild-latency regression rather than expected warm-up.
ANN_SETTLE_DEADLINE_S = 30.0
ANN_SETTLE_STABLE_N = 3
ANN_SETTLE_POLL_S = 0.25

# Default production pack set (must match RuntimeConfig::default().packs in
# crates/khive-runtime/src/config.rs so config_id agrees between front-end
# and daemon child).
_DEFAULT_PACKS = "kg,gtd,memory,brain,comm,schedule,knowledge,session,git,code"

TOPICS = [
    ("knowledge graph", ["entity", "edge", "relation", "graph", "node", "ontology", "triple", "schema", "link", "concept"]),
    ("memory recall",   ["recall", "memory", "episodic", "semantic", "salience", "decay", "retrieval", "forget", "remember", "rank"]),
    ("vector search",   ["vector", "embedding", "ANN", "cosine", "similarity", "nearest", "index", "HNSW", "Vamana", "fusion"]),
    ("Rust cargo",      ["Rust", "cargo", "clippy", "fmt", "workspace", "crate", "lint", "test", "build", "compile"]),
    ("agent orchestration", ["agent", "orchestration", "dispatch", "task", "workflow", "lambda", "spawn", "parallel", "schedule", "async"]),
    ("git workflow",    ["git", "commit", "branch", "PR", "merge", "pull", "push", "diff", "review", "rebase"]),
    ("FTS text search", ["FTS5", "text", "search", "BM25", "trigram", "tokenise", "index", "query", "rank", "keyword"]),
    ("brain profile",   ["brain", "profile", "Bayesian", "posterior", "prior", "feedback", "signal", "calibration", "score", "update"]),
    ("namespace",       ["namespace", "isolation", "security", "gate", "policy", "auth", "token", "scope", "local", "actor"]),
    ("embedding model", ["MiniLM", "lattice", "model", "dimension", "sentence", "transformer", "embed", "passage", "query", "multilingual"]),
    ("lionagi SDK",     ["lionagi", "SDK", "flow", "package", "Python", "API", "integration", "session", "tool", "hypothesis"]),
    ("formal verification", ["lean4", "proof", "formal", "theorem", "styx", "type", "dependent", "logic", "verification", "soundness"]),
    ("CI pipeline",     ["CI", "GitHub", "workflow", "runner", "action", "build", "pass", "fail", "test", "regression"]),
    ("data storage",    ["SQLite", "WAL", "storage", "database", "migration", "schema", "DDL", "backend", "pool", "transaction"]),
    ("scoring ranking", ["score", "rank", "RRF", "fusion", "weight", "threshold", "MMR", "diversity", "relevance", "temporal"]),
]

QUERIES = [
    ("knowledge graph entity edge relation",          "knowledge graph"),
    ("memory recall salience decay ranking",          "memory recall"),
    ("vector embedding ANN similarity search",        "vector search"),
    ("Rust cargo workspace lint build",               "Rust cargo"),
    ("agent orchestration parallel dispatch workflow","agent orchestration"),
    ("git commit branch PR review",                   "git workflow"),
    ("FTS text search BM25 trigram keyword",          "FTS text search"),
    ("brain profile Bayesian posterior feedback",     "brain profile"),
    ("namespace isolation security gate token",       "namespace"),
    ("MiniLM lattice embedding model dimension",      "embedding model"),
    ("lionagi SDK flow Python integration",           "lionagi SDK"),
    ("lean4 formal proof verification styx",          "formal verification"),
    ("CI GitHub workflow runner regression",          "CI pipeline"),
    ("SQLite storage migration database schema",      "data storage"),
    ("score rank RRF fusion weight relevance",        "scoring ranking"),
]

# ── Corpus generator (mirrors pipeline_gate.rs:generate_corpus) ──────────────

def generate_corpus():
    notes = []
    for topic, words in TOPICS:
        for i in range(40):
            w0 = words[i % len(words)]
            w1 = words[(i + 1) % len(words)]
            w2 = words[(i + 3) % len(words)]
            prefix = "core" if i % 3 == 0 else ("detail" if i % 3 == 1 else "context")
            notes.append(f"{prefix} {w0}: {topic} with {w1} and {w2} — note {i}")
    return notes

# ── Settle sentinel ───────────────────────────────────────────────────────────

_SENTINEL_SEQ = itertools.count()


def _make_sentinel_content():
    """Return a run-unique content string that cannot collide with corpus text.

    Built from os.getpid() plus a per-run counter so it is distinct even across
    concurrent bench invocations on the same host. It shares no words with any
    TOPICS entry, so it can never satisfy a topic-containment check and never
    counts toward (or against) any query's Precision@K.
    """
    return f"__khive_bench_settle_sentinel_pid{os.getpid()}_seq{next(_SENTINEL_SEQ)}__"

# ── MCP stdio driver (mirrors smoke_test.py pattern) ─────────────────────────

_request_id = 0

def _next_id():
    global _request_id
    _request_id += 1
    return _request_id


def _send(proc, method, params=None):
    msg = {"jsonrpc": "2.0", "id": _next_id(), "method": method}
    if params is not None:
        msg["params"] = params
    proc.stdin.write((json.dumps(msg) + "\n").encode())
    proc.stdin.flush()


def _recv(proc):
    line = proc.stdout.readline()
    if not line:
        raise RuntimeError("MCP binary closed stdout unexpectedly")
    return json.loads(line)


def _call_request(proc, ops_string):
    _send(proc, "tools/call", {"name": "request", "arguments": {"ops": ops_string}})
    resp = _recv(proc)
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


def _call_verb(proc, verb, args):
    ops = json.dumps([{"tool": verb, "args": args}])
    body = _call_request(proc, ops)
    if body is None:
        raise RuntimeError(f"Empty response for verb {verb}")
    results = body.get("results") or []
    if not results:
        raise RuntimeError(f"No results in response for verb {verb}: {body}")
    first = results[0]
    if not first.get("ok", False):
        raise RuntimeError(f"Verb {verb} failed: {first.get('error', '<no error>')}")
    return first.get("result")


def _handshake(proc):
    _send(proc, "initialize", {
        "protocolVersion": "2024-11-05",
        "capabilities": {},
        "clientInfo": {"name": "bench-pipeline", "version": "1.0.0"},
    })
    init = _recv(proc)
    if "error" in init:
        raise RuntimeError(f"initialize failed: {init['error']}")
    notify = {"jsonrpc": "2.0", "method": "notifications/initialized"}
    proc.stdin.write((json.dumps(notify) + "\n").encode())
    proc.stdin.flush()

# ── Quality ───────────────────────────────────────────────────────────────────

def precision_at_k(contents, expected_topic):
    # Standard P@K: divide by TOP_K (not by returned count) so under-filling
    # is penalized. A truncation regression returning 1 result when TOP_K=10
    # yields P@K = 1/10 = 0.10 (correctly fails the gate), not 1/1 = 1.00.
    if not contents:
        return 0.0
    return sum(1 for c in contents if expected_topic in c) / TOP_K

# ── Daemon engagement assertions ──────────────────────────────────────────────

def _read_pid_file(pid_path):
    """Return (pid:int, raw:str) from pid file, or (None, None) if absent/bad."""
    try:
        raw = pathlib.Path(pid_path).read_text().strip()
        return int(raw), raw
    except Exception:
        return None, None


def _pid_alive(pid):
    """Return True if the process is alive (signal 0)."""
    try:
        os.kill(pid, 0)
        return True
    except (ProcessLookupError, PermissionError):
        return False


def _argv_is_khive_daemon(pid):
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


def _lsof_has_bench_db(pid, bench_db):
    """Return True if the process has bench_db open (best-effort, skips if lsof absent)."""
    try:
        out = subprocess.check_output(
            ["lsof", "-p", str(pid)],
            stderr=subprocess.DEVNULL,
        ).decode()
        bench_db_real = str(pathlib.Path(bench_db).resolve())
        return bench_db_real in out
    except FileNotFoundError:
        print("[SKIP] lsof not available — skipping open-file sub-check", flush=True)
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
    pid, _ = _read_pid_file(pid_path)
    if pid is None:
        errors.append(
            f"[DAEMON-CHECK-{label}] FAIL: bench PID file {pid_path!r} absent or unreadable."
        )
    elif not _pid_alive(pid):
        errors.append(
            f"[DAEMON-CHECK-{label}] FAIL: PID {pid} from {pid_path!r} is not alive."
        )
    elif not _argv_is_khive_daemon(pid):
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
        db_open = _lsof_has_bench_db(pid, bench_db)
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
            "KHIVE_NO_DAEMON=1 was set — something spawned a daemon unexpectedly.",
            file=sys.stderr,
            flush=True,
        )
        sys.exit(1)
    print(
        f"[DAEMON-CHECK-{label}] PASS: no bench socket with KHIVE_NO_DAEMON=1 (correct)",
        flush=True,
    )


# ── Provenance ────────────────────────────────────────────────────────────────

def _git_sha():
    try:
        out = subprocess.check_output(
            ["git", "rev-parse", "HEAD"],
            cwd=str(pathlib.Path(__file__).parent.parent.parent),
            stderr=subprocess.DEVNULL,
        )
        return out.decode().strip()
    except Exception:
        return "unknown"


def _runner_os():
    runner = os.environ.get("RUNNER_OS", "").lower()
    if runner:
        return runner
    import platform
    s = platform.system().lower()
    m = platform.machine().lower()
    return f"{s}-{m}"


def _loadavg1():
    try:
        with open("/proc/loadavg") as f:
            return float(f.read().split()[0])
    except Exception:
        pass
    try:
        out = subprocess.check_output(["sysctl", "-n", "vm.loadavg"], stderr=subprocess.DEVNULL)
        s = out.decode().strip().strip("{}").strip()
        return float(s.split()[0])
    except Exception:
        return 0.0


def _iso_now():
    t = time.gmtime()
    return f"{t.tm_year:04d}-{t.tm_mon:02d}-{t.tm_mday:02d}T{t.tm_hour:02d}:{t.tm_min:02d}:{t.tm_sec:02d}Z"

# ── Ledger ────────────────────────────────────────────────────────────────────

LEDGER_PATH = pathlib.Path(__file__).parent.parent.parent / "perf" / "pipeline-ledger.csv"
LEDGER_HEADER = ["git_sha", "runner_os", "loadavg1", "produced_at",
                 "precision_at_k", "precision_vector_only", "precision_keyword_only",
                 "p50_us", "p95_us", "n_queries", "n_corpus"]


def _append_ledger(row):
    LEDGER_PATH.parent.mkdir(parents=True, exist_ok=True)
    write_header = not LEDGER_PATH.exists()
    with open(LEDGER_PATH, "a", newline="") as f:
        w = csv.DictWriter(f, fieldnames=LEDGER_HEADER)
        if write_header:
            w.writeheader()
        w.writerow(row)


def _read_baseline_p50():
    if not LEDGER_PATH.exists():
        return None
    rows = []
    try:
        with open(LEDGER_PATH) as f:
            reader = csv.DictReader(f)
            for r in reader:
                try:
                    rows.append(float(r["p50_us"]))
                except (KeyError, ValueError):
                    pass
    except Exception:
        return None
    return rows[0] if rows else None

# ── Safety guard ──────────────────────────────────────────────────────────────

_LIVE_DB_PATHS = frozenset([
    os.path.expanduser("~/.khive/khive.db"),
    os.path.expanduser("~/.khive/khive-graph.db"),
])


def _assert_not_live_db(path):
    resolved = str(pathlib.Path(path).resolve())
    for live in _LIVE_DB_PATHS:
        live_resolved = str(pathlib.Path(live).resolve())
        if resolved == live_resolved or resolved.startswith(str(pathlib.Path(live).parent.resolve())):
            print(f"FATAL: bench DB path {path!r} resolves to live DB location. Aborting.", file=sys.stderr)
            sys.exit(2)

# ── Percentile ────────────────────────────────────────────────────────────────

def _pct(sorted_list, p):
    if not sorted_list:
        return 0.0
    idx = min(int(len(sorted_list) * p), len(sorted_list) - 1)
    return sorted_list[idx]

# ── Ingest ────────────────────────────────────────────────────────────────────

_BATCH_SIZE = 50


def _ingest(proc, corpus):
    print(f"Ingesting {len(corpus)} notes in batches of {_BATCH_SIZE}...", flush=True)
    for start in range(0, len(corpus), _BATCH_SIZE):
        batch = corpus[start:start + _BATCH_SIZE]
        ops_list = [
            {"tool": "memory.remember", "args": {"content": text, "memory_type": "semantic", "salience": 0.7, "decay_factor": 0.0}}
            for text in batch
        ]
        ops = json.dumps(ops_list)
        body = _call_request(proc, ops)
        if body is None:
            raise RuntimeError(f"Empty response ingesting batch starting at {start}")
        summary = body.get("summary", {})
        failed = summary.get("failed", 0)
        if failed:
            raise RuntimeError(f"Ingest batch at {start}: {failed} ops failed; {body}")
    print("Ingest done.", flush=True)

# ── Query ─────────────────────────────────────────────────────────────────────

def _query_once(proc, query_text, fusion_strategy=None):
    """Issue one memory.recall and return (elapsed_us, contents).

    When fusion_strategy is provided (e.g. "vector_only" or "keyword_only"),
    it is passed as-is to the memory.recall verb. The daemon accepts the
    snake_case strings defined in parse_fusion_strategy_str:
      "vector_only"  — dense ANN leg only (khive-vamana / khive-hnsw)
      "keyword_only" — FTS5 / bm25 leg only
    Confirmed live against the daemon: both return hits without parse errors.
    """
    t0 = time.perf_counter_ns()
    args = {"query": query_text, "limit": TOP_K, "full_content": True}
    if fusion_strategy is not None:
        args["fusion_strategy"] = fusion_strategy
    result = _call_verb(proc, "memory.recall", args)
    elapsed_us = (time.perf_counter_ns() - t0) // 1000

    if isinstance(result, list):
        arr = result
    elif isinstance(result, dict):
        arr = result.get("results") or result.get("items") or []
    else:
        arr = []

    contents = [r["content"] for r in arr if isinstance(r, dict) and "content" in r]
    return elapsed_us, contents

def _wait_for_ann_convergence(
    proc,
    sentinel_content,
    deadline_s=ANN_SETTLE_DEADLINE_S,
    stable_needed=ANN_SETTLE_STABLE_N,
    poll_interval_s=ANN_SETTLE_POLL_S,
):
    """Poll the vector-only leg, querying with `sentinel_content` itself, until
    the EXACT sentinel note is present in the results for `stable_needed`
    consecutive polls, or raise loudly if `deadline_s` elapses first.

    This is the deterministic settle step between ingest and the query phase:
    it samples the SAME structural signal the gate later asserts on
    (fusion_strategy="vector_only"), so "converged" here means the ANN
    background rebuild has actually installed over the full post-ingest
    corpus — not a blind sleep guessing at a duration.

    `sentinel_content` MUST be the content of a note written with its own
    SEQUENTIAL memory.remember call issued strictly after every batch ingest
    op has returned — never a note drawn from a batch. Batch ingest ops run
    in parallel with no inter-op ordering (ADR-016), so the "last" element of
    a batch has no guarantee of being the last note actually committed; and
    the daemon's background rebuild is a full-corpus rebuild, not incremental,
    so a build snapshotted mid-ingest can already satisfy a topic-level probe
    against EARLY-ingested content while still missing the last few batches.
    Requiring the exact sentinel — written after all batch ingestion
    completed and unique enough that only a post-sentinel-write build could
    surface it — is the only way to prove the installed index actually
    covers the complete corpus.
    """
    deadline = time.time() + deadline_s
    t_start = time.time()
    consecutive_ok = 0
    attempts = 0
    last_hit = False
    while time.time() < deadline:
        attempts += 1
        _, contents = _query_once(proc, sentinel_content, fusion_strategy="vector_only")
        last_hit = any(sentinel_content in c for c in contents)
        if last_hit:
            consecutive_ok += 1
            if consecutive_ok >= stable_needed:
                print(
                    f"[SETTLE] ANN vector leg converged after {attempts} probe(s), "
                    f"{time.time() - t_start:.2f}s",
                    flush=True,
                )
                return
        else:
            consecutive_ok = 0
        time.sleep(poll_interval_s)
    raise RuntimeError(
        f"ANN vector-leg did not converge within {deadline_s}s of ingest "
        f"({attempts} probe(s), sentinel note {sentinel_content!r} not found "
        "in vector-only results). The background rebuild triggered by ingest "
        "never installed a corpus-covering index. This exceeds the expected "
        "warm-up window for this corpus size and looks like a genuine "
        "convergence-latency regression in the ANN rebuild path, not benign "
        "eventual-consistency staleness — investigate before re-running."
    )


# ── Daemon teardown helper ────────────────────────────────────────────────────

def _teardown_daemon(pid_path, tmpdir):
    """SIGTERM the bench daemon (if any), then rmtree the tmpdir."""
    import shutil
    pid, _ = _read_pid_file(pid_path)
    if pid is not None and _pid_alive(pid) and _argv_is_khive_daemon(pid):
        try:
            os.kill(pid, signal.SIGTERM)
            # Give it a moment to exit cleanly.
            for _ in range(20):
                time.sleep(0.1)
                if not _pid_alive(pid):
                    break
        except Exception:
            pass
    try:
        shutil.rmtree(tmpdir, ignore_errors=True)
    except Exception:
        pass

# ── No-daemon control run ─────────────────────────────────────────────────────

def _run_nodaemon_control(binary):
    """Run a lightweight NO_DAEMON control: one ingest + one recall, assert no daemon spawns."""
    import shutil
    tmpdir = tempfile.mkdtemp(prefix="khive-bench-nodaemon-")
    bench_db = os.path.join(tmpdir, "bench.db")
    sock_path = os.path.join(tmpdir, "khived.sock")
    pid_path_file = os.path.join(tmpdir, "khived.pid")
    _assert_not_live_db(bench_db)

    print("\n[CONTROL] Running KHIVE_NO_DAEMON=1 control run (one ingest + one recall)...", flush=True)
    proc = None
    try:
        env = {**os.environ}
        env["KHIVE_DB"] = bench_db
        env["KHIVE_SOCKET"] = sock_path
        env["KHIVE_PID"] = pid_path_file
        env["KHIVE_LOCK"] = os.path.join(tmpdir, "khived.recovery.lock")
        env["KHIVE_PACKS"] = _DEFAULT_PACKS
        env["KHIVE_NO_DAEMON"] = "1"

        proc = subprocess.Popen(
            [binary, "mcp", "--log", "error"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=env,
        )
        _handshake(proc)

        # Minimal ingest: just first 2 notes
        corpus = generate_corpus()
        mini = corpus[:2]
        ops_list = [
            {"tool": "memory.remember", "args": {"content": t, "memory_type": "semantic", "salience": 0.7, "decay_factor": 0.0}}
            for t in mini
        ]
        _call_request(proc, json.dumps(ops_list))

        # One recall
        _call_verb(proc, "memory.recall", {"query": QUERIES[0][0], "limit": 2, "full_content": True})

        # Assert: no daemon socket, no kkernel mcp --daemon process
        assert_no_daemon_spawned(sock_path, label="nodaemon")
        print("[CONTROL] PASS: KHIVE_NO_DAEMON=1 control run completed without spawning a daemon.", flush=True)

    finally:
        if proc is not None:
            try:
                proc.stdin.close()
            except Exception:
                pass
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait()
        # No daemon was spawned (that's the point), so just rmtree
        shutil.rmtree(tmpdir, ignore_errors=True)

# ── Main ──────────────────────────────────────────────────────────────────────

def main():
    repo_root = pathlib.Path(__file__).parent.parent.parent

    binary = os.environ.get(
        "KKERNEL_BENCH_BINARY",
        str(repo_root / "crates" / "target" / "release" / "kkernel-bench"),
    )
    if not pathlib.Path(binary).exists():
        print(f"FATAL: bench binary not found at {binary!r}. Build with:", file=sys.stderr)
        print(f"  cd crates && cargo build --release -p kkernel --features bench-embedder", file=sys.stderr)
        print(f"  cp crates/target/release/kkernel crates/target/release/kkernel-bench", file=sys.stderr)
        sys.exit(2)

    tmpdir = tempfile.mkdtemp(prefix="khive-bench-")
    bench_db = os.path.join(tmpdir, "bench.db")
    sock_path = os.path.join(tmpdir, "khived.sock")
    pid_path_file = os.path.join(tmpdir, "khived.pid")
    lock_path = os.path.join(tmpdir, "khived.recovery.lock")
    _assert_not_live_db(bench_db)

    proc = None
    try:
        # Pass DB and all daemon-isolation knobs as ENVIRONMENT VARIABLES so the
        # spawned daemon child inherits them. CLI args are NOT inherited by the
        # daemon child that spawn_daemon() launches via current_exe().
        env = {**os.environ}
        env["KHIVE_DB"] = bench_db
        env["KHIVE_SOCKET"] = sock_path
        env["KHIVE_PID"] = pid_path_file
        env["KHIVE_LOCK"] = lock_path
        # Set KHIVE_PACKS explicitly to the default production set so that the
        # front-end and its daemon child compute identical config_id values.
        env["KHIVE_PACKS"] = _DEFAULT_PACKS
        # Do NOT set KHIVE_NO_DAEMON — we WANT the daemon path.
        env.pop("KHIVE_NO_DAEMON", None)

        print(f"Spawning {binary} mcp (DB via env: {bench_db})", flush=True)
        proc = subprocess.Popen(
            [binary, "mcp", "--log", "warn"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=env,
        )

        _handshake(proc)
        print("[ok] MCP handshake", flush=True)

        corpus = generate_corpus()
        _ingest(proc, corpus)

        # Sequential settle sentinel: a single memory.remember call, issued only
        # after every batch ingest op above has returned, so its commit cannot
        # precede any corpus note (see _wait_for_ann_convergence docstring).
        sentinel_content = _make_sentinel_content()
        sentinel_body = _call_request(proc, json.dumps([
            {"tool": "memory.remember", "args": {
                "content": sentinel_content,
                "memory_type": "semantic",
                "salience": 0.7,
                "decay_factor": 0.0,
            }}
        ]))
        if sentinel_body is None or sentinel_body.get("summary", {}).get("failed", 0):
            raise RuntimeError(f"Sentinel note write failed: {sentinel_body}")

        # Assert recall is non-empty before measuring
        _, warmup_hits = _query_once(proc, QUERIES[0][0])
        if not warmup_hits:
            raise RuntimeError(
                f"Recall returned 0 results for warm-up query {QUERIES[0][0]!r}. "
                "FTS and vector paths may both be empty. Check ingest."
            )
        print(f"[ok] Warm-up returned {len(warmup_hits)} hit(s)", flush=True)

        # ── Daemon engagement assertions ──────────────────────────────────────
        # After the first warmup request, the daemon must have been spawned and
        # bound the bench socket. These assertions fail loudly if the gate is
        # silently falling back to local (in-process) dispatch.
        print("\n[DAEMON-CHECKS] Verifying daemon engagement...", flush=True)
        # Give daemon a moment to write PID file and bind socket (it may have
        # just been spawned by the warmup request).
        deadline = time.time() + 5.0
        while not pathlib.Path(sock_path).exists() and time.time() < deadline:
            time.sleep(0.1)
        assert_daemon_engaged(sock_path, pid_path_file, bench_db, label="main")

        # Deterministic settle: block until the ANN background rebuild
        # triggered by ingest has actually installed a corpus-covering index,
        # so the measurement loop below never samples the convergence race
        # (see ANN_SETTLE_* tunables and ADR-107 sec 1).
        print("\n[SETTLE] Waiting for ANN vector leg to converge post-ingest...", flush=True)
        # Probe with the sequential sentinel note's own unique content, not a
        # bench query against a topic: only a build that ran at or after the
        # sentinel write can surface it, so its presence proves the corpus is
        # actually complete (see _wait_for_ann_convergence docstring).
        _wait_for_ann_convergence(proc, sentinel_content)

        # Additional warm-up rounds (discard timings)
        for q, _ in QUERIES:
            _query_once(proc, q)

        # Measure
        per_query = []
        all_latencies = []
        all_pass = True

        print(f"\nRunning {len(QUERIES)} queries x {N_REPS} reps:", flush=True)
        for query_text, expected_topic in QUERIES:
            rep_latencies = []
            last_contents = []
            for _ in range(N_REPS):
                us, contents = _query_once(proc, query_text)
                rep_latencies.append(us)
                last_contents = contents

            # Per-leg structural recalls (one shot each — no latency tracking)
            _, vec_contents = _query_once(proc, query_text, fusion_strategy="vector_only")
            _, kw_contents = _query_once(proc, query_text, fusion_strategy="keyword_only")

            all_latencies.extend(rep_latencies)
            p_at_k = precision_at_k(last_contents, expected_topic)
            p_vec = precision_at_k(vec_contents, expected_topic)
            p_kw = precision_at_k(kw_contents, expected_topic)

            q_pass = p_at_k >= RECALL_FLOOR
            vec_pass = p_vec >= VECTOR_FLOOR
            kw_pass = p_kw >= KEYWORD_FLOOR
            if not q_pass or not vec_pass or not kw_pass:
                all_pass = False

            rep_latencies.sort()
            q_p50 = _pct(rep_latencies, 0.5)
            fused_verdict = "PASS" if q_pass else "FAIL"
            vec_verdict = "PASS" if vec_pass else "FAIL"
            kw_verdict = "PASS" if kw_pass else "FAIL"
            print(
                f"  {expected_topic:<22} P@K={p_at_k:.2f}  "
                f"vec={p_vec:.2f}({vec_verdict})  kw={p_kw:.2f}({kw_verdict})  "
                f"({len(last_contents)} hits)  p50={q_p50}µs  {fused_verdict}",
                flush=True,
            )
            per_query.append({
                "query": query_text,
                "expected_topic": expected_topic,
                "precision_at_k": p_at_k,
                "precision_vector_only": p_vec,
                "precision_keyword_only": p_kw,
                "top_k_count": len(last_contents),
                "query_pass": q_pass,
                "vec_pass": vec_pass,
                "kw_pass": kw_pass,
                "p50_us": q_p50,
            })

        all_latencies.sort()
        p50_us = _pct(all_latencies, 0.5)
        p95_us = _pct(all_latencies, 0.95)
        mean_pak = sum(r["precision_at_k"] for r in per_query) / len(per_query)
        mean_vec = sum(r["precision_vector_only"] for r in per_query) / len(per_query)
        mean_kw = sum(r["precision_keyword_only"] for r in per_query) / len(per_query)

        print(f"\nMean Precision@K={mean_pak:.3f}  floor={RECALL_FLOOR}", flush=True)
        print(f"Mean VectorOnly P@K={mean_vec:.3f}  floor={VECTOR_FLOOR}", flush=True)
        print(f"Mean KeywordOnly P@K={mean_kw:.3f}  floor={KEYWORD_FLOOR}", flush=True)
        print(f"p50={p50_us}µs  p95={p95_us}µs  n_latencies={len(all_latencies)}", flush=True)
        verdict = "PASS" if all_pass else "FAIL"
        print(f"Gate: {verdict}", flush=True)

        # Ledger
        sha = _git_sha()
        runner_os = _runner_os()
        loadavg1 = _loadavg1()
        produced_at = _iso_now()
        baseline_p50 = _read_baseline_p50()
        row = {
            "git_sha": sha,
            "runner_os": runner_os,
            "loadavg1": loadavg1,
            "produced_at": produced_at,
            "precision_at_k": f"{mean_pak:.4f}",
            "precision_vector_only": f"{mean_vec:.4f}",
            "precision_keyword_only": f"{mean_kw:.4f}",
            "p50_us": p50_us,
            "p95_us": p95_us,
            "n_queries": len(QUERIES),
            "n_corpus": len(corpus),
        }
        try:
            _append_ledger(row)
            print(f"Ledger row written: {LEDGER_PATH}", flush=True)
        except Exception as exc:
            print(f"NOTE: ledger write failed (non-fatal): {exc}", flush=True)

        if baseline_p50 is not None and p50_us > baseline_p50 * 1.5:
            print(
                f"NOTE: p50 latency {p50_us}µs > 1.5x baseline {baseline_p50}µs. "
                "This is a soft warning — latency is tracked but not gated.",
                flush=True,
            )

        return 0 if all_pass else 1

    finally:
        if proc is not None:
            try:
                proc.stdin.close()
            except Exception:
                pass
            # Give the front-end a moment to relay shutdown to its daemon child.
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait()

        # Reap the bench daemon child (it survives the front-end).
        _teardown_daemon(pid_path_file, tmpdir)

    # Run KHIVE_NO_DAEMON=1 control after main teardown so it uses a fresh tmpdir.


def _main_with_control():
    binary = os.environ.get(
        "KKERNEL_BENCH_BINARY",
        str(pathlib.Path(__file__).parent.parent.parent / "crates" / "target" / "release" / "kkernel-bench"),
    )
    rc = main()
    # Only run the control when the binary exists (it was already checked in main).
    if pathlib.Path(binary).exists():
        _run_nodaemon_control(binary)
    return rc


if __name__ == "__main__":
    sys.exit(_main_with_control())
