"""Contract tests: create(items=[...]) kind-aware bulk creation semantics.

ADR: ADR-017 (pack standard)
section: bulk entity/note creation; atomic vs non-atomic path; spec-building failure semantics
Issue: #260 (formal-pack + create_many coverage), surface: PR #232

Source of truth for all asserted semantics:
  crates/khive-pack-kg/src/handlers/create.rs — handle_bulk_create,
  prepare_bulk_create_spec.

The create verb's bulk path activates when the top-level `items` key is present.
Each item's `kind` resolves independently to either an entity kind (e.g.
"concept") or a note kind (e.g. "observation") — bulk create is NOT
entity-only. Entity items require `name`; note items require `content` and
default `note_kind` to "observation". Both substrates may appear in the same
`items` batch.

--- atomic=true (default) ---

  Specs are built for every item first (each resolved independently), then
  notes are created one at a time and entities are created in a single
  self.runtime.create_many(token, entity_specs) call. Any failure — spec
  building or the runtime write — aborts the whole batch before returning;
  any notes already written are rolled back.
  Response shape: { attempted, created, skipped: 0, failed: 0 }
  When verbose=true: adds "entities" and "notes" arrays of the created
  objects. Both keys are ABSENT when verbose=false (the default).

--- atomic=false ---

  Each item's spec is built and (if valid) written individually; a failure
  at either step is collected as a per-item {index, error} entry and does
  not stop the remaining items from being attempted.
  Response always includes "errors" key (even if the list is empty).
  Response shape: { attempted, created, skipped: 0, failed, errors: [...] }
  When verbose=true: adds "entities" and "notes" arrays of the successful
  objects for each substrate.

--- Substrate-domain rejection ---

  Items whose `kind` resolves to neither an entity nor a note kind (edge,
  event, proposal) hit the substrate-rejection branch:
    "items[{idx}]: bulk create supports only entity and note kinds; got {:?}"
  This is spec-building, evaluated per item before any runtime write:
  - atomic=true: this aborts the entire request before anything is created
    (nothing partially written, including valid siblings elsewhere in the
    batch).
  - atomic=false: the best-effort loop catches this per item and appends
    an {index, error} entry to "errors"; valid siblings still succeed and
    persist.

--- Limit guard ---

  More than 1000 items returns Err("bulk create limited to 1000 entries per request").

--- BulkCreateEntry schema (create.rs/params.rs) ---

  #[serde(deny_unknown_fields)]
  Fields: kind (String), name?, entity_kind?, note_kind?, entity_type?,
          description?, content?, salience?, annotates?, properties?, tags?.
  Entity items use name/entity_kind/entity_type/description; note items use
  content/note_kind/salience/annotates. Both accept properties/tags.
"""

from __future__ import annotations

import uuid

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
# Mixed entity + note items — the intended kind-aware bulk contract
# ---------------------------------------------------------------------------


@pytest.mark.create_many
@pytest.mark.slow
def test_create_many_mixed_entity_and_note_items_succeeds(
    khive_session: KhiveMcpSession,
    temp_namespace: str,
) -> None:
    """A note-kind item with valid `content` creates a note, not a rejection.

    Source: create.rs prepare_bulk_create_spec — KindSpec::Entity and
    KindSpec::Note are both accepted bulk substrates (docs/guide/api-reference.md,
    "Create"). A note item is only rejected for missing/invalid note fields
    (e.g. missing `content`), never for carrying a note kind.
    """
    ns = temp_namespace
    items = [
        _item(f"cm_mixed_entity_{ns[-6:]}"),
        {"kind": "observation", "content": f"cm_mixed_note_{ns[-6:]}"},
    ]

    result = khive_session.verb("create", {
        "items": items,
        "verbose": True,
        "namespace": ns,
    })

    assert result.get("attempted") == 2, f"attempted must be 2; got {result}"
    assert result.get("created") == 2, (
        f"both the entity item and the note item must succeed; got {result}"
    )
    assert result.get("failed") == 0, f"failed must be 0; got {result}"
    entities = result.get("entities", [])
    notes = result.get("notes", [])
    assert len(entities) == 1, f"expected exactly 1 created entity; got {result}"
    assert len(notes) == 1, f"expected exactly 1 created note; got {result}"


# ---------------------------------------------------------------------------
# Substrate-domain rejection — kinds outside entity|note
# ---------------------------------------------------------------------------


