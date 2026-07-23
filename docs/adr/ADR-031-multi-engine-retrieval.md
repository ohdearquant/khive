# ADR-031: Multi-Engine Retrieval: Embedder Registry, Configuration, and Pack Orchestration

**Status**: accepted\
**Date**: 2026-05-23\
**Authors**: khive maintainers\
**Supersedes**: ADR-011 §"single-embedder direction"\
**Depends on**: ADR-005 (Storage Capability Traits), ADR-030 (Retrieval Stack Port)\
**Related**:

- ADR-012 (Retrieval Composition): composes over this layer
- ADR-024 (Deterministic Fold Primitives): Objective implementations operate on candidates this layer produces
- ADR-028 (Pack-Scoped Backends): pack `engines = [...]` in `khive.toml` drives `filter()`
- ADR-029 (SubstrateCoordinator): backend-level unweighted RRF is distinct from the engine-level weighted RRF here
- ADR-035 (CLI Config): project-vs-user TOML override semantics extended by this ADR

## Context

Embedding quality varies across languages and corpus types. A single configured model cannot
be assumed to dominate every workload, and raw similarity scores from different models are not
directly comparable. The runtime therefore needs an explicit multi-engine contract: peer
embedding services, one vector index per engine identity, and deterministic fusion of
ranked results.

### What "multi-engine" means

Distinct from multi-actor namespace isolation (per ADR-007) and distinct from model migration
(single model, swap atomically). Multi-engine means:

- N peer embedding services run concurrently in the same process
- Each service may be a different provider (native, hosted API, or custom)
- Each service has its own vector index: one `vec_*` table per (model_id, dim)
- Every write embeds with all N services and stores in all N indices
- Every query embeds with all N services, searches all N indices in parallel, then fuses
- Results merge via weighted RRF using per-engine weight
- Engine weights are explicit inputs to weighted fusion

### Why "engine" not "model"

A model is an `EmbeddingModel` variant: a specific weights file with a specific
dimensionality. An engine is a complete embedding service: trait implementation, model handle,
cache, concurrency policy, provider semantics (local inference vs. HTTP API). Two engines can
implement the same model (local BGE vs. remote BGE); one engine can serve only one model (each
`Embedder` instance is pinned, per D1 below). The ADR uses "engine" as the substitutable unit
and failure-isolation boundary.

### Layer map

```
┌─────────────────────────────────────────────────────────────────────────┐
│  kkernel binary                                                          │
│  - Reads [[engines]] from khive.toml at startup (D3)                    │
│  - Constructs EmbedderRegistry once; holds Arc<EmbedderRegistry>        │
│  - Per-pack filter() applied at pack construction (D2)                  │
└─────────────────────────────────────────────────────────────────────────┘
                                    ↓
┌─────────────────────────────────────────────────────────────────────────┐
│  Pack handlers (pack-kg and others)          (D5)               │
│  - embed_query_all → per-engine search → normalization → weighted RRF   │
│  - embed_document_all → upsert_vector per engine                        │
│  - Pack-specific scoring layered on top      │
└─────────────────────────────────────────────────────────────────────────┘
                                    ↓
┌─────────────────────────────────────────────────────────────────────────┐
│  KhiveRuntime                                        (D4)               │
│  - Holds Arc<EmbedderRegistry> (filtered) for metadata access only      │
│  - No embedder field or direct provider dependency                      │
│  - vector_search(ns, model_id, query_vec, top_k, kind): model_id       │
│    routes to vec_{snake(model_id)} table; no embedding generation here  │
│  - upsert_vector(ns, model_id, entity_id, vector)                       │
└─────────────────────────────────────────────────────────────────────────┘
                                    ↓
┌─────────────────────────────────────────────────────────────────────────┐
│  khive-runtime embedding modules                     (D1, D2)           │
│  - Embedder trait, EmbedderRegistry, EngineConfig                       │
│  - Native provider adapter behind an optional feature                   │
│  - filter() returns Arc<EmbedderRegistry-subset>; engine Arcs shared    │
│  - vec_model_key(): canonical (model_id, dim) → table name mapping     │
└─────────────────────────────────────────────────────────────────────────┘
                                    ↓
┌─────────────────────────────────────────────────────────────────────────┐
│  khive-storage / khive-db                            (ADR-005, ADR-030)  │
│  - VectorStore and TextSearch traits                                     │
│  - vectors_for_namespace(model_key, dim, ns): per-(model, dim) tables  │
│  - HnswIndex, Bm25Index, FusionStrategy from ADR-030                   │
└─────────────────────────────────────────────────────────────────────────┘
```

