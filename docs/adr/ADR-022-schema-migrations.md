# ADR-022: Schema Migrations

**Status**: accepted\
**Date**: 2026-05-15\
**Authors**: Ocean, lambda:khive

## Context

`khive-db` needs a migration mechanism that (a) applies schema changes idempotently on every open,
(b) survives ALTER TABLE / column-rename / constraint-addition without silent no-ops, and (c)
produces a queryable history of applied versions. `CREATE TABLE IF NOT EXISTS` alone is insufficient
because it cannot upgrade an existing table; an integer `user_version` pragma is insufficient
because it produces no audit trail.

## Decision

Introduce a **versioned, ordered, idempotent migration system** for `khive-db`:

- Each migration is a `Migration { version: u32, name: &'static str, up: &'static str }` value where
  `up` is a SQLite DDL/DML string.
- Migrations are collected in `pub const MIGRATIONS: &[Migration]` in `migrations.rs`, ordered by
  ascending `version`. The array must be contiguous (1, 2, 3, ...) with no gaps; this is verified at
  runtime on first call.
- Applied migrations are tracked in a `_schema_migrations` table:
  ```sql
  CREATE TABLE IF NOT EXISTS _schema_migrations (
      version   INTEGER PRIMARY KEY,
      name      TEXT NOT NULL,
      applied_at INTEGER NOT NULL
  );
  ```
- `pub fn run_migrations(conn: &mut rusqlite::Connection) -> Result<u32, SqliteError>` applies all
  unapplied migrations in order and returns the highest version now applied (0 if the DB is empty).
- Each migration runs inside its own `conn.transaction()`. A failure mid-migration rolls back that
  migration only; the DB remains at the previously applied version and an error is returned.

## Migration Discovery

Appending a new migration is as simple as adding one entry to the `MIGRATIONS` array. The version
must be the next integer after the last entry. `run_migrations` validates that the array is
contiguous (versions 1, 2, 3, ...) and returns `SqliteError::InvalidData` on violation.

## Initial State (V1)

V1 contains the complete current schema: the four core tables (`entities`, `graph_edges`, `notes`,
`events`) plus their indexes, exactly as defined in `stores/entity.rs`, `stores/graph.rs`,
`stores/note.rs`, and `stores/event.rs` today.

Fresh DBs start at V0 and apply V1+ in order. If `run_migrations` opens a DB that already has a
populated `entities` table but no `_schema_migrations` row, it seeds the migrations table with V1
marked as already applied — this is the bootstrap path for any DB whose schema predates the
introduction of `_schema_migrations`.

## Atomicity

Each migration runs in an explicit `conn.transaction()`. If any statement in the `up` SQL fails, the
transaction is rolled back and `SqliteError::Migration { version, error }` is returned. The
`_schema_migrations` row is inserted inside the same transaction, so a partial migration can never
be recorded as applied.

## Error Type

A new variant is added to `SqliteError`:

```rust
#[error("migration v{version} failed: {error}")]
Migration { version: u32, error: String },
```

## Open Questions

- **Down migrations**: Not implemented in v0.1. Rollback requires manual intervention or restoring
  from a backup.
- **Partial rollback of multi-statement migrations**: Not implemented. If V2 contains ten ALTER
  TABLE statements and the fifth fails, the first four are rolled back with the transaction; the DB
  stays at V1.
- **In-flight schema changes**: Not implemented. Long-running queries during a migration are handled
  by SQLite's own locking; the writer lock in `ConnectionPool` serializes this in practice.

## Alternatives Considered

| Alternative                                      | Pros                   | Cons                                                                          | Why rejected                     |
| ------------------------------------------------ | ---------------------- | ----------------------------------------------------------------------------- | -------------------------------- |
| External migration tool (sqlx-migrate, refinery) | Mature, battle-tested  | Adds a heavyweight dependency; sqlx doesn't fit the trait-only model          | Dependency cost                  |
| Per-service `CREATE TABLE IF NOT EXISTS` only    | Minimal code           | Does not solve the ordering problem; no guarantee of forward-only progression | Doesn't address the root problem |
| Integer user_version pragma                      | Zero additional tables | SQLite-specific; no history visible in the DB                                 | Opaque; no audit trail           |

## Consequences

### Positive

- Schema changes can now be applied safely to existing DBs without data loss.
- The migration history is queryable from the DB itself (`SELECT * FROM _schema_migrations`).
- Adding a new migration is a one-line change to the `MIGRATIONS` array.

### Negative

- `run_migrations` must be called explicitly by callers that want versioned migrations; backends
  that bypass it and apply DDL directly do not benefit from the ordering guarantee.
- No down-migration support makes rollback more manual.

## References

- ADR-005: Storage Capability Traits — defines `khive-db` as the SQLite implementation crate
- `crates/khive-db/src/migrations.rs` — implementation
