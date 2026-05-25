# ADR-043: Embedding Model Migration

**Status**: accepted
**Date**: 2026-05-23
**Authors**: Ocean, lambda:khive
**Depends on**:

- ADR-011 (Embedding and Inference Architecture)
- ADR-022 (Events Query Surface — events table, EventFilter)
- ADR-031 (Multi-Engine Retrieval — `[[engines]]` config, EngineConfig, vec table naming)
- ADR-033 (Recall Pipeline — fallback semantics during migration)
- ADR-044 (Vector Store Extensions — orphan sweep, batch search, Capabilities)
- lattice ADR-029 (RegisteredModel registry)

## Context

A khive deployment's vectors are static once written — every stored vector encodes a
record at the embedding model that was active at write time. When the deployment's
active model changes (operator decision, model upgrade, new variant), all existing
vectors under the old model become stale relative to the new model's embedding space.
Cosine across distinct embedding spaces is not meaningful; recall against mixed-model
storage returns nonsense or, worse, silently degrades without warning.

Old khive ADR-040 specified an `_embedding_models` registry + background re-embed
worker for exactly this problem. v1 dropped both without a replacement — a correctness
hazard for any deployment that changes its `[[engines]].name`. This ADR restores the
subsystem, layered on top of what lattice already provides.

### What lattice already provides

Per `/Users/lion/projects/khive/lattice/crates/{embed,transport}/`:

- **`lattice_embed::EmbeddingKey { model, revision, dims, metric, dtype, norm }`** with
  `canonical_bytes()`. Vectors under different keys are not exchangeable; lattice's
  cache key includes `{model}:{key_version()}:{dims}` so stale vectors are never
  returned under a newer model.
- **`lattice_embed::EmbeddingModel::key_version()`** returning the model-family revision
  string (`v1.5`, `v2`, …). The unit of supersession at the model-family level.
- **`lattice_embed::migration::MigrationController`** — a persisted state machine:
  `Planned → InProgress → Completed | Failed | Paused`. Operates on a `MigrationPlan
  { source_model, target_model, total_embeddings, batch_size }` and tracks progress
  durably so a crashed worker resumes from the last completed batch.
- **`lattice_transport::drift::detect_drift_records`** — Wasserstein/Sinkhorn OT
  distance for detecting when re-embed is warranted. Returns a `DriftReport` with
  source/target model labels and the computed distance.

### What lattice does NOT provide

- A dynamic registry of "which model is currently active per engine" — lattice's
  `EmbeddingModel` is a static enum, and `lattice-tune::registry::RegisteredModel` is
  about model metadata, not active-model selection.
- A trigger mechanism — "this model is now superseded, start re-embedding."
- A background worker that drives the migration through `MigrationController`'s
  state transitions.
- An audit trail tying stored vectors to the model that produced them.

khive owns these four. The math (drift, state machine, key) stays in lattice.

### Scope

This ADR covers: a registry of embedding models known to a khive deployment, the
trigger surface for starting a migration, the worker that executes it, the
coexistence rules during migration, and the audit trail. It does NOT cover the
`embedding_model_version` user-facing config knob from old ADR-040 §6 — that is
deferred until lattice's structured-output testing completes (operator directive,
2026-05-23). See `.khive/plans/embedding-version-config.md`.

## Decision

### 1. `_embedding_models` registry

The `_embedding_models` registry is owned by `khive-runtime` (substrate-shared), not
by a specific pack. Both the memory pack (notes) and the kg pack (entity descriptions)
reference the active model via the registry. Drift-check sampling uses notes + entity
descriptions across all packs that emit embeddings.

The schema:

