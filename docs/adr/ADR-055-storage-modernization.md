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

Each of the seven tenant-scoped tables carries its own `namespace TEXT` column
(`knowledge_atoms`, `knowledge_domains`, and `knowledge_sections` each declare one in
`schema.sql`), so every one of them contributes to the backfill and receives a `namespace_id`
column:

```sql
CREATE TABLE namespaces (
    id   INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL UNIQUE
);

-- One row per distinct namespace already in use, across every tenant-scoped table:
INSERT OR IGNORE INTO namespaces(name)
    SELECT DISTINCT namespace FROM entities
    UNION SELECT DISTINCT namespace FROM notes
    UNION SELECT DISTINCT namespace FROM events
    UNION SELECT DISTINCT namespace FROM graph_edges
    UNION SELECT DISTINCT namespace FROM knowledge_atoms
    UNION SELECT DISTINCT namespace FROM knowledge_domains
    UNION SELECT DISTINCT namespace FROM knowledge_sections;

-- graph_edges receives namespace_id inside the section 2 table rebuild, not here.
ALTER TABLE entities           ADD COLUMN namespace_id INTEGER REFERENCES namespaces(id) ON DELETE RESTRICT;
ALTER TABLE notes              ADD COLUMN namespace_id INTEGER REFERENCES namespaces(id) ON DELETE RESTRICT;
ALTER TABLE events             ADD COLUMN namespace_id INTEGER REFERENCES namespaces(id) ON DELETE RESTRICT;
ALTER TABLE knowledge_atoms    ADD COLUMN namespace_id INTEGER REFERENCES namespaces(id) ON DELETE RESTRICT;
ALTER TABLE knowledge_domains  ADD COLUMN namespace_id INTEGER REFERENCES namespaces(id) ON DELETE RESTRICT;
ALTER TABLE knowledge_sections ADD COLUMN namespace_id INTEGER REFERENCES namespaces(id) ON DELETE RESTRICT;

-- Backfill namespace_id from existing namespace TEXT:
UPDATE entities           SET namespace_id = (SELECT id FROM namespaces WHERE name = entities.namespace);
UPDATE notes              SET namespace_id = (SELECT id FROM namespaces WHERE name = notes.namespace);
UPDATE events             SET namespace_id = (SELECT id FROM namespaces WHERE name = events.namespace);
UPDATE knowledge_atoms    SET namespace_id = (SELECT id FROM namespaces WHERE name = knowledge_atoms.namespace);
UPDATE knowledge_domains  SET namespace_id = (SELECT id FROM namespaces WHERE name = knowledge_domains.namespace);
UPDATE knowledge_sections SET namespace_id = (SELECT id FROM namespaces WHERE name = knowledge_sections.namespace);
-- graph_edges.namespace_id is backfilled by the INSERT ... SELECT in the section 2 rebuild.
```

The runtime resolves a namespace string to a `NamespaceId` once per connection and carries the
integer through queries, instead of comparing strings on every row:

```rust
pub struct NamespaceId(pub i64);
```

`ON DELETE RESTRICT` prevents deleting a namespace that still owns records. Namespace teardown is
a deliberate, separate operation and must never be an accident.

### 2. ON DELETE CASCADE for incident graph_edges rows

Add `ON DELETE CASCADE` foreign keys from `graph_edges.source_id` and `graph_edges.target_id`
to `entities(id)`, so a hard delete of an entity removes its incident edges in one database
statement rather than relying on application code.

`graph_edges.source_id` and `graph_edges.target_id` are already `TEXT NOT NULL` columns whose
values are entity UUIDs. SQLite supports a foreign key that references a `TEXT PRIMARY KEY`
directly, so the foreign key targets the existing `entities(id)` column. No integer surrogate is
introduced: `entities.id` is the primary key and is the correct, unique parent column for the
reference. (An earlier draft proposed a `rowid_int INTEGER` surrogate and referenced it. That
column carries no uniqueness constraint, so SQLite rejects the foreign key with
`foreign key mismatch - graph_edges referencing entities`. The surrogate is therefore removed.)

