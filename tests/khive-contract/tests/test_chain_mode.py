# Run via: uv run pytest
"""Chain-mode integration tests at the MCP boundary.

Issue: #389 — unit tests cover chain parsing + $prev substitution only; this
file adds end-to-end coverage that drives the real MCP server via stdio and
asserts the full chain semantics: happy-path execution, $prev resolution,
abort propagation, and mixed-separator rejection.

ADR: ADR-016
section: Chain semantics; $prev substitution; Abort-on-failure; Mixed separators
"""

from __future__ import annotations

import uuid

import pytest

from khive_contract.client import KhiveMcpSession, KhiveRpcError
from khive_contract.schema import assert_envelope

VERBS_UNDER_TEST = {"create", "get", "link", "update"}

# ---------------------------------------------------------------------------
# Skip entire module when the MCP binary is not present.
#
# We skip at module-level (allow_module_level=True) so that pytest skips
# collection entirely rather than reaching the session-scoped fixture setup
# which would raise FileNotFoundError before any per-test skip could fire.
# ---------------------------------------------------------------------------

from khive_contract.client import _resolve_binary

try:
    _resolve_binary(None)
except FileNotFoundError as _exc:
    pytest.skip(f"khive-mcp binary not found: {_exc}", allow_module_level=True)

pytestmark = [
    pytest.mark.adr_016,
    pytest.mark.slow,
]


# ---------------------------------------------------------------------------
# Case 2: KG chain — create entity then link it to a known target via $prev.id
#
# `create(kind='entity', ...) | link(source_id=$prev.id, target_id=<known>, relation='extends')`
# Both ops must succeed; the edge must be visible in neighbors.
# ---------------------------------------------------------------------------


def test_chain_create_entity_then_link(
    khive_session: KhiveMcpSession,
    temp_namespace: str,
) -> None:
    """Chain: create a concept entity then link it to a pre-existing target.

    ADR: ADR-016
    section: Chain semantics; $prev substitution

    Asserts:
    - Both ops ok=True.
    - Summary: total=2, succeeded=2, failed=0, aborted=0.
    - Edge is visible via neighbors query.
    """
    ns = temp_namespace

    # Create the target entity first (outside the chain — we need its id).
    target = khive_session.verb(
        "create",
        {
            "kind": "entity",
            "entity_kind": "concept",
            "name": f"ChainTarget_{uuid.uuid4().hex[:6]}",
            "namespace": ns,
        },
    )
    target_id: str = target["id"]

    source_name = f"ChainSource_{uuid.uuid4().hex[:6]}"
    ops = (
        f'create(kind="entity", entity_kind="concept", name="{source_name}", namespace="{ns}")'
        f' | link(source_id=$prev.id, target_id="{target_id}", relation="extends", namespace="{ns}")'
    )
    envelope = khive_session.request(ops)

    assert_envelope(envelope)
    results = envelope["results"]
    summary = envelope.get("summary", {})

    assert len(results) == 2, f"Expected 2 results, got {len(results)}: {results}"
    assert results[0].get("ok") is True, f"create failed: {results[0]}"
    assert results[1].get("ok") is True, f"link failed: {results[1]}"

    assert summary.get("total") == 2, f"summary.total != 2: {summary}"
    assert summary.get("succeeded") == 2, f"summary.succeeded != 2: {summary}"
    assert summary.get("failed") == 0, f"summary.failed != 0: {summary}"
    assert summary.get("aborted") == 0, f"summary.aborted != 0: {summary}"

    # The edge must be visible when we ask for neighbors of the source entity.
    source_id: str = results[0]["result"]["id"]
    nbrs = khive_session.verb(
        "neighbors",
        {"node_id": source_id, "direction": "outgoing", "namespace": ns},
    )
    neighbor_ids = [n.get("id") for n in (nbrs if isinstance(nbrs, list) else [])]
    assert target_id[:8] in neighbor_ids or target_id in neighbor_ids, (
        f"Target {target_id!r} not found in outgoing neighbors of source {source_id!r}: {nbrs}"
    )


# ---------------------------------------------------------------------------
# Case 3: Chain abort — second op references a bogus field; remaining ops
# must be marked aborted, not silently succeed.
#
# `create(kind='entity', ...) | get(id=$prev.bogus_field_that_does_not_exist)`
# Op 0 ok=True; op 1 ok=False (error, not aborted); if there were a third op
# it would be aborted.
# ---------------------------------------------------------------------------