@pytest.mark.create_many
@pytest.mark.slow
def test_create_many_unsupported_kind_in_items_rejected(
    khive_session: KhiveMcpSession,
    temp_namespace: str,
) -> None:
    """An item whose kind resolves outside entity|note aborts the whole batch (atomic=true).

    Source: create.rs prepare_bulk_create_spec, KindSpec::Edge | Event | Proposal:
      return Err(RuntimeError::InvalidInput(format!(
          "items[{idx}]: bulk create supports only entity and note kinds; got {:?}",
          entry.kind
      )));
    This runs inside the atomic spec-building loop, which uses `?` per item —
    the whole request fails before anything is written, regardless of where
    in the batch the unsupported item sits.
    """
    ns = temp_namespace
    items = [
        _item(f"cm_kindrej_concept_{ns[-6:]}"),  # valid entity item
        {"kind": "edge", "name": f"cm_kindrej_edge_{ns[-6:]}"},  # unsupported substrate
    ]

    with pytest.raises(KhiveOperationError) as exc_info:
        khive_session.verb("create", {
            "items": items,
            "namespace": ns,
        })

    error_msg = exc_info.value.message.lower()
    assert "entity" in error_msg and "note" in error_msg, (
        "unsupported-kind rejection must name the allowed substrates (entity, note); "
        f"got: {exc_info.value.message!r}"
    )

    # Whole-batch abort must mean nothing was written — the valid first item
    # (index 0) must NOT have been persisted alongside the rejected one.
    listed = khive_session.verb("list", {
        "kind": "entity",
        "entity_kind": "concept",
        "namespace": ns,
    })
    names = [e.get("name") for e in listed]
    assert f"cm_kindrej_concept_{ns[-6:]}" not in names, (
        "atomic=true must not persist the valid first item when a later item "
        f"in the same batch is rejected; found it in namespace listing: {names}"
    )


@pytest.mark.create_many
@pytest.mark.slow
def test_create_many_unsupported_kind_rejected_with_atomic_false(
    khive_session: KhiveMcpSession,
    temp_namespace: str,
) -> None:
    """An unsupported-kind item is a per-item error under atomic=false; siblings still succeed.

    Source: create.rs handle_bulk_create non-atomic loop — prepare_bulk_create_spec
    is called per item inside the best-effort loop, so its InvalidInput becomes
    that item's {index, error} entry instead of aborting the request. This is
    the per-item-error contract atomic=false exists to provide; it applies to
    substrate rejection the same way it applies to any other per-item failure.
    """
    ns = temp_namespace
    items = [
        _item(f"cm_kindrej2_concept_{ns[-6:]}"),  # valid entity item, index 0
        {"kind": "edge", "name": f"cm_kindrej2_edge_{ns[-6:]}"},  # rejected, index 1
    ]

    result = khive_session.verb("create", {
        "items": items,
        "atomic": False,
        "namespace": ns,
    })

    assert result.get("attempted") == 2, f"attempted must be 2; got {result}"
    assert result.get("created") == 1, (
        f"the valid entity sibling must still be created; got {result}"
    )
    assert result.get("failed") == 1, f"the unsupported-kind item must fail; got {result}"
    errors = result.get("errors")
    assert errors is not None and len(errors) == 1, (
        f"errors must contain exactly one entry for index 1; got {result}"
    )
    error_entry = errors[0]
    assert error_entry.get("index") == 1, f"error entry must name index 1; got {error_entry}"
    error_msg = str(error_entry.get("error", "")).lower()
    assert "entity" in error_msg and "note" in error_msg, (
        "per-item error must carry the same kind-domain message as the atomic=true case; "
        f"got: {error_entry!r}"
    )

    # The valid sibling (index 0) must have actually persisted, not just been
    # counted — look it up by namespace-scoped list and confirm it is retrievable.
    listed = khive_session.verb("list", {
        "kind": "entity",
        "entity_kind": "concept",
        "namespace": ns,
    })
    sibling_name = f"cm_kindrej2_concept_{ns[-6:]}"
    matches = [e for e in listed if e.get("name") == sibling_name]
    assert len(matches) == 1, (
        f"the valid sibling {sibling_name!r} must be persisted exactly once; "
        f"got matches: {matches}"
    )
    sibling_id = matches[0].get("id")
    fetched = khive_session.verb("get", {"id": sibling_id, "namespace": ns})
    assert fetched is not None, f"get({sibling_id}) returned None for persisted sibling"
    assert fetched.get("name") == sibling_name, (
        f"get({sibling_id}) name mismatch for persisted sibling; got {fetched}"
    )


# ---------------------------------------------------------------------------
# Per-item `annotates` cap and dedup — note items only
# ---------------------------------------------------------------------------


