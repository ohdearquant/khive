# khive-runtime

Composable Service API used by khive's daemon, MCP server, and CLI: entity/note CRUD,
graph traversal, hybrid search, and curation, plus the pack registration and
verb-dispatch machinery that lets packs (`kg`, `gtd`, `memory`, …) extend the surface.

## Features

- **`KhiveRuntime`** — a cloneable handle wrapping a `khive-db::StorageBackend` with
  namespace-scoped accessors for every storage capability, plus a lazily-configured
  embedder registry
- **`VerbRegistry` / `VerbRegistryBuilder`** — registers packs (`PackRuntime` impls),
  an authorization `Gate`, an actor identity, and dispatches verbs by name
- **`PackRuntime` trait** — the object-safe runtime counterpart to `khive-types::Pack`;
  every pack declares handlers, owned entity/note kinds, edge-endpoint extensions, and
  an optional auxiliary `SchemaPlan`
- **Curation** (`EntityPatch`, `NotePatch`, `EdgePatch`, `MergeSummary`,
  `EntityDedupMergePolicy`) — update/merge semantics per
  [ADR-014](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-014-curation-operations.md)
- **Retrieval objectives** (`RrfFusionObjective`, `VectorSimilarityObjective`,
  `TextRelevanceObjective`, `TemporalRecencyObjective`, `DecayAwareSalienceObjective`, …)
  composed into a `MemoryRecallPipeline`
- **Graph traversal** (`PathNode`) and **validation** (`ValidationRule`,
  `ValidationReport`, `Violation`) for domain-specific graph-shape rules
- **Daemon** (unix only) — `run_daemon`, socket/pid path helpers, and the
  request/response frame types for the persistent `kkernel mcp --daemon` process

## Usage

```rust
use khive_runtime::{KhiveRuntime, RuntimeConfig};
use khive_types::namespace::Namespace;

// In-memory runtime (tests); production callers set RuntimeConfig::db_path or
// use KhiveRuntime::from_backend with a pre-built StorageBackend.
let runtime = KhiveRuntime::new(RuntimeConfig::default())?;

// Every read/write is scoped by a NamespaceToken minted through the configured Gate.
let token = runtime.authorize(Namespace::local())?;
let entities = runtime.entities(&token)?; // Arc<dyn khive_storage::EntityStore>
let graph = runtime.graph(&token)?; // Arc<dyn khive_storage::GraphStore>
```

Packs are composed through the builder, not `KhiveRuntime` directly:

```rust
use khive_runtime::{VerbRegistryBuilder, GateRef, AllowAllGate};
use std::sync::Arc;

let mut builder = VerbRegistryBuilder::new();
builder
    .with_gate(Arc::new(AllowAllGate) as GateRef)
    .with_default_namespace("local");
    // .register(KgPack::new(...))  // any Pack + PackRuntime impl
let registry = builder.build()?;

let result = registry
    .dispatch("search", serde_json::json!({"kind": "entity", "query": "LoRA"}))
    .await?;
```

## Architecture

```text
              KhiveRuntime::new(RuntimeConfig)
                        │
              StorageBackend (khive-db)
                        │
     ┌──────────────────┼──────────────────────┐
authorize(ns)     entities/graph/notes/…   embedder(name)
     │             (khive-storage traits)  (lattice-embed)
     ▼
NamespaceToken ──── VerbRegistryBuilder::register(pack) × N
                         │
                    VerbRegistryBuilder::build()
                         │
                    VerbRegistry::dispatch(verb, params)
                         │           │
                    Gate::check  first pack whose handlers() match verb
                    (authoritative
                     Deny; errors
                     fail open)
```

`dispatch` short-circuits to `describe_verb` when `params["help"] == true`, otherwise
resolves the request namespace (explicit `namespace` arg, else the registry default),
checks the `Gate`, and routes to the first registered pack whose `HandlerDef`s cover
the verb. `KhiveRuntime::authorize` mints a `NamespaceToken` whose read-visibility set
defaults to `[ns]`; `authorize_with_visibility` widens it for callers that read across
namespaces (e.g. an agent reading both its own and a shared namespace) while writes
stay pinned to the primary.

## Where this sits

`khive-runtime` sits directly above `khive-db`/`khive-query`/`khive-gate`/`khive-fusion`
and below every pack crate:

```text
types -> score -> storage -> db -> query -> runtime -> pack-kg / pack-gtd / … -> mcp
```

It re-exports the `khive-db` and `khive-gate` types packs need
(`StorageBackend`, `ConnectionPool`, `Gate`, `GateDecision`, `ActorRef`, …) so most
pack crates depend on `khive-runtime` alone rather than reaching past it. Governing
ADRs: pack contract and object-safe dispatch
([ADR-017](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-017-pack-standard.md)),
verb surface, visibility and composition
([ADR-023](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-023-declarative-pack-format.md)),
dynamic pack loading via self-registration
([ADR-027](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-027-dynamic-pack-loading.md)),
pack-scoped backends and per-pack schema
([ADR-028](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-028-pack-scoped-backends.md)),
and the authorization gate
([ADR-018](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-018-authorization-gate.md)).

## License

BUSL-1.1. See the repository [LICENSE](https://github.com/ohdearquant/khive/blob/main/LICENSE).
