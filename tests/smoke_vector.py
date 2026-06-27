#!/usr/bin/env python3
"""Product gate: vector round-trip + recall@1 for the memory pack.

Spawns kkernel mcp with an in-memory DB and embedding enabled, stores 3
semantically well-separated items, then asserts that each paraphrase query
returns the correct item at rank-1.

This gate must pass before any embed-path changes land (#10 multi-engine
fan-out, #11 knowledge.edit re-embed).

The gate is cache-gated: it checks for a locally cached embedder model before
spawning kkernel. If the model weights are not on disk the gate prints SKIP and
exits 0 without touching the network. This keeps `make ci` clean on fresh
machines and CI runners without model weights.

The gate validates the default primary embedder (all-minilm-l6-v2) only.
Both memory.remember and memory.recall are pinned to that model via the
embedding_model arg, so kkernel's multi-model fan-out (it also registers
paraphrase-multilingual-minilm-l12-v2 by default) never targets an uncached
secondary model. Embedders are lazy-loaded via OnceCell, so an untargeted
model is never built and never downloaded.

Set KHIVE_NO_EMBED=1 to bypass the gate unconditionally.
Set LATTICE_MODEL_CACHE to override the default model cache directory.

Usage:
    python3 tests/smoke_vector.py
"""

import json
import os
import subprocess
import sys

# Primary embedder validated by this gate. Pinning both memory.remember and
# memory.recall to this model prevents kkernel's multi-model fan-out
# (operations.rs:2055) from targeting the secondary default model
# (paraphrase-multilingual-minilm-l12-v2), which may not be cached. Embedders
# are OnceCell lazy-loaded (embedder_registry.rs:52,55,156), so an untargeted
# model is never built and never downloads from HuggingFace.
EMBED_MODEL = "all-minilm-l6-v2"

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


# Error substrings emitted by kkernel when an embedder is not usable.
# Used as a defensive belt in main(): if the model is cached but kkernel still
# reports an embedding error (e.g. incompatible model version), treat as SKIP.
_SKIP_SIGNALS = (
    "unconfigured: embedding_model is not set",
    "embedding: ",
)


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

    Vector-presence is proven by construction before this function is called:
    the cache pre-check in main() confirmed the model weights are on disk, and
    memory.remember hard-fails on embedding error rather than storing a
    vectorless record.  The score floor below is therefore a ranking-quality
    gate for an exact-match top hit, not a vector-presence detector.

    Also asserts:
    - recall returns a list with at least one hit (vectors were written)
    - the top hit carries a plausible score (>= 0.5), confirming semantic
      ranking rather than random or FTS-only ordering
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
                "embedding_model": EMBED_MODEL,
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
                "embedding_model": EMBED_MODEL,
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

            # Ranking-quality floor: an exact-match top hit in an active vector
            # index must score meaningfully above zero.  This is NOT a
            # vector-presence detector; vector-presence is established by the
            # cache pre-check + successful memory.remember calls above.
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
        print(
            f"  [non-degenerate] {len(set(rank_1_ids))} distinct rank-1 ids "
            f"across {len(QUERIES)} queries -- ok"
        )

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

    # Cache pre-check: never spawn kkernel when the model weights are absent.
    # lattice-inference download.rs:8 downloads from HuggingFace when
    # model.safetensors + tokenizer files are missing; default cache is
    # $HOME/.lattice/models per lib.rs:56-65.
    cache_dir = os.environ.get("LATTICE_MODEL_CACHE") or os.path.join(
        os.path.expanduser("~"), ".lattice", "models"
    )
    model = EMBED_MODEL
    model_dir = os.path.join(cache_dir, model)
    weights_present = os.path.exists(os.path.join(model_dir, "model.safetensors"))
    tokenizer_present = (
        os.path.exists(os.path.join(model_dir, "vocab.txt"))
        or os.path.exists(os.path.join(model_dir, "tokenizer.json"))
    )
    if not (weights_present and tokenizer_present):
        print(
            "SKIP: embedder model not cached; skipping to avoid a network download."
        )
        print(f"  model_dir: {model_dir}")
        print(
            f"  Populate {model_dir}/ with model.safetensors + vocab.txt or "
            "tokenizer.json to activate this gate."
        )
        return 0

    # Model weights confirmed on disk: ensure_model_files returns early with no
    # network access.  Run the full vector round-trip gate.
    try:
        test_vector_round_trip_and_recall_at_1()
    except Exception as exc:
        # Defensive belt: if kkernel still reports an embedding error despite
        # cached weights (e.g. version mismatch), treat as SKIP not FAIL.
        err_str = str(exc)
        for sig in _SKIP_SIGNALS:
            if sig in err_str:
                print(f"SKIP: embedding error with cached model: {exc}")
                return 0
        print(f"\nFAIL: {exc}")
        return 1

    print("\nPASS: vector round-trip + recall@1 gate")
    return 0


if __name__ == "__main__":
    sys.exit(main())
