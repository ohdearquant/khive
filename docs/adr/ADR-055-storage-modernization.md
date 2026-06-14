# ADR-055: Storage Modernization -- Integer Namespace FK, Cascade Delete, Stable UUIDs, Live-Edge Indexes

**Status**: Proposed
**Date**: 2026-06-13

## Context

`khive-db` (~2.8K Rust + ~0.5K SQL) is the SQLite backend behind the storage traits
([ADR-005](ADR-005-storage-capability-traits.md)). Several of its choices are deliberately good:
FTS5 trigram tokenization (strong for CJK and partial matching), sqlite-vec vectors, recursive-CTE
traversal, and the per-feature table layout that the knowledge pack and event sourcing depend on.
Those stay.

The real core tables (verified in `crates/khive-db/sql/schema.sql`) are:

- `entities` -- entity records with `id TEXT PRIMARY KEY`, `namespace TEXT NOT NULL`, `kind TEXT`
- `graph_edges` -- edges with `PRIMARY KEY (namespace, id)`, `source_id TEXT`, `target_id TEXT`
- `notes` -- note records with `id TEXT PRIMARY KEY`, `namespace TEXT NOT NULL`, `kind TEXT`
- `events` -- audit and substrate events with `id TEXT PRIMARY KEY`, `namespace TEXT NOT NULL`
- `knowledge_atoms`, `knowledge_domains`, `knowledge_sections` -- knowledge pack tables

Four schema-engineering weaknesses are worth a migration:

1. **String namespace coupling.** Tenancy is carried as a `namespace TEXT` column in every
   table. String comparison is slower than integer, and a `TEXT` namespace column cannot be a
   foreign key, so the database cannot enforce referential integrity on tenancy.
2. **Hard delete is unreliable.** `graph_edges.source_id` and `graph_edges.target_id` reference
   `entities.id` only by convention. There is no `ON DELETE CASCADE` FK from `graph_edges` to
   `entities`, so hard-deleting an entity relies on application code to clean up incident edges.
   A missed path leaves dangling edge rows. ([ADR-002](ADR-002-edge-ontology.md) and the
   `CLAUDE.md` "Edge cascade" rule require "no dangling references" on hard delete; today that
   is an application invariant, not a schema one.)
3. **No stable external identifier.** `entities` and `notes` use a `TEXT PRIMARY KEY` (`id`)
   that is a UUID string, but there is no uniqueness constraint scoped to `(namespace, id)`.
   Cross-system references and idempotent re-ingest have no schema-enforced deduplication anchor.
4. **Indexes cover dead rows.** The edge indexes on `graph_edges`
   (`idx_graph_edges_ns_source`, `idx_graph_edges_ns_target`, and the composite indexes in
   `schema.sql:218-224`) include soft-deleted rows (`deleted_at IS NOT NULL`), so live-edge
   lookups scan tombstones that every query then filters out.

This ADR proposes a single additive migration that addresses the four weaknesses against the real
tables, while explicitly not touching the parts of `khive-db` that are load-bearing or correct.

## Decision

A new `VersionedMigration` (`version = <last + 1>`, currently version 4) with DDL authored in a
new `crates/khive-db/sql/004-storage-modernization.sql` file and referenced via `include_str!`
in `crates/khive-db/src/migrations.rs` ([ADR-015](ADR-015-schema-migrations.md)). V1 is never
edited. The migration is lintable by `scripts/lint-sql.sh`.

### 1. Integer namespace foreign key

Add a `namespaces` dimension table and a `namespace_id INTEGER` column to the tenant-scoped
tables (`entities`, `notes`, `events`, `graph_edges`, `knowledge_atoms`, `knowledge_domains`,
`knowledge_sections`), alongside the existing `namespace TEXT` (not replacing it in this
migration):

