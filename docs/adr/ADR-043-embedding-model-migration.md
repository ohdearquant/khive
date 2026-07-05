# ADR-043: Embedding Model Migration

**Status**: accepted
**Date**: 2026-05-23
**Authors**: khive maintainers
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

Per the lattice repository's embed and transport crates:

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
V17 (`vector_embedding_model_tag_preserving_rebuild`) performs the shipped
copy-with-default rebuild: it stages existing rows, recreates the virtual table
with `field` and `embedding_model`, restores the rows, and backfills missing
values to inferred defaults. After migrations, `khive-db/src/backend.rs` refuses
to open an unmigrated vec0 table that still lacks `field` or `embedding_model`
instead of silently dropping data.

### 2. Triggers — shipped state and deferred migration events

Shipped startup code registers configured embedding models directly in `_embedding_models`.
I found no shipped event emission path for startup population. Operator migration and
drift commands are parsable under `kkernel engine`, but `migrate` and `drift-check`
return NotImplemented and point to follow-up #380.

| Source                                                       | Shipped action                                    | Event emitted today |
| ------------------------------------------------------------ | ------------------------------------------------- | ------------------- |
| Engine config at startup                                     | Register active model rows in `_embedding_models` | None found          |
| Operator CLI: `kkernel engine migrate <engine> --to <model>` | Callable stub returns NotImplemented (#380)       | None                |
| Operator CLI: `kkernel engine drift-check <engine>`          | Callable stub returns NotImplemented (#380)       | None                |

`EmbeddingModelChanged`, `EmbeddingMigrationCompleted`, `EmbeddingMigrationFailed`, and
`EmbeddingDriftDetected` remain event enum contracts for the deferred migration worker and
drift implementation.

### 3. `EmbedMigrationWorker` — deferred

No shipped `EmbedMigrationWorker` or `MigrationController` integration was found under
`khive-runtime` or `khive-pack-memory`. The migration state machine, pending-table swap,
resume/abort behavior, and completion/failure event emission are deferred to #380.

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

### 5. Drift detection — deferred

`kkernel engine drift-check <engine> [--sample N]` is a parsable operator command, but the
shipped implementation returns NotImplemented and points to #380. Sampling, Wasserstein
distance computation, `EmbeddingDriftDetected` emission, and `drift_threshold` config are
deferred.

### 6. Verb surface — CLI only

| Command                                            | Shipped status | Purpose                                                     |
| -------------------------------------------------- | -------------- | ----------------------------------------------------------- |
| `kkernel engine list`                              | shipped        | List engines and model history from `_embedding_models`.    |
| `kkernel engine status <engine>`                   | shipped        | Show active model and whether a pending row exists.         |
| `kkernel engine migrate <engine> --to <model>`     | stub           | Returns NotImplemented; migration worker deferred to #380.  |
| `kkernel engine migrate <engine> --resume`         | stub           | Returns NotImplemented; migration worker deferred to #380.  |
| `kkernel engine migrate <engine> --abort`          | stub           | Returns NotImplemented; migration worker deferred to #380.  |
| `kkernel engine drift-check <engine> [--sample N]` | stub           | Returns NotImplemented; drift integration deferred to #380. |

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

**V16 — `vector_embedding_model_tag`**:

4. For each existing regular `vec_*` table (discovered at runtime, validated as
   alphanumeric-suffix only): `ALTER TABLE vec_<engine> ADD COLUMN embedding_model
   TEXT NOT NULL DEFAULT 'all-minilm-l6-v2'`.
5. `CREATE INDEX idx_vec_<engine>_subject_model ON vec_<engine>(subject_id, embedding_model)`.

**V17 — `vector_embedding_model_tag_preserving_rebuild`**:

6. For sqlite-vec virtual tables (`vec0`) missing `field` or `embedding_model`, stage
   existing rows, recreate the virtual table with the full schema, restore rows, and drop
   the staging table. Backend open now errors if an old vec0 table remains unmigrated.

Startup population registers configured embedding models in `_embedding_models` directly.
No shipped startup `EmbeddingModelChanged` emission path was found.

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

- `_embedding_models` schema: `khive-db` migrations and backend registry helpers; runtime
  exposes read access via `KhiveRuntime::list_embedding_models`.
- Migration worker: deferred to #380.
- Engine operator subcommands: `crates/kkernel/src/engine.rs`; the TypeScript `khive`
  CLI has no `engine` group.
- Event kinds: `khive-types::event::EventKind`.
- Lattice migration/drift composition: deferred to #380.

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

The shipped command group is `kkernel engine`, implemented in `crates/kkernel/src/engine.rs`:

```rust
match cmd {
    EngineCommand::List(args) => cmd_engine_list(args).await,
    EngineCommand::Status(args) => cmd_engine_status(args).await,
    EngineCommand::Migrate(args) => cmd_engine_migrate(args),        // NotImplemented (#380)
    EngineCommand::DriftCheck(args) => cmd_engine_drift_check(args), // NotImplemented (#380)
}
```

`list` reads `_embedding_models` through `KhiveRuntime::list_embedding_models`.
`status` is computed in `kkernel` from active/pending registry rows. There is no shipped
`runtime.start_migration` or `runtime.drift_check` API, and no MCP surface.

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
- [ADR-071](ADR-071-backend-pluggable-runtime.md) — `EmbeddingModelRecord` type change (see Amendment A1 below).

## Amendment A1: `EmbeddingModelRecord` replaces `khive_db::EmbeddingModelRegistryRecord` (ADR-071, 2026-06-25)

ADR-043 §Implementation specifies:

> "`_embedding_models` schema: `khive-db` migrations and backend registry helpers; runtime
> exposes read access via `KhiveRuntime::list_embedding_models`."

The current shipped implementation returns `Vec<khive_db::EmbeddingModelRegistryRecord>` from
`list_embedding_models`. `EmbeddingModelRegistryRecord` is a concrete `khive-db` struct,
which leaks a backend-specific type into the runtime's public API.

ADR-071 §5 introduces a runtime-owned type, `EmbeddingModelRecord`, in
`crates/khive-runtime/src/embedding.rs`. `KhiveRuntime::list_embedding_models` returns
`RuntimeResult<Vec<EmbeddingModelRecord>>` after this change.

`EmbeddingModelRecord` carries the same fields as `EmbeddingModelRegistryRecord`. The
conversion is a one-to-one field mapping done at the `khive-db` query boundary. Callers
of `list_embedding_models` (the `kkernel engine list` and `kkernel engine status` subcommands)
update their import from `khive_db::EmbeddingModelRegistryRecord` to
`khive_runtime::EmbeddingModelRecord`.

The `_embedding_models` table schema, migration versions V14/V16/V17, the registry query
logic, and all other ADR-043 mechanics are unchanged. Only the return type of the one public
API method changes.