## Decision

This ADR consolidates five decisions, D1 through D5. Their order reflects their dependency
sequence: interface, registry, configuration, runtime routing, and handler orchestration.

### D1: `Embedder` trait: provider-agnostic, one-model-per-instance

```rust
// khive-runtime embedding interface
#[async_trait]
pub trait Embedder: Send + Sync {
    /// Canonical engine identifier (e.g., "bge-small-en-v1.5"). Stable;
    /// used as vector table key suffix and as cache-key component.
    fn model_id(&self) -> &str;

    /// Output vector dimension. Must equal every vector returned by embed().
    fn dim(&self) -> usize;

    /// Embed a batch of texts. Returns one Vec<f32> per input, in order.
    async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError>;

    /// Query-side prefix for asymmetric retrieval (E5: "query: "; Qwen3: instruction).
    /// Applied by the registry before calling embed(); default None.
    fn query_prefix(&self) -> Option<&'static str> { None }

    /// Document-side prefix applied at storage time. Default None.
    fn document_prefix(&self) -> Option<&'static str> { None }
}
```

Three invariants:

1. **One model per instance.** An `Embedder` is pinned to a single (model_id, dim). N peer
   engines = N `Embedder` instances. This matches the
   `NativeEmbeddingService::with_model(model)` construction pattern.
2. **Provider-agnostic.** A native implementation, a hosted API adapter, or a custom
   provider implements the same trait without modifying `khive-runtime` or `khive-db`.
3. **Asymmetric retrieval built in.** Query and document prefixes are first-class because
   some model families require different input forms. The registry applies prefixes;
   callers do not.

The native provider adapter remains behind a feature so builds that supply another provider
need not link the native model runtime.

### D2: `EmbedderRegistry`: process-wide, filtered per pack via `Arc::filter()`

```rust
// khive-runtime registry interface
pub struct EmbedderRegistry { /* internal: Vec<RegisteredEngine> */ }

struct RegisteredEngine {
    config:  EngineConfig,
    service: Arc<dyn Embedder>,
}

impl EmbedderRegistry {
    pub fn from_config(configs: Vec<EngineConfig>) -> Result<Self, EmbedError>;

    /// All engines in TOML declaration order. First entry is the "primary" engine
    /// for single-model paths (reranker dispatch, CLI embed command).
    pub fn engines(&self) -> &[RegisteredEngine];

    /// Parallel embed: query side; applies query_prefix per engine.
    pub async fn embed_query_all(&self, text: &str)
        -> Result<Vec<(EngineConfig, Vec<f32>)>, EmbedError>;

    /// Parallel embed: document side; applies document_prefix per engine.
    pub async fn embed_document_all(&self, text: &str)
        -> Result<Vec<(EngineConfig, Vec<f32>)>, EmbedError>;

    /// Embed with a specific engine by model_id (no prefix applied).
    pub async fn embed_one(&self, model_id: &str, text: &str)
        -> Result<Vec<f32>, EmbedError>;

    /// Look up engine config by model_id (for table routing metadata).
    pub fn get(&self, model_id: &str) -> Option<&EngineConfig>;

    /// Return a new registry exposing ONLY engines whose model_id is in `allow`.
    /// Engine Arcs are SHARED with self: filter is a view, not a clone.
    pub fn filter(self: &Arc<Self>, allow: &[String]) -> Arc<EmbedderRegistry>;
}
```

**D2 core property**: kkernel constructs `EmbedderRegistry` once from the `[[engines]]` array.
Each pack declares the engines it uses (per ADR-028: `engines = ["bge-small-en-v1.5"]`). At
pack construction, kkernel calls `registry.filter(&pack_cfg.engines)` and passes the filtered
`Arc<EmbedderRegistry>` to the pack's `KhiveRuntime::from_backend`. Engine Arcs are shared:
the filter is a view into the parent registry, not a copy of the models.

