#!/usr/bin/env python3
"""Smoke + behavioural tests for the knowledge pack verbs.

Covers: learn / cite / topic (behavioural) and upsert_atoms / list / get /
search / edit / delete_atoms / stats / fold / suggest / compose (write-path
round-trip + dispatch shape).

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
    "KKERNEL_BINARY",
    os.path.join(os.path.dirname(__file__), "..", "crates", "target", "release", "kkernel"),
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
            "mcp", "--db", ":memory:",
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
    items = result["results"]
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
    items = result["results"]
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
    items = result["results"]
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
    items = result["results"]
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
    items = result["results"]
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


# ── content fixtures (≥20 words, lexically distinctive) ──────────────────────

_ATOM_CONTENT_BREW = (
    "Zymurgical fermentation processes in ancient brewing traditions where "
    "yeast converts sugars into ethanol through anaerobic metabolic pathways "
    "producing distinctive flavor compounds and carbonation effects"
)

_ATOM_CONTENT_EPIST = (
    "Quizzical epistemological frameworks for analyzing knowledge acquisition "
    "through Socratic dialogue where questions reveal hidden assumptions and "
    "systematic inquiry builds justified belief structures over time"
)

# ≥80 characters, valid section_type
_SECTION_CONTENT_OVERVIEW = (
    "An overview section providing detailed technical information about the atom "
    "and how it integrates with the knowledge pack smoke test harness for regression coverage"
)

_DOMAIN_DESC = (
    "A comprehensive test domain for smoke testing purposes covering "
    "all aspects of the knowledge pack verb surface and integration tests "
    "within the khive system for quality assurance and regression prevention"
)


# ── new test functions ────────────────────────────────────────────────────────

def test_upsert_atoms_write_path(proc):
    """upsert_atoms → list → get confirm the basic write path is wired end-to-end."""
    r = call_verb(proc, "knowledge.upsert_atoms", {
        "atoms": [
            {"slug": "brew-atom", "name": "BrewAtom", "content": _ATOM_CONTENT_BREW, "tags": ["smoke"]},
            {"slug": "epist-atom", "name": "EpistAtom", "content": _ATOM_CONTENT_EPIST, "tags": ["smoke"]},
        ]
    })
    assert r["created"] == 2, f"expected created=2: {r}"
    assert r["updated"] == 0, f"expected updated=0: {r}"
    assert r["total"] == 2, f"expected total=2: {r}"

    listed = call_verb(proc, "knowledge.list", {"limit": 10})
    assert listed["total"] == 2, f"expected list total=2: {listed}"
    slugs = {item["slug"] for item in listed.get("results", [])}
    assert "brew-atom" in slugs, f"brew-atom missing from list: {slugs}"
    assert "epist-atom" in slugs, f"epist-atom missing from list: {slugs}"

    fetched = call_verb(proc, "knowledge.get", {"id": "brew-atom"})
    assert fetched["name"] == "BrewAtom", f"wrong name: {fetched}"
    assert fetched["slug"] == "brew-atom", f"wrong slug: {fetched}"
    assert fetched["kind"] == "atom", f"wrong kind: {fetched}"
    print("  [ok] upsert_atoms write path (upsert → list → get)")


def test_search_finds_draft_atoms(proc):
    """search with include_drafts=True returns draft atoms by distinctive lexical token.

    Atoms are status=draft by default; search excludes drafts unless include_drafts=True.
    FTS is lexical and works without embedding, so this is a real content assertion.
    """
    call_verb(proc, "knowledge.upsert_atoms", {
        "atoms": [
            {"slug": "brew-search", "name": "BrewSearch", "content": _ATOM_CONTENT_BREW},
        ]
    })
    r = call_verb(proc, "knowledge.search", {"query": "zymurgical", "include_drafts": True})
    assert r["total"] >= 1, f"expected >=1 atom matching 'zymurgical': {r}"
    results = r.get("results", [])
    slugs = [item["slug"] for item in results]
    assert "brew-search" in slugs, f"brew-search not in search results: {slugs}"
    print("  [ok] search finds draft atoms with include_drafts=True")


def test_edit_sections_roundtrip(proc):
    """edit upserts a section; get(include_sections=True) returns it persisted."""
    call_verb(proc, "knowledge.upsert_atoms", {
        "atoms": [{"slug": "edit-smoke", "name": "EditSmoke", "content": _ATOM_CONTENT_BREW}]
    })
    edit_r = call_verb(proc, "knowledge.edit", {
        "id": "edit-smoke",
        "sections": [{"section_type": "overview", "content": _SECTION_CONTENT_OVERVIEW}],
    })
    assert edit_r["upserted"] == 1, f"expected upserted=1: {edit_r}"
    inline_sections = edit_r.get("sections", [])
    assert len(inline_sections) == 1, f"expected 1 section in edit response: {inline_sections}"
    assert inline_sections[0]["section_type"] == "overview", (
        f"wrong section_type in edit response: {inline_sections[0]}"
    )

    fetched = call_verb(proc, "knowledge.get", {"id": "edit-smoke", "include_sections": True})
    stored = fetched.get("sections", [])
    assert len(stored) >= 1, f"expected sections after edit: {fetched}"
    types = [s["section_type"] for s in stored]
    assert "overview" in types, f"overview section missing after edit: {types}"
    print("  [ok] edit sections roundtrip")


def test_delete_atoms_removes_atom(proc):
    """delete_atoms removes the atom; subsequent get returns a per-op error."""
    call_verb(proc, "knowledge.upsert_atoms", {
        "atoms": [{"slug": "del-smoke", "name": "DelSmoke", "content": _ATOM_CONTENT_BREW}]
    })
    del_r = call_verb(proc, "knowledge.delete_atoms", {"ids": ["del-smoke"]})
    assert del_r["deleted"] == 1, f"expected deleted=1: {del_r}"
    assert del_r["requested"] == 1, f"expected requested=1: {del_r}"

    err = call_verb_expect_error(proc, "knowledge.get", {"id": "del-smoke"})
    assert err, "expected non-empty error after delete"
    assert "not found" in err.lower(), f"error must mention 'not found': {err!r}"
    print("  [ok] delete_atoms removes atom (get returns error afterward)")


def test_stats_tracks_atom_count(proc):
    """stats total_atoms increments by the number of atoms upserted."""
    before = call_verb(proc, "knowledge.stats", {})
    count_before = before["total_atoms"]

    call_verb(proc, "knowledge.upsert_atoms", {
        "atoms": [
            {"slug": "stats-a", "name": "StatsA", "content": _ATOM_CONTENT_BREW},
            {"slug": "stats-b", "name": "StatsB", "content": _ATOM_CONTENT_EPIST},
        ]
    })
    after = call_verb(proc, "knowledge.stats", {})
    count_after = after["total_atoms"]
    assert count_after == count_before + 2, (
        f"expected total_atoms to increase by 2: before={count_before} after={count_after}"
    )
    print(f"  [ok] stats tracks atom count ({count_before} → {count_after})")


def test_fold_knapsack_budget_constrained(proc):
    """fold selects highest-score items fitting in budget; overflow candidate excluded.

    aaa(300)+ccc(250)+ddd(100)=650 == budget. bbb(400) would overflow 300+400=700 > 650.
    """
    r = call_verb(proc, "knowledge.fold", {
        "candidates": [
            {"id": "aaa", "score": 0.9, "size": 300},
            {"id": "bbb", "score": 0.8, "size": 400},
            {"id": "ccc", "score": 0.7, "size": 250},
            {"id": "ddd", "score": 0.6, "size": 100},
        ],
        "budget": 650,
    })
    assert r["selected_count"] == 3, f"expected selected_count=3: {r}"
    assert r["total_size"] == 650, f"expected total_size=650: {r}"
    selected_ids = {item["id"] for item in r.get("selected", [])}
    assert "aaa" in selected_ids, f"aaa (highest score) must be selected: {selected_ids}"
    assert "ccc" in selected_ids, f"ccc must be selected: {selected_ids}"
    assert "ddd" in selected_ids, f"ddd must be selected: {selected_ids}"
    assert "bbb" not in selected_ids, f"bbb (overflows budget) must be excluded: {selected_ids}"
    print("  [ok] fold knapsack budget-constrained (bbb excluded, total_size=650)")


def test_fold_min_score_filter(proc):
    """fold min_score excludes candidates below threshold regardless of remaining budget."""
    r = call_verb(proc, "knowledge.fold", {
        "candidates": [
            {"id": "high", "score": 0.9, "size": 100},
            {"id": "low",  "score": 0.3, "size": 100},
        ],
        "budget": 1000,
        "min_score": 0.5,
    })
    assert r["selected_count"] == 1, f"expected only 1 item above min_score=0.5: {r}"
    selected_ids = {item["id"] for item in r.get("selected", [])}
    assert "high" in selected_ids, f"high-score item must be selected: {selected_ids}"
    assert "low" not in selected_ids, (
        f"low-score item must be excluded by min_score=0.5: {selected_ids}"
    )
    print("  [ok] fold min_score filter")


def test_suggest_dispatch_smoke(proc):
    """suggest returns expected shape (total as int) under no-corpus (empty but well-formed).

    No domains are loaded in-memory, so total=0; this asserts dispatch + response shape only.
    """
    r = call_verb(proc, "knowledge.suggest", {
        "query": (
            "rust async tokio programming patterns middleware error handling "
            "retry circuit breaker distributed systems fault tolerance"
        ),
        "limit": 5,
    })
    assert "total" in r, f"suggest result must carry 'total' key: {r}"
    assert isinstance(r["total"], int), f"total must be int: {r}"
    print(f"  [ok] suggest dispatch smoke (total={r['total']}, no-corpus shape verified)")


def test_compose_dispatch_smoke(proc):
    """compose returns expected shape (data.count, data.markdown, data.query) under no-corpus.

    No atoms are loaded in-memory; this asserts dispatch + response structure only.
    Query must be >=10 words for auto-compose mode.
    """
    r = call_verb(proc, "knowledge.compose", {
        "query": (
            "rust async tokio futures programming patterns middleware "
            "error handling retry circuit breaker backoff"
        ),
    })
    assert "data" in r, f"compose result must carry 'data' key: {r}"
    data = r["data"]
    assert "count" in data, f"data must carry 'count': {data}"
    assert "markdown" in data, f"data must carry 'markdown': {data}"
    assert "query" in data, f"data must carry 'query': {data}"
    assert isinstance(data["count"], int), f"count must be int: {data}"
    assert isinstance(data["markdown"], str), f"markdown must be str: {data}"
    print(f"  [ok] compose dispatch smoke (count={data['count']}, no-corpus shape verified)")


def test_upsert_atoms_missing_slug_error(proc):
    """upsert_atoms with atom missing required slug returns per-op error mentioning slug."""
    err = call_verb_expect_error(proc, "knowledge.upsert_atoms", {
        "atoms": [{"name": "NoSlug", "content": _ATOM_CONTENT_BREW}]
    })
    assert err, "expected non-empty error"
    assert "slug" in err.lower(), f"error must mention 'slug': {err!r}"
    print(f"  [ok] upsert_atoms missing slug -> error ({err[:60]})")


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
    # upsert_atoms / list / get / search / edit / delete_atoms / stats
    (test_upsert_atoms_write_path,         "upsert_atoms write path (upsert → list → get)"),
    (test_search_finds_draft_atoms,        "search finds draft atoms with include_drafts=True"),
    (test_edit_sections_roundtrip,         "edit sections roundtrip"),
    (test_delete_atoms_removes_atom,       "delete_atoms removes atom"),
    (test_stats_tracks_atom_count,         "stats tracks atom count"),
    # fold — deterministic knapsack, fully assertable without corpus
    (test_fold_knapsack_budget_constrained,"fold knapsack budget-constrained"),
    (test_fold_min_score_filter,           "fold min_score filter"),
    # suggest / compose — dispatch-shape smoke under no-corpus
    (test_suggest_dispatch_smoke,          "suggest dispatch smoke (no-corpus)"),
    (test_compose_dispatch_smoke,          "compose dispatch smoke (no-corpus)"),
    # error path
    (test_upsert_atoms_missing_slug_error, "upsert_atoms missing slug -> error"),
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
    total = len(TESTS)
    if failed == 0:
        print(f"  ALL {total} KNOWLEDGE PACK SMOKE TESTS PASSED")
        return 0
    else:
        print(f"  {failed}/{total} TESTS FAILED")
        return 1


if __name__ == "__main__":
    sys.exit(main())
