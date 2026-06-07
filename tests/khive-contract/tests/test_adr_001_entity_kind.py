"""Entity kind taxonomy contract tests.

ADR: ADR-001
section: Entity kinds closed-set registry; MCP verb resolution; Registry contract
"""

from __future__ import annotations

import pytest

from khive_contract.client import KhiveMcpSession, KhiveOperationError

VERBS_UNDER_TEST = {"create", "list", "get"}

# Runtime-confirmed entity kinds (6 legacy kinds; ADR-001 spec adds artifact/service as drift)
RUNTIME_ENTITY_KINDS = ("concept", "document", "project", "dataset", "person", "org")


@pytest.mark.adr_001
@pytest.mark.slow
@pytest.mark.parametrize("entity_kind", RUNTIME_ENTITY_KINDS)
def test_create_list_get_each_entity_kind(
    khive_session: KhiveMcpSession,
    temp_namespace: str,
    sample_entity,
    entity_kind: str,
) -> None:
    """Create, list-filtered, and get each runtime entity kind.

    ADR: ADR-001
    section: 8 entity kinds / MCP verb resolution

    Each create returns an id and name; list filtered by that entity_kind contains
    the returned id; get returns a kind=="entity" wrapper with matching data.
    """
    args = sample_entity(entity_kind=entity_kind, name=f"e_{entity_kind}")
    result = khive_session.verb("create", args)
    assert result is not None, f"create({entity_kind}) returned None"
    entity_id = result.get("id")
    assert entity_id, f"create({entity_kind}) missing 'id': {result}"
    # Runtime response uses 'kind' field for entity_kind value
    assert result.get("kind") == entity_kind, (
        f"kind mismatch: got {result.get('kind')!r}, expected {entity_kind!r}"
    )
    assert result.get("name") == f"e_{entity_kind}", f"name mismatch: {result}"

    # list filtered by entity_kind must include the new id
    listed = khive_session.verb("list", {"kind": "entity", "entity_kind": entity_kind,
                                         "namespace": temp_namespace})
    assert isinstance(listed, list), f"list returned non-list: {listed!r}"
    ids = [e.get("id") for e in listed]
    assert entity_id in ids, (
        f"list(entity_kind={entity_kind}, namespace={temp_namespace}) omitted id={entity_id}; "
        f"got {ids}"
    )

    # Per P-H2 (ADR-045): get returns a flat object with granular kind at top —
    # no {data: ...} wrapper, same shape as create/list.
    fetched = khive_session.verb("get", {"id": entity_id, "namespace": temp_namespace})
    assert fetched is not None, f"get({entity_id}) returned None"
    assert "data" not in fetched, (
        f"get must NOT wrap in {{data: ...}} (P-H2); got: {fetched}"
    )
    assert fetched.get("kind") == entity_kind, (
        f"get kind should be granular {entity_kind!r}, got {fetched.get('kind')!r}"
    )
    assert fetched.get("name") == f"e_{entity_kind}", f"get name mismatch: {fetched}"


@pytest.mark.adr_001
@pytest.mark.slow
def test_invalid_entity_kind_reports_closed_set(
    khive_session: KhiveMcpSession,
    temp_namespace: str,
) -> None:
    """Invalid entity_kind returns per-op error that names the offending kind.

    ADR: ADR-001
    section: Registry contract

    The error must name 'galaxy' and list all valid entity kinds so agents
    can self-correct.  Currently the runtime exposes 6 legacy kinds.
    """
    envelope = khive_session.request_batch([
        {"tool": "create", "args": {
            "kind": "entity",
            "entity_kind": "galaxy",
            "name": "StarSystem",
            "namespace": temp_namespace,
        }}
    ])
    results = envelope.get("results", [])
    assert results, "Expected results in envelope"
    first = results[0]
    assert not first.get("ok", False), "Expected per-op error for invalid entity_kind"
    err = first.get("error", "")
    assert err, "Error message must be non-empty"
    assert "galaxy" in err.lower(), f"Error must name the offending kind 'galaxy': {err!r}"

    # All runtime-known valid kinds must be listed
    for kind in RUNTIME_ENTITY_KINDS:
        assert kind in err, (
            f"Valid entity_kind '{kind}' missing from error message: {err!r}"
        )


@pytest.mark.adr_001
@pytest.mark.slow
def test_create_entity_stores_description_and_tags(
    khive_session: KhiveMcpSession,
    temp_namespace: str,
    sample_entity,
) -> None:
    """create(entity) stores description and tags; get returns them.

    ADR: ADR-001
    section: MCP verb resolution; Entity field contract
    """
    args = sample_entity(
        entity_kind="concept",
        name="TaggedConcept",
        description="a test description",
        tags=["alpha", "beta"],
    )
    result = khive_session.verb("create", args)
    entity_id = result["id"]

    # Per P-H2 (ADR-045): get returns flat object, no data wrapper.
    fetched = khive_session.verb("get", {"id": entity_id, "namespace": temp_namespace})
    assert fetched.get("description") == "a test description", (
        f"description not stored: {fetched}"
    )
    tags = set(fetched.get("tags", []))
    assert "alpha" in tags and "beta" in tags, f"tags not stored correctly: {tags}"


@pytest.mark.adr_001
@pytest.mark.slow
def test_create_entity_namespace_is_stored(
    khive_session: KhiveMcpSession,
    temp_namespace: str,
    sample_entity,
) -> None:
    """Created entity namespace matches the request namespace.

    ADR: ADR-001
    section: MCP verb resolution
    """
    args = sample_entity(entity_kind="concept", name="NamespaceCheck")
    result = khive_session.verb("create", args)
    assert result.get("namespace") == temp_namespace, (
        f"Entity namespace {result.get('namespace')!r} != {temp_namespace!r}"
    )
