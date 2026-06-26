"""Namespace contract tests (ADR-007 Rev 6).

ADR: ADR-007
section: By-ID namespace-agnostic access (Rule 2); Multi-record namespace scoping (Rule 3);
         Link endpoint resolution (namespace-scoped write path)
"""

from __future__ import annotations

import pytest

from khive_contract.client import KhiveMcpSession

VERBS_UNDER_TEST = {"create", "get", "list", "search", "link"}


@pytest.mark.adr_007
@pytest.mark.slow
def test_read_isolation_between_namespaces(
    khive_session: KhiveMcpSession,
    temp_namespace: str,
    sample_entity,
) -> None:
    """By-ID ops are namespace-agnostic; multi-record ops remain namespace-scoped.

    ADR: ADR-007
    section: By-ID namespace-agnostic access (Rule 2); Namespace scoping (Rule 3)

    ADR-007 Rev 6 Rule 2 (SHIPPED, PR-A1 commit 2607e263): get/update/delete resolve by UUID
    with WHERE id = ? only — no namespace equality check at any layer (store, runtime, handler).

    Contract verified here:
    - get(id, namespace=beta) where entity lives in alpha: SUCCEEDS — full-UUID by-ID is
      namespace-agnostic (ADR-007 Rev 6 Rule 2, SHIPPED PR-A1 commit 2607e263).
    - 8-char prefix get from beta: FAILS with not-found — prefix expansion is a namespace-scoped
      lookup, distinct from the full-UUID by-ID path; ADR-007 Rule 2 covers "by UUID" only.
    - list(namespace=beta): alpha entity ABSENT — multi-record namespace scoping survives.
    - search(namespace=beta): alpha entity ABSENT — multi-record namespace scoping survives.
    - get(id, namespace=alpha): SUCCEEDS — control path confirming same-namespace access.
    """
    ns_alpha = f"{temp_namespace}_alpha"
    ns_beta = f"{temp_namespace}_beta"

    # Create entity in alpha
    entity = khive_session.verb("create", {
        "kind": "entity",
        "entity_kind": "concept",
        "name": "AlphaEntity",
        "description": "Only visible in alpha",
        "namespace": ns_alpha,
    })
    full_id = entity["id"]

    # By-ID get from beta: must SUCCEED (ADR-007 Rev 6 Rule 2 — namespace-agnostic)
    envelope_get = khive_session.request_batch([{
        "tool": "get",
        "args": {"id": full_id, "namespace": ns_beta},
    }])
    first_get = envelope_get["results"][0]
    assert first_get.get("ok", False), (
        "get by UUID from beta namespace must succeed: by-ID ops are namespace-agnostic "
        "(ADR-007 Rev 6 Rule 2, SHIPPED PR-A1 commit 2607e263)"
    )
    cross_ns_result = first_get.get("result", {})
    assert cross_ns_result.get("kind") == "concept", (
        f"get from beta must return kind=concept, got: {first_get}"
    )
    assert cross_ns_result.get("name") == "AlphaEntity", (
        f"get from beta must return AlphaEntity, got: {first_get}"
    )

    # list from beta must not include the alpha entity (multi-record namespace scoping survives)
    entities_beta = khive_session.verb("list", {
        "kind": "entity",
        "entity_kind": "concept",
        "namespace": ns_beta,
    })
    ids_beta = [e["id"] for e in entities_beta]
    assert full_id not in ids_beta, (
        f"AlphaEntity must not appear in beta namespace list (multi-record scoping): {ids_beta}"
    )

    # search from beta must not find the alpha entity (multi-record namespace scoping survives)
    hits_beta = khive_session.verb("search", {
        "kind": "entity",
        "query": "AlphaEntity",
        "namespace": ns_beta,
    })
    hit_ids_beta = [h.get("id", h.get("entity_id", "")) for h in hits_beta]
    assert full_id not in hit_ids_beta, (
        f"AlphaEntity must not appear in beta namespace search (multi-record scoping): "
        f"{hit_ids_beta}"
    )

    # Per P-H2 (ADR-045): get returns flat object with granular kind — no {data: ...} wrapper.
    # get from alpha: control path — same namespace as creator, must succeed.
    fetched = khive_session.verb("get", {"id": full_id, "namespace": ns_alpha})
    assert fetched.get("kind") == "concept", (
        f"get from alpha must return kind=concept, got: {fetched}"
    )
    assert fetched.get("name") == "AlphaEntity", (
        f"Entity name mismatch: {fetched}"
    )

    # 8-char prefix get from beta: prefix expansion is namespace-scoped (distinct from full-UUID
    # by-ID path). ADR-007 Rule 2 covers "by UUID" only; prefix resolution expands via a
    # namespace-scoped query and correctly returns not-found for a cross-namespace prefix.
    prefix8 = full_id[:8]
    envelope_prefix = khive_session.request_batch([{
        "tool": "get",
        "args": {"id": prefix8, "namespace": ns_beta},
    }])
    first_prefix = envelope_prefix["results"][0]
    assert not first_prefix.get("ok", False), (
        "8-char prefix get from beta namespace must not resolve alpha entity: "
        "prefix expansion is namespace-scoped (not the full-UUID by-ID path of ADR-007 Rule 2)"
    )
    err_prefix = first_prefix.get("error", "").lower()
    assert "not found" in err_prefix or "no record" in err_prefix, (
        f"Expected not-found prefix error from beta, got: {first_prefix.get('error')!r}"
    )


