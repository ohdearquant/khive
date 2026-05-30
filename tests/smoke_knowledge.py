#!/usr/bin/env python3
"""Smoke + behavioural tests for the knowledge pack verbs (learn / cite / topic).

Spawns khive-mcp with an in-memory DB, --no-embed, and --pack kg --pack knowledge,
then drives every advertised behaviour through the MCP stdio `request` tool.

Usage:
    uv run python tests/smoke_knowledge.py
    # or: python3 tests/smoke_knowledge.py
"""

import json
import subprocess
import sys
import os

BINARY = os.environ.get(
    "KHIVE_MCP_BINARY",
    os.path.join(os.path.dirname(__file__), "..", "crates", "target", "release", "khive-mcp"),
)

request_id = 0


def next_id():
    global request_id
    request_id += 1
    return request_id


def send(proc, method, params=None):
    msg = {"jsonrpc": "2.0", "id": next_id(), "method": method}
    if params is not None:
        msg["params"] = params
    line = json.dumps(msg) + "\n"
    proc.stdin.write(line.encode())
    proc.stdin.flush()


def recv(proc):
    line = proc.stdout.readline()
    if not line:
        raise RuntimeError("MCP server closed stdout")
    return json.loads(line)


def _call_request_raw(proc, ops_string):
    """Send `request(ops=<ops_string>)`. Return the parsed response body."""
    send(proc, "tools/call", {"name": "request", "arguments": {"ops": ops_string}})
    resp = recv(proc)
    if "error" in resp:
        raise RuntimeError(f"MCP error calling request: {resp['error']}")
    result = resp.get("result", {})
    if result.get("isError"):
        content = result.get("content", [])
        text = content[0]["text"] if content else "(no text)"
        raise RuntimeError(f"request returned protocol error: {text}")
    content = result.get("content", [])
    text = content[0]["text"] if content else ""
    return json.loads(text) if text else None


def call_verb(proc, name, args):
    """Call a single verb through `request`. Return that verb's result, or raise on per-op error."""
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


def call_verb_expect_error(proc, name, args):
    """Call a single verb; expect a per-op error. Return the error string."""
    ops = json.dumps([{"tool": name, "args": args}])
    body = _call_request_raw(proc, ops)
    if body is None:
        raise RuntimeError(f"request returned empty body for verb {name} (expected error)")
    results = body.get("results") or []
    if not results:
        raise RuntimeError(f"request returned no results for {name}: {body}")
    first = results[0]
    if first.get("ok", False):
        raise RuntimeError(
            f"expected verb {name} to fail but it succeeded: {first.get('result')}"
        )
    return first.get("error", "")


