#!/usr/bin/env python3
"""Product gate: vector round-trip + recall@1 for the memory pack.

Spawns kkernel mcp with an in-memory DB (no --no-embed, so the embedding
model runs live), stores 3 semantically well-separated items, then asserts
that each paraphrase query returns the correct item at rank-1.

This gate must pass before any embed-path changes land (#10 multi-engine
fan-out, #11 knowledge.edit re-embed).

When no embedding model is configured the test prints a clear SKIP line
and exits 0 so default CI (GitHub Actions runners that lack the model) is
not broken.  Set KHIVE_EMBEDDING_MODEL or add [[engines]] to
.khive/config.toml to activate the gate.

Usage:
    uv run python tests/smoke_vector.py
    # or: python3 tests/smoke_vector.py
"""

import json
import os
import subprocess
import sys

BINARY = os.environ.get(
    "KKERNEL_BINARY",
    os.path.join(os.path.dirname(__file__), "..", "crates", "target", "release", "kkernel"),
)

# Fall back to the installed binary when the release build does not exist.
_INSTALLED = os.path.expanduser("~/.cargo/bin/kkernel")
if not os.path.exists(BINARY) and os.path.exists(_INSTALLED):
    BINARY = _INSTALLED

# Repository root: two directories above tests/smoke_vector.py.
# Used to locate .khive/config.toml regardless of the shell working directory
# (ci.sh cd's into crates/ before invoking Python scripts).
_REPO_ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

request_id = 0


def next_id():
    global request_id
    request_id += 1
    return request_id


def send(proc, method, params=None):
    msg = {"jsonrpc": "2.0", "id": next_id(), "method": method}
    if params is not None:
        msg["params"] = params
    proc.stdin.write((json.dumps(msg) + "\n").encode())
    proc.stdin.flush()


def recv(proc):
    line = proc.stdout.readline()
    if not line:
        raise RuntimeError("MCP server closed stdout unexpectedly")
    return json.loads(line)


def _call_request_raw(proc, ops_string):
    send(proc, "tools/call", {"name": "request", "arguments": {"ops": ops_string}})
    resp = recv(proc)
    if "error" in resp:
        raise RuntimeError(f"MCP error: {resp['error']}")
    result = resp.get("result", {})
    if result.get("isError"):
        content = result.get("content", [])
        text = content[0]["text"] if content else "(no text)"
        raise RuntimeError(f"request returned protocol error: {text}")
    content = result.get("content", [])
    text = content[0]["text"] if content else ""
    return json.loads(text) if text else None


def call_verb(proc, name, args):
    ops = json.dumps([{"tool": name, "args": args}])
    body = _call_request_raw(proc, ops)
    if body is None:
        raise RuntimeError(f"request returned empty body for verb {name}")
    results = body.get("results") or []
    if not results:
        raise RuntimeError(f"request returned no results for verb {name}: {body}")
    first = results[0]
    if not first.get("ok", False):
        raise RuntimeError(f"verb {name} failed: {first.get('error', '<no error string>')}")
    return first.get("result")


# ── Embed availability detection ─────────────────────────────────────────────

def _config_has_engines(path):
    """Return True if the file at path contains at least one [[engines]] entry."""
    try:
        with open(path) as fh:
            return "[[engines]]" in fh.read()
    except OSError:
        return False


def _find_embed_config():
    """Return the absolute path of the first config file that declares [[engines]], or None.

    Mirrors kkernel's KhiveConfig::load_with_home_fallback resolution order,
    but roots the project-local search at _REPO_ROOT instead of the process
    working directory.  This lets the test find the project's .khive/config.toml
    regardless of which directory ci.sh happened to cd into before spawning Python.

    Resolution order:
      1. $KHIVE_CONFIG (explicit override)
      2. <repo_root>/khive.toml          (tier 2 — project root)
      3. <repo_root>/.khive/config.toml  (tier 3 — project hidden dir)
      4. ~/.khive/config.toml            (tier 4 — user-global)
    """
    explicit = os.environ.get("KHIVE_CONFIG", "")
    if explicit and _config_has_engines(explicit):
        return os.path.abspath(explicit)

    for rel in ("khive.toml", ".khive/config.toml"):
        p = os.path.join(_REPO_ROOT, rel)
        if _config_has_engines(p):
            return p

    home_cfg = os.path.expanduser("~/.khive/config.toml")
    if _config_has_engines(home_cfg):
        return home_cfg

    return None


def embed_available():
    """Return True if an embedding model is configured for the current environment.

    Checked in priority order:
      - KHIVE_NO_EMBED set (any truthy value) -> False (explicitly disabled)
      - KHIVE_EMBEDDING_MODEL set (non-empty)  -> True  (env-var path)
      - [[engines]] found in config search     -> True  (TOML config path)
      - nothing found                          -> False (skip the gate)
    """
    no_embed = os.environ.get("KHIVE_NO_EMBED", "").strip().lower()
    if no_embed in ("1", "true", "yes", "on"):
        return False
    if os.environ.get("KHIVE_EMBEDDING_MODEL", "").strip():
        return True
    return _find_embed_config() is not None


# ── Process lifecycle ─────────────────────────────────────────────────────────

