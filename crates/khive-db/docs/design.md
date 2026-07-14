# khive-db Design

## ADR Compliance

### Graph Edge Routing (ADR-009)

- `graph_edges` carries a `target_backend` column added in V9 that enables
  backend-specific routing for edge traversal.
- On conflict (duplicate source/target/relation triple), the upsert uses
  `ON CONFLICT ... DO UPDATE` to refresh weight/metadata on the existing row.

### ADR-013: Note Kind Taxonomy

- The FTS5 trigram tokenizer is used by default because it handles CJK text
  correctly without whitespace-based tokenization. All `text()` and
  `text_with_tokenizer()` backends default to `trigram`.

### Schema Migration System (ADR-015)

- `migrations.rs` contains all versioned DDL in a single file — splitting
  across files would make migration sequencing harder to verify.
- Migrations are forward-only, applied in version order, each in its own
  transaction. V1 is immutable.
- Legacy `ServiceSchemaPlan`/`apply_schema_plan` API preserved for
  backward compatibility. New schema changes use the versioned `MIGRATIONS`
  array.
- V6/V7/V8 are frozen no-op slots; their `name` strings appear in the
  production `_schema_migrations` table and must not change.

### Pack Standard — Pack-Auxiliary Schema (ADR-017)

- `apply_pack_ddl_statements` runs pack DDL idempotently without version
  tracking. Pack auxiliary tables use `CREATE TABLE IF NOT EXISTS` and are
  not recorded in `_schema_versions`.
- The `SchemaPlan` type lives in `khive-runtime` (above this crate); this
  method accepts `&[&'static str]` to avoid a circular dependency.

### SparseStore (ADR-031)

- `stores/sparse.rs` implements the SQLite-backed `SparseStore` trait.

### Embedding Model Registry (ADR-043)

- `_embedding_models` table (created in V14) tracks which embedding model
  is active per vector engine with a canonical key for deduplication.
- `EMBEDDING_MODELS_DDL` is shared between the V14 migration and the
  belt-and-suspenders creation in `StorageBackend::vectors_for_namespace`
  so the schema cannot silently diverge.
- sqlite-vec virtual tables (`vec0`) do not support `ALTER TABLE ADD COLUMN`;
  the startup backfill rebuild handles them after migrations complete.
- V16 adds `embedding_model` column to regular `vec_*` tables; V17 performs
  a preserving rebuild of vec0 virtual tables to add the same column without
  data loss.

### Old-Schema Vec0 Detection (ADR-044)

- At vector store open time, `pragma_table_info` inspects whether the `field`
  column exists. Tables predating the field column are flagged with an error
  after V17 (the silent-drop path was removed in V17).

### Event-Sourced Proposals (ADR-046)

- V15 creates `proposals_open`, a fold-derived projection of proposal events
  that makes `list(kind=proposal, status="open")` an index scan.
- V18 adds `'applying'` to the `proposals_open` status CHECK constraint to
  handle the apply/withdraw race condition.

### Entity Domain Filter Case Sensitivity (ADR-047)

- The tags/domain filter in `SqlEntityStore` normalizes values to lowercase
  before comparison so that domain filtering is case-insensitive.

### Brain Pack + Knowledge Sections (ADR-048)

- V20 creates `brain_profile_snapshots` and `brain_event_log` tables for
  the brain pack (Phase 1).
- V21 creates `knowledge_sections` with a 10-value SectionType enum, FK to
  `knowledge_atoms`, and UNIQUE(atom_id, section_type) (Phase 2).

### Daemon & Warm Startup (ADR-049)

- V22 extends `knowledge_atoms`, `knowledge_sections`, and `knowledge_domains`
  with a `status` column (NOT NULL DEFAULT 'draft'), plus `source_uri` and
  `source_type` provenance columns on atoms. Indexes accelerate
  status-filtered list/search paths. Existing finalized atoms are backfilled
  to `'reviewed'`.

### Single-Writer Write Queue (ADR-067 Component A)

Multiple stores and namespaces can be constructed over the same
`ConnectionPool` (per DB file), but every mutating statement must still
serialize through exactly one writer connection — otherwise concurrent
stores would open independent connections that contend with each other at
`BEGIN IMMEDIATE`, defeating the purpose of a write queue. `ConnectionPool`
lazily spawns a single `WriterTask` behind a `OnceLock`: the first caller to
need it runs the init closure, every later caller (from any store, any
namespace) receives a clone of the same handle. When `KHIVE_WRITE_QUEUE=1`,
store methods route single-row DML through this shared `WriterTask` instead
of taking the pool mutex directly; `orphan_sweep` and other closures that
manage their own transaction bypass the queue via the "unmanaged" path
instead, since a transaction-owning closure cannot be sent through a channel
that already wraps every request in its own transaction.

See `crates/khive-db/docs/api/pool.md` and `crates/khive-db/docs/api/vectors.md`
for the per-function routing rules and the tests that pin them down.

## Consistency Notes

- **sqlite-vec KNN non-monotonicity** (`stores/vectors.rs`): The IN-subquery
  approach for namespace-scoped KNN can produce non-monotonic results. Tracked
  in MEMORY.md under `project_sqlite_vec_knn_bug.md`.

- **`embedding_coverage` stat hardcoded**: `stats()` reports
  `embedding_coverage: 0.0` regardless of actual indexed vector count. This is
  a known lie in the stats implementation, not a data issue.

Last reviewed: 2026-06-06
