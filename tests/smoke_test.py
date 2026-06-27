#!/usr/bin/env python3
"""Smoke test for khive-mcp binary over stdio MCP.

Spawns the binary with an in-memory DB, sends JSON-RPC MCP requests, and
verifies the full verb-consolidated surface works end-to-end. As of v0.2 the
MCP server exposes a single tool, `request` (ADR-016 + ADR-027), that accepts
a function-call DSL or JSON-form batch; every verb is reached through it.

Verb semantics (unchanged from v0.1): create, get, list, update, delete, merge,
search, link, neighbors, traverse, query — plus the gtd pack's assign, next,
complete, tasks, transition when KHIVE_PACKS=...,gtd.
get/update/delete/merge auto-detect record kind from UUID — no kind= needed.
get returns {"kind": "entity"|"note"|"edge", "data": {...}}.

Usage:
    uv run python tests/smoke_test.py
    # or: python3 tests/smoke_test.py
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
    """Call a single verb through `request`. Return that verb's result, or raise on per-op error.

    The MCP server exposes a single tool (`request`) per ADR-027; tests
    express intent in terms of verbs and this helper handles the
    JSON-form encoding and per-op unwrapping.
    """
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


def main():
    print(f"Binary: {BINARY}")
    assert os.path.exists(BINARY), f"Binary not found: {BINARY}"

    env = {**os.environ, "KHIVE_NO_DAEMON": "1"}
    proc = subprocess.Popen(
        [BINARY, "mcp", "--db", ":memory:", "--no-embed", "--log", "error"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env=env,
    )

    try:
        # 1. Initialize
        send(proc, "initialize", {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "smoke-test", "version": "0.1.0"},
        })
        init = recv(proc)
        assert init["result"]["serverInfo"]["name"] == "khive-mcp", f"unexpected: {init}"
        print("  [ok] initialize")

        # Send initialized notification
        notify = {"jsonrpc": "2.0", "method": "notifications/initialized"}
        proc.stdin.write((json.dumps(notify) + "\n").encode())
        proc.stdin.flush()

        # 2. List tools — must be exactly `request` (single-tool surface).
        #    The request tool's description must include each KG verb so MCP
        #    clients can discover them via `tools/list`.
        send(proc, "tools/list", {})
        tools_resp = recv(proc)
        tools = tools_resp["result"]["tools"]
        tool_names = [t["name"] for t in tools]
        assert tool_names == ["request"], (
            f"expected exactly [request], got {tool_names}"
        )
        request_desc = tools[0].get("description") or ""
        for verb in (
            "create", "get", "list", "update", "delete", "merge",
            "search", "link", "neighbors", "traverse", "query",
        ):
            assert verb in request_desc, (
                f"request description missing verb {verb!r}; got:\n{request_desc}"
            )
        print(f"  [ok] tools/list — single `request` tool; description lists 11 verbs")

        # 3. Create entities
        lora = call_verb(proc, "create", {
            "kind": "entity",
            "entity_kind": "concept",
            "name": "LoRA",
            "description": "Low-Rank Adaptation",
            "properties": {"domain": "fine-tuning", "year": 2021},
        })
        assert lora["name"] == "LoRA", f"unexpected: {lora}"
        lora_id = lora["id"]
        print(f"  [ok] create entity — LoRA ({lora_id[:8]}...)")

        qlora = call_verb(proc, "create", {
            "kind": "entity",
            "entity_kind": "concept",
            "name": "QLoRA",
            "description": "Quantized LoRA",
        })
        qlora_id = qlora["id"]
        print(f"  [ok] create entity — QLoRA ({qlora_id[:8]}...)")

        paper = call_verb(proc, "create", {
            "kind": "entity",
            "entity_kind": "document",
            "name": "LoRA: Low-Rank Adaptation of Large Language Models",
            "properties": {"authors": "Hu et al.", "year": 2021},
        })
        paper_id = paper["id"]
        print(f"  [ok] create entity — paper ({paper_id[:8]}...)")

        # stats: aggregate substrate counts.
        # Result shape from stats.rs:30-34: {"entities": int, "edges": int, "notes": int}.
        # At this point: 3 entities created, 0 edges, 0 notes — a clean deterministic state.
        counts = call_verb(proc, "stats", {})
        assert isinstance(counts.get("entities"), int), f"stats must return integer 'entities': {counts}"
        assert isinstance(counts.get("edges"), int), f"stats must return integer 'edges': {counts}"
        assert isinstance(counts.get("notes"), int), f"stats must return integer 'notes': {counts}"
        assert counts["entities"] == 3, f"expected 3 entities after creates, got {counts}"
        assert counts["edges"] == 0, f"expected 0 edges before any link, got {counts}"
        assert counts["notes"] == 0, f"expected 0 notes before any note create, got {counts}"
        print(f"  [ok] stats — entities={counts['entities']} edges={counts['edges']} notes={counts['notes']}")

        # verbs: verb discovery introspection.
        # Result shape from handler_defs.rs:746-748: {"verbs": list, "total": int}.
        # Each entry has verb, pack, description, category (handler_defs.rs:735-742).
        verbs_result = call_verb(proc, "verbs", {})
        assert "verbs" in verbs_result, f"verbs must return 'verbs' key: {verbs_result}"
        assert "total" in verbs_result, f"verbs must return 'total' key: {verbs_result}"
        assert isinstance(verbs_result["verbs"], list), f"verbs must be a list: {verbs_result}"
        # Surface-contract tripwire: the default config (no --pack, KHIVE_PACKS
        # unset) loads all 7 production packs, so verbs() returns exactly 67
        # user-facing MCP-callable verbs (count what verbs() returns, not internal
        # dispatch arms). Update this number when the pack set or verb surface
        # changes; a silent drift here is the bug this assertion exists to catch.
        assert verbs_result["total"] == 67, (
            f"expected 67 user-facing verbs from the 7 default packs, "
            f"got {verbs_result['total']}: {verbs_result}"
        )
        verb_names = [v["verb"] for v in verbs_result["verbs"]]
        assert "create" in verb_names, f"'create' must appear in verbs listing: {verb_names}"
        assert "stats" in verb_names, f"'stats' must appear in verbs listing: {verb_names}"
        # each entry carries verb, pack, description, category per handler_defs.rs:735-742
        first = verbs_result["verbs"][0]
        for key in ("verb", "pack", "description", "category"):
            assert key in first, f"verb entry missing key {key!r}: {first}"
        # pack= filter: handler_defs.rs:729 applies pack_name.eq_ignore_ascii_case(pk);
        # each returned entry carries its "pack" field (handler_defs.rs:736).
        # Assertions must be non-vacuous: a pack=kg filter that was ignored would
        # return the full 67-verb list, so we verify the filter returns only kg verbs.
        kg_verbs = call_verb(proc, "verbs", {"pack": "kg"})
        assert len(kg_verbs["verbs"]) > 0, f"kg pack filter must return a nonempty list: {kg_verbs}"
        assert kg_verbs["total"] == len(kg_verbs["verbs"]), (
            f"total must equal len(verbs): total={kg_verbs['total']} list={len(kg_verbs['verbs'])}"
        )
        kg_verb_packs = [v["pack"].lower() for v in kg_verbs["verbs"]]
        assert all(p == "kg" for p in kg_verb_packs), (
            f"every entry returned by pack=kg filter must have pack='kg': {kg_verb_packs}"
        )
        kg_verb_names = [v["verb"] for v in kg_verbs["verbs"]]
        assert "create" in kg_verb_names, f"'create' must appear in kg-filtered verbs: {kg_verb_names}"
        assert "stats" in kg_verb_names, f"'stats' must appear in kg-filtered verbs: {kg_verb_names}"
        print(f"  [ok] verbs — {verbs_result['total']} total verbs, {kg_verbs['total']} in kg pack")

        # 4. Get entity via get (auto-detects substrate; flat shape per W2 #454,
        #    granular kind at top level — same shape as create/list)
        fetched = call_verb(proc, "get", {"id": lora_id})
        assert fetched["kind"] == "concept", f"expected granular kind=concept, got: {fetched}"
        assert fetched["name"] == "LoRA", f"unexpected: {fetched}"
        print(f"  [ok] get entity — flat response kind={fetched['kind']}")

        # 5. List entities
        entities = call_verb(proc, "list", {"kind": "entity", "entity_kind": "concept"})
        assert len(entities) == 2, f"expected 2 concepts, got {len(entities)}"
        print(f"  [ok] list entities — {len(entities)} concepts")

        # 6. Create edges via link
        edge1 = call_verb(proc, "link", {
            "source_id": qlora_id,
            "target_id": lora_id,
            "relation": "variant_of",
            "weight": 0.9,
        })
        assert edge1["relation"] == "variant_of"
        print(f"  [ok] link — QLoRA variant_of LoRA")

        # ADR-002: introduced_by direction is concept → document (a concept
        # was introduced by a paper). Reverse the source/target accordingly.
        call_verb(proc, "link", {
            "source_id": lora_id,
            "target_id": paper_id,
            "relation": "introduced_by",
            "weight": 1.0,
        })
        print(f"  [ok] link — LoRA introduced_by paper")

        # 7. Get edge via get (auto-detects kind)
        edge_id = edge1["id"]
        fetched_edge = call_verb(proc, "get", {"id": edge_id})
        assert fetched_edge["kind"] == "edge", f"expected kind=edge, got: {fetched_edge}"
        print(f"  [ok] get edge — wrapped response kind={fetched_edge['kind']}")

        # 8. Neighbors — LoRA has 1 inbound (QLoRA variant_of) and 1 outbound
        # (LoRA introduced_by paper, per ADR-002 direction).
        nbrs_in = call_verb(proc, "neighbors", {
            "node_id": lora_id,
            "direction": "in",
        })
        assert len(nbrs_in) == 1, f"expected 1 inbound neighbor, got {len(nbrs_in)}"
        nbrs_out = call_verb(proc, "neighbors", {
            "node_id": lora_id,
            "direction": "out",
        })
        assert len(nbrs_out) == 1, f"expected 1 outbound neighbor, got {len(nbrs_out)}"
        print(f"  [ok] neighbors — 1 inbound + 1 outbound to LoRA")

        # 9. Edge list
        edges = call_verb(proc, "list", {"kind": "edge", "source_id": qlora_id})
        assert len(edges) == 1
        print(f"  [ok] list edges")

        # 10. Edge update
        updated_edge = call_verb(proc, "update", {"id": edge_id, "kind": "edge", "weight": 0.95})
        assert abs(updated_edge["weight"] - 0.95) < 0.01
        print(f"  [ok] update edge weight")

        # 11. Entity update
        patched = call_verb(proc, "update", {
            "id": lora_id,
            "kind": "entity",
            "description": "Low-Rank Adaptation of LLMs",
        })
        assert patched["description"] == "Low-Rank Adaptation of LLMs"
        print(f"  [ok] update entity")

        # 12. Create note
        note = call_verb(proc, "create", {
            "kind": "note",
            "note_kind": "observation",
            "content": "LoRA reduces trainable parameters by 10000x",
            "salience": 0.8,
        })
        assert note["kind"] == "observation"
        note_id = note["id"]
        print(f"  [ok] create note — observation ({note_id[:8]}...)")

        # 13. List notes
        notes = call_verb(proc, "list", {"kind": "note", "note_kind": "observation"})
        assert len(notes) == 1
        print(f"  [ok] list notes — {len(notes)} observation")

        # 14. Search entities
        search_hits = call_verb(proc, "search", {
            "kind": "entity",
            "query": "LoRA parameter efficient fine-tuning",
            "limit": 5,
        })
        assert isinstance(search_hits, list), f"expected list, got: {search_hits}"
        print(f"  [ok] search entities — {len(search_hits)} hit(s)")

        # 15. Search notes
        note_hits = call_verb(proc, "search", {
            "kind": "note",
            "query": "LoRA parameters",
            "limit": 5,
        })
        assert isinstance(note_hits, list), f"expected list, got: {note_hits}"
        print(f"  [ok] search notes — {len(note_hits)} hit(s)")

        # 16. Cross-substrate: annotated note (ADR-024)
        call_verb(proc, "create", {
            "kind": "note",
            "note_kind": "insight",
            "content": "LoRA is parameter-efficient",
            "annotates": [lora_id],
        })
        nbrs_in = call_verb(proc, "neighbors", {
            "node_id": lora_id,
            "direction": "in",
            "relations": ["annotates"],
        })
        assert len(nbrs_in) == 1, f"expected 1 annotates neighbor, got {len(nbrs_in)}"
        print(f"  [ok] create annotated note + neighbors(annotates)")

        # 17. GQL query
        rows = call_verb(proc, "query", {
            "query": "MATCH (a:concept)-[e:variant_of]->(b:concept) RETURN a, b LIMIT 10",
        })
        assert len(rows) >= 1, f"expected at least 1 row, got {len(rows)}"
        print(f"  [ok] query (GQL) — {len(rows)} row(s)")

        # 18. Entity merge
        dupe = call_verb(proc, "create", {
            "kind": "entity",
            "entity_kind": "concept",
            "name": "LoRA duplicate",
        })
        summary = call_verb(proc, "merge", {
            "into_id": lora_id,
            "from_id": dupe["id"],
            "strategy": "prefer_into",
        })
        assert summary["kept_id"] == lora_id
        print(f"  [ok] merge entity")

        # 19. Entity delete
        del_result = call_verb(proc, "delete", {"id": qlora_id, "kind": "entity"})
        assert del_result["deleted"] is True
        print(f"  [ok] delete entity")

        # 20. Edge delete
        del_edge = call_verb(proc, "delete", {"id": edge_id, "kind": "edge"})
        assert del_edge["deleted"] is True
        print(f"  [ok] delete edge")

        # 21. Note delete
        del_note = call_verb(proc, "delete", {"id": note_id, "kind": "note"})
        assert del_note["deleted"] is True
        print(f"  [ok] delete note")

        # 22. Traverse
        a = call_verb(proc, "create", {"kind": "entity", "entity_kind": "concept", "name": "TraverseA"})
        b = call_verb(proc, "create", {"kind": "entity", "entity_kind": "concept", "name": "TraverseB"})
        c = call_verb(proc, "create", {"kind": "entity", "entity_kind": "concept", "name": "TraverseC"})
        call_verb(proc, "link", {"source_id": a["id"], "target_id": b["id"], "relation": "extends"})
        call_verb(proc, "link", {"source_id": b["id"], "target_id": c["id"], "relation": "extends"})
        paths = call_verb(proc, "traverse", {
            "roots": [a["id"]],
            "max_depth": 2,
            "include_roots": False,
        })
        # #148: traverse response uses canonical "id" (not "node_id")
        # Traverse returns full 36-char UUIDs; create returns short 8-char ids by default
        # (W1-K #447). Match by prefix to bridge the two id forms.
        all_node_ids = [n["id"] for p in paths for n in p.get("nodes", [])]
        assert any(nid.startswith(b["id"]) for nid in all_node_ids), (
            f"B must be reachable: b={b['id']!r} nodes={all_node_ids}"
        )
        assert any(nid.startswith(c["id"]) for nid in all_node_ids), (
            f"C must be reachable at depth 2: c={c['id']!r}"
        )
        print(f"  [ok] traverse — depth-2 multi-hop")

        # 23. Parallel batch — independent ops must all succeed in one request call.
        bulk_ops = json.dumps([
            {"tool": "create", "args": {"kind": "entity", "entity_kind": "concept", "name": "BulkA"}},
            {"tool": "create", "args": {"kind": "entity", "entity_kind": "concept", "name": "BulkB"}},
            {"tool": "create", "args": {"kind": "entity", "entity_kind": "concept", "name": "BulkC"}},
        ])
        bulk = _call_request_raw(proc, bulk_ops)
        summary = bulk.get("summary", {})
        assert summary.get("total") == 3 and summary.get("failed") == 0, (
            f"expected 3/0 summary, got {summary}"
        )
        print(f"  [ok] parallel batch — 3 independent creates in one request call")

        # 24. Malformed DSL must surface as RPC-level invalid_params, not silent success.
        try:
            _call_request_raw(proc, "create(")
            print("  [FAIL] malformed DSL was accepted")
            return 1
        except RuntimeError as e:
            assert "expected" in str(e) or "invalid" in str(e), f"unexpected error: {e}"
            print(f"  [ok] malformed DSL rejected at MCP boundary")

        print(f"\n  ALL VERB SMOKE TESTS PASSED (single-tool surface)")

    finally:
        proc.stdin.close()
        proc.wait(timeout=5)

    return 0


def gtd_smoke():
    """Optional smoke test for the gtd pack — only runs if KHIVE_PACKS=...,gtd."""
    env = {**os.environ, "KHIVE_NO_DAEMON": "1"}
    proc = subprocess.Popen(
        [
            BINARY, "mcp", "--db", ":memory:", "--no-embed", "--log", "error",
            "--pack", "kg", "--pack", "gtd",
        ],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env=env,
    )
    try:
        send(proc, "initialize", {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "gtd-smoke", "version": "0.1.0"},
        })
        recv(proc)
        notify = {"jsonrpc": "2.0", "method": "notifications/initialized"}
        proc.stdin.write((json.dumps(notify) + "\n").encode())
        proc.stdin.flush()

        # gtd.assign → gtd.next → gtd.complete round-trip
        assigned = call_verb(proc, "gtd.assign", {
            "title": "ship pack-gtd",
            "status": "next",
            "priority": "p0",
        })
        assert assigned["kind"] == "task"
        assert assigned["status"] == "next"
        print(f"  [gtd] gtd.assign — {assigned['title']!r} ({assigned['id']})")

        ready = call_verb(proc, "gtd.next", {})
        assert any(t["full_id"] == assigned["full_id"] for t in ready), (
            f"assigned task not in gtd.next(): {ready}"
        )
        print(f"  [gtd] gtd.next — {len(ready)} actionable")

        done = call_verb(proc, "gtd.complete", {
            "id": assigned["full_id"],
            "result": "smoke-test pass",
        })
        assert done["to"] == "done"
        print(f"  [gtd] gtd.complete — transitioned to done")

        # gtd.tasks: list tasks filtered by status
        t1 = call_verb(proc, "gtd.assign", {
            "title": "waiting task",
            "status": "waiting",
            "priority": "p1",
        })
        t2 = call_verb(proc, "gtd.assign", {
            "title": "inbox task",
            "status": "inbox",
            "priority": "p2",
        })
        waiting_tasks = call_verb(proc, "gtd.tasks", {"status": "waiting"})
        assert isinstance(waiting_tasks, list), f"gtd.tasks must return a list, got: {waiting_tasks}"
        waiting_ids = [t["full_id"] for t in waiting_tasks]
        assert t1["full_id"] in waiting_ids, (
            f"'waiting task' must appear in gtd.tasks(status=waiting): {waiting_ids}"
        )
        assert t2["full_id"] not in waiting_ids, (
            f"'inbox task' must NOT appear in gtd.tasks(status=waiting): {waiting_ids}"
        )
        print(f"  [gtd] gtd.tasks(status=waiting) — {len(waiting_tasks)} task(s)")

        # gtd.transition: explicit lifecycle change with validation
        trans = call_verb(proc, "gtd.transition", {
            "id": t2["full_id"],
            "status": "next",
            "note": "promoted from inbox",
        })
        assert trans["transitioned"] is True, f"gtd.transition must set transitioned=true: {trans}"
        assert trans["to"] == "next", f"gtd.transition must report to=next: {trans}"
        print(f"  [gtd] gtd.transition inbox→next — ok")

        # gtd.transition: idempotent (same status) must not error
        trans_idem = call_verb(proc, "gtd.transition", {
            "id": t2["full_id"],
            "status": "next",
        })
        assert trans_idem["transitioned"] is False, (
            f"idempotent gtd.transition must set transitioned=false: {trans_idem}"
        )
        print(f"  [gtd] gtd.transition idempotent — ok")

        print(f"\n  GTD PACK SMOKE TESTS PASSED")
    finally:
        proc.stdin.close()
        proc.wait(timeout=5)


def memory_smoke():
    """Optional smoke test for the memory pack — exercises remember and recall."""
    env = {**os.environ, "KHIVE_NO_DAEMON": "1"}
    proc = subprocess.Popen(
        [
            BINARY, "mcp", "--db", ":memory:", "--no-embed", "--log", "error",
            "--pack", "kg", "--pack", "memory",
        ],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env=env,
    )
    try:
        send(proc, "initialize", {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "memory-smoke", "version": "0.1.0"},
        })
        recv(proc)
        notify = {"jsonrpc": "2.0", "method": "notifications/initialized"}
        proc.stdin.write((json.dumps(notify) + "\n").encode())
        proc.stdin.flush()

        # memory.remember: store a memory note
        mem = call_verb(proc, "memory.remember", {
            "content": "khive uses SQLite with FTS5 and sqlite-vec for hybrid search",
            "salience": 0.9,
            "memory_type": "semantic",
        })
        assert mem is not None, "memory.remember must return a result"
        mem_id = mem["id"]
        assert mem_id, f"memory.remember must return an id: {mem}"
        print(f"  [memory] memory.remember — id {str(mem_id)[:8]}...")

        # memory.remember: second memory with different content
        mem2 = call_verb(proc, "memory.remember", {
            "content": "The runtime enforces namespace isolation for every ID-based operation",
            "salience": 0.7,
            "memory_type": "semantic",
        })
        assert mem2 is not None, "second memory.remember must return a result"
        print(f"  [memory] memory.remember (second) — ok")

        # memory.recall: returns a list (possibly empty with --no-embed, FTS still works)
        hits = call_verb(proc, "memory.recall", {
            "query": "SQLite hybrid search",
            "limit": 5,
        })
        assert isinstance(hits, list), f"memory.recall must return a list, got: {hits}"
        print(f"  [memory] memory.recall — {len(hits)} hit(s)")

        # memory.prune dry-run: count candidates without deleting.
        # Result shape from prune.rs:121-127: {"pruned": 0, "dry_run": true, "would_prune": int, "namespace": str}.
        prune_dry = call_verb(proc, "memory.prune", {
            "min_salience": 0.5,
            "before": 0,  # 0 = skip expiry filter (prune.rs:101-102: Some(0) => None)
            "dry_run": True,
        })
        assert prune_dry.get("dry_run") is True, f"dry_run response must set dry_run=true: {prune_dry}"
        assert "would_prune" in prune_dry, f"dry_run response must include would_prune key: {prune_dry}"
        print(f"  [memory] memory.prune(dry_run=True) — would_prune={prune_dry['would_prune']}")

        # Store a low-salience memory so the real prune has something to delete.
        mem_low = call_verb(proc, "memory.remember", {
            "content": "ephemeral low-salience note for prune coverage test",
            "salience": 0.1,
            "memory_type": "episodic",
        })
        assert mem_low is not None, "low-salience memory.remember must return a result"

        # memory.prune real run: salience < 0.2 filter removes the 0.1-salience memory.
        # before=0 skips expiry filter (NULL expires_at rows are safe regardless).
        # Result shape from prune.rs:138-142: {"pruned": int, "dry_run": false, "namespace": str}.
        prune_result = call_verb(proc, "memory.prune", {
            "min_salience": 0.2,
            "before": 0,
        })
        assert prune_result.get("dry_run") is False, f"real prune must set dry_run=false: {prune_result}"
        assert "pruned" in prune_result, f"prune response must include pruned count: {prune_result}"
        assert prune_result["pruned"] >= 1, (
            f"at least the 0.1-salience memory must be pruned: {prune_result}"
        )
        print(f"  [memory] memory.prune — pruned={prune_result['pruned']}")

        # memory.vacuum: reclaim space freed by soft-deleted rows.
        # Result shape from prune.rs:156-158: {"ok": true}.
        vacuum_result = call_verb(proc, "memory.vacuum", {})
        assert vacuum_result.get("ok") is True, f"memory.vacuum must return ok=true: {vacuum_result}"
        print(f"  [memory] memory.vacuum — ok")

        print(f"\n  MEMORY PACK SMOKE TESTS PASSED")
    finally:
        proc.stdin.close()
        proc.wait(timeout=5)


def formal_smoke():
    """Smoke test for the formal-pack EntityOfType edge rules (vocab.rs).

    The formal pack (khive-pack-formal) adds 21 additive endpoint rules keyed
    on entity_type (vocab.rs:27-137). Formal math entities are plain concept
    entities with entity_type set to the subtype ("theorem", "definition", etc.).
    The pair exercised here:

        depends_on: theorem -> definition  (vocab.rs:37-42)

    Without the formal pack, concept depends_on concept is rejected by the base
    contract (operations.rs:298-304: depends_on is p->p, s->{p,s,a,ds}, a->{p,s}).
    With --pack formal loaded, EndpointKind::EntityOfType matching in
    operations.rs:231-234 (substrate=="entity" && kind==k && entity_type==Some(t))
    permits it.
    """
    env = {**os.environ, "KHIVE_NO_DAEMON": "1"}
    proc = subprocess.Popen(
        [
            BINARY, "mcp", "--db", ":memory:", "--no-embed", "--log", "error",
            "--pack", "kg", "--pack", "formal",
        ],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env=env,
    )
    try:
        send(proc, "initialize", {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "formal-smoke", "version": "0.1.0"},
        })
        recv(proc)
        notify = {"jsonrpc": "2.0", "method": "notifications/initialized"}
        proc.stdin.write((json.dumps(notify) + "\n").encode())
        proc.stdin.flush()

        # Create a concept entity with entity_type="theorem".
        # entity_type is stored via Entity::with_entity_type (operations.rs:487),
        # making it available to the EntityOfType endpoint matcher.
        thm = call_verb(proc, "create", {
            "kind": "entity",
            "entity_kind": "concept",
            "entity_type": "theorem",
            "name": "FormalSmokeTheorem",
            "description": "Synthetic theorem for formal-pack smoke coverage",
        })
        assert thm["name"] == "FormalSmokeTheorem", f"unexpected create result: {thm}"
        thm_id = thm["id"]
        print(f"  [formal] create concept entity_type=theorem — {thm_id[:8]}...")

        # Create a concept entity with entity_type="definition".
        defn = call_verb(proc, "create", {
            "kind": "entity",
            "entity_kind": "concept",
            "entity_type": "definition",
            "name": "FormalSmokeDefinition",
            "description": "Synthetic definition for formal-pack smoke coverage",
        })
        defn_id = defn["id"]
        print(f"  [formal] create concept entity_type=definition — {defn_id[:8]}...")

        # Link theorem -[depends_on]-> definition.
        # Permitted by FORMAL_EDGE_RULES[1] (vocab.rs:37-42):
        #   EdgeEndpointRule { relation: DependsOn,
        #     source: EntityOfType { kind: "concept", entity_type: "theorem" },
        #     target: EntityOfType { kind: "concept", entity_type: "definition" } }
        edge = call_verb(proc, "link", {
            "source_id": thm_id,
            "target_id": defn_id,
            "relation": "depends_on",
            "weight": 1.0,
        })
        assert edge["relation"] == "depends_on", (
            f"formal-pack depends_on edge must succeed: {edge}"
        )
        print(f"  [formal] link theorem -[depends_on]-> definition — ok")

        print(f"\n  FORMAL PACK SMOKE TESTS PASSED")
    finally:
        proc.stdin.close()
        proc.wait(timeout=5)


def epistemic_smoke():
    """E2E smoke test for supports/refutes epistemic edge relations (ADR-055).

    Endpoint contract (ADR-055 §"Secondary rail: Entity→Entity" and ADR-002
    §"Epistemic relations"):

    Entity-form legal pairs (source → target):
      concept  → concept   (operations.rs:212,216)
      document → concept   (operations.rs:213,217)
      dataset  → concept   (operations.rs:214,218)
      artifact → concept   (operations.rs:215,219)

    Note-form: any note kind → any note kind (substrate-level, operations.rs:702).

    Illegal: document → document is rejected because target is not concept
    (operations.rs:695-699).

    Direction: source = evidence, target = claim. NOT symmetric.
    (ADR-055 §"Direction and symmetry")
    """
    env = {**os.environ, "KHIVE_NO_DAEMON": "1"}
    proc = subprocess.Popen(
        [BINARY, "mcp", "--db", ":memory:", "--no-embed", "--log", "error"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env=env,
    )
    try:
        send(proc, "initialize", {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "epistemic-smoke", "version": "0.1.0"},
        })
        recv(proc)
        notify = {"jsonrpc": "2.0", "method": "notifications/initialized"}
        proc.stdin.write((json.dumps(notify) + "\n").encode())
        proc.stdin.flush()

        # --- entity-form: document → concept (supports) ---
        # ADR-055 §"Secondary rail": document is a legal evidence source; concept is
        # the only legal entity-form claim target.
        claim = call_verb(proc, "create", {
            "kind": "entity",
            "entity_kind": "concept",
            "name": "EpistemicClaim",
            "description": "Hypothesis: epistemic edges work",
        })
        claim_id = claim["id"]

        paper = call_verb(proc, "create", {
            "kind": "entity",
            "entity_kind": "document",
            "name": "EpistemicEvidencePaper",
            "properties": {"authors": "Test et al.", "year": 2024},
        })
        paper_id = paper["id"]

        sup_edge = call_verb(proc, "link", {
            "source_id": paper_id,
            "target_id": claim_id,
            "relation": "supports",
            "weight": 0.9,
        })
        assert sup_edge["relation"] == "supports", f"expected supports, got: {sup_edge}"
        sup_edge_id = sup_edge["id"]
        print(f"  [epistemic] link document -[supports]-> concept — ok")

        # Verify via neighbors(direction=in) on the claim: the paper must appear as
        # an inbound supports neighbor.  Direction=in means "edges INTO the node"
        # i.e. source→node; the evidence paper is the source. (ADR-055: query the
        # inverse with direction=in, exactly as for every other directional relation.)
        nbrs_in = call_verb(proc, "neighbors", {
            "node_id": claim_id,
            "direction": "in",
            "relations": ["supports"],
        })
        nbr_ids = [n.get("id", "") for n in nbrs_in]
        assert any(nid == paper_id or nid.startswith(paper_id) for nid in nbr_ids), (
            f"paper must appear as inbound supports neighbor of claim; got: {nbr_ids}"
        )
        print(f"  [epistemic] neighbors(direction=in, supports) sees evidence paper — ok")

        # Confirm via get that the edge fields are correct
        fetched_edge = call_verb(proc, "get", {"id": sup_edge_id})
        assert fetched_edge["kind"] == "edge", f"expected kind=edge: {fetched_edge}"
        assert fetched_edge["relation"] == "supports", f"expected supports relation: {fetched_edge}"
        print(f"  [epistemic] get supports edge — ok")

        # --- entity-form: document → concept (refutes) ---
        counter = call_verb(proc, "create", {
            "kind": "entity",
            "entity_kind": "document",
            "name": "EpistemicCounterEvidencePaper",
        })
        counter_id = counter["id"]

        ref_edge = call_verb(proc, "link", {
            "source_id": counter_id,
            "target_id": claim_id,
            "relation": "refutes",
            "weight": 0.7,
        })
        assert ref_edge["relation"] == "refutes", f"expected refutes, got: {ref_edge}"
        print(f"  [epistemic] link document -[refutes]-> concept — ok")

        # Verify both edges are visible together as inbound neighbors
        nbrs_both = call_verb(proc, "neighbors", {
            "node_id": claim_id,
            "direction": "in",
        })
        all_neighbor_ids = [n.get("id", "") for n in nbrs_both]
        assert any(nid == paper_id or nid.startswith(paper_id) for nid in all_neighbor_ids), (
            f"supports paper must appear in combined inbound neighbors: {all_neighbor_ids}"
        )
        assert any(nid == counter_id or nid.startswith(counter_id) for nid in all_neighbor_ids), (
            f"refutes paper must appear in combined inbound neighbors: {all_neighbor_ids}"
        )
        print(f"  [epistemic] neighbors(direction=in) sees both supports + refutes evidence — ok")

        # --- note-form: observation -[supports]-> question (Note→Note rail) ---
        # ADR-055 §"Primary rail: Note→Note": any note kind → any note kind,
        # enforced at substrate level (operations.rs:702).
        finding_note = call_verb(proc, "create", {
            "kind": "note",
            "note_kind": "observation",
            "content": "Experiment result confirms the hypothesis with p<0.001",
        })
        finding_id = finding_note["id"]

        hypothesis_note = call_verb(proc, "create", {
            "kind": "note",
            "note_kind": "question",
            "content": "Does epistemic edge feature work correctly?",
        })
        hypothesis_id = hypothesis_note["id"]

        note_sup_edge = call_verb(proc, "link", {
            "source_id": finding_id,
            "target_id": hypothesis_id,
            "relation": "supports",
            "weight": 0.85,
        })
        assert note_sup_edge["relation"] == "supports", (
            f"Note→Note supports edge must succeed: {note_sup_edge}"
        )
        print(f"  [epistemic] link observation -[supports]-> question (Note→Note rail) — ok")

        # --- NEGATIVE case: document -[supports]-> document must be REJECTED ---
        # ADR-055 §"Secondary rail": target must be concept for entity-form.
        # document → document is rejected because document is not concept.
        # Error from operations.rs:695-699: "(document) -[supports]-> (document) is not
        # in the base endpoint allowlist; supports requires concept|document|dataset|artifact
        # -> concept (or same-substrate note -> note)"
        other_doc = call_verb(proc, "create", {
            "kind": "entity",
            "entity_kind": "document",
            "name": "EpistemicDocTarget",
        })
        other_doc_id = other_doc["id"]

        ops_neg = json.dumps([{"tool": "link", "args": {
            "source_id": paper_id,
            "target_id": other_doc_id,
            "relation": "supports",
        }}])
        body_neg = _call_request_raw(proc, ops_neg)
        results_neg = body_neg.get("results") or []
        assert results_neg, f"expected at least one result entry in batch response: {body_neg}"
        neg_result = results_neg[0]
        assert not neg_result.get("ok", True), (
            f"document -[supports]-> document must be rejected (target must be concept); "
            f"got ok=True: {neg_result}"
        )
        err_msg = neg_result.get("error", "")
        assert "allowlist" in err_msg or "concept" in err_msg, (
            f"rejection error must mention 'allowlist' or 'concept'; got: {err_msg!r}"
        )
        print(f"  [epistemic] document -[supports]-> document rejected (target not concept) — ok")

        print(f"\n  EPISTEMIC SMOKE TESTS PASSED (ADR-055)")
    finally:
        proc.stdin.close()
        proc.wait(timeout=5)


if __name__ == "__main__":
    failed_sections: list[str] = []

    code = main()
    if code != 0:
        failed_sections.append("kg")

    if os.environ.get("KHIVE_SMOKE_GTD", "1") != "0":
        try:
            gtd_smoke()
        except Exception as e:
            print(f"  [gtd FAIL] {e}")
            failed_sections.append("gtd")

    if os.environ.get("KHIVE_SMOKE_MEMORY", "1") != "0":
        try:
            memory_smoke()
        except Exception as e:
            print(f"  [memory FAIL] {e}")
            failed_sections.append("memory")

    if os.environ.get("KHIVE_SMOKE_FORMAL", "1") != "0":
        try:
            formal_smoke()
        except Exception as e:
            print(f"  [formal FAIL] {e}")
            failed_sections.append("formal")

    if os.environ.get("KHIVE_SMOKE_EPISTEMIC", "1") != "0":
        try:
            epistemic_smoke()
        except Exception as e:
            print(f"  [epistemic FAIL] {e}")
            failed_sections.append("epistemic")

    if failed_sections:
        print(f"\nFAILED sections: {', '.join(failed_sections)}", file=sys.stderr)
        sys.exit(1)
    sys.exit(0)