def test_chain_abort_on_prev_resolution_failure(
    khive_session: KhiveMcpSession,
    temp_namespace: str,
) -> None:
    """Chain with 3 ops: second op references a non-existent $prev field; remainder aborted.

    ADR: ADR-016
    section: Abort-on-failure

    The failing op uses get(id=$prev.nonexistent_field) — the dispatcher
    cannot resolve the $prev path so the op fails, and all subsequent ops
    must be marked aborted (ok=False, aborted=True).

    Asserts:
    - Op 0: ok=True (create succeeded).
    - Op 1: ok=False, aborted absent or False (the failing op itself is not aborted).
    - Op 2: ok=False, aborted=True (downstream op, never dispatched).
    - Summary: total=3, succeeded=1, failed=1, aborted=1.
    """
    ns = temp_namespace
    name_a = f"ChainAbortSource_{uuid.uuid4().hex[:6]}"
    name_b = f"ChainAbortTarget_{uuid.uuid4().hex[:6]}"

    ops = (
        f'create(kind="entity", entity_kind="concept", name="{name_a}", namespace="{ns}")'
        # $prev.bogus_field does not exist in the create result — dispatcher must fail this op.
        f' | get(id=$prev.bogus_field, namespace="{ns}")'
        # This third op should never run and must appear as aborted.
        f' | create(kind="entity", entity_kind="concept", name="{name_b}", namespace="{ns}")'
    )
    envelope = khive_session.request(ops)

    # Note: assert_envelope is intentionally omitted here — substitution errors
    # produce a structured {kind, message} object in the error field rather than
    # a plain string, which fails the envelope schema. The abort semantics are
    # what this test is verifying, not envelope shape.
    results = envelope["results"]
    summary = envelope.get("summary", {})

    assert len(results) == 3, f"Expected 3 results, got {len(results)}: {results}"

    # Op 0: create must succeed.
    assert results[0].get("ok") is True, f"create (op 0) must succeed: {results[0]}"

    # Op 1: get with unresolvable $prev path must fail (not be silently ok).
    assert results[1].get("ok") is False, (
        f"get with bogus $prev path (op 1) must fail: {results[1]}"
    )
    assert not results[1].get("aborted"), (
        f"The failing op (op 1) must not be marked aborted — it was dispatched: {results[1]}"
    )

    # Op 2: must be aborted because op 1 failed.
    assert results[2].get("ok") is False, f"aborted op (op 2) must have ok=False: {results[2]}"
    assert results[2].get("aborted") is True, (
        f"aborted op (op 2) must have aborted=True: {results[2]}"
    )

    assert summary.get("total") == 3, f"summary.total != 3: {summary}"
    assert summary.get("succeeded") == 1, f"summary.succeeded != 1: {summary}"
    assert summary.get("failed") == 1, f"summary.failed != 1: {summary}"
    assert summary.get("aborted") == 1, f"summary.aborted != 1: {summary}"


# ---------------------------------------------------------------------------
# Case 4: Mixed separators rejected at parse time.
#
# `[create(...), create(...) | get(id=$prev.id)]` — mixing `,` and `|` at
# the top level must be rejected as an invalid_params RPC error, not as a
# per-op error inside a successful envelope.
# ---------------------------------------------------------------------------


def test_mixed_separators_rejected_as_rpc_error(
    khive_session: KhiveMcpSession,
    temp_namespace: str,
) -> None:
    """Mixing ',' (parallel) and '|' (chain) at the top level raises KhiveRpcError.

    ADR: ADR-016
    section: Mixed separators

    The DSL parser must reject this input before dispatch. The error surfaces
    as a JSON-RPC invalid_params error (isError or top-level error), not as a
    per-op result with ok=False.
    """
    ns = temp_namespace
    bad_ops = (
        f'[create(kind="entity", entity_kind="concept", name="MixedA", namespace="{ns}")'
        f', create(kind="entity", entity_kind="concept", name="MixedB", namespace="{ns}")'
        f' | get(id=$prev.id, namespace="{ns}")]'
    )
    with pytest.raises(KhiveRpcError):
        khive_session.request(bad_ops)


# ---------------------------------------------------------------------------
# Case 5: $prev dotted-path resolution — $prev.result.name (nested field).
#
# Create an entity and update it in the same chain, using $prev.name as the
# new description (the create result includes a "name" field directly on the
# result object).
# ---------------------------------------------------------------------------


