"""Contract tests: coordinator fan-out routing surface.

ADR: ADR-029 (coordinator design), ADR-007 Rev 3 (namespace-agnostic locate)
section: coordinator routing; fan-out search; cross-substrate isolation; zero-change invariant
Issue: #260 bullet 4 — Python contract coverage for coordinator routing
Source of truth:
  crates/khive-mcp/src/coordinator.rs   — CoordinatorService trait + T6 Rust tests
  crates/kkernel/src/coordinator/tests.rs — T1-T7 Rust regression suite

These tests exercise the observable MCP verb surface properties that the
coordinator routing contract specifies.  The original tests drive the standard
single-backend kkernel session as the zero-change invariant baseline.  The T2/T3
regressions added for #563 drive two physical SQLite backends through the public
MCP process and verify both the wire result and the on-disk routing.

Rust suite coverage (covered by kkernel coordinator tests, not repeated here):
  T1  single-backend zero-change invariant (Rust unit test, internal)
  T5  record_created prewarns locator (internal prewarm)
  D2  LocatorCache internals

Python contract coverage added here:
  T2         — cross-backend link stamps target_backend on the source backend
  T3         — note search merges hits from two physical backends
  T6c analog  — single-backend search and link produce correct results (zero-change)
  T6d analog  — malformed tags are rejected with per-op ok=false, not silently dropped
  T7a analog  — entity_kind field is non-null in entity search results
  T7b analog  — granular kind filter excludes entities of other kinds
  T7c analog  — min_score floor is applied; impossibly high threshold yields empty results
  cross-substrate isolation  — entity and note searches do not contaminate each other
  batch dispatch  — search and link in the same batch op dispatch independently
"""

from __future__ import annotations

import json
import sqlite3
from dataclasses import dataclass
from pathlib import Path
from typing import Iterator

import pytest

from khive_contract.client import KhiveMcpSession

VERBS_UNDER_TEST = {"create", "search", "link"}


@dataclass(frozen=True)
class TwoBackendHarness:
    session: KhiveMcpSession
    main_db: Path
    tasks_db: Path


@pytest.fixture(scope="module")
def khive_two_backend_session(
    tmp_path_factory: pytest.TempPathFactory,
) -> Iterator[TwoBackendHarness]:
    """KG on ``main`` plus GTD on a second physical SQLite backend."""
    root = tmp_path_factory.mktemp("coordinator-fanout")
    main_db = root / "main.db"
    tasks_db = root / "tasks.db"
    config = root / "khive.toml"
    config.write_text(
        "\n".join(
            [
                "[[backends]]",
                'name = "main"',
                'kind = "sqlite"',
                f"path = {json.dumps(str(main_db))}",
                "",
                "[[backends]]",
                'name = "tasks"',
                'kind = "sqlite"',
                f"path = {json.dumps(str(tasks_db))}",
                "",
                "[packs.gtd]",
                'backend = "tasks"',
                "",
            ]
        ),
        encoding="utf-8",
    )

    with KhiveMcpSession(
        db=None,
        config=config,
        packs=("kg", "gtd"),
        no_embed=True,
        log="error",
    ) as session:
        yield TwoBackendHarness(session=session, main_db=main_db, tasks_db=tasks_db)


def _note_kind(db: Path, note_id: str) -> str | None:
    with sqlite3.connect(f"file:{db}?mode=ro", uri=True) as connection:
        row = connection.execute("SELECT kind FROM notes WHERE id = ?", (note_id,)).fetchone()
    return None if row is None else str(row[0])


def _edge_target_backend(db: Path, edge_id: str) -> tuple[bool, str | None]:
    with sqlite3.connect(f"file:{db}?mode=ro", uri=True) as connection:
        row = connection.execute(
            "SELECT target_backend FROM graph_edges WHERE id = ?", (edge_id,)
        ).fetchone()
    if row is None:
        return False, None
    return True, None if row[0] is None else str(row[0])


# ---------------------------------------------------------------------------
# T2/T3: true two-physical-backend contract coverage (#563)
# ---------------------------------------------------------------------------


