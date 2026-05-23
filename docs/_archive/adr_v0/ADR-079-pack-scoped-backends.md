# ADR-079: Pack-Scoped Backends — Each Pack Owns Its Storage

**Status**: proposed\
**Date**: 2026-05-22\
**Authors**: Ocean, lambda:khive\
**Depends on**: ADR-025 (Pack Standard), ADR-058 (Fold Cognitive Primitives)\
**Partially supersedes**: ADR-013 §"What v0.1 ships" (single-backend implicit assumption),
ADR-061 §"What exists today" (single `khive-runtime` over single backend), ADR-025
§"PackRuntime" (which takes a single runtime by reference)\
**Extended by**: ADR-080 (SubstrateCoordinator) — cross-backend graph operations and
substrate-kind dispatch above the per-pack runtimes this ADR establishes; ADR-085 (Pack
Schema Declaration) — Pack trait gains `schema_plan()` for per-pack schema management on the
backend assigned by this ADR (split out 2026-05-22)

## Context

khive-internal (archived at `.khive/archive/khive-internal/`) operated multiple SQLite
backends concurrently in the MCP daemon. The canonical example is the `lore` service:
~300K-atom cold corpus on its own `lore.db`, isolated from `khive.db` which served hot
data (notes, memory, entities, work, tasks).

Evidence (`apps/cli/src/server/unified.rs:611`):

```rust
fn lore_storage_backend() -> &'static khive_db::StorageBackend {
    static LORE_DB: OnceLock<khive_db::StorageBackend> = OnceLock::new();
    LORE_DB.get_or_init(|| {
        let path = lore_db_path();  // ~/.khive/lore.db
        let db = khive_db::StorageBackend::sqlite(&path)
            .unwrap_or_else(|e| panic!("open lore.db at {path:?}: {e}"));
        if let Err(e) = db.apply_schema(&khive_lore::service::SCHEMA_PLAN) {
            tracing::warn!("lore.db schema apply: {e}");
        }
        db
    })
}

// dispatch picks the backend per verb family:
pub async fn dispatch_lore(_db: &StorageBackend, ...) {
    let backend = lore_backend_for(&args);   // ← lore.db, separate file, separate schema
    ...
}
pub async fn dispatch_memory(db: &StorageBackend, ...) {
    let backend = backend_for(db, &args);    // ← khive.db, shared with kg/gtd/entity
    ...
}
```

This was not incidental. Different services had different storage profiles:

| Service                             | Backend               | Reason                                                                 |
| ----------------------------------- | --------------------- | ---------------------------------------------------------------------- |
| memory, kg, gtd, entity, work, comm | `khive.db` (shared)   | Hot, frequently linked across each other                               |
| lore                                | `lore.db` (dedicated) | Cold corpus; large; rarely linked to hot data; separate VACUUM cadence |

Different services also shipped their own schema plans (`khive_lore::service::SCHEMA_PLAN`),
applied to the backend at service-init time. Migrations were service-scoped, not
process-scoped.

The current open-core port collapsed this:

```rust
// crates/khive-runtime/src/runtime.rs (current)
pub struct KhiveRuntime {
    backend: StorageBackend,         // ← exactly one
    embedder: Arc<OnceCell<...>>,    // ← exactly one (separate regression, see ADR-078)
    // ...
}
impl KhiveRuntime {
    pub fn new(config: RuntimeConfig) -> RuntimeResult<Self>;  // takes ONE data_path
}
```

One `KhiveRuntime` per process, owning one `StorageBackend`. All packs share that one
backend. To host lore on a separate DB today requires spinning up a second daemon process —
which loses in-process cross-pack composition and doubles the operational surface.

This is a regression. Multi-backend was a design property, used in production, dropped
without ADR.

## Decision

**Each pack declares its backend by name in `khive.toml`. The MCP boot reads the
configuration, instantiates the named backends, applies each pack's schema to its assigned
backend, and constructs the pack with a `KhiveRuntime` wrapping that backend.**

Sharing is opt-in via shared backend name. Cross-pack composition (linking, joint queries)
only works when packs are assigned to the same backend. No routing layer is added — the
assignment is resolved at construction time.

### 1. Configuration schema

`~/.khive/khive.toml` — process-wide composition:

