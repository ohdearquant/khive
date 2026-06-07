# khive-vcs Protocol and Persistence Invariants

**Review date:** 2026-06-06
**ADRs:** ADR-010, ADR-020, ADR-035, ADR-036, ADR-037, ADR-042

## Overview

`khive-vcs` provides content-addressed snapshot hashing and NDJSON-to-SQLite sync
for the khive knowledge graph. KG state lives as sorted NDJSON files in a git
repository; this crate computes deterministic `SnapshotId`s and rebuilds the
SQLite working database from those files.

## Modules

| Module | File | Purpose |
|--------|------|---------|
| `types` | `src/types.rs` | `SnapshotId`, `SnapshotCoverage`, `VcsState` |
| `hash` | `src/hash.rs` | Canonical JSON serialization and SHA-256 hashing |
| `sync` | `src/sync.rs` | NDJSON parse, validate-first import, remote fetch |
| `error` | `src/error.rs` | `VcsError` enum |

## Snapshot Format (ADR-010, ADR-042)

A `SnapshotId` is `sha256:` followed by exactly 64 lower-case hex characters.
The canonical JSON used for hashing is:

```json
{"edges":[<sorted-edges>],"entities":[<sorted-entities>]}
```

- Entities sorted by UUID string (case-insensitive ascending).
- Edges sorted by (source, target, relation) ascending.
- Property keys sorted alphabetically within each entity.
- Tags sorted lexicographically within each entity.
- `entity_type` is included in entity serialization (ADR-020 §entity-record-shape).
- `exported_at`, `namespace`, `format`, `version` are excluded from the hash.

## v1 Coverage (ADR-010 §snapshot-coverage)

`KG_V1_COVERAGE`: entities=true, edges=true, notes=false.
Notes are excluded until note packs define versioned export and merge semantics.

## Sync Protocol (ADR-020)

### Local sync (`run_sync`)

1. Read `<repo_root>/.khive/kg/entities.ndjson` and `edges.ndjson`.
2. **Validate-first gate**: parse all edge relations before any DB write. An
   invalid relation aborts the sync; the existing DB is left intact.
3. Build working database in `<db_path>.tmp`.
4. Upsert entities and populate FTS5 index. Skip vector embeddings (computed
   lazily via `kkernel kg embed`, ADR-035 §6).
5. Checkpoint the WAL (`PRAGMA wal_checkpoint(TRUNCATE)`).
6. Atomic rename: `<db_path>.tmp` → `<db_path>`. A crash before this step
   leaves the previous DB intact (all-or-nothing guarantee).

### Remote sync (`run_sync_remote`, ADR-037)

1. `git clone --depth=1 --filter=blob:none` into a temp staging directory.
2. Sparse-checkout `.khive/kg/entities.ndjson` and `.khive/kg/edges.ndjson`.
3. **Validate-first**: `build_kg_archive` parses all edge relations and returns
   an error if any relation is invalid. This runs before any cache write.
4. Compute `SnapshotId` over the validated archive.
5. Pin verification (fail-closed): if `pin` is set and `repin=false`, a hash
   mismatch returns `VcsError::HashMismatch` before any cache file is written.
6. Atomically write cache: entities and edges written to `.tmp` files, then
   renamed into `.khive/kg/remotes/<name>/`.
7. Write `meta.json` with `fetched_at`, `git_ref`, `commit_sha`, `content_hash`.

## Invariants

- `SnapshotId` always satisfies: starts with `"sha256:"`, followed by exactly
  64 lower-case hex characters, no whitespace. Enforced by `from_hash` and
  the custom `Deserialize` implementation.
- Cache files are never written until content hash verification passes.
- The working database is never partially replaced: either the rename succeeds
  or the previous file remains.
- Edge relations are validated before any database or cache write.

## Failure Modes

| Scenario | Behaviour |
|----------|-----------|
| Invalid edge relation in NDJSON | Error before any DB/cache write; previous DB intact |
| Hash mismatch on remote pin | `VcsError::HashMismatch`; no cache files written |
| Git clone failure | Error with remote name only (URL redacted from message) |
| Non-UTF-8 staging path | `Path` passed directly to `Command::arg`; no panic |
| Non-finite edge weight | `VcsError::Internal` from `edge_to_canonical_value` |
| WAL checkpoint failure | Error before rename; previous DB intact |

## Test Coverage

- Unit tests: `src/hash.rs` (hash correctness, edge cases), `src/types.rs`
  (SnapshotId validation, serde rejection), `src/sync.rs` (sync helpers,
  remote fetch, atomicity, FTS population).
- Integration tests: `tests/integration.rs` (cross-module composition,
  adapter pipeline, VcsState roundtrip).

## Baseline Performance

| Scenario | Baseline | Date | Commit | Machine |
|----------|----------|------|--------|---------|
| (not yet measured) | — | — | — | — |
