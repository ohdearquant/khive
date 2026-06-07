"""Smoke tests — full verb surface coverage across KG, GTD, and memory packs.

ADR: ADR-027
section: Single-tool surface; KG verb coverage; GTD pack verbs; Memory pack verbs
"""

from __future__ import annotations

import json

import pytest

from khive_contract.client import KhiveMcpSession, KhiveRpcError

VERBS_UNDER_TEST = {
    "create", "get", "list", "update", "delete", "merge",
    "search", "link", "neighbors", "traverse", "query",
    "gtd.assign", "gtd.next", "gtd.complete", "gtd.tasks", "gtd.transition",
    "memory.remember", "memory.recall",
}


@pytest.mark.adr_027
@pytest.mark.slow
def test_kg_smoke(
    khive_session: KhiveMcpSession,
    temp_namespace: str,
    sample_entity,
    sample_note,
) -> None:
    """Full KG verb surface smoke test: create→get→list→link→neighbors→update→search→query→merge→delete→traverse.

    ADR: ADR-027
    section: Single-tool surface; KG verb coverage

    Ports the complete flow from smoke_test.py main() into pytest.
    Uses temp_namespace for per-test isolation.
    """
    ns = temp_namespace

    # create entities
    lora = khive_session.verb("create", {
        "kind": "entity",
        "entity_kind": "concept",
        "name": "SmokeLoRA",
        "description": "Low-Rank Adaptation",
        "properties": {"domain": "fine-tuning", "year": 2021},
        "namespace": ns,
    })
    assert lora.get("name") == "SmokeLoRA", f"create entity name mismatch: {lora}"
    lora_id = lora["id"]

    qlora = khive_session.verb("create", {
        "kind": "entity",
        "entity_kind": "concept",
        "name": "SmokeQLoRA",
        "description": "Quantized LoRA",
        "namespace": ns,
    })
    qlora_id = qlora["id"]

    paper = khive_session.verb("create", {
        "kind": "entity",
        "entity_kind": "document",
        "name": "SmokeLoRA Paper",
        "properties": {"authors": "Hu et al.", "year": 2021},
        "namespace": ns,
    })
    paper_id = paper["id"]

    # Per P-H2 (ADR-045): get returns flat object with granular kind — no {data: ...} wrapper.
    # get entity
    fetched = khive_session.verb("get", {"id": lora_id, "namespace": ns})
    assert fetched.get("kind") == "concept", f"get must return granular kind 'concept': {fetched}"
    assert fetched.get("name") == "SmokeLoRA", f"get name mismatch: {fetched}"

    # list entities
    concepts = khive_session.verb("list", {"kind": "entity", "entity_kind": "concept",
                                            "namespace": ns})
    assert isinstance(concepts, list), "list must return a list"
    concept_ids = [e["id"] for e in concepts]
    assert lora_id in concept_ids, "SmokeLoRA must appear in concept list"
    assert qlora_id in concept_ids, "SmokeQLoRA must appear in concept list"

    # link: QLoRA variant_of LoRA
    edge1 = khive_session.verb("link", {
        "source_id": qlora_id,
        "target_id": lora_id,
        "relation": "variant_of",
        "weight": 0.9,
        "namespace": ns,
    })
    assert edge1.get("relation") == "variant_of", f"link relation mismatch: {edge1}"
    edge1_id = edge1["id"]

    # link: LoRA introduced_by paper (concept→document direction required by ADR-002)
    khive_session.verb("link", {
        "source_id": lora_id,
        "target_id": paper_id,
        "relation": "introduced_by",
        "weight": 1.0,
        "namespace": ns,
    })

    # get edge
    fetched_edge = khive_session.verb("get", {"id": edge1_id, "namespace": ns})
    assert fetched_edge.get("kind") == "edge", f"get edge must return kind=edge: {fetched_edge}"

    # neighbors
    nbrs_in = khive_session.verb("neighbors", {"node_id": lora_id, "direction": "in",
                                                "namespace": ns})
    assert isinstance(nbrs_in, list), "neighbors must return a list"
    assert len(nbrs_in) >= 1, f"LoRA must have >=1 inbound neighbors (QLoRA), got: {nbrs_in}"

    nbrs_out = khive_session.verb("neighbors", {"node_id": lora_id, "direction": "out",
                                                 "namespace": ns})
    assert isinstance(nbrs_out, list), "neighbors must return a list"
    assert len(nbrs_out) >= 1, f"LoRA must have >=1 outbound neighbors (paper), got: {nbrs_out}"

    # edge list
    edges_from_qlora = khive_session.verb("list", {"kind": "edge", "source_id": qlora_id,
                                                    "namespace": ns})
    assert isinstance(edges_from_qlora, list), "list edges must return a list"
    assert len(edges_from_qlora) >= 1, "QLoRA must have >=1 outbound edge"

    # update edge weight
    updated_edge = khive_session.verb("update", {
        "id": edge1_id,
        "kind": "edge",
        "weight": 0.95,
        "namespace": ns,
    })
    assert updated_edge is not None, "update edge returned None"

    # update entity description
    patched = khive_session.verb("update", {
        "id": lora_id,
        "kind": "entity",
        "description": "Low-Rank Adaptation of LLMs",
        "namespace": ns,
    })
    assert patched is not None, "update entity returned None"

    # create note
    note = khive_session.verb("create", {
        "kind": "note",
        "note_kind": "observation",
        "content": "LoRA reduces trainable parameters by 10000x",
        "salience": 0.8,
        "namespace": ns,
    })
    assert note.get("kind") == "observation", f"note kind mismatch: {note}"
    note_id = note["id"]

    # list notes
    notes = khive_session.verb("list", {"kind": "note", "note_kind": "observation",
                                         "namespace": ns})
    assert isinstance(notes, list), "list notes must return a list"
    note_ids = [n["id"] for n in notes]
    assert note_id in note_ids, "created observation note must appear in list"

    # search entities
    search_hits = khive_session.verb("search", {
        "kind": "entity",
        "query": "LoRA parameter efficient",
        "limit": 5,
        "namespace": ns,
    })
    assert isinstance(search_hits, list), f"search entities must return a list: {search_hits}"

    # search notes
    note_hits = khive_session.verb("search", {
        "kind": "note",
        "query": "LoRA parameters",
        "limit": 5,
        "namespace": ns,
    })
    assert isinstance(note_hits, list), f"search notes must return a list: {note_hits}"

    # annotated note (ADR-024 convenience shortcut)
    ann_note = khive_session.verb("create", {
        "kind": "note",
        "note_kind": "insight",
        "content": "LoRA is parameter-efficient",
        "annotates": [lora_id],
        "namespace": ns,
    })
    assert ann_note is not None, "annotated note create must return a result"
    ann_nbrs = khive_session.verb("neighbors", {
        "node_id": lora_id,
        "direction": "in",
        "relations": ["annotates"],
        "namespace": ns,
    })
    assert isinstance(ann_nbrs, list), "annotates neighbors must return a list"
    assert len(ann_nbrs) >= 1, f"LoRA must have >=1 annotates inbound neighbors: {ann_nbrs}"

    # GQL query
    query_result = khive_session.verb("query", {
        "query": "MATCH (a:concept)-[e:variant_of]->(b:concept) RETURN a, b LIMIT 10",
        "namespace": ns,
    })
    rows = query_result.get("rows", query_result) if isinstance(query_result, dict) else query_result
    assert isinstance(rows, list), f"query must return list of rows: {query_result}"
    assert len(rows) >= 1, f"Expected >=1 GQL rows: {rows}"

    # merge
    dupe = khive_session.verb("create", {
        "kind": "entity",
        "entity_kind": "concept",
        "name": "SmokeLoRADupe",
        "namespace": ns,
    })
    merge_summary = khive_session.verb("merge", {
        "into_id": lora_id,
        "from_id": dupe["id"],
        "strategy": "prefer_into",
        "namespace": ns,
    })
    assert merge_summary.get("kept_id") == lora_id, (
        f"merge must return kept_id={lora_id}: {merge_summary}"
    )

    # delete entity
    del_entity = khive_session.verb("delete", {"id": qlora_id, "kind": "entity",
                                                "namespace": ns})
    assert del_entity.get("deleted") is True, f"delete entity must return deleted=True: {del_entity}"

    # delete edge
    del_edge = khive_session.verb("delete", {"id": edge1_id, "kind": "edge",
                                              "namespace": ns})
    assert del_edge.get("deleted") is True, f"delete edge must return deleted=True: {del_edge}"

    # delete note
    del_note = khive_session.verb("delete", {"id": note_id, "kind": "note",
                                              "namespace": ns})
    assert del_note.get("deleted") is True, f"delete note must return deleted=True: {del_note}"

    # traverse multi-hop
    a = khive_session.verb("create", {"kind": "entity", "entity_kind": "concept",
                                       "name": "TraverseA", "namespace": ns})
    b = khive_session.verb("create", {"kind": "entity", "entity_kind": "concept",
                                       "name": "TraverseB", "namespace": ns})
    c = khive_session.verb("create", {"kind": "entity", "entity_kind": "concept",
                                       "name": "TraverseC", "namespace": ns})
    khive_session.verb("link", {"source_id": a["id"], "target_id": b["id"],
                                 "relation": "extends", "namespace": ns})
    khive_session.verb("link", {"source_id": b["id"], "target_id": c["id"],
                                 "relation": "extends", "namespace": ns})
    paths = khive_session.verb("traverse", {
        "roots": [a["id"]],
        "max_depth": 2,
        "include_roots": False,
        "namespace": ns,
    })
    assert isinstance(paths, list), f"traverse must return a list: {paths}"
    all_node_ids = [n["id"] for p in paths for n in p.get("nodes", [])]
    assert b["id"] in all_node_ids, f"B must be reachable from A at depth 1: {all_node_ids}"
    assert c["id"] in all_node_ids, f"C must be reachable from A at depth 2: {all_node_ids}"

    # parallel batch
    envelope = khive_session.request_batch([
        {"tool": "create", "args": {"kind": "entity", "entity_kind": "concept",
                                     "name": "BulkA", "namespace": ns}},
        {"tool": "create", "args": {"kind": "entity", "entity_kind": "concept",
                                     "name": "BulkB", "namespace": ns}},
        {"tool": "create", "args": {"kind": "entity", "entity_kind": "concept",
                                     "name": "BulkC", "namespace": ns}},
    ])
    summary = envelope.get("summary", {})
    assert summary.get("total") == 3 and summary.get("failed") == 0, (
        f"parallel batch must have total=3, failed=0: {summary}"
    )


