#!/usr/bin/env python3
"""Smoke test for khive-mcp binary over stdio MCP.

Spawns the binary with an in-memory DB, sends JSON-RPC MCP requests,
and verifies the full verb-consolidated tool surface (ADR-023 + ADR-024)
works end-to-end.

11 tools: create, get, list, update, delete, merge, search,
          link, neighbors, traverse, query.

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


def call_tool(proc, name, args):
    send(proc, "tools/call", {"name": name, "arguments": args})
    resp = recv(proc)
    if "error" in resp:
        raise RuntimeError(f"MCP error calling {name}: {resp['error']}")
    result = resp.get("result", {})
    if result.get("isError"):
        content = result.get("content", [])
        text = content[0]["text"] if content else "(no text)"
        raise RuntimeError(f"Tool {name} returned error: {text}")
    content = result.get("content", [])
    text = content[0]["text"] if content else ""
    return json.loads(text) if text else None


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

        # 2. List tools — expect exactly 11 verb tools (ADR-023 + ADR-024)
        send(proc, "tools/list", {})
        tools_resp = recv(proc)
        tool_names = [t["name"] for t in tools_resp["result"]["tools"]]
        print(f"  [ok] tools/list — {len(tool_names)} tools: {', '.join(sorted(tool_names))}")
        expected_tools = {
            "create", "get", "list", "update", "delete", "merge",
            "search", "link", "neighbors", "traverse", "query",
        }
        missing = expected_tools - set(tool_names)
        assert not missing, f"Missing tools: {missing}"
        assert "resolve" not in tool_names, "resolve must not exist (absorbed into get)"
        assert len(tool_names) == 11, f"Expected 11 tools, got {len(tool_names)}: {tool_names}"

        # 3. Create entities via create(kind="entity")
        lora = call_tool(proc, "create", {
            "kind": "entity",
            "entity_kind": "concept",
            "name": "LoRA",
            "description": "Low-Rank Adaptation",
            "properties": {"domain": "fine-tuning", "year": 2021},
        })
        assert lora["name"] == "LoRA", f"unexpected: {lora}"
        lora_id = lora["id"]
        print(f"  [ok] create entity — LoRA ({lora_id[:8]}...)")

        qlora = call_tool(proc, "create", {
            "kind": "entity",
            "entity_kind": "concept",
            "name": "QLoRA",
            "description": "Quantized LoRA",
        })
        qlora_id = qlora["id"]
        print(f"  [ok] create entity — QLoRA ({qlora_id[:8]}...)")

        paper = call_tool(proc, "create", {
            "kind": "entity",
            "entity_kind": "document",
            "name": "LoRA: Low-Rank Adaptation of Large Language Models",
            "properties": {"authors": "Hu et al.", "year": 2021},
        })
        paper_id = paper["id"]
        print(f"  [ok] create entity — paper ({paper_id[:8]}...)")

        # 4. Get entity via get (auto-detects kind; returns {"kind": "entity", "data": {...}})
        fetched = call_tool(proc, "get", {"id": lora_id})
        assert fetched["kind"] == "entity", f"expected kind=entity, got: {fetched}"
        assert fetched["data"]["name"] == "LoRA", f"unexpected: {fetched}"
        print(f"  [ok] get entity — wrapped response kind={fetched['kind']}")

        # 5. List entities via list(kind="entity")
        entities = call_tool(proc, "list", {"kind": "entity", "entity_kind": "concept"})
        assert len(entities) == 2, f"expected 2 concepts, got {len(entities)}"
        print(f"  [ok] list entities — {len(entities)} concepts")

        # 6. Create edges via link
        edge1 = call_tool(proc, "link", {
            "source_id": qlora_id,
            "target_id": lora_id,
            "relation": "variant_of",
            "weight": 0.9,
        })
        assert edge1["relation"] == "variant_of"
        print(f"  [ok] link — QLoRA variant_of LoRA")

        edge2 = call_tool(proc, "link", {
            "source_id": paper_id,
            "target_id": lora_id,
            "relation": "introduced_by",
            "weight": 1.0,
        })
        print(f"  [ok] link — paper introduced_by LoRA")

        # 7. Get edge via get (auto-detects kind; returns {"kind": "edge", "data": {...}})
        edge_id = edge1["id"]
        fetched_edge = call_tool(proc, "get", {"id": edge_id})
        assert fetched_edge["kind"] == "edge", f"expected kind=edge, got: {fetched_edge}"
        print(f"  [ok] get edge — wrapped response kind={fetched_edge['kind']}")

        # 8. Neighbors via neighbors
        nbrs = call_tool(proc, "neighbors", {
            "node_id": lora_id,
            "direction": "in",
        })
        assert len(nbrs) == 2, f"expected 2 inbound neighbors, got {len(nbrs)}"
        print(f"  [ok] neighbors — {len(nbrs)} inbound to LoRA")

        # 9. Edge list via list(kind="edge")
        edges = call_tool(proc, "list", {"kind": "edge", "source_id": qlora_id})
        assert len(edges) == 1
        print(f"  [ok] list edges")

        # 10. Edge update via update (auto-detects kind from UUID)
        updated_edge = call_tool(proc, "update", {"id": edge_id, "weight": 0.95})
        assert abs(updated_edge["weight"] - 0.95) < 0.01
        print(f"  [ok] update edge weight")

        # 11. Entity update via update (auto-detects kind from UUID)
        patched = call_tool(proc, "update", {
            "id": lora_id,
            "description": "Low-Rank Adaptation of LLMs",
        })
        assert patched["description"] == "Low-Rank Adaptation of LLMs"
        print(f"  [ok] update entity")

        # 12. Create note via create(kind="note")
        note = call_tool(proc, "create", {
            "kind": "note",
            "note_kind": "observation",
            "content": "LoRA reduces trainable parameters by 10000x",
            "salience": 0.8,
        })
        assert note["kind"] == "observation"
        note_id = note["id"]
        print(f"  [ok] create note — observation ({note_id[:8]}...)")

        # 13. List notes via list(kind="note")
        notes = call_tool(proc, "list", {"kind": "note", "note_kind": "observation"})
        assert len(notes) == 1
        print(f"  [ok] list notes — {len(notes)} observation")

        # 14. Search entities via search(kind="entity")
        search_hits = call_tool(proc, "search", {
            "kind": "entity",
            "query": "LoRA parameter efficient fine-tuning",
            "limit": 5,
        })
        assert isinstance(search_hits, list), f"expected list, got: {search_hits}"
        print(f"  [ok] search entities — {len(search_hits)} hit(s)")

        # 15. Search notes via search(kind="note")
        note_hits = call_tool(proc, "search", {
            "kind": "note",
            "query": "LoRA parameters",
            "limit": 5,
        })
        assert isinstance(note_hits, list), f"expected list, got: {note_hits}"
        print(f"  [ok] search notes — {len(note_hits)} hit(s)")

        # 16. Cross-substrate: create annotated note (ADR-024)
        annotated_note = call_tool(proc, "create", {
            "kind": "note",
            "note_kind": "insight",
            "content": "LoRA is parameter-efficient",
            "annotates": [lora_id],
        })
        annotated_note_id = annotated_note["id"]
        nbrs_in = call_tool(proc, "neighbors", {
            "node_id": lora_id,
            "direction": "in",
            "relations": ["annotates"],
        })
        assert len(nbrs_in) == 1, f"expected 1 annotates neighbor, got {len(nbrs_in)}"
        print(f"  [ok] create annotated note + neighbors(annotates)")

        # 17. GQL query
        rows = call_tool(proc, "query", {
            "query": "MATCH (a:concept)-[e:variant_of]->(b:concept) RETURN a, b LIMIT 10",
        })
        assert len(rows) >= 1, f"expected at least 1 row, got {len(rows)}"
        print(f"  [ok] query (GQL) — {len(rows)} row(s)")

        # 18. Entity merge via merge (auto-detects kind; both IDs must be entities)
        dupe = call_tool(proc, "create", {
            "kind": "entity",
            "entity_kind": "concept",
            "name": "LoRA duplicate",
        })
        summary = call_tool(proc, "merge", {
            "into_id": lora_id,
            "from_id": dupe["id"],
            "strategy": "prefer_into",
        })
        assert summary["kept_id"] == lora_id
        print(f"  [ok] merge entity")

        # 19. Entity delete via delete (auto-detects kind from UUID)
        del_result = call_tool(proc, "delete", {"id": qlora_id})
        assert del_result["deleted"] is True
        print(f"  [ok] delete entity")

        # 20. Edge delete via delete (auto-detects kind from UUID)
        del_edge = call_tool(proc, "delete", {"id": edge_id})
        assert del_edge["deleted"] is True
        print(f"  [ok] delete edge")

        # 21. Note delete via delete (auto-detects kind from UUID)
        del_note = call_tool(proc, "delete", {"id": note_id})
        assert del_note["deleted"] is True
        print(f"  [ok] delete note")

        # 22. Traverse
        a = call_tool(proc, "create", {"kind": "entity", "entity_kind": "concept", "name": "TraverseA"})
        b = call_tool(proc, "create", {"kind": "entity", "entity_kind": "concept", "name": "TraverseB"})
        c = call_tool(proc, "create", {"kind": "entity", "entity_kind": "concept", "name": "TraverseC"})
        call_tool(proc, "link", {"source_id": a["id"], "target_id": b["id"], "relation": "extends"})
        call_tool(proc, "link", {"source_id": b["id"], "target_id": c["id"], "relation": "extends"})
        paths = call_tool(proc, "traverse", {
            "roots": [a["id"]],
            "max_depth": 2,
            "include_roots": False,
        })
        all_node_ids = [n["node_id"] for p in paths for n in p.get("nodes", [])]
        assert b["id"] in all_node_ids, "B must be reachable"
        assert c["id"] in all_node_ids, "C must be reachable at depth 2"
        print(f"  [ok] traverse — depth-2 multi-hop")

        print(f"\n  ALL 11 VERB TOOLS SMOKE TESTS PASSED")

    finally:
        proc.stdin.close()
        proc.wait(timeout=5)

    return 0


if __name__ == "__main__":
    sys.exit(main())