SQLite cannot add a foreign key to an existing table with `ALTER TABLE`, so this change uses the
standard SQLite table-rebuild procedure: create a new table that carries the foreign keys, copy
the rows, drop the old table, and rename. The rebuild preserves the existing
`PRIMARY KEY (namespace, id)` and every existing column.

```sql
-- Foreign keys cannot be added by ALTER TABLE, so rebuild graph_edges.
-- Wrap the rebuild in a transaction with foreign keys disabled, per the
-- SQLite-documented table-modification procedure.
PRAGMA foreign_keys = OFF;
BEGIN;

CREATE TABLE graph_edges_new (
    namespace      TEXT NOT NULL,
    id             TEXT NOT NULL,
    source_id      TEXT NOT NULL,
    target_id      TEXT NOT NULL,
    relation       TEXT NOT NULL,
    weight         REAL NOT NULL DEFAULT 1.0,
    created_at     INTEGER NOT NULL,
    updated_at     INTEGER NOT NULL,
    deleted_at     INTEGER,
    metadata       TEXT,
    target_backend TEXT,
    namespace_id   INTEGER REFERENCES namespaces(id) ON DELETE RESTRICT,
    PRIMARY KEY (namespace, id),
    FOREIGN KEY (source_id) REFERENCES entities(id) ON DELETE CASCADE,
    FOREIGN KEY (target_id) REFERENCES entities(id) ON DELETE CASCADE
);

-- The old graph_edges has no namespace_id column (it is introduced by this
-- rebuild, not by ALTER), so resolve it from the namespace TEXT during the copy.
INSERT INTO graph_edges_new
    SELECT namespace, id, source_id, target_id, relation, weight,
           created_at, updated_at, deleted_at, metadata, target_backend,
           (SELECT id FROM namespaces WHERE name = graph_edges.namespace)
    FROM graph_edges;

DROP TABLE graph_edges;
ALTER TABLE graph_edges_new RENAME TO graph_edges;

COMMIT;
PRAGMA foreign_key_check;  -- must return no rows
PRAGMA foreign_keys = ON;
```

The rebuilt table includes the `namespace_id` column from section 1, so the namespace foreign key
is declared in the same rebuild rather than by a separate `ALTER TABLE` on `graph_edges`. The
live-edge indexes from section 4 and the existing `graph_edges` indexes are recreated after the
rename (an `ALTER TABLE ... RENAME` carries indexes forward, but a rebuild from a freshly created
table does not, so the index DDL is reissued).

`PRAGMA foreign_keys = ON` must be set per connection for SQLite to honor cascade at runtime;
without it the foreign key is recorded but never enforced. The connection setup and a migration
test both assert it.

**Soft delete semantics are unchanged.** Cascade applies to hard delete only. Soft delete still
sets `deleted_at` and leaves rows in place for the view layer to filter
([CLAUDE.md](../../CLAUDE.md) "Data vs. view"). The schema does not change soft-delete behavior.

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
`schema.sql:218-219`) are kept for queries that must reach soft-deleted rows. Because the section
2 rebuild creates `graph_edges` afresh, these indexes are reissued after the rename rather than
carried forward, and the partial indexes are added alongside them. The partial indexes supplement
the full-table indexes; they do not replace them.

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
   `entities`, `notes`, `events`, `graph_edges`, `knowledge_atoms`, `knowledge_domains`, and
   `knowledge_sections`.
2. Add `namespace_id INTEGER` to `entities`, `notes`, `events`, and the three knowledge pack
   tables via `ALTER TABLE ADD COLUMN ... REFERENCES namespaces(id)`; backfill each from its
   existing `namespace TEXT` column. (`graph_edges` receives `namespace_id` in step 3 instead,
   because it is rebuilt there.)
3. Rebuild `graph_edges` with the standard SQLite table-rebuild procedure to add the
   `ON DELETE CASCADE` foreign keys on `source_id` and `target_id` referencing `entities(id)`,
   plus its `namespace_id` column. Resolve `namespace_id` from the `namespace TEXT` column during
   the copy. Run `PRAGMA foreign_key_check` after the rebuild.
4. Create the namespace-scoped unique indexes on `entities` and `notes`.
5. Recreate the existing `graph_edges` indexes that the rebuild did not carry forward, then
   create the partial live-edge indexes on `graph_edges`.
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
