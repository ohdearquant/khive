"""Contract tests: khive-pack-formal EntityOfType edge endpoint rules.

ADR: ADR-017
section: EntityOfType edge endpoint rules; formal-math ontology pack; depends_on
Issue: #260 (formal-pack + create_many coverage), surface: PR #231

Source of truth for all asserted rules:
  crates/khive-pack-formal/src/vocab.rs — FORMAL_EDGE_RULES (21 entries)

The formal-math pack extends the closed edge ontology with 21 EntityOfType
rules for six concept subtypes registered in BUILTIN_DEFS:
  theorem, definition, structure, instance, axiom, goal
  (crates/khive-pack-kg/src/entity_type_registry.rs, lines ~90-125)

All six subtypes use kind="concept" at the endpoint level (vocab.rs, macro
formal_ep! line 14-21).  Relations covered by the pack:
  depends_on  — 14 rules (vocab.rs lines 29-98)
  instance_of — 1 rule  (vocab.rs lines 100-104)
  extends     — 2 rules (vocab.rs lines 106-116)
  variant_of  — 4 rules (vocab.rs lines 118-136)

Why depends_on is the canonical test surface:
  The base allowlist (crates/khive-runtime/src/operations.rs, function
  base_entity_rule_allows, lines 266-335) has NO concept->concept entry for
  depends_on.  Only the formal pack grants these permissions.  In contrast,
  extends/variant_of/instance_of already have base c->c or *->c entries that
  permit concept->concept regardless of the pack.

Endpoint validation flow (operations.rs lines 1163-1229):
  1. pack_rule_allows() — if any pack rule matches, return Ok immediately.
  2. base_entity_rule_allows() — fallback for kinds without a pack rule.
  For depends_on + concept->concept: step 1 decides the outcome entirely.

Positive tests: formal pack loaded; legal depends_on type pairs succeed.
Negative tests (illegal type pair): formal pack loaded; rejected.
Baseline test: no formal pack; any concept depends_on is rejected.
"""

from __future__ import annotations

import pytest

from khive_contract.client import KhiveOperationError, KhiveMcpSession

VERBS_UNDER_TEST = {"create", "link"}


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def _make_formal_concept(
    session: KhiveMcpSession,
    namespace: str,
    entity_type: str,
    suffix: str,
) -> str:
    """Create a concept entity with the given formal entity_type; return its id."""
    result = session.verb("create", {
        "kind": "concept",
        "name": f"fp_{entity_type}_{suffix}",
        "entity_type": entity_type,
        "namespace": namespace,
    })
    return result["id"]


# ---------------------------------------------------------------------------
# Positive: legal depends_on pairs (formal pack required)
# ---------------------------------------------------------------------------


@pytest.mark.formal_pack
@pytest.mark.slow
def test_formal_depends_on_theorem_to_definition(
    khive_formal_session: KhiveMcpSession,
    temp_namespace: str,
) -> None:
    """depends_on theorem->definition is legal under the formal pack.

    Source: vocab.rs lines 35-39 — FORMAL_EDGE_RULES entry
      EdgeEndpointRule { relation: DependsOn, source: formal_ep!("theorem"),
                         target: formal_ep!("definition") }

    Without the formal pack this link would be rejected because the base
    allowlist has no concept->concept entry for depends_on.
    """
    ns = temp_namespace
    src_id = _make_formal_concept(khive_formal_session, ns, "theorem", "thm_src")
    tgt_id = _make_formal_concept(khive_formal_session, ns, "definition", "def_tgt")

    result = khive_formal_session.verb("link", {
        "source_id": src_id,
        "target_id": tgt_id,
        "relation": "depends_on",
        "namespace": ns,
    })

    assert result is not None, (
        "link(theorem->definition, depends_on) must succeed with formal pack loaded; "
        f"got None"
    )
    assert result.get("source_id") == src_id or result.get("id") is not None, (
        f"link result must reference the created edge; got {result}"
    )


