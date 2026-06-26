# ADR-062: FTS and ANN Consolidation -- Unified Search Tables (Schema V4)

**Status**: Accepted
**Date**: 2026-06-19
**PR**: #171
**Amends**: [ADR-015](ADR-015-schema-migrations.md) -- adds schema version 4
**Depends on**: [ADR-031](ADR-031-multi-engine-retrieval.md), [ADR-044](ADR-044-vector-store-extensions.md), [ADR-052](ADR-052-ann-production-lifecycle.md)

---

## Context

### Per-namespace FTS tables (the problem)

Before schema V4, full-text search tables were created on demand, one per namespace. An entity
create in namespace `local` wrote into `fts_entities_local`; one in namespace `tenant-a` wrote
into `fts_entities_tenant_a`. The table key was computed by sanitizing the namespace string and
appending it as a suffix.

This design had several consequences:

1. **FTS fanout in memory recall**: the memory pack's `collect_recall_candidates` iterated over
   every visible namespace in a loop, issuing one FTS query per namespace. For a caller with a
   visible set of two namespaces, that was two serial FTS round-trips per recall operation.

2. **Merge and curation code carried the sanitized suffix**: `merge_entity` and `merge_note` in
   `curation.rs` reproduced the namespace-sanitize computation to derive the FTS table name.
   Any divergence between the sanitize implementations would silently corrupt the FTS index.

3. **ANN indexes were also per-namespace**: the memory pack's ANN (Vamana) snapshot key was
   `{namespace}::memory_vamana::{model}`. Recall in a multi-namespace context required separate
   index builds per namespace.

4. **Stale partitions accumulated**: old per-namespace FTS and ANN tables stayed in the SQLite
   file indefinitely. `kkernel reindex` had no sweep step to prune them.

### Blocking condition

The khernel daemon (`kkernel mcp --daemon`) had already migrated the live `~/.khive/khive.db` to
schema V4 before this ADR's migration landed in `main`. The pre-V4 `main` codebase triggered the
schema-ahead guard in `run_migrations` (`current(4) > latest_known(3)`) and crashed at boot. This
PR was the V4 realignment to unblock the studio build.

---

## Decision

Consolidate per-namespace FTS5 virtual tables into two shared tables with a `namespace` column,
and replace per-namespace ANN index keys with a single global index per model. Add a sweep pass to
`kkernel reindex` that drops stale per-namespace partitions.

---

## Schema V4 Changes (`crates/khive-db/sql/004-fts-consolidation.sql`)

Two FTS5 virtual tables are created, both using the `trigram` tokenizer:

### `fts_entities`

Unified entity full-text search table, one row per entity across all namespaces.

```sql
CREATE VIRTUAL TABLE IF NOT EXISTS fts_entities USING fts5(
    subject_id UNINDEXED,
    kind       UNINDEXED,
    title,
    body,
    tags       UNINDEXED,
    namespace  UNINDEXED,
    metadata   UNINDEXED,
    updated_at UNINDEXED,
    tokenize = 'trigram'
);
```

Replaces the family of `fts_entities_{namespace}` tables created on demand before V4.

### `fts_notes`

Unified note full-text search table with identical column structure.

```sql
CREATE VIRTUAL TABLE IF NOT EXISTS fts_notes USING fts5(
    subject_id UNINDEXED,
    kind       UNINDEXED,
    title,
    body,
    tags       UNINDEXED,
    namespace  UNINDEXED,
    metadata   UNINDEXED,
    updated_at UNINDEXED,
    tokenize = 'trigram'
);
```

Replaces the family of `fts_notes_{namespace}` tables.

### Migration properties

- Forward-only, `IF NOT EXISTS` safe, idempotent on re-run.
- Does not contain any `DROP TABLE` for the old per-namespace tables. Stale partition cleanup
  is performed at runtime by `kkernel reindex`, not in the SQL, so no namespace names appear
  in the migration file.
- After the migration runs, callers must execute `kkernel reindex --no-knowledge` to repopulate
  the new unified tables from the entity and note rows already in the database.

---

## Runtime Changes

### FTS table key (`crates/khive-runtime/src/runtime.rs`)

`KhiveRuntime::text()` and `text_for_notes()` previously computed a per-namespace key:

```rust
// before V4
let key = format!("entities_{}", sanitize_key(token.namespace().as_str()));
self.backend.text(&key)
```

After V4, the namespace argument is ignored; the shared table name is used directly:

```rust
// V4
self.backend.text("entities")   // text()
self.backend.text("notes")      // text_for_notes()
```

