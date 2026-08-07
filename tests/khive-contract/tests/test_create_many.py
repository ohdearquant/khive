"""Contract tests: create(items=[...]) mixed entity/note semantics.

ADR: ADR-017 (pack standard)
section: mixed bulk creation, per-item failures, atomic opt-in, natural keys
Issue: #260 (formal-pack + create_many coverage), surface: PR #232

Source of truth for all asserted semantics:
  crates/khive-pack-kg/src/handlers/create.rs — handle_create, bulk path lines 68-203

The create verb's bulk path activates when the top-level `items` key is present.

--- default / atomic=false ---

  Every item is deserialized, validated, and written independently. One invalid
  item does not prevent valid siblings from landing. `results` has one ordered
  `{index, ok, ...}` entry per input.

--- atomic=true ---

  All items validate before one mixed entity/note transaction. Validation or
  write failure rejects the whole batch and writes nothing.

--- Note natural keys ---

  Note items accept `content`, optional note fields, and `external_id` (also
  accepted as `properties.external_id`). Retrying the same
  `(namespace, note kind, external_id)` returns the canonical row ID and counts
  as skipped, not created.

--- Limit guard (create.rs lines 92-95) ---

  More than 1000 items returns Err("bulk create limited to 1000 entries per request").

--- BulkCreateEntry schema (create.rs/params.rs lines 16-26) ---

  #[serde(deny_unknown_fields)]
  Fields: kind (String), name?, content?, entity_kind?, note_kind?, entity_type?,
          description?, properties?, tags?, salience?, external_id?
"""

from __future__ import annotations

import pytest

from khive_contract.client import KhiveOperationError, KhiveMcpSession

VERBS_UNDER_TEST = {"create"}


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def _item(name: str, *, kind: str = "concept", **kwargs: object) -> dict:
    entry: dict = {"kind": kind, "name": name}
    entry.update(kwargs)
    return entry


# ---------------------------------------------------------------------------
# default best-effort — basic batch
# ---------------------------------------------------------------------------


@pytest.mark.create_many
@pytest.mark.slow
def test_create_many_basic(
    khive_session: KhiveMcpSession,
    temp_namespace: str,
) -> None:
    """create(items=[...]) with 3 valid concepts returns attempted=3 created=3.

    Source: create.rs mixed bulk best-effort path (verbose=false).
    Response must have keys attempted/created/skipped/failed.
    "entities" key must be ABSENT when verbose is not passed (default false).
    """
    ns = temp_namespace
    items = [
        _item(f"cm_basic_{i}_{ns[-6:]}", description=f"batch item {i}")
        for i in range(3)
    ]

    result = khive_session.verb("create", {"items": items, "namespace": ns})

    assert isinstance(result, dict), f"bulk create must return a dict; got {type(result)}"
    assert result.get("attempted") == 3, (
        f"attempted must equal the number of submitted items (3); got {result}"
    )
    assert result.get("created") == 3, (
        f"created must equal 3 when all items succeed; got {result}"
    )
    assert result.get("failed") == 0, (
        f"failed must be 0 when all items succeed; got {result}"
    )
    assert result.get("skipped") == 0, (
        f"skipped must be 0; got {result}"
    )
    # verbose=false (default): "entities" key must not be present
    assert "entities" not in result, (
        "verbose=false must NOT include 'entities' key; "
        f"got keys: {list(result.keys())}"
    )


@pytest.mark.create_many
@pytest.mark.slow
def test_create_many_verbose(
    khive_session: KhiveMcpSession,
    temp_namespace: str,
) -> None:
    """create(items=[...], verbose=true) adds 'entities' array with created objects.

    Source: create.rs bulk response assembly (verbose=true branch).
    The entities array must have length == created count.
    Each element must carry an "id" field (the new entity's UUID).
    """
    ns = temp_namespace
    n = 3
    items = [_item(f"cm_verbose_{i}_{ns[-6:]}") for i in range(n)]

    result = khive_session.verb("create", {
        "items": items,
        "verbose": True,
        "namespace": ns,
    })

    assert result.get("created") == n, (
        f"created must be {n}; got {result}"
    )
    assert "entities" in result, (
        "verbose=true must include 'entities' key; "
        f"got keys: {list(result.keys())}"
    )
    entities = result["entities"]
    assert isinstance(entities, list), (
        f"'entities' must be a list; got {type(entities)}"
    )
    assert len(entities) == n, (
        f"entities list length ({len(entities)}) must equal created count ({n})"
    )
    for ent in entities:
        assert "id" in ent, (
            f"each entity in 'entities' must have an 'id' field; got {ent}"
        )


# ---------------------------------------------------------------------------
# atomic=false — non-atomic batch
# ---------------------------------------------------------------------------