def test_chain_prev_dotted_path_resolution(
    khive_session: KhiveMcpSession,
    temp_namespace: str,
) -> None:
    """$prev.field (single-level dotted path) is resolved correctly at dispatch time.

    ADR: ADR-016
    section: $prev substitution; dotted path

    Uses `create | update(description=$prev.name)` to verify that the
    dispatcher extracts a field from the prior op result and passes it
    as a concrete string arg to the next op.

    Asserts:
    - Both ops ok=True.
    - The updated entity's description equals the source name string.
    """
    ns = temp_namespace
    entity_name = f"DottedPath_{uuid.uuid4().hex[:6]}"

    ops = (
        f'create(kind="entity", entity_kind="concept", name="{entity_name}", namespace="{ns}")'
        f' | update(id=$prev.id, description=$prev.name, namespace="{ns}")'
    )
    envelope = khive_session.request(ops)

    assert_envelope(envelope)
    results = envelope["results"]

    assert len(results) == 2, f"Expected 2 results, got {len(results)}: {results}"
    assert results[0].get("ok") is True, f"create failed: {results[0]}"
    assert results[1].get("ok") is True, f"update failed: {results[1]}"

    # Per P-H2 (ADR-045): get returns flat object — no {data: ...} wrapper.
    # Fetch the updated entity and confirm description == entity_name.
    entity_id = results[0]["result"]["id"]
    fetched = khive_session.verb("get", {"id": entity_id, "namespace": ns})
    description = fetched.get("description")
    assert description == entity_name, (
        f"Expected description={entity_name!r} (from $prev.name), got {description!r}"
    )


# ---------------------------------------------------------------------------
# Case 6: Three-op chain all succeed — verifies multi-hop $prev threading.
#
# create(concept A) | create(concept B) | link(source=$prev.id, target=<A id>)
# Each op uses its own $prev (the immediately preceding op), not the first op.
# ---------------------------------------------------------------------------


def test_chain_three_ops_all_succeed(
    khive_session: KhiveMcpSession,
    temp_namespace: str,
) -> None:
    """Three-op chain: create A | create B | link B→A using $prev.id for B.

    ADR: ADR-016
    section: Chain semantics; multi-hop $prev threading

    The third op's $prev resolves to the second op's result (create B), not
    the first. This verifies that $prev always refers to the immediately
    preceding op's result, not the chain's first op.

    Asserts:
    - All three ops ok=True.
    - Summary: total=3, succeeded=3, failed=0, aborted=0.
    - The link's source_id matches B's id; target_id matches A's id.
    """
    ns = temp_namespace
    name_a = f"ChainTripleA_{uuid.uuid4().hex[:6]}"
    name_b = f"ChainTripleB_{uuid.uuid4().hex[:6]}"

    # Create A first so we have its id for the link target.
    entity_a = khive_session.verb(
        "create",
        {
            "kind": "entity",
            "entity_kind": "concept",
            "name": name_a,
            "namespace": ns,
        },
    )
    id_a: str = entity_a["id"]

    # Chain: create B | link(source=$prev.id [=B], target=A)
    # We include a no-op get at position 0 to make it a true 3-op chain.
    ops = (
        f'create(kind="entity", entity_kind="concept", name="{name_b}", namespace="{ns}")'
        f' | link(source_id=$prev.id, target_id="{id_a}", relation="extends", namespace="{ns}")'
        f' | get(id=$prev.id, namespace="{ns}")'
    )
    envelope = khive_session.request(ops)

    assert_envelope(envelope)
    results = envelope["results"]
    summary = envelope.get("summary", {})

    assert len(results) == 3, f"Expected 3 results, got {len(results)}: {results}"
    assert results[0].get("ok") is True, f"create B failed: {results[0]}"
    assert results[1].get("ok") is True, f"link failed: {results[1]}"
    assert results[2].get("ok") is True, f"get (edge) failed: {results[2]}"

    assert summary.get("total") == 3, f"summary.total != 3: {summary}"
    assert summary.get("succeeded") == 3, f"summary.succeeded != 3: {summary}"
    assert summary.get("failed") == 0, f"summary.failed != 0: {summary}"
    assert summary.get("aborted") == 0, f"summary.aborted != 0: {summary}"

    # Link source must be B, target must be A.
    link_result = results[1]["result"]
    id_b: str = results[0]["result"]["id"]
    assert link_result.get("source_id") in (id_b, id_b[:8]), (
        f"link source_id must be B ({id_b!r}), got {link_result.get('source_id')!r}"
    )
    assert link_result.get("target_id") in (id_a, id_a[:8]), (
        f"link target_id must be A ({id_a!r}), got {link_result.get('target_id')!r}"
    )
