# ADR-015: Schema Migrations

**Status**: accepted\
**Date**: 2026-05-23\
**Authors**: khive maintainers

## Context

Database schemas evolve. Tables gain columns, indexes are added or dropped,
constraints tighten, denormalized views are materialized. khive needs a migration
mechanism that:

1. **Applies idempotently.** Running migrations against an already-current
   database is a no-op, not an error.
2. **Survives non-trivial DDL.** `ALTER TABLE ADD COLUMN`, index drops, constraint
   additions, data migrations: all must work without silent failures.
3. **Audits what happened.** A queryable history table records what migrations
   ran and when. `CREATE TABLE IF NOT EXISTS` + `PRAGMA user_version` is
   insufficient because neither produces an audit trail.
4. **Works across federated backends.** Multi-file deployments (ADR-009) have
   one SQLite file per backend. Each backend independently advances through the
   same migration sequence.
5. **Separates core evolution from pack extension.** Core substrate tables
   (entities, notes, edges, events) evolve through versioned migrations. Packs
   add their own auxiliary tables through idempotent schema declarations.

## Migration Ledger

The shipped migration sequence is contiguous. `khive_db::migrations::MIGRATIONS` is the
machine-readable authority for migration identifiers and SQL. The table below summarizes
the complete V1 through V12 sequence in this distribution.

| Version | Purpose                                       | Status  |
| ------: | --------------------------------------------- | ------- |
|      V1 | Consolidated initial schema                   | shipped |
|      V2 | Narrow FTS section-update trigger             | shipped |
|      V3 | Backfill domain mirror atoms                  | shipped |
|      V4 | Consolidate full-text search tables           | shipped |
|      V5 | Add a compatibility data-integrity constraint | shipped |
|      V6 | Add compatibility schema support              | shipped |
|      V7 | Add monotonic note sequence                   | shipped |
|      V8 | Repair note sequence values                   | shipped |
|      V9 | Add case-insensitive entity-name index        | shipped |
|     V10 | Add entity content reference                  | shipped |
|     V11 | Add ANN write log                             | shipped |
|     V12 | Index ANN write-log model sequences           | shipped |

**Invariant:** ADR number order and migration version order are independent. Migration versions reflect schema ledger assignment order. A migration may only depend on schema created by earlier versions.

**Allocation rule:** When a new ADR introduces a schema migration, it must claim the
next ledger version in the same change. Merged ADRs must not retain placeholder versions.

## Decision

### Two schema mechanisms

khive has two distinct schema-application mechanisms with non-overlapping
responsibilities:

| Mechanism                    | Owner      | Purpose                                         | Versioning                                  | Trigger                              |
| ---------------------------- | ---------- | ----------------------------------------------- | ------------------------------------------- | ------------------------------------ |
| **Versioned migrations**     | `khive-db` | Forward-only evolution of core substrate tables | Yes (`_schema_migrations`)                  | `kkernel db migrate`                 |
| **Pack schema declarations** | Pack crate | Idempotent declaration of pack-auxiliary tables | No (boot-time `CREATE TABLE IF NOT EXISTS`) | Pack registration at runtime startup |

Versioned migrations evolve the core schema: tables that ADR-004 substrates
need. Pack schema declarations add pack-specific tables that depend on a backend
but do not belong to the core substrate.

The two mechanisms do not overlap. Core tables are owned by versioned migrations;
pack-auxiliary tables are owned by the pack that declares them.

### Versioned migration model

A migration is a forward-only schema change identified by a contiguous integer
version starting at 1:

```rust
pub struct VersionedMigration {
    /// Contiguous integer version starting at 1. Must equal index+1 in MIGRATIONS.
    pub version: u32,
    /// Human-readable name recorded in _schema_migrations.
    pub name: &'static str,
    /// SQL DDL/DML applied in one transaction. Multiple statements separated by `;`.
    pub up: &'static str,
}

const V1_UP: &str = include_str!("../sql/schema.sql");

pub const MIGRATIONS: &[VersionedMigration] = &[
    VersionedMigration { version: 1, name: "initial_schema", up: V1_UP },
    // V2 through V12 follow contiguously; each sources its own .sql file.
];
```

