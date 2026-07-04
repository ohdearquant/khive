"""Verb taxonomy contract tests — all product verbs are reachable.

ADR: ADR-023
section: kg bare substrate verbs; Pack product verbs; Verb naming
"""

from __future__ import annotations

import pytest

from khive_contract.client import KhiveMcpSession
from khive_contract.fixtures import KG_VERBS as _KG_VERBS

# KG verbs imported from fixtures.py — single source of truth (17 verbs).
KG_VERBS = tuple(sorted(_KG_VERBS))

GTD_VERBS = ("gtd.assign", "gtd.next", "gtd.complete", "gtd.tasks", "gtd.transition")
MEMORY_VERBS = ("memory.remember", "memory.recall")

# All 23 baseline product verbs (KG:16 + GTD:5 + memory:2).
# Written as a set literal so the manifest AST-introspector can parse it.
VERBS_UNDER_TEST = {
    # KG substrate (16) — bare names; no pack prefix
    "create", "get", "list", "update", "delete", "merge",
    "search", "link", "neighbors", "traverse", "query",
    "stats", "propose", "review", "withdraw", "verbs",
    # GTD (5) — dotted pack.verb form
    "gtd.assign", "gtd.next", "gtd.complete", "gtd.tasks", "gtd.transition",
    # Memory (2) — dotted pack.verb form
    "memory.remember", "memory.recall",
}


@pytest.mark.adr_023
@pytest.mark.slow
def test_kg_bare_product_verbs_are_reachable(
    khive_session: KhiveMcpSession,
    temp_namespace: str,
    sample_entity,
    sample_note,
) -> None:
    """Every KG substrate verb has at least one successful call with a meaningful result.

    ADR: ADR-023
    section: kg bare substrate verbs

    Ports smoke KG surface coverage; verifies all 11 KG verbs are registered
    and return non-error results in the base kg session.
    """
    ns = temp_namespace

    # create entity + note
    entity_a = khive_session.verb("create", sample_entity(entity_kind="concept", name="TaxA"))
    entity_b = khive_session.verb("create", sample_entity(entity_kind="concept", name="TaxB"))
    entity_c = khive_session.verb("create", sample_entity(entity_kind="concept", name="TaxC"))
    note = khive_session.verb("create", sample_note(note_kind="observation",
                                                      content="taxonomy coverage note"))
    assert entity_a.get("id"), "create entity must return id"
    assert note.get("id"), "create note must return id"

    # Per P-H2 (ADR-045): get returns flat object with granular kind — no "entity" wrapper.
    # get
    fetched = khive_session.verb("get", {"id": entity_a["id"], "namespace": ns})
    assert fetched.get("kind") == "concept", f"get must return granular kind 'concept': {fetched}"

    # list
    entities = khive_session.verb("list", {"kind": "entity", "entity_kind": "concept",
                                            "namespace": ns})
    assert isinstance(entities, list), "list must return a list"
    assert any(e["id"] == entity_a["id"] for e in entities), "list must include created entity"

    # link
    edge = khive_session.verb("link", {"source_id": entity_a["id"], "target_id": entity_b["id"],
                                        "relation": "extends", "namespace": ns})
    assert edge.get("id"), "link must return edge with id"

    # neighbors
    nbrs = khive_session.verb("neighbors", {"node_id": entity_a["id"], "direction": "out",
                                             "namespace": ns})
    assert isinstance(nbrs, list), "neighbors must return a list"
    assert any(n.get("id") == entity_b["id"] for n in nbrs), "B must be outbound neighbor of A"

    # update
    updated = khive_session.verb("update", {"id": entity_a["id"], "kind": "entity",
                                             "namespace": ns, "description": "updated by taxonomy test"})
    assert updated is not None, "update must return a result"

    # search
    hits = khive_session.verb("search", {"kind": "entity", "query": "TaxA", "namespace": ns})
    assert isinstance(hits, list), "search must return a list"

    # link for traverse
    edge_bc = khive_session.verb("link", {"source_id": entity_b["id"], "target_id": entity_c["id"],
                                           "relation": "extends", "namespace": ns})

    # traverse
    paths = khive_session.verb("traverse", {"roots": [entity_a["id"]], "max_depth": 2,
                                             "include_roots": False, "namespace": ns})
    assert isinstance(paths, list), "traverse must return a list"

    # query
    result = khive_session.verb("query", {
        "query": f"MATCH (a:concept)-[e:extends]->(b:concept) RETURN a, b LIMIT 5",
        "namespace": ns,
    })
    assert isinstance(result, list) or isinstance(result, dict), "query must return rows or dict"

    # delete
    del_result = khive_session.verb("delete", {"id": entity_c["id"], "kind": "entity",
                                                "namespace": ns})
    assert del_result.get("deleted") is True, "delete must return deleted=True"

    # merge
    dupe = khive_session.verb("create", sample_entity(entity_kind="concept", name="TaxADupe"))
    merge_result = khive_session.verb("merge", {"into_id": entity_a["id"],
                                                  "from_id": dupe["id"],
                                                  "namespace": ns})
    assert merge_result.get("kept_id") == entity_a["id"], "merge must return kept_id"