@pytest.mark.adr_027
@pytest.mark.slow
def test_gtd_smoke(
    khive_gtd_session: KhiveMcpSession,
    temp_namespace: str,
) -> None:
    """GTD pack smoke test: assign→next→tasks→transition→complete round-trip.

    ADR: ADR-027
    section: GTD pack verbs

    Ports gtd_smoke() from smoke_test.py into pytest.
    """
    ns = temp_namespace

    # assign
    assigned = khive_gtd_session.verb("gtd.assign", {
        "title": "smoke-gtd task",
        "status": "next",
        "priority": "p0",
        "namespace": ns,
    })
    assert assigned.get("kind") == "task", f"gtd.assign must return kind=task: {assigned}"
    assert assigned.get("status") == "next", f"gtd.assign status mismatch: {assigned}"
    task_full_id = assigned.get("full_id") or assigned.get("id")
    assert task_full_id, f"gtd.assign must return a task id: {assigned}"

    # next
    ready = khive_gtd_session.verb("gtd.next", {"namespace": ns})
    assert isinstance(ready, list), f"gtd.next must return a list: {ready}"
    assert any(t.get("full_id") == task_full_id for t in ready), (
        f"assigned task must appear in gtd.next(): {ready}"
    )

    # tasks
    waiting_task = khive_gtd_session.verb("gtd.assign", {
        "title": "waiting-task",
        "status": "waiting",
        "priority": "p1",
        "namespace": ns,
    })
    inbox_task = khive_gtd_session.verb("gtd.assign", {
        "title": "inbox-task",
        "status": "inbox",
        "priority": "p2",
        "namespace": ns,
    })
    waiting_tasks = khive_gtd_session.verb("gtd.tasks", {"status": "waiting", "namespace": ns})
    assert isinstance(waiting_tasks, list), f"gtd.tasks must return a list: {waiting_tasks}"
    waiting_ids = [t.get("full_id") for t in waiting_tasks]
    assert waiting_task.get("full_id") in waiting_ids, (
        f"waiting task must appear in gtd.tasks(status=waiting): {waiting_ids}"
    )
    assert inbox_task.get("full_id") not in waiting_ids, (
        f"inbox task must NOT appear in gtd.tasks(status=waiting): {waiting_ids}"
    )

    # transition
    trans = khive_gtd_session.verb("gtd.transition", {
        "id": inbox_task.get("full_id"),
        "status": "next",
        "note": "promoted from inbox",
        "namespace": ns,
    })
    assert trans.get("transitioned") is True, f"gtd.transition must set transitioned=True: {trans}"
    assert trans.get("to") == "next", f"gtd.transition must report to=next: {trans}"

    # idempotent transition
    trans_idem = khive_gtd_session.verb("gtd.transition", {
        "id": inbox_task.get("full_id"),
        "status": "next",
        "namespace": ns,
    })
    assert trans_idem.get("transitioned") is False, (
        f"idempotent gtd.transition must set transitioned=False: {trans_idem}"
    )

    # complete
    done = khive_gtd_session.verb("gtd.complete", {
        "id": task_full_id,
        "result": "smoke-test pass",
        "namespace": ns,
    })
    assert done.get("to") == "done", f"gtd.complete must return to=done: {done}"


