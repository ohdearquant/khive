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

The repository consolidated its earlier V1--V22 development ledger into a new
V1 baseline at v0.2.8. The live post-consolidation sequence is:

- **V1**: Complete consolidated `initial_schema` baseline.
- **V2**: Narrows the FTS sections update trigger.
- **V3**: Backfills domain mirror atoms.
- **V4**: Consolidates entity and note FTS tables.
- **V5**: Adds the unique comm external-message-id index.
- **V6**: Adds the brain retune driver state.
- **V7**: Adds the durable `notes_seq` ledger.
- **V8**: Repairs partially populated `notes_seq` ledgers.
- **V9**: Adds the case-insensitive entity-name index.
- **V10**: Adds entity `content_ref` storage and its partial index.
- **V11**: Adds the ANN write-log table.
- **V12**: Adds the model-leading ANN write-log sequence index.
- **V13**: Adds immutable entity, note, and edge insertion-sequence ledgers,
  ordered upgrade backfills, and atomic assignment triggers for stable list
  cursors.
- **V14**: Adds an idempotent compatibility guard enforcing global uniqueness
  of `graph_edges.id`, ahead of the UUID-keyed edge ledger.
- **V15**: Adds serve-ledger attribution fields.
- **V16**: Adds GTD dependency-cycle guard triggers.
- **V17**: Adds the agents pack schema.
- **V18**: Adds ANN consumer-pending lifecycle metadata.
- **V19**: Repairs divergent V13/V14 migration names and cursor-sequence rows.
- **V20**: Adds bounded blob-GC claims and entity write-fence triggers.

The historical pre-consolidation allocation table remains in ADR-015 for
provenance; its version numbers do not describe the live migration array.

## Legacy API

A separate `ServiceSchemaPlan` / `apply_schema_plan` API exists for
per-service migration tracking via the `_schema_versions` table. This
predates the versioned system and is preserved for backward compatibility.
New schema changes should use the versioned `MIGRATIONS` array.
