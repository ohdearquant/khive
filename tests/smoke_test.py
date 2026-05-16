#!/usr/bin/env python3
"""Smoke test for khive-mcp binary over stdio MCP.

Spawns the binary with an in-memory DB, sends JSON-RPC MCP requests,
and verifies the full tool surface works end-to-end.

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
        raise RuntimeError(f"MCP error: {resp['error']}")
    content = resp.get("result", {}).get("content", [])
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

        # 2. List tools
        send(proc, "tools/list", {})
        tools_resp = recv(proc)
        tool_names = [t["name"] for t in tools_resp["result"]["tools"]]
        print(f"  [ok] tools/list — {len(tool_names)} tools: {', '.join(sorted(tool_names))}")
        assert "entity_create" in tool_names
        assert "note_create" in tool_names
        assert "link" in tool_names
        assert "query" in tool_names

        # 3. Create entities
        lora = call_tool(proc, "entity_create", {
            "kind": "concept",
            "name": "LoRA",
            "description": "Low-Rank Adaptation",
            "properties": {"domain": "fine-tuning", "year": 2021},
        })
        assert lora["name"] == "LoRA", f"unexpected: {lora}"
        lora_id = lora["id"]
        print(f"  [ok] entity_create — LoRA ({lora_id[:8]}...)")

        qlora = call_tool(proc, "entity_create", {
            "kind": "concept",
            "name": "QLoRA",
            "description": "Quantized LoRA",
        })
        qlora_id = qlora["id"]
        print(f"  [ok] entity_create — QLoRA ({qlora_id[:8]}...)")

        paper = call_tool(proc, "entity_create", {
            "kind": "document",
            "name": "LoRA: Low-Rank Adaptation of Large Language Models",
            "properties": {"authors": "Hu et al.", "year": 2021},
        })
        paper_id = paper["id"]
        print(f"  [ok] entity_create — paper ({paper_id[:8]}...)")

        # 4. Get entity
        fetched = call_tool(proc, "entity_get", {"id": lora_id})
        assert fetched["name"] == "LoRA"
        print(f"  [ok] entity_get")

        # 5. List entities
        entities = call_tool(proc, "entity_list", {"kind": "concept"})
        assert len(entities) == 2, f"expected 2 concepts, got {len(entities)}"
        print(f"  [ok] entity_list — {len(entities)} concepts")

        # 6. Create edges
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

        # 7. Neighbors
        nbrs = call_tool(proc, "neighbors", {
            "node_id": lora_id,
            "direction": "in",
        })
        assert len(nbrs) == 2, f"expected 2 inbound neighbors, got {len(nbrs)}"
        print(f"  [ok] neighbors — {len(nbrs)} inbound to LoRA")

        # 8. Edge list
        edges = call_tool(proc, "edge_list", {"source_id": qlora_id})
        assert len(edges) == 1
        print(f"  [ok] edge_list")

        # 9. Edge update
        edge_id = edge1["id"]
        updated = call_tool(proc, "edge_update", {"id": edge_id, "weight": 0.95})
        assert abs(updated["weight"] - 0.95) < 0.01
        print(f"  [ok] edge_update")

        # 10. Entity update
        patched = call_tool(proc, "entity_update", {
            "id": lora_id,
            "description": "Low-Rank Adaptation of LLMs",
        })
        assert patched["description"] == "Low-Rank Adaptation of LLMs"
        print(f"  [ok] entity_update")

        # 11. Create note
        note = call_tool(proc, "note_create", {
            "kind": "observation",
            "content": "LoRA reduces trainable parameters by 10000x",
            "salience": 0.8,
        })
        assert note["kind"] == "observation"
        print(f"  [ok] note_create — observation ({note['id'][:8]}...)")

        # 12. List notes
        notes = call_tool(proc, "note_list", {"kind": "observation"})
        assert len(notes) == 1
        print(f"  [ok] note_list — {len(notes)} observation")

        # 13. GQL query
        rows = call_tool(proc, "query", {
            "query": "MATCH (a:concept)-[e:variant_of]->(b:concept) RETURN a, b LIMIT 10",
        })
        assert len(rows) >= 1, f"expected at least 1 row, got {len(rows)}"
        print(f"  [ok] query (GQL) — {len(rows)} row(s)")

        # 14. Entity merge
        dupe = call_tool(proc, "entity_create", {
            "kind": "concept",
            "name": "LoRA duplicate",
        })
        summary = call_tool(proc, "entity_merge", {
            "into_id": lora_id,
            "from_id": dupe["id"],
            "strategy": "prefer_into",
        })
        assert summary["kept_id"] == lora_id
        print(f"  [ok] entity_merge")

        # 15. Delete
        del_result = call_tool(proc, "entity_delete", {"id": qlora_id})
        assert del_result["deleted"] is True
        print(f"  [ok] entity_delete")

        # 16. Edge delete
        del_edge = call_tool(proc, "edge_delete", {"id": edge_id})
        assert del_edge["deleted"] is True
        print(f"  [ok] edge_delete")

        print(f"\n  ALL 16 TOOL SMOKE TESTS PASSED")

    finally:
        proc.stdin.close()
        proc.wait(timeout=5)

    return 0


if __name__ == "__main__":
    sys.exit(main())
