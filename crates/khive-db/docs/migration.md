# Schema Migration System

khive-db uses a forward-only versioned migration system defined in
`src/migrations.rs` (governed by ADR-015).

## How it works

Each migration is a `VersionedMigration` struct with three fields:

- `version` -- monotonically increasing u32 starting at 1
- `name` -- human-readable label recorded in the audit table
- `up` -- SQL DDL statements executed via `execute_batch`

The `run_migrations` function:

1. Creates the `_migrations` tracking table if absent
2. Reads the current DB version (max applied version, or 0)
3. Applies each migration with `version > current` in order
4. Each migration runs in its own transaction; failure rolls back that
   migration and leaves the DB at the prior version
5. Records the applied version, name, and timestamp in `_migrations`

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

## Legacy API

A separate `ServiceSchemaPlan` / `apply_schema_plan` API exists for
per-service migration tracking via the `_schema_versions` table. This
predates the versioned system and is preserved for backward compatibility.
New schema changes should use the versioned `MIGRATIONS` array.
