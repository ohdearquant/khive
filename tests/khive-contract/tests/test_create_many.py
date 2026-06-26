"""Contract tests: create(items=[...]) bulk entity creation semantics.

ADR: ADR-017 (pack standard)
section: bulk entity creation; atomic vs non-atomic path; spec-building failure semantics
Issue: #260 (formal-pack + create_many coverage), surface: PR #232

Source of truth for all asserted semantics:
  crates/khive-pack-kg/src/handlers/create.rs — handle_create, bulk path lines 68-203

The create verb's bulk path activates when the top-level `items` key is present.

--- atomic=true (default, create.rs lines 97-164) ---

  specs built for all items (one-shot), then self.runtime.create_many(token, specs)
  called once.  Any runtime failure propagates via `?`, failing the whole batch.
  Response shape: { attempted, created, skipped: 0, failed: 0 }
  When verbose=true: adds "entities" array of created entity objects.
  "entities" key is ABSENT when verbose=false (the default).

--- atomic=false (create.rs lines 165-201) ---

  Each spec calls self.runtime.create_many(token, vec![spec]) individually.
  Per-item errors are collected; successful items are counted in "created".
  Response always includes "errors" key (even if the list is empty).
  Response shape: { attempted, created, skipped: 0, failed, errors: [...] }
  When verbose=true: adds "entities" array of the successful entity objects.

--- Spec-building errors (create.rs lines 109-148) ---

  The for loop that builds EntityCreateSpec values uses `?` for each item.
  If any item triggers an error during spec building, the handler returns Err
  immediately, before reaching the atomic/non-atomic split.
  Items carrying a note kind (e.g., "observation") hit the `_ =>` branch at
  create.rs lines 130-136:
    return Err(RuntimeError::InvalidInput(format!(
        "items[{idx}]: bulk create only supports entity kinds; got {:?}",
        entry.kind
    )))
  This failure is NOT per-item in the non-atomic sense — it aborts the batch
  regardless of the atomic flag.

--- Limit guard (create.rs lines 92-95) ---

  More than 1000 items returns Err("bulk create limited to 1000 entries per request").

--- BulkCreateEntry schema (create.rs/params.rs lines 16-26) ---

  #[serde(deny_unknown_fields)]
  Fields: kind (String), name (String), entity_kind?, entity_type?,
          description?, properties?, tags?
  Note: "content" is not a field; passing it would fail serde deserialization.
  Bulk create supports ENTITY kinds only (kind must resolve to KindSpec::Entity).
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

    Source: create.rs lines 151-164 (atomic=true default path, verbose=false).
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

    Source: create.rs lines 160-162 (atomic path, verbose=true branch).
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

    Source: create.rs lines 165-201 (non-atomic path).
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
    # Non-atomic path: "errors" key is ALWAYS present (create.rs line 189).
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

    Source: create.rs lines 154-163 (atomic response JSON).
    The serde_json::json! literal for the atomic path does not include 'errors'.
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
# Spec-building failures — affect the whole batch before atomic split
# ---------------------------------------------------------------------------


@pytest.mark.create_many
@pytest.mark.slow
def test_create_many_note_kind_in_items_rejected(
    khive_session: KhiveMcpSession,
    temp_namespace: str,
) -> None:
    """items containing a note kind triggers a whole-batch failure before atomic/non-atomic split.

    Source: create.rs lines 111-136 (spec-building loop, _ => branch):
      match &item_kind_spec {
          KindSpec::Entity { .. } => { ... }
          _ => {
              return Err(RuntimeError::InvalidInput(format!(
                  "items[{idx}]: bulk create only supports entity kinds; got {:?}",
                  entry.kind
              )));
          }
      }
    This return Err is inside the for loop before the atomic check at line 151.
    The error propagates immediately regardless of the atomic flag.
    Note: "content" is not a BulkCreateEntry field (deny_unknown_fields).
          Use name= to avoid a serde deserialization error.
    """
    ns = temp_namespace
    items = [
        _item(f"cm_noterej_concept_{ns[-6:]}"),  # valid entity item
        {"kind": "observation", "name": f"cm_noterej_note_{ns[-6:]}"},  # note kind
    ]

    with pytest.raises(KhiveOperationError) as exc_info:
        khive_session.verb("create", {
            "items": items,
            "namespace": ns,
        })

    error_msg = exc_info.value.message.lower()
    # Error from create.rs line 134: "bulk create only supports entity kinds"
    assert "entity" in error_msg or "bulk" in error_msg or "kind" in error_msg, (
        "note-kind rejection must mention entity/bulk/kind in the error; "
        f"got: {exc_info.value.message!r}"
    )


@pytest.mark.create_many
@pytest.mark.slow
def test_create_many_note_kind_rejected_with_atomic_false(
    khive_session: KhiveMcpSession,
    temp_namespace: str,
) -> None:
    """Note kind in items is rejected even when atomic=false.

    The spec-building loop failure happens before the atomic split
    (create.rs lines 109-148 vs lines 151/165).  atomic=false does not
    provide per-item tolerance for spec-building errors — only runtime
    errors in the non-atomic loop at line 170 are collected per-item.
    """
    ns = temp_namespace
    items = [{"kind": "observation", "name": f"cm_noterej2_{ns[-6:]}"}]

    with pytest.raises(KhiveOperationError):
        khive_session.verb("create", {
            "items": items,
            "atomic": False,
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
