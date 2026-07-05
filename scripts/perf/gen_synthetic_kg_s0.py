#!/usr/bin/env python3
"""Build a direct-SQL-seeded structural KG for the S0 verb-latency funnel.

Targets the S0 task's requested floor for the pure graph-structural verbs
(neighbors/traverse/search) that don't depend on embeddings: >=100k entities,
>=500k edges, >=50k notes.

Two-step build (the "direct-build" pattern — see khive memory
reference_khive_native_db_direct_build):
  1. Spawn the real kkernel binary against a FRESH empty path and issue ONE
     create(entity) + create(note) call through the verb layer. This makes
     the runtime apply its idempotent schema bootstrap for the 'local'
     namespace — base tables, indexes, AND the per-namespace FTS5 shadow
     tables + sync triggers (fts_entities_local / fts_notes_local). Those
     triggers only get wired up once a real write touches that namespace;
     without this step, raw bulk INSERTs below would land in the base
     tables but never reach FTS, and `search()` would return nothing.
  2. Shut the process down cleanly (WAL checkpoint), then bulk-INSERT the
     remaining rows directly with stdlib sqlite3, batched executemany in one
     transaction. This is orders of magnitude faster than 100k individual
     async verb-dispatch calls and is legitimate here because these rows
     don't need embeddings — neighbors/traverse are pure graph BFS over
     graph_edges, and this corpus's search() numbers are reported as an
     FTS-only leg (no vector table is created — vec0 is intentionally
     skipped, matching the direct-build pattern).

NEVER touches ~/.khive/khive.db — refuses if --db resolves to it.
"""

from __future__ import annotations

import argparse
import json
import os
import pathlib
import random
import sqlite3
import subprocess
import sys
import time
import uuid

_LIVE_DB_PATHS = frozenset(
    str(pathlib.Path(p).expanduser().resolve())
    for p in ("~/.khive/khive.db", "~/.khive/khive-graph.db")
)

# See bench_verb_funnel_s0.py: real HOME's ~/.khive/config.toml declares
# [[backends]], which conflicts with an explicit KHIVE_DB override.
_FAKE_HOME = "/tmp/khive-s0-fakehome"
pathlib.Path(_FAKE_HOME).mkdir(parents=True, exist_ok=True)


def assert_not_live_db(path: str) -> None:
    resolved = str(pathlib.Path(path).resolve())
    if resolved in _LIVE_DB_PATHS:
        print(f"FATAL: refusing to build synthetic corpus over live DB {path!r}.", file=sys.stderr)
        sys.exit(2)


def bootstrap_namespace(binary: str, db_path: str) -> None:
    """One real create(entity)+create(note) call to wire up FTS for 'local'."""
    env = {**os.environ, "HOME": _FAKE_HOME, "KHIVE_DB": db_path, "KHIVE_NO_DAEMON": "1"}
    proc = subprocess.Popen(
        [binary, "mcp", "--log", "warn"],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        env=env,
    )
    try:
        def send(method, params=None):
            msg = {"jsonrpc": "2.0", "id": 1, "method": method}
            if params is not None:
                msg["params"] = params
            proc.stdin.write((json.dumps(msg) + "\n").encode())
            proc.stdin.flush()

        def recv():
            line = proc.stdout.readline()
            if not line:
                err = proc.stderr.read().decode(errors="replace")
                raise RuntimeError(f"kkernel closed stdout during bootstrap; stderr:\n{err}")
            return json.loads(line)

        send("initialize", {
            "protocolVersion": "2024-11-05", "capabilities": {},
            "clientInfo": {"name": "gen-synthetic-kg-s0", "version": "1.0.0"},
        })
        recv()
        notify = {"jsonrpc": "2.0", "method": "notifications/initialized"}
        proc.stdin.write((json.dumps(notify) + "\n").encode())
        proc.stdin.flush()

        for i, ops in enumerate([
            json.dumps([{"tool": "create", "args": {
                "kind": "entity", "entity_kind": "concept",
                "name": "BootstrapEntity", "description": "bootstrap namespace for FTS wiring",
            }}]),
            json.dumps([{"tool": "create", "args": {
                "kind": "observation", "content": "bootstrap note for FTS wiring",
            }}]),
        ], start=2):
            send("tools/call", {"name": "request", "arguments": {"ops": ops}})
            resp = recv()
            if "error" in resp:
                raise RuntimeError(f"bootstrap call failed: {resp['error']}")
    finally:
        try:
            proc.stdin.close()
        except Exception:
            pass
        try:
            proc.wait(timeout=10)
        except Exception:
            proc.kill()
            proc.wait()
    # Give SQLite a moment to fully release the WAL before raw writes.
    time.sleep(1.0)


CONCEPT_WORDS = [
    "graph", "vector", "index", "fusion", "latency", "throughput", "embedding",
    "retrieval", "traversal", "ranking", "schema", "migration", "namespace",
    "daemon", "profile", "recall", "search", "cache", "budget", "funnel",
]


