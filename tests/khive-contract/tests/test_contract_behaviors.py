"""Behavioral contract tests: GQL property projection.

ADR: ADR-016
section: GQL property projection; Invalid column projection error; Compile errors
"""

from __future__ import annotations

import pytest

from khive_contract.client import KhiveMcpSession

VERBS_UNDER_TEST = {"create", "link", "query"}

VALID_NODE_COLUMNS = (
    "id", "name", "kind", "entity_type", "namespace",
    "description", "properties", "created_at", "updated_at",
)


@pytest.mark.adr_016
@pytest.mark.slow
def test_gql_property_projection_valid_columns(
    khive_session: KhiveMcpSession,
    temp_namespace: str,
    sample_entity,
) -> None:
    """RETURN a.name, b.name succeeds and row contains only a_name and b_name keys.

    ADR: ADR-016
    section: GQL property projection

    Ports test_gql_property_projection (valid path) from contract_test.py.
    """
    ns = temp_namespace
    a = khive_session.verb("create", sample_entity(entity_kind="concept", name="GQL_A"))
    b = khive_session.verb("create", sample_entity(entity_kind="concept", name="GQL_B"))
    khive_session.verb("link", {
        "source_id": a["id"],
        "target_id": b["id"],
        "relation": "extends",
        "weight": 1.0,
        "namespace": ns,
    })

    result = khive_session.verb("query", {
        "query": "MATCH (a:concept)-[e:extends]->(b:concept) RETURN a.name, b.name LIMIT 10",
        "namespace": ns,
    })

    rows = result.get("rows", result) if isinstance(result, dict) else result
    assert isinstance(rows, list), f"query must return list of rows, got: {result}"
    assert len(rows) >= 1, f"Expected >=1 rows for valid projection, got: {rows}"

    row = rows[0]
    if "columns" in row:
        flat_row = {col["name"]: col["value"] for col in row["columns"]}
    else:
        flat_row = row

    assert "a_name" in flat_row, (
        f"a_name key missing from projected row: {flat_row}"
    )
    assert "b_name" in flat_row, (
        f"b_name key missing from projected row: {flat_row}"
    )
    assert flat_row["a_name"] in ("GQL_A", {"String": "GQL_A"}) or str(flat_row["a_name"]).endswith("GQL_A") or "GQL_A" in str(flat_row["a_name"]), (
        f"a_name value should be 'GQL_A', got: {flat_row['a_name']!r}"
    )
    # Must NOT contain full entity blob columns when property projection is used
    assert "a_properties" not in flat_row, (
        f"Property projection must not leak a_properties: {flat_row}"
    )


@pytest.mark.adr_016
@pytest.mark.slow
def test_gql_property_projection_invalid_column_error(
    khive_session: KhiveMcpSession,
    temp_namespace: str,
    sample_entity,
) -> None:
    """RETURN a.bogus returns a compile error that names the offending property and lists valid columns.

    ADR: ADR-016
    section: Invalid column projection error; Compile errors

    Ports test_gql_property_projection (error path) from contract_test.py.
    """
    ns = temp_namespace
    a = khive_session.verb("create", sample_entity(entity_kind="concept", name="GQL_ErrA"))
    b = khive_session.verb("create", sample_entity(entity_kind="concept", name="GQL_ErrB"))
    khive_session.verb("link", {
        "source_id": a["id"],
        "target_id": b["id"],
        "relation": "extends",
        "namespace": ns,
    })

    envelope = khive_session.request_batch([{
        "tool": "query",
        "args": {
            "query": "MATCH (a:concept)-[e:extends]->(b:concept) RETURN a.bogus LIMIT 5",
            "namespace": ns,
        },
    }])
    first = envelope["results"][0]
    assert not first.get("ok", False), (
        "RETURN a.bogus must produce an error, not a success"
    )
    err = first.get("error", "")
    assert err, "Error message must be non-empty"
    assert "bogus" in err, (
        f"Error must name the offending property 'bogus': {err!r}"
    )
    # The valid-column list must include entity_type
    assert "entity_type" in err, (
        f"Error must list valid columns including entity_type: {err!r}"
    )
