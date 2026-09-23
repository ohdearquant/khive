"""Contract tests: create(items=[...]) bulk creation semantics.

ADR: ADR-017 (pack standard), ADR-023 (note-aware bulk create amendment)
section: bulk creation; atomic vs non-atomic path; per-item failures
Issue: #260 (formal-pack + create_many coverage), surface: PR #232; note items: #890

Source of truth for all asserted semantics:
  crates/khive-pack-kg/src/handlers/create.rs — the bulk path of handle_create
  (prepare_bulk_item for each item, then one atomic unit or one unit per item)

The create verb's bulk path activates when the top-level `items` key is present.

--- Items ---

  Each item carries its own `kind` and is parsed from its own JSON value. An
  entity item takes entity fields (`name` required; entity_kind, entity_type,
  description); a note item takes note fields (`content` required; note_kind,
  salience). properties and tags are shared. A field that does not apply to the
  item's substrate is refused. Entity and note items mix freely in one batch.

--- atomic=true (default) ---

  Every item is prepared before any write, then all items commit in one
  transaction or none do. A failing item fails the whole call.
  Response shape: { attempted, created, skipped: 0, failed: 0, results: [...] }
  results[i] = { index: i, ok: true, result: { id, kind, created: true } }
  When verbose=true: adds "entities" array of created entity objects.
  "entities" key is ABSENT when verbose=false (the default). No "errors" key.

--- atomic=false ---

  Each item is prepared and committed on its own. A malformed item, or one whose
  preparation or commit fails, is that item's own indexed failure; valid
  siblings still commit and the call itself succeeds.
  Response shape: { attempted, created, skipped: 0, failed, errors: [...], results: [...] }
  errors[j] = { index, error: <message string> }; the matching results entry
  carries ok: false. "errors" is always present, even when empty.
  When verbose=true: adds "entities" array of the successful entity objects.

--- Limit guard ---

  More than 1000 items returns Err("bulk create limited to 1000 entries per request").
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
# atomic=true (default) — basic batch
# ---------------------------------------------------------------------------


@pytest.mark.create_many
@pytest.mark.slow
def test_create_many_basic(
    khive_session: KhiveMcpSession,
    temp_namespace: str,
) -> None:
    """create(items=[...]) with 3 valid concepts returns attempted=3 created=3.

    Source: the atomic=true commit path, verbose=false.
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
        "atomic=true + verbose=false must NOT include 'entities' key; "
        f"got keys: {list(result.keys())}"
    )


