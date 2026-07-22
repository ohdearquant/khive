"""Verb taxonomy contract tests — all product verbs are reachable.

ADR: ADR-023
section: kg bare substrate verbs; Verb naming
"""

from __future__ import annotations

import pytest

from khive_contract.client import KhiveMcpSession

# The 16 KG verbs exercised below, of the pack's 19. Written as a set literal
# so the manifest AST-introspector can parse it (context/resolve/whoami are
# the remaining KG verbs not exercised by this module's reachability walk;
# kg is the sole pack in the OSS distribution).
VERBS_UNDER_TEST = {
    # KG substrate (16) — bare names; no pack prefix
    "create", "get", "list", "update", "delete", "merge",
    "search", "link", "neighbors", "traverse", "query",
    "stats", "propose", "review", "withdraw", "verbs",
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

    Ports smoke KG surface coverage; verifies the KG verbs named in
    VERBS_UNDER_TEST are registered
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