The migration array is contiguous (`1, 2, 3, ...`); `run_migrations` validates
this at startup and returns `SqliteError::InvalidData` on gaps. Appending a
migration is a one-line change to the array. Inserting in the middle, renumbering,
or skipping versions is a hard error.

### DDL lives in `.sql` files, not inline Rust strings

Migration SQL is authored in `.sql` files under `crates/khive-db/sql/` and pulled
into the migration array with `include_str!`. The `up` field of every
`VersionedMigration` points at a file, not a hand-concatenated Rust string
literal. `V1`'s body is `crates/khive-db/sql/schema.sql`; each later migration uses
`crates/khive-db/sql/NNN-<name>.sql` and references it the same way.

This is the canonical place for schema DDL. Reasons:

- **Tooling sees real SQL.** A `.sql` file is lintable, formattable, and loadable
  into a throwaway SQLite database. `scripts/lint-sql.sh` (wired into CI and
  pre-commit) executes every `crates/**/*.sql` file against an in-memory database
  and checks hygiene, so a malformed migration fails before it ships. Inline
  `"CREATE TABLE …\" \\\n …"` string literals are invisible to every tool and
  drift silently.
- **Diffs are readable.** A schema change shows up as a SQL diff, not as edits to
  escaped Rust string concatenation.
- **No recompile to inspect.** Operators and reviewers read the schema directly
  from the file.

The only DDL that remains an inline Rust constant is the small belt-and-suspenders
set that is _also_ applied outside the migration path (e.g. `EMBEDDING_MODELS_DDL`,
referenced both by the V1 schema and by `StorageBackend::vectors_for_namespace` so
the registry table exists even on a backend created lazily). Those constants are
the documented exception, not the rule: anything that exists only to evolve the
schema belongs in a `.sql` file.

### Future direction: extract reusable SQL into named files

The same principle extends beyond migrations. Non-trivial query SQL: recall
fusion, traversal, scoring/calibration queries: is a candidate for extraction
into `.sql` files (or `.sql`-templated fragments) rather than living as inline
string literals in handler code. Separated SQL can be linted, profiled with
`EXPLAIN QUERY PLAN`, A/B-compared, and tuned for calibration without touching or
recompiling Rust. This is a directional preference, applied where a query is
large, hot, or tuned often enough to justify it: not a mandate to externalize
every one-line `SELECT`.

### Migration tracking table

Every backend file has its own `_schema_migrations` table:

```sql
CREATE TABLE IF NOT EXISTS _schema_migrations (
    version    INTEGER PRIMARY KEY,
    name       TEXT NOT NULL,
    applied_at INTEGER NOT NULL  -- microseconds since epoch
);
```

The table records which migrations have been applied. `run_migrations` reads
the highest applied version and runs only newer migrations. Idempotent calls
are no-ops.

### Atomicity per migration

Each migration runs in its own `BEGIN IMMEDIATE` transaction:

```text
BEGIN IMMEDIATE
    execute_batch(migration.up)
    INSERT INTO _schema_migrations (version, name, applied_at) VALUES (...)
COMMIT
```

A failure within a migration rolls back that migration's transaction. The
database remains at the previously applied version, the `_schema_migrations`
row is not inserted, and `run_migrations` returns
`SqliteError::Migration { version, error }`.

Each migration is independently atomic. If V5 fails, V6 is never attempted, and
the DB stays at V4.

### Per-backend independence (multi-file federation)

In a multi-backend deployment (ADR-009), each SQLite file has its own
`_schema_migrations` table and its own current version. `kkernel db migrate`
iterates over all configured backends and runs migrations independently on each:

```text
for backend in config.backends:
    run_migrations(backend.connection)
```

Backends advance independently. A freshly added backend starts at version 0 and
catches up to the latest version on first migrate. An existing backend at V4
advances to V7 when the codebase ships V5–V7. The migration array is the same
for every backend; the application state may differ per backend until all are
brought current.

### `kkernel db migrate` is the operator entry point

Migration execution is an operator-context operation (ADR-003), not an
agent-context operation. `kkernel db migrate` is the canonical command:

