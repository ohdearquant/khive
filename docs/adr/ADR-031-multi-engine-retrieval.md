# ADR-031: Multi-Engine Retrieval — Embedder Trait, Registry, Configuration, and Pack Orchestration

**Status**: accepted\
**Date**: 2026-05-23\
**Authors**: Ocean, lambda:khive\
**Consolidates**: ADR-078 (umbrella), ADR-081 (Embedder trait + EmbedderRegistry), ADR-082
(engine TOML schema), ADR-083 (runtime API — caller-computed embeddings), ADR-084 (pack
fan-out + weighted RRF), ADR-091 (runtime-layer composition + SparseStore + memory.recall_* verbs)\
**Supersedes**: ADR-011 §"single-embedder direction"\
**Depends on**: ADR-005 (Storage Capability Traits), ADR-030 (Retrieval Stack Port)\
**Related**:

- ADR-012 (Retrieval Composition) — composes over this layer
- ADR-024 (Fold Cognitive Primitives) — Objective implementations operate on candidates this layer produces
- ADR-028 (Pack-Scoped Backends) — pack `engines = [...]` in `khive.toml` drives `filter()`
- ADR-029 (SubstrateCoordinator) — backend-level unweighted RRF is distinct from the engine-level weighted RRF here
- ADR-033 (Recall Pipeline) — memory recall consumes the pack fan-out pattern specified here
- ADR-035 (CLI Config) — project-vs-user TOML override semantics extended by this ADR

## Context

khive-internal implemented multi-engine embedding: N peer models run concurrently, each with
its own HNSW index, every write embedding with all N models, every query fusing per-engine
rankings via weighted RRF. `deploy/engine.toml` was the canonical config; per-engine
normalization parameters (`noise_floor`, `max_similarity`, `threshold`) were tuned during the
2026-03-26 Chinese-blindspot crisis (mE5 migration). That crisis confirmed empirically that
no single embedding model dominates across languages and corpus types — multilingual and
paraphrase-heavy corpora require peer engines.

The open-core port regressed this to single-model:

```rust
// crates/khive-runtime/src/runtime.rs (regression site)
pub struct RuntimeConfig {
    pub embedding_model: Option<EmbeddingModel>,         // one model
}
pub struct KhiveRuntime {
    embedder: Arc<OnceCell<Arc<dyn EmbeddingService>>>,  // one service
}
// vector_search() / hybrid_search() — no model parameter, assumes singleton
```

This is a regression against a design property that had been tuned and retained through every
khive-internal refactor. The next step was recorded explicitly after the 2026-03-26 event:
"Multi-index architecture: engine.toml + code supports `Vec<EmbedModelConfig>`. Add Qwen3 as
peer after HNSW namespace split." The open-core port erased this without an ADR.

### What "multi-engine" means

Distinct from multi-tenant (namespace isolation per ADR-007) and distinct from model migration
(single model, swap atomically). Multi-engine means:

- N peer embedding services run concurrently in the same process
- Each service may be a different provider (lattice-embed native, OpenAI API, custom)
- Each service has its own vector index — one `vec_*` table per (model_id, dim)
- Every write embeds with all N services and stores in all N indices
- Every query embeds with all N services, searches all N indices in parallel, then fuses
- Results merge via weighted RRF using per-engine weight
- Per-engine score normalization (noise_floor, max_similarity, threshold) calibrates
  cross-engine comparability

### Why "engine" not "model"

