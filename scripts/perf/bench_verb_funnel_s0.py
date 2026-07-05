#!/usr/bin/env python3
"""S0 verb-latency funnel — per-verb request latency through the real kkernel
binary, in-process (KHIVE_NO_DAEMON=1) so handler-level profiling env vars
(KHIVE_RECALL_PROFILE, KHIVE_CONTEXT_PROFILE) land on OUR stderr pipe instead
of being swallowed by Stdio::null() on the detached daemon child.

Measures against two corpora:
  1. --real-db <path>   a point-in-time snapshot of production khive.db
     (real MiniLM embeddings, real graph/atom scale as it exists today)
  2. --synth-db <path>  a direct-SQL-seeded structural KG (no embeddings)
     built by gen_synthetic_kg_s0.py, sized to the S0 task's requested
     >=100k-entity / >=500k-edge / >=50k-note floor for the pure graph verbs
     (neighbors/traverse/search-FTS-leg) that don't depend on embeddings.

NEVER opens ~/.khive/khive.db directly — callers must pass a scratch copy.
Safety guard below refuses to run against the live path regardless.

Output: a JSON results file (--out) with per-verb p50/p95/n, phase splits
where the handler emits them, and load-average/corpus-size provenance.
"""

from __future__ import annotations

import argparse
import json
import os
import pathlib
import statistics
import subprocess
import sys
import time

_LIVE_DB_PATHS = frozenset(
    str(pathlib.Path(p).expanduser().resolve())
    for p in ("~/.khive/khive.db", "~/.khive/khive-graph.db")
)

# The real HOME has a ~/.khive/config.toml declaring [[backends]]; kkernel
# refuses to combine that with an explicit --db/KHIVE_DB override (ambiguous
# which declared backend the override replaces). Point HOME at an empty
# scratch dir for spawned processes so config discovery finds nothing and
# KHIVE_DB is unambiguous.
_FAKE_HOME = "/tmp/khive-s0-fakehome"
pathlib.Path(_FAKE_HOME).mkdir(parents=True, exist_ok=True)


def assert_not_live_db(path: str) -> None:
    resolved = str(pathlib.Path(path).resolve())
    if resolved in _LIVE_DB_PATHS:
        print(f"FATAL: refusing to open live DB {path!r} for a bench run.", file=sys.stderr)
        sys.exit(2)


# ── MCP stdio JSON-RPC driver (same wire pattern as bench_pipeline_daemon.py) ──