```bash
kkernel db migrate              # migrate all configured backends
kkernel db migrate --backend main   # migrate one specific backend
kkernel db migrate --dry-run        # show what would be applied
kkernel db migrate --check          # exit 0 if current; nonzero otherwise
```

The MCP-serving mode (`kkernel mcp`) does not apply migrations at startup. It assumes
the operator has already run `kkernel db migrate`. If a backend's schema is
behind the codebase's expectation, startup fails fast with a
diagnostic pointing at `kkernel db migrate`. This prevents silent partial
operation on a stale schema.

**Exception**: in-memory `khive-db` backends (used for tests and ephemeral
deployments) apply migrations automatically on creation. There's no operator
to invoke `kkernel db migrate` for an ephemeral DB, and the migration cost is
negligible against an empty store.

### Bootstrap path for pre-versioning databases

A database created before `_schema_migrations` existed (or by an in-process
`CREATE TABLE IF NOT EXISTS` from an earlier khive version) is seeded with the
current latest version on first `run_migrations` call. The bootstrap heuristic:

```text
if _schema_migrations does not exist AND core tables exist:
    create _schema_migrations
    insert (V1, "initial_schema", now)
    run normally from V2 onward
```

This handles the case where a developer ran the in-memory schema-creation path
(used by store-DDL bootstraps in tests) before migrations existed. For migrations
that add a column that already exists (e.g., V2 adds `name` to notes when the
store DDL already includes it), the runner detects the existing column via
`pragma_table_info` and records the migration as applied without re-running its
`ALTER TABLE`.

### Pack schema declarations

Pack-auxiliary tables: tables a pack needs but that do not belong to the core
substrate: are declared by the pack and applied at runtime startup through a
`SchemaPlan`.

`SchemaPlan` statements use idempotent DDL such as `CREATE TABLE IF NOT EXISTS` and
`CREATE INDEX IF NOT EXISTS`. The runtime applies each loaded pack's plan to its
assigned backend. If a deployed auxiliary table needs a shape change, that evolution
uses a coordinated versioned migration rather than boot-time conditional DDL.

### Note kinds and edge relations do not require migrations

Pack-registered note kinds (ADR-013) and pack-extensible edge endpoint rules
(ADR-017, Pack Standard `const EDGE_RULES`) do not require schema migrations. They store data in the existing
`notes` and `graph_edges` tables. New kinds are validated values, not schema changes.

The same applies to entity_type values (ADR-001): the `entity_type` column is
already in the schema; new values flow through `EntityTypeRegistry` validation,
not migrations.

### Down migrations: not supported

Forward-only. Down migrations require either:

