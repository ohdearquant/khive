# ADR-028: Pack-Scoped Backends and Per-Pack Schema Declaration

**Status**: accepted\
**Date**: 2026-05-23\
**Authors**: Ocean, lambda:khive

## Context

ADR-009 establishes that v1's multi-backend story is multiple SQLite files in one
process, with pack-scoped backend assignment and cross-backend routing handled by the
SubstrateCoordinator (ADR-029). ADR-017 establishes packs as the unit of vocabulary +
verb-handler composition. This ADR specifies (a) how packs declare their backend
assignment, (b) how each pack declares its schema additions, and (c) how the boot path
instantiates backends, applies schemas, and constructs packs.

### Different storage profiles

A research KG runtime has packs with materially different storage profiles:

| Pack                                      | Profile         | Reason                                                                           |
| ----------------------------------------- | --------------- | -------------------------------------------------------------------------------- |
| `kg`, `gtd`, `memory`, `comm`, `schedule` | Hot, shared     | Cross-pack linking is common (`task → entity`, `memory → entity`)                |
| `lore`                                    | Cold, dedicated | Large corpus (100K+ atoms); rare cross-link to hot data; separate VACUUM cadence |
| `archive`                                 | Read-only       | Frozen historical KG; never written; separate backup cadence                     |

These cannot be served by a single SQLite file without giving up:

- **Separate VACUUM schedules** — running VACUUM on a 100K-atom corpus blocks hot reads
- **Per-domain failure isolation** — corruption in the cold corpus should not corrupt the
  hot KG
- **Independent backup granularity** — `sqlite3 .backup` per file, not per logical slice
- **Per-tier query planner stats** — mixing hot + cold workloads poisons the SQLite
  optimizer's stats

The architecture must satisfy:

1. **Operator-declared backends.** Each backend is a named SQLite file with per-backend
   tuning (cache size, journal mode, read-only). The set of backends is explicit, not
   inferred.
2. **Pack-to-backend is 1:1.** Each pack instance is assigned exactly one backend.
   Multi-tier topologies are modeled as multiple pack instances.
3. **Cross-pack composition on shared backends.** Two packs assigned to the same backend
   compose directly (kg-entity links to memory-note works because they share tables).
4. **Per-pack schema isolation.** Each pack ships its own migrations; collisions on
   shared backends are caught at boot, not at first SQL error.
5. **Per-pack runtime instances.** Each pack receives a `KhiveRuntime` constructed over
   its assigned backend. The runtime sees one backend; the coordinator handles the rest.

## Decision

ADR-028 remains the accepted target design for pack-scoped backends, but the shipped
v1 configuration/runtime surface is narrower:

1. `KhiveConfig` currently parses only `[[engines]]` and `[actor]`.
2. Runtime boot constructs a single `KhiveRuntime` over one default backend.
3. Pack selection is global per ADR-027, not configured per `[packs.<name>]`.
4. `[[backends]]`, `[packs.<name>] backend = ...`, per-pack engine lists, declaration-order
   schema application, and per-pack runtime instances are deferred.
5. Unknown TOML keys are currently ignored by the parser; unsupported backend/pack keys do
   not configure runtime behavior.

The remaining multi-backend sections in this ADR describe the deferred target design unless
explicitly marked as shipped.

### 1. Current shipped configuration schema

The shipped config file is `khive.toml` or `.khive/config.toml` resolved per ADR-035. The
accepted fields today are:

```toml
[[engines]]
name = "default"
model = "all-minilm-l6-v2"
default = true
fusion_weight = 1.0
dims = 384

[actor]
id = "local"
display_name = "Local khive"
```

The following target fields are **deferred** and are not part of the shipped parser:

- `[[backends]]`
- `[packs.<name>]`
- `[packs.<name>].backend`
- per-pack `[packs.<name>].engines`
- backend tuning fields such as `cache_mb`, `journal_mode`, `pragma_synchronous`, and
  `read_only`

Current backend behavior is one default backend, `BackendId::main()`, backed by
`RuntimeConfig::db_path` (`~/.khive/khive.db` by default). The `kkernel backend`
commands expose this single default backend shape while the multi-backend parser/boot path
is deferred.