```sql
CREATE TABLE _embedding_models (
    id              BLOB PRIMARY KEY,            -- UUIDv7
    engine_name     TEXT NOT NULL,               -- matches [[engines]].name (ADR-031 D3)
    model_id        TEXT NOT NULL,               -- e.g. "bge-small-en-v1.5"
    key_version     TEXT NOT NULL,               -- EmbeddingModel::key_version()
    dim             INTEGER NOT NULL,
    output_dim      INTEGER,                     -- MRL truncation; matches EngineConfig
    status          TEXT NOT NULL CHECK (status IN ('pending', 'active', 'superseded', 'archived')),
    activated_at    INTEGER,                     -- unix microsec; non-null when status reached 'active'
    superseded_at   INTEGER,                     -- non-null when status moved to 'superseded'
    superseded_by   BLOB,                        -- _embedding_models.id of the replacement
    canonical_key   BLOB NOT NULL UNIQUE,        -- EmbeddingKey::canonical_bytes()
    created_at      INTEGER NOT NULL
);

-- One active model per engine at any time. Enforced at the schema level.
CREATE UNIQUE INDEX idx_embed_models_one_active
    ON _embedding_models(engine_name) WHERE status = 'active';

CREATE INDEX idx_embed_models_engine_status
    ON _embedding_models(engine_name, status);
```

The partial unique index makes "two active models on one engine" structurally
impossible — any attempt to insert a second `active` row for the same engine fails
the constraint. Migrations therefore execute as `BEGIN; UPDATE active→superseded;
UPDATE pending→active; COMMIT;` — atomic by virtue of the index.

#### Vector store column addition (V16, ADR-015)

Each regular `vec_<engine>` table (ADR-031 D3) gains a TEXT model tag column.
This was formalized in migration V16:

```sql
ALTER TABLE vec_<engine> ADD COLUMN embedding_model TEXT NOT NULL
    DEFAULT 'all-minilm-l6-v2';
CREATE INDEX idx_vec_<engine>_subject_model
    ON vec_<engine>(subject_id, embedding_model);
```

The composite `(subject_id, embedding_model)` index supports the scoped recall
SQL: `WHERE subject_id = ? AND embedding_model = ?`. The default value at column
creation time was chosen so existing rows backfill to the legacy MiniLM model;
deployments using a non-default model **must** run the dedicated backfill worker
described in §8 before relying on model-scoped recall.

**Design trade-off — TEXT vs BLOB FK.** ADR-043's first draft (pre-V16) specified
`embedding_model_id BLOB REFERENCES _embedding_models(id)`. V16 instead stores
the model_id directly as TEXT, joining against `_embedding_models.model_id`
when needed:

- TEXT model_id is the natural primary key used everywhere else in the runtime
  (kkernel engine list, `EmbeddingService::key_version()`, env var
  `KHIVE_ADDITIONAL_EMBEDDING_MODELS`) — keeping the same shape end-to-end.
- BLOB FK would require a sub-select on every vector insert/search to resolve
  the active model's UUID. The hot path is recall scoring; the join cost is
  unjustified for a column whose values change only on registry events.
- Schema-level referential integrity is replaced by application-level
  validation in the runtime registry: unknown model names are rejected at
  `KhiveRuntime::embedder(name)` and at `RecallParams.embedding_model`
  validation.

The `_embedding_models` registry table (V14) still owns the authoritative model
metadata (dim, output_dim, status, key_version). V16's `embedding_model TEXT`
column is the foreign-key-by-value reference back to `_embedding_models.model_id`.

**sqlite-vec virtual tables.** vec0 virtual tables cannot accept `ALTER TABLE
ADD COLUMN` because they declare their columns at `CREATE VIRTUAL TABLE` time.
V16 handles this via the open-time path in `khive-db/src/backend.rs`: when
opening a `vec_<engine>` table that lacks `embedding_model`, the runtime
rebuilds the virtual table with the new schema. **Existing rows are lost on
rebuild** — this is acceptable for deployments that have not yet enabled
dual-embedding because vectors will be re-embedded by the next backfill cycle,
but **operators must take a backup before upgrading any production deployment
with persisted non-default embeddings**. A follow-up migration (tracked in
ADR-043 §8.2) will implement a copy-with-default rebuild to preserve old
vectors with their inferred model tag.

### 2. Triggers — three sources, one event

A migration begins on one of:

| Source                                                                    | Action                                                                                             | Event emitted                                                         |
| ------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------- |
| Engine config change in `khive.toml` (`[[engines]].name` or `output_dim`) | Detected at startup; kkernel compares config-declared keys against `_embedding_models` active rows | `EmbeddingModelChanged`                                               |
| Operator CLI: `khive engine migrate <engine> --to <model>`                | Explicit start                                                                                     | `EmbeddingModelChanged`                                               |
| Drift threshold breach via `khive engine drift-check`                     | Operator-triggered; lattice's drift detection returns distance > threshold                         | `EmbeddingDriftDetected` (advisory only — does NOT start a migration) |