@pytest.mark.coordinator_fanout
@pytest.mark.slow
def test_cross_backend_link_stamps_and_persists_target_backend(
    khive_two_backend_session: TwoBackendHarness,
    temp_namespace: str,
) -> None:
    """A task→entity link is written on the task backend and points at ``main``."""
    harness = khive_two_backend_session
    session = harness.session

    concept = session.verb(
        "create",
        {
            "kind": "concept",
            "name": f"cross_backend_target_{temp_namespace[-6:]}",
        },
    )
    task = session.verb(
        "gtd.assign",
        {
            "title": f"cross backend source {temp_namespace[-6:]}",
        },
    )
    task_id = task["full_id"]

    edge = session.verb(
        "link",
        {
            "source_id": task_id,
            "target_id": concept["id"],
            "relation": "annotates",
        },
    )

    assert edge["target_backend"] == "main"
    assert edge["source_id"] == task_id
    assert edge["target_id"] == concept["id"]
    assert _edge_target_backend(harness.tasks_db, edge["id"]) == (True, "main")
    assert _edge_target_backend(harness.main_db, edge["id"]) == (False, None)


@pytest.mark.coordinator_fanout
@pytest.mark.slow
def test_note_search_merges_hits_from_two_physical_backends(
    khive_two_backend_session: TwoBackendHarness,
    temp_namespace: str,
) -> None:
    """A note fan-out returns the KG note from ``main`` and GTD task from ``tasks``."""
    harness = khive_two_backend_session
    session = harness.session
    probe = f"physicalfanout{temp_namespace[-6:]}"

    observation = session.verb(
        "create",
        {
            "kind": "observation",
            "content": f"{probe} observation on the main backend",
        },
    )
    task = session.verb(
        "gtd.assign",
        {
            "title": f"{probe} task on the tasks backend",
        },
    )
    task_id = task["full_id"]

    assert _note_kind(harness.main_db, observation["id"]) == "observation"
    assert _note_kind(harness.main_db, task_id) is None
    assert _note_kind(harness.tasks_db, task_id) == "task"
    assert _note_kind(harness.tasks_db, observation["id"]) is None

    hits = session.verb(
        "search",
        {"kind": "note", "query": probe},
    )
    hits_by_id = {hit["id"]: hit for hit in hits}

    assert hits_by_id[observation["id"]]["note_kind"] == "observation"
    assert hits_by_id[task_id]["note_kind"] == "task"


# ---------------------------------------------------------------------------
# T7a analog: entity_kind field present and correct in search results
# ---------------------------------------------------------------------------


@pytest.mark.coordinator_fanout
@pytest.mark.slow
def test_search_entity_hit_has_entity_kind_field(
    khive_session: KhiveMcpSession,
    temp_namespace: str,
) -> None:
    """search(kind="entity", ...) results must carry a non-null entity_kind field.

    Source: crates/kkernel/src/coordinator/tests.rs
      t7a_multi_backend_search_populates_real_entity_kind

    The coordinator's response-shaping code must populate entity_kind from the
    kind registry after an RRF merge.  A null entity_kind in any hit indicates
    the kind-resolution pass was missing or incorrect.  Single-backend baseline:
    the pack handler always returns entity_kind in the hit JSON (search.rs line
    147).  Multi-backend: the coordinator must replicate that shape.
    """
    ns = temp_namespace

    khive_session.verb("create", {
        "kind": "concept",
        "name": f"cft7a_concept_{ns[-6:]}",
        "description": "coordinator fanout T7a probe for entity_kind presence",
        "namespace": ns,
    })

    hits = khive_session.verb("search", {
        "kind": "entity",
        "query": "cft7a_concept",
        "namespace": ns,
    })

    assert isinstance(hits, list), f"search must return a list; got {type(hits)}"
    assert len(hits) >= 1, (
        "search must find the seeded entity; got empty results"
    )
    for hit in hits:
        assert "entity_kind" in hit, (
            "every entity search hit must carry the 'entity_kind' field; "
            f"got keys {list(hit.keys())} in hit {hit}"
        )
        assert isinstance(hit["entity_kind"], str) and hit["entity_kind"], (
            "entity_kind must be a non-empty string; "
            f"got {hit['entity_kind']!r} in hit {hit}"
        )