### 2. Deferred target Rust types

```rust
// crates/khive-config/src/lib.rs  (or in khive-mcp for v1 interim)
#[derive(Debug, Deserialize)]
pub struct AppConfig {
    pub backends: Vec<BackendConfig>,
    pub engines:  Vec<EngineConfig>,
    pub packs:    HashMap<String, PackConfig>,
}

#[derive(Debug, Deserialize)]
pub struct BackendConfig {
    pub name: String,
    #[serde(default)]
    pub kind: BackendKind,
    pub path: Option<PathBuf>,             // ignored when kind = Memory
    #[serde(default)]
    pub cache_mb: Option<usize>,
    #[serde(default = "default_journal_mode")]
    pub journal_mode: String,
    #[serde(default)]
    pub read_only: bool,
}

/// Backend kinds — enum so adding non-SQLite stores does not break the config schema.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BackendKind {
    /// On-disk SQLite (default).
    #[default]
    Sqlite,
    /// In-memory SQLite — for tests and ephemeral deployments.
    Memory,
    // Future kinds (RocksDB, lmdb, custom) extend behind feature flags.
}

#[derive(Debug, Deserialize)]
pub struct PackConfig {
    /// Exactly one backend. References [[backends.name]].
    pub backend: String,
    /// References [[engines.name]]. Empty = no embeddings (CRUD-only pack).
    #[serde(default)]
    pub engines: Vec<String>,
}
```

### 3. `KhiveRuntime` accepts an instantiated backend

```rust
// crates/khive-runtime/src/runtime.rs
pub struct KhiveRuntime {
    backend:   Arc<StorageBackend>,
    embedders: Arc<EmbedderRegistry>,   // per ADR-031, filtered per pack
}

impl KhiveRuntime {
    /// Boot path constructs the backend + registry; runtime wraps them.
    pub fn from_backend(
        backend:   Arc<StorageBackend>,
        embedders: Arc<EmbedderRegistry>,
    ) -> Self;

    /// In-memory backend for tests. Default empty engine registry.
    pub fn memory() -> Result<Self, RuntimeError>;

    /// Access the underlying backend (e.g., to apply schemas during init).
    pub fn backend(&self) -> &StorageBackend;
}
```

`RuntimeConfig` shrinks — the data_path and embedding model move out (per ADR-031). What
remains is per-runtime tuning if any.

### 4. Pack-to-backend is 1:1

Each declared pack instance is assigned **exactly one** backend. Multi-tier topologies
(e.g., hot + cold memory) are modeled as multiple pack instances:

```toml
[packs.memory-hot]
backend = "main"
engines = ["bge-small-en-v1.5"]

[packs.memory-cold]
backend = "archive"
engines = ["bge-small-en-v1.5"]
```

Each pack instance gets one `KhiveRuntime` from its assigned backend. Routing within a
pack (when to write to hot vs cold) is the pack's responsibility, not the backend config's
— and is typically done by deploying two separate packs, not by complicating one. This
keeps "which backend did this write land on?" a config-visible question, not a pack-
internal one.

### 5. Per-pack schema declaration

`Pack` trait gains `schema_plan()`:

```rust
pub trait Pack: Send + Sync {
    fn name(&self) -> &str;
    fn version(&self) -> &str;

    /// Schema plan for this pack's substrate extensions.
    /// None = pure compute (no DB tables beyond shared substrate).
    fn schema_plan(&self) -> Option<&ServiceSchemaPlan>;

    // ... existing trait methods unchanged (ADR-017)
}
```

`ServiceSchemaPlan` already exists in `khive-db::migrations` per ADR-015. Each pack's plan
is applied to its assigned backend at boot, idempotently, with per-pack version tracking
via the existing `_schema_migrations` table (one row per pack-versioned migration).

The substrate / pack-extension boundary:

- **Substrate** (in `khive-db` base migrations): anything inherent to a substrate kind —
  `entities`, `notes`, `events`, `graph_edges`, `vec_*`, `fts_*`.
