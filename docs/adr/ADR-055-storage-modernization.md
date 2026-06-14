# ADR-055: Storage Modernization (Integer FK, Cascade, Normalized Schema)

**Status**: Proposed
**Date**: 2026-06-13
**Origin**: khivedb storage comparison (salvage)

## Context

Head-to-head comparison of khive-db and khivedb-sqlite shows khivedb has
better schema engineering: integer FK for namespace isolation (vs string PK),
ON DELETE CASCADE at the schema level (vs application-level soft-delete only),
normalized records table with substrate/kind columns (vs per-feature tables),
and 3x test density.

khive-db uses string namespace PKs, no foreign key cascade, and 8 specialized
tables (entities, notes, events, etc.) plus knowledge pack tables. This works
but creates coupling between namespace strings and every query, and makes hard
delete unreliable (edges can dangle).

## Decision

Modernize khive-db schema in a new migration (V-next, not editing V1):

### Changes

1. **Integer namespace ID**: Add `namespace_id INTEGER` FK column to tables
   that currently use `namespace TEXT`. Resolve namespace strings to IDs at
   the runtime boundary, not in every SQL query.

2. **ON DELETE CASCADE**: Add foreign key constraints from edges, vectors,
   memory_meta to their parent records. Hard delete of an entity automatically
   cleans up its incident edges and vectors.

3. **UUID column**: Add `uuid BLOB(16)` to records for stable external
   identifiers (vs text-based ID generation). Enables `record_resolve_uuid()`
   for cross-system references.

4. **Partial indexes on live edges**: `CREATE INDEX idx_edges_source ON edges
   (source_id) WHERE deleted_at IS NULL` — only index live edges, not
   tombstoned ones.

### What does NOT change

- Per-feature tables (entities, notes, events) remain. khivedb's single
  `records` table is cleaner but khive's per-feature tables are load-bearing
  for the knowledge pack (atoms, domains, sections) and event sourcing.
- FTS5 trigram tokenizer stays (khive's trigram is better than khivedb's
  default tokenizer for CJK and partial matching).
- khive-storage traits remain the abstraction boundary.
- Soft delete semantics are preserved; cascade applies to hard delete only.

### Migration strategy

- New migration adds integer namespace_id + FK constraints + UUID column.
- Backfill existing data in the migration.
- Runtime resolves namespace string to ID once per connection, carries the
  integer through all queries.
- No breaking change to khive-storage traits (the trait surface is ID-based
  already).

## Consequences

- Namespace isolation enforced at schema level (FK), not just runtime.
- Hard delete is reliable (cascade cleans up edges/vectors/meta).
- Query performance improves (integer comparison vs string comparison).
- Migration is one-time; existing databases upgrade transparently.
- ~150 LOC migration SQL + ~100 LOC runtime namespace resolution.