This gives:

- **Model locality**: BGE is loaded once across all consuming packs.
- **Cache locality**: a query against one pack warms the BGE LRU cache for other packs.
- **Pack autonomy**: a pack declaring `engines = []` cannot invoke an unconfigured engine.

Engine failure semantics: `embed_query_all` returns a partial list when one engine errors.
If at least one engine succeeds, the search proceeds with the available engines. If all
engines fail, the request fails. This is an availability-over-strict-consistency policy.
A future `embed_query_all_strict()` variant may be added for operators
who need all-or-nothing behavior.

### D3: `[[engines]]` TOML schema, vector table naming, and fallback

Engines are declared as a TOML array in `.khive/config.toml` or in a file selected with
`--config` or `KHIVE_CONFIG`:

```toml
[[engines]]
name = "primary"
model = "all-minilm-l6-v2"
default = true
fusion_weight = 1.0
dims = 384

[[engines]]
name = "multilingual"
model = "paraphrase-multilingual-minilm-l12-v2"
fusion_weight = 0.8
dims = 384
```

The serialized type in `khive-runtime` is:

```rust
#[derive(Debug, Clone, Deserialize)]
pub struct EngineConfig {
    pub name: String,
    pub model: String,
    pub default: bool,
    pub fusion_weight: Option<f64>,
    pub dims: Option<u32>,
}
```

Configuration validation requires exactly one default engine, unique names, and a finite
positive `fusion_weight` whenever a weight is present. `dims` is an optional consistency
check; the selected embedding implementation remains authoritative for dimensions.

The project-local file is the default file-based surface. Environment variables remain a
fallback when no file is found. If both sources are present, the file wins. The file defines
the complete engine list; entries are not merged with environment-derived engines.

`RuntimeConfig::embedding_model` and `additional_embedding_models` provide compatibility for
callers that do not use the file-based surface. The default engine becomes
`embedding_model`, and the remaining declared engines retain declaration order in
`additional_embedding_models`.

Vector indexes are dimension-fixed, so each `(model_id, dimension)` pair has its own table.
`vec_model_key` replaces non-alphanumeric characters with `_` to derive a stable table-key
component. Implementations must validate model identity and dimensions before reusing a
persisted vector table.

The legacy `vec_default` migration is idempotent. It renames the table only when the target
model table does not already exist; otherwise the target table wins and the legacy table is
left for explicit cleanup.

### D4: Runtime API: caller-computed vectors, `model_id` routing, no embedder ownership

`KhiveRuntime` no longer constructs or owns embedders:

```rust
// crates/khive-runtime/src/runtime.rs
pub struct KhiveRuntime {
    backend:   Arc<StorageBackend>,
    embedders: Arc<EmbedderRegistry>,  // metadata access only; no embed() calls here
}

impl KhiveRuntime {
    pub fn from_backend(
        backend:   Arc<StorageBackend>,
        embedders: Arc<EmbedderRegistry>,
    ) -> Self;

    /// In-memory backend for tests. Default empty engine registry.
    pub fn memory() -> Result<Self, RuntimeError>;

    /// Accessor for pack handlers. Returns the filtered registry this runtime holds.
    pub fn embedders(&self) -> &Arc<EmbedderRegistry>;
}
```

`RuntimeConfig` loses `embedding_model`. `KhiveRuntime` loses the `embedder: Arc<OnceCell<...>>`
field. `embed()` and `embedder()` methods are removed. The runtime is single-purpose:
store and query. Embedding generation is the caller's responsibility.

The registry on `KhiveRuntime` exists for metadata access: resolving `model_id` to `EngineConfig`,
looking up `dim`, etc. The runtime does not invoke `embed_query_all` or `embed_document_all`.
Only pack handlers call those methods.

**Retrieval method signatures**: every method touching a vector table gains `model_id`:

```rust
impl KhiveRuntime {
    pub async fn vector_search(
        &self,
        namespace:  Option<&str>,
        model_id:   &str,          // routes to vec_{snake_case(model_id)} table
        query_vec:  Vec<f32>,      // caller pre-computed via registry
        top_k:      u32,
        kind:       Option<SubstrateKind>,
    ) -> RuntimeResult<Vec<VectorSearchHit>>;

    pub async fn hybrid_search(
        &self,
        namespace:  Option<&str>,
        model_id:   &str,
        query_text: &str,
        query_vec:  Vec<f32>,
        strategy:   Option<FusionStrategy>,
        limit:      u32,
    ) -> RuntimeResult<Vec<SearchHit>>;

    pub async fn upsert_vector(
        &self,
        namespace: Option<&str>,
        model_id:  &str,
        entity_id: Uuid,
        vector:    Vec<f32>,
    ) -> RuntimeResult<()>;
}
```

`khive-runtime` exposes registry types without requiring pack handlers to depend on a concrete
embedding provider. Native provider dependencies remain feature-gated.

**`RetrievalContext`** extends the runtime with per-engine stores:

```rust
pub struct RetrievalContext {
    engines:       Vec<EngineConfig>,
    dense_stores:  HashMap<String, Arc<dyn VectorStore>>,   // per dense engine
    fts:           Arc<dyn TextSearch>,                      // SQLite FTS5
}
```

Multi-engine write atomicity: all N vector inserts (one per engine) are batched in a single
transaction. If any fail, all roll back. This preserves the atomicity guarantee established
in ADR-009.

### D5: Pack handler fan-out: parallel embedding and weighted RRF

Multi-engine orchestration lives in pack handlers, not in `khive-runtime` and not in
`khive-retrieval`. The orchestration shape is verb-specific: each pack may differ in how it scores the
fused candidate set.

**Read-side pattern (five steps)**:

```text
async fn handle_search(args):
    registry = self.runtime.embedders()

    # Step 1: parallel embed across all configured engines (query_prefix applied per D1)
    embeddings = registry.embed_query_all(args.query)       # Vec<(EngineConfig, Vec<f32>)>

    # Step 2: per-engine search
    per_engine_hits = []
    for (cfg, query_vec) in embeddings:
        hits = self.runtime.vector_search(
            args.namespace, cfg.model, query_vec,
            candidate_pool_size, Some(SubstrateKind::Entity))
        per_engine_hits.push(hits)

    # Step 3: weighted RRF across engines
    weights      = registry.engines().iter().map(|e| e.config.fusion_weight.unwrap_or(1.0)).collect()
    vector_fused = khive_fusion::fuse(
        per_engine_hits, FusionStrategy::Weighted { weights }, candidate_pool_size)

    # Step 4: layer FTS5 keyword path and final fusion
    text_hits = self.runtime.text(args.namespace).search(...)
    fused     = fuse(vector_fused, text_hits, args.fusion_strategy, args.limit)

    # Step 5: pack-specific scoring
    return apply_pack_scoring(fused, args)
```

**Write-side mirror**:

```text
async fn handle_create(args):
    registry = self.runtime.embedders()
    id = self.runtime.create_entity(args.name, ...)

    embeddings = registry.embed_document_all(args.name)  # document_prefix applied per D1
    for (cfg, vector) in embeddings:
        self.runtime.upsert_vector(args.namespace, cfg.model, id, vector)
```

**Weighted RRF rationale**: per-engine weight encodes relative quality for the deployment's
corpus. BGE's English semantic strength, mE5's multilingual coverage, Qwen3's instruction-
tuned recall: operators set weights from empirical retrieval quality. `FusionStrategy::Weighted`
already exists in `khive-fusion`; pack handlers wire `EngineConfig.fusion_weight` into it.
An omitted weight contributes the neutral value `1.0`.

**This is engine-level weighted RRF: distinct from the backend-level unweighted RRF in
ADR-029 §D4.** ADR-029's D4 fuses ranked lists across backends at the substrate-search layer,
using RRF because backends are isolation boundaries, not relevance signals. This ADR's D5
fuses across peer embedding engines within a backend, using weights because engines have
known differential quality. Different concerns at different layers.