# ---------------------------------------------------------------------------
# T7b analog: granular kind filter excludes off-kind entities
# ---------------------------------------------------------------------------


@pytest.mark.coordinator_fanout
@pytest.mark.slow
def test_search_kind_filter_excludes_off_kind(
    khive_session: KhiveMcpSession,
    temp_namespace: str,
) -> None:
    """search(kind="concept", ...) must not return entities of other kinds.

    Source: crates/kkernel/src/coordinator/tests.rs
      t7b_multi_backend_search_kind_filter_excludes_off_kind

    Seeds a concept and a document with overlapping name tokens.  A granular
    kind="concept" search must return only the concept and exclude the document.
    The coordinator must forward the kind_filter to each backend's hybrid_search
    rather than discarding it.  Each returned hit must also carry entity_kind
    matching the requested kind.
    """
    ns = temp_namespace

    concept = khive_session.verb("create", {
        "kind": "concept",
        "name": f"cft7b_target_probe_{ns[-6:]}",
        "description": "coordinator fanout T7b concept probe",
        "namespace": ns,
    })
    concept_id = concept["id"]

    document = khive_session.verb("create", {
        "kind": "document",
        "name": f"cft7b_target_probe_{ns[-6:]}_doc",
        "description": "coordinator fanout T7b document probe",
        "namespace": ns,
    })
    document_id = document["id"]

    hits = khive_session.verb("search", {
        "kind": "concept",
        "query": "cft7b_target_probe",
        "namespace": ns,
    })

    assert isinstance(hits, list), f"search must return a list; got {type(hits)}"
    hit_ids = [h.get("id", "") for h in hits]

    assert concept_id in hit_ids, (
        f"concept entity must appear in concept-filtered search; hit_ids={hit_ids}"
    )
    assert document_id not in hit_ids, (
        "document entity must be excluded from kind='concept' search; "
        f"hit_ids={hit_ids}"
    )
    for hit in hits:
        assert hit.get("entity_kind") == "concept", (
            "every hit from kind='concept' search must have entity_kind='concept'; "
            f"got {hit.get('entity_kind')!r} in {hit}"
        )


# ---------------------------------------------------------------------------
# T7c analog: min_score floor applied
# ---------------------------------------------------------------------------


@pytest.mark.coordinator_fanout
@pytest.mark.slow
def test_search_min_score_filters_all_below_threshold(
    khive_session: KhiveMcpSession,
    temp_namespace: str,
) -> None:
    """search(kind="entity", min_score=2.0) returns empty results for any real entity.

    Source: crates/kkernel/src/coordinator/tests.rs
      t7c_multi_backend_search_min_score_applied

    RRF scores for any real hit are always <= 1/(60+1) ~= 0.016.  A min_score
    of 2.0 is above any achievable RRF score.  If the coordinator or handler
    ignores min_score, the seeded entity would be returned and this test fails.
    An empty result proves min_score is applied (search.rs line 138,
    score_floor = p.min_score.unwrap_or(0.0).max(0.0)).
    """
    ns = temp_namespace

    khive_session.verb("create", {
        "kind": "concept",
        "name": f"cft7c_minscore_probe_{ns[-6:]}",
        "description": "coordinator fanout T7c min_score probe entity",
        "namespace": ns,
    })

    # Confirm the entity is findable without the floor (sanity check).
    unfiltered = khive_session.verb("search", {
        "kind": "entity",
        "query": "cft7c_minscore_probe",
        "namespace": ns,
    })
    assert isinstance(unfiltered, list) and len(unfiltered) >= 1, (
        "sanity: seeded entity must appear without min_score filter"
    )

    # Now apply an impossibly high min_score.
    hits = khive_session.verb("search", {
        "kind": "entity",
        "query": "cft7c_minscore_probe",
        "min_score": 2.0,
        "namespace": ns,
    })

    assert isinstance(hits, list), f"search must return a list; got {type(hits)}"
    assert hits == [], (
        "min_score=2.0 must exclude all results (no real RRF score can reach 2.0); "
        f"got {len(hits)} hit(s): {hits}"
    )