class McpSession:
    def __init__(self, binary, db_path, extra_env=None, packs=None, log="warn"):
        assert_not_live_db(db_path)
        env = {**os.environ}
        env["HOME"] = _FAKE_HOME
        env["KHIVE_DB"] = db_path
        env["KHIVE_NO_DAEMON"] = "1"
        if packs:
            env["KHIVE_PACKS"] = packs
        if extra_env:
            env.update(extra_env)
        self.proc = subprocess.Popen(
            [binary, "mcp", "--log", log],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=env,
            bufsize=0,
        )
        self._req_id = 0
        self._stderr_lines = []
        self._handshake()

    def _next_id(self):
        self._req_id += 1
        return self._req_id

    def _send(self, method, params=None):
        msg = {"jsonrpc": "2.0", "id": self._next_id(), "method": method}
        if params is not None:
            msg["params"] = params
        self.proc.stdin.write((json.dumps(msg) + "\n").encode())
        self.proc.stdin.flush()

    def _recv(self):
        line = self.proc.stdout.readline()
        if not line:
            self._drain_stderr()
            raise RuntimeError(
                f"kkernel closed stdout unexpectedly; stderr tail:\n"
                + "".join(self._stderr_lines[-40:])
            )
        return json.loads(line)

    def _handshake(self):
        self._send("initialize", {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "bench-verb-funnel-s0", "version": "1.0.0"},
        })
        init = self._recv()
        if "error" in init:
            raise RuntimeError(f"initialize failed: {init['error']}")
        notify = {"jsonrpc": "2.0", "method": "notifications/initialized"}
        self.proc.stdin.write((json.dumps(notify) + "\n").encode())
        self.proc.stdin.flush()

    def call_verb(self, verb, args):
        ops = json.dumps([{"tool": verb, "args": args}])
        self._send("tools/call", {"name": "request", "arguments": {"ops": ops}})
        resp = self._recv()
        if "error" in resp:
            raise RuntimeError(f"MCP RPC error calling {verb}: {resp['error']}")
        result = resp.get("result", {})
        if result.get("isError"):
            content = result.get("content", [])
            text = content[0]["text"] if content else "(no text)"
            raise RuntimeError(f"request returned protocol error for {verb}: {text}")
        content = result.get("content", [])
        text = content[0]["text"] if content else ""
        body = json.loads(text) if text else None
        if body is None:
            raise RuntimeError(f"empty response for verb {verb}")
        results = body.get("results") or []
        if not results:
            raise RuntimeError(f"no results in response for verb {verb}: {body}")
        first = results[0]
        if not first.get("ok", False):
            raise RuntimeError(f"verb {verb} failed: {first.get('error', '<no error>')}")
        return first.get("result")

    def drain_new_stderr(self):
        """Non-blocking best-effort drain of stderr accumulated since last call.

        The daemon-free front end writes profiling `eprintln!` JSON lines to
        its own stderr synchronously as part of handling the request, so by
        the time call_verb() returns, any KHIVE_RECALL_PROFILE / KHIVE_CONTEXT_
        PROFILE lines for that call are already flushed and waiting to be
        read. We switch the fd nonblocking once and read whatever is ready.
        """
        import fcntl
        fd = self.proc.stderr.fileno()
        fl = fcntl.fcntl(fd, fcntl.F_GETFL)
        fcntl.fcntl(fd, fcntl.F_SETFL, fl | os.O_NONBLOCK)
        lines = []
        try:
            while True:
                chunk = self.proc.stderr.read(65536)
                if not chunk:
                    break
                lines.append(chunk.decode(errors="replace"))
        except (BlockingIOError, TypeError):
            pass
        finally:
            fcntl.fcntl(fd, fcntl.F_SETFL, fl)
        text = "".join(lines)
        self._stderr_lines.extend(text.splitlines(keepends=True))
        return text

    def _drain_stderr(self):
        try:
            self.drain_new_stderr()
        except Exception:
            pass

    def close(self):
        try:
            self.proc.stdin.close()
        except Exception:
            pass
        try:
            self.proc.wait(timeout=5)
        except Exception:
            self.proc.kill()
            self.proc.wait()


# ── Stats helpers ─────────────────────────────────────────────────────────────

def pct(data, p):
    if not data:
        return None
    s = sorted(data)
    k = (len(s) - 1) * (p / 100.0)
    f = int(k)
    c = min(f + 1, len(s) - 1)
    if f == c:
        return s[f]
    return s[f] + (s[c] - s[f]) * (k - f)


def parse_phase_lines(stderr_text):
    """Parse `{"c":ID,"s":"stage","us":N[,"n":M]}` plog lines into stage->[us]."""
    phases = {}
    for line in stderr_text.splitlines():
        line = line.strip()
        if not line.startswith("{"):
            continue
        try:
            obj = json.loads(line)
        except json.JSONDecodeError:
            continue
        if "s" in obj and "us" in obj:
            phases.setdefault(obj["s"], []).append(obj["us"])
    return phases


def merge_phase_maps(maps):
    out = {}
    for m in maps:
        for k, v in m.items():
            out.setdefault(k, []).extend(v)
    return out


# ── Measurement ────────────────────────────────────────────────────────────────

def measure_verb(session, label, verb, args, reps, warmup):
    for _ in range(warmup):
        session.call_verb(verb, args)
        session.drain_new_stderr()

    latencies_ms = []
    phase_maps = []
    for _ in range(reps):
        t0 = time.perf_counter()
        session.call_verb(verb, args)
        dt = (time.perf_counter() - t0) * 1000.0
        latencies_ms.append(dt)
        stderr_text = session.drain_new_stderr()
        phases = parse_phase_lines(stderr_text)
        if phases:
            phase_maps.append(phases)

    merged_phases = merge_phase_maps(phase_maps) if phase_maps else {}
    phase_summary = {
        stage: {"p50_us": pct(vals, 50), "p95_us": pct(vals, 95), "n": len(vals)}
        for stage, vals in merged_phases.items()
    }

    return {
        "label": label,
        "verb": verb,
        "n": reps,
        "warmup": warmup,
        "p50_ms": round(pct(latencies_ms, 50), 3),
        "p95_ms": round(pct(latencies_ms, 95), 3),
        "mean_ms": round(statistics.mean(latencies_ms), 3),
        "phases": phase_summary,
    }


