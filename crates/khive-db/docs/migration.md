# Schema Migration System

khive-db uses a forward-only versioned migration system defined in
`src/migrations.rs` (governed by ADR-015).

## How it works

Each migration is a `VersionedMigration` struct with three fields:

- `version` -- monotonically increasing u32 starting at 1
- `name` -- human-readable label recorded in the audit table
- `up` -- SQL DDL statements executed via `execute_batch`

The `run_migrations` function:

1. Creates the `_schema_migrations` tracking table if absent
2. Reads the current DB version (max applied version, or 0)
3. Applies each migration with `version > current` in order
4. Each migration runs in its own transaction; failure rolls back that
   migration and leaves the DB at the prior version
5. Records the applied version, name, and timestamp in `_schema_migrations`

## Version numbering

Versions form a contiguous sequence: 1, 2, 3, ... Gaps are rejected at
runtime. To add a migration, append a `VersionedMigration` entry with
`version = <last + 1>` to the `MIGRATIONS` array.

## Rules

- **Never edit V1.** It is immutable on existing databases.
- **Column-existence guards**: Some migrations add columns that may already
  exist in the DDL constants (used by test/in-process schema creation). The
  runner checks column existence before applying `ALTER TABLE` to stay
  idempotent.
- **Dedup-then-constrain**: Migrations that add unique indexes first
  deduplicate existing rows (keeping the earliest), then create the index.

## Per-version notes

- **V2**: `NOTES_DDL` already includes `name TEXT` for in-process schema
  creation. The migration runner checks column existence before applying V2 to
  stay idempotent.
- **V4**: Deduplicates existing `graph_edges` rows sharing the same
  `(namespace, source_id, target_id, relation)` triple, then adds a unique
  index.
- **V5**: `ENTITIES_DDL` already includes `entity_type TEXT`. Same
  column-existence guard as V2.
- **V9**: Adds lifecycle columns (`updated_at`, `deleted_at`) and
  `target_backend` to `graph_edges` via table rebuild.
- **V13**: Event observability columns. DDL computed at runtime via
  `build_v13_event_observability_sql` to avoid duplicate-column errors.
- **V14**: Embedding model registry (`_embedding_models`). DDL computed at
  runtime to discover existing `vec_*` tables.
- **V16**: Adds `embedding_model` column to regular `vec_*` tables.
- **V17**: Preserving rebuild of `vec0` virtual tables to add `field` and
  `embedding_model` columns without data loss.

## Legacy API

A separate `ServiceSchemaPlan` / `apply_schema_plan` API exists for
per-service migration tracking via the `_schema_versions` table. This
predates the versioned system and is preserved for backward compatibility.
New schema changes should use the versioned `MIGRATIONS` array.