@pytest.mark.create_many
@pytest.mark.slow
def test_create_many_note_annotates_over_cap_rejected_per_item(
    khive_session: KhiveMcpSession,
    temp_namespace: str,
) -> None:
    """A note item's `annotates` array over the per-item cap is rejected, not resolved.

    Source: crates/khive-pack-kg/src/handlers/common.rs resolve_annotates_targets,
    ANNOTATES_CAP. The oversized array is rejected before any target lookup —
    under atomic=false this surfaces as a per-item {index, error} entry naming
    the cap; the valid sibling item still succeeds.
    """
    ns = temp_namespace
    over_cap_targets = [str(uuid.uuid4()) for _ in range(101)]
    items = [
        _item(f"cm_annocap_sibling_{ns[-6:]}"),
        {
            "kind": "observation",
            "content": f"cm_annocap_note_{ns[-6:]}",
            "annotates": over_cap_targets,
        },
    ]

    result = khive_session.verb("create", {
        "items": items,
        "atomic": False,
        "namespace": ns,
    })

    assert result.get("attempted") == 2, f"attempted must be 2; got {result}"
    assert result.get("created") == 1, (
        f"the valid sibling entity must still be created; got {result}"
    )
    assert result.get("failed") == 1, f"the over-cap note item must fail; got {result}"
    errors = result.get("errors")
    assert errors is not None and len(errors) == 1, (
        f"errors must contain exactly one entry for index 1; got {result}"
    )
    error_entry = errors[0]
    assert error_entry.get("index") == 1, f"error entry must name index 1; got {error_entry}"
    error_msg = str(error_entry.get("error", "")).lower()
    assert "annotates" in error_msg and "100" in error_msg, (
        f"cap-exceeded error must name the field and the cap; got: {error_entry!r}"
    )


@pytest.mark.create_many
@pytest.mark.slow
def test_create_many_note_annotates_duplicate_targets_produce_one_edge(
    khive_session: KhiveMcpSession,
    temp_namespace: str,
) -> None:
    """Duplicated targets in a legal-sized `annotates` array collapse to one edge.

    Source: resolve_annotates_targets deduplicates raw targets before resolving
    each to a UUID and linking — a repeated target must not produce repeated
    `annotates` edges.
    """
    ns = temp_namespace
    target = khive_session.verb("create", {
        "kind": "concept",
        "name": f"cm_annodedup_target_{ns[-6:]}",
        "namespace": ns,
    })
    target_id = target["id"]

    result = khive_session.verb("create", {
        "items": [{
            "kind": "observation",
            "content": f"cm_annodedup_note_{ns[-6:]}",
            "annotates": [target_id, target_id, target_id],
        }],
        "namespace": ns,
    })
    assert result.get("created") == 1, f"note create must succeed; got {result}"

    neighbors = khive_session.verb("neighbors", {
        "id": target_id,
        "direction": "incoming",
        "relations": ["annotates"],
        "namespace": ns,
    })
    assert len(neighbors) == 1, (
        f"duplicated annotates targets must produce exactly one edge; got {neighbors}"
    )


# ---------------------------------------------------------------------------
# Aggregate `annotates` budget across a bulk request
# ---------------------------------------------------------------------------


@pytest.mark.create_many
@pytest.mark.slow
def test_create_many_annotates_aggregate_budget_over_rejected_before_resolution(
    khive_session: KhiveMcpSession,
    temp_namespace: str,
) -> None:
    """Per-item arrays each under the per-item cap, but summing over the
    aggregate budget, are rejected before any target resolution.

    Source: crates/khive-pack-kg/src/handlers/common.rs
    check_bulk_annotates_budget, ANNOTATES_BULK_BUDGET (1000). 11 note items
    with 100 targets each (under the per-item cap of 100) sum to 1100, over
    the 1000 aggregate budget — the whole request is rejected up front, not
    surfaced as a per-item error.
    """
    ns = temp_namespace
    items = [
        {
            "kind": "observation",
            "content": f"cm_aggcap_note_{ns[-6:]}_{i}",
            "annotates": [str(uuid.uuid4()) for _ in range(100)],
        }
        for i in range(11)
    ]

    with pytest.raises(KhiveOperationError) as exc_info:
        khive_session.verb("create", {
            "items": items,
            "atomic": False,
            "namespace": ns,
        })

    error_msg = exc_info.value.message.lower()
    assert "1000" in error_msg, (
        f"aggregate-budget error must name the budget (1000); got: {exc_info.value.message!r}"
    )
    assert "1100" in error_msg, (
        f"aggregate-budget error must name the offending total (1100); got: {exc_info.value.message!r}"
    )


