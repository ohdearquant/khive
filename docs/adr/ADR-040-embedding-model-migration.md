# ADR-040: Embedding Model Migration Path

**Status**: proposed\
**Date**: 2026-05-19\
**Authors**: khive maintainers

## Context

khive embeds every entity and note at write time using a model configured in `RuntimeConfig`
(ADR-012). The resulting vectors are stored in a per-model virtual table `vec_{model_key}` inside
`khive-db` (see `crates/khive-db/src/stores/vectors.rs`, `SqliteVecStore`). The table name is
derived by sanitizing `EmbeddingModel::to_string()` — e.g., `all_minilm_l6_v2` — and has no
metadata binding it to a particular model's dimension count or semantic space.

This creates a correctness hazard. Vectors are not interchangeable across models:

- **Same model, different crate version** (`lattice-embed 0.1.x → 0.2.x`, same checkpoint): if
  the checkpoint is unchanged, existing vectors remain valid. If the checkpoint is updated with the
  crate version, vectors shift without any runtime-level warning.
- **Same architecture, new checkpoint** (e.g., MiniLM v1 → v2): dimensions are identical, but the
  latent space shifts subtly. Search quality degrades silently — no error, no signal.
- **Different model** (e.g., `AllMiniLmL6V2` at 384 dims → `BgeBaseEnV15` at 768 dims): querying
  the old table with a new-model query vector will produce nonsense results or a
  dimension-mismatch error from sqlite-vec.

Today khive has no migration path for any of these scenarios. A user who sets
`KHIVE_EMBEDDING_MODEL=bge-base-en-v1.5` on a previously-indexed database silently searches an
incompatible table. GitHub issue #4 captures this as a P2 gap: "Switching embedding models
invalidates the per-model vec table; users have no command to re-embed everything with the new
model today."

The strategic context makes a clean solution feasible: inference is local via `lattice-embed`
(pure-Rust, in-process, SIMD-accelerated, ADR-012). Background re-embedding costs CPU/GPU time
but zero API dollars and no rate limits.

This ADR defines the migration path, storage contract, and re-embed strategy for v0.2.

## Decision

### D1: Model identity includes (name, version, dim)

Every embedding model is identified by a triple `(name, version, dim)` stored in a new
`_embedding_models` table. Each row in the vector stores carries a foreign key to this table.
The runtime registers a model on startup if it is not already present.

`EmbeddingModel::to_string()` (from `lattice-embed`) provides the `name` field. The `version`
field tracks the lattice-embed checkpoint version distinct from the crate semver: it is the
string the `EmbeddingModel` variant is registered with at startup, supplied in `RuntimeConfig`
(see D5). The `dim` field is `EmbeddingModel::dimensions()`.

### D2: Multiple models coexist in the vector store

The current single `vec_{model_key}` table structure is replaced by a model-tagged design.
The same virtual table **per dimension** serves all registered models of that dimension; each
row carries `model_id` alongside `subject_id`, `namespace`, and `kind`. A single active model
is tracked in `_embedding_models.is_active`. (sqlite-vec's fixed-dimension constraint means a
separate virtual table is required for each distinct embedding dimension — see D3.)

When a user switches the active model:
1. The runtime registers the new model in `_embedding_models` (if absent) and marks it active.
2. Old rows for the previous model remain in the table — they are not deleted.
3. Searches use the active model's vectors. Rows with no vector for the active model are
   absent from vector results; FTS5 text search still covers them, so search gracefully
   degrades rather than breaking.
4. A background re-embed job (D4) restores full hybrid coverage asynchronously.

### D3: Schema — V3 migration

Building on the versioned migration system (ADR-022), the next migration is **V3** (current last
version in `MIGRATIONS` is V2: `add_name_to_notes`, `crates/khive-db/src/migrations.rs` line 176).

V3 creates the `_embedding_models` registry table and drops legacy per-model-key vector tables.
No vector virtual table is created during the migration — the runtime creates per-dimension
`vec_{dim}` tables lazily on first model registration (see the authoritative DDL below).