**Pack-specific scoring is layered on top of the fused candidate set**:

- kg pack (ADR-012): entity-density scoring
- future packs: custom scoring over the same fused candidate set

This is why orchestration lives in packs rather than in `khive-retrieval`. The retrieval
crate (ADR-030) provides building blocks: `HnswIndex`, `Bm25Index`, `FusionStrategy`, and
storage adapters. Multi-engine is a registry of `VectorStore` implementations the handler
selects among. `khive-retrieval` does not own the orchestration shape.

**Boundary correction**: ADR-030 provides retrieval engines and low-level fusion primitives
(engine-level RRF). ADR-042 provides reranker traits and rerank-stage integration. ADR-031
(this ADR) sits between them: owning candidate-set policy across embedding engines. ADR-030
does NOT define `Reranker` traits; those belong in ADR-042.

`FusionStrategy::Weighted` for engine fusion lives in `khive-fusion`. Pack-specific scoring
lives in each pack's scoring module.

## Rationale

### Why embedding is the caller's responsibility (D4)

The runtime's role is storage and retrieval. Embedding generation is an embedding concern.
Conflating them in `KhiveRuntime` blocked multi-engine from the start: a single
`Arc<OnceCell<Arc<dyn EmbeddingService>>>` cannot serve N engines. Separating the
responsibilities removes the architectural coupling and allows the runtime to be used
(in tests, in SQL-only consumers) without any embedding infrastructure.

### Why the registry lives on `KhiveRuntime` rather than exclusively in pack constructors

Pack handlers reach the registry through `runtime.embedders()`. Passing the registry
separately to every pack constructor, independent of the runtime, produces a
longer constructor signature and removes the natural grouping between "where your data lives"
(backend) and "which engines that backend's pack uses" (filtered registry). The runtime holds
the registry for metadata access only; it never calls `embed*()`.

### Why file configuration replaces environment-derived engines (D3)

The project file encodes the complete engine contract. Merging it entry by entry with
environment-derived engines would allow silent divergence between model sets and vector
tables. Replacement semantics require the operator to declare the full list in one source.

### Why pack handlers own fan-out, not `khive-retrieval` (D5)

`khive-retrieval` (ADR-030) is a building-blocks crate. It provides `HnswIndex`, `Bm25Index`,
`FusionStrategy` variants, and storage adapters. It has no opinion about pack-specific scoring
(for example, entity-density) and does NOT own reranker traits (those belong in
ADR-042). If fan-out lived in `khive-retrieval`, all packs would share one orchestration shape
and lose the ability to apply their own scoring between the multi-engine candidate set and the
final result. The building-block model is the right abstraction; the orchestration shape belongs
in the pack.

### Why engine-level RRF is weighted and backend-level RRF is unweighted (D5 vs. ADR-029 D4)

Engine weights encode measurable quality differentials (BGE on English, mE5 on Chinese). The
operator has empirical evidence for these weights; they have a calibration target.

Backend weights would encode deployment topology: is `main` more authoritative than
`corpus`? That is a configuration aesthetic with no calibration target. ADR-029 rejected the
knob entirely.

## Alternatives Considered

### A. Keep single-model, defer multi-engine

Single-model configuration cannot express the multilingual and mixed-corpus requirement
stated in this ADR. It is rejected as the only supported architecture.

### B. Multi-engine as a sidecar process

Spawn a dedicated embedding server; packs talk to it over IPC. Pros: process isolation; can
scale embeddings independently. Cons: per-call IPC cost on every retrieval query; conflicts
with the in-process MCP daemon model. Embedding lives in-process.

Rejected.

### C. Registry inside `khive-runtime` with `runtime.embed()` convenience wrapper

Keep `runtime.embed(text) -> Vec<f32>` wrapping `registry.embed_one(primary, text)`. Rejected:
a convenience wrapper invites callers to forget which engine they used, breaking multi-engine
semantics. The abstraction inversion returns, so engine identity remains explicit.

### D. Single multi-model service via `EmbeddingService::embed(texts, model)` trait

