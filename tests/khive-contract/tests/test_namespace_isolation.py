"""Namespace isolation contract tests.

ADR: ADR-003
section: Namespace isolation; Cross-namespace access; Write path isolation
"""

from __future__ import annotations

import pytest

from khive_contract.client import KhiveMcpSession

VERBS_UNDER_TEST = {"create", "get", "list", "search", "link"}


@pytest.mark.adr_003
@pytest.mark.slow
def test_read_isolation_between_namespaces(
    khive_session: KhiveMcpSession,
    temp_namespace: str,
    sample_entity,
) -> None:
    """Entity created in alpha namespace is invisible via get/list/search from beta namespace.

    ADR: ADR-003
    section: Namespace isolation

    Ports test_namespace_isolation from contract_test.py.
    Entity in alpha: get(beta) → not found; list(beta) → absent; search(beta) → absent.
    Entity in alpha: get(alpha) → succeeds.
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

    # get from beta must fail
    envelope_get = khive_session.request_batch([{
        "tool": "get",
        "args": {"id": full_id, "namespace": ns_beta},
    }])
    first_get = envelope_get["results"][0]
    assert not first_get.get("ok", False), (
        "get from beta namespace must not find alpha entity"
    )
    assert "not found" in first_get.get("error", "").lower(), (
        f"Expected not-found error from beta get, got: {first_get.get('error')!r}"
    )

    # list from beta must not include the alpha entity
    entities_beta = khive_session.verb("list", {
        "kind": "entity",
        "entity_kind": "concept",
        "namespace": ns_beta,
    })
    ids_beta = [e["id"] for e in entities_beta]
    assert full_id not in ids_beta, (
        f"AlphaEntity appeared in beta namespace list: {ids_beta}"
    )

    # search from beta must not find the alpha entity
    hits_beta = khive_session.verb("search", {
        "kind": "entity",
        "query": "AlphaEntity",
        "namespace": ns_beta,
    })
    hit_ids_beta = [h.get("id", h.get("entity_id", "")) for h in hits_beta]
    assert full_id not in hit_ids_beta, (
        f"AlphaEntity appeared in beta namespace search: {hit_ids_beta}"
    )

    # Per P-H2 (ADR-045): get returns flat object with granular kind — no {data: ...} wrapper.
    # get from alpha must succeed
    fetched = khive_session.verb("get", {"id": full_id, "namespace": ns_alpha})
    assert fetched.get("kind") == "concept", (
        f"get from alpha must return kind=concept, got: {fetched}"
    )
    assert fetched.get("name") == "AlphaEntity", (
        f"Entity name mismatch: {fetched}"
    )

    # 8-char prefix from beta must not resolve to the alpha entity
    prefix8 = full_id[:8]
    envelope_prefix = khive_session.request_batch([{
        "tool": "get",
        "args": {"id": prefix8, "namespace": ns_beta},
    }])
    first_prefix = envelope_prefix["results"][0]
    assert not first_prefix.get("ok", False), (
        "8-char prefix should not resolve alpha entity from beta namespace"
    )
    err_prefix = first_prefix.get("error", "").lower()
    assert "not found" in err_prefix or "no record" in err_prefix, (
        f"Expected not-found prefix error from beta, got: {first_prefix.get('error')!r}"
    )


@pytest.mark.adr_003
@pytest.mark.slow
def test_write_isolation_cross_namespace_link_fails(
    khive_session: KhiveMcpSession,
    temp_namespace: str,
    sample_entity,
) -> None:
    """link from beta using alpha entity UUID must fail — write path enforces namespace isolation.

    ADR: ADR-003
    section: Write path isolation; Cross-namespace access

    Ports the link-write portion of test_namespace_isolation from contract_test.py.
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