```toml
# Backends — named SQLite databases. Multiple packs may share by name.
[[backends]]
name = "main"
path = "~/.khive/khive.db"
cache_mb = 256            # optional, SQLite cache_size in MB
journal_mode = "wal"      # optional, default wal
pragma_synchronous = "normal"   # optional

[[backends]]
name = "lore"
path = "~/.khive/lore.db"
cache_mb = 128

[[backends]]
name = "archive"
path = "~/.khive/archive.db"
read_only = true          # opens with SQLITE_OPEN_READONLY

# Embedding engines (per ADR-078) — process-wide registry.
[[engines]]
name = "bge-small-en-v1.5"
dim = 384
weight = 1.0

[[engines]]
name = "multilingual-e5-small"
dim = 384
weight = 0.8

# Pack configuration — declares backend assignment + engine selection.
[packs.kg]
backend = "main"
engines = ["bge-small-en-v1.5", "multilingual-e5-small"]

[packs.memory]
backend = "main"          # shared with kg → entities can link to memory notes
engines = ["bge-small-en-v1.5", "multilingual-e5-small"]

[packs.gtd]
backend = "main"          # shared with kg → tasks can reference entities
engines = []              # GTD is CRUD, no vectorization

[packs.lore]
backend = "lore"          # dedicated → no cross-link to hot data
engines = ["bge-small-en-v1.5"]   # cheap single engine for cold corpus

[packs.archive]
backend = "archive"
engines = []
```

Project-level override (`.khive/khive.toml`, repo-local) follows the same shape as ADR-057's
user/project resolution: project overrides user; missing keys fall through to user defaults
then to built-in defaults. Built-in default if no config file present: one `[[backends.main]]`
at `~/.khive/khive.db`, default engine list per ADR-078, all known packs assigned to `main`.

### 2. Rust types

`KhiveRuntime` becomes a thin wrapper:

```rust
// crates/khive-runtime/src/runtime.rs (proposed)
pub struct KhiveRuntime {
    backend: Arc<StorageBackend>,
    embedders: Arc<EmbedderRegistry>,   // filtered to this pack's engines (per ADR-078)
}

impl KhiveRuntime {
    /// Construct a runtime from an already-instantiated backend.
    /// MCP boot is responsible for `StorageBackend::sqlite(...)` and schema application.
    pub fn from_backend(
        backend: Arc<StorageBackend>,
        embedders: Arc<EmbedderRegistry>,
    ) -> Self;

    /// In-memory backend for tests. Default empty engine registry.
    pub fn memory() -> Result<Self, RuntimeError>;

    /// Access the underlying backend (e.g., for `apply_schema` during init).
    pub fn backend(&self) -> &StorageBackend;
}
```

`RuntimeConfig` shrinks dramatically — it was largely about the data_path and embedding model.
Both move out. What remains is per-runtime tuning if any (currently none beyond defaults).

`PackRuntime` trait already accepts an `&KhiveRuntime` per verb invocation. The new shape
keeps that surface; the pack just holds its own runtime instance internally.

App configuration:

```rust
// new module — probably crates/khive-config or in khive-mcp (open question 1)
#[derive(Debug, Deserialize)]
pub struct AppConfig {
    pub backends: Vec<BackendConfig>,
    pub engines: Vec<EngineConfig>,
    pub packs: HashMap<String, PackConfig>,
}

#[derive(Debug, Deserialize)]
pub struct BackendConfig {
    pub name: String,
    /// Backend kind (D5). Default: Sqlite.
    #[serde(default)]
    pub kind: BackendKind,
    /// Only used when kind = Sqlite; ignored for Memory.
    pub path: Option<PathBuf>,
    #[serde(default)]
    pub cache_mb: Option<usize>,
    #[serde(default = "default_journal_mode")]
    pub journal_mode: String,
    #[serde(default)]
    pub read_only: bool,
    // ...
}

/// Backend kinds (D5). Enum future-proofs adding non-SQLite stores
/// without breaking the config schema. Only `Sqlite` and `Memory` are wired in v1.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BackendKind {
    /// On-disk SQLite (default).
    #[default]
    Sqlite,
    /// In-memory SQLite — for tests and ephemeral deployments.
    /// `path` field is ignored.
    Memory,
    // Future kinds (RocksDB, lmdb, custom) extend this enum behind feature flags.
}

#[derive(Debug, Deserialize)]
pub struct PackConfig {
    /// Exactly one backend (D6). References [[backends.name]].
    pub backend: String,
    /// References [[engines.name]]. Empty = no embeddings.
    #[serde(default)]
    pub engines: Vec<String>,
}
```

### D5: `BackendKind` enum for future-proofing