@pytest.mark.adr_007
@pytest.mark.slow
def test_write_isolation_cross_namespace_link_fails(
    khive_session: KhiveMcpSession,
    temp_namespace: str,
    sample_entity,
) -> None:
    """link endpoint resolution is namespace-scoped — cross-namespace links are rejected.

    ADR: ADR-007
    section: Link endpoint resolution; Multi-record namespace scoping (Rule 3)

    link is not a by-ID op: endpoint resolution looks up source and target entities within the
    caller-supplied namespace. A link that references an entity from a different namespace fails
    with not-found because the endpoint is invisible to the scoped lookup — consistent with
    Rule 3 (multi-record namespace scoping) even though the UUID itself is globally unique.

    This test is distinct from the by-ID get contract (Rule 2): link does NOT use WHERE id = ?
    unconditionally; it uses a namespace-scoped entity fetch for endpoint validation.
    """
    ns_alpha = f"{temp_namespace}_alpha"
    ns_beta = f"{temp_namespace}_beta"

    # Create alpha entity
    alpha = khive_session.verb("create", {
        "kind": "entity",
        "entity_kind": "concept",
        "name": "AlphaNode",
        "namespace": ns_alpha,
    })
    alpha_id = alpha["id"]

    # Create beta entity
    beta = khive_session.verb("create", {
        "kind": "entity",
        "entity_kind": "concept",
        "name": "BetaNode",
        "namespace": ns_beta,
    })
    beta_id = beta["id"]

    # link from beta using alpha as target must fail
    envelope_fwd = khive_session.request_batch([{
        "tool": "link",
        "args": {
            "source_id": beta_id,
            "target_id": alpha_id,
            "relation": "depends_on",
            "namespace": ns_beta,
        },
    }])
    first_fwd = envelope_fwd["results"][0]
    assert not first_fwd.get("ok", False), (
        "Cross-namespace link (beta→alpha, beta caller) must fail"
    )
    err_fwd = first_fwd.get("error", "").lower()
    assert "not found" in err_fwd, (
        f"Cross-namespace link must fail with not-found, got: {first_fwd.get('error')!r}"
    )

    # link with alpha as source from beta namespace must also fail
    envelope_rev = khive_session.request_batch([{
        "tool": "link",
        "args": {
            "source_id": alpha_id,
            "target_id": beta_id,
            "relation": "extends",
            "namespace": ns_beta,
        },
    }])
    first_rev = envelope_rev["results"][0]
    assert not first_rev.get("ok", False), (
        "Cross-namespace link (alpha→beta, beta caller) must fail"
    )
    err_rev = first_rev.get("error", "").lower()
    assert "not found" in err_rev, (
        f"Cross-namespace reverse link must fail with not-found, got: {first_rev.get('error')!r}"
    )
