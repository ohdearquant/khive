# khive-runtime

Composable Service API used by khive's daemon, MCP server, and CLI: entity/note CRUD,
graph traversal, hybrid search, and curation, plus the pack registration and
verb-dispatch machinery that lets packs (`kg`, `gtd`, `memory`, …) extend the surface.

## Features

- **`KhiveRuntime`** — a cloneable handle wrapping a `khive-db::StorageBackend` with
  namespace-scoped accessors for every storage capability, plus a lazily-configured
  embedder registry
- **`BlobHydrator`** — one store-paired, runtime-shared weighted byte budget for
  bounded, digest-verified whole-blob reads. Its non-cloneable `VerifiedBlob`
  keeps admission until callers finish using the borrowed bytes, and tracked
  supervisors keep native work visible to daemon drain after request cancellation
- **Role-keyed attachments** — main-backend-only record metadata over `ContentRef`,
  atomic entity-plus-role publication, compatibility `content_ref` projection,
  and transactional hard-delete cleanup
- **Complete pack-owned embedding-space binding** — consumers of
  `vectors_for_embedding_space` pass the immutable
  `khive_storage::EmbeddingSpaceIdentity`; that seam derives no table from a
  display model name and verifies existing geometry/model metadata. The text
  provider cutover remains ADR-160 Phase 7
- **`VerbRegistry` / `VerbRegistryBuilder`** — registers packs (`PackRuntime` impls),
  an authorization `Gate`, an actor identity, and dispatches verbs by name
- **`PackRuntime` trait** — the object-safe runtime counterpart to `khive-types::Pack`;
  every pack declares handlers, owned entity/note kinds, edge-endpoint extensions, and
  an optional auxiliary `SchemaPlan`
- **Curation** (`EntityPatch`, `NotePatch`, `EdgePatch`, `MergeSummary`,
  `MergeEdgeConflictPreimage`, `MergeEdgePreimage`, `EntityDedupMergePolicy`) —
  update/merge semantics, including reversible natural-key edge-conflict drops, per
  [ADR-014](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-014-curation-operations.md)
- **Retrieval objectives** (`RrfFusionObjective`, `VectorSimilarityObjective`,
  `TextRelevanceObjective`, `TemporalRecencyObjective`,
  `AmplifiedDecayAwareSalienceObjective`, …) composed into a `MemoryRecallPipeline`, plus
  `DecayAwareSalienceObjective` for standalone fixed-rate scoring
- **Graph traversal** (`PathNode`) and **validation** (`ValidationRule`,
  `ValidationReport`, `Violation`) for domain-specific graph-shape rules
- **Daemon** (unix only) — `run_daemon`, socket/pid path helpers, and the
  request/response frame types for the persistent `kkernel mcp --daemon` process

## Usage

```rust
use khive_runtime::{KhiveRuntime, RuntimeConfig};
use khive_types::namespace::Namespace;

// In-memory runtime (tests and pure local embedding). Production callers use
// khive-mcp/kkernel's async host builders so legacy V21 attachment cutover is
// completed before any runtime is exposed.
let runtime = KhiveRuntime::new(RuntimeConfig::default())?;

// Every read/write is scoped by a NamespaceToken minted through the configured Gate.
let token = runtime.authorize(Namespace::local())?;
let entities = runtime.entities(&token)?; // Arc<dyn khive_storage::EntityStore>
let graph = runtime.graph(&token)?; // Arc<dyn khive_storage::GraphStore>
```

`KhiveRuntime::new` is suitable for fresh, exact-current, and in-memory
databases. It deliberately refuses a legacy V20 database that needs verified
application-assisted migration. Official production boot uses the async
`khive_mcp::serve::build_single_backend_runtime` or multi-backend builders and
then `KhiveRuntime::from_prepared_backend`. The infallible `from_backend`
constructor is a low-level assembly seam whose caller must already have
completed V21; it is not a migration or serving entrypoint.

Before any Phase-4b builder is deployed, the Phase-4a GC compatibility build
must converge on every process sharing the database/blob root and all pre-Phase-4a
processes must be drained. Phase 4a leaves V20 schema/data unchanged and only
fail-closes transactional GC unless the database is exact completed V21.
Every Phase-4a application-serving/read-write process must also be quiesced for
cutover, or proven unable to access the database. Only a GC-only worker has
narrow completed-V21 compatibility; start Phase-4b serving after exact-current
topology validation.

Artifact publication uses `create_entity_with_attachments`; the former
single-column `create_entity_with_content_ref` seam is removed. Packs assigned
to a secondary database must call `runtime.core().attachments()` because only
the canonical main database participates in blob GC liveness.

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
     ┌──────────────────┼─────────────────────────────┐
authorize(ns)     entities/graph/notes/…   BlobHydrator / embedder(name)
     │             (khive-storage traits)  (bounded bytes / lattice-embed)
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
                     fail closed)
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

Apache-2.0.