@pytest.mark.formal_pack
@pytest.mark.slow
def test_formal_depends_on_goal_to_axiom(
    khive_formal_session: KhiveMcpSession,
    temp_namespace: str,
) -> None:
    """depends_on goal->axiom is legal under the formal pack.

    Source: vocab.rs lines 89-98 — FORMAL_EDGE_RULES entry
      EdgeEndpointRule { relation: DependsOn, source: formal_ep!("goal"),
                         target: formal_ep!("axiom") }
    """
    ns = temp_namespace
    src_id = _make_formal_concept(khive_formal_session, ns, "goal", "goal_src")
    tgt_id = _make_formal_concept(khive_formal_session, ns, "axiom", "axm_tgt")

    result = khive_formal_session.verb("link", {
        "source_id": src_id,
        "target_id": tgt_id,
        "relation": "depends_on",
        "namespace": ns,
    })

    assert result is not None, (
        "link(goal->axiom, depends_on) must succeed with formal pack loaded"
    )


@pytest.mark.formal_pack
@pytest.mark.slow
def test_formal_depends_on_instance_to_structure(
    khive_formal_session: KhiveMcpSession,
    temp_namespace: str,
) -> None:
    """depends_on instance->structure is legal under the formal pack.

    Source: vocab.rs lines 69-72 — FORMAL_EDGE_RULES entry
      EdgeEndpointRule { relation: DependsOn, source: formal_ep!("instance"),
                         target: formal_ep!("structure") }
    """
    ns = temp_namespace
    src_id = _make_formal_concept(khive_formal_session, ns, "instance", "inst_src")
    tgt_id = _make_formal_concept(khive_formal_session, ns, "structure", "str_tgt")

    result = khive_formal_session.verb("link", {
        "source_id": src_id,
        "target_id": tgt_id,
        "relation": "depends_on",
        "namespace": ns,
    })

    assert result is not None, (
        "link(instance->structure, depends_on) must succeed with formal pack loaded"
    )


@pytest.mark.formal_pack
@pytest.mark.slow
def test_formal_depends_on_definition_to_theorem(
    khive_formal_session: KhiveMcpSession,
    temp_namespace: str,
) -> None:
    """depends_on definition->theorem is legal under the formal pack.

    Source: vocab.rs lines 58-62 — FORMAL_EDGE_RULES entry
      EdgeEndpointRule { relation: DependsOn, source: formal_ep!("definition"),
                         target: formal_ep!("theorem") }

    Confirms bidirectional reachability in the theorem<->definition pair.
    """
    ns = temp_namespace
    src_id = _make_formal_concept(khive_formal_session, ns, "definition", "def_src")
    tgt_id = _make_formal_concept(khive_formal_session, ns, "theorem", "thm_tgt")

    result = khive_formal_session.verb("link", {
        "source_id": src_id,
        "target_id": tgt_id,
        "relation": "depends_on",
        "namespace": ns,
    })

    assert result is not None, (
        "link(definition->theorem, depends_on) must succeed with formal pack loaded"
    )


# ---------------------------------------------------------------------------
# Negative: illegal depends_on type pairs (formal pack loaded; still rejected)
# ---------------------------------------------------------------------------


@pytest.mark.formal_pack
@pytest.mark.slow
def test_formal_depends_on_axiom_as_source_rejected(
    khive_formal_session: KhiveMcpSession,
    temp_namespace: str,
) -> None:
    """depends_on axiom->theorem is rejected even with the formal pack loaded.

    axiom does NOT appear as a depends_on source in FORMAL_EDGE_RULES.
    vocab.rs search: no entry with source=formal_ep!("axiom") under DependsOn.
    The base allowlist also has no concept->concept depends_on entry
    (operations.rs base_entity_rule_allows lines 298-304 — project/service/artifact only).
    Both checks fail, so the link must be rejected.
    """
    ns = temp_namespace
    src_id = _make_formal_concept(khive_formal_session, ns, "axiom", "axm_src")
    tgt_id = _make_formal_concept(khive_formal_session, ns, "theorem", "thm_tgt")

    with pytest.raises(KhiveOperationError) as exc_info:
        khive_formal_session.verb("link", {
            "source_id": src_id,
            "target_id": tgt_id,
            "relation": "depends_on",
            "namespace": ns,
        })

    error_msg = exc_info.value.message.lower()
    assert "valid relations" in error_msg or "invalid relation" in error_msg or "allowlist" in error_msg, (
        "error must indicate a relation/endpoint rule violation "
        "(expected 'Valid relations:' or 'Invalid relation' or 'allowlist'); "
        f"got: {exc_info.value.message!r}"
    )