**sqlite-vec dimension constraint**: `vec0` virtual tables require a fixed dimension in DDL
(`float[384]`). A single table cannot hold vectors of different dimensions. The design therefore
uses **per-dimension virtual tables** (`vec_384`, `vec_768`, …) created lazily by the runtime
when a model of that dimension is first registered. `_embedding_models` stores a `table_name`
column pointing to the correct table; the runtime dispatches via a `model_id → table_name`
lookup. This is the minimal departure from the idealized single-table design required by the
storage constraint.

The V3 DDL is:

```sql
CREATE TABLE IF NOT EXISTS _embedding_models (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    name            TEXT    NOT NULL,
    version         TEXT    NOT NULL DEFAULT 'default',
    dim             INTEGER NOT NULL,
    table_name      TEXT    NOT NULL,          -- e.g. vec_384, vec_768
    registered_at   INTEGER NOT NULL,
    is_active       INTEGER NOT NULL DEFAULT 0,
    last_reembed_at INTEGER,                   -- set when background re-embed completes
    UNIQUE (name, version, dim)
);

CREATE INDEX IF NOT EXISTS idx_embedding_models_active
    ON _embedding_models(is_active);
```

The per-dimension virtual tables (`vec_384`, `vec_768`, ...) are created lazily by the runtime
when a model of that dimension is first registered. Each has the shape:

```sql
CREATE VIRTUAL TABLE IF NOT EXISTS vec_{dim} USING vec0(
    subject_id  TEXT    NOT NULL,
    model_id    INTEGER NOT NULL,
    namespace   TEXT    NOT NULL,
    kind        TEXT    NOT NULL,
    embedding   float[{dim}]
);
```

Existing `vec_{model_key}` tables from pre-V3 databases are dropped during the migration
(pre-alpha: no production data at risk). The runtime creates `vec_{dim}` tables lazily on first
model registration; no backfill is needed because pre-V3 vectors are model-specific and cannot be
reused across model boundaries.

### D4: Background re-embed strategy

When the active model changes, khive spawns a background re-embed job in a Tokio task that
processes the namespace in batches. During re-embed:

- Searches for the active model return results only for records already re-embedded. Missing
  records fall through to FTS5 text results.
- Search does NOT fall back to the old model's vectors. Cross-model vector comparison would
  produce meaningless scores.
- The job runs until all entities and notes in the namespace have vectors in the active model's
  table, then marks the migration complete in `_embedding_models` via a new
  `last_reembed_at INTEGER` column.

Batch size defaults to 64 records and is configurable via `RuntimeConfig.reembed_batch_size`
(default: `64`). The job uses the existing `embed_batch` operation
(`crates/khive-runtime/src/retrieval.rs`, line 64).

The job is idempotent: records that already have a vector for the active `model_id` are
skipped. On restart, it resumes from the first record without an active-model vector.

### D5: Runtime configuration change

`RuntimeConfig` gains one field to express model version alongside model variant:

```rust
/// Optional checkpoint version string for the active embedding model.
/// Defaults to "default" if unset. Used to distinguish checkpoint-level
/// breaks within the same EmbeddingModel variant (e.g., MiniLM v1 → v2).
pub embedding_model_version: String,
```

The default is `"default"`. Users who manage checkpoint provenance explicitly can set this to a
semver or date string via `KHIVE_EMBEDDING_MODEL_VERSION`.

At startup, `KhiveRuntime::new` registers the model in `_embedding_models` using
`(embedding_model.to_string(), embedding_model_version, embedding_model.dimensions())`. If a
row already exists with this triple, the runtime uses the existing `id`. If the triple is new,
a new row is inserted. The runtime then sets `is_active = 1` for this row and `is_active = 0`
for all others.

### D6: lattice-embed contract

khive-runtime treats `lattice-embed` as a capability provider with a fixed contract
(per ADR-012). For embedding model migration, khive requires:

- `EmbeddingModel::dimensions() -> usize` — already available; used to derive the per-dimension
  vector table name.
- `EmbeddingModel::to_string() -> &str` — already available; used as the `name` field in
  `_embedding_models`.
- `NativeEmbeddingService::embed_one(text, model) -> Vec<f32>` and
  `EmbeddingService::embed(texts, model) -> Vec<Vec<f32>>` — already available in
  `khive-runtime::retrieval`; the background job uses `embed_batch` which delegates to the
  second form.