`BackendConfig.kind` is an enum, not a string. Only `Sqlite` and `Memory` are wired in v1.
Future kinds (RocksDB, lmdb, custom) extend the enum behind feature flags.

**Why an enum now**: oxigraph's `StorageKind { RocksDb, Memory }` pattern. The schema/parsing cost
is ~5 LOC; not having the enum means a breaking config-schema change when the second kind
arrives. The default value (`Sqlite`) means existing TOML files without `kind` continue to work.

### D6: Pack to backend assignment is 1:1

Each declared pack instance is assigned **exactly one** backend. Multi-tier topologies (e.g.,
hot + cold memory) are modeled as multiple pack instances:

```toml
[packs.memory-hot]
backend = "main"
engines = ["bge-small-en-v1.5"]

[packs.memory-cold]
backend = "archive"
engines = ["bge-small-en-v1.5"]
```

Each pack instance gets one `KhiveRuntime` constructed from its assigned backend. Routing
within a pack (when to write to hot vs cold) is the pack's responsibility, not the backend
config's — and is typically done by deploying two separate packs, not by complicating one.

**Why 1:1**: 1:N adds routing-within-pack semantics (when does write go to hot vs cold?) that
the operator can configure better than we can guess. Splitting into separate packs makes the
assignment explicit in TOML. 1:N also obscures observability — "which backend did this write
land on?" becomes a pack-internal question rather than a config-visible one.

### 3. MCP boot

```rust
// crates/khive-mcp/src/main.rs (sketch)
fn main() -> Result<(), ServerError> {
    let cfg = AppConfig::load(&config_path)?;

    // 1. Construct named backends.
    //    Deduped by (resolved) path — two declarations of the same path collapse to one Arc.
    let backends: HashMap<String, Arc<StorageBackend>> =
        instantiate_backends(&cfg.backends)?;

    // 2. Construct engine registry (per ADR-078). Process-wide.
    let all_engines = Arc::new(EmbedderRegistry::from_config(cfg.engines)?);

    // 3. For each declared pack: pick its backend, filter engines, apply schema,
    //    construct pack with a fresh KhiveRuntime.
    let mut registry = VerbRegistryBuilder::new();
    for (pack_name, pack_cfg) in &cfg.packs {
        let backend = backends.get(&pack_cfg.backend)
            .ok_or_else(|| ServerError::UnknownBackend(pack_cfg.backend.clone()))?
            .clone();
        let engines = all_engines.filter(&pack_cfg.engines);
        let runtime = KhiveRuntime::from_backend(backend.clone(), engines);

        let pack: Box<dyn PackRuntime> = construct_pack(pack_name, runtime)?;

        // Apply per-pack schema. Idempotent, version-tracked per pack.
        pack.apply_schema(&backend).await?;

        registry.register(pack);
    }

    server::serve(registry.build()).await
}
```

`construct_pack` is the registry of known pack constructors — kg, gtd, memory, brain, lore,
archive, channel, calendar, skills, etc. New packs register via the inventory crate (per
ADR-025 §"Pack registration").

### 4. Per-pack schema (split to ADR-085)

The mechanism by which packs declare their schema (`Pack::schema_plan()`), the substrate-vs-
pack-extension boundary, the table-naming convention, and the TOML declaration-order
application policy (originally D7 in this ADR) are owned by [ADR-085](ADR-085-pack-schema-
declaration.md). That ADR is the pack-author contract; this ADR owns the operator-facing
config and runtime wiring.

### 5. Backend instantiation

```rust
// helper, in khive-mcp or khive-config
fn instantiate_backends(
    configs: &[BackendConfig],
) -> Result<HashMap<String, Arc<StorageBackend>>, ServerError> {
    // Dedup by canonical path — same file referenced by two names collapses to one Arc.
    let mut by_path: HashMap<PathBuf, Arc<StorageBackend>> = HashMap::new();
    let mut by_name: HashMap<String, Arc<StorageBackend>> = HashMap::new();

    for cfg in configs {
        let canonical = cfg.path.canonicalize().unwrap_or(cfg.path.clone());
        let backend = by_path.entry(canonical).or_insert_with(|| {
            Arc::new(open_backend(cfg).expect("open backend"))
        }).clone();
        by_name.insert(cfg.name.clone(), backend);
    }

    Ok(by_name)
}

fn open_backend(cfg: &BackendConfig) -> Result<StorageBackend, SqliteError> {
    let mut backend = if cfg.read_only {
        StorageBackend::sqlite_read_only(&cfg.path)?
    } else {
        StorageBackend::sqlite(&cfg.path)?
    };
    if let Some(mb) = cfg.cache_mb {
        backend.apply_pragma("cache_size", &format!("-{}", mb * 1024))?;
    }
    backend.apply_pragma("journal_mode", &cfg.journal_mode)?;
    Ok(backend)
}
```