`curation.rs` (merge paths) was updated in the same commit to use `"fts_entities"` and
`"fts_notes"` as the FTS table name, eliminating the sanitized-suffix computation that duplicated
the key logic.

### Memory pack FTS search (`crates/khive-pack-memory/src/handlers/common.rs`)

The `collect_recall_candidates` function previously fanned out over each visible namespace with one
FTS call per namespace. After V4, `collect_text_hits` receives the full visible namespace set and
issues a single FTS query with a `namespace IN (...)` filter.

### ANN index key (`crates/khive-pack-memory/src/ann.rs`)

`AnnKey` previously included the namespace as a field. After V4:

- `AnnKey` is model-only; the namespace field is removed.
- Snapshot key format is `global::memory_vamana::{model}` regardless of namespace.
- `warm_existing_memory_indexes()` issues one `ensure_ann_for_model` call per model using
  `Namespace::local()` as the token; the resulting index serves all namespaces.
- ANN over-fetch (factor 4, minimum 32) with a bounded retry loop (default 3 rounds, env-
  overridable via `ANN_OVERFETCH_MAX_ROUNDS`) addresses the case where a global index is
  dominated by foreign-namespace vectors: each retry widens the fetch limit by 2x until the
  visible-namespace survivor count reaches `candidate_limit` or the corpus is exhausted.

### Stale partition sweep (`crates/kkernel/src/reindex.rs`)

`run_reindex` calls two new idempotent sweep functions at the end of each reindex:

- `purge_stale_memory_vamana_snapshots`: deletes rows from `retrieval_snapshots` where
  `index_type = 'memory_vamana' AND namespace != 'global'`.
- `sweep_stale_fts_partitions`: queries `sqlite_master` for tables matching `fts_entities_%` and
  `fts_notes_%`, skips the canonical shared tables and FTS5 shadow tables (`*_data`, `*_idx`,
  `*_docsize`, `*_config`, `*_content`), and drops each remaining stale table.

Both sweeps are no-ops on databases that have no stale partitions.

---

## Migration Path for Existing Databases

1. The versioned migration system in `run_migrations` applies the V4 SQL automatically on the next
   boot if the database is at V3.
2. After migration, run `kkernel reindex --no-knowledge` to populate `fts_entities` and `fts_notes`
   from existing entity and note rows and to purge stale per-namespace FTS and ANN partitions.
3. Databases already at V4 (migrated by a pre-release daemon) are unaffected; the migration is
   idempotent.

---

## Consequences

### Positive

- A single FTS query per recall operation regardless of the size of the visible namespace set.
- No per-namespace key computation in the runtime or curation layer.
- A single ANN index per model; no namespace-specific snapshot management.
- Stale per-namespace FTS and ANN tables are cleaned up automatically on reindex.
- The schema is now consistent with any deployment topology: single-namespace OSS and
  multi-namespace cloud use the same code path.

### Negative

- After migration, `kkernel reindex --no-knowledge` is required to populate the new tables.
  Until reindex completes, FTS search and ANN recall return empty results on newly-migrated
  databases. The memory pack emits a `WARN`-level FTS population guard during `warm()` when
  the base note count exceeds the FTS row count by more than 2x.
- ANN over-fetch increases the number of vector candidates retrieved per query. For deployments
  where the global index is heavily dominated by foreign-namespace vectors, additional retry rounds
  may increase recall latency.

---

## Alternatives Considered

**Keep per-namespace FTS tables with a cross-namespace UNION query.** Would avoid the reindex
requirement but requires dynamic SQL construction that references table names that may or may not
exist. Fragile under concurrent schema changes. Rejected.

**Add namespace as a query parameter rather than a column.** Not supported by the FTS5 `MATCH`
syntax; namespace filtering must be a WHERE clause on an UNINDEXED column. The chosen approach
is the canonical pattern for FTS5 multi-tenant tables in SQLite.

---

## References

- [ADR-015](ADR-015-schema-migrations.md) -- schema migration system; V4 follows the same
  `VersionedMigration` pattern as V1-V3
- [ADR-031](ADR-031-multi-engine-retrieval.md) -- multi-engine retrieval; FTS is one component
- [ADR-044](ADR-044-vector-store-extensions.md) -- vector store extensions; ANN lifecycle
- [ADR-052](ADR-052-ann-production-lifecycle.md) -- ANN production lifecycle; snapshot management
- `crates/khive-db/sql/004-fts-consolidation.sql` -- the V4 migration SQL
- `crates/khive-db/src/migrations.rs` -- V4 registered as `fts_consolidation` at version 4
- `crates/kkernel/src/reindex.rs` -- stale partition sweep implementation