@pytest.mark.formal_pack
@pytest.mark.slow
def test_formal_depends_on_structure_as_source_rejected(
    khive_formal_session: KhiveMcpSession,
    temp_namespace: str,
) -> None:
    """depends_on structure->definition is rejected even with the formal pack loaded.

    structure does NOT appear as a depends_on source in FORMAL_EDGE_RULES.
    vocab.rs search: no entry with source=formal_ep!("structure") under DependsOn.
    """
    ns = temp_namespace
    src_id = _make_formal_concept(khive_formal_session, ns, "structure", "str_src")
    tgt_id = _make_formal_concept(khive_formal_session, ns, "definition", "def_tgt")

    with pytest.raises(KhiveOperationError) as exc_info:
        khive_formal_session.verb("link", {
            "source_id": src_id,
            "target_id": tgt_id,
            "relation": "depends_on",
            "namespace": ns,
        })

    error_msg = exc_info.value.message.lower()
    assert "valid relations" in error_msg or "invalid relation" in error_msg or "allowlist" in error_msg, (
        "error must indicate a relation/endpoint rule violation "
        "(expected 'Valid relations:' or 'Invalid relation' or 'allowlist'); "
        f"got: {exc_info.value.message!r}"
    )


@pytest.mark.formal_pack
@pytest.mark.slow
def test_formal_depends_on_theorem_to_instance_rejected(
    khive_formal_session: KhiveMcpSession,
    temp_namespace: str,
) -> None:
    """depends_on theorem->instance is rejected: instance is not a valid depends_on target for theorem.

    vocab.rs lines 29-48 list the four legal theorem depends_on targets:
      formal_ep!("theorem"), formal_ep!("definition"),
      formal_ep!("structure"), formal_ep!("axiom")
    formal_ep!("instance") is absent from that list.
    """
    ns = temp_namespace
    src_id = _make_formal_concept(khive_formal_session, ns, "theorem", "thm_src")
    tgt_id = _make_formal_concept(khive_formal_session, ns, "instance", "inst_tgt")

    with pytest.raises(KhiveOperationError) as exc_info:
        khive_formal_session.verb("link", {
            "source_id": src_id,
            "target_id": tgt_id,
            "relation": "depends_on",
            "namespace": ns,
        })

    error_msg = exc_info.value.message.lower()
    assert "valid relations" in error_msg or "invalid relation" in error_msg or "allowlist" in error_msg, (
        "error must indicate a relation/endpoint rule violation "
        "(expected 'Valid relations:' or 'Invalid relation' or 'allowlist'); "
        f"got: {exc_info.value.message!r}"
    )


# ---------------------------------------------------------------------------
# Baseline: formal pack is required — no pack means no concept depends_on
# ---------------------------------------------------------------------------


@pytest.mark.formal_pack
@pytest.mark.slow
def test_no_formal_pack_concept_depends_on_rejected(
    khive_session: KhiveMcpSession,
    temp_namespace: str,
) -> None:
    """Without the formal pack, all concept->concept depends_on links are rejected.

    This test uses khive_session (packs=("kg",) only).  The base allowlist has
    no concept->concept entry for depends_on (operations.rs lines 298-304).
    Without formal pack rules, pack_rule_allows returns false, and the link falls
    through to base_entity_rule_allows which also returns false.

    This proves the formal pack is not vacuous: it is the sole source of
    permission for theorem->definition depends_on.
    """
    ns = temp_namespace

    # Create concept entities using the KG-only session (entity_type still accepted
    # because it is registered in BUILTIN_DEFS, not in formal pack).
    src_result = khive_session.verb("create", {
        "kind": "concept",
        "name": f"fp_baseline_theorem_{ns[-6:]}",
        "entity_type": "theorem",
        "namespace": ns,
    })
    tgt_result = khive_session.verb("create", {
        "kind": "concept",
        "name": f"fp_baseline_definition_{ns[-6:]}",
        "entity_type": "definition",
        "namespace": ns,
    })

    with pytest.raises(KhiveOperationError) as exc_info:
        khive_session.verb("link", {
            "source_id": src_result["id"],
            "target_id": tgt_result["id"],
            "relation": "depends_on",
            "namespace": ns,
        })

    error_msg = exc_info.value.message.lower()
    assert "valid relations" in error_msg or "invalid relation" in error_msg or "allowlist" in error_msg, (
        "without formal pack, concept depends_on must fail with an endpoint rule error; "
        f"got: {exc_info.value.message!r}"
    )
