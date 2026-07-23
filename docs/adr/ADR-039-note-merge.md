# ADR-039: Note Merge Operation

**Status**: accepted\
**Date**: 2026-05-23\
**Authors**: khive maintainers

## Context

ADR-014 (Curation Operations) defines `merge_entity` and establishes the `EntityDedupMergePolicy`
enum. The `MergeParams` struct carries `namespace`, `into_id`, `from_id`, and `strategy`: no
substrate discriminator. All `merge` verb calls route to the entity path.

Notes are structurally distinct from entities. The `Note` struct in `khive-types` carries fields
with no entity counterpart:

- `content: String`: authored body text; entities have no content field.
- `status: NoteStatus`: active / archived / deleted lifecycle distinct from entity deletion.
- `salience: f64` / `decay_factor: f64`: temporal attention weight.
- `expires_at: Option<Timestamp>`: expiry semantics absent from entities.

Silently losing note content during a merge is a data-integrity failure. A note merge that
overwrites a body without recording the source is not recoverable. The `merge_entity` path, if
called with note UUIDs, would apply entity field rules (name, description, entity_type) to a note
record, producing garbage or a runtime error.

Notes also follow a tombstone model rather than hard-delete: the note lifecycle uses
`status = deleted` plus `deleted_at`, matching the existing `NoteStatus` state machine. Entity
merge hard-deletes `from_id`. Applying entity merge semantics to notes would bypass the lifecycle
entirely.

## Decision

Extend the `merge` verb handler in the KG pack to dispatch on the `kind` discriminator,
which is the canonical public DSL parameter (per ADR-014 §public curation DSL uses `kind`).
When `kind = "note"` (or any note-substrate granular kind such as `"observation"` or
`"insight"`) is present, dispatch routes to the `merge_note` runtime path.
`kind = "entity"` (or any entity-substrate granular kind) routes to `merge_entity`,
preserving full backward compatibility for existing callers.

The `substrate` field is internal resolver output: it is never a public DSL parameter.

Mixing entity and note IDs in a single `merge` call, where `into_id` resolves to an entity
and `from_id` resolves to a note or vice versa, is rejected with `InvalidInput` before any
state change occurs. The caller is responsible for ensuring both IDs are the same kind.

### Extended MergeParams

```rust
// crates/khive-pack-kg/src/handlers.rs
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MergeParams {
    into_id:          String,
    from_id:          String,
    kind:             Option<String>,          // public DSL discriminator: "entity" (default) | "note" | granular kinds
    strategy:         Option<String>,          // prefer_into | prefer_from | union
    content_strategy: Option<String>,          // append (default) | prefer_into | prefer_from
    dry_run:          Option<bool>,
    verbose:          Option<bool>,
}
```

`kind` absent or `"entity"` (or any entity-substrate granular kind) routes to the
`merge_entity` path. `kind = "note"` (or any note-substrate granular kind such as
`"observation"` or `"insight"`) routes to the new `merge_note` path defined
below. **Amendment:** `merge_entity`'s signature and description-merge
behavior were _not_ left unchanged: see below.

`substrate` is not a field on `MergeParams`: it is the internal resolved value
after the registry maps `kind` to its storage family.

### Content strategy