# ---------------------------------------------------------------------------
# T6d analog: malformed tags produces per-op error, not silently empty
# ---------------------------------------------------------------------------


@pytest.mark.coordinator_fanout
@pytest.mark.slow
def test_search_malformed_tags_rejected_as_per_op_error(
    khive_session: KhiveMcpSession,
    temp_namespace: str,
) -> None:
    """search with tags containing a non-string element must produce ok=false.

    Source: crates/khive-mcp/src/coordinator.rs
      t6d_malformed_tags_return_per_op_error_in_multi_backend

    When the tags array contains non-string elements (e.g. [42]), strict serde
    deserialization must reject the input at the parameter boundary and return a
    per-op error (ok=false).  Silently collapsing the filter to an empty Vec
    would return unfiltered results with ok=true — that is the bug this test
    guards against.

    Uses request_batch directly so the per-op ok/error fields are visible.
    verb() would raise KhiveOperationError on ok=false, masking the field check.
    """
    ns = temp_namespace

    # Seed one entity so the server has something to match, making the
    # "silently returns unfiltered results" failure mode more visible.
    khive_session.verb("create", {
        "kind": "concept",
        "name": f"cft6d_probe_{ns[-6:]}",
        "description": "coordinator fanout T6d malformed tags probe entity",
        "namespace": ns,
    })

    # Non-string element in tags — serde Vec<String> deserialization must reject this.
    envelope = khive_session.request_batch([{
        "tool": "search",
        "args": {
            "kind": "entity",
            "query": "cft6d_probe",
            "tags": [42],
            "namespace": ns,
        },
    }])

    results = envelope.get("results", [])
    assert results, "response must contain at least one result entry"

    first = results[0]
    assert first.get("ok") is False, (
        "tags=[42] (non-string element) must produce ok=false per-op error; "
        f"got ok={first.get('ok')!r}. Full entry: {first}"
    )
    assert first.get("error"), (
        "per-op error entry must carry a non-empty error message string; "
        f"got: {first}"
    )


# ---------------------------------------------------------------------------
# Cross-substrate isolation: entity search does not return notes
# ---------------------------------------------------------------------------


@pytest.mark.coordinator_fanout
@pytest.mark.slow
def test_search_entity_does_not_return_notes(
    khive_session: KhiveMcpSession,
    temp_namespace: str,
) -> None:
    """search(kind="entity", ...) must not include note IDs in results.

    The coordinator dispatches entity and note fan-outs to separate substrate
    paths (hybrid_search vs search_notes).  Entity and note hits must never
    cross-contaminate.  Single-backend baseline: guaranteed by the handler's
    substrate routing.  Multi-backend: the coordinator must preserve this
    isolation across the merged result set.
    """
    ns = temp_namespace

    entity = khive_session.verb("create", {
        "kind": "concept",
        "name": f"cfxe_entity_probe_{ns[-6:]}",
        "description": "coordinator fanout cross-substrate entity probe",
        "namespace": ns,
    })
    entity_id = entity["id"]

    note = khive_session.verb("create", {
        "kind": "observation",
        "content": f"cfxe_entity_probe_{ns[-6:]} note cross-substrate probe observation",
        "namespace": ns,
    })
    note_id = note["id"]

    hits = khive_session.verb("search", {
        "kind": "entity",
        "query": "cfxe_entity_probe",
        "namespace": ns,
    })

    assert isinstance(hits, list), f"entity search must return a list; got {type(hits)}"
    hit_ids = [h.get("id", "") for h in hits]

    assert entity_id in hit_ids, (
        f"entity must appear in entity-substrate search; hit_ids={hit_ids}"
    )
    assert note_id not in hit_ids, (
        "note ID must not appear in entity-substrate search results (cross-substrate leak); "
        f"hit_ids={hit_ids}"
    )


# ---------------------------------------------------------------------------
# Cross-substrate isolation: note search does not return entities
# ---------------------------------------------------------------------------