def loadavg():
    try:
        out = subprocess.check_output(["sysctl", "-n", "vm.loadavg"], stderr=subprocess.DEVNULL)
        s = out.decode().strip().strip("{}").strip()
        parts = [float(x) for x in s.split()]
        return {"1m": parts[0], "5m": parts[1], "15m": parts[2]}
    except Exception:
        return None


def git_sha(repo_root):
    try:
        out = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=str(repo_root), stderr=subprocess.DEVNULL)
        return out.decode().strip()
    except Exception:
        return "unknown"


def corpus_stats(db_path):
    """Best-effort row counts for provenance — read-only, separate sqlite3 CLI call."""
    tables = {
        "entities": "SELECT count(*) FROM entities WHERE deleted_at IS NULL",
        "notes": "SELECT count(*) FROM notes WHERE deleted_at IS NULL",
        "graph_edges": "SELECT count(*) FROM graph_edges WHERE deleted_at IS NULL",
        "memory_notes": "SELECT count(*) FROM notes WHERE deleted_at IS NULL AND kind='memory'",
        "knowledge_atoms": "SELECT count(*) FROM knowledge_atoms",
        "knowledge_sections": "SELECT count(*) FROM knowledge_sections",
        "vec_all_minilm_l6_v2": "SELECT count(*) FROM vec_all_minilm_l6_v2_rowids",
    }
    stats = {}
    for name, q in tables.items():
        try:
            out = subprocess.check_output(
                ["sqlite3", db_path, q], stderr=subprocess.DEVNULL
            ).decode().strip()
            stats[name] = int(out) if out else 0
        except Exception:
            stats[name] = None
    return stats


def pick_hub_node(db_path, min_degree=1):
    """Entity id with the most outgoing edges — best structural target for neighbors/traverse."""
    q = (
        "SELECT source_id, count(*) c FROM graph_edges WHERE deleted_at IS NULL "
        "GROUP BY source_id ORDER BY c DESC LIMIT 1"
    )
    try:
        out = subprocess.check_output(["sqlite3", db_path, q], stderr=subprocess.DEVNULL).decode().strip()
        if not out:
            return None
        node_id = out.split("|")[0]
        return node_id
    except Exception:
        return None


# ── Battery definitions ────────────────────────────────────────────────────────

def build_battery(db_stats, hub_id, mode):
    battery = []
    battery.append(("stats_baseline", "stats", {}))

    if mode == "real":
        battery.append(("memory.recall", "memory.recall", {"query": "khive verb latency benchmark ANN fusion recall", "limit": 10}))
        battery.append(("kg.search.entity", "search", {"kind": "entity", "query": "knowledge graph retrieval benchmark semantic", "limit": 20}))
        battery.append(("kg.search.note", "search", {"kind": "note", "query": "khive verb latency benchmark ANN fusion recall", "limit": 20}))
        battery.append(("knowledge.compose", "knowledge.compose", {"query": "khive verb latency benchmark request DSL runtime performance optimization", "max_tokens": 4000}))
        battery.append(("knowledge.search", "knowledge.search", {"query": "khive verb latency benchmark request DSL runtime performance optimization", "rerank": True}))
        battery.append(("comm.inbox", "comm.inbox", {"limit": 10}))
    else:
        battery.append(("kg.search.entity.fts_only", "search", {"kind": "entity", "query": "synthetic scale entity benchmark corpus", "limit": 20}))
        battery.append(("kg.search.note.fts_only", "search", {"kind": "note", "query": "synthetic scale note benchmark corpus", "limit": 20}))

    if hub_id:
        battery.append(("kg.neighbors.both", "neighbors", {"node_id": hub_id, "direction": "both"}))
        battery.append(("kg.traverse.depth2", "traverse", {"roots": [hub_id], "max_depth": 2, "direction": "out"}))
        battery.append(("kg.traverse.depth3", "traverse", {"roots": [hub_id], "max_depth": 3, "direction": "out"}))
        battery.append(("kg.context", "context", {"query": "khive verb latency benchmark funnel", "entity_ids": [hub_id]}))
    else:
        battery.append(("kg.context.query_only", "context", {"query": "khive verb latency benchmark funnel"}))

    battery.append(("kg.create.entity", "create", {"kind": "entity", "entity_kind": "concept", "name": "S0BenchEntity", "description": "s0 verb funnel synthetic create-cost probe"}))
    battery.append(("kg.create.note", "create", {"kind": "observation", "content": "s0 verb funnel synthetic create-cost probe note"}))

    return battery