- **Pack extension** (in the pack's `schema_plan`): anything pack-specific —
  `memory_salience` columns, `task_due_dates`, `brain_posteriors`.

A pack only declares additional tables it needs beyond the substrate. The shared substrate
tables are owned by the substrate layer, not by packs.

### 6. Table naming convention to avoid collisions

When two packs share a backend, their schema plans apply to the same SQLite file. To
avoid collisions:

- **Pack-specific tables**: prefixed with the pack name. e.g., `kg_entity_extensions`,
  `gtd_task_dependencies`, `memory_salience`, `brain_posteriors`.
- **Shared substrate tables** (`entities`, `notes`, `events`, `graph_edges`, `vec_*`,
  `fts_*`): owned by `khive-db` base migrations; packs do not declare or modify them.

The prefix convention is binding for all packs. A pack author who declares a non-prefixed
table commits to substrate-level semantics and must coordinate that change via ADR.

### 7. Schema applied in TOML declaration order

For two packs sharing a backend, their schema plans apply in **TOML declaration order**.
Collisions on table names = boot failure with explicit error naming both packs and the
conflicting table.

```toml
# This order is also the schema-apply order on the shared backend.
[packs.kg]     = { backend = "main", ... }    # kg's plan applies first
[packs.gtd]    = { backend = "main", ... }    # then gtd's plan
[packs.memory] = { backend = "main", ... }    # then memory's plan
[packs.lore]   = { backend = "lore", ... }    # separate backend; independent order
```

Collision policy:

```
SchemaCollision { backend: "main", table: "tasks", packs: ["gtd", "tasktracker"] }
```

The error names both packs and the conflicting table. Boot does not proceed; the operator
must either rename one pack's table (by editing the pack) or move one pack to a different
backend. **Auto-prefixing** (silently renaming `tasks` → `gtd_tasks`) is rejected: it
hides bugs where two packs unintentionally claim the same logical table.

### 8. Deferred target boot sequence

```rust
// crates/khive-mcp/src/main.rs (and kkernel boot)
fn main() -> Result<(), ServerError> {
    let cfg = AppConfig::load(&config_path)?;

    // 1. Construct named backends. Deduped by canonical path.
    let backends: HashMap<String, Arc<StorageBackend>> =
        instantiate_backends(&cfg.backends)?;

    // 2. Construct engine registry. Process-wide; one set of loaded models.
    let all_engines = Arc::new(EmbedderRegistry::from_config(cfg.engines)?);

    // 3. Discover available packs via inventory (ADR-027).
    let registry = PackRegistry::discover();

    // 4. For each declared pack: pick backend, filter engines, apply schema,
    //    construct pack with a fresh KhiveRuntime.
    let mut verb_builder = VerbRegistryBuilder::new();
    for (pack_name, pack_cfg) in &cfg.packs {
        let backend = backends.get(&pack_cfg.backend)
            .ok_or_else(|| ServerError::UnknownBackend(pack_cfg.backend.clone()))?
            .clone();
        let engines = all_engines.filter(&pack_cfg.engines);
        let runtime = KhiveRuntime::from_backend(backend.clone(), engines);

        let pack: Box<dyn PackRuntime> = registry.construct(pack_name, runtime)?;

        // Apply per-pack schema. Idempotent, version-tracked, collision-checked.
        if let Some(plan) = pack.schema_plan() {
            apply_schema_with_collision_check(&backend, pack_name, plan)?;
        }

        verb_builder.register(pack);
    }

    server::serve(verb_builder.build()).await
}

fn instantiate_backends(
    configs: &[BackendConfig],
) -> Result<HashMap<String, Arc<StorageBackend>>, ServerError> {
    // Dedup by canonical path — two declarations of the same path collapse to one Arc.
    let mut by_path: HashMap<PathBuf, Arc<StorageBackend>> = HashMap::new();
    let mut by_name: HashMap<String, Arc<StorageBackend>> = HashMap::new();
    for cfg in configs {
        let canonical = cfg.path.as_ref()
            .and_then(|p| p.canonicalize().ok())
            .unwrap_or_else(|| cfg.path.clone().unwrap_or_default());
        let backend = by_path.entry(canonical).or_insert_with(|| {
            Arc::new(open_backend(cfg).expect("open backend"))
        }).clone();
        by_name.insert(cfg.name.clone(), backend);
    }
    Ok(by_name)
}
```

### 9. Cross-pack composition

- **Same backend**: two packs share substrate tables; cross-pack linking is a direct
  edge insert. No coordinator involvement. This is the common case for hot packs.
- **Different backends**: cross-pack operations route through SubstrateCoordinator
  (ADR-029). The coordinator owns cross-backend edges, fan-out search, and partition
  tolerance.

For a deployment that declares one backend with all packs assigned to it (the default
shape), the coordinator degenerates to a thin pass-through. Multi-backend complexity is
opt-in via TOML.

## Rationale

### Why TOML config, not code-level pack/backend pairing

Pack authors do not know the deployment shape. An operator who wants to put `lore` on a
separate file should not have to recompile or fork the lore pack. The TOML assignment is
the right place for this decision — pack authors define vocabulary; operators define
topology.

### Why 1:1, not 1:N pack-to-backend

1:N would push routing-within-pack semantics into the pack code (when does a write go to
hot vs cold?). Operators can configure this better than pack authors can guess. Splitting
into separate packs makes the assignment explicit and observable.

### Why `BackendKind` enum, not string

oxigraph's `StorageKind { RocksDb, Memory }` adopted as the precedent. An enum adds ~5
LOC and prevents a breaking config-schema change when the second kind arrives. Default
`Sqlite` means existing TOML files without `kind` continue to work.

### Why declaration order, not topological sort, for schema application

Declaration order is operator-controlled and operator-readable. Topological sort would
require packs to declare schema-level dependencies (orthogonal to `Pack::REQUIRES`, which
is vocabulary-level). A pack author who needs to run after another pack documents this in
their pack README; operators place the entries accordingly. Cheap and predictable.

### Why deduplication by canonical path

Two TOML declarations pointing to the same file should not open the file twice. Canonical
path dedup catches `~/.khive/khive.db` and `/home/user/.khive/khive.db` as the same
backend, producing one `Arc<StorageBackend>` shared by both names.

### Why collision is a boot error, not auto-prefix

Auto-prefixing silently renames tables and hides the conflict. Pack authors who declared
the same logical table by mistake need to fix it; pack authors who genuinely meant the
same table (an extension/subclass relationship) need a separate ADR. The collision error
catches both cases at boot, immediately.

## Alternatives Considered

### A. Single backend with namespace-only isolation

Keep one SQLite file; use the existing `namespace` field on every row. Pros: simplest
mental model. Cons: cannot isolate VACUUM, cannot read-only one slice, cannot backup
independently, hot/cold data interleave on disk. khive-internal explicitly rejected this
for the lore use case for these reasons.

Rejected. Namespace is for tenancy; backend is for storage profile.

### B. Backend registry inside `KhiveRuntime`

Runtime holds `HashMap<String, Arc<StorageBackend>>`. Verb dispatch picks a backend by
name. Pros: one runtime instance. Cons: pushes routing into runtime; packs would need to
know backend names; verb dispatch grows a routing concern. Rejected — routing belongs in
the configuration + coordinator layer.

### C. Multiple daemon processes, one per backend

Spin up one `kkernel` per backend. Pros: zero shared state. Cons: loses in-process cross-
pack composition; doubles operational surface; per-process MCP client connections
multiply. Rejected for v1 — process isolation is a future scale-out option.

### D. Pack declares backend in code, not config

Each pack hardcodes its backend name (e.g., `LorePack::BACKEND_NAME = "lore"`). Pros:
pack authors can't be misconfigured. Cons: removes the operator's flexibility; users who
want lore on the main backend (small deployments) lose that choice. Rejected.

### E. Topological sort by `Pack::REQUIRES` for schema order

Reuse the same dependency graph used for vocabulary loading (ADR-027). Pros: one ordering
rule. Cons: vocabulary dependencies and schema dependencies are different concerns. A pack
may consume another pack's vocabulary without needing its tables created first. Declaration
order is the right knob for schemas.

Rejected for schema ordering; `REQUIRES` is for vocabulary only.

### F. Auto-prefix on schema collision

Pros: never blocks boot. Cons: hides bugs (two packs that meant different things end up
sharing the renamed table, or two packs that meant the same thing end up with two parallel
tables). The collision detection forces the operator to resolve the conflict explicitly.

Rejected.

## Consequences

### Positive

- **Multi-backend** — main + lore + archive + arbitrary additional backends in one daemon
  process.
- **Per-backend tuning** — cache size, WAL pragma, read-only set per backend.
- **Cold/hot separation** — corpus on its own file with its own VACUUM schedule.
- **Per-domain failure isolation** — corruption in `lore.db` does not corrupt `main.db`.
- **Per-pack schema isolation** — each pack ships its own migrations; no schema crowding
  in `khive-db`.
- **Composable** — packs sharing a backend link directly; packs on separate backends
  compose through the coordinator (ADR-029).
- **Self-contained packs** — every pack declares (a) backend, (b) engines, (c) schema —
  one place to read what a pack needs operationally.
- **Backup granularity** — one SQLite file per backend; `sqlite3 .backup` per file.

### Negative

- **TOML configuration burden** — new mandatory `khive.toml` with backends + packs
  sections. Mitigation: built-in default config when file missing produces single-backend
  single-engine current behavior.
- **Cross-backend operations are non-atomic** — covered in ADR-029. Documented per-
  operation.
- **Schema collision potential** — two packs sharing a backend that declare the same
  table = boot failure. Mitigated by pack-name prefix convention and clear collision
  message.
- **Two-place editing for new packs** — code-level pack registration (ADR-027) AND
  config-level pack assignment. Mitigation: default config could include all known packs
  pointing at `main` so deployments that do not customize do not need to edit.

### Neutral

- **`khive-db` largely unchanged** — adds `sqlite_read_only` + `apply_pragma` helpers.
- **`khive-runtime` simpler** — `RuntimeConfig` shrinks; constructor becomes
  `from_backend`; no embedder field (ADR-031).
- **Verb dispatch unchanged** — verb→pack mapping resolved at pack construction.
- **MCP wire protocol unchanged** — clients see the same verbs; backend assignment is
  invisible.

## Migration

Single-backend users get auto-upgraded: the built-in default config produces one
`[[backends.main]]` entry pointing at the existing `~/.khive/khive.db`. Existing data is
unchanged. Existing `KhiveRuntime::new(RuntimeConfig)` deprecated but retained for tests;
new code uses `from_backend`.

ADR-029's `target_backend` column on `graph_edges` adds a nullable column with no data
churn for single-backend deployments.

## Open Questions

1. **`AppConfig` crate placement**. Two candidates: (a) `khive-mcp` directly (avoids new
   crate); (b) new `khive-config` crate (reusable from future binaries like a CLI tool).
   Default: (a) for v1, extract later if needed.

2. **Backend health checks at boot**. If a backend file is missing or corrupt, abort the
   whole daemon or skip that backend and load others? Default: abort with explicit error;
   partial boot is a footgun. Operators can comment out backends in TOML to skip.

3. **Pre-existing data migration**. Users with current single-backend deployment have
   data in `~/.khive/khive.db`. The default config keeps that file as `main`, so migration
   is automatic. If they later add lore to a separate file, existing lore-typed data in
   `khive.db` stays put; new lore writes go to `lore.db`. Document this clearly; provide
   a `kkernel migrate-backend` admin command for splitting if needed (future).

4. **Per-backend extension loading**. Different backends might want different SQLite
   extensions (sqlite-vec for vector-using backends, JSON1 for everywhere). Default v1:
   sqlite-vec loaded on every backend (cheap, idempotent). Per-backend extension lists are
   a future ADR if a real need emerges.

5. **Schema downgrade / pack removal**. If a pack is removed from `khive.toml`, its tables
   stay in place (no destructive cleanup). Future: a `kkernel db prune` admin command.
   Not v1 scope.

## References

- [ADR-009](ADR-009-backend-architecture.md) — multi-file SQLite federation; this ADR
  realizes the pack-scoped backend assignment.
- [ADR-015](ADR-015-schema-migrations.md) — `ServiceSchemaPlan` mechanism reused per pack.
- [ADR-017](ADR-017-pack-standard.md) — `Pack` trait gains `schema_plan()`.
- [ADR-027](ADR-027-dynamic-pack-loading.md) — `PackRegistry::discover()` provides
  available packs.
- [ADR-029](ADR-029-substrate-coordinator.md) — cross-backend operations atop the per-pack
  runtimes this ADR constructs.
- [ADR-031](ADR-031-multi-engine-retrieval.md) — `EmbedderRegistry::filter()` produces
  per-pack engine views.
- [ADR-035](ADR-035-cli-config-and-auto-embed.md) — project-vs-user TOML override
  resolution.
- [ADR-071](ADR-071-backend-pluggable-runtime.md) — `BackendHandle` seam (see Amendment A1 below).

## Amendment A1: `BackendHandle` replaces `Arc<StorageBackend>` (ADR-071, 2026-06-25)

ADR-028 §3 specifies `KhiveRuntime { backend: Arc<StorageBackend>, ... }` in its deferred
target Rust types. ADR-071 amends this: the field becomes `handle: BackendHandle`, where
`BackendHandle` is a struct of `Arc<dyn Trait>` handles defined in `khive-runtime`.

The amendment changes two items in ADR-028:

**§3 — `KhiveRuntime` accepts an instantiated backend**

The struct definition and constructor change from:

```rust
pub struct KhiveRuntime {
    backend:   Arc<StorageBackend>,
    embedders: Arc<EmbedderRegistry>,
}

impl KhiveRuntime {
    pub fn from_backend(backend: Arc<StorageBackend>, embedders: Arc<EmbedderRegistry>) -> Self;
    pub fn backend(&self) -> &StorageBackend;
}
```

To:

```rust
pub struct KhiveRuntime {
    handle:      BackendHandle,
    /// `None` when bound to main; `Some(main_handle)` for secondary backends.
    /// See ADR-073 for the core-backend accessor contract.
    core_handle: Option<BackendHandle>,
    embedders:   Arc<EmbedderRegistry>,
}

impl KhiveRuntime {
    /// Boot path constructs a BackendHandle from backend traits; runtime wraps it.
    pub fn from_handle(handle: BackendHandle, embedders: Arc<EmbedderRegistry>) -> Self;

    /// Convenience constructor for the shipped SQLite boot path.
    pub fn from_sqlite(backend: Arc<StorageBackend>, embedders: Arc<EmbedderRegistry>) -> Self;

    /// In-memory backend for tests.
    pub fn memory() -> Result<Self, RuntimeError>;

    /// Return a runtime handle bound to the main (shared-graph) backend. See ADR-073 §2.
    pub fn core(&self) -> KhiveRuntime;

    /// Wire this runtime as a secondary-backend runtime pointing at `main_handle`.
    /// Called by the boot path for non-main pack runtimes. See ADR-073 §3-4.
    pub fn with_core_handle(self, main_handle: BackendHandle) -> Self;
}
```

`KhiveRuntime::backend()` is removed. Code that accessed the backend directly accesses
specific capability handles through `BackendHandle` instead.

**§8 — Deferred target boot sequence**

The boot sequence constructs a `BackendHandle` per pack (via `BackendHandle::from_sqlite`
for the SQLite boot path) instead of an `Arc<StorageBackend>`. `KhiveRuntime::from_handle`
replaces `KhiveRuntime::from_backend` at the pack-instantiation step. For non-main packs,
`with_core_handle` wires the main backend accessor per ADR-073 §4:

```rust
let backend     = backends.get(&pack_cfg.backend)?.clone();
let handle      = BackendHandle::from_sqlite(backend);
let main_handle = BackendHandle::from_sqlite(main_backend.clone());
let runtime     = KhiveRuntime::from_handle(handle, engines);
let runtime     = if pack_cfg.backend != BackendId::MAIN {
    runtime.with_core_handle(main_handle)
} else {
    runtime
};
```

All other ADR-028 mechanics (TOML shape, 1:1 pack-to-backend assignment, schema collision
detection, declaration-order application, cross-pack composition) are unchanged.