`EmbeddingModelChanged` payload:

```rust
pub struct EmbeddingModelChangedPayload {
    pub engine_name:     String,
    pub source_model_id: Option<Uuid>,    // _embedding_models.id; None on first registration
    pub target_model_id: Uuid,            // new pending row's id
    pub initiated_by:    InitiationKind,  // ConfigDiff | OperatorCli | (future) DriftAuto
    pub plan:            MigrationPlanSummary,  // engine_name, source/target model ids, source/target dims, initiated_by
    // Note: total_embeddings and batch_size are NOT in MigrationPlanSummary;
    // they live on lattice_embed::migration::MigrationPlan, accessed by the worker
    // directly from MigrationController — not from the event payload.
}
```

**No auto-migration on drift.** Drift detection surfaces a recommendation event; the
operator decides whether to act. Auto-migrate on drift is out of scope (it risks
runaway re-embed loops with model-quality oscillations) — its own ADR if revisited.

### 3. `EmbedMigrationWorker`

A background task launched at kkernel startup (`khive-pack-memory::migration::worker`).
It subscribes to `EmbeddingModelChanged` events (via `PackEventConsumer`, ADR-017)
and drives the migration to completion through `lattice_embed::migration::MigrationController`.

Per-event flow:

1. **Plan**: load the event payload's `MigrationPlanSummary`, construct a lattice
   `MigrationPlan` via `MigrationController::plan(source, target, batch_size)`.
2. **Stage table**: `CREATE TABLE vec_<engine>_pending AS SELECT * FROM vec_<engine> WHERE 0;`
   — schema-matching empty table. `_pending` carries the new model's vectors as
   they are produced.
3. **Iterate batches**:
   ```rust
   while let Some(batch) = controller.next_batch().await? {
       let records: Vec<(Uuid, String)> = load_record_texts(&batch).await?;
       let new_vectors = registry.embed_batch(&records, target_model_id).await?;
       insert_into_pending(&new_vectors).await?;
       controller.mark_batch_done(batch.id).await?;
   }
   ```
4. **Swap (atomic)**:
   ```sql
   BEGIN;
     ALTER TABLE vec_<engine>         RENAME TO vec_<engine>_old;
     ALTER TABLE vec_<engine>_pending RENAME TO vec_<engine>;
     UPDATE _embedding_models SET status = 'superseded',
         superseded_at = ?, superseded_by = ?
         WHERE engine_name = ? AND status = 'active';
     UPDATE _embedding_models SET status = 'active', activated_at = ?
         WHERE id = ?;
   COMMIT;
   DROP TABLE vec_<engine>_old;
   ```
5. **Emit completion**: `EmbeddingMigrationCompleted` event carrying both model
   ids, total records re-embedded, and elapsed time.

#### sqlite-vec virtual table rename caveat

The RENAME in step 4 behaves differently depending on the backend:

- **Regular table** (e.g., quantized/flat backend): `ALTER TABLE vec_<engine> RENAME TO
  vec_<engine>_old` followed by `ALTER TABLE vec_<engine>_pending RENAME TO vec_<engine>`
  works as written. This path applies when the engine's vector store is backed by a plain
  SQLite table.
- **sqlite-vec virtual table**: Virtual tables created via `CREATE VIRTUAL TABLE ... USING
  vec0(...)` do not reliably support `ALTER TABLE ... RENAME TO` once data is present. For
  this path, use the documented sqlite-vec recreate pattern:
  1. `CREATE VIRTUAL TABLE vec_<engine>_new USING vec0(embedding float[<new_dim>])` — create
     a fresh virtual table with the target schema.
  2. `INSERT INTO vec_<engine>_new SELECT * FROM vec_<engine>_pending` — copy new vectors.
  3. `DROP TABLE vec_<engine>` — remove the old virtual table.
  4. `ALTER TABLE vec_<engine>_new RENAME TO vec_<engine>` — rename succeeds because the
     new table has no in-use readers (within the same transaction boundary).
     Note: `CREATE TABLE vec_<engine>_new AS SELECT * FROM vec_<engine> WHERE 0` does NOT
     produce a virtual table from a virtual source — always use `CREATE VIRTUAL TABLE` explicitly
     when the source is a vec0 table.