@pytest.mark.adr_027
@pytest.mark.slow
def test_memory_smoke(
    khive_memory_session: KhiveMcpSession,
    temp_namespace: str,
) -> None:
    """Memory pack smoke test: remember + recall round-trip.

    ADR: ADR-027
    section: Memory pack verbs

    Ports memory_smoke() from smoke_test.py into pytest.
    """
    ns = temp_namespace

    # remember first memory
    mem = khive_memory_session.verb("memory.remember", {
        "content": "khive uses SQLite with FTS5 and sqlite-vec for hybrid search",
        "salience": 0.9,
        "memory_type": "semantic",
        "namespace": ns,
    })
    assert mem is not None, "memory.remember must return a result"
    mem_id = mem.get("id") or mem.get("note_id")
    assert mem_id, f"memory.remember must return an id: {mem}"

    # remember second memory
    mem2 = khive_memory_session.verb("memory.remember", {
        "content": "The runtime enforces namespace isolation for every ID-based operation",
        "salience": 0.7,
        "memory_type": "semantic",
        "namespace": ns,
    })
    assert mem2 is not None, "second memory.remember must return a result"

    # recall
    hits = khive_memory_session.verb("memory.recall", {
        "query": "SQLite hybrid search",
        "limit": 5,
        "namespace": ns,
    })
    assert isinstance(hits, list), f"memory.recall must return a list, got: {hits}"