A model is an `EmbeddingModel` variant — a specific weights file with a specific
dimensionality. An engine is a complete embedding service: trait implementation, model handle,
cache, concurrency policy, provider semantics (local inference vs. HTTP API). Two engines can
implement the same model (local BGE vs. hosted BGE); one engine can serve only one model (each
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
│  Pack handlers (pack-memory, pack-kg, etc.)          (D5)               │
│  - embed_query_all → per-engine search → normalization → weighted RRF   │
│  - embed_document_all → upsert_vector per engine                        │
│  - Pack-specific scoring layered on top (memory decay, kg density)      │
└─────────────────────────────────────────────────────────────────────────┘
                                    ↓
┌─────────────────────────────────────────────────────────────────────────┐
│  KhiveRuntime                                        (D4)               │
│  - Holds Arc<EmbedderRegistry> (filtered) for metadata access only      │
│  - No embedder field; no lattice-embed direct dep                       │
│  - vector_search(ns, model_id, query_vec, top_k, kind) — model_id       │
│    routes to vec_{snake(model_id)} table; no embedding generation here  │
│  - upsert_vector(ns, model_id, entity_id, vector)                       │
│  - RetrievalContext holds dense + sparse stores per engine (D6)         │
└─────────────────────────────────────────────────────────────────────────┘
                                    ↓
┌─────────────────────────────────────────────────────────────────────────┐
│  khive-embed                                         (D1, D2)           │
│  - Embedder trait, EmbedderRegistry, EngineConfig                       │
│  - LatticeEmbedder adapter behind feature "lattice" (default)           │
│  - filter() returns Arc<EmbedderRegistry-subset>; engine Arcs shared    │
│  - vec_model_key() — canonical (model_id, dim) → table name mapping     │
└─────────────────────────────────────────────────────────────────────────┘
                                    ↓
┌─────────────────────────────────────────────────────────────────────────┐
│  khive-storage / khive-db                            (ADR-005, ADR-030)  │
│  - VectorStore + SparseStore traits                                     │
│  - vectors_for_namespace(model_key, dim, ns) — per-(model, dim) tables  │
│  - HnswIndex, Bm25Index, FusionStrategy from ADR-030                   │
└─────────────────────────────────────────────────────────────────────────┘
```

## Decision

This ADR consolidates six decisions, D1 through D6. Each is independently implementable in
the order shown (Phase A through C); each leaves the build green at its completion.

### D1 — `Embedder` trait: provider-agnostic, one-model-per-instance

```rust
// crates/khive-embed/src/trait.rs
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
   engines = N `Embedder` instances. This matches khive-internal's
   `NativeEmbeddingService::with_model(model)` pattern.
2. **Provider-agnostic.** `lattice-embed` is one implementation; `OpenAiEmbedder`,
   `CohereEmbedder`, or custom providers implement the same trait without modifying
   `khive-runtime` or `khive-db`.
3. **Asymmetric retrieval built in.** E5 / Qwen3 prefixes are first-class. Omitting them
   causes cosine scores to cluster at 0.93–0.95 (the documented cause of the Chinese-blindspot
   crisis). The registry applies prefixes; callers do not.

The first concrete implementation wraps `lattice-embed`'s `CachedEmbeddingService`:

```rust
// crates/khive-embed/src/lattice.rs   (feature "lattice", default)
pub struct LatticeEmbedder {
    model:   lattice_embed::EmbeddingModel,
    service: Arc<lattice_embed::CachedEmbeddingService>,
    dim:     usize,
}
#[async_trait]
impl Embedder for LatticeEmbedder { /* delegates to CachedEmbeddingService */ }
```

The `lattice` feature is `default = ["lattice"]`. Consumers who want lattice-free builds
(remote-API-only deployments) disable it.

The `khive-embed` crate lives in the platform layer. If a non-lattice provider ships, the
adapter is extracted to a sibling crate; until then one crate with a feature flag is simpler.

### D2 — `EmbedderRegistry`: process-wide, filtered per pack via `Arc::filter()`

```rust
// crates/khive-embed/src/registry.rs
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

    /// Parallel embed — query side; applies query_prefix per engine.
    pub async fn embed_query_all(&self, text: &str)
        -> Result<Vec<(EngineConfig, Vec<f32>)>, EmbedError>;

    /// Parallel embed — document side; applies document_prefix per engine.
    pub async fn embed_document_all(&self, text: &str)
        -> Result<Vec<(EngineConfig, Vec<f32>)>, EmbedError>;

    /// Embed with a specific engine by model_id (no prefix applied).
    pub async fn embed_one(&self, model_id: &str, text: &str)
        -> Result<Vec<f32>, EmbedError>;

    /// Look up engine config by model_id (for table routing metadata).
    pub fn get(&self, model_id: &str) -> Option<&EngineConfig>;

    /// Return a new registry exposing ONLY engines whose model_id is in `allow`.
    /// Engine Arcs are SHARED with self — filter is a view, not a clone.
    pub fn filter(self: &Arc<Self>, allow: &[String]) -> Arc<EmbedderRegistry>;
}
```

**D2 core property**: kkernel constructs `EmbedderRegistry` once from the `[[engines]]` array.
Each pack declares the engines it uses (per ADR-028: `engines = ["bge-small-en-v1.5"]`). At
pack construction, kkernel calls `registry.filter(&pack_cfg.engines)` and passes the filtered
`Arc<EmbedderRegistry>` to the pack's `KhiveRuntime::from_backend`. Engine Arcs are shared —
the filter is a view into the parent registry, not a copy of the models.

This gives:

- **Memory locality** — BGE loaded once across all consuming packs.
- **Cache locality** — a query against kg warms the BGE LRU cache; memory pack benefits.
- **Pack autonomy** — a pack declaring `engines = []` cannot invoke an unconfigured engine.

Engine failure semantics: `embed_query_all` returns a partial list when one engine errors.
If at least one engine succeeds, the search proceeds with the available engines. If all
engines fail, the request fails. This is the khive-internal behavior (availability over
strict consistency). A future `embed_query_all_strict()` variant may be added for operators
who need all-or-nothing behavior.

### D3 — `[[engines]]` TOML schema, vector table naming, single-engine fallback

Engines are declared as a TOML array in `khive.toml` (user or project level; project replaces
user — no merge):

```toml
[[engines]]
name = "bge-small-en-v1.5"      # Embedder::model_id(); snake_case for table keys
dim = 384
weight = 1.0                     # RRF fusion weight (D5)
noise_floor = 0.30               # cosines below this are discarded as noise
max_similarity = 0.75            # cap for per-engine normalization
threshold = 0.25                 # minimum cosine to enter fusion stage
# device = "metal"               # user-level only; not committed in project config

[[engines]]
name = "multilingual-e5-small"
dim = 384
weight = 0.8
noise_floor = 0.15
max_similarity = 0.65
threshold = 0.30

[[engines]]
name = "qwen3-embedding-0.6b"
dim = 1024
weight = 1.2
noise_floor = 0.10
max_similarity = 0.70
threshold = 0.20
output_dim = 512                 # MRL truncation; only for models that support it
```

Rust type (lives in `khive-embed`, co-located with the registry):

```rust
#[derive(Debug, Clone, Deserialize)]
pub struct EngineConfig {
    pub name:           String,
    pub dim:            usize,
    pub weight:         f32,
    pub noise_floor:    f64,
    pub max_similarity: f64,
    pub threshold:      f64,
    pub output_dim:     Option<usize>,
}
```

**Override semantics**: user-level `~/.khive/khive.toml` sets the default engine list.
Project-level `.khive/khive.toml` replaces it entirely if it declares `[[engines]]`. There is
no per-entry merge. Replacing, not merging, enforces project-consistency — collaborators
sharing a project must run the same engines, so vectors are produced by the same models.

Machine-local fields (`device`) are user-level only. The project commits to `name` / `dim` /
`weight` / calibration; the execution environment is operator-local.

**Single-engine fallback**: if neither user nor project declares `[[engines]]`, kkernel falls
back to one built-in engine (`bge-small-en-v1.5`, 384-dim, weight 1.0, calibrated defaults).
This preserves backward compatibility for deployments predating this ADR.

**Vector table naming** — one table per (model_id, output_dim) pair:

- Base: `vec_{snake_case(model_id)}`
  - `bge-small-en-v1.5` → `vec_bge_small_en_v1_5`
  - `multilingual-e5-small` → `vec_multilingual_e5_small`
- MRL variant: `vec_{snake_case(model_id)}_dim_{N}`
  - `qwen3-embedding-4b` truncated to 1024d → `vec_qwen3_embedding_4b_dim_1024`

Sanitization rule (`vec_model_key`): replace every non-alphanumeric character with `_`. This
helper moves from `khive-runtime` to `khive-embed` as the canonical engine-identity-to-table-
key bridge; behavior is unchanged.

HNSW indexes are dimension-fixed (a 384d node and a 1024d node cannot share a graph).
Per-(model, dim) table sharding is for correctness, not optimization — it is the INV-1
invariant from khive-internal's `foundation/embed/DESIGN.md`.

**Migration shim for `vec_default`**: deployments predating this ADR have data in a
`vec_default` table. At first startup post-D3:

1. If `[[engines]]` is missing, fall back to built-in default with `model_id = "bge-small-en-v1.5"`.
2. If `vec_default` exists but `vec_bge_small_en_v1_5` does not, rename the table once.
3. Both tables present: prefer `vec_bge_small_en_v1_5`; log warning about `vec_default`.

This migration is idempotent and runs once at boot.

### D4 — Runtime API: caller-computed vectors, `model_id` routing, no embedder ownership

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

The registry on `KhiveRuntime` exists for metadata access — resolving `model_id` to `EngineConfig`,
looking up `dim`, etc. The runtime does not invoke `embed_query_all` or `embed_document_all`.
Only pack handlers call those methods.

**Retrieval method signatures** — every method touching a vector table gains `model_id`:

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

`khive-runtime/Cargo.toml` drops the `lattice-embed` direct dependency. The dependency moves
to `khive-embed`. `khive-runtime` depends on `khive-embed` for the registry type — but
`lattice-embed` is now transitive through `khive-embed`'s feature flag, not direct. Consumers
that want lattice-free builds can disable the feature.

**`RetrievalContext`** extends the runtime with per-engine stores (added alongside D6 sparse
support):

```rust
pub struct RetrievalContext {
    engines:       Vec<EngineConfig>,
    dense_stores:  HashMap<String, Arc<dyn VectorStore>>,   // per dense engine
    sparse_stores: HashMap<String, Arc<dyn SparseStore>>,   // per sparse engine
    fts:           Arc<dyn TextSearch>,                      // SQLite FTS5
}
```

Multi-engine write atomicity: all N vector inserts (one per engine) are batched in a single
transaction. If any fail, all roll back. This preserves the atomicity guarantee established
in ADR-009.

### D5 — Pack handler fan-out: parallel embed, per-engine normalization, weighted RRF

Multi-engine orchestration lives in pack handlers, not in `khive-runtime` and not in
`khive-retrieval`. The orchestration shape is verb-specific — memory's decay-weighted recall,
kg's entity-scored search, and future packs' custom logic all differ in how they score the
fused candidate set.

**Read-side pattern (four steps)**:

```text
async fn handle_recall(args):
    registry = self.runtime.embedders()

    # Step 1 — parallel embed across all configured engines (query_prefix applied per D1)
    embeddings = registry.embed_query_all(args.query)       # Vec<(EngineConfig, Vec<f32>)>

    # Step 2 — per-engine search
    per_engine_hits = []
    for (cfg, query_vec) in embeddings:
        hits = self.runtime.vector_search(
            args.namespace, cfg.name, query_vec,
            candidate_pool_size, Some(SubstrateKind::Note))
        normalized = normalize_hits(hits, cfg.noise_floor, cfg.max_similarity)
        filtered   = filter_threshold(normalized, cfg.threshold)
        per_engine_hits.push(filtered)

    # Step 3 — weighted RRF across engines
    weights      = registry.engines().iter().map(|e| e.config.weight as f64).collect()
    vector_fused = khive_fusion::fuse(
        per_engine_hits, FusionStrategy::Weighted { weights }, candidate_pool_size)

    # Step 4 — layer FTS5 keyword path and final fusion
    text_hits = self.runtime.text(args.namespace).search(...)
    fused     = fuse(vector_fused, text_hits, args.fusion_strategy, args.limit)

    # Step 5 — pack-specific scoring
    return apply_pack_scoring(fused, args)
```

**Write-side mirror**:

```text
async fn handle_remember(args):
    registry = self.runtime.embedders()
    note_id  = self.runtime.create_note(args.content, ...)

    embeddings = registry.embed_document_all(args.content)  # document_prefix applied per D1
    for (cfg, vector) in embeddings:
        self.runtime.upsert_vector(args.namespace, cfg.name, note_id, vector)
```

**Per-engine normalization** (`noise_floor`, `max_similarity`, `threshold` from `EngineConfig`):

- `noise_floor`: cosine scores below this are treated as noise; discarded before fusion.
- `max_similarity`: cap for normalization — brings disparate engines onto a comparable scale.
- `threshold`: per-engine minimum score to enter the fusion stage.

These parameters were tuned empirically in khive-internal. v1 inherits those defaults;
per-corpus retuning is operator responsibility.

**Weighted RRF rationale**: per-engine weight encodes relative quality for the deployment's
corpus. BGE's English semantic strength, mE5's multilingual coverage, Qwen3's instruction-
tuned recall — operators set weights from empirical retrieval quality. `FusionStrategy::Weighted`
already exists in `khive-fusion`; pack handlers wire `EngineConfig.weight` into it.

**This is engine-level weighted RRF — distinct from the backend-level unweighted RRF in
ADR-029 §D4.** ADR-029's D4 fuses ranked lists across backends at the substrate-search layer,
using RRF because backends are isolation boundaries, not relevance signals. This ADR's D5
fuses across peer embedding engines within a backend, using weights because engines have
known differential quality. Different concerns at different layers.

**Pack-specific scoring is layered on top of the fused candidate set**:

- memory pack (ADR-033): `salience × exp(-decay_factor × age_days)` — decay-weighted recall
- kg pack (ADR-012): entity-density scoring
- future packs: custom scoring over the same fused candidate set

This is why orchestration lives in packs rather than in `khive-retrieval`. The retrieval
crate (ADR-030) provides building blocks: `HnswIndex`, `Bm25Index`, `FusionStrategy`, and
storage adapters. Multi-engine is a registry of `VectorStore` implementations the handler
selects among. `khive-retrieval` does not own the orchestration shape.

**Boundary correction**: ADR-030 provides retrieval engines and low-level fusion primitives
(engine-level RRF). ADR-042 provides reranker traits and rerank-stage integration. ADR-031
(this ADR) sits between them — owning candidate-set policy across embedding engines. ADR-030
does NOT define `Reranker` traits; those belong in ADR-042.

Normalization helpers (`normalize_hits`, `filter_threshold`) are co-located with `EngineConfig`
in `khive-embed`, shared across packs. `FusionStrategy::Weighted` for engine fusion lives in
`khive-fusion`. Pack-specific scoring lives in each pack's scoring module.

### D6 — `SparseStore` trait and `memory.recall_*` dotted verbs

**`SparseStore` trait** extends `khive-storage` (parallel to `VectorStore` from ADR-005):

```rust
// crates/khive-storage/src/traits.rs
#[async_trait]
pub trait SparseStore: Send + Sync {
    async fn insert_sparse(
        &self,
        id:        Uuid,
        kind:      SubstrateKind,
        namespace: &str,
        vector:    SparseVector,
    ) -> StorageResult<()>;

    async fn search_sparse(
        &self,
        query:  &SparseVector,
        top_k:  u32,
        filter: Option<NamespaceFilter>,
    ) -> StorageResult<Vec<SparseSearchHit>>;
}

pub struct SparseVector {
    pub indices: Vec<u32>,
    pub values:  Vec<f32>,
}
```

Implemented in `khive-db-ruvector` via `ruvector_core::sparse_vector`. FTS5 is retained
alongside sparse: FTS5 handles exact-keyword and trigram (CJK substring) queries; sparse
handles semantic-with-lexical-bias retrieval. Neither replaces the other.

**`memory.recall_*` dotted verbs** extend the pack-memory surface (per ADR-023 §4: non-kg
pack verbs are pack-prefixed with single-dot snake_case sub-variants):

| Verb                                           | Behavior                                                   |
| ---------------------------------------------- | ---------------------------------------------------------- |
| `memory.recall(query)`                         | Default: hybrid dense + sparse + FTS5, brain-tuned weights |
| `memory.recall_diverse(query, lambda=0.5)`     | MMR diversity rerank over default recall results           |
| `memory.recall_engine(query, engine="bge-zh")` | Force a single engine; no fusion, no brain tuning          |
| `memory.recall_matryoshka(query, fast_dim=N)`  | Two-stage: fast retrieval at fast_dim, rerank at full dim  |
| `memory.recall_candidates(query)`              | Debug — raw per-source rankings before fusion              |
| `memory.recall_fuse(query)`                    | Debug — fusion output before final scoring                 |
| `memory.recall_score(query, id)`               | Debug — score breakdown for a specific candidate           |

Per-call overrides on `recall`: `engines=[...]`, `weights={engine: w}`,
`strategy="rrf"|"linear"|"dbsf"`. Per-call overrides are for experimentation; the brain
learns from the unoverridden default path.

`memory.recall_matryoshka` is a separate verb rather than a hidden internal optimization
because explicit verbs let the brain measure when matryoshka helps. If it were always on,
the brain could not isolate its contribution.

## Rationale

### Why embedding is the caller's responsibility (D4)

The runtime's role is storage and retrieval. Embedding generation is an embedding concern.
Conflating them in `KhiveRuntime` blocked multi-engine from the start — a single
`Arc<OnceCell<Arc<dyn EmbeddingService>>>` cannot serve N engines. Separating the
responsibilities removes the architectural coupling and allows the runtime to be used
(in tests, in SQL-only consumers) without any embedding infrastructure.

### Why the registry lives on `KhiveRuntime` rather than exclusively in pack constructors

Pack handlers reach the registry through `runtime.embedders()`. The alternative —
registry passed separately to every pack constructor, independent of the runtime — produces a
longer constructor signature and removes the natural grouping between "where your data lives"
(backend) and "which engines that backend's pack uses" (filtered registry). The runtime holds
the registry for metadata access only; it never calls `embed*()`.

### Why project config replaces user config (D3)

Project config encodes the engine contract for a collaboration. Collaborators sharing a project
must run the same engines; their vectors must be produced by the same models. A merge
semantics would allow silent divergence — one user adds an engine, another does not, and the
project's vector tables become inconsistent across users. Replace is strict; the operator
re-declares the full list when they add an engine.

### Why `device` is user-level only (D3)

Device identifiers (`"metal"`, `"cuda"`) describe the local execution environment, not the
project's semantic commitments. A project config containing `device = "metal"` would break
Linux collaborators with no recourse.

### Why pack handlers own fan-out, not `khive-retrieval` (D5)

`khive-retrieval` (ADR-030) is a building-blocks crate. It provides `HnswIndex`, `Bm25Index`,
`FusionStrategy` variants, and storage adapters. It has no opinion about pack-specific scoring
(memory decay, entity-density, etc.) and does NOT own reranker traits (those belong in
ADR-042). If fan-out lived in `khive-retrieval`, all packs would share one orchestration shape
and lose the ability to apply their own scoring between the multi-engine candidate set and the
final result. The building-block model is the right abstraction; the orchestration shape belongs
in the pack.

### Why engine-level RRF is weighted and backend-level RRF is unweighted (D5 vs. ADR-029 D4)

Engine weights encode measurable quality differentials (BGE on English, mE5 on Chinese). The
operator has empirical evidence for these weights; they have a calibration target.

Backend weights would encode deployment topology — `main` is "more authoritative" than
`lore`? That is a configuration aesthetic with no calibration target. ADR-029 rejected the
knob entirely.

### Why FTS5 is retained alongside sparse (D6)

Different jobs. FTS5 handles exact-keyword queries and trigram CJK substring search — use
cases where the user knows a literal string. Sparse retrieval handles semantic-with-lexical-
bias cases — where meaning anchors matter more than exact tokens. The recall verb fuses both
signals via RRF.

## Alternatives Considered

### A. Keep single-model, defer multi-engine

The current regression in the open-core port is the result of exactly this. Multi-engine
had shipped, had been tuned, and was required for multilingual quality. Deferring again would
require a third ADR to restore it later. Rejected.

### B. Multi-engine as a sidecar process

Spawn a dedicated embedding server; packs talk to it over IPC. Pros: process isolation; can
scale embeddings independently. Cons: per-call IPC cost on every retrieval query; conflicts
with the in-process MCP daemon model. Embedding lives in-process.

Rejected.

### C. Registry inside `khive-runtime` with `runtime.embed()` convenience wrapper

Keep `runtime.embed(text) -> Vec<f32>` wrapping `registry.embed_one(primary, text)`. Rejected:
a convenience wrapper invites callers to forget which engine they used, breaking multi-engine
semantics. The abstraction inversion returns. Explicit > implicit.

### D. Single multi-model service via `EmbeddingService::embed(texts, model)` trait

One service instance dispatches to multiple loaded models per call. Rejected: each
`NativeEmbeddingService` in lattice-embed is pinned to one model; multi-model-per-service
would require lattice-embed restructure. Provider diversity (OpenAI vs. BGE) cannot live
behind one trait object due to orthogonal configuration. Per-engine `Embedder` instances are
the simpler unit.

### E. `khive-retrieval` owns multi-engine fan-out via `MultiEngineSearcher`

`khive-retrieval` exposes a `MultiEngineSearcher` that pack handlers call once. Rejected:
the retrieval crate is a building-blocks crate by design (ADR-030). Forcing all consumers
through one orchestration shape removes pack autonomy and embedding of pack-specific scoring
inside the retrieval crate. A helper extraction is deferred until three or more packs share
identical fan-out code — the duplication threshold for extraction.

### F. Sequential per-engine fan-out

Embed and search one engine at a time. Rejected: defeats the parallelism that makes
multi-engine cost-acceptable. `tokio::join_all` / `try_join_all` is the right pattern;
wall-time cost is O(1), not O(N engines).

### G. One table, model_id-keyed rows

Single `vec0` table; embed rows tagged with `model_id`. Rejected: HNSW indexes are
dimension-fixed — a 384d node and a 1024d node cannot share a graph. Per-(model, dim)
table sharding is for correctness.

### H. Per-engine partial merge in project config override

Project `[[engines]]` entries with matching `name` merge field-by-field; others ignored.
Rejected: silent merge surprises break the project-as-invariant principle.

## Consequences

### Positive

- Multi-engine quality restored — peer engines, weighted RRF, per-engine normalization,
  matching khive-internal's tuned shape
- Provider-agnostic — `Embedder` trait admits OpenAI, Cohere, custom implementations
  without modifying `khive-runtime` or `khive-db`
- Memory efficiency — engine instances loaded once, shared across packs via Arc + filter
- Asymmetric retrieval correct — E5 / Qwen3 prefixes handled at the registry boundary
- Pack autonomy — each pack applies its own scoring over the multi-engine candidate set
- Engine failure isolation — outage of one engine doesn't kill search; remaining engines
  continue serving
- Runtime decoupled from `lattice-embed` directly — binary consumers that don't need
  embedding can disable the feature
- Backward compatibility — single-engine fallback + `vec_default` rename preserves existing
  deployments
- Calibration knobs preserved — `noise_floor` / `max_similarity` / `threshold` / `weight`
  match the tuned khive-internal schema verbatim
- Sparse retrieval path added — `SparseStore` trait extends the storage surface for
  semantic-with-lexical-bias recall alongside dense and FTS5
- Recall verb surface extended — `memory.recall_*` dotted verbs expose retrieval strategy
  without polluting the top-level verb namespace

### Negative

- N× embedding cost per query and write — mitigated by parallel embedding and per-engine
  LRU cache; default ships one engine, so the cost scales with explicit opt-in
- N× storage per write — N vector tables; tolerable for research KGs; a future `write_engines`
  allowlist (per D3 OQ-2) can mitigate if storage cost becomes a constraint
- Every retrieval call-site changes signature — `model_id` + `query_vec` instead of inline
  embed; migration touches each consuming verb handler, but the change is mechanical
- `khive-embed` adds a new crate — one more `Cargo.toml` and publish step
- Configuration burden — operators learn `[[engines]]` array and calibration parameters;
  mitigated by single-engine fallback and pre-tuned defaults inherited from khive-internal
- Pack handler complexity grows ~50 LOC per recall/search verb for the fan-out loop

### Neutral

- `khive-storage` gains `SparseStore` trait — additive, no existing trait changes
- `khive-retrieval` (ADR-030) is unchanged — adapters consume per-engine tables instead of
  a singleton; `HnswIndex` / `Bm25Index` / `FusionStrategy` API is unaffected
- `khive-fusion` requires no new fusion strategy — `FusionStrategy::Weighted` already exists
- MCP wire protocol unchanged — multi-engine is internal to handlers; clients see the same
  verbs
- `khive-fold` / objectives (ADR-024) unchanged — Objective composition operates on the
  candidate set after fusion

## Migration

Three phases; each leaves the build green independently:

**Phase A — `khive-embed` crate (D1, D2)**. New crate; no behavior change in any existing
crate. `khive-runtime` still owns its single embedder until Phase B. Smoke test passes.

**Phase B — Runtime API change (D3 migration shim, D4)**. `KhiveRuntime` drops the embedder
field. `vector_search` / `hybrid_search` / `upsert_vector` gain `model_id`. One-time boot
migration renames `vec_default` → `vec_bge_small_en_v1_5`. Single-engine behavior is
preserved — the single engine is now the built-in default, passed explicitly by the pack
handler rather than owned by the runtime.

**Phase C — Multi-engine config + pack fan-out (D3 full, D5, D6)**. `[[engines]]` TOML
schema activated; pack handlers fan out across all configured engines. `SparseStore` trait
added. `memory.recall_*` dotted verbs added. New deployments get multi-engine by declaring the array;
existing single-engine deployments see no behavior change.

## Open Questions

1. **`MultiEngineSearcher` helper extraction.** Defer until three or more packs need
   identical fan-out code. Pack handlers copy the pattern; extract when the duplication
   threshold is reached.
2. **Multi-engine write policy.** Default: every engine embeds every write. A future
   `write_engines` allowlist on `EngineConfig` (or `PackConfig`) would allow a pack to store
   writes in a subset of engines, reading from all. Deferred to v2.
3. **Remote-API engine config.** A `[[engines]] provider = "openai"` shape needs `api_key_env`
   / `endpoint` / `timeout` fields on `EngineConfig`. The `Embedder` trait supports it; the
   TOML schema is lattice-shaped for v1. Future ADR when a concrete remote-API provider ships.
4. **Primary engine convention.** `engines()[0]` is the implicit primary for single-model
   operations. A named `primary` field on the registry is a future option if the convention
   causes confusion.
5. **Calibration split.** `[[engines]]` mixes identity (`name`, `dim`) with calibration
   (`noise_floor`, `max_similarity`, `threshold`, `weight`). Calibration changes more often.
   Future ADR may introduce a separate `[[engine_calibrations]]` table.
6. **`vec_default` cleanup command.** After the one-time rename migration, `vec_default` is
   gone. If both tables existed at migration time, the `vec_default` is left in place with a
   warning. A `kkernel db prune-legacy-tables` admin command can remove it. Not v1 scope.

## References

- [ADR-005](ADR-005-storage-capability-traits.md) — `VectorStore` + `SparseStore` traits;
  `vec_model_key` pattern this ADR extends
- [ADR-011](ADR-011-embedding-and-inference.md) — single-embedder direction superseded by
  this ADR
- [ADR-012](ADR-012-retrieval-composition.md) — retrieval composition layer that sits above
  the multi-engine candidate set this ADR produces
- [ADR-024](ADR-024-fold-cognitive-primitives.md) — Objective implementations that consume
  fused candidates
- [ADR-028](ADR-028-pack-scoped-backends.md) — `[[engines]]` appears in the same `khive.toml`
  as `[[backends]]`; `engines = [...]` in `[packs.X]` drives `filter()`
- [ADR-029](ADR-029-substrate-coordinator.md) — backend-level unweighted RRF (D4) is
  distinct from and does not conflict with this ADR's engine-level weighted RRF (D5)
- [ADR-030](ADR-030-retrieval-stack-port.md) — provides `HnswIndex`, `Bm25Index`,
  `FusionStrategy` that pack handlers compose over
- [ADR-033](ADR-033-recall-pipeline.md) — memory recall verb consumes the pack fan-out
  pattern from D5
- [ADR-035](ADR-035-cli-config-and-auto-embed.md) — project-vs-user TOML override semantics
  that D3 extends
- khive-internal `deploy/engine.toml` — canonical multi-engine schema being restored
- khive-internal `foundation/embed/DESIGN.md` — INV-1..INV-8 invariants; per-(model, dim)
  table sharding rationale; asymmetric retrieval prefix invariant
- khive-internal `apps/cli/src/server/unified.rs:414-664` — `resolve_embed_models`,
  historical multi-engine wiring; D2 pattern source
- khive-internal summary `summary_20260326_165542_recall_overhaul_multi_index_architecture.md`
  — Chinese-blindspot crisis; per-engine calibration history
