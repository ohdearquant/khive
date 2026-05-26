"""Curation operations contract tests: update, delete, merge.

ADR: ADR-014
section: Patch-style updates; Soft vs hard delete; merge_entity semantics
"""

from __future__ import annotations

import pytest

from khive_contract.client import KhiveMcpSession, KhiveOperationError

VERBS_UNDER_TEST = {"create", "link", "update", "delete", "merge", "get", "list"}


@pytest.mark.adr_014
@pytest.mark.slow
def test_update_entity_fields(
    khive_session: KhiveMcpSession,
    temp_namespace: str,
    sample_entity,
) -> None:
    """update(entity) patches description, tags, and properties; get reflects changes.

    ADR: ADR-014
    section: Patch-style updates
    """
    args = sample_entity(
        entity_kind="concept",
        name="UpdateTarget",
        description="original description",
        tags=["old"],
    )
    entity = khive_session.verb("create", args)
    entity_id = entity["id"]

    updated = khive_session.verb("update", {
        "id": entity_id,
        "kind": "entity",
        "namespace": temp_namespace,
        "description": "updated description",
        "tags": ["new", "fresh"],
    })
    assert updated is not None, "update returned None"

    # Per P-H2 (ADR-045): get returns flat object — no {data: ...} wrapper.
    fetched = khive_session.verb("get", {"id": entity_id, "namespace": temp_namespace})
    assert fetched.get("description") == "updated description", (
        f"description not updated: {fetched.get('description')!r}"
    )
    tags = set(fetched.get("tags", []))
    assert "new" in tags and "fresh" in tags, f"tags not updated: {tags}"
    assert "old" not in tags, f"old tag should be replaced: {tags}"


@pytest.mark.adr_014
@pytest.mark.slow
def test_update_edge_weight(
    khive_session: KhiveMcpSession,
    temp_namespace: str,
    sample_entity,
) -> None:
    """update(edge) patches weight; get reflects new value.

    ADR: ADR-014
    section: Patch-style updates
    """
    src = khive_session.verb("create", sample_entity(entity_kind="concept", name="EdgeUpdSrc"))
    tgt = khive_session.verb("create", sample_entity(entity_kind="concept", name="EdgeUpdTgt"))
    edge = khive_session.verb("link", {
        "source_id": src["id"], "target_id": tgt["id"],
        "relation": "extends", "weight": 0.3, "namespace": temp_namespace,
    })
    edge_id = edge["id"]

    khive_session.verb("update", {"id": edge_id, "kind": "edge", "namespace": temp_namespace,
                                   "weight": 0.9})

    # Per P-H2 (ADR-045): get returns flat object — no {data: ...} wrapper.
    fetched = khive_session.verb("get", {"id": edge_id, "namespace": temp_namespace})
    updated_weight = fetched.get("weight")
    assert updated_weight is not None, f"weight not in edge response: {fetched}"
    assert abs(updated_weight - 0.9) < 0.01, (
        f"edge weight not updated: {updated_weight!r}"
    )


@pytest.mark.adr_014
@pytest.mark.slow
def test_update_note_content_and_salience(
    khive_session: KhiveMcpSession,
    temp_namespace: str,
    sample_note,
) -> None:
    """update(note) patches content and salience; get reflects changes.

    ADR: ADR-014
    section: Patch-style updates
    """
    note = khive_session.verb("create", sample_note(
        note_kind="observation",
        content="original content",
        salience=0.3,
    ))
    note_id = note["id"]

    khive_session.verb("update", {"id": note_id, "kind": "note", "namespace": temp_namespace,
                                   "content": "updated content", "salience": 0.8})

    # Per P-H2 (ADR-045): get returns flat object — no {data: ...} wrapper.
    fetched = khive_session.verb("get", {"id": note_id, "namespace": temp_namespace})
    assert fetched.get("content") == "updated content", f"content not updated: {fetched}"
    assert abs(fetched.get("salience", 0) - 0.8) < 0.01, f"salience not updated: {fetched}"


@pytest.mark.adr_014
@pytest.mark.slow
def test_delete_entity_soft_and_hard(
    khive_session: KhiveMcpSession,
    temp_namespace: str,
    sample_entity,
) -> None:
    """Soft delete returns deleted=True; hard delete returns deleted=True.

    ADR: ADR-014
    section: Soft vs hard delete

    Ports delete assertions from smoke_test.py.
    """
    # Soft delete
    e_soft = khive_session.verb("create", sample_entity(entity_kind="concept", name="SoftDel"))
    del_result = khive_session.verb("delete", {"id": e_soft["id"], "kind": "entity",
                                                "namespace": temp_namespace})
    assert del_result.get("deleted") is True, f"soft delete should return deleted=True: {del_result}"

    # Hard delete
    e_hard = khive_session.verb("create", sample_entity(entity_kind="concept", name="HardDel"))
    del_result_h = khive_session.verb("delete", {
        "id": e_hard["id"], "kind": "entity", "hard": True, "namespace": temp_namespace,
    })
    assert del_result_h.get("deleted") is True, (
        f"hard delete should return deleted=True: {del_result_h}"
    )

    # Hard-deleted entity must not be gettable
    envelope = khive_session.request_batch([{"tool": "get", "args": {"id": e_hard["id"],
                                                                       "namespace": temp_namespace}}])
    first = envelope["results"][0]
    assert not first.get("ok", False), "Hard-deleted entity must not be gettable"
    assert "not found" in first.get("error", "").lower(), (
        f"Expected not-found error after hard delete: {first.get('error')!r}"
    )