khive does not prescribe lattice-embed internals. Alternative embedding providers (e.g., a
hypothetical remote provider) could satisfy the same contract by implementing `EmbeddingService`.
That extensibility is future work; this ADR covers the local-lattice path only.

## Rationale

### Why multi-model coexistence (D2) over single-active-model-with-deletion

With local inference there is no API cost to keep old vectors, so the calculus differs from
cloud-embedding systems. The primary benefit is **non-destructive model evaluation**: a user
testing whether `bge-base-en-v1.5` ranks better than `all-minilm-l6-v2` can flip back without
a full re-embed. Option A (delete on switch) is simpler but irreversible, which is unfriendly
during the lattice-embed maturation phase where model quality is actively improving.

### Why background re-embed (D4) over eager or lazy

Eager blocks the server until all vectors are updated — unbounded for large namespaces and
inconsistent with the lazy model-load design in ADR-012. Lazy leaves an unbounded long tail of
unembedded records. Background re-embed using the existing `embed_batch` operation gives bounded
completion time with no server downtime, and the job is idempotent on restart.

### Why (name, version, dim) triple as model identity (D1)

The enum variant name alone is insufficient: a crate version bump that changes the checkpoint
is indistinguishable. The `version` field lets users express checkpoint provenance when it
matters; the `"default"` sentinel is practical for users who do not track checkpoints explicitly.

## Alternatives Considered

| Alternative | Pros | Cons | Why rejected |
| ----------- | ---- | ---- | ------------ |
| **Option A: single active model, delete old vectors on switch** | Simpler schema; no multi-model dispatch | Irreversible data loss; no rollback without full re-embed; hostile to model A/B evaluation | Rejected — local inference makes keeping old vectors cheap; data loss is the worse trade-off |
| **Eager re-embed: block server until all vectors are updated on model switch** | Predictable state after switch; no partial-coverage period | Blocking latency is unbounded for large namespaces; inconsistent with lazy model load (ADR-012) | Rejected — unacceptable for any namespace with thousands of records |
| **Lazy re-embed: re-embed records only when touched post-switch** | Zero overhead at switch time; no background goroutine | Unbounded tail: stale records may never get re-embedded; misleading search coverage | Rejected — coverage is the invariant we want; lazy provides no bound on when it is restored |
| **Per-model-variant separate tables (vec_{model_key} as today)** | Clean isolation per model; no dispatch logic | Table proliferation; FKs cannot span virtual tables; no active-model tracking; does not fix silent dimension mismatch | Rejected — the `_embedding_models` registry + per-dimension dispatch design consolidates this cleanly |
| **Store model identity as a column on entities/notes (not a separate table)** | No extra table | Denormalized; makes active-model tracking harder; harder to enumerate all registered models | Rejected — separate `_embedding_models` table is clean and queryable |

## Consequences

### Positive

- Model switches are non-destructive: old vectors are preserved, enabling rollback and A/B
  evaluation across models.
- Search quality degrades gracefully during re-embed rather than breaking.
- The background re-embed leverages existing `embed_batch` infrastructure and local inference —
  no new dependencies and no API cost.
- Model identity is queryable from the DB:
  `SELECT * FROM _embedding_models WHERE is_active = 1`.
- The migration is forward-only per ADR-022 discipline; schema history is auditable via
  `_schema_migrations`.

### Negative

- Vector store dispatch adds a `model_id → table_name` lookup per insert/search (one indexed read).
- sqlite-vec's fixed-dimension constraint forces per-dimension tables for cross-dim model pairs;
  the `table_name` column in `_embedding_models` keeps dispatch transparent to callers.
- The background re-embed job is a new runtime concern; idempotent resume mitigates mid-job
  failures but the code must be tested (T4, T5).
- `RuntimeConfig` gains two fields; callers using `::default()` are unaffected.

### Neutral

- Existing databases at V2 receive V3 via the standard `run_migrations` path (ADR-022). The
  migration drops legacy `vec_*` per-model-key tables and creates the `_embedding_models`
  registry. No vector virtual table is created during migration; the runtime creates `vec_{dim}`
  tables lazily on first model registration.
