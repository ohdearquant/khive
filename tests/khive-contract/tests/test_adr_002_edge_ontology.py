"""Edge ontology contract tests.

ADR: ADR-002
section: 17 canonical relations; Base endpoint contract; Cascade behavior;
         Annotation relation; Endpoint validation
"""

from __future__ import annotations

import pytest

from khive_contract.client import KhiveMcpSession, KhiveOperationError
from khive_contract.fixtures import EDGE_RELATIONS

VERBS_UNDER_TEST = {"create", "link", "get", "list", "neighbors", "delete"}

# Relations confirmed to work concept-to-concept in the runtime base allowlist.
# introduced_by and implements require specific non-concept endpoint types.
# competes_with and composed_with are symmetric: runtime may canonicalize endpoint order.
# supports and refutes (ADR-055 epistemic) also permit concept→concept.
CONCEPT_CONCEPT_RELATIONS = (
    "extends",
    "enables",
    "contains",
    "part_of",
    "instance_of",
    "variant_of",
    "supersedes",
    "competes_with",
    "composed_with",
    "supports",
    "refutes",
)

# All 17 canonical relations (ADR-002 base 15 + ADR-055 epistemic 2).
# Imported from fixtures.py — single source of truth.
ALL_CANONICAL_RELATIONS = tuple(sorted(EDGE_RELATIONS))


@pytest.mark.adr_002
@pytest.mark.slow
@pytest.mark.parametrize("relation", CONCEPT_CONCEPT_RELATIONS)
def test_link_concept_to_concept_relations(
    khive_session: KhiveMcpSession,
    temp_namespace: str,
    sample_entity,
    relation: str,
) -> None:
    """Each non-annotates relation links concept→concept and returns a valid edge.

    ADR: ADR-002
    section: 13 canonical relations; Base endpoint contract

    Each link succeeds, relation matches, get returns kind=="edge" wrapper.
    """
    src = khive_session.verb("create", sample_entity(entity_kind="concept", name=f"src_{relation}"))
    tgt = khive_session.verb("create", sample_entity(entity_kind="concept", name=f"tgt_{relation}"))

    edge = khive_session.verb("link", {
        "source_id": src["id"],
        "target_id": tgt["id"],
        "relation": relation,
        "namespace": temp_namespace,
    })
    assert edge is not None, f"link({relation}) returned None"
    assert edge.get("id"), f"link({relation}) missing 'id': {edge}"
    assert edge.get("relation") == relation, (
        f"link relation mismatch: got {edge.get('relation')!r}, expected {relation!r}"
    )
    # Some symmetric relations are canonicalized by the runtime (endpoint order may swap)
    assert {edge.get("source_id"), edge.get("target_id")} == {src["id"], tgt["id"]}, (
        f"edge endpoints wrong: {edge}"
    )

    # get must return kind=="edge" wrapper
    fetched = khive_session.verb("get", {"id": edge["id"], "namespace": temp_namespace})
    assert fetched.get("kind") == "edge", (
        f"get wrapper kind should be 'edge', got {fetched.get('kind')!r}"
    )


@pytest.mark.adr_002
@pytest.mark.slow
def test_invalid_relation_reports_closed_relation_set(
    khive_session: KhiveMcpSession,
    temp_namespace: str,
    sample_entity,
) -> None:
    """link with invalid relation returns per-op error listing all 17 canonical relations.

    ADR: ADR-002
    section: Rules; Closed-set taxonomy
    """
    src = khive_session.verb("create", sample_entity(entity_kind="concept", name="TaxSrc"))
    tgt = khive_session.verb("create", sample_entity(entity_kind="concept", name="TaxTgt"))

    envelope = khive_session.request_batch([
        {"tool": "link", "args": {
            "source_id": src["id"],
            "target_id": tgt["id"],
            "relation": "invented_by",
            "namespace": temp_namespace,
        }}
    ])
    results = envelope.get("results", [])
    assert results, "Expected results in envelope"
    first = results[0]
    assert not first.get("ok", False), "Expected per-op error for invalid relation"
    err = first.get("error", "")
    assert err, "Error message must be non-empty"
    assert "invented_by" in err, f"Error must name offending relation 'invented_by': {err!r}"

    # All 17 canonical relations must be listed
    for rel in ALL_CANONICAL_RELATIONS:
        assert rel in err, (
            f"Canonical relation '{rel}' missing from error message: {err!r}"
        )