```sql
CREATE TABLE namespaces (
    id   INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL UNIQUE
);

-- One row per distinct namespace already in use (backfill):
-- INSERT OR IGNORE INTO namespaces(name)
--     SELECT DISTINCT namespace FROM entities
--     UNION SELECT DISTINCT namespace FROM notes
--     UNION SELECT DISTINCT namespace FROM events
--     UNION SELECT DISTINCT namespace FROM graph_edges;

ALTER TABLE entities    ADD COLUMN namespace_id INTEGER REFERENCES namespaces(id) ON DELETE RESTRICT;
ALTER TABLE notes       ADD COLUMN namespace_id INTEGER REFERENCES namespaces(id) ON DELETE RESTRICT;
ALTER TABLE events      ADD COLUMN namespace_id INTEGER REFERENCES namespaces(id) ON DELETE RESTRICT;
ALTER TABLE graph_edges ADD COLUMN namespace_id INTEGER REFERENCES namespaces(id) ON DELETE RESTRICT;

-- Backfill namespace_id from existing namespace TEXT:
-- UPDATE entities    SET namespace_id = (SELECT id FROM namespaces WHERE name = entities.namespace);
-- UPDATE notes       SET namespace_id = (SELECT id FROM namespaces WHERE name = notes.namespace);
-- UPDATE events      SET namespace_id = (SELECT id FROM namespaces WHERE name = events.namespace);
-- UPDATE graph_edges SET namespace_id = (SELECT id FROM namespaces WHERE name = graph_edges.namespace);
```

The runtime resolves a namespace string to a `NamespaceId` once per connection and carries the
integer through queries, instead of comparing strings on every row:

```rust
pub struct NamespaceId(pub i64);
```

`ON DELETE RESTRICT` prevents deleting a namespace that still owns records. Namespace teardown is
a deliberate, separate operation and must never be an accident.

### 2. ON DELETE CASCADE for incident graph_edges rows

Add integer FK columns to `graph_edges` that reference `entities(rowid)` with cascade semantics,
so a hard delete of an entity removes its incident edges in one database statement rather than
relying on application code:

```sql
-- entities.rowid is the SQLite implicit integer primary key (accessible because
-- entities uses TEXT PRIMARY KEY, which is an alias for a TEXT column, so the
-- SQLite rowid is still present as an implicit integer row identifier).
-- A safer approach uses a dedicated integer surrogate on entities:
ALTER TABLE entities ADD COLUMN rowid_int INTEGER;
-- backfill: UPDATE entities SET rowid_int = rowid;
-- Then FK from graph_edges:
ALTER TABLE graph_edges ADD COLUMN source_rowid INTEGER REFERENCES entities(rowid_int) ON DELETE CASCADE;
ALTER TABLE graph_edges ADD COLUMN target_rowid INTEGER REFERENCES entities(rowid_int) ON DELETE CASCADE;
-- Backfill source_rowid and target_rowid from source_id / target_id:
-- UPDATE graph_edges SET source_rowid = (SELECT rowid_int FROM entities WHERE id = graph_edges.source_id);
-- UPDATE graph_edges SET target_rowid = (SELECT rowid_int FROM entities WHERE id = graph_edges.target_id);
```

`PRAGMA foreign_keys = ON` must be set per connection for SQLite to honor cascade. The migration
and connection setup both assert it.

**Soft delete semantics are unchanged.** Cascade applies to hard delete only. Soft delete still
sets `deleted_at` and leaves rows in place for the view layer to filter
([CLAUDE.md](../../CLAUDE.md) "Data vs. view"). The schema does not change soft-delete behavior.

**Implementer note.** SQLite's `ALTER TABLE ... ADD COLUMN` does not support adding a FK column
that references a nullable column directly in older SQLite versions. If the SQLite version in use
does not support this form, the migration must use the SQLite-recommended table-rebuild pattern
(CREATE new table, INSERT SELECT, DROP old, RENAME). This is a migration-implementation detail,
not a design question.

### 3. Stable namespace-scoped UUID index

`entities` and `notes` already use a UUID string as `id TEXT PRIMARY KEY`. The missing piece is
a unique constraint scoped to `(namespace, id)` so cross-system references cannot silently
collide across namespaces:

```sql
-- Uniqueness within a namespace (separate from PRIMARY KEY uniqueness across the table):
CREATE UNIQUE INDEX IF NOT EXISTS idx_entities_ns_id ON entities(namespace, id);
CREATE UNIQUE INDEX IF NOT EXISTS idx_notes_ns_id    ON notes(namespace, id);
```