`StorageBackend` already supports the operations needed; `apply_pragma` and
`sqlite_read_only` are minor additions to `khive-db` (see Phase B in the implementation plan).

### 6. Cross-pack composition (split to ADR-080 sub-ADRs)

Two packs on the same backend compose directly via shared substrate tables and namespace
scoping. Two packs on different backends compose via the **SubstrateCoordinator**
([ADR-080](ADR-080-substrate-coordinator-cross-backend-operations.md) umbrella and its four
sub-ADRs ADR-086/087/088/089). This ADR owns the per-pack runtime construction; ADR-080 owns
the dispatch layer above it.

### What this ADR owns vs what ADR-080/085 own

| Concern                                                                      | Owner                                                      |
| ---------------------------------------------------------------------------- | ---------------------------------------------------------- |
| TOML `[[backends]]` + `[packs.*]` schema                                     | this ADR (§1)                                              |
| `BackendKind` enum + per-backend tuning (D5)                                 | this ADR (Decision section)                                |
| 1:1 pack→backend assignment (D6)                                             | this ADR (Decision section)                                |
| `KhiveRuntime::from_backend` constructor + AppConfig types                   | this ADR (§2)                                              |
| MCP boot sequence (instantiate backends → engines → packs)                   | this ADR (§3, §5)                                          |
| `Pack::schema_plan()` trait extension + collision policy + naming convention | [ADR-085](ADR-085-pack-schema-declaration.md)              |
| Cross-backend edges + locator + link mechanics                               | [ADR-086](ADR-086-cross-backend-edge-representation.md)    |
| Substrate-kind federated search                                              | [ADR-087](ADR-087-substrate-kind-federated-search.md)      |
| Cross-backend traversal + curation semantics                                 | [ADR-088](ADR-088-cross-backend-traversal-and-curation.md) |
| Partition tolerance / degraded reads                                         | [ADR-089](ADR-089-coordinator-partition-tolerance.md)      |

For deployments that only declare one backend with all packs assigned to it (the default
shape), ADR-080's sub-ADR machinery degenerates to a thin pass-through; observable behavior is
identical to a pre-ADR-079 single-backend khive. Multi-backend complexity is opt-in via TOML.

## Alternatives Considered

### A. Backend registry inside `KhiveRuntime`

Runtime holds `HashMap<String, Arc<StorageBackend>>`. Verb dispatch picks a backend by name.
Packs use a `runtime.backend("main")` accessor.

Pros: one runtime instance; one place to look up backends. Cons: pushes routing logic into
runtime; packs need to know backend names (couples pack code to deployment config); verb
dispatch grows a routing concern.

Rejected. Routing belongs in the configuration layer, not the dispatch layer. The pack
already knows its data — let it own the backend.

### B. Multiple daemon processes, one per backend

Spin up one `khive-mcp` process per backend. Each has its own KhiveRuntime, its own packs.

Pros: zero shared state; full process isolation; matches Unix philosophy. Cons: loses
in-process cross-pack composition (kg + memory must talk across IPC); doubles operational
surface; per-process MCP client connections multiply.

Rejected for v1. Process isolation is a future scale-out option; the failure mode it
protects against (one pack OOMing another) is rare and addressable with `cgroups` or
`Resource limits`. Keep single-process for the common case.

### C. Single backend with namespace-only isolation

Keep one SQLite file; use the existing `namespace` field on every row to scope pack data.

Pros: simplest mental model; no schema changes. Cons: doesn't isolate VACUUM, can't
read-only one slice, can't backup independently, hot/cold data interleave on disk and
fragment together, query planner stats poisoned by mixed workload. khive-internal explicitly
rejected this for lore for these reasons.

Rejected. Namespace isolation is sufficient for tenancy; it is not sufficient for
storage-profile isolation.

### D. Tenancy: one backend per tenant

`backend.path = "~/.khive/tenants/{tenant_id}.db"` — every tenant gets a private DB.

