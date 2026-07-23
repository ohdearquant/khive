# ADR-062: FTS and ANN Consolidation — Unified Search Tables (Schema V4)

**Status**: Accepted
**Date**: 2026-06-19
**Amends**: [ADR-015](./ADR-015-schema-migrations.md), adding schema version 4
**Depends on**: [ADR-031](./ADR-031-multi-engine-retrieval.md),
[ADR-044](./ADR-044-vector-store-extensions.md),
[ADR-052](./ADR-052-ann-production-lifecycle.md)

## Context

Earlier full-text search tables were created per namespace. Table names embedded a
sanitized namespace suffix, so multi-namespace reads required dynamic fan-out and curation
code repeated the key derivation. Per-namespace ANN snapshots had similar partition
management and left stale tables or snapshots after namespaces changed.

Namespace is data used for visibility filtering; it does not require a separate SQL table
or ANN structure for each value.

## Decision

Schema V4 consolidates full-text search into one entity table and one note table, each with
an unindexed namespace column. ANN indexes are keyed by model rather than namespace.
Reindexing removes stale pre-V4 partitions.

### 1. Unified entity table

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

### 2. Unified note table

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

Visible-namespace filtering is a `WHERE namespace IN (...)` predicate applied in the same
query as `MATCH`. One search operation issues one FTS query for each substrate, not one
query per namespace.

### 3. Migration properties

The V4 migration:

- is forward-only and idempotent;
- uses `IF NOT EXISTS`;
- creates no namespace-derived identifiers; and
- does not drop old partition tables.

Destructive cleanup occurs only during explicit reindex after canonical rows are available
to rebuild the shared tables.

### 4. Runtime table keys

`KhiveRuntime::text()` and `text_for_notes()` use fixed keys:

```rust
self.backend.text("entities");
self.backend.text("notes");
```

Curation and merge paths use the same fixed table names. Namespace sanitization is removed
from search-table selection.

### 5. Global ANN key

The ANN key contains the embedding model and index type, not namespace. Each stored vector
retains namespace metadata. Search over the global index over-fetches candidates, filters
them against the caller's visible namespace set, and retries with a larger bound until:

- the requested visible result count is reached;
- the configured retry count is exhausted; or
- the corpus is exhausted.

Retry growth is bounded. Namespace filtering occurs before final ranking results are
returned.

### 6. Reindex cleanup

`kkernel reindex`:

1. rebuilds `fts_entities` and `fts_notes` from canonical records;
2. rebuilds model-keyed ANN indexes;
3. removes legacy namespace-keyed ANN snapshots; and
4. drops stale `fts_entities_*` and `fts_notes_*` tables.

The FTS sweep excludes the canonical table and all FTS5 shadow tables, including
`*_data`, `*_idx`, `*_docsize`, `*_config`, and `*_content`. Each sweep is
idempotent.

### 7. Existing database path

On boot, `run_migrations` applies V4 to a V3 database. Search remains incomplete until an
explicit reindex populates the shared tables and model-keyed ANN indexes. Reindex must
complete before the old partitions are removed.

A database already at V4 is accepted when its migration identity matches
`fts_consolidation`.

## Invariants

- Namespace values never determine SQL identifiers.
- Search visibility is enforced in every FTS and ANN result path.
- Reindex builds canonical shared structures before deleting legacy structures.
- Shadow tables are never selected as stale partitions.
- ANN retry and over-fetch are bounded.
- Schema-ahead detection remains fail-loud.

## Verification

Tests must cover:

- V3-to-V4 migration and idempotent V4 re-run;
- entity and note search across multiple visible namespaces in one query;
- exclusion of non-visible FTS and ANN results;
- global ANN over-fetch with a namespace-skewed corpus;
- bounded retry exhaustion;
- curation updates using fixed FTS table names;
- stale FTS partition detection without shadow-table deletion;
- stale ANN snapshot removal after rebuild; and
- crash/restart ordering that never deletes the only usable index.

## Alternatives considered

| Alternative                                               | Reason rejected                                                              |
| --------------------------------------------------------- | ---------------------------------------------------------------------------- |
| Keep per-namespace tables and issue dynamic UNION queries | Requires dynamic identifiers and table-existence coordination.               |
| Keep namespace-keyed ANN indexes                          | Duplicates index lifecycle and snapshot management.                          |
| Drop legacy partitions inside the migration               | Destructive before the explicit rebuild proves canonical replacements exist. |
| Encode namespace inside the FTS `MATCH` expression        | Namespace is an unindexed filter column, not full-text content.              |

## Consequences

### Positive

- Query count no longer scales with visible namespace count.
- Runtime and curation code share fixed table keys.
- One ANN index per model replaces namespace-specific snapshots.
- Explicit reindex removes stale partitions safely.

### Negative

- Search may be incomplete between migration and reindex.
- Global ANN search may need bounded over-fetch when visible records are sparse.
- Namespace filtering becomes a mandatory correctness condition on every result path.

## References

- [ADR-015](./ADR-015-schema-migrations.md): versioned migrations
- [ADR-031](./ADR-031-multi-engine-retrieval.md): retrieval engines
- [ADR-044](./ADR-044-vector-store-extensions.md): vector metadata and filtering
- [ADR-052](./ADR-052-ann-production-lifecycle.md): ANN persistence lifecycle