This enables a `resolve_by_ns_id()` lookup that is namespace-scoped and idempotent for
re-ingest without touching the existing `id TEXT PRIMARY KEY`.

### 4. Partial indexes on live edges

Replace the full-table edge indexes with partial indexes covering only live rows, so live-edge
lookups (neighbors, traverse) hit a smaller, faster index:

```sql
CREATE INDEX IF NOT EXISTS idx_graph_edges_live_source
    ON graph_edges(namespace, source_id)
    WHERE deleted_at IS NULL;

CREATE INDEX IF NOT EXISTS idx_graph_edges_live_target
    ON graph_edges(namespace, target_id)
    WHERE deleted_at IS NULL;
```

The existing full-table indexes (`idx_graph_edges_ns_source`, `idx_graph_edges_ns_target` in
`schema.sql:219-220`) remain in place for queries that must reach soft-deleted rows. The partial
indexes are additive, not replacements.

### What does NOT change

- **Per-feature tables stay as-is.** `entities`, `notes`, `events`, `graph_edges`, and the
  knowledge pack tables (`knowledge_atoms`, `knowledge_domains`, `knowledge_sections`) carry real
  structure and event-sourcing semantics. Collapsing them into a normalized single-record table
  is a larger, higher-risk decision with its own tradeoffs. It is explicitly out of scope here
  and would need its own ADR. A unified view surface (e.g. `record_counts_by_kind`) can be
  satisfied by a SQL view over the per-feature tables without a schema rewrite.
- **FTS5 trigram tokenizer stays.** It is better than the SQLite default for CJK and partial
  matching, which khive's multilingual corpus needs.
- **Narrow FTS triggers stay narrow.** FTS sync triggers fire only on the FTS-indexed columns
  (`UPDATE OF name, data`), never on embedding or metadata columns. Broad triggers caused WAL
  bloat and corruption previously. The migration preserves and documents this and does not widen
  any trigger.
- **khive-storage traits are unchanged.** The trait surface is already ID-based, so integer FKs
  and cascade are below the abstraction boundary. No consumer code changes.

### Migration strategy

1. Create the `namespaces` table, backfill one row per distinct `namespace` string present in
   `entities`, `notes`, `events`, and `graph_edges`.
2. Add `namespace_id INTEGER` to `entities`, `notes`, `events`, `graph_edges`, and the
   knowledge pack tables; backfill from the existing `namespace TEXT` column.
3. Add `rowid_int` to `entities`; backfill; add FK columns to `graph_edges`; backfill.
4. Create the namespace-scoped unique indexes on `entities` and `notes`.
5. Create the partial live-edge indexes on `graph_edges`.
6. Assert `PRAGMA foreign_keys = ON` in connection setup and in a migration test.
7. (Follow-up, optional) once all reads and writes use `namespace_id`, a later migration may
   drop the redundant `namespace TEXT` columns. That is a separate version, never an edit to
   this one.

## Consequences

- Tenancy referential integrity is enforced by the schema (`ON DELETE RESTRICT` on the namespace
  FK), not only by runtime checks (which remain; see [ADR-054](ADR-054-authorization-gate.md)).
- Hard delete of an entity cascades to its incident `graph_edges` rows in one statement,
  satisfying the "no dangling references" rule at the schema level.
- Namespace comparisons become integer comparisons for queries that use `namespace_id`.
- Live-edge lookups hit partial indexes, skipping soft-deleted rows the query filters out anyway.
- A `UNIQUE(namespace, id)` index per core table enables namespace-scoped idempotent re-ingest.
- Scope is deliberately bounded: one new `.sql` migration file (approximately 60-80 LOC) plus
  runtime `NamespaceId` resolution (~50 LOC). The records-table normalization, the `namespace
  TEXT` drop, and any tokenizer change are each separate future decisions.
- Existing databases upgrade in one forward migration. V1 is untouched. The `.sql` file is
  lint-clean and tested by `scripts/lint-sql.sh`.