1. Inverse SQL written for every migration (doubles maintenance burden), or
2. Generic transaction rollback (can't undo a committed migration), or
3. Time-travel via versioning (the snapshot mechanism in ADR-010).

khive picks option 3 as the rollback story. To rollback a schema change in
production: restore from a git snapshot (entities + edges) and replay events
to the target point. For ad-hoc rollback during development: drop the database
and re-migrate (the dev environment is regeneratable).

The cost is real: a migration that ships a bug requires either a forward
migration to fix it or a full restore. The benefit is one fewer thing to
maintain (down SQL) and one fewer way the migration system can be wrong.

### Partial rollback within a multi-statement migration

A migration whose `up` contains multiple statements runs as one transaction. If
the fifth `ALTER TABLE` of ten fails, all five preceding statements roll back
with the transaction; the DB stays at the prior version.

There is no within-migration partial rollback (e.g., "apply statements 1-4 even
though 5 failed"). Migrations are atomic units.

### In-flight schema changes

SQLite serializes writers via its own locking. A migration acquires the writer
lock for the duration of its transaction; concurrent writers wait or fail with
`SQLITE_BUSY`. Long-running queries during a migration are handled by SQLite's
own concurrency model.

Pool-level coordination (`ConnectionPool` in `khive-db`) ensures that migrations
run before any other writer claims the lock. The `kkernel db migrate` command
runs to completion before any service connections accept writes.

### Schema diagnostics

`kkernel db check` reports per-backend schema state without applying changes:

```text
$ kkernel db check
main:    V12 (current)
corpus:  V10 (behind: V11, V12 pending)
archive: V12 (current)
```

`kkernel db check --strict` exits nonzero if any backend is behind. CI uses
this to verify migrations are current before deployment.

## Rationale

### Why versioned migrations (not `PRAGMA user_version`)?

`PRAGMA user_version` stores one integer in the database header. It works but:

- No audit trail. Can't answer "what migration ran on this DB, when?"
- No name. A version integer doesn't tell a maintainer what V7 actually does.
- No multi-statement atomicity guarantee. SQLite applies the pragma but doesn't
  bind it to a transaction.

A dedicated `_schema_migrations` table costs ~16 bytes per migration row and
gives full audit history with timestamps and names.

### Why one migration per transaction (not all in one)?

Running every migration in one transaction means a V8 failure rolls back V5–V7
that already applied cleanly. The next attempt would re-run all of them,
wasting work and potentially exposing the V5 statements to a partially-modified
schema state.

Per-migration transactions are bounded and recoverable. V5 applies cleanly and
stays applied; V8 fails and the DB stops at V7. The next run retries from V8.

### Why forward-only?

Inverse migrations double the maintenance burden: every `ALTER TABLE ADD
COLUMN` needs an `ALTER TABLE DROP COLUMN` counterpart, every data migration
needs a reverse data migration, every index addition needs the drop. Most teams
that write down migrations never run them; the few times they need rollback,
they use a backup.

khive's snapshot mechanism (ADR-010) provides the rollback story. Down
migrations would be a parallel mechanism with its own bugs.

### Why two mechanisms (migrations vs SchemaPlan)?

Core substrate tables (`entities`, `graph_edges`, `notes`, `events`) are
fundamental to khive's data model. They evolve carefully through versioned
migrations.

Pack-auxiliary tables belong to the pack that introduces them. Forcing every pack to ship
versioned migrations couples pack evolution to `khive-db` releases and adds
governance overhead disproportionate to the value.

Idempotent `CREATE TABLE IF NOT EXISTS` is the default tool for pack-auxiliary
tables: they appear when the pack loads. Structural changes to deployed tables still
belong in versioned migrations.

### Why migration application is operator-context, not agent-context?

Migrations are operationally significant. A pack that auto-migrates on startup
can corrupt data if the migration has a bug: and the bug isn't noticed until
the agent makes a call that exercises the corrupted state. An operator running
`kkernel db migrate` deliberately has the option to dry-run, check, and stage
the change.

The operator can run migrations in a CI/CD pipeline, in a maintenance window,
or after taking a backup. Auto-apply at startup forfeits all of these options.

### Why per-backend independent state?

Multi-backend deployments (ADR-009) have backends with different lifecycles. A
backup-restored `archive.db` might be at V4 when the rest of the deployment is
at V7. Running `kkernel db migrate` brings the restored backend current. If all
backends shared one version, restoring `archive.db` would force a global
rollback or break the system.

Per-backend state isolates these cases. Each backend advances independently;
the codebase's migration set is global, but applied state is per-file.

## Alternatives Considered

| Alternative                                      | Why rejected                                                                                 |
| ------------------------------------------------ | -------------------------------------------------------------------------------------------- |
| `PRAGMA user_version` only                       | No audit trail, no names, no migration history.                                              |
| `CREATE TABLE IF NOT EXISTS` only                | Cannot evolve schema (no ALTER), no ordering, no audit.                                      |
| External migration tool (sqlx-migrate, refinery) | Adds a heavy dependency and duplicates the existing trait-oriented migration implementation. |
| Down migrations + reversal                       | Doubles maintenance; snapshots cover the use case.                                           |
| Auto-apply migrations at startup                 | Operationally dangerous; no dry-run, no staging.                                             |
| Single global version across all backends        | Breaks under multi-file federation with independent backend lifecycles.                      |
| Pack-owned versioned migrations in v1            | Adds machinery for an unproven case; pack tables work via idempotent CREATE IF NOT EXISTS.   |
| All migrations in one transaction                | A single failure rolls back all prior migrations; wasted work and recovery confusion.        |

## Consequences

### Positive

- Migration history is queryable: `SELECT * FROM _schema_migrations`.
- Per-migration transactions: failure leaves the DB at the prior version.
- Operator-context execution: deliberate, scriptable, dry-runnable.
- Multi-backend support: each SQLite file advances independently.
- Pack tables stay separate from core schema: packs don't need to coordinate
  with `khive-db` releases for their own tables.
- Forward-only keeps the maintenance burden bounded; rollback is the snapshot
  mechanism's job.

### Negative

- No down migrations. Rollback requires snapshot restore.
  Mitigated: ADR-010 versioning is the rollback story; the dev path is drop+remigrate.
- Operator must run `kkernel db migrate` before serving traffic.
  Mitigated: `kkernel db check --strict` integrates with CI; documented in
  deployment guide.
- Pack tables can't evolve through `ALTER` in v1.
  Mitigated: deferred until a concrete pack use case justifies the machinery.
- A buggy migration can lock progress until fixed and shipped as a new version.
  Mitigated: same as any forward-only migration system; standard CI practices
  catch this.

### Neutral

- Migration runner detects already-applied columns (V2 of notes.name) and
  records the migration as applied without re-running its `ALTER`. This handles
  databases bootstrapped via in-process schema before migrations existed.
- `_schema_migrations` table is created lazily on first `run_migrations` call;
  it is not part of V1.
- In-memory databases auto-migrate on creation (no operator to invoke
  `kkernel db migrate`).

## Implementation

- `crates/khive-db/sql/`:
  - `schema.sql`: the full V1 baseline schema, included via `include_str!`.
  - V2 through V12 each use a numbered SQL file; future migrations append the same way.
- `crates/khive-db/src/migrations.rs`:
  - `VersionedMigration` struct; `up` sourced from a `.sql` file via `include_str!`.
  - `MIGRATIONS: &[VersionedMigration]`: contiguous, append-only.
  - `run_migrations(conn)`: applies all unapplied migrations in order.
  - `MIGRATION_TRACKING_TABLE` DDL for `_schema_migrations`.
- `scripts/lint-sql.sh`:
  - Executes every `crates/**/*.sql` against an in-memory SQLite database and
    checks hygiene. Wired into `scripts/ci.sh` and `.pre-commit-config.yaml`.
- `crates/kkernel/src/db.rs` (or similar subcommand module):
  - `kkernel db migrate [--backend <name>] [--dry-run] [--check]`.
  - `kkernel db check [--strict]`.
- `crates/khive-runtime/src/runtime.rs`:
  - In-memory runtime constructors call `BackendMigrator::migrate` on creation.
  - File-backed `KhiveRuntime::new(config)` verifies migration state at
    startup; fails fast if behind.
- `crates/khive-storage/src/migrations.rs`:
  - `BackendMigrator` defines `migrate()` and `current_version()` without exposing a
    database-driver type to the runtime.
- `khive-db::SqliteMigrator` implements `BackendMigrator` over its connection pool and
  delegates migration application to `run_migrations`.
- `khive-storage::Pack` trait (ADR-017, Pack Standard): adds `fn schema_plan(&self) ->
  SchemaPlan` for pack-auxiliary tables. Applied during pack registration.

## References

- ADR-001: Entity Kind Taxonomy: `entity_type` column added via migration.
- ADR-003: System Architecture: `kkernel db migrate` operator command.
- ADR-004: Substrate Observables: Link namespace/timestamp columns added via
  migration; NoteKindSpec lifecycle data stored in pack-auxiliary tables.
- ADR-005: Storage Capability Traits: `SparseStore` table added via migration.
- ADR-009: Backend Architecture: multi-file federation; per-backend migration state.
- ADR-010: KG Versioning: snapshot mechanism is the rollback story.
- ADR-013: Note Kind Taxonomy: pack-registered kinds don't require schema
  changes (rows in existing `notes` table).
- ADR-017: Pack Standard: `SchemaPlan` trait for pack-auxiliary tables.
- ADR-017: Pack Standard (§EDGE_RULES): endpoint rules don't require migrations.
- ADR-071: Backend-Pluggable Runtime: `BackendMigrator` trait.