@pytest.mark.adr_002
@pytest.mark.slow
def test_hard_delete_cascades_incident_edges_soft_delete_preserves(
    khive_session: KhiveMcpSession,
    temp_namespace: str,
    sample_entity,
) -> None:
    """Hard-delete removes incident edges; soft-delete leaves edges in place.

    ADR: ADR-002
    section: Cascade Behavior; ADR-014 Soft vs hard delete

    Ports test_edge_cascade_hard_delete from contract_test.py.
    """
    hub = khive_session.verb("create", sample_entity(entity_kind="concept", name="HubHard"))
    spoke1 = khive_session.verb("create", sample_entity(entity_kind="concept", name="Spoke1Hard"))
    spoke2 = khive_session.verb("create", sample_entity(entity_kind="concept", name="Spoke2Hard"))

    e1 = khive_session.verb("link", {
        "source_id": hub["id"], "target_id": spoke1["id"],
        "relation": "extends", "namespace": temp_namespace,
    })
    e2 = khive_session.verb("link", {
        "source_id": spoke2["id"], "target_id": hub["id"],
        "relation": "enables", "namespace": temp_namespace,
    })
    e1_id, e2_id = e1["id"], e2["id"]

    # Verify edges exist before delete
    edges_before = khive_session.verb("list", {"kind": "edge", "source_id": hub["id"],
                                               "namespace": temp_namespace})
    assert any(e.get("id") == e1_id for e in edges_before), (
        "outbound edge from hub not listed before hard-delete"
    )

    # Hard-delete the hub
    del_result = khive_session.verb("delete", {
        "id": hub["id"], "kind": "entity", "hard": True, "namespace": temp_namespace,
    })
    assert del_result.get("deleted") is True, f"Hard delete should return deleted=True: {del_result}"

    # Both incident edges must be gone
    envelope_e1 = khive_session.request_batch([{"tool": "get", "args": {"id": e1_id,
                                                                          "namespace": temp_namespace}}])
    first_e1 = envelope_e1["results"][0]
    assert not first_e1.get("ok", False), "Outbound edge should be gone after hard-delete"
    assert "not found" in first_e1.get("error", "").lower(), (
        f"Expected not-found error for outbound edge, got: {first_e1.get('error')!r}"
    )

    envelope_e2 = khive_session.request_batch([{"tool": "get", "args": {"id": e2_id,
                                                                          "namespace": temp_namespace}}])
    first_e2 = envelope_e2["results"][0]
    assert not first_e2.get("ok", False), "Inbound edge should be gone after hard-delete"
    assert "not found" in first_e2.get("error", "").lower(), (
        f"Expected not-found error for inbound edge, got: {first_e2.get('error')!r}"
    )

    # Soft delete: edges must remain
    hub_soft = khive_session.verb("create", sample_entity(entity_kind="concept", name="HubSoft"))
    spoke_soft = khive_session.verb("create", sample_entity(entity_kind="concept", name="SpokeSoft"))
    e_soft = khive_session.verb("link", {
        "source_id": hub_soft["id"], "target_id": spoke_soft["id"],
        "relation": "extends", "namespace": temp_namespace,
    })
    e_soft_id = e_soft["id"]

    del_soft = khive_session.verb("delete", {"id": hub_soft["id"], "kind": "entity",
                                              "namespace": temp_namespace})
    assert del_soft.get("deleted") is True

    # Edge should still be retrievable after soft delete
    fetched_edge = khive_session.verb("get", {"id": e_soft_id, "namespace": temp_namespace})
    assert fetched_edge.get("kind") == "edge", (
        f"Edge should survive soft-delete of incident entity: {fetched_edge}"
    )


@pytest.mark.adr_002
@pytest.mark.slow
def test_annotates_requires_note_source_and_cascades_on_hard_delete(
    khive_session: KhiveMcpSession,
    temp_namespace: str,
    sample_entity,
    sample_note,
) -> None:
    """annotates source must be a note; entity-as-source rejected; cascade on hard delete.

    ADR: ADR-002
    section: Annotation relation; Cascade Behavior; Endpoint Validation

    Ports test_annotates_source_must_be_note from contract_test.py.
    """
    concept = khive_session.verb("create", sample_entity(entity_kind="concept", name="AnnotatesTarget"))
    another = khive_session.verb("create", sample_entity(entity_kind="concept", name="WrongSource"))

    # entity → entity annotates must fail
    envelope = khive_session.request_batch([
        {"tool": "link", "args": {
            "source_id": another["id"],
            "target_id": concept["id"],
            "relation": "annotates",
            "namespace": temp_namespace,
        }}
    ])
    first = envelope["results"][0]
    assert not first.get("ok", False), "entity→entity annotates must fail"
    err = first.get("error", "")
    assert "note" in err.lower(), f"Error must mention 'note' (ADR-002 constraint): {err!r}"
    assert "annotates" in err.lower(), f"Error must mention 'annotates': {err!r}"

    # No edge must have been created
    edges_after = khive_session.verb("list", {
        "kind": "edge", "source_id": another["id"], "namespace": temp_namespace,
    })
    assert edges_after == [], (
        f"No edge should exist after rejected annotates link, got: {edges_after}"
    )

    # note → entity annotates must succeed
    note = khive_session.verb("create", sample_note(
        note_kind="observation",
        content="Observation about AnnotatesTarget",
        salience=0.7,
    ))
    edge = khive_session.verb("link", {
        "source_id": note["id"],
        "target_id": concept["id"],
        "relation": "annotates",
        "weight": 1.0,
        "namespace": temp_namespace,
    })
    assert edge.get("relation") == "annotates", f"Expected annotates edge, got: {edge}"
    edge_id = edge["id"]

    # Confirm note appears as inbound annotates neighbor of concept
    nbrs = khive_session.verb("neighbors", {
        "node_id": concept["id"],
        "direction": "in",
        "relations": ["annotates"],
        "namespace": temp_namespace,
    })
    neighbor_ids = [n.get("id", "") for n in nbrs]
    assert note["id"] in neighbor_ids, (
        f"Note should appear as annotates neighbor of concept; neighbors: {neighbor_ids}"
    )

    # Hard-delete the target entity cascades the annotates edge
    del_result = khive_session.verb("delete", {
        "id": concept["id"], "kind": "entity", "hard": True, "namespace": temp_namespace,
    })
    assert del_result.get("deleted") is True

    # Edge must be gone
    envelope_edge = khive_session.request_batch([{"tool": "get", "args": {"id": edge_id,
                                                                            "namespace": temp_namespace}}])
    first_edge = envelope_edge["results"][0]
    assert not first_edge.get("ok", False), "annotates edge must be cascade-deleted"
    assert "not found" in first_edge.get("error", "").lower(), (
        f"annotates edge must be cascade-deleted when target hard-deleted; "
        f"got: {first_edge.get('error')!r}"
    )