@pytest.mark.create_many
@pytest.mark.slow
def test_create_many_verbose(
    khive_session: KhiveMcpSession,
    temp_namespace: str,
) -> None:
    """create(items=[...], verbose=true) adds 'entities' array with created objects.

    Source: the atomic commit path, verbose=true branch.
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
        "atomic=true + verbose=true must include 'entities' key; "
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
    """create(items=[...], atomic=false) with all valid items: errors key always present.

    Source: the non-atomic path.
    Unlike the atomic path, the non-atomic response always carries "errors" even
    when it is an empty list.  The atomic path does NOT include "errors" at all.
    This difference in response shape is the canonical distinction.
    """
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
    # Non-atomic path: "errors" key is ALWAYS present.
    assert "errors" in result, (
        "atomic=false response must always include 'errors' key (even when empty); "
        f"got keys: {list(result.keys())}"
    )
    assert result["errors"] == [], (
        f"errors must be empty when all items succeed; got {result['errors']}"
    )


@pytest.mark.create_many
@pytest.mark.slow
def test_create_many_atomic_true_has_no_errors_key(
    khive_session: KhiveMcpSession,
    temp_namespace: str,
) -> None:
    """atomic=true (default) response does NOT include 'errors' key.

    Source: the atomic commit path's response JSON, which does not include
    'errors'.
    This is the shape distinction between atomic and non-atomic responses.
    """
    ns = temp_namespace
    items = [_item(f"cm_noerr_{i}_{ns[-6:]}") for i in range(2)]

    result = khive_session.verb("create", {"items": items, "namespace": ns})

    assert "errors" not in result, (
        "atomic=true response must NOT include 'errors' key; "
        f"got keys: {list(result.keys())}"
    )


# ---------------------------------------------------------------------------
# Note items — entity and note items in one batch
# ---------------------------------------------------------------------------


def _concept_names(khive_session: KhiveMcpSession, ns: str) -> list:
    page = khive_session.verb("list", {"kind": "entity", "entity_kind": "concept",
                                       "namespace": ns})
    listed = page["items"]
    assert isinstance(listed, list), f"list returned non-list: {listed!r}"
    return [e.get("name") for e in listed]


@pytest.mark.create_many
@pytest.mark.slow
def test_create_many_mixed_entity_and_note_items_commit(
    khive_session: KhiveMcpSession,
    temp_namespace: str,
) -> None:
    """An entity item and a note item in one default (atomic) batch both commit.

    Each item resolves its own substrate from its `kind`. `results` is
    index-aligned and carries each new record's id and granular kind even
    without verbose, and each record reads back through `get` at that kind.
    """
    ns = temp_namespace
    items = [
        _item(f"cm_mixed_concept_{ns[-6:]}"),
        {"kind": "observation", "content": f"cm_mixed_note_{ns[-6:]}"},
    ]

    result = khive_session.verb("create", {"items": items, "namespace": ns})

    assert result.get("attempted") == 2 and result.get("created") == 2, (
        f"both items must commit; got {result}"
    )
    assert result.get("failed") == 0, f"failed must be 0; got {result}"
    assert "errors" not in result, (
        f"atomic=true response must NOT include 'errors'; got keys: {list(result.keys())}"
    )
    results = result.get("results")
    assert isinstance(results, list) and len(results) == 2, (
        f"results must hold one entry per item; got {result}"
    )
    for idx, want_kind in enumerate(["concept", "observation"]):
        entry = results[idx]
        assert entry.get("index") == idx and entry.get("ok") is True, (
            f"results[{idx}] must be ok at its own index; got {entry}"
        )
        record = entry.get("result") or {}
        assert record.get("kind") == want_kind and record.get("id"), (
            f"results[{idx}].result must carry an id and kind {want_kind!r}; got {entry}"
        )
        fetched = khive_session.verb("get", {"id": record["id"], "namespace": ns})
        assert fetched.get("kind") == want_kind, (
            f"get({record['id']}) must return kind {want_kind!r}; got {fetched}"
        )


@pytest.mark.create_many
@pytest.mark.slow
def test_create_many_atomic_false_invalid_note_item_fails_at_its_index(
    khive_session: KhiveMcpSession,
    temp_namespace: str,
) -> None:
    """Under atomic=false, a note item without `content` fails at its own index.

    The call succeeds, the valid entity item beside it commits, and the failure
    appears in `errors` as a message string and in `results` as ok=false.
    """
    ns = temp_namespace
    concept = f"cm_partial_concept_{ns[-6:]}"
    items = [
        _item(concept),
        {"kind": "observation", "name": f"cm_partial_note_{ns[-6:]}"},  # no content
    ]

    result = khive_session.verb("create", {
        "items": items,
        "atomic": False,
        "namespace": ns,
    })

    assert result.get("attempted") == 2, f"attempted must be 2; got {result}"
    assert result.get("created") == 1 and result.get("failed") == 1, (
        f"the entity item must commit and only the note item fail; got {result}"
    )
    errors = result.get("errors")
    assert isinstance(errors, list) and len(errors) == 1, (
        f"errors must hold exactly the failed item; got {result}"
    )
    assert errors[0].get("index") == 1 and isinstance(errors[0].get("error"), str), (
        f"errors[0] must name index 1 with a message string; got {errors[0]}"
    )
    results = result.get("results")
    assert isinstance(results, list) and [r.get("ok") for r in results] == [True, False], (
        f"results must be ok for the entity item and not ok for the note item; got {result}"
    )
    assert concept in _concept_names(khive_session, ns), (
        f"the valid entity item must be committed; list omitted {concept!r}"
    )


@pytest.mark.create_many
@pytest.mark.slow
def test_create_many_atomic_invalid_note_item_writes_nothing(
    khive_session: KhiveMcpSession,
    temp_namespace: str,
) -> None:
    """Under the default atomic=true, a note item without `content` fails the call.

    The valid entity item in the same batch is not written. The same list call
    then sees that entity once it is created on its own, so its absence above
    is a reading and not an empty list.
    """
    ns = temp_namespace
    concept = f"cm_atomic_concept_{ns[-6:]}"
    items = [
        _item(concept),
        {"kind": "observation", "name": f"cm_atomic_note_{ns[-6:]}"},  # no content
    ]

    with pytest.raises(KhiveOperationError) as exc_info:
        khive_session.verb("create", {"items": items, "namespace": ns})

    assert "items[1]" in exc_info.value.message, (
        f"the error must name the failing item's index; got: {exc_info.value.message!r}"
    )
    assert concept not in _concept_names(khive_session, ns), (
        f"atomic=true must write nothing when an item fails; list shows {concept!r}"
    )

    khive_session.verb("create", {"items": [_item(concept)], "namespace": ns})
    assert concept in _concept_names(khive_session, ns), (
        f"list must see {concept!r} once it is written"
    )


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

    Source: the bulk path's limit guard:
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