One service instance dispatches to multiple loaded models per call. Rejected: native
embedding services are pinned to one model; multi-model-per-service would require
provider-specific restructuring. Provider diversity cannot live
behind one trait object due to orthogonal configuration. Per-engine `Embedder` instances are
the simpler unit.

### E. `khive-retrieval` owns multi-engine fan-out via `MultiEngineSearcher`

`khive-retrieval` exposes a `MultiEngineSearcher` that pack handlers call once. Rejected:
the retrieval crate is a building-blocks crate by design (ADR-030). Forcing all consumers
through one orchestration shape removes pack autonomy and embedding of pack-specific scoring
inside the retrieval crate. A helper extraction is deferred until three or more packs share
identical fan-out code: the duplication threshold for extraction.

### F. Sequential per-engine fan-out

Embed and search one engine at a time. Rejected: defeats the parallelism that makes
multi-engine cost-acceptable. `tokio::join_all` / `try_join_all` is the right pattern;
wall-time cost is O(1), not O(N engines).

### G. One table, model_id-keyed rows

Single `vec0` table; embed rows tagged with `model_id`. Rejected: HNSW indexes are
dimension-fixed: a 384d node and a 1024d node cannot share a graph. Per-(model, dim)
table sharding is for correctness.

### H. Per-engine partial merge in project config override

Project `[[engines]]` entries with matching `name` merge field-by-field; others ignored.
Rejected: silent merge surprises break the project-as-invariant principle.

## Consequences

### Positive

- Multi-engine retrieval supports peer engines and weighted RRF
- Provider-agnostic: `Embedder` trait admits OpenAI, Cohere, custom implementations
  without modifying `khive-runtime` or `khive-db`
- Model-memory efficiency: engine instances loaded once, shared across packs via Arc + filter
- Asymmetric retrieval correct: E5 / Qwen3 prefixes handled at the registry boundary
- Pack autonomy: each pack applies its own scoring over the multi-engine candidate set
- Engine failure isolation: outage of one engine doesn't kill search; remaining engines
  continue serving
- Runtime decoupled from concrete providers directly; binary consumers that do not need
  a native provider can disable its feature
- Backward compatibility: single-engine fallback + `vec_default` rename preserves existing
  deployments
- Engine weights have a validated, explicit configuration source

### Negative

- N× embedding cost per query and write: mitigated by parallel embedding and per-engine
  LRU cache; default ships one engine, so the cost scales with explicit opt-in
- N× storage per write: N vector tables; tolerable for research KGs; a future `write_engines`
  allowlist (per D3 OQ-2) can mitigate if storage cost becomes a constraint
- Every retrieval call-site changes signature: `model_id` + `query_vec` instead of inline
  embed; migration touches each consuming verb handler, but the change is mechanical
- Configuration burden: operators learn the `[[engines]]` array and fusion weights;
  mitigated by the single-engine fallback
- Pack handler complexity grows for each retrieval verb for the fan-out loop

### Neutral

- `khive-retrieval` (ADR-030) is unchanged: adapters consume per-engine tables instead of
  a singleton; `HnswIndex` / `Bm25Index` / `FusionStrategy` API is unaffected
- `khive-fusion` requires no new fusion strategy: `FusionStrategy::Weighted` already exists
- MCP wire protocol unchanged: multi-engine is internal to handlers; clients see the same
  verbs
- `khive-fold` / objectives (ADR-024) unchanged: Objective composition operates on the
  candidate set after fusion

## Migration

`vector_search`, `hybrid_search`, and `upsert_vector` route by explicit `model_id`. On boot,
the idempotent compatibility migration renames `vec_default` to the key derived for the
selected default model when no target table exists. A configuration without `[[engines]]`
continues through the single-engine compatibility path. Declaring the array enables handler
fan-out across the configured engines.

## Open Questions

1. **`MultiEngineSearcher` helper extraction.** Defer until three or more packs need
   identical fan-out code. Pack handlers copy the pattern; extract when the duplication
   threshold is reached.
2. **Multi-engine write policy.** Default: every engine embeds every write. A future
   `write_engines` allowlist on `EngineConfig` (or `PackConfig`) would allow a pack to store
   writes in a subset of engines, reading from all. Deferred to v2.