Note content requires its own merge policy because entities have no content field.
`ContentMergeStrategy` is defined once here and reused, unchanged, by entity merge
(ADR-014 amendment) to govern the entity `description` field: entities
have no `content` field, but `description` plays the same "freeform text body"
role and needed the same three-way choice. `merge_entity` gained a `content_strategy`
parameter (previously description selection silently followed the entity `policy`
field, which could not express "prefer_from content but prefer_into everything
else"). Both merge paths share the type; the behavior tables below apply verbatim
to entity `description` with `content` read as `description`.

| Value              | Behaviour                                                                                                                                                       |
| ------------------ | --------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `append` (default) | Appends `from` body to `into` body with a plain separator: `\n\n---\n\n{from_content}`. Provenance is stored in `_merge_history`, not embedded in note content. |
| `prefer_into`      | Keeps `into.content` unchanged; `from.content` is discarded.                                                                                                    |
| `prefer_from`      | Replaces `into.content` with `from.content`.                                                                                                                    |

`append` is the default because it is the only lossless option. `prefer_into` and `prefer_from`
are explicit opt-outs: the caller takes responsibility for discarding content.

### Field-level merge rules

| Field          | Rule                                                                                                                                  |
| -------------- | ------------------------------------------------------------------------------------------------------------------------------------- |
| `content`      | Per `content_strategy` above.                                                                                                         |
| `properties`   | Deep-merge via existing `merge_properties`, then append `_merge_history`.                                                             |
| `tags`         | Shipped storage-level notes have no `tags` column; `tags_unioned` remains `0`. Tags in `properties.tags` follow property merge rules. |
| `status`       | `prefer_into` always: status lifecycle is owned by the target note.                                                                   |
| `salience`     | `max(into.salience, from.salience)`: preserve the higher attention weight.                                                            |
| `decay_factor` | `into.decay_factor` always: the caller owns the decay policy of the target note.                                                      |
| `expires_at`   | Later of the two expiry timestamps if either is set; `None` if both are `None`.                                                       |
| `created_at`   | Preserve `into.created_at`: the target note retains its original timestamp.                                                           |
| `updated_at`   | Set to the merge timestamp.                                                                                                           |
| `kind`         | Must match between `into` and `from`; mismatched kinds reject with `IncompatibleKinds`.                                               |

`kind` is immutable per ADR-013. Two notes of different kinds (e.g., `observation` and `insight`)
cannot merge. The caller must supersede one with the other if cross-kind consolidation is needed.

### Provenance

After a successful merge, `into.properties["_merge_history"]` is updated to a JSON array
with one appended entry per merge operation:

```json
{
  "merged_from": "<from_id>",
  "merged_at": 1710000000000000,
  "strategy": "PreferInto | PreferFrom | Union",
  "content_strategy": "Append | PreferInto | PreferFrom"
}
```

`merged_at` is an integer microsecond timestamp. Strategy values are the Rust Debug-form
variant names emitted by the shipped implementation. The `_merge_history` key is created
on first merge if absent. Subsequent merges into the same `into` note append entries.
No provenance edge is created by `merge_note`.

### Graph and index behaviour

Edge rewiring follows the same logic as `merge_entity_sql` in `khive-runtime`:

1. All edges incident to `from_id` (as source or target) are collected.
2. Each edge is rewired: `source_id == from_id` becomes `into_id`; `target_id == from_id`
   becomes `into_id`.
3. If rewiring produces a self-loop (`source == target`), the edge is deleted.
4. Otherwise the rewired edge is upserted via the existing
   `ON CONFLICT(namespace, source_id, target_id, relation) DO NOTHING` clause, which drops
   duplicate natural edges automatically.
5. FTS5 and vector index entries for `from_id` are deleted inside the transaction.
6. After the transaction commits, `into_id` is reindexed (FTS5 + async vector re-insert),
   identical to the entity merge post-commit pattern.

### Tombstoning, not hard-delete

After a successful merge, `from_id` is tombstoned:

- `status` is set to `deleted`.
- `deleted_at` is set to the merge timestamp.

`from_id` is NOT hard-deleted. This matches the note retention model (NoteStatus state machine
per ADR-013) and departs from entity merge, which hard-deletes `from_id`. Notes are designed
to be retained for audit and recovery; tombstoning `from_id` preserves the record while
excluding it from live queries (which filter `deleted_at IS NULL` or `status != deleted`).

A caller that wants `from_id` permanently removed after a merge can call
`delete(kind="note", id=from_id, hard=true)` explicitly.

### Atomicity

All SQL operations: note reads, field writes, edge rewires, FTS5 delete, vec-delete: execute
inside a single `BEGIN IMMEDIATE` transaction via a new `merge_note_sql` function in
`khive-runtime/src/curation.rs`, mirroring the structure of `merge_entity_sql`.

Vector re-insert for `into_id` runs after the transaction commits (embedding generation is async
and cannot run inside `BEGIN IMMEDIATE`), identical to the entity merge pattern. If embedding
fails post-commit, the note is persisted but stale in the vector index; the failure is logged
and the caller can retry the reindex idempotently.

`dry_run = true` reads both records, computes the merged state, builds the `MergeSummary`
preview, and returns without opening a write transaction.

### Extended MergeSummary

Reuses `MergeSummary` from `khive-runtime/src/curation.rs` with two new fields:

```rust
pub struct MergeSummary {
    pub kept_id:            Uuid,
    pub removed_id:         Uuid,
    pub edges_rewired:      usize,
    pub properties_merged:  usize,
    pub tags_unioned:       usize,
    // new in ADR-039
    pub content_appended:   bool,  // true when content_strategy=append and from had non-empty content
    pub dry_run:            bool,  // true when called with dry_run=true
}
```

`content_appended` is `false` for `prefer_into` and `prefer_from` strategies even if `from`
had content. It signals whether the append path actually ran.

## Rationale

### Why extend `merge` rather than add a new verb?

`merge` is the curation verb for deduplication. Adding `merge_note` as a separate verb would
bloat the verb surface (violating the principle of a minimal, stable surface from ADR-025) and
would require callers to know the storage substrate before picking a verb. Kind-discriminated
dispatch via `kind` keeps the surface clean and matches the pattern already used by
`create`, `list`, `search`, and `delete` across all curation verbs (per ADR-014).

### Why `append` as the default content strategy?

`append` is the only lossless option: both bodies are preserved. `prefer_into` and
`prefer_from` both discard one body. Making a lossy option the default would mean callers who
omit `content_strategy` silently lose data. The default must be safe.

### Why tombstone `from_id` instead of hard-deleting?

Entity merge hard-deletes `from_id` because entity identity lives in the graph structure;
once edges are rewired, the source entity has no remaining purpose and can be reclaimed.

Notes carry authored content that may have independent value outside the merge context.
Tombstoning `from_id` gives callers visibility into what was merged (via `status = deleted` +
`_merge_history` provenance) and preserves recovery options. The difference in behaviour is
intentional and reflects the different roles of entities and notes in the system.

### Why reject mixed-kind calls?

An entity UUID and a note UUID are structurally incompatible: they live in different storage
tables, have different field sets, and are indexed differently. Attempting to merge them would
require the runtime to guess how to coerce one record shape into the other: a correctness
hazard. Failing fast with `InvalidInput` before any state change is safer and produces a clear
error message that names the offending IDs and their resolved kinds.

### Why must note kinds match?

Note kind is immutable per ADR-013. Two notes of different kinds can have different semantic
contracts. Merging them would produce a record with an ambiguous kind. The correct consolidation
pattern is to supersede one with a new note of the desired kind, which makes the intent
explicit.

### Why preserve `decay_factor` from `into`?

`decay_factor` is a policy setting that controls how quickly a note's salience decays over
time. Inheriting `from.decay_factor` would silently change the retrieval dynamics of the
surviving note. The caller who owns `into` set that decay policy deliberately; merge should
not override it. If the caller wants the merged note to have a different decay factor, they
patch it after the merge.

## Alternatives Considered

| Alternative                                     | Why rejected                                                                                                       |
| ----------------------------------------------- | ------------------------------------------------------------------------------------------------------------------ |
| New `merge_note` verb                           | Surface bloat; kind discriminator on `merge` is cleaner and consistent with other verbs.                           |
| Hard-delete `from_id` like entity merge         | Notes have authored content worth preserving; tombstone model matches note lifecycle.                              |
| `prefer_into` as default content strategy       | Default would silently discard `from.content`; `append` is the only lossless default.                              |
| Union strategy for `salience`                   | Taking `max` captures the higher-attention record; averaging would suppress both.                                  |
| Allow cross-kind merge                          | Kind is immutable; cross-kind consolidation is semantically ambiguous. Supersession is the right tool.             |
| Store provenance in a separate event table only | `_merge_history` in properties makes provenance visible in a single `get` call without joining event tables.       |
| Run vector re-insert inside `BEGIN IMMEDIATE`   | Embedding generation is async; cannot execute inside a synchronous SQLite transaction without blocking the writer. |

## Consequences

### Positive

- Note content is never silently overwritten; the default `append` strategy preserves both bodies.
- Provenance in `_merge_history` makes merges auditable without a separate event-log query.
- Tombstoning `from_id` preserves recovery options consistent with the note retention model.
- Reuses `merge_properties` and edge-rewire infrastructure from entity merge.
- Backward-compatible: all existing `merge` callers without `kind` (or with `kind="entity"`) are unaffected.
- `dry_run` allows callers to preview the merge outcome before committing.

### Negative

- `merge_note_sql` is a new function (~200 LOC) that mirrors `merge_entity_sql` in structure.
  A future refactor could extract the edge-rewire step into a shared helper to reduce duplication.
- Tombstoned `from_id` notes remain stored for audit and recovery but are excluded from live
  queries by lifecycle filters. This differs from entity merge, which removes the source row.
- Different-kind merge is a hard rejection. A caller trying to consolidate an `observation`
  and an `insight` must create a new note explicitly.

## Tests Required

- Happy path: two notes (same kind) merged; content appended with a plain `---` separator;
  properties deep-merged; `_merge_history` property entry written; `tags_unioned == 0`.
- `prefer_into` content strategy: `into.content` unchanged; `from.content` discarded;
  `content_appended = false` in summary.
- `prefer_from` content strategy: `into.content` replaced with `from.content`.
- Mixed-kind rejection: `into_id` resolves to an entity, `from_id` resolves to a note;
  error returned before any mutation.
- Mixed-kind rejection (reversed): `into_id` is a note, `from_id` is an entity.
- Different-kind rejection: two notes with different kinds return `IncompatibleKinds`.
- `dry_run = true`: returns `MergeSummary` with `dry_run = true`; no mutation occurs; a
  subsequent `get(from_id)` confirms `from_id` still has `status = active`.
- Missing `into_id`: returns an error with the offending ID.
- Missing `from_id`: returns an error with the offending ID.
- `from_id == into_id`: self-merge returns `InvalidInput`.
- Edge rewire: self-loop dropped: an edge `(from_id, relation, from_id)` is deleted, not
  rewired to `(into_id, relation, into_id)`.
- Edge rewire: duplicate natural edge dropped: if `(into_id, relation, X)` already exists
  and `(from_id, relation, X)` is rewired, the `ON CONFLICT DO NOTHING` clause fires and the
  count remains 1.
- Tombstone: after merge, `from_id.status == deleted` and `from_id.deleted_at` is set.
- FTS index: `from_id` is absent from FTS search results after merge; `into_id` returns
  content from both notes when `append` was used.
- Vector index: `from_id` is absent from vector search results after merge.
- Salience: `into.salience = max(into.salience, from.salience)` after merge.
- `expires_at`: later timestamp wins; `None` + `Some(t)` yields `Some(t)`.
- `_merge_history` accumulation: merging a third note into `into_id` appends a second entry
  to `_merge_history` without overwriting the first.

## Open Questions

- **Reindex retry protocol**: if the async vector re-insert fails post-commit, what is the
  retry mechanism? ADR-014 notes the failure is logged and the reindex is idempotent, but no
  explicit retry queue is defined. A follow-up ADR may introduce a `pending_reindex` table.
- **Cross-backend merge**: the same `CoordinatorError::CrossBackendUnsupported` constraint
  from entity merge applies here. If note sharding across backends becomes a deployment
  pattern, a future ADR should extend the coordinator.

## References

- ADR-013: Note Kind Taxonomy: `NoteStatus` state machine and kind immutability.
- ADR-014: Curation Operations: `merge_entity` and `merge_entity_sql` baseline; `MergeSummary`;
  `EntityDedupMergePolicy`; edge rewire protocol; this ADR also amends entity description
  selection through the shared content strategy.
- ADR-017: Pack Standard: KG pack owns the `merge` verb dispatch; `kind` discriminator
  is resolved to substrate before handler selection.
- `khive-runtime/src/curation.rs`: `merge_entity`, `merge_entity_sql`, `merge_properties`,
  `union_tags` helpers: `merge_note_sql` mirrors this structure.
- `khive-types/src/note.rs`: `Note` struct and `NoteStatus` enum.