def bulk_seed(db_path: str, n_entities: int, n_edges: int, n_notes: int, seed: int = 0) -> None:
    rng = random.Random(seed)
    now_us = int(time.time() * 1_000_000)
    conn = sqlite3.connect(db_path)
    conn.execute("PRAGMA journal_mode=WAL")
    conn.execute("PRAGMA synchronous=NORMAL")

    print(f"seeding {n_entities} entities...", flush=True)
    entity_ids = [str(uuid.uuid4()) for _ in range(n_entities)]
    rows = []
    for i, eid in enumerate(entity_ids):
        w0, w1, w2 = (CONCEPT_WORDS[(i + k) % len(CONCEPT_WORDS)] for k in (0, 1, 3))
        rows.append((
            eid, "local", "entity", "concept",
            f"SynthEntity{i:06d}",
            f"synthetic scale entity {w0} {w1} {w2} benchmark corpus node {i}",
            "{}", "[]", now_us, now_us,
        ))
    with conn:
        conn.executemany(
            "INSERT INTO entities (id, namespace, kind, entity_type, name, description, "
            "properties, tags, created_at, updated_at) VALUES (?,?,?,?,?,?,?,?,?,?)",
            rows,
        )
    print(f"seeded {n_entities} entities.", flush=True)

    print(f"seeding {n_edges} edges (scale-free-ish hub bias)...", flush=True)
    # Bias source selection toward a small hub set so neighbors()/traverse()
    # on a hub node has a realistic large fan-out to measure, while the bulk
    # of edges scatter across the rest of the corpus.
    hub_count = max(1, n_entities // 1000)
    hub_ids = entity_ids[:hub_count]
    relations = ["contains", "extends", "depends_on", "related_to".replace("related_to", "extends")]
    edge_rows = []
    seen_pairs = set()
    batch_size = 50_000
    edges_written = 0
    while edges_written < n_edges:
        batch = []
        target_n = min(batch_size, n_edges - edges_written)
        for _ in range(target_n):
            if rng.random() < 0.3:
                src = rng.choice(hub_ids)
            else:
                src = entity_ids[rng.randrange(n_entities)]
            tgt = entity_ids[rng.randrange(n_entities)]
            if src == tgt:
                continue
            key = (src, tgt)
            if key in seen_pairs:
                continue
            seen_pairs.add(key)
            rel = "contains" if rng.random() < 0.7 else "extends"
            batch.append((
                "local", str(uuid.uuid4()), src, tgt, rel, 1.0, now_us, now_us,
            ))
        with conn:
            conn.executemany(
                "INSERT OR IGNORE INTO graph_edges (namespace, id, source_id, target_id, "
                "relation, weight, created_at, updated_at) VALUES (?,?,?,?,?,?,?,?)",
                batch,
            )
        edges_written += len(batch)
        print(f"  edges: {edges_written}/{n_edges}", flush=True)

    print(f"seeding {n_notes} notes...", flush=True)
    note_rows = []
    for i in range(n_notes):
        w0, w1 = (CONCEPT_WORDS[(i + k) % len(CONCEPT_WORDS)] for k in (0, 2))
        note_rows.append((
            str(uuid.uuid4()), "local", "observation", "active",
            None,
            f"synthetic scale note {w0} {w1} benchmark corpus observation {i}",
            None, None, None, "{}", now_us, now_us,
        ))
    with conn:
        conn.executemany(
            "INSERT INTO notes (id, namespace, kind, status, name, content, salience, "
            "decay_factor, expires_at, properties, created_at, updated_at) "
            "VALUES (?,?,?,?,?,?,?,?,?,?,?,?)",
            note_rows,
        )
    print(f"seeded {n_notes} notes.", flush=True)

    conn.execute("PRAGMA wal_checkpoint(TRUNCATE)")
    conn.close()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--binary", required=True)
    ap.add_argument("--db", required=True)
    ap.add_argument("--entities", type=int, default=100_000)
    ap.add_argument("--edges", type=int, default=500_000)
    ap.add_argument("--notes", type=int, default=50_000)
    ap.add_argument("--force", action="store_true", help="rebuild even if --db already exists")
    args = ap.parse_args()

    assert_not_live_db(args.db)
    db_path = pathlib.Path(args.db)

    if db_path.exists() and not args.force:
        conn = sqlite3.connect(str(db_path))
        try:
            n = conn.execute("SELECT count(*) FROM entities").fetchone()[0]
        except Exception:
            n = 0
        conn.close()
        if n >= args.entities:
            print(f"{db_path} already has {n} entities (>= target {args.entities}); skipping build. Use --force to rebuild.")
            return
        print(f"{db_path} exists but only has {n} entities; rebuilding.")
        db_path.unlink()
        for suffix in ("-wal", "-shm", "-journal"):
            p = pathlib.Path(str(db_path) + suffix)
            if p.exists():
                p.unlink()
    elif db_path.exists() and args.force:
        db_path.unlink()
        for suffix in ("-wal", "-shm", "-journal"):
            p = pathlib.Path(str(db_path) + suffix)
            if p.exists():
                p.unlink()

    db_path.parent.mkdir(parents=True, exist_ok=True)

    print(f"bootstrapping namespace/FTS wiring at {db_path}...", flush=True)
    bootstrap_namespace(args.binary, str(db_path))

    t0 = time.time()
    bulk_seed(str(db_path), args.entities, args.edges, args.notes)
    print(f"done in {time.time() - t0:.1f}s", flush=True)


if __name__ == "__main__":
    main()
