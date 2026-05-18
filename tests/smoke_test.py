#!/usr/bin/env python3
"""Smoke test for khive-mcp binary over stdio MCP.

Spawns the binary with an in-memory DB, sends JSON-RPC MCP requests, and
verifies the full verb-consolidated surface works end-to-end. As of v0.2 the
MCP server exposes a single tool, `request`, that accepts a function-call DSL
or JSON-form batch; every verb is reached through it.

Verb semantics (unchanged from v0.1): create, get, list, update, delete, merge,
search, link, neighbors, traverse, query. get/update/delete/merge auto-detect
record kind from UUID — no kind= needed. get returns {"kind": "entity"|"note"|"edge", "data": {...}}.

Usage:
    uv run python tests/smoke_test.py
    # or: python3 tests/smoke_test.py
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
    """Call a single verb through `request`. Return that verb's result, or raise on per-op error.

    The wire shape is the single `request` MCP tool (ADR-027). Tests express
    intent in terms of verbs; this helper handles the encoding/unwrapping.
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

    proc = subprocess.Popen(
        [BINARY, "--db", ":memory:", "--no-embed", "--log", "error"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
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

        # 2. List tools — must be exactly `request` (ADR-027 single-tool surface).
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

        # 4. Get entity via get (auto-detects kind; returns {"kind": "entity", "data": {...}})
        fetched = call_verb(proc, "get", {"id": lora_id})
        assert fetched["kind"] == "entity", f"expected kind=entity, got: {fetched}"
        assert fetched["data"]["name"] == "LoRA", f"unexpected: {fetched}"
        print(f"  [ok] get entity — wrapped response kind={fetched['kind']}")

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

        call_verb(proc, "link", {
            "source_id": paper_id,
            "target_id": lora_id,
            "relation": "introduced_by",
            "weight": 1.0,
        })
        print(f"  [ok] link — paper introduced_by LoRA")

        # 7. Get edge via get (auto-detects kind)
        edge_id = edge1["id"]
        fetched_edge = call_verb(proc, "get", {"id": edge_id})
        assert fetched_edge["kind"] == "edge", f"expected kind=edge, got: {fetched_edge}"
        print(f"  [ok] get edge — wrapped response kind={fetched_edge['kind']}")

        # 8. Neighbors
        nbrs = call_verb(proc, "neighbors", {
            "node_id": lora_id,
            "direction": "in",
        })
        assert len(nbrs) == 2, f"expected 2 inbound neighbors, got {len(nbrs)}"
        print(f"  [ok] neighbors — {len(nbrs)} inbound to LoRA")

        # 9. Edge list
        edges = call_verb(proc, "list", {"kind": "edge", "source_id": qlora_id})
        assert len(edges) == 1
        print(f"  [ok] list edges")

        # 10. Edge update (auto-detects kind from UUID)
        updated_edge = call_verb(proc, "update", {"id": edge_id, "weight": 0.95})
        assert abs(updated_edge["weight"] - 0.95) < 0.01
        print(f"  [ok] update edge weight")

        # 11. Entity update (auto-detects kind from UUID)
        patched = call_verb(proc, "update", {
            "id": lora_id,
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
        del_result = call_verb(proc, "delete", {"id": qlora_id})
        assert del_result["deleted"] is True
        print(f"  [ok] delete entity")

        # 20. Edge delete
        del_edge = call_verb(proc, "delete", {"id": edge_id})
        assert del_edge["deleted"] is True
        print(f"  [ok] delete edge")

        # 21. Note delete
        del_note = call_verb(proc, "delete", {"id": note_id})
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
        all_node_ids = [n["node_id"] for p in paths for n in p.get("nodes", [])]
        assert b["id"] in all_node_ids, "B must be reachable"
        assert c["id"] in all_node_ids, "C must be reachable at depth 2"
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

        print(f"\n  ALL VERB SMOKE TESTS PASSED (single-tool surface)")

    finally:
        proc.stdin.close()
        proc.wait(timeout=5)

    return 0


if __name__ == "__main__":
    sys.exit(main())