The v1 implementation uses the virtual-table path. The code in
`khive-runtime::migration::swap` branches on the `VectorStoreCapabilities.index_kinds`
field (ADR-044 §1) to select the correct rename strategy:

```rust
// Branch on whether the backend is a sqlite-vec virtual table:
match vec_store.capabilities().index_kinds.iter().find(|k| matches!(k, VectorIndexKind::SqliteVec)) {
    Some(_) => /* sqlite-vec recreate pattern (steps 1-4 above) */,
    None    => /* regular table RENAME path */,
}
```

If interrupted mid-batch, `MigrationController`'s persisted state lets the worker
resume from the last `mark_batch_done`. If a batch's embedding call fails (lattice
returns an error), the controller enters `Failed` state — the worker emits
`EmbeddingMigrationFailed` and exits the loop. Operators recover via
`khive engine migrate <engine> --resume` (re-enters the loop from the same
`MigrationController` state) or `--abort`.

`--abort` path: drop `vec_<engine>_pending`, leave the `_embedding_models` row in
`pending` status for manual inspection, then call `orphan_sweep` to clean leftover
vectors in the pending table:

```rust
vector_store.orphan_sweep(OrphanSweepConfig {
    namespaces: vec![target_namespace],
    subject_id_allowlist: None,   // None = scan all rows (ADR-044 §5)
    max_delete: 1000,             // operator-context default; override for large tables
    dry_run: false,
}).await?;
```

Default `max_delete` is the smaller of (10% of estimated orphan count, 1000); operators
override via `OrphanSweepConfig::max_delete`.

On swap-back-rollback paths (where the pending table was already renamed to
`vec_<engine>` before a failure), invoke `orphan_sweep` to clean up residual
vectors from the incomplete migration before leaving the table in a known-good
state. The `DROP TABLE vec_<engine>_old` in step 4 removes the old table outright;
no orphan sweep is needed for that side.

### 4. Recall during migration

The recall path (ADR-033) reads only the `active` model's vectors — i.e., reads
from `vec_<engine>` joined against `_embedding_models WHERE status = 'active'`.

While a migration is in progress, the `pending` model's vectors live in
`vec_<engine>_pending` and are NOT yet visible to recall. The `active` model is
still the previous one — it serves traffic unchanged. When the swap commits, the
new model atomically becomes active. There is no window where both models are
queryable for the same recall.

Cold-start: if an operator deletes the active model row (or starts a fresh
deployment with no `active` row), recall falls back to FTS5 only. When
`RecallConfig.fallback_during_migration = true` and no model has `status='active'`,
the recall pipeline in `khive-pack-memory::recall` skips the vector-search stage and
returns FTS5-only results. This is composition-layer behavior, not a `VectorStore`
trait fallback — ADR-005 keeps `VectorStore` and `TextSearch` as separate traits with
no cross-dependency. This is the only case where vector search is silently skipped —
the event log carries an `EmbeddingMigrationInProgress` annotation on each recall
during this window so the gap is observable.

### 5. Drift detection

`khive engine drift-check <engine> [--sample N]` (default N=1000) runs:

1. Sample N stored records uniformly from the active vector table.
2. Sample N representative texts from the namespace (notes + entity descriptions, across all packs that emit embeddings — memory and kg both contribute).
3. Re-embed the texts under the currently-active model.
4. Compute Wasserstein distance via `lattice_transport::drift::detect_drift_records`.
5. Emit `EmbeddingDriftDetected` with payload `{ engine_name, distance, sample_size,
   threshold, recommendation }`.

The threshold is configurable per engine in `[[engines]]`:

```toml
[[engines]]
name = "bge-small-en-v1.5"
# ...
drift_threshold = 0.15   # advisory; emit DriftDetected when distance exceeds
```

The drift check is CPU-bounded and runs off the recall hot path. Operators schedule
it (cron, manual, post-major-data-event). khive does NOT schedule it automatically.

### 6. Verb surface — CLI only