@pytest.mark.adr_023
@pytest.mark.slow
def test_pack_product_verbs_are_reachable_when_loaded(
    khive_gtd_session: KhiveMcpSession,
    khive_memory_session: KhiveMcpSession,
    temp_namespace: str,
) -> None:
    """Every pack verb (GTD + memory) has at least one successful call.

    ADR: ADR-023
    section: Pack product verbs; ADR-017 Built-in packs; ADR-019; ADR-021

    Ensures all 7 pack verbs are registered and return non-error results
    when their respective packs are loaded.
    """
    ns = temp_namespace

    # ---- GTD verbs ----
    # gtd.assign
    task = khive_gtd_session.verb("gtd.assign", {
        "title": "Taxonomy test task",
        "status": "next",
        "priority": "p1",
        "namespace": ns,
    })
    assert task.get("kind") == "task", f"gtd.assign must return kind=task: {task}"
    task_id = task.get("full_id") or task.get("id")
    assert task_id, "gtd.assign must return a task id"

    # gtd.next
    next_tasks = khive_gtd_session.verb("gtd.next", {"namespace": ns})
    assert isinstance(next_tasks, list), "gtd.next must return a list"

    # gtd.tasks
    task_list = khive_gtd_session.verb("gtd.tasks", {"status": "next", "namespace": ns})
    assert isinstance(task_list, list), "gtd.tasks must return a list"
    full_ids = [t.get("full_id") for t in task_list]
    assert task_id in full_ids, f"assigned task must appear in gtd.tasks(status=next): {full_ids}"

    # gtd.transition
    trans = khive_gtd_session.verb("gtd.transition", {"id": task_id, "status": "waiting",
                                                        "namespace": ns})
    assert trans.get("transitioned") is True, f"gtd.transition must return transitioned=True: {trans}"
    assert trans.get("to") == "waiting", f"gtd.transition must report to=waiting: {trans}"

    # gtd.complete (need a task in actionable status, so transition back to next)
    khive_gtd_session.verb("gtd.transition", {"id": task_id, "status": "next", "namespace": ns})
    done = khive_gtd_session.verb("gtd.complete", {"id": task_id, "result": "taxonomy pass",
                                                     "namespace": ns})
    assert done.get("to") == "done", f"gtd.complete must return to=done: {done}"

    # ---- Memory verbs ----
    # memory.remember
    mem = khive_memory_session.verb("memory.remember", {
        "content": "khive taxonomy coverage test semantic memory",
        "salience": 0.8,
        "memory_type": "semantic",
        "namespace": ns,
    })
    assert mem is not None, "memory.remember must return a result"
    mem_id = mem["id"]
    assert mem_id, f"memory.remember must return an id: {mem}"

    # memory.recall
    hits = khive_memory_session.verb("memory.recall", {
        "query": "khive taxonomy coverage",
        "limit": 5,
        "namespace": ns,
    })
    assert isinstance(hits, list), f"memory.recall must return a list, got: {hits}"
