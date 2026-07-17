# Snapshot hashing (`src/hash.rs`, `src/types.rs`)

A `SnapshotId` is a content-addressed SHA-256 hash of the canonical JSON
representation of a KG's entities and edges. It identifies a specific state
of the graph independent of how it's stored — two exports with the same
entities and edges always hash identically, regardless of insertion order.

## `SnapshotId` canonical form

```text
sha256:<64 lower-case hex characters>
```

No whitespace, no upper-case hex. `from_hash` accepts upper-case input and
normalizes it to lower-case; the custom `Deserialize` implementation is
strict and rejects upper-case hex, whitespace, a missing/wrong prefix, or
the wrong length outright rather than normalizing. `VCS-AUD-004` covers this
with tests exercising each of those rejection cases directly.

## Canonical JSON shape used for hashing

```json
{ "edges": [<sorted-edges>], "entities": [<sorted-entities>] }
```

Sort order (ADR-020 §canonical NDJSON record shape and snapshot hash):

1. Entities sorted by UUID string, case-insensitive ascending.
2. Edges sorted by `(source, target, relation)` ascending.
3. Property keys sorted alphabetically within each entity.
4. Tags sorted lexicographically within each entity.

Root object key order is alphabetical (`edges` before `entities`).
`entity_type` **is** included in the hash (ADR-020 §entity-record-shape) —
`VCS-AUD-003` tests confirm two entities differing only in `entity_type`
produce different `SnapshotId` values. `exported_at`, `namespace`, `format`,
and `version` are excluded from the hash; only entity and edge content
contributes.

Non-finite edge weights (`NaN`, `Infinity`) are rejected by
`edge_to_canonical_value` with `VcsError::Internal` — a correctness gate on
the hash input, not merely a serialization nicety, since a non-finite weight
has no canonical JSON representation to hash consistently.

## v1 coverage

`KG_V1_COVERAGE`: `entities=true, edges=true, notes=false`. Notes are
excluded from snapshot coverage until note packs define versioned export,
import, privacy/redaction, and merge semantics — see `../design.md`.

## Invariants

- A `SnapshotId` always satisfies: starts with `"sha256:"`, followed by
  exactly 64 lower-case hex characters, no whitespace — enforced by both
  `from_hash` (normalizing) and the custom `Deserialize` (strict rejection).
- Cache files under `.khive/kg/remotes/<name>/` are never written until
  content-hash verification passes (see `sync.md`'s remote-sync pin
  verification step).