| Command                                          | Purpose                                                                                        |
| ------------------------------------------------ | ---------------------------------------------------------------------------------------------- |
| `khive engine list`                              | Engines and their model history (active / superseded / archived rows from `_embedding_models`) |
| `khive engine status <engine>`                   | Per-engine: active model, migration in progress?, last drift check                             |
| `khive engine migrate <engine> --to <model>`     | Start a migration to a new model                                                               |
| `khive engine migrate <engine> --resume`         | Resume a `Failed` migration                                                                    |
| `khive engine migrate <engine> --abort`          | Abort an `InProgress` migration; drop `_pending`                                               |
| `khive engine drift-check <engine> [--sample N]` | One-shot drift detection                                                                       |

No MCP verbs. Agents do not initiate migrations — brain profiles tune what they're
given but cannot decide to swap the underlying model. This is the architectural
boundary: model selection is operator territory; brain-tuned weights and adapters
are agent-influenced territory.

### 7. New event kinds

Added to `EventKind` (ADR-032 §3) and to the closed substrate event log:

- `EmbeddingModelChanged` — migration started
- `EmbeddingMigrationCompleted` — swap committed
- `EmbeddingMigrationFailed` — controller entered `Failed`
- `EmbeddingDriftDetected` — drift-check threshold breach (advisory)

All four carry `engine_name` and the relevant `_embedding_models.id`(s) in payload.
None carries `served_by_profile_id` — these are operator/system events, not
profile-served (ADR-032 §3 rule).

### 8. Backward compatibility — one-shot startup migration (V14 + V16)

Deployments predating this ADR have `vec_<engine>` tables without an
`embedding_model` column and no `_embedding_models` rows. The startup
migration runs in two steps, landed in two separate `VersionedMigration`
slots:

**V14 — `embedding_model_registry`** (already shipped):

1. `CREATE TABLE _embedding_models` (per §1 schema).
2. `CREATE UNIQUE INDEX idx_embed_models_one_active`.
3. `CREATE INDEX idx_embed_models_engine_status`.

**V16 — `vector_embedding_model_tag`** (shipped in v022-polish):

4. For each existing regular `vec_*` table (discovered at runtime, validated as
   alphanumeric-suffix only): `ALTER TABLE vec_<engine> ADD COLUMN embedding_model
   TEXT NOT NULL DEFAULT 'all-minilm-l6-v2'`.
5. `CREATE INDEX idx_vec_<engine>_subject_model ON vec_<engine>(subject_id, embedding_model)`.
6. sqlite-vec virtual tables (`vec0`) cannot accept `ALTER TABLE` — handled at
   open time in `khive-db/src/backend.rs` by rebuilding the virtual table with
   the new schema. See §1.1 final paragraph for the operator backup warning;
   a preserving rebuild is the documented follow-up.

Operator population of `_embedding_models` (steps for populating registry rows
from `[[engines]]` config and emitting `EmbeddingModelChanged` events) is a
separate startup-code path tracked in #385, not part of the SQL migrations.

The startup population emits one `EmbeddingModelChanged` event per engine with
`source_model_id = None` and `initiated_by = ConfigDiff` so the audit trail starts
clean.

## Rationale

### Why a separate registry table and not metadata on `[[engines]]`

