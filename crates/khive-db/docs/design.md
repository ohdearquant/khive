# khive-db Design

## ADR Compliance

### ADR-009: Backend Architecture - Graph Edge Routing

- `graph_edges` carries a `target_backend` column added in V9 that enables
  backend-specific routing for edge traversal.
- On conflict (duplicate source/target/relation triple), the upsert uses
  `ON CONFLICT ... DO UPDATE` to refresh weight/metadata on the existing row.

### ADR-013: Note Kind Taxonomy

- The FTS5 trigram tokenizer is used by default because it handles CJK text
  correctly without whitespace-based tokenization. All `text()` and
  `text_with_tokenizer()` backends default to `trigram`.

### ADR-015: Schema Migrations - Schema Migration System

- `migrations.rs` contains all versioned DDL in a single file — splitting
  across files would make migration sequencing harder to verify.
- Migrations are forward-only, applied in version order, each in its own
  transaction. V1 is immutable.
- Legacy `ServiceSchemaPlan`/`apply_schema_plan` API preserved for
  backward compatibility. New schema changes use the versioned `MIGRATIONS`
  array.
- V6/V7/V8 are frozen no-op slots; their `name` strings appear in the
  production `_schema_migrations` table and must not change.

### ADR-017: Pack Standard — Pack-Auxiliary Schema

- `apply_pack_ddl_statements` runs pack DDL idempotently without version
  tracking. Pack auxiliary tables use `CREATE TABLE IF NOT EXISTS` and are
  not recorded in `_schema_versions`.
- The `SchemaPlan` type lives in `khive-runtime` (above this crate); this
  method accepts `&[&'static str]` to avoid a circular dependency.

### ADR-031: Multi-Engine Retrieval - Embedder Trait, Registry, Configuration, and Pack Orchestration - SparseStore

- `stores/sparse.rs` implements the SQLite-backed `SparseStore` trait.

### ADR-043: Embedding Model Migration - Embedding Model Registry

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

### ADR-044: Vector Store Extensions - Capabilities, Metadata Filter, Batched Search, Update, Orphan Sweep - Old-Schema Vec0 Detection

- At vector store open time, `pragma_table_info` inspects whether the `field`
  column exists. Tables predating the field column are flagged with an error
  after V17 (the silent-drop path was removed in V17).

### ADR-046: Event-Sourced Agent KG Proposals - Event-Sourced Proposals

- V15 creates `proposals_open`, a fold-derived projection of proposal events
  that makes `list(kind=proposal, status="open")` an index scan.
- V18 adds `'applying'` to the `proposals_open` status CHECK constraint to
  handle the apply/withdraw race condition.

### ADR-047: Knowledge Pack - Entity Domain Filter Case Sensitivity

- The tags/domain filter in `SqlEntityStore` normalizes values to lowercase
  before comparison so that domain filtering is case-insensitive.

### ADR-048: Knowledge Section Profiles - Brain Pack + Knowledge Sections

- V20 creates `brain_profile_snapshots` and `brain_event_log` tables for
  the brain pack (Phase 1).
- V21 creates `knowledge_sections` with a 10-value SectionType enum, FK to
  `knowledge_atoms`, and UNIQUE(atom_id, section_type) (Phase 2).

### ADR-049: khived daemon - persistent warm runtime over a Unix socket - Daemon & Warm Startup

- V22 extends `knowledge_atoms`, `knowledge_sections`, and `knowledge_domains`
  with a `status` column (NOT NULL DEFAULT 'draft'), plus `source_uri` and
  `source_type` provenance columns on atoms. Indexes accelerate
  status-filtered list/search paths. Existing finalized atoms are backfilled
  to `'reviewed'`.

## Consistency Notes

- **sqlite-vec KNN non-monotonicity** (`stores/vectors.rs`): The IN-subquery
  approach for namespace-scoped KNN can produce non-monotonic results. Tracked
  in MEMORY.md under `project_sqlite_vec_knn_bug.md`.

- **`embedding_coverage` stat hardcoded**: `stats()` reports
  `embedding_coverage: 0.0` regardless of actual indexed vector count. This is
  a known lie in the stats implementation, not a data issue.

Last reviewed: 2026-06-06
