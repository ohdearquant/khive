"""End-to-end smoke against a scratch daemon: the database loop a user
actually runs — create, link, read back, query, diagnose."""

from __future__ import annotations

import pytest

from khive import BatchError, Khive, OperationError, op


@pytest.fixture()
def db(scratch_daemon) -> Khive:
    return Khive(socket_path=str(scratch_daemon["socket"]))


def test_handshake_and_stats(db: Khive):
    served = db.session.handshake()
    assert served, "handshake must adopt a non-empty served_config_id"
    stats = db.stats()
    assert isinstance(stats, dict) and stats, f"stats() returned {stats!r}"


def test_entity_crud_and_edges(db: Khive):
    a = db.entities.create(kind="concept", name="smoke-source")
    b = db.entities.create(kind="concept", name="smoke-target")
    assert a.id and b.id and a.id != b.id, "server must mint distinct ids"

    edge = db.graph.link(a.id, b.id, "extends", source_weight=0.9, target_weight=0.4)
    assert edge.source_id == a.id and edge.target_id == b.id
    assert edge.kind.value == "extends", "an edge's kind IS its relation"
    assert edge.weight_for(a.id) == 0.9 and edge.weight_for(b.id) == 0.4, (
        "weight is per-incidence: the same edge weighs differently per node"
    )
    assert edge.namespace and edge.updated_at, "edges carry the full record core"

    fetched = db.graph.edges(limit=50)
    mine = next(e for e in fetched.items if e.id == edge.id)
    assert mine.weight_for(b.id) == 0.4, "per-node weights must survive a round-trip"

    back = db.entities.get(a.id)
    assert back.name == "smoke-source"

    neighborhood = db.graph.neighbors(a.id)
    text = str(neighborhood)
    assert b.id in text, f"neighbor read must surface the linked node: {text[:300]}"


def test_hyperedge_three_members(db: Khive):
    from khive import Incidence

    xs = [db.entities.create(kind="concept", name=f"hyper-{i}") for i in range(3)]
    edge = db.graph.hyperlink(
        "composed_with",
        members=[
            Incidence(node_id=xs[0].id, weight=1.0),
            Incidence(node_id=xs[1].id, weight=0.6),
            Incidence(node_id=xs[2].id, weight=0.2),
        ],
    )
    assert len(edge.members) == 3
    assert edge.weight_for(xs[2].id) == 0.2
    assert edge.source_id is None, "hyperedges have members, not a source/target"

    # The hyperedge must be REAL topology: reachable from any member,
    # exposing every co-member with that co-member's own weight.
    hits = db.graph.incident(xs[2].id, kind="composed_with")
    assert any(e.id == edge.id for e in hits), "edge must be found from a non-source member"
    neighbors = {m.node_id: m.weight for _, m in db.graph.co_members(xs[2].id)}
    assert neighbors[xs[0].id] == 1.0 and neighbors[xs[1].id] == 0.6


def test_note_and_search(db: Khive):
    note = db.notes.create(
        subject="vector search latency",
        content="smoke observation about vector search latency",
    )
    assert note.id
    assert note.subject == "vector search latency"
    page = db.notes.list(limit=10)
    assert page.items, "created note must be listable"


def test_query_gql(db: Khive):
    db.entities.create(kind="concept", name="gql-probe")
    result = db.query("MATCH (c:concept) RETURN c")
    assert result is not None


def test_diagnostics_shape(db: Khive):
    diag = db.diagnostics()
    assert isinstance(diag, dict) and diag
    metrics = db.metrics()
    assert isinstance(metrics, dict)


def test_batch_partial_failure_reported(db: Khive):
    ops = [
        op("create", kind="concept", name="batch-ok"),
        op("get", id="00000000-0000-0000-0000-000000000000"),
    ]
    with pytest.raises(BatchError) as excinfo:
        db.batch(ops)
    err = excinfo.value
    assert len(err.results) == 2 and len(err.failures) == 1
    assert err.failures[0][0] == 1, "the failing op must be identified by index"


def test_invalid_kind_is_op_error(db: Khive):
    with pytest.raises(OperationError):
        db.entities.create(kind="not-a-kind", name="x")