@pytest.mark.create_many
@pytest.mark.slow
def test_create_many_atomic_false_all_valid(
    khive_session: KhiveMcpSession,
    temp_namespace: str,
) -> None:
    """create(items=[...], atomic=false) returns an empty errors list when valid."""
    ns = temp_namespace
    n = 3
    items = [_item(f"cm_nonatomic_{i}_{ns[-6:]}") for i in range(n)]

    result = khive_session.verb("create", {
        "items": items,
        "atomic": False,
        "namespace": ns,
    })

    assert isinstance(result, dict), f"non-atomic bulk create must return a dict; got {type(result)}"
    assert result.get("attempted") == n, (
        f"attempted must equal {n}; got {result}"
    )
    assert result.get("created") == n, (
        f"created must equal {n} when all items succeed; got {result}"
    )
    assert result.get("failed") == 0, (
        f"failed must be 0 when all items succeed; got {result}"
    )
    # Every bulk response carries the aggregate error list.
    assert "errors" in result, (
        "atomic=false response must always include 'errors' key (even when empty); "
        f"got keys: {list(result.keys())}"
    )
    assert result["errors"] == [], (
        f"errors must be empty when all items succeed; got {result['errors']}"
    )


@pytest.mark.create_many
@pytest.mark.slow
def test_create_many_results_are_ordered_and_complete(
    khive_session: KhiveMcpSession,
    temp_namespace: str,
) -> None:
    """Every bulk response carries one ordered result per submitted item."""
    ns = temp_namespace
    items = [_item(f"cm_noerr_{i}_{ns[-6:]}") for i in range(2)]

    result = khive_session.verb("create", {"items": items, "namespace": ns})
    assert result["errors"] == []
    assert [entry["index"] for entry in result["results"]] == [0, 1]
    assert all(entry["ok"] is True for entry in result["results"])


# ---------------------------------------------------------------------------
# Note items, natural keys, and per-item failures
# ---------------------------------------------------------------------------


@pytest.mark.create_many
@pytest.mark.slow
def test_create_many_note_kind_in_items_supported(
    khive_session: KhiveMcpSession,
    temp_namespace: str,
) -> None:
    """Entity and note items coexist; natural-key retries expose one canonical ID."""
    ns = temp_namespace
    external_id = f"contract:mixed:{ns}"
    items = [
        _item(f"cm_mixed_concept_{ns[-6:]}"),
        {
            "kind": "observation",
            "content": f"cm mixed note {ns}",
            "external_id": external_id,
        },
        {
            "kind": "note",
            "note_kind": "observation",
            "content": "retry content must not overwrite",
            "properties": {"external_id": external_id},
        },
    ]

    result = khive_session.verb("create", {
        "items": items,
        "verbose": True,
        "namespace": ns,
    })
    assert result["attempted"] == 3
    assert result["created"] == 2
    assert result["created_notes"] == 1
    assert result["skipped"] == 1
    assert result["results"][1]["id"] == result["results"][2]["id"]
    assert result["results"][1]["created"] is True
    assert result["results"][2]["deduplicated"] is True
    assert len(result["notes"]) == 2  # canonical row returned for both outcomes


@pytest.mark.create_many
@pytest.mark.slow
def test_create_many_invalid_note_isolated_from_valid_siblings(
    khive_session: KhiveMcpSession,
    temp_namespace: str,
) -> None:
    """Default best effort reports missing note content without aborting siblings."""
    ns = temp_namespace
    result = khive_session.verb("create", {
        "items": [
            _item(f"cm_valid_{ns[-6:]}"),
            {"kind": "observation", "name": "missing content"},
            {"kind": "observation", "content": f"valid note {ns}"},
        ],
        "namespace": ns,
    })
    assert result["attempted"] == 3
    assert result["created"] == 2
    assert result["failed"] == 1
    assert [entry["ok"] for entry in result["results"]] == [True, False, True]


@pytest.mark.create_many
@pytest.mark.slow
def test_create_many_atomic_validation_failure_writes_nothing(
    khive_session: KhiveMcpSession,
    temp_namespace: str,
) -> None:
    """`atomic=true` retains all-or-nothing validation semantics."""
    ns = temp_namespace
    with pytest.raises(KhiveOperationError):
        khive_session.verb("create", {
            "atomic": True,
            "items": [
                _item(f"cm_atomic_should_not_land_{ns[-6:]}"),
                {"kind": "observation", "name": "missing content"},
            ],
            "namespace": ns,
        })


# ---------------------------------------------------------------------------
# Limit guard
# ---------------------------------------------------------------------------


@pytest.mark.create_many
@pytest.mark.slow
def test_create_many_limit_exceeded(
    khive_session: KhiveMcpSession,
    temp_namespace: str,
) -> None:
    """create(items=[...]) with > 1000 items is rejected before any creation.

    Source: create.rs lines 92-95:
      if attempted > 1000 {
          return Err(RuntimeError::InvalidInput(
              "bulk create limited to 1000 entries per request".into(),
          ));
      }
    This guard fires before spec building and before the atomic split.
    """
    ns = temp_namespace
    # 1001 items — one over the 1000-item limit.
    items = [_item(f"cm_limit_{i}") for i in range(1001)]

    with pytest.raises(KhiveOperationError) as exc_info:
        khive_session.verb("create", {
            "items": items,
            "namespace": ns,
        })

    error_msg = exc_info.value.message.lower()
    assert "1000" in error_msg or "limit" in error_msg or "bulk" in error_msg, (
        "1001-item batch must be rejected with a limit-exceeded error; "
        f"got: {exc_info.value.message!r}"
    )
