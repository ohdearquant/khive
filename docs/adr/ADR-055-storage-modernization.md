# ADR-055: Storage Modernization -- Integer Namespace FK, Cascade Delete, Stable UUIDs, Live-Edge Indexes

**Status**: Proposed
**Date**: 2026-06-13

## Context

khive-db (~2.8K Rust + ~0.5K SQL) is the SQLite backend behind the storage traits
([ADR-005](ADR-005-storage-capability-traits.md)). It works, and several of its choices are
deliberately good: FTS5 trigram tokenization (strong for CJK and partial matching), sqlite-vec
vectors, recursive-CTE traversal, and the per-feature table layout that the knowledge pack
(atoms, domains, sections) and event sourcing depend on. Those stay.

Four schema-engineering weaknesses are worth a migration:

1. **String namespace coupling.** Tenancy is carried as a `namespace TEXT` value threaded through
   queries. String comparison is slower than integer, and a `TEXT` namespace cannot be a foreign
   key, so the database cannot enforce referential integrity on tenancy.
2. **Hard delete is unreliable.** There is no `ON DELETE CASCADE` from edges / vectors /
   memory-meta to their parent records. Hard-deleting an entity relies on application code
   remembering to clean up incident rows; a missed path leaves dangling edges or orphaned
   vectors. ([ADR-002](ADR-002-edge-ontology.md) and the CLAUDE.md "Edge cascade" rule require
   "no dangling references" on hard delete -- today that is an application invariant, not a schema
   one.)
3. **No stable external identifier.** Records lack a dedicated UUID column with a uniqueness
   constraint scoped to the namespace, which makes cross-system references and idempotent
   re-ingest awkward.
4. **Indexes cover dead rows.** Edge indexes include soft-deleted (`deleted_at IS NOT NULL`)
   rows, so live-edge lookups scan tombstones that every query then filters out.

This ADR proposes a single additive migration that fixes the four, while explicitly **not**
touching the parts of khive-db that are load-bearing or already correct.

## Decision

A new `VersionedMigration` (`version = <last + 1>`) with DDL authored in a new
`crates/khive-db/sql/NNN-storage-modernization.sql` file pulled in via `include_str!`
([ADR-015](ADR-015-schema-migrations.md)) -- V1 is never edited. The migration is lintable by
`scripts/lint-sql.sh` (loads every `.sql` into an in-memory SQLite db in `make ci`).

### 1. Integer namespace foreign key

Add a `namespaces` dimension table and a `namespace_id INTEGER` FK to tenant-scoped tables, in
addition to (not replacing, in the migration step) the existing `namespace TEXT`:

```sql
CREATE TABLE namespaces (
    id   INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE
);
-- backfill one row per distinct existing namespace string, then:
ALTER TABLE records ADD COLUMN namespace_id INTEGER
    REFERENCES namespaces(id) ON DELETE RESTRICT;
-- UPDATE records SET namespace_id = (SELECT id FROM namespaces WHERE name = records.namespace);
```

The runtime resolves a namespace string to a `NamespaceId` **once per connection** and carries
the integer through queries, instead of comparing strings on every row.

```rust
pub struct NamespaceId(pub i64);   // bridges the SQL integer FK to the string `Namespace` type
```

`ON DELETE RESTRICT` on the namespace FK prevents deleting a namespace that still owns records --
namespace teardown is a deliberate, separate operation, never an accident.

### 2. ON DELETE CASCADE for incident rows

Foreign keys from child rows to their parent record, with cascade, so a **hard** delete cleans up
incident rows in one statement and the schema -- not application code -- guarantees no dangling
references:

```sql
-- edges reference their endpoints; vectors and memory-meta reference their owning record
... source_id INTEGER REFERENCES records(id) ON DELETE CASCADE
... target_id INTEGER REFERENCES records(id) ON DELETE CASCADE
... record_id INTEGER REFERENCES records(id) ON DELETE CASCADE   -- vectors, memory_meta
```