`[[engines]]` is the operator-declared _configuration_. `_embedding_models` is the
runtime _state_ (what's been activated, what's mid-migration, what's archived). A
single TOML row could represent multiple model versions over the deployment's
lifetime — the configuration says "this engine uses bge-small," the registry says
"version v1.5 was active from T1 to T2, then v2 from T2 onward." Conflating the
two loses history; an audit trail of "what was producing my vectors on April 3?"
needs the registry.

### Why operator-only, no MCP

Agents tune _within_ an embedding space (brain LoRA on rerank, weight calibration
on RecallConfig). Switching the embedding space is a different category — it
invalidates every stored vector and changes the geometry of similarity. Letting an
agent trigger this would be giving the brain power over its own substrate. Hard
no until there's a forcing function that says otherwise.

### Why no auto-migrate on drift

Drift detection is noisy at the boundary. Stored content shifts (new notes, new
entities) cause drift even without a model change. Two competing models can show
oscillating drift scores depending on which corpus sample is drawn. Auto-migrating
on threshold breach creates a feedback loop where the system thrashes between
models. Operator-in-the-loop is the right friction.

### Why per-engine `drift_threshold` rather than global

Different engines hit different drift profiles — a multilingual model on
English-only corpus shows different drift than the same model on a multilingual
corpus. Operator calibrates per-engine; one global threshold over-triggers on
sensitive engines and under-triggers on robust ones.

### Why no `embedding_model_version` knob in v1

Old ADR-040 §6 had `embedding_model_version` as a RuntimeConfig field — letting
deployments pin a specific model version (e.g., bge-small-en-v1.5 specifically,
not whatever the engine resolves). v1 defers this because:

1. Lattice's structured-output testing has not yet established the stability
   contract for specific model versions (operator directive, 2026-05-23).
2. `[[engines]].name` already pins one model identity per engine — a separate
   version knob duplicates that resolution path.
3. When lattice's testing completes, the knob lands as an additive `version: Option<String>`
   field on `EngineConfig` (ADR-031 §D3) — not a new ADR.

Tracked in `.khive/plans/embedding-version-config.md`.

## Alternatives Considered

| Alternative                                                                      | Why rejected                                                                                                                                                                                                                                                                                             |
| -------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Reimplement migration state machine in khive                                     | Lattice ships it; duplication has no upside                                                                                                                                                                                                                                                              |
| Store model id on every record (`notes`, `entities`) row                         | Triple-write cost; the vector table is the right grain — only vectors are model-bound                                                                                                                                                                                                                    |
| Migrate vectors in place (rewrite same table)                                    | Loses atomicity. Failure mid-migration leaves a half-rewritten table with no clean rollback                                                                                                                                                                                                              |
| MCP verb `brain.migrate_model` for agent-triggered migrations                    | Crosses the brain-substrate boundary; risks the feedback loop described in Rationale                                                                                                                                                                                                                     |
| Auto-archive `superseded` rows after N days                                      | Premature; an explicit `khive engine archive --before <date>` is enough                                                                                                                                                                                                                                  |
| ~~Per-record `model_id` on `vec_<engine>` instead of FK to `_embedding_models`~~ | **Superseded by V16 (2026-05-25)**: per-record `embedding_model TEXT` is what V16 actually ships. The supersession chain is preserved via `_embedding_models.superseded_by` joined on `model_id`. See §1.1 for the trade-off rationale (hot-path join cost, end-to-end consistency with kkernel/env-var) |

## Consequences

### Positive

- Model migration is a first-class, auditable, resumable operation.
- The recall path stays simple — one active model per engine, served from one
  table, no cross-model fusion at query time.
- Lattice and khive responsibilities are cleanly split — math vs orchestration.

### Negative

- Migration is bandwidth-heavy: every vector is recomputed. A 10M-row corpus with
  100ms-per-batch embed cost is ~3 hours wall-clock per engine. Operators must
  plan accordingly.
- `vec_<engine>_pending` doubles disk usage transiently. A 50GB vector table
  needs ~100GB free during migration.
- The `superseded` rows in `_embedding_models` accumulate over time. No automatic
  cleanup — relying on `khive engine archive`.

### Neutral

- New event kinds: `EmbeddingModelChanged`, `EmbeddingMigrationCompleted`,
  `EmbeddingMigrationFailed`, `EmbeddingDriftDetected`. Brain folds see these
  events but typically ignore them (they carry no `served_by_profile_id`).
- The startup backward-compat migration emits one `EmbeddingModelChanged` event
  per existing engine. Brain folds replaying history must accept these without
  side effect.

## Implementation

### Crate placement

- `_embedding_models` schema: `khive-runtime` (substrate-shared; both memory and kg
  packs are consumers)
- Migration worker: `khive-runtime::migration` (shares ownership boundary with registry)
- Drift CLI subcommand: `khive-cli::engine` (operator surface)
- Event kinds: `khive-types::events::EventKind` (added to the closed enum)
- Lattice composition: `lattice_embed::migration::MigrationController` consumed
  directly; no wrapper crate

### `MigrationPlanSummary`

`EmbeddingModelChanged` events carry a `MigrationPlanSummary` in their payload. This
is a khive-owned type — derived from but not equal to
`lattice_embed::migration::MigrationPlan`. It carries only the fields the worker needs
at event-dispatch time:

```rust
/// Summary of the migration plan, carried in EmbeddingModelChanged event payloads.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrationPlanSummary {
    pub engine_name:       String,
    pub source_model_id:   String,
    pub target_model_id:   String,
    pub source_dimensions: u32,
    pub target_dimensions: u32,
    pub initiated_by:      String, // actor ref (matches InitiationKind display)
}
```

`MigrationPlanSummary` does not carry `batch_size`, `total_embeddings`, or state
machine fields — those live on `lattice_embed::migration::MigrationPlan` and are
accessed by the worker directly from the `MigrationController`, not from the event
payload.

### Migration version

The ADR-043 schema work landed in two ledger versions in
`crates/khive-db/src/migrations.rs`:

**V14 — `embedding_model_registry`** (cluster-20):

1. `CREATE TABLE _embedding_models` (per §1)
2. `CREATE UNIQUE INDEX idx_embed_models_one_active`
3. `CREATE INDEX idx_embed_models_engine_status`

**V16 — `vector_embedding_model_tag`** (v022-polish):

4. For each existing regular `vec_*` table (runtime-discovered, name-validated):
   - `ALTER TABLE vec_<engine> ADD COLUMN embedding_model TEXT NOT NULL DEFAULT 'all-minilm-l6-v2'`
   - `CREATE INDEX idx_vec_<engine>_subject_model ON vec_<engine>(subject_id, embedding_model)`
5. Startup backfill (run-once code, tracked separately in #385): populate
   `_embedding_models` from `[[engines]]`; per-table model-inferred tag rewrite
   for deployments with non-default models (deferred — see §1.1 final paragraph).

### Worker registration

`khive-runtime::Pack::on_register` adds `EmbedMigrationWorker` to the
runtime's `PackEventConsumer` list. The full trait implementation (ADR-017):

```rust
#[async_trait]
impl PackEventConsumer for EmbedMigrationWorker {
    fn event_filter(&self) -> EventFilter {
        EventFilter { kinds: vec![EventKind::EmbeddingModelChanged], ..Default::default() }
    }

    async fn on_event(
        &self,
        view: &EventView,
        ctx: &RuntimeEventContext,
    ) -> RuntimeResult<()> {
        let plan: MigrationPlanSummary = serde_json::from_value(view.event.payload.clone())?;
        // ... swap protocol per §3
    }
}
```

`EmbeddingModelChanged` events have no observations (per ADR-041 §3 role-mapping
table, operator-emitted event kinds are not projected). The worker reads
`&view.event.payload` only — `view.observations` is empty for this kind.

Note: `EventFilter.kinds: Vec<EventKind>` is defined in ADR-022 §3a (Filter
semantics) alongside `verbs: Vec<String>` as the dual-axis canonical filter. Both
fields are plural-named internally; the `kinds` field lowers to `kind IN (?, ?, …)`
in the SQL WHERE clause, parallel to the `verbs` field. ADR-043 uses `kinds` here
per that canonical shape — no new field is introduced by this ADR.

### CLI subcommands

`khive engine` subcommand group lives in `khive-cli`. Each subcommand wraps a
direct runtime API call:

```rust
match subcommand {
    EngineCmd::List => runtime.list_embedding_models().await,
    EngineCmd::Status { engine } => runtime.engine_status(&engine).await,
    EngineCmd::Migrate { engine, to, resume, abort } => {
        runtime.start_migration(&engine, to, resume, abort).await
    }
    EngineCmd::DriftCheck { engine, sample } => {
        runtime.drift_check(&engine, sample).await
    }
}
```

No MCP surface — the runtime methods are CLI-only via `khive-cli` and admin-only
via `kkernel call`.

## References

- Old khive ADR-040 (Embedding Model Migration) — origin of `_embedding_models`
  and the worker pattern; this ADR is its v1 reincarnation.
- lattice `crates/embed/src/migration/mod.rs` — `MigrationController`,
  `MigrationPlan`, state machine
- lattice `crates/embed/src/types.rs:113–180` — `EmbeddingKey`, `canonical_bytes()`
- lattice `crates/transport/src/drift.rs` — `detect_drift_memories`,
  `detect_drift_records`, `DriftReport`
- ADR-031 §D3 — `[[engines]]` schema, `vec_<engine>` table naming, `EngineConfig`
- ADR-032 §3 — `EventKind` enum (extended here with four new variants)
- ADR-033 §1 — `RecallConfig.fallback_during_migration` (added here)