Pros: hard tenancy isolation; per-tenant backup and quota. Cons: orthogonal to this ADR;
the multi-backend mechanism this ADR introduces is the prerequisite, not the policy. A
future ADR can layer tenant-per-backend on top.

Deferred. This ADR establishes the mechanism (multi-backend); tenant-per-backend is a policy
that could be added once the mechanism lands.

### E. Pack declares backend in code, not config

Each pack hardcodes its backend name (e.g., `LorePack::BACKEND_NAME = "lore"`). The TOML
provides backend definitions, but packs pick names themselves.

Pros: pack authors can't be misconfigured into the wrong backend. Cons: removes the
configuration knob; users who want lore on the main backend (small deployments) lose that
flexibility; testing requires recompiling.

Rejected. The TOML assignment is the right place for this decision — pack authors don't
know the deployment shape, operators do.

## Consequences

### Positive

- **Multi-backend restored** — main + lore + arbitrary additional backends, all in one
  daemon process
- **Per-backend tuning** — cache size, WAL pragma, read-only mode set per backend
- **Cold/hot separation** — lore corpus on its own file with its own VACUUM schedule
- **Per-domain failure isolation** — corruption in lore.db doesn't affect khive.db
- **Per-pack schema isolation** (per ADR-085) — each pack ships its own migrations; no schema crowding
- **Composable** — packs sharing a backend can link cross-pack (today's pattern); packs on
  separate backends are isolated (the gain)
- **Self-contained packs** — every pack now declares (a) backend, (b) engines, (c) schema —
  one place to read what a pack needs operationally
- **Backup granularity** — one SQLite file per backend; `sqlite3 .backup` per file

### Negative

- **TOML configuration burden** — new mandatory `khive.toml` with backends + packs sections.
  Mitigation: built-in default config when file missing produces single-backend single-engine
  current-behavior shape
- **Cross-backend operations are non-atomic** — `link` is per-backend atomic, but `hard_delete`'s
  incoming-edge cascade and any future cross-backend coordinator operation crosses SQLite
  transaction boundaries. The cross-backend layer has eventual-consistency semantics, not
  ACID. Documented per-operation in ADR-080 §D11/§D12. Mitigation: operators with
  consistency-critical workloads should keep dependent data on the same backend; ADR-080's
  coordinator surfaces cross-backend metrics so this stays observable.
- **Cross-backend merge unsupported in v1** — `merge_entity` across backends returns
  `CrossBackendMergeUnsupported`. Workaround: export+delete+import manually, or co-locate the
  packs whose entities you intend to merge.
- **Schema collision potential** — two packs sharing a backend that declare the same
  table name = boot failure. Owned by ADR-085's collision policy + pack-name table prefix
  convention.
- **Migration step for existing deployments** — single-DB users get auto-upgraded to one
  `[[backends.main]]` entry; existing data unchanged. One-time config write on first run.
  ADR-080's `target_backend` column adds nullable column to `graph_edges` (no data churn).
- **Two-place editing** — adding a new pack means both a code-level pack registration AND a
  config-level pack assignment. Mitigation: default config could include all known packs
  pointing at `main` so deployments that don't customize don't need to edit

### Neutral

- **`khive-db` largely unchanged** — `StorageBackend::sqlite()` already supports the file
  case; adds `sqlite_read_only` + `apply_pragma` helpers
- **`khive-runtime` simpler** — `RuntimeConfig` largely empty; constructor becomes
  `from_backend`; no embedder field (per ADR-078)
- **Verb dispatch unchanged** — verb→pack mapping already exists; backend selection is
  resolved at pack construction, transparent to dispatch
- **MCP wire protocol unchanged** — clients see the same verbs; backend assignment is
  invisible to clients

## Migration Plan

Per ADR-058 phasing convention, three steps that each leave the build green:

**Phase B1 — `KhiveRuntime::from_backend` constructor (no behavior change)**

1. Add `KhiveRuntime::from_backend(Arc<StorageBackend>, Arc<EmbedderRegistry>) -> Self`
   alongside existing `new(RuntimeConfig)`. The latter delegates to the former for now.
2. Add `KhiveRuntime::backend() -> &StorageBackend` accessor.
3. Existing callers unchanged. Ships behind no flag.

**Phase B2 — Config + per-pack wiring (single-backend by default)**

1. Add `AppConfig` + TOML loader (in `khive-mcp` or new `khive-config` crate).
2. Add built-in default config: one `[[backends.main]]` at `~/.khive/khive.db`, all known
   packs on `main`.
3. Update MCP boot to: load config → instantiate backends → construct one KhiveRuntime per
   pack from the assigned backend → register pack.
4. Default behavior identical to current: all packs share `main`. No user-visible change.
5. Existing `KhiveRuntime::new(RuntimeConfig)` deprecated but kept for tests.

**Phase B3** (per-pack schema declaration) is owned by [ADR-085](ADR-085-pack-schema-
declaration.md) — see that ADR's "Migration Plan" section for details.

Phases B1 and B2 are independently shippable, each leaving the build green. B1 lands the
abstraction; B2 lands the wiring without changing behavior; B3 (per ADR-085) turns on per-pack
schema declaration once B2's multi-backend boot is in place.

This phase order interleaves with ADR-078's phases (engine extraction). Both ADRs share the
`KhiveRuntime::from_backend` signature — Phase B1 must land before ADR-078's runtime-API
change (Phase D) can avoid touching the same lines twice. It also interleaves with ADR-080
sub-ADRs B4 (coordinator) and B5 (target_backend column).

## Open Questions

1. **`AppConfig` lives in which crate?** Two candidates: (a) `khive-mcp` directly (avoids
   new crate, but couples config to MCP binary); (b) new `khive-config` crate (reusable from
   future binaries like a CLI tool). Default: (a) for v1, extract later if needed.

2. **Backend health checks at boot** — if a backend file is missing or corrupt, abort the
   whole daemon or skip that backend and load others? Default: abort with explicit error;
   partial boot is a footgun. Operators can comment out backends in TOML to skip.

3. **Pre-existing data migration** — users with a current single-backend deployment have data
   in `~/.khive/khive.db`. The default config keeps that file as `main`, so migration is
   automatic. But if they later add lore to a separate file, existing lore-typed data in
   `khive.db` stays where it is; new lore writes go to `lore.db`. Document this clearly;
   provide a `khive migrate-backend` CLI for splitting if needed (future).

4. **In-memory backends in config** — should `path = ":memory:"` work for ephemeral testing?
   Default: yes; the `StorageBackend::memory()` path is already there.

(Original OQ-2 "schema collision policy" → moved to [ADR-085](ADR-085-pack-schema-declaration.md);
original OQ-6 "per-backend extensions" → moved to ADR-085's Open Questions.)

## References

- `apps/cli/src/server/unified.rs:556-664` (khive-internal) — `backend_for`,
  `lore_backend_for`, `lore_storage_backend`
- `apps/cli/src/server/unified.rs:730-848` (khive-internal) — per-verb-family dispatch
  picking backend (`dispatch_lore` vs `dispatch_memory`)
- `platform/db/src/backend/mod.rs:54-59` (khive-internal) — `StorageBackend` struct
- `platform/service/src/backend.rs:420` (khive-internal) — `ServiceBackend` composition over
  capability traits
- `crates/khive-db/src/backend.rs` (current) — single-backend regression
- `crates/khive-runtime/src/runtime.rs:18-238` (current) — single-backend `KhiveRuntime`
- `/Users/lion/projects/_references/oxigraph/lib/oxigraph/src/storage/mod.rs:50-90` —
  `StorageKind { RocksDb, Memory }` pattern adopted by D5
- ADR-013 (this repo) — retrieval port scope; superseded for single-backend assumption
- ADR-022 (this repo) — schema migrations; used for per-pack schema plans and ADR-080's
  `target_backend` column add
- ADR-025 (this repo) — Pack Standard; this ADR extends to include backend declaration
- ADR-057 (this repo) — CLI config layering; this ADR follows the same project/user override
  pattern for `khive.toml`
- ADR-061 (this repo) — retrieval infrastructure; superseded for single-runtime assumption
- ADR-069 (this repo) — request batch; same-backend composition still uses batching
- ADR-076 (this repo) — kernel/MCP split; ADR-080's coordinator lives in `kkernel`
- ADR-078 (this repo) — multi-engine embedding architecture; shares the
  `KhiveRuntime::from_backend` signature
- ADR-080 (this repo) — SubstrateCoordinator umbrella; cross-backend graph operations and
  substrate-kind dispatch above the per-pack runtimes this ADR establishes
- ADR-085 (this repo) — Pack Schema Declaration; the `Pack::schema_plan()` trait extension and
  collision/naming-convention rules that this ADR defers to
- ADR-086 / 087 / 088 / 089 (this repo) — Coordinator sub-ADRs that operate on top of the
  per-pack runtimes this ADR constructs