- Cross-encoder reranking (deferred in ADR-013) is unaffected: it does not interact with the
  vector store schema.
- The `VectorStore` trait in `khive-storage` (ADR-005) is **unchanged**. Model identity is
  **store-scoped**: `SqliteVecStore` is constructed with a resolved `(model_id, table_name, dim)`
  tuple instead of a bare `model_key` string. Each store instance is bound to a single model;
  the runtime dispatches to the correct store based on the active model. This is an
  implementation-layer change fully contained within `khive-db` and the runtime dispatch layer.
  `VectorRecord` and `VectorSearchRequest` do not carry `model_id`.

## Implementation

### V3 migration (`crates/khive-db/src/migrations.rs`)

Add to `MIGRATIONS` (append after the V2 entry at line 176):

```rust
VersionedMigration {
    version: 3,
    name: "embedding_model_registry",
    up: V3_UP,
},
```

`V3_UP` creates the `_embedding_models` table, drops existing per-key vec tables (idempotent
via `DROP TABLE IF EXISTS vec_%`-style enumeration at migration time), and creates no virtual
table — the runtime creates `vec_{dim}` tables lazily on first model registration. V3 therefore
has no data loss risk on fresh databases (no vec tables yet) and handles the pre-alpha case by
dropping whatever `vec_*` tables exist.

### `_embedding_models` table

```sql
CREATE TABLE IF NOT EXISTS _embedding_models (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    name          TEXT    NOT NULL,
    version       TEXT    NOT NULL DEFAULT 'default',
    dim           INTEGER NOT NULL,
    table_name    TEXT    NOT NULL,
    registered_at INTEGER NOT NULL,
    is_active     INTEGER NOT NULL DEFAULT 0,
    last_reembed_at INTEGER,
    UNIQUE (name, version, dim)
);
CREATE INDEX IF NOT EXISTS idx_embedding_models_active
    ON _embedding_models(is_active);
```

### `vec_{dim}` virtual table (created lazily by runtime)

```sql
CREATE VIRTUAL TABLE IF NOT EXISTS vec_{dim} USING vec0(
    subject_id  TEXT    NOT NULL,
    model_id    INTEGER NOT NULL,
    namespace   TEXT    NOT NULL,
    kind        TEXT    NOT NULL,
    embedding   float[{dim}]
);
```

The model_id is used in both `INSERT` and the `WHERE` clause of `SELECT`, so an index on
`model_id` within the virtual table is desirable. sqlite-vec's vec0 does not support auxiliary
indexes; filtering by `model_id` is handled via a subquery (same pattern as the existing `kind`
filter in `SqliteVecStore::search`, lines 346-363).

### Updated `SqliteVecStore` constructor

```rust
pub fn new_with_model_id(
    pool: Arc<ConnectionPool>,
    is_file_backed: bool,
    model_id: i64,
    table_name: String,    // e.g. "vec_384"
    dimensions: usize,
    namespace: String,
) -> Result<Self, SqliteError>
```

The existing `model_key`-based constructor is kept for tests; production code routes through
the model-registry path.

### `RuntimeConfig` change

```rust
pub struct RuntimeConfig {
    // ... existing fields ...
    /// Checkpoint version string for the active embedding model.
    /// Defaults to "default". Set via KHIVE_EMBEDDING_MODEL_VERSION.
    pub embedding_model_version: String,
    /// Number of records processed per batch in background re-embed jobs.
    /// Defaults to 64.
    pub reembed_batch_size: usize,
}
```

### Background re-embed job sketch

```rust
/// Spawned by KhiveRuntime when the active model changes.
/// Processes `batch_size` records at a time; skips records already
/// having a vector for `active_model_id`.
pub async fn reembed_namespace(
    rt: KhiveRuntime,
    namespace: String,
    active_model_id: i64,
    batch_size: usize,
) -> RuntimeResult<u64>   // returns count of newly embedded records
```

The job:
1. Pages through `entities` and `notes` in the namespace using `list` operations.
2. For each batch, filters to records without a row in `vec_{dim}` for `model_id =
   active_model_id`.
3. Calls `embed_batch(texts)` and inserts results via the updated `VectorStore::insert`.
4. On completion, updates `_embedding_models.last_reembed_at` for the active model.

