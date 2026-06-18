#!/usr/bin/env python3
"""Pipeline regression gate — drives the daemon path.

Ingest 600 deterministic notes (15 topic clusters x 40), run 15 recall
queries through the kkernel mcp binary over stdio (which forwards to the
warm daemon on first use), measure Precision@K and round-trip latency.

Gate: mean Precision@K >= 0.70. Exit 0 = PASS, exit 1 = FAIL.
Latency is tracked (ledger row) but never gated.

Safety:
  - A fresh temp DB is created per run; live ~/.khive/khive.db is never touched.
  - The bench binary is identified by the path passed via KKERNEL_BENCH_BINARY
    env var or discovered at crates/target/release/kkernel-bench. The live
    daemon keyed on ~/.khive/khive.db is untouched because config_id differs.
  - Both the front-end process and its bench daemon child are terminated on exit.
"""

import csv
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
N_REPS = 5  # repetitions per query for latency measurement

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
    if not contents:
        return 0.0
    return sum(1 for c in contents if expected_topic in c) / len(contents)

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
                 "precision_at_k", "p50_us", "p95_us", "n_queries", "n_corpus"]


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

def _query_once(proc, query_text):
    t0 = time.perf_counter_ns()
    result = _call_verb(proc, "memory.recall", {"query": query_text, "limit": TOP_K, "full_content": True})
    elapsed_us = (time.perf_counter_ns() - t0) // 1000

    if isinstance(result, list):
        arr = result
    elif isinstance(result, dict):
        arr = result.get("results") or result.get("items") or []
    else:
        arr = []

    contents = [r["content"] for r in arr if isinstance(r, dict) and "content" in r]
    return elapsed_us, contents

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
    _assert_not_live_db(bench_db)

    proc = None
    try:
        env = {**os.environ}
        # KHIVE_NO_DAEMON=0 means we WANT daemon forwarding (the production path).
        # Remove any env var that would suppress the daemon.
        env.pop("KHIVE_NO_DAEMON", None)

        print(f"Spawning {binary} mcp --db {bench_db}", flush=True)
        proc = subprocess.Popen(
            [binary, "mcp", "--db", bench_db, "--log", "error"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=env,
        )

        _handshake(proc)
        print("[ok] MCP handshake", flush=True)

        corpus = generate_corpus()
        _ingest(proc, corpus)

        # Assert recall is non-empty before measuring
        _, warmup_hits = _query_once(proc, QUERIES[0][0])
        if not warmup_hits:
            raise RuntimeError(
                f"Recall returned 0 results for warm-up query {QUERIES[0][0]!r}. "
                "FTS and vector paths may both be empty. Check ingest."
            )
        print(f"[ok] Warm-up returned {len(warmup_hits)} hit(s)", flush=True)

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

            all_latencies.extend(rep_latencies)
            p_at_k = precision_at_k(last_contents, expected_topic)
            q_pass = p_at_k >= RECALL_FLOOR
            if not q_pass:
                all_pass = False

            rep_latencies.sort()
            q_p50 = _pct(rep_latencies, 0.5)
            print(
                f"  {expected_topic:<22} P@K={p_at_k:.2f} ({len(last_contents)} hits)"
                f"  p50={q_p50}µs  {'PASS' if q_pass else 'FAIL'}",
                flush=True,
            )
            per_query.append({
                "query": query_text,
                "expected_topic": expected_topic,
                "precision_at_k": p_at_k,
                "top_k_count": len(last_contents),
                "query_pass": q_pass,
                "p50_us": q_p50,
            })

        all_latencies.sort()
        p50_us = _pct(all_latencies, 0.5)
        p95_us = _pct(all_latencies, 0.95)
        mean_pak = sum(r["precision_at_k"] for r in per_query) / len(per_query)

        print(f"\nMean Precision@K={mean_pak:.3f}  floor={RECALL_FLOOR}", flush=True)
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
            # The bench daemon's config_id differs from the live one so we only
            # need to ensure the front-end exits (which drains its daemon child).
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait()

        # Cleanup bench DB
        import shutil
        try:
            shutil.rmtree(tmpdir, ignore_errors=True)
        except Exception:
            pass


if __name__ == "__main__":
    sys.exit(main())