3. **Remote-API engine config.** A `[[engines]] provider = "openai"` shape needs `api_key_env`
   / `endpoint` / `timeout` fields on `EngineConfig`. The `Embedder` trait supports it; the
   TOML schema initially covers the native provider. A future ADR may add hosted-provider
   credentials and endpoint fields when a concrete implementation ships.
4. **`vec_default` cleanup command.** After the one-time rename migration, `vec_default` is
   gone. If both tables existed at migration time, the `vec_default` is left in place with a
   warning. A `kkernel db prune-legacy-tables` admin command can remove it. Not v1 scope.

## References

- [ADR-005](./ADR-005-storage-capability-traits.md): `VectorStore` trait;
  `vec_model_key` pattern this ADR extends
- [ADR-011](./ADR-011-embedding-and-inference.md): single-embedder direction superseded by
  this ADR
- [ADR-012](./ADR-012-retrieval-composition.md): retrieval composition layer that sits above
  the multi-engine candidate set this ADR produces
- [ADR-024](./ADR-024-fold-cognitive-primitives.md): Objective implementations that consume
  fused candidates
- [ADR-028](./ADR-028-pack-scoped-backends.md): `[[engines]]` appears in the same `khive.toml`
  as `[[backends]]`; `engines = [...]` in `[packs.X]` drives `filter()`
- [ADR-029](./ADR-029-substrate-coordinator.md): backend-level unweighted RRF (D4) is
  distinct from and does not conflict with this ADR's engine-level weighted RRF (D5)
- [ADR-030](./ADR-030-retrieval-stack-port.md): provides `HnswIndex`, `Bm25Index`,
  `FusionStrategy` that pack handlers compose over
- [ADR-035](./ADR-035-cli-config-and-auto-embed.md): project-vs-user TOML override semantics
  that D3 extends

## Amendment: Pack-extensible `EmbedderRegistry`

### Motivation

ADR-031 D2 requires registration of multiple embedding providers at boot. A closed map of
native model variants cannot represent providers contributed by independently compiled
packs, so the registry accepts provider trait objects.

### Decision

A new `EmbedderProvider` async trait and `EmbedderRegistry` struct replace the private
`HashMap<String, EmbedderEntry>` inside `KhiveRuntime`.

**EmbedderProvider contract**:

```rust
#[async_trait]
pub trait EmbedderProvider: Send + Sync {
    fn name(&self) -> &str;            // stable, unique name
    fn dimensions(&self) -> usize;     // output vector dimension
    async fn build(&self) -> Result<Arc<dyn EmbeddingService>, RuntimeError>;
}
```

**EmbedderRegistry** stores `Box<dyn EmbedderProvider>` + a `OnceCell` per entry (lazy init,
cached). Direct registration is last-writer-wins for API compatibility. Extensions must check
the existing names and must not depend on registration order when selecting an override.

**KhiveRuntime integration**:

- `embedder_registry: Arc<RwLock<EmbedderRegistry>>` replaces `embedders: Arc<HashMap<...>>`.
- `KhiveRuntime::register_embedder(provider)`: public, callable post-construction.
- Existing `embedder(name)`, `resolve_embedding_model(name)`, and
  `registered_embedding_model_names()` continue to work. Built-in model aliases are
  normalized before registry lookup; custom provider names are looked up directly.
- `RwLockGuard` is never held across `await`: entries are cloned before `OnceCell::get_or_init`.

**Pack extension hook**:

```rust
// PackRuntime trait (khive-runtime/src/pack.rs)
fn register_embedders(&self, _runtime: &KhiveRuntime) {}   // default no-op
```

Packs that provide custom embedding backends implement this method; the transport should call it
during pack initialisation before the first verb dispatch.

**Backward compatibility**: `RuntimeConfig::embedding_model` and
`additional_embedding_models` remain. Built-in native models are pre-registered as provider
instances during `KhiveRuntime::new` and `from_backend`; existing callers need no changes.

Configuration requires unique provider names. Direct registry calls retain the documented
last-writer-wins behavior. Lazy initialization must not hold an `RwLockGuard` across `await`,
and registration completes before the first verb dispatch.