def spawn():
    """Spawn kkernel mcp with an in-memory DB and embedding enabled.

    Passes --config pointing at the project's .khive/config.toml when found
    so the test works correctly regardless of the shell working directory.
    """
    env = {**os.environ, "KHIVE_NO_DAEMON": "1"}
    cmd = [
        BINARY, "mcp",
        "--db", ":memory:",
        "--log", "error",
        "--pack", "kg",
        "--pack", "memory",
    ]
    cfg = _find_embed_config()
    if cfg:
        cmd += ["--config", cfg]
    return subprocess.Popen(
        cmd,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env=env,
    )


def init_proc(proc):
    send(proc, "initialize", {
        "protocolVersion": "2024-11-05",
        "capabilities": {},
        "clientInfo": {"name": "vector-gate", "version": "0.1.0"},
    })
    recv(proc)
    notify = {"jsonrpc": "2.0", "method": "notifications/initialized"}
    proc.stdin.write((json.dumps(notify) + "\n").encode())
    proc.stdin.flush()


# ── Test data ─────────────────────────────────────────────────────────────────

# Three topics chosen to be maximally distinct so ranking is deterministic.
ITEMS = [
    (
        "networking",
        "TCP three-way handshake SYN ACK networking connection protocol packet routing",
    ),
    (
        "cooking",
        "Sourdough bread fermentation wild yeast lactic acid bacteria baking flour dough",
    ),
    (
        "astronomy",
        "Black hole event horizon Schwarzschild radius gravitational singularity spacetime",
    ),
]

# Each query is a clear paraphrase of exactly one item above.
QUERIES = [
    ("networking", "network handshake connection protocol TCP"),
    ("cooking",    "bread baking yeast fermentation sourdough"),
    ("astronomy",  "black hole gravity event horizon spacetime"),
]


def test_vector_round_trip_and_recall_at_1():
    """
    Core assertion: for each paraphrase query the correct item is rank-1.

    Also asserts:
    - recall returns a list with at least one hit (vectors were written)
    - the top hit carries a plausible score (>= 0.5), proving semantic ranking
      rather than random or FTS-only ordering
    - a second query for a different topic returns a DIFFERENT rank-1 item,
      confirming the index is not degenerate (always returning the same row)
    """
    proc = spawn()
    try:
        init_proc(proc)

        # Store all items and record the returned ids.
        stored_ids = {}
        for key, content in ITEMS:
            result = call_verb(proc, "memory.remember", {
                "content": content,
                "salience": 0.9,
                "memory_type": "semantic",
            })
            assert result is not None, f"memory.remember must return a result for {key!r}"
            item_id = result.get("id")
            assert item_id, f"memory.remember must return an id for {key!r}: {result}"
            stored_ids[key] = item_id
            print(f"  [store] {key}: id={item_id}")

        assert len(stored_ids) == len(ITEMS), (
            f"expected {len(ITEMS)} stored items, got {len(stored_ids)}"
        )

        # For each query, assert rank-1 is the matching item.
        rank_1_ids = []
        for target_key, query in QUERIES:
            hits = call_verb(proc, "memory.recall", {
                "query": query,
                "limit": len(ITEMS),
            })

            # Vectors actually written: recall must return at least one hit.
            assert isinstance(hits, list) and len(hits) >= 1, (
                f"memory.recall for {target_key!r} must return >= 1 hit; "
                f"got {hits!r}. Vectors may not have been written."
            )

            rank_1 = hits[0]
            rank_1_id = rank_1.get("id")
            rank_1_score = rank_1.get("score", rank_1.get("rank_score", 0.0))
            expected_id = stored_ids[target_key]

            # recall@1: the correct item must be at rank-1.
            assert rank_1_id == expected_id, (
                f"recall@1 FAILED for query={query!r}: "
                f"expected id={expected_id!r} ({target_key}), "
                f"got id={rank_1_id!r} (score={rank_1_score:.3f}). "
                f"Full hits: {hits}"
            )

            # Plausible score: semantic match must score meaningfully above zero.
            assert rank_1_score >= 0.5, (
                f"rank-1 score {rank_1_score:.3f} < 0.5 for query={query!r}; "
                f"suggests random ordering, not semantic ranking."
            )

            rank_1_ids.append(rank_1_id)
            print(
                f"  [recall@1] {target_key}: "
                f"rank-1 id={rank_1_id} score={rank_1_score:.3f} -- CORRECT"
            )

        # Non-degenerate: the index must not always return the same row.
        assert len(set(rank_1_ids)) > 1, (
            f"All queries returned the same rank-1 id ({rank_1_ids[0]!r}). "
            f"The index is degenerate -- every query hits the same row."
        )
        print(f"  [non-degenerate] {len(set(rank_1_ids))} distinct rank-1 ids across {len(QUERIES)} queries -- ok")

    finally:
        proc.stdin.close()
        proc.wait(timeout=10)


def main():
    print(f"Binary: {BINARY}")
    if not os.path.exists(BINARY):
        print(f"FAIL: binary not found: {BINARY}")
        return 1

    if not embed_available():
        print("SKIP: no embedding model configured (KHIVE_EMBEDDING_MODEL not set,")
        print("  no [[engines]] in khive.toml / .khive/config.toml / ~/.khive/config.toml).")
        print("  Set KHIVE_EMBEDDING_MODEL or add [[engines]] to .khive/config.toml to enable.")
        return 0

    try:
        test_vector_round_trip_and_recall_at_1()
    except Exception as exc:
        print(f"\nFAIL: {exc}")
        return 1

    print("\nPASS: vector round-trip + recall@1 gate")
    return 0


if __name__ == "__main__":
    sys.exit(main())
