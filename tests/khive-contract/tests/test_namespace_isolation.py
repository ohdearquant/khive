"""Namespace contract tests (ADR-007 Rev 6).

ADR: ADR-007
section: By-ID namespace-agnostic access (Rule 2); Multi-record namespace scoping (Rule 3);
         Link endpoint resolution (by-ID, namespace-agnostic)
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
    - 8-char prefix get from beta: SUCCEEDS — prefix expansion for by-ID ops is unfiltered
      (issue #391 fix): the prefix path now matches the full-UUID by-ID contract instead of
      silently narrowing it to the caller namespace. Ambiguity across namespaces errors;
      a unique prefix resolves regardless of namespace.
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

    # 8-char prefix get from beta: prefix expansion for by-ID CRUD is unfiltered (issue #391
    # fix). The prior contract asserted not-found here, but that namespace-scoped prefix filter
    # silently narrowed by-ID lookups to a boundary the full-UUID path never had (ADR-007 Rev 6
    # Rule 2). Prefix resolution now matches the full-UUID by-ID contract: a unique prefix
    # resolves regardless of namespace; an ambiguous prefix errors.
    prefix8 = full_id[:8]
    envelope_prefix = khive_session.request_batch([{
        "tool": "get",
        "args": {"id": prefix8, "namespace": ns_beta},
    }])
    first_prefix = envelope_prefix["results"][0]
    assert first_prefix.get("ok", False), (
        "8-char prefix get from beta namespace must resolve the alpha entity: by-ID prefix "
        "expansion is unfiltered, matching the full-UUID by-ID contract "
        f"(ADR-007 Rev 6 Rule 2, issue #391), got: {first_prefix}"
    )
    prefix_result = first_prefix.get("result", {})
    assert prefix_result.get("name") == "AlphaEntity", (
        f"prefix get from beta must return AlphaEntity, got: {first_prefix}"
    )


@pytest.mark.adr_007
@pytest.mark.slow
def test_write_cross_namespace_link_succeeds(
    khive_session: KhiveMcpSession,
    temp_namespace: str,
    sample_entity,
) -> None:
    """link endpoint resolution is by-ID and namespace-agnostic — cross-namespace links succeed.

    ADR: ADR-007
    section: By-ID namespace-agnostic access (Rule 2); Link endpoint resolution

    link consumes caller-supplied endpoint IDs, so endpoint validation is a by-ID operation
    under ADR-007 Rev 6 Rule 2: no namespace equality check at any layer. A link whose source
    and target live in different namespaces succeeds; the caller namespace stamps the edge
    (attribution), it does not gate endpoint visibility.

    The prior contract asserted not-found here. That namespace-scoped endpoint fetch was the
    v1 fail-closed bug pattern (the same class removed by PR-A1 for get) and was corrected
    for link in #631.
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

    # link from beta using alpha as target must succeed (by-ID endpoints, Rule 2)
    envelope_fwd = khive_session.request_batch([{
        "tool": "link",
        "args": {
            "source_id": beta_id,
            "target_id": alpha_id,
            "relation": "extends",
            "namespace": ns_beta,
        },
    }])
    first_fwd = envelope_fwd["results"][0]
    assert first_fwd.get("ok", False), (
        "Cross-namespace link (beta→alpha, beta caller) must succeed: link endpoint "
        f"resolution is by-ID and namespace-agnostic, got: {first_fwd.get('error')!r}"
    )

    # link with alpha as source from beta namespace must also succeed
    envelope_rev = khive_session.request_batch([{
        "tool": "link",
        "args": {
            "source_id": alpha_id,
            "target_id": beta_id,
            "relation": "supersedes",
            "namespace": ns_beta,
        },
    }])
    first_rev = envelope_rev["results"][0]
    assert first_rev.get("ok", False), (
        "Cross-namespace link (alpha→beta, beta caller) must succeed: link endpoint "
        f"resolution is by-ID and namespace-agnostic, got: {first_rev.get('error')!r}"
    )
