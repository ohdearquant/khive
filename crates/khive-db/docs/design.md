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

- `migrations.rs` owns the contiguous version/name ledger and includes each
  migration's DDL from a numbered file under `sql/`.
- Migrations are forward-only, applied in version order, each in its own
  transaction. V1 is immutable.
- Legacy `ServiceSchemaPlan`/`apply_schema_plan` API preserved for
  backward compatibility. New schema changes use the versioned `MIGRATIONS`
  array.
- V6/V7/V8 are frozen no-op slots; their `name` strings appear in the
  production `_schema_migrations` table and must not change.
- V20 adds durable `blob_gc_claims` plus entity INSERT/UPDATE trigger fences
  for ADR-091 Amendment 9's external-I/O-free transactional blob sweep.
- After the separately released Phase-4a GC gate has converged fleet-wide and
  every pre-Phase-4a process is drained, quiesce every Phase-4a application
  reader/writer before Phase 4b/V21 stages role-keyed attachments under the
  canonical database GC owner. A GC-only Phase-4a worker's completed-V21
  compatibility is not general serving compatibility. Phase 4b
  authenticates application-owned roles through the async host coordinator,
  then atomically switches liveness/fences to attachments and drops
  `entities.content_ref`. Pending/incomplete state never enables GC, and the
  Phase-4b service fleet starts only after exact-current topology validation.
- V22 (`embedding_space_shadow_stage`) is an additive, dormant registry stage.
  It preserves exact legacy tuple provenance in dedicated tables and records
  `legacy_staged`, but leaves `_embedding_models` and every provider/vector/ANN/
  cache/log path live and unchanged. The V20 coordinator advances through V22
  after completing V21 and before returning. Prepared-runtime assembly requires
  exact current V22 rather than merely a completed V21 marker.

### Attachments and Blob Liveness (ADR-111, ADR-121, ADR-160)

- `stores/attachment.rs` implements `AttachmentStore`; entity role `content`
  is projected into the compatibility `Entity.content_ref` response field.
- Entity-plus-initial-attachments publication and entity/note hard-delete
  cleanup are transactional. Soft deletion retains attachment liveness.
- Transactional filesystem blob GC validates every attachment/claim ref,
  anti-joins all attachment rows, and relies on attachment INSERT/UPDATE claim
  fences. The current epoch gate admits only V21 or canonical
  `(22, "embedding_space_shadow_stage")`
  after revalidating the exact completed-V21 ledger/marker/fence/schema
  combination; V20, pending/incomplete/malformed V21, foreign V22, and unknown V23-or-later
  states refuse dry-run and destructive sweep before root/filesystem or claim
  mutation. The older Phase-4a binary safely refuses V22 as ahead of its exact
  V21 contract.

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
- Live V22 stages ADR-160 D6's target registry shape in
  `_embedding_models_v22_shadow` and preserves the complete legacy source tuple
  in `_embedding_model_legacy_provenance`. The shadow is intentionally not a
  serving API: no vector or ANN object is relabeled, rebuilt, or selected from
  it, and the later provider-bound atomic cutover remains unimplemented.

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

### Historical pre-consolidation Brain Pack + Knowledge Sections (ADR-048)

- Historical V20 creates `brain_profile_snapshots` and `brain_event_log` tables for
  the brain pack (Phase 1).
- Historical V21 creates `knowledge_sections` with a 10-value SectionType enum, FK to
  `knowledge_atoms`, and UNIQUE(atom_id, section_type) (Phase 2).

### Daemon & Warm Startup (ADR-049)

- Historical V22 extends `knowledge_atoms`, `knowledge_sections`, and `knowledge_domains`
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
namespace) receives a clone of the same handle. Store methods resolve that
handle again at write time, so construction before a Tokio runtime cannot
permanently cache a queue bypass. Single-row, batch, and transaction-owning
operations submit DML-only closures through the shared task; the task owns the
outer transaction. A non-strict compatibility fallback may still use the
legacy standalone/pool-mutex writer and records a store-specific
`direct_route_violation`. Strict mode refuses that fallback before it opens a
direct writer.

Strict routing remains opt-in. Flipping its default is separately gated by
ADR-135 F2 and ADR-136 D2 production A/B evidence plus the release gate; this
write-time routing hardening does not claim that evidence. The unified helper
covers the SQLite store layer; remaining runtime-orchestration direct-writer
call sites stay in #1847's follow-up inventory rather than being silently
classified as complete.

See `crates/khive-db/docs/api/pool.md` and `crates/khive-db/docs/api/vectors.md`
for the per-function routing rules and the tests that pin them down.

### Write-transaction external-work invariant (ADR-091 Amendment 9)

SQLite write transactions contain database statement execution and bounded
in-memory preparation only. They never contain filesystem/process/network I/O,
sleeps, blocking waits, embedding/model work, or another subsystem call. The
enumerated owner/caller audit lives in ADR-091 and is a review invariant: adding
or widening any `BEGIN IMMEDIATE`, `WriterGuard::transaction`, writer-task
request, or `SqlAccess::atomic_unit` scope requires updating that table.

Filesystem blob GC is the cross-resource reference design. A database-scoped
process/advisory owner lock makes every pre-existing claim safely recoverable
even after root relocation or database restore. Candidate recovery, claim,
physical deletion, and cleanup proceed in batches of at most 128. Each claim
and cleanup transaction is SQL-only and commits before filesystem deletion;
attachment INSERT/UPDATE triggers reject claimed references in every
released-writer interval.

## Consistency Notes

- **sqlite-vec KNN non-monotonicity** (`stores/vectors.rs`): The IN-subquery
  approach for namespace-scoped KNN can produce non-monotonic results. Tracked
  in MEMORY.md under `project_sqlite_vec_knn_bug.md`.

- **`embedding_coverage` stat hardcoded**: `stats()` reports
  `embedding_coverage: 0.0` regardless of actual indexed vector count. This is
  a known lie in the stats implementation, not a data issue.

Last reviewed: 2026-08-17
