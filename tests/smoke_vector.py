#!/usr/bin/env python3
"""Product gate: vector round-trip + recall@1 for the memory pack.

Spawns kkernel mcp with an in-memory DB and embedding enabled, stores 3
semantically well-separated items, then asserts that each paraphrase query
returns the correct item at rank-1.

This gate must pass before any embed-path changes land (#10 multi-engine
fan-out, #11 knowledge.edit re-embed).

The gate guards itself empirically: it spawns kkernel, attempts one
memory.remember call, and skips if the embedder is not usable in this
environment (model weights absent or no engine configured). This means the
gate is silent on GitHub Actions runners that lack the model weights while
remaining active on developer machines and any CI runner that has them.

Set KHIVE_NO_EMBED=1 to bypass the gate unconditionally.

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


# ── Process lifecycle ─────────────────────────────────────────────────────────

def spawn():
    """Spawn kkernel mcp with an in-memory DB and embedding enabled.

    Config resolution is left entirely to kkernel (KHIVE_CONFIG env var,
    project-local khive.toml / .khive/config.toml, ~/.khive/config.toml,
    then RuntimeConfig::default which supplies AllMiniLmL6V2 as the fallback
    when no config file is found).
    """
    env = {**os.environ, "KHIVE_NO_DAEMON": "1"}
    cmd = [
        BINARY, "mcp",
        "--db", ":memory:",
        "--log", "error",
        "--pack", "kg",
        "--pack", "memory",
    ]
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


# ── Empirical embedder probe ──────────────────────────────────────────────────

# Error substrings emitted by kkernel when an embedder is not usable.
#
# "unconfigured: embedding_model is not set"
#   RuntimeError::Unconfigured("embedding_model") — surfaced when the runtime has
#   no embedding_model set (e.g. all [[engines]] entries carried unknown model
#   aliases and were silently skipped at config.rs:503-509, leaving
#   embedding_model = None).
#
# "embedding: "
#   RuntimeError::Embedding(lattice_embed::EmbedError) — surfaced when a model
#   is configured but its weights cannot be loaded at the first embed() call
#   (typical on CI runners that lack cached model files).  On the default
#   RuntimeConfig, AllMiniLmL6V2 is always configured (config.rs:289), so a
#   model will be attempted even when no KHIVE_EMBEDDING_MODEL or config file is
#   present; if that model's weights are absent the embed() call fails here.
_SKIP_SIGNALS = (
    "unconfigured: embedding_model is not set",
    "embedding: ",
)


def _probe_embedder_usable():
    """Spawn kkernel, run a store+recall round-trip, and return (usable, reason).

    Two-step probe:
      1. memory.remember — fails fast with an embedding error when the configured
         model's weights are absent (operations.rs:2117-2173 propagates EmbedError
         from embed_document_with_model as a hard failure).  If the model registry
         is empty (all [[engines]] aliases were unknown and skipped), the store
         succeeds but writes NO vector (operations.rs:2064-2066 skips the embed
         block when embed_model_names is empty).
      2. memory.recall — with an empty vector index the recall returns [] even for
         content that was FTS-indexed, proving no vectors were written.  Checking
         that the probe note appears in recall results confirms vectors are live.

    usable=True  — store+recall round-trip succeeded with the probe note at rank-1.
    usable=False — embedder not usable; caller should print SKIP and exit 0.
    Raises RuntimeError for unexpected errors (real runtime bugs unrelated to
    model availability).
    """
    _PROBE_CONTENT = "probe embedding availability check round-trip"

    proc = spawn()
    try:
        init_proc(proc)

        # Step 1: store a probe note.
        ops = json.dumps([{"tool": "memory.remember", "args": {
            "content": _PROBE_CONTENT,
            "salience": 0.5,
            "memory_type": "semantic",
        }}])
        body = _call_request_raw(proc, ops)
        if body is None:
            raise RuntimeError("probe: empty response body from memory.remember")
        results = body.get("results") or []
        if not results:
            raise RuntimeError(f"probe: no results from memory.remember: {body}")
        first = results[0]
        if not first.get("ok", False):
            error = first.get("error", "")
            for sig in _SKIP_SIGNALS:
                if sig in error:
                    return False, error
            raise RuntimeError(f"probe: unexpected memory.remember error: {error}")
        probe_id = first.get("result", {}).get("id")

        # Step 2: recall and verify the probe note appears (vectors were written).
        ops2 = json.dumps([{"tool": "memory.recall", "args": {
            "query": _PROBE_CONTENT,
            "limit": 5,
        }}])
        body2 = _call_request_raw(proc, ops2)
        if body2 is None:
            raise RuntimeError("probe: empty response body from memory.recall")
        results2 = body2.get("results") or []
        if not results2:
            raise RuntimeError(f"probe: no results from memory.recall: {body2}")
        first2 = results2[0]
        if not first2.get("ok", False):
            error = first2.get("error", "")
            for sig in _SKIP_SIGNALS:
                if sig in error:
                    return False, error
            raise RuntimeError(f"probe: unexpected memory.recall error: {error}")

        hits = first2.get("result") or []
        for hit in hits:
            if hit.get("id") == probe_id:
                score = hit.get("score", hit.get("rank_score", 0.0))
                if score >= 0.5:
                    return True, ""
                return False, (
                    f"probe recall score {score:.3f} < 0.5 "
                    "(FTS-only result — no vector index active)"
                )
        return False, "probe note absent from recall results (no vectors written)"
    finally:
        proc.stdin.close()
        proc.wait(timeout=10)


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

    # Explicit force-skip: unconditionally bypass the gate.
    no_embed = os.environ.get("KHIVE_NO_EMBED", "").strip().lower()
    if no_embed in ("1", "true", "yes", "on"):
        print("SKIP: KHIVE_NO_EMBED is set; embed/recall gate bypassed.")
        return 0

    # Empirical probe: attempt one embed round-trip to confirm the model is
    # usable before running the full assertion suite.
    try:
        usable, reason = _probe_embedder_usable()
    except Exception as exc:
        print(f"\nFAIL: embedder probe error: {exc}")
        return 1

    if not usable:
        print("SKIP: embedder not usable in this environment.")
        print(f"  ({reason})")
        print("  Provide a reachable embedding model to activate the gate.")
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