def spawn():
    """Spawn a fresh in-memory MCP server with kg + knowledge packs loaded."""
    proc = subprocess.Popen(
        [
            BINARY,
            "--db", ":memory:",
            "--no-embed",
            "--log", "error",
            "--pack", "kg",
            "--pack", "knowledge",
        ],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    send(proc, "initialize", {
        "protocolVersion": "2024-11-05",
        "capabilities": {},
        "clientInfo": {"name": "knowledge-smoke", "version": "0.1.0"},
    })
    recv(proc)
    notify = {"jsonrpc": "2.0", "method": "notifications/initialized"}
    proc.stdin.write((json.dumps(notify) + "\n").encode())
    proc.stdin.flush()
    return proc


def teardown(proc):
    proc.stdin.close()
    proc.wait(timeout=5)


# ── test functions ────────────────────────────────────────────────────────────

def test_learn_happy_path(proc):
    """learn with name + description + domain + tags returns correct fields."""
    result = call_verb(proc, "knowledge.learn", {
        "name": "FlashAttention",
        "description": "Memory-efficient attention using IO-aware tiling",
        "domain": "attention",
        "tags": ["gpu", "inference"],
    })
    assert result["kind"] == "concept", f"expected kind=concept: {result}"
    assert result["name"] == "FlashAttention", f"unexpected name: {result}"
    assert result["description"] == "Memory-efficient attention using IO-aware tiling", result
    assert result["domain"] == "attention", f"expected domain=attention: {result}"
    tags = result["tags"]
    assert "attention" in tags, f"domain must be promoted to tags: {tags}"
    assert "gpu" in tags, f"explicit tag missing: {tags}"
    assert "inference" in tags, f"explicit tag missing: {tags}"
    # Shape: 8-char short id + full UUID
    assert len(result["id"]) == 8, f"expected 8-char short id: {result['id']}"
    assert "-" in result["full_id"], f"expected UUID in full_id: {result['full_id']}"
    assert result["namespace"] is not None
    print("  [ok] learn happy path")


def test_learn_auto_name(proc):
    """learn with only description and no name: name auto-generated from first ~60 chars."""
    long_desc = (
        "Grouped-Query Attention reduces key-value cache by sharing heads "
        "across multiple query groups for efficient inference at scale"
    )
    result = call_verb(proc, "knowledge.learn", {"description": long_desc})
    assert result["kind"] == "concept", result
    name = result["name"]
    assert name, "auto-generated name must not be empty"
    # Auto-name truncates at last word boundary <= 60 chars
    assert len(name) <= 60, f"auto-name too long: {name!r}"
    # Description preserved verbatim
    assert result["description"] == long_desc, f"description mismatch: {result}"
    print(f"  [ok] learn auto-name (generated: {name!r})")


def test_learn_content_alias(proc):
    """learn with content= instead of description= works (alias)."""
    result = call_verb(proc, "knowledge.learn", {
        "name": "RoPE",
        "content": "Rotary Position Embedding for transformers",
    })
    assert result["name"] == "RoPE", result
    assert result["description"] == "Rotary Position Embedding for transformers", (
        f"content alias must populate description: {result}"
    )
    print("  [ok] learn content alias")


def test_learn_empty_name_and_description(proc):
    """learn with no name and no description returns an error."""
    err = call_verb_expect_error(proc, "knowledge.learn", {"domain": "attention"})
    assert err, "expected non-empty error message"
    assert "name" in err.lower() or "content" in err.lower(), (
        f"error must mention name/content: {err!r}"
    )
    print("  [ok] learn empty name+description -> error")


def test_learn_domain_normalization(proc):
    """learn with domain='Attention' stores it lowercase."""
    result = call_verb(proc, "knowledge.learn", {
        "name": "SparseAttention",
        "domain": "Attention",
    })
    assert result["domain"] == "attention", (
        f"domain must be lowercased: {result['domain']!r}"
    )
    assert "attention" in result["tags"], (
        f"lowercased domain must appear in tags: {result['tags']}"
    )
    print("  [ok] learn domain normalization (Attention -> attention)")


def test_learn_domain_dedup_in_tags(proc):
    """learn with domain='ml' and tags=['ml', 'ai'] must not duplicate 'ml' in tags."""
    result = call_verb(proc, "knowledge.learn", {
        "name": "MLConcept",
        "domain": "ml",
        "tags": ["ml", "ai"],
    })
    tags = result["tags"]
    ml_count = tags.count("ml")
    assert ml_count == 1, f"'ml' must appear exactly once in tags, got {ml_count}: {tags}"
    assert "ai" in tags, f"'ai' tag missing: {tags}"
    print("  [ok] learn domain dedup in tags")


def test_cite_happy_path(proc):
    """cite creates an introduced_by edge between concept and document."""
    concept = call_verb(proc, "knowledge.learn", {
        "name": "LoRA",
        "domain": "fine-tuning",
    })
    concept_full_id = concept["full_id"]

    # KG create verb goes through Agent presentation mode, which truncates UUIDs
    # to 8-char prefixes.  Pass the full_id from learn (which explicitly carries it)
    # as concept_id, and use the 8-char id from create as source_id.
    # The cite response concept_id/source_id are also 8-char truncated by the
    # presentation layer — compare against the first 8 chars of the full UUIDs.
    paper = call_verb(proc, "create", {
        "kind": "document",
        "name": "LoRA: Low-Rank Adaptation of Large Language Models",
    })
    # paper["id"] is 8-char under Agent mode; use it directly as source_id
    paper_id = paper["id"]

    result = call_verb(proc, "knowledge.cite", {
        "concept_id": concept_full_id,
        "source_id": paper_id,
        "weight": 0.9,
    })
    assert result["relation"] == "introduced_by", f"unexpected relation: {result['relation']}"
    # presentation layer truncates both IDs to 8 chars in the response
    assert result["concept_id"] == concept_full_id[:8], (
        f"concept_id should be 8-char prefix of full UUID: {result}"
    )
    assert result["source_id"] == paper_id, (
        f"source_id should match paper id: {result}"
    )
    assert abs(result["weight"] - 0.9) < 1e-6, f"weight mismatch: {result['weight']}"
    assert len(result["id"]) == 8, f"expected 8-char edge id: {result['id']}"
    assert "-" in result["full_id"], f"expected UUID in full_id: {result['full_id']}"
    print("  [ok] cite happy path")


def test_cite_weight_clamping(proc):
    """cite with weight=2.0 clamps to 1.0."""
    concept = call_verb(proc, "knowledge.learn", {"name": "QLoRA"})
    paper = call_verb(proc, "create", {"kind": "document", "name": "QLoRA paper"})

    result = call_verb(proc, "knowledge.cite", {
        "concept_id": concept["full_id"],
        "source_id": paper["id"],
        "weight": 2.0,
    })
    assert abs(result["weight"] - 1.0) < 1e-6, (
        f"weight 2.0 must clamp to 1.0, got: {result['weight']}"
    )
    print("  [ok] cite weight clamping (2.0 -> 1.0)")


def test_cite_weight_zero(proc):
    """cite with weight=0.0 is accepted (minimum valid value)."""
    concept = call_verb(proc, "knowledge.learn", {"name": "DPO"})
    paper = call_verb(proc, "create", {"kind": "document", "name": "DPO paper"})

    result = call_verb(proc, "knowledge.cite", {
        "concept_id": concept["full_id"],
        "source_id": paper["id"],
        "weight": 0.0,
    })
    assert abs(result["weight"] - 0.0) < 1e-6, (
        f"weight 0.0 must be accepted, got: {result['weight']}"
    )
    print("  [ok] cite weight=0.0 accepted")


def test_cite_invalid_concept_id(proc):
    """cite with a non-existent concept_id returns an error."""
    err = call_verb_expect_error(proc, "knowledge.cite", {
        "concept_id": "nonexistent",
        "source_id": "00000000-0000-0000-0000-000000000001",
    })
    assert err, "expected non-empty error"
    print(f"  [ok] cite invalid concept_id -> error ({err[:60]})")


def test_topic_list_all(proc):
    """topic() with no filter returns all learned concepts."""
    for name in ("Alpha", "Beta", "Gamma"):
        call_verb(proc, "knowledge.learn", {"name": name})

    result = call_verb(proc, "knowledge.topic", {})
    items = result["items"]
    assert isinstance(items, list), f"expected list, got: {type(items)}"
    assert len(items) >= 3, f"expected at least 3 concepts, got {len(items)}: {items}"
    total = result["total"]
    assert total >= 3, f"total must be >= 3, got {total}"
    print(f"  [ok] topic list all ({len(items)} items, total={total})")


def test_topic_domain_filter(proc):
    """topic(domain='attention') returns only attention concepts."""
    call_verb(proc, "knowledge.learn", {"name": "MHA", "domain": "attention"})
    call_verb(proc, "knowledge.learn", {"name": "MQA", "domain": "attention"})
    call_verb(proc, "knowledge.learn", {"name": "KVCache", "domain": "inference"})

    result = call_verb(proc, "knowledge.topic", {"domain": "attention"})
    items = result["items"]
    names = [i["name"] for i in items]
    assert len(items) == 2, f"expected 2 attention concepts, got {len(items)}: {names}"
    assert "MHA" in names, f"MHA missing: {names}"
    assert "MQA" in names, f"MQA missing: {names}"
    assert "KVCache" not in names, f"KVCache should not appear: {names}"
    print("  [ok] topic domain filter")


def test_topic_domain_case_insensitive(proc):
    """topic(domain='ATTENTION') matches concepts stored with domain='Attention'."""
    call_verb(proc, "knowledge.learn", {"name": "PagedAttention", "domain": "Attention"})

    result = call_verb(proc, "knowledge.topic", {"domain": "ATTENTION"})
    items = result["items"]
    names = [i["name"] for i in items]
    assert any("PagedAttention" == n for n in names), (
        f"case-insensitive match must find PagedAttention: {names}"
    )
    print("  [ok] topic domain filter is case insensitive")


def test_topic_with_query_fts(proc):
    """topic(query='...') uses FTS to find a concept by a distinctive name fragment."""
    call_verb(proc, "knowledge.learn", {
        "name": "SpeculativeDecodingXYZ",
        "description": "Draft model accelerates generation",
    })
    call_verb(proc, "knowledge.learn", {
        "name": "SomethingUnrelated",
        "description": "Completely different topic",
    })

    result = call_verb(proc, "knowledge.topic", {"query": "SpeculativeDecodingXYZ"})
    items = result["items"]
    names = [i["name"] for i in items]
    assert any("SpeculativeDecodingXYZ" == n for n in names), (
        f"FTS query must find exact name match: {names}"
    )
    # Search path items include score field
    matching = [i for i in items if i["name"] == "SpeculativeDecodingXYZ"]
    assert "score" in matching[0], f"search path items must include score: {matching[0]}"
    print("  [ok] topic with FTS query")


def test_topic_limit(proc):
    """topic(limit=2) returns at most 2 items; total reflects pre-limit count."""
    for i in range(5):
        call_verb(proc, "knowledge.learn", {"name": f"LimitConcept{i}"})

    result = call_verb(proc, "knowledge.topic", {"limit": 2})
    items = result["items"]
    total = result["total"]
    assert len(items) <= 2, f"limit=2 must return at most 2 items, got {len(items)}"
    assert total >= 5, (
        f"total on listing path must reflect pre-limit count (>= 5), got {total}"
    )
    print(f"  [ok] topic limit=2 (items={len(items)}, total={total})")


def test_cite_nonexistent_source(proc):
    """cite with valid concept_id but non-existent source_id returns an error."""
    concept = call_verb(proc, "knowledge.learn", {"name": "SpecDec"})
    err = call_verb_expect_error(proc, "knowledge.cite", {
        "concept_id": concept["full_id"],
        "source_id": "00000000-0000-0000-0000-000000000099",
    })
    assert err, "expected non-empty error for nonexistent source"
    print(f"  [ok] cite nonexistent source -> error ({err[:60]})")


# ── main ──────────────────────────────────────────────────────────────────────

TESTS = [
    # (test_fn, label) — each gets its own fresh proc to keep state independent
    (test_learn_happy_path,              "learn happy path"),
    (test_learn_auto_name,               "learn auto-name from description"),
    (test_learn_content_alias,           "learn content alias"),
    (test_learn_empty_name_and_description, "learn empty name+description -> error"),
    (test_learn_domain_normalization,    "learn domain normalization"),
    (test_learn_domain_dedup_in_tags,    "learn domain dedup in tags"),
    (test_cite_happy_path,               "cite happy path"),
    (test_cite_weight_clamping,          "cite weight clamping"),
    (test_cite_weight_zero,              "cite weight=0.0"),
    (test_cite_invalid_concept_id,       "cite invalid concept_id"),
    (test_topic_list_all,                "topic list all"),
    (test_topic_domain_filter,           "topic domain filter"),
    (test_topic_domain_case_insensitive, "topic domain case insensitive"),
    (test_topic_with_query_fts,          "topic with FTS query"),
    (test_topic_limit,                   "topic limit"),
    (test_cite_nonexistent_source,       "cite nonexistent source"),
]


def main():
    print(f"Binary: {BINARY}")
    assert os.path.exists(BINARY), f"Binary not found: {BINARY}"

    failed = 0
    for fn, label in TESTS:
        proc = spawn()
        try:
            fn(proc)
        except Exception as e:
            print(f"  [FAIL] {label}: {e}")
            failed += 1
        finally:
            teardown(proc)

    print()
    if failed == 0:
        print(f"  ALL {len(TESTS)} KNOWLEDGE PACK SMOKE TESTS PASSED")
        return 0
    else:
        print(f"  {failed}/{len(TESTS)} TESTS FAILED")
        return 1


if __name__ == "__main__":
    sys.exit(main())
