# ADR-068: Note Merge Operation

**Status**: proposed\
**Date**: 2026-05-21\
**Authors**: Ocean, lambda:khive\
**Depends on**: ADR-014 (KG Curation Operations), ADR-019 (Note Kind Taxonomy), ADR-025 (Pack Standard)

## Context

The runtime ships `merge_entity` (in `crates/khive-runtime/src/curation.rs`) with `MergeStrategy`
(PreferInto, PreferFrom, Union) and edge rewiring. The existing `MergeParams` struct carries
`namespace`, `into_id`, `from_id`, and `strategy` — no substrate discriminator.

Notes are structurally distinct from entities. The `Note` struct (`crates/khive-types/src/note.rs`)
carries fields with no entity counterpart:

- `content: String` — human-authored body text (entities have none)
- `status: NoteStatus` — active/archived/deleted lifecycle distinct from entity deletion
- `salience: f64` / `decay_factor: f64` — temporal attention weight
- `expires_at: Option<Timestamp>` — expiry semantics absent from entities

Silently losing note content during a merge is a data integrity failure.
A note merge that overwrites a body without recording the source is not recoverable.

## Decision

Extend the `merge` verb to accept an optional `substrate` discriminator. When
`substrate = "note"` is present, dispatch routes to a new `merge_note` runtime path.
Mixing entity and note IDs in a single merge call is rejected as `InvalidInput`.

### Extended params

```rust
// crates/khive-pack-kg/src/handlers.rs
#[derive(Deserialize)]
struct MergeParams {
    namespace: Option<String>,
    into_id: String,
    from_id: String,
    strategy: Option<String>,       // prefer_into | prefer_from | union
    substrate: Option<String>,      // "note" enables note-merge path; absent = entity
    content_strategy: Option<String>, // append | prefer_into | prefer_from
    dry_run: Option<bool>,
    verbose: Option<bool>,
}
```

`substrate` defaults to `"entity"` for backward compatibility with existing callers.

### Content strategy

| Value              | Behaviour                                                                                                                          |
| ------------------ | ---------------------------------------------------------------------------------------------------------------------------------- |
| `append` (default) | Appends `from` content to `into` with a provenance separator: `\n\n---\n*merged from`{from_id}`at`{merged_at}`*\n\n{from_content}` |
| `prefer_into`      | Keeps `into.content` unchanged                                                                                                     |
| `prefer_from`      | Replaces with `from.content`                                                                                                       |

### Field-level merge rules

| Field          | Rule                                                              |
| -------------- | ----------------------------------------------------------------- |
| `content`      | Per `content_strategy` above                                      |
| `properties`   | Deep-merge via existing `merge_properties` helper                 |
| `tags`         | Union via existing `union_tags` helper                            |
| `status`       | `prefer_into` always — status lifecycle is set by the target note |
| `salience`     | `max(into.salience, from.salience)`                               |
| `decay_factor` | `into.decay_factor` always (caller owns decay policy)             |
| `expires_at`   | Later of the two expiry timestamps, if either is set              |
| `created_at`   | Preserve `into.created_at`                                        |
| `updated_at`   | Set to merge timestamp                                            |

Provenance is recorded in `into.properties` under the key `_merge_history` as a JSON array
with entries `{ merged_from, merged_at, merge_strategy, content_strategy }`.

### Graph and index behaviour

Edge rewiring follows the same logic as `merge_entity_sql`:

1. All edges incident to `from_id` are rewired to `into_id`.
2. Self-edges produced by rewiring (`source == target`) are deleted.
3. Duplicate natural edges `(source, target, relation)` are dropped (the existing
   `ON CONFLICT(namespace, source_id, target_id, relation) DO NOTHING` covers this).
4. FTS and vector indexes for `from_id` are deleted; `into_id` is reindexed after commit.

`from_id` is tombstoned rather than hard-deleted: `status` is set to `deleted` and
`deleted_at` is recorded, matching the existing note retention model. This departs from
entity merge, which hard-deletes `from_id`.

### Atomicity

All SQL (note reads/writes, edge rewires, FTS delete, vec-delete) executes in one
`BEGIN IMMEDIATE` transaction via a new `merge_note_sql` function mirroring
`merge_entity_sql`. Vector re-insert for `into_id` runs after commit (async embedding),
identical to the entity merge pattern.

`dry_run = true` returns a `MergeSummary`-equivalent preview without committing.

### Return type

Reuse `MergeSummary` (`crates/khive-runtime/src/curation.rs:63`) plus two new fields:

```rust
pub struct MergeSummary {
    pub kept_id: Uuid,
    pub removed_id: Uuid,
    pub edges_rewired: usize,
    pub properties_merged: usize,
    pub tags_unioned: usize,
    // new
    pub content_appended: bool,    // true when content_strategy = append and from had content
    pub dry_run: bool,
}
```

## Consequences

### Positive

- Note content is never silently overwritten; default `append` preserves both bodies.
- Provenance in `_merge_history` makes merges auditable without a separate event log.
- Reuses existing `merge_properties`, `union_tags`, and edge-rewire logic.
- Backward-compatible: existing `merge` callers without `substrate` are unaffected.

### Negative

- `merge_note_sql` is a new ~200-LOC function; duplicates some structure from `merge_entity_sql`.
  A future refactor could extract the edge-rewire logic into a shared helper.
- Tombstone-not-delete for notes means `from_id` remains visible in queries unless the caller
  filters by status. This is consistent with the note lifecycle but differs from entity merge.

### Tests required

- Happy path: two notes merged, content appended, tags unioned, properties deep-merged.
- `prefer_into` and `prefer_from` content strategies.
- Mixed-substrate rejection (entity `into_id` + note `from_id`).
- `dry_run` returns preview without mutation.
- Missing `into_id` and missing `from_id` error paths.
- Edge rewire: self-edge dropped, duplicate natural edge dropped.
- FTS and vector index updated after merge.
- `_merge_history` provenance entry written to properties.

## References

- ADR-014: KG Curation Operations (merge verb is listed there)
- ADR-019: Note Kind Taxonomy (NoteKind closed enum)
- ADR-025: Pack Standard (Pack trait and dispatch contract)
- `crates/khive-runtime/src/curation.rs`: `merge_entity`, `merge_entity_sql`, helpers
- `crates/khive-types/src/note.rs`: `Note` struct and `NoteStatus`