@pytest.mark.create_many
@pytest.mark.slow
def test_create_many_annotates_aggregate_budget_at_exact_limit_succeeds(
    khive_session: KhiveMcpSession,
    temp_namespace: str,
) -> None:
    """A bulk request whose total annotates (after per-item dedup) lands
    exactly on the aggregate budget still succeeds.

    10 note items x 100 distinct, real targets each = 1000 total, exactly at
    ANNOTATES_BULK_BUDGET.
    """
    ns = temp_namespace
    target_ids = [
        khive_session.verb("create", {
            "kind": "concept",
            "name": f"cm_aggexact_target_{ns[-6:]}_{i}",
            "namespace": ns,
        })["id"]
        for i in range(100)
    ]

    items = [
        {
            "kind": "observation",
            "content": f"cm_aggexact_note_{ns[-6:]}_{i}",
            "annotates": target_ids,
        }
        for i in range(10)
    ]

    result = khive_session.verb("create", {
        "items": items,
        "atomic": False,
        "namespace": ns,
        "verbose": True,
    })
    assert result.get("attempted") == 10, f"attempted must be 10; got {result}"
    assert result.get("created") == 10, f"all 10 note items must succeed; got {result}"
    assert result.get("failed") == 0, f"no item should fail; got {result}"


@pytest.mark.create_many
@pytest.mark.slow
def test_create_many_annotates_uuid_case_variants_dedup_to_one_edge(
    khive_session: KhiveMcpSession,
    temp_namespace: str,
) -> None:
    """Two encodings of the same UUID (differing letter case) in one item's
    `annotates` dedup to a single resolution and a single edge.

    Source: resolve_annotates_targets now dedups on the parsed UUID (not the
    raw string) when a target parses as a UUID — case variants of the same
    UUID collapse instead of producing duplicate edges.
    """
    ns = temp_namespace
    target = khive_session.verb("create", {
        "kind": "concept",
        "name": f"cm_annocase_target_{ns[-6:]}",
        "namespace": ns,
    })
    target_id = target["id"]
    upper = target_id.upper()
    assert target_id != upper, "fixture UUID must contain letters for a meaningful case test"

    result = khive_session.verb("create", {
        "items": [{
            "kind": "observation",
            "content": f"cm_annocase_note_{ns[-6:]}",
            "annotates": [target_id, upper],
        }],
        "namespace": ns,
    })
    assert result.get("created") == 1, f"note create must succeed; got {result}"

    neighbors = khive_session.verb("neighbors", {
        "id": target_id,
        "direction": "incoming",
        "relations": ["annotates"],
        "namespace": ns,
    })
    assert len(neighbors) == 1, (
        f"case-variant encodings of the same UUID must produce exactly one edge; got {neighbors}"
    )


# ---------------------------------------------------------------------------
# Verbose bulk-note responses project lifecycle status
# ---------------------------------------------------------------------------


@pytest.mark.create_many
@pytest.mark.slow
def test_create_many_verbose_note_projects_lifecycle_status(
    khive_session: KhiveMcpSession,
    temp_namespace: str,
) -> None:
    """A bulk-created note with properties.status returns the projected status,
    same as singleton create — for both atomic=true and atomic=false.

    Source: crates/khive-pack-kg/src/handlers/create.rs handle_bulk_create;
    common.rs remap_note_status_array applies the same projection used by
    singleton create/get/list to the bulk verbose "notes" array.
    """
    ns = temp_namespace

    singleton = khive_session.verb("create", {
        "kind": "observation",
        "content": f"cm_lifecycle_singleton_{ns[-6:]}",
        "properties": {"status": "blocked"},
        "namespace": ns,
    })
    assert singleton.get("status") == "blocked", f"singleton create must project status; got {singleton}"
    assert singleton.get("lifecycle") == "active", f"singleton create must preserve row-visibility as lifecycle; got {singleton}"

    for atomic in (True, False):
        result = khive_session.verb("create", {
            "items": [{
                "kind": "observation",
                "content": f"cm_lifecycle_bulk_{atomic}_{ns[-6:]}",
                "properties": {"status": "blocked"},
            }],
            "atomic": atomic,
            "verbose": True,
            "namespace": ns,
        })
        notes = result.get("notes", [])
        assert len(notes) == 1, f"expected exactly 1 created note (atomic={atomic}); got {result}"
        note = notes[0]
        assert note.get("status") == "blocked", (
            f"bulk verbose note (atomic={atomic}) must project lifecycle status like singleton create; got {note}"
        )
        assert note.get("lifecycle") == "active", (
            f"bulk verbose note (atomic={atomic}) must preserve row-visibility as lifecycle; got {note}"
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
