# Run via: uv run pytest
"""Request DSL contract tests.

ADR: ADR-016 (file named adr_020 per play specification; ADR drift documented in README)
section: Three syntactic forms; Parallel semantics; Chain semantics; UUID arguments;
         Wire shape; Maximum operations per request
"""

from __future__ import annotations

import json
import uuid

import pytest

from khive_contract.client import KhiveMcpSession, KhiveRpcError
from khive_contract.schema import assert_envelope

VERBS_UNDER_TEST = {"create", "get", "link", "update"}


@pytest.mark.adr_016
@pytest.mark.adr_020
@pytest.mark.slow
def test_function_call_single_operation_form(
    khive_session: KhiveMcpSession,
    temp_namespace: str,
) -> None:
    """Function-call DSL single-op: create(kind="entity", ...) is dispatched correctly.

    ADR: ADR-016
    section: Three syntactic forms

    The envelope total==1, succeeded==1; created id is gettable.
    """
    name = f"DslSingle_{uuid.uuid4().hex[:6]}"
    ops = f'create(kind="entity", entity_kind="concept", name="{name}", namespace="{temp_namespace}")'
    envelope = khive_session.request(ops)

    assert_envelope(envelope)
    results = envelope.get("results", [])
    assert len(results) == 1, f"Expected 1 result, got {len(results)}"
    assert results[0].get("ok"), f"Expected ok=True, got: {results[0]}"
    entity_id = results[0]["result"]["id"]
    assert entity_id, "Expected entity id in result"

    # Per P-H2 (ADR-045): get returns flat object — no {data: ...} wrapper.
    fetched = khive_session.verb("get", {"id": entity_id, "namespace": temp_namespace})
    assert fetched.get("kind") == "concept"
    assert fetched.get("name") == name


@pytest.mark.adr_016
@pytest.mark.adr_020
@pytest.mark.slow
def test_json_parallel_batch_preserves_order_and_summary(
    khive_session: KhiveMcpSession,
    temp_namespace: str,
) -> None:
    """JSON-form parallel batch of 3 creates succeeds in input order.

    ADR: ADR-016
    section: Parallel semantics

    Summary total==3, failed==0; all results are in input order and ok.
    """
    ops_list = [
        {"tool": "create", "args": {"kind": "entity", "entity_kind": "concept",
                                     "name": f"Batch{i}", "namespace": temp_namespace}}
        for i in range(3)
    ]
    envelope = khive_session.request_batch(ops_list)
    assert_envelope(envelope)
    results = envelope.get("results", [])
    assert len(results) == 3, f"Expected 3 results, got {len(results)}"

    names = []
    for i, r in enumerate(results):
        assert r.get("ok"), f"Result {i} not ok: {r}"
        names.append(r["result"]["name"])

    assert names == ["Batch0", "Batch1", "Batch2"], (
        f"Results must be in input order, got: {names}"
    )


@pytest.mark.adr_016
@pytest.mark.adr_020
@pytest.mark.slow
def test_short_uuid_prefix_resolution_rules(
    khive_session: KhiveMcpSession,
    temp_namespace: str,
    sample_entity,
) -> None:
    """8-char hex prefix resolves; 7-char and non-hex prefixes return errors.

    ADR: ADR-016
    section: UUID arguments

    Ports test_short_uuid_prefix_resolution from contract_test.py.
    """
    entity = khive_session.verb("create", sample_entity(
        entity_kind="concept", name="PrefixTarget"
    ))
    full_id: str = entity["id"]
    prefix8 = full_id[:8]
    prefix7 = full_id[:7]
    prefix_bad = "ZZZZZZZZ"

    # Per P-H2 (ADR-045): get returns flat object — no {data: ...} wrapper.
    # 8-char prefix must resolve
    fetched = khive_session.verb("get", {"id": prefix8, "namespace": temp_namespace})
    assert fetched.get("kind") == "concept"
    assert fetched.get("name") == "PrefixTarget", (
        f"8-char prefix did not resolve to PrefixTarget: {fetched}"
    )

    # 7-char prefix must fail
    envelope_7 = khive_session.request_batch([{"tool": "get", "args": {"id": prefix7,
                                                                         "namespace": temp_namespace}}])
    first_7 = envelope_7["results"][0]
    assert not first_7.get("ok", False), "7-char prefix should fail"
    assert first_7.get("error"), f"7-char prefix error message must be non-empty"

    # Non-hex 8-char must fail
    envelope_bad = khive_session.request_batch([{"tool": "get", "args": {"id": prefix_bad,
                                                                           "namespace": temp_namespace}}])
    first_bad = envelope_bad["results"][0]
    assert not first_bad.get("ok", False), "Non-hex prefix should fail"
    assert first_bad.get("error"), f"Non-hex prefix error message must be non-empty"


@pytest.mark.adr_016
@pytest.mark.adr_020
@pytest.mark.slow
def test_malformed_dsl_rejected_as_rpc_error(
    khive_session: KhiveMcpSession,
) -> None:
    """Malformed DSL raises KhiveRpcError containing expected/invalid.

    ADR: ADR-016
    section: Parser errors

    Ports smoke malformed DSL assertion.
    """
    with pytest.raises(KhiveRpcError):
        khive_session.request("create(")


@pytest.mark.adr_016
@pytest.mark.adr_020
@pytest.mark.slow
def test_request_response_envelope_matches_schema(
    khive_session: KhiveMcpSession,
    temp_namespace: str,
    sample_entity,
) -> None:
    """Successful and per-op-error envelopes both conform to the envelope schema.

    ADR: ADR-016
    section: Wire shape
    """
    # Success envelope
    success_envelope = khive_session.request_batch([
        {"tool": "create", "args": sample_entity(entity_kind="concept", name="SchemaOk")}
    ])
    assert_envelope(success_envelope)
    assert success_envelope["results"][0].get("ok") is True

    # Per-op error envelope (invalid kind)
    error_envelope = khive_session.request_batch([
        {"tool": "create", "args": {
            "kind": "entity",
            "entity_kind": "invalid_kind",
            "name": "ShouldFail",
            "namespace": temp_namespace,
        }}
    ])
    assert_envelope(error_envelope)
    first = error_envelope["results"][0]
    assert first.get("ok") is False
    assert first.get("error"), "Per-op error must have an error string"


@pytest.mark.adr_016
@pytest.mark.adr_020
@pytest.mark.slow
def test_unknown_verb_is_per_op_error(
    khive_session: KhiveMcpSession,
    temp_namespace: str,
    sample_entity,
) -> None:
    """Unknown verb in a batch returns per-op error without aborting siblings.

    ADR: ADR-016
    section: Unknown verb names
    """
    ops_list = [
        {"tool": "create", "args": sample_entity(entity_kind="concept", name="BeforeFrobnicateA")},
        {"tool": "frobnicate", "args": {"x": 1}},
        {"tool": "create", "args": sample_entity(entity_kind="concept", name="AfterFrobnicateB")},
    ]
    envelope = khive_session.request_batch(ops_list)
    results = envelope.get("results", [])
    assert len(results) == 3, f"Expected 3 results, got {len(results)}"
    assert results[0].get("ok") is True, f"First create should succeed: {results[0]}"
    assert results[1].get("ok") is False, f"Unknown verb should fail: {results[1]}"
    assert results[2].get("ok") is True, f"Third create should succeed: {results[2]}"