@pytest.mark.coordinator_fanout
@pytest.mark.slow
def test_search_note_does_not_return_entities(
    khive_session: KhiveMcpSession,
    temp_namespace: str,
) -> None:
    """search(kind="note", ...) must not include entity IDs in results.

    The coordinator's note fan-out calls search_notes on each backend and merges
    note hits only.  Entity hits from a parallel entity fan-out must not appear
    in the note-substrate result set.  Single-backend baseline: same guarantee
    from the handler's substrate routing.
    """
    ns = temp_namespace

    entity = khive_session.verb("create", {
        "kind": "concept",
        "name": f"cfxn_entity_probe_{ns[-6:]}",
        "description": "coordinator fanout cross-substrate note probe concept",
        "namespace": ns,
    })
    entity_id = entity["id"]

    note = khive_session.verb("create", {
        "kind": "observation",
        "content": f"cfxn_entity_probe_{ns[-6:]} observation note cross-substrate probe",
        "namespace": ns,
    })
    note_id = note["id"]

    hits = khive_session.verb("search", {
        "kind": "note",
        "query": "cfxn_entity_probe",
        "namespace": ns,
    })

    assert isinstance(hits, list), f"note search must return a list; got {type(hits)}"
    hit_ids = [h.get("id", "") for h in hits]

    assert note_id in hit_ids, (
        f"observation note must appear in note-substrate search; hit_ids={hit_ids}"
    )
    assert entity_id not in hit_ids, (
        "entity ID must not appear in note-substrate search results (cross-substrate leak); "
        f"hit_ids={hit_ids}"
    )


# ---------------------------------------------------------------------------
# Zero-change invariant: batch with search and link dispatches independently
# ---------------------------------------------------------------------------


@pytest.mark.coordinator_fanout
@pytest.mark.slow
def test_batch_search_and_link_dispatch_independently(
    khive_session: KhiveMcpSession,
    temp_namespace: str,
) -> None:
    """A batch containing search and link ops must dispatch each independently.

    Source: crates/kkernel/src/coordinator/tests.rs T6c (single-backend bypass),
      T6a/T6b (multi-backend routing)

    In single-backend mode the coordinator is bypassed and each op goes through
    the registry independently (zero-change invariant).  Each op in the response
    envelope must have its own ok/result entry, and the results must be correct
    (search finds the seeded entities; link returns a valid edge).
    """
    ns = temp_namespace

    concept_a = khive_session.verb("create", {
        "kind": "concept",
        "name": f"cfbatch_a_{ns[-6:]}",
        "description": "coordinator fanout batch op concept a",
        "namespace": ns,
    })
    concept_b = khive_session.verb("create", {
        "kind": "concept",
        "name": f"cfbatch_b_{ns[-6:]}",
        "description": "coordinator fanout batch op concept b",
        "namespace": ns,
    })

    # Single batch containing one search op and one link op.
    envelope = khive_session.request_batch([
        {
            "tool": "search",
            "args": {
                "kind": "entity",
                "query": "cfbatch",
                "namespace": ns,
            },
        },
        {
            "tool": "link",
            "args": {
                "source_id": concept_a["id"],
                "target_id": concept_b["id"],
                "relation": "extends",
                "namespace": ns,
            },
        },
    ])

    results = envelope.get("results", [])
    assert len(results) == 2, (
        f"batch of 2 ops must produce 2 result entries; got {len(results)}: {results}"
    )

    search_entry, link_entry = results[0], results[1]

    assert search_entry.get("ok") is True, (
        f"search op in batch must succeed (ok=true); got: {search_entry}"
    )
    search_hits = search_entry.get("result", [])
    assert isinstance(search_hits, list), (
        f"search result must be a list; got {type(search_hits)}"
    )
    assert len(search_hits) >= 1, (
        "batch search must find the seeded concepts; got no hits"
    )

    assert link_entry.get("ok") is True, (
        f"link op in batch must succeed (ok=true); got: {link_entry}"
    )
    link_result = link_entry.get("result")
    assert link_result is not None, "link result must be non-null"
    assert link_result.get("relation") == "extends", (
        f"link result relation must be 'extends'; got {link_result.get('relation')!r}"
    )