**Soft delete semantics are unchanged.** Cascade applies to hard delete only; soft delete still
sets `deleted_at` and leaves rows in place for the view layer to filter
([CLAUDE.md](../../CLAUDE.md) "Data vs. view"). `PRAGMA foreign_keys = ON` must be set per
connection for SQLite to honor the cascade -- the migration and the connection setup both assert
it.

### 3. Stable UUID column

```sql
ALTER TABLE records ADD COLUMN uuid BLOB(16);
-- backfill: generate a UUID for every existing record
CREATE UNIQUE INDEX idx_records_ns_uuid ON records(namespace_id, uuid);
```

A 16-byte `BLOB` UUID (not a 36-char text UUID) with `UNIQUE(namespace_id, uuid)` gives a stable,
namespace-scoped external identifier and enables a `record_resolve_uuid()` lookup for
cross-system references and idempotent re-ingest.

### 4. Partial indexes on live edges

```sql
CREATE INDEX idx_edges_source ON edges(source_id) WHERE deleted_at IS NULL;
CREATE INDEX idx_edges_target ON edges(target_id) WHERE deleted_at IS NULL;
```

Live-edge lookups (neighbors, traverse) are the hot path; partial indexes keep the index over
live rows only, smaller and faster, and skip the soft-deleted rows the query filters out anyway.

### What does NOT change

- **Per-feature tables stay.** entities / notes / events and the knowledge-pack tables (atoms,
  domains, sections) carry real structure and event-sourcing semantics. Collapsing them into a
  single normalized `records` table is a larger, higher-risk decision with its own tradeoffs;
  it is explicitly **out of scope** here and would need its own ADR. (Where KQL engine
  statements -- [ADR-052](ADR-052-khiveql-integration.md) -- need a unified
  `record_list_filtered` / `record_counts_by_kind` surface, that is satisfied by a view over the
  per-feature tables, not by a schema rewrite.)
- **FTS5 trigram tokenizer stays.** It is better than the SQLite default for CJK and partial
  matching, which khive's multilingual corpus needs.
- **Narrow FTS triggers stay narrow.** FTS sync triggers fire only on the FTS-indexed columns
  (`UPDATE OF name, data`), never on embedding or metadata columns -- a hard-won discipline
  (broad triggers caused WAL bloat and corruption). The migration preserves and documents this;
  it does not widen any trigger.
- **khive-storage traits are unchanged.** The trait surface is already ID-based, so integer FKs
  and cascade are below the abstraction boundary. No consumer code changes.

### Migration strategy

1. Add `namespaces` + backfill from distinct `namespace` strings.
2. Add `namespace_id`, `uuid`, FK constraints, and partial indexes; backfill `namespace_id` and
   `uuid` for existing rows in the same migration.
3. Set `PRAGMA foreign_keys = ON` in connection setup; assert it in a migration test.
4. Runtime resolves `namespace` string -> `NamespaceId` once per connection; threads the integer.
5. (Follow-up, optional) once all reads/writes use `namespace_id`, a later migration may drop the
   redundant `namespace TEXT` -- a separate version, never by editing this one.

## Consequences

- Tenancy referential integrity is enforced by the schema (FK), not only by runtime checks
  (which remain -- see [ADR-054](ADR-054-authorization-gate.md)).
- Hard delete is reliable: cascade removes incident edges, vectors, and memory-meta in one
  statement, satisfying the "no dangling references" rule at the schema level.
- Namespace comparisons become integer comparisons; live-edge lookups hit partial indexes.
- A stable `BLOB(16)` UUID per record enables cross-system references and idempotent re-ingest.
- Scope is deliberately bounded: ~150 LOC migration SQL + ~100 LOC runtime namespace resolution.
  The records-table normalization, the `namespace TEXT` drop, and any tokenizer change are each
  separate future decisions, not bundled here.
- Existing databases upgrade in one forward migration; no V1 edit, lint-clean `.sql`,
  trigger discipline preserved.