The job is launched via `tokio::task::spawn` in `KhiveRuntime::switch_active_model`. No
persistent job queue is introduced in v0.2 — the job runs in-process for the lifetime of the
server. If the server restarts mid-job, the next startup re-discovers partially-covered records
and resumes automatically (idempotent by design).

A future version may expose job progress via a `reembed_status(namespace)` runtime operation
and MCP verb.

### CLI surface (per GitHub issue #4)

The administrative `migrate-embeddings` operation described in issue #4 maps to the above:

```
khive migrate-embeddings --to <model> [--keep-old] [--batch-size N]
```

`--keep-old` is the default (multi-model coexistence, D2); explicitly passing `--drop-old` is
allowed and triggers deletion of non-active model rows after re-embed completes. The CLI
subcommand is an administrative path; it is NOT exposed via the MCP `request` verb surface
(per the issue: "not exposed yet — administrative operation").

## Test Contract

The following test scenarios must be present before this ADR is considered implemented:

**T1: Model registration and active-model tracking**
- Create a runtime with `EmbeddingModel::AllMiniLmL6V2`.
- Verify `_embedding_models` has one row with `is_active = 1` and `dim = 384`.
- Switch to `EmbeddingModel::BgeBaseEnV15`.
- Verify the original row has `is_active = 0` and a new row with `dim = 768` has `is_active = 1`.

**T2: Multi-model coexistence — vectors for old model survive switch**
- Index N entities with `AllMiniLmL6V2`.
- Switch active model to `BgeBaseEnV15`.
- Query `vec_384` for the old `model_id`: rows are still present.
- Query `vec_768` for the new `model_id`: no rows yet (re-embed not run).

**T3: Search graceful degradation before re-embed**
- Same setup as T2, post-switch, before re-embed.
- Call `search(kind="entity", query="...")`.
- Verify call returns results (FTS5 text hits) without panic or dimension error.
- Verify no vector-source hits are returned for the new model before re-embed.

**T4: Background re-embed restores full coverage**
- Index N entities. Switch active model. Run `reembed_namespace` to completion.
- Verify `vec_{dim}` has N rows for the new `model_id`.
- Verify `search` now returns vector-source hits for the new model.

**T5: Re-embed job is idempotent**
- Run `reembed_namespace` twice on the same namespace and model.
- Verify record count does not double; verify no duplicate `subject_id + model_id` rows.

**T6: Dimension mismatch is rejected at insert, not silently stored**
- Attempt to insert a 384-dim vector into a store configured for 768-dim.
- Verify the insert returns an error, not silent corruption.

## References

- ADR-005: Storage Capability Traits — `VectorStore` trait this ADR extends
- ADR-012: Retrieval Architecture — establishes `lattice-embed` as the embedding source and
  `RuntimeConfig.embedding_model` as the active model field; this ADR extends that config
- ADR-013: Retrieval Port Scope — `embed_batch` is in-scope for v0.1; the background re-embed
  job in this ADR uses it directly
- ADR-022: Schema Migrations — V3 migration follows the `VersionedMigration` pattern defined
  there; new entry appended to `MIGRATIONS` at `version = 3`
- ADR-024: Note Search and Cross-Substrate — vector store schema changes in this ADR extend the
  `vec_{dim}` tables to cover notes as well as entities (the `kind` column is preserved)
- GitHub issue #4 (`ohdearquant/khive`): "Embedding model migration path" — this ADR closes
  that issue by specifying `migrate_embeddings` runtime operation, `--keep-old` default, and
  the multi-model coexistence design
- `crates/khive-db/src/stores/vectors.rs` — `SqliteVecStore` implementation this ADR modifies
- `crates/khive-runtime/src/runtime.rs` — `RuntimeConfig` and `vec_model_key()` this ADR extends
- `crates/khive-runtime/src/retrieval.rs` — `embed_batch` reused by the background re-embed job
- `crates/khive-db/src/migrations.rs` — V3 migration appended here; current last version is V2
  (`add_name_to_notes`, line 176)
- `lattice-embed` (crates.io): the embedding library providing `EmbeddingModel::dimensions()`,
  `EmbeddingModel::to_string()`, and `EmbeddingService::embed`