@pytest.mark.adr_014
@pytest.mark.slow
def test_merge_entity_rewires_edges_unions_tags_drops_self_loops(
    khive_session: KhiveMcpSession,
    temp_namespace: str,
    sample_entity,
) -> None:
    """merge(into, from) rewires edges, unions tags, from becomes inaccessible, self-loop dropped.

    ADR: ADR-014
    section: merge_entity semantics

    Ports test_merge_semantics from contract_test.py.
    """
    kept = khive_session.verb("create", sample_entity(
        entity_kind="concept", name="KeptEntity", tags=["alpha", "beta"]
    ))
    gone = khive_session.verb("create", sample_entity(
        entity_kind="concept", name="GoneEntity", tags=["beta", "gamma"]
    ))
    third = khive_session.verb("create", sample_entity(
        entity_kind="concept", name="ThirdEntity"
    ))

    # third → gone (inbound to gone)
    e_inbound = khive_session.verb("link", {
        "source_id": third["id"],
        "target_id": gone["id"],
        "relation": "enables",
        "weight": 0.7,
        "namespace": temp_namespace,
    })
    # gone → kept (becomes self-loop after merge, must be dropped)
    e_self_loop = khive_session.verb("link", {
        "source_id": gone["id"],
        "target_id": kept["id"],
        "relation": "extends",
        "weight": 0.5,
        "namespace": temp_namespace,
    })
    e_inbound_id = e_inbound["id"]
    e_self_loop_id = e_self_loop["id"]

    # Execute merge
    summary = khive_session.verb("merge", {
        "into_id": kept["id"],
        "from_id": gone["id"],
        "strategy": "prefer_into",
        "namespace": temp_namespace,
    })
    assert summary.get("kept_id") == kept["id"], (
        f"kept_id mismatch: expected {kept['id']}, got {summary.get('kept_id')}"
    )
    assert summary.get("removed_id") == gone["id"], (
        f"removed_id mismatch: expected {gone['id']}, got {summary.get('removed_id')}"
    )

    # from_id must not be gettable
    envelope_gone = khive_session.request_batch([{"tool": "get", "args": {"id": gone["id"],
                                                                            "namespace": temp_namespace}}])
    first_gone = envelope_gone["results"][0]
    assert not first_gone.get("ok", False), "Merged-away entity must not be gettable"
    assert "not found" in first_gone.get("error", "").lower(), (
        f"Expected not-found for merged-away entity: {first_gone.get('error')!r}"
    )

    # Per P-H2 (ADR-045): get returns flat object — no {data: ...} wrapper.
    # Inbound edge must be rewired to kept_id
    rewired = khive_session.verb("get", {"id": e_inbound_id, "namespace": temp_namespace})
    assert rewired.get("kind") == "edge", f"rewired edge not found: {rewired}"
    assert rewired.get("target_id") == kept["id"], (
        f"Inbound edge target should be rewired to kept_id={kept['id']}, "
        f"got target_id={rewired.get('target_id')}"
    )
    assert rewired.get("source_id") == third["id"], (
        f"Source should still be third={third['id']}, got {rewired.get('source_id')}"
    )

    # Tags must be unioned on kept entity
    kept_after = khive_session.verb("get", {"id": kept["id"], "namespace": temp_namespace})
    assert kept_after.get("kind") == "concept"
    tags_after = set(kept_after.get("tags", []))
    assert "alpha" in tags_after, f"Tag 'alpha' missing after merge: {tags_after}"
    assert "beta" in tags_after, f"Tag 'beta' missing after merge: {tags_after}"
    assert "gamma" in tags_after, f"Tag 'gamma' missing after merge: {tags_after}"

    # Self-loop edge (gone→kept, now kept→kept) must be dropped
    envelope_loop = khive_session.request_batch([
        {"tool": "get", "args": {"id": e_self_loop_id, "namespace": temp_namespace}}
    ])
    first_loop = envelope_loop["results"][0]
    assert not first_loop.get("ok", False), "Self-loop edge must be deleted after merge"
    assert "not found" in first_loop.get("error", "").lower(), (
        f"Self-loop edge should be not-found: {first_loop.get('error')!r}"
    )

    # No edges referencing the removed entity
    gone_out = khive_session.verb("list", {"kind": "edge", "source_id": gone["id"],
                                            "namespace": temp_namespace})
    assert gone_out == [], (
        f"No edges with source_id=gone should remain: {gone_out}"
    )
    gone_in = khive_session.verb("list", {"kind": "edge", "target_id": gone["id"],
                                           "namespace": temp_namespace})
    assert gone_in == [], (
        f"No edges with target_id=gone should remain: {gone_in}"
    )