# ── Main ────────────────────────────────────────────────────────────────────────

def run_mode(binary, db_path, mode, reps, warmup, extra_env=None):
    assert_not_live_db(db_path)
    stats = corpus_stats(db_path)
    hub_id = pick_hub_node(db_path)

    env = {"KHIVE_RECALL_PROFILE": "1", "KHIVE_CONTEXT_PROFILE": "1"}
    if extra_env:
        env.update(extra_env)

    session = McpSession(binary, db_path, extra_env=env)
    try:
        battery = build_battery(stats, hub_id, mode)
        results = []
        for label, verb, args in battery:
            try:
                r = measure_verb(session, label, verb, args, reps=reps, warmup=warmup)
                results.append(r)
                print(f"  [{mode}] {label:<28} p50={r['p50_ms']:>9.3f}ms p95={r['p95_ms']:>9.3f}ms n={r['n']}", flush=True)
            except Exception as e:
                print(f"  [{mode}] {label:<28} FAILED: {e}", file=sys.stderr, flush=True)
                results.append({"label": label, "verb": verb, "error": str(e)})
    finally:
        session.close()

    return {
        "mode": mode,
        "db_path": db_path,
        "corpus_stats": stats,
        "hub_id": hub_id,
        "results": results,
    }


def one_shot_cold_start(binary, db_path, n=5):
    """Compare cold `kkernel exec`-style one-shot process spawns against the
    persistent-session numbers above, per the S0 task's explicit ask."""
    assert_not_live_db(db_path)
    lat = []
    for _ in range(n):
        env = {**os.environ, "KHIVE_DB": db_path, "KHIVE_NO_DAEMON": "1"}
        t0 = time.perf_counter()
        session = McpSession(binary, db_path, extra_env={})
        session.call_verb("stats", {})
        session.close()
        lat.append((time.perf_counter() - t0) * 1000.0)
    return {"n": n, "p50_ms": round(pct(lat, 50), 3), "p95_ms": round(pct(lat, 95), 3), "raw_ms": [round(x, 3) for x in lat]}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--binary", default=None)
    ap.add_argument("--real-db", default=None)
    ap.add_argument("--synth-db", default=None)
    ap.add_argument("--reps", type=int, default=25)
    ap.add_argument("--warmup", type=int, default=5)
    ap.add_argument("--out", required=True)
    args = ap.parse_args()

    repo_root = pathlib.Path(__file__).parent.parent.parent
    binary = args.binary or str(repo_root / "crates" / "target" / "release" / "kkernel")
    if not pathlib.Path(binary).exists():
        print(f"FATAL: binary not found at {binary}", file=sys.stderr)
        sys.exit(2)

    out = {
        "schema_version": "1.0",
        "produced_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "git_sha": git_sha(repo_root),
        "loadavg_start": loadavg(),
        "binary": binary,
        "modes": [],
    }

    if args.real_db:
        print(f"=== real corpus: {args.real_db} ===", flush=True)
        out["modes"].append(run_mode(binary, args.real_db, "real", args.reps, args.warmup))
        out["one_shot_cold_start_real"] = one_shot_cold_start(binary, args.real_db)

    if args.synth_db:
        print(f"=== synthetic corpus: {args.synth_db} ===", flush=True)
        out["modes"].append(run_mode(binary, args.synth_db, "synth", args.reps, args.warmup))

    out["loadavg_end"] = loadavg()

    outpath = pathlib.Path(args.out)
    outpath.parent.mkdir(parents=True, exist_ok=True)
    outpath.write_text(json.dumps(out, indent=2))
    print(f"\nWrote results to {outpath}", flush=True)


if __name__ == "__main__":
    main()
