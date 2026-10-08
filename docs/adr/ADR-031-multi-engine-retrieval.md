# ADR-031: Multi-Engine Retrieval — Embedder Trait, Registry, Configuration, and Pack Orchestration

**Status**: accepted\
**Date**: 2026-05-23\
**Authors**: khive maintainers\
**Amended by**: Amendment 5 below (ordered peer engines and explicit retrieval strategies, proposed); proposed [ADR-160](ADR-160-shared-pack-infrastructure.md), which atomically binds a
provider factory to a complete immutable embedding-space identity and replaces sanitized
model-derived physical selection on acceptance.\
**Consolidates** (retired v0-series drafts predating the 2026-05-23 ADR renumbering; these
numbers do not refer to current-index ADRs): ADR-078 (umbrella), ADR-081 (Embedder trait +
EmbedderRegistry), ADR-082 (engine TOML schema), ADR-083 (runtime API — caller-computed
embeddings), ADR-084 (pack fan-out + weighted RRF), ADR-091 (runtime-layer composition +
SparseStore + memory.recall_* verbs)\
**Supersedes**: ADR-011 §"single-embedder direction"\
**Depends on**: ADR-005 (Storage Capability Traits), ADR-030 (Retrieval Stack Port)\
**Related**:

- ADR-012 (Retrieval Composition) — composes over this layer
- ADR-024 (Fold Cognitive Primitives) — Objective implementations operate on candidates this layer produces
- ADR-028 (Pack-Scoped Backends) — pack `engines = [...]` in `khive.toml` drives `filter()`
- ADR-029 (SubstrateCoordinator) — backend-level unweighted RRF is distinct from the engine-level weighted RRF here
- ADR-033 (Recall Pipeline) — memory recall consumes the pack fan-out pattern specified here
- ADR-035 (CLI Config) — project-vs-user TOML override semantics extended by this ADR
- ADR-051 (Knowledge Section Embeddings) — knowledge uses only its searchable default model

## Context

An earlier implementation delivered multi-engine embedding: N peer models run concurrently, each
with its own HNSW index, with participating writes embedding in all N models and corresponding
queries fusing per-engine rankings via weighted RRF. `deploy/engine.toml` was the canonical config; per-engine
normalization parameters (`noise_floor`, `max_similarity`, `threshold`) were tuned during the
2026-03-26 Chinese-blindspot crisis (mE5 migration). That crisis confirmed empirically that
no single embedding model dominates across languages and corpus types — multilingual and
paraphrase-heavy corpora require peer engines.

The current release regressed this to single-model:

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
the earlier refactor. The next step was recorded explicitly after the 2026-03-26 event:
"Multi-index architecture: engine.toml + code supports `Vec<EmbedModelConfig>`. Add Qwen3 as
peer after HNSW namespace split." The current release erased this without an ADR.

### What "multi-engine" means

Distinct from multi-actor namespace isolation (per ADR-007) and distinct from model migration
(single model, swap atomically). For a substrate with a multi-engine read path, multi-engine means:

- N peer embedding services run concurrently in the same process
- Each service may be a different provider (lattice-embed native, OpenAI API, custom)
- Each service has its own vector index — one `vec_*` table per (model_id, dim)
- Every participating write embeds with all N services and stores in all N indices
- Every corresponding query embeds with all N services, searches all N indices in parallel, then fuses
- Results merge via weighted RRF using per-engine weight
- Per-engine score normalization (noise_floor, max_similarity, threshold) calibrates
  cross-engine comparability

### Amendment 1 (2026-08-01): readable-model symmetry for knowledge

The fan-out contract is write/read symmetric, not permission to create vectors no serving path
can consume. Entity, note, and memory paths retain multi-engine write and read fan-out. Knowledge
search, ANN warming, fresh-tail fusion, and compose currently select only the default embedder and
expose no model selector or cross-model fusion, so `knowledge.index` writes only that default
model. ADR-051 Amendment 1 records the same knowledge-specific decision. Multi-model knowledge
indexing must land together with a model-aware or fused knowledge read path; until then secondary
knowledge vectors are dead embedding and storage work. This supersedes Phase C's unqualified
"pack handlers fan out" wording and Open Question 2's deferral for this specific substrate.

### Why "engine" not "model"

A model is an `EmbeddingModel` variant — a specific weights file with a specific
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
   engines = N `Embedder` instances. This matches the
   `NativeEmbeddingService::with_model(model)` pattern from the prior implementation.
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
engines fail, the request fails. This follows the prior availability-over-strict-consistency
behavior. A future `embed_query_all_strict()` variant may be added for operators
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

**Default search path**: `.khive/config.toml` relative to the MCP server's working directory
(project-local). This collocates config with the per-project `khive-test.db` that already
lives under `.khive/`. `~/.khive/config.toml` is reserved for personal/global settings and
is NOT searched automatically — use `--config` or `KHIVE_CONFIG` to point at it explicitly.

**Override semantics**: project-local `.khive/config.toml` sets the engine list.
There is no per-entry merge with any global config. Replacing, not merging, enforces
project-consistency — collaborators sharing a project must run the same engines, so
vectors are produced by the same models.

Machine-local fields (`device`) are user-level only. The project commits to `name` / `dim` /
`weight` / calibration; the execution environment is operator-local.

**Single-engine fallback**: if no project config declares `[[engines]]`, kkernel falls
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
invariant from `foundation/embed/DESIGN.md` (earlier implementation, not in this repository).

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
    # Both arrays preserve registry declaration order: [engine_0, ..., engine_n].
    weights      = registry.engines().iter().map(|e| e.config.weight as f64).collect()
    vector_fused = khive_fusion::fuse(
        per_engine_hits, FusionStrategy::Weighted { weights }, candidate_pool_size)

    # Step 4 — layer FTS5 keyword path and final fusion
    text_hits = self.runtime.text(args.namespace).search(...)
    fused     = fuse([vector_fused, text_hits], args.fusion_strategy, args.limit)

    # Step 5 — pack-specific scoring
    return apply_pack_scoring(fused, args)
```

**Write-side mirror**:

```text
async fn handle_remember(args):
    registry = self.runtime.embedders()
    id = self.runtime.create_note(args.content, ...)

    embeddings = registry.embed_document_all(args.content)  # document_prefix applied per D1
    for (cfg, vector) in embeddings:
        self.runtime.upsert_vector(args.namespace, cfg.name, id, vector)
```

**Per-engine normalization** (`noise_floor`, `max_similarity`, `threshold` from `EngineConfig`):

- `noise_floor`: cosine scores below this are treated as noise; discarded before fusion.
- `max_similarity`: cap for normalization — brings disparate engines onto a comparable scale.
- `threshold`: per-engine minimum score to enter the fusion stage.

These parameters were tuned empirically in the earlier implementation. v1 inherits those defaults;
per-corpus retuning is operator responsibility.

**Weighted RRF rationale**: per-engine weight encodes relative quality for the deployment's
corpus. BGE's English semantic strength, mE5's multilingual coverage, Qwen3's instruction-
tuned recall — operators set weights from empirical retrieval quality. `FusionStrategy::Weighted`
already exists in `khive-fusion`; pack handlers wire `EngineConfig.weight` into it.

The positional contract at this stage is N-engine registry order: the source at
index `i` is weighted by the config at index `i`. The later two-arm hybrid stage
uses `[combined_vector, text]` (the runtime/retrieval `[vector, keyword]`
contract). Neither rule changes the dual-index migration router's independent
`[primary, legacy]` contract.

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
// crates/khive-storage/src/sparse.rs (implemented shape: see Amendment 2)
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

The current regression in the current release is the result of exactly this. Multi-engine
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
  matching the prior tuned shape
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
  match the prior tuned schema
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
  mitigated by single-engine fallback and pre-tuned defaults inherited from the earlier implementation
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
schema activated; handlers with multi-engine reads fan out across all configured engines.
`SparseStore` trait
added. `memory.recall_*` dotted verbs added. New deployments get multi-engine by declaring the array;
existing single-engine deployments see no behavior change.

## Open Questions

1. **`MultiEngineSearcher` helper extraction.** Defer until three or more packs need
   identical fan-out code. Pack handlers copy the pattern; extract when the duplication
   threshold is reached.
2. **Multi-engine write policy.** Default for a multi-engine read path: every engine embeds every
   write. A future `write_engines` allowlist on `EngineConfig` (or `PackConfig`) would allow a pack
   to store writes in a subset of engines while reading from all. Deferred to v2. Knowledge's
   default-only policy is a different case: it writes exactly the one engine it can read.
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
- `deploy/engine.toml` (earlier implementation, not in this repository) — canonical multi-engine
  schema being restored
- `foundation/embed/DESIGN.md` (earlier implementation, not in this repository) — INV-1..INV-8
  invariants; per-(model, dim)
  table sharding rationale; asymmetric retrieval prefix invariant
- `apps/cli/src/server/unified.rs:414-664` — `resolve_embed_models`,
  historical multi-engine wiring; D2 pattern source
- `summary_20260326_165542_recall_overhaul_multi_index_architecture.md`

---

## Addendum — `[[engines]]` TOML config surface (v024/engines-toml-config, 2026-05-25)

### Motivation

The current release had no config-file path for engine registration. Operators had to set env
vars:

```
KHIVE_EMBEDDING_MODEL=all-minilm-l6-v2
KHIVE_ADDITIONAL_EMBEDDING_MODELS=paraphrase,bge-small-en-v1.5
```

This two-tier hack is a regression from D3's specified `[[engines]]` array. The Addendum
implements D3's TOML schema for the MCP binary boot path.

### Decision

**New module**: `crates/khive-runtime/src/engine_config.rs`

- `KhiveConfig` — top-level config struct; `[[engines]]` array; future sections addable.
- `EngineConfig` — per-engine: `name`, `model`, `default`, `fusion_weight`, `dims`.
- `KhiveConfig::load(path: Option<&Path>) -> Result<Option<Self>, ConfigError>` — loads and
  validates the config file. Returns `Ok(None)` when no file is found.
- `config_from_env() -> KhiveConfig` — builds an in-memory `KhiveConfig` from the legacy
  env-var path; emits `tracing::info!` to direct operators to the config file.

**New function**: `runtime_config_from_khive_config(cfg: &KhiveConfig, base: RuntimeConfig)`

Converts `KhiveConfig` to `RuntimeConfig`: the `default = true` engine becomes
`RuntimeConfig::embedding_model`; others go to `additional_embedding_models`. Unknown model
names are skipped with a warning.

**CLI flag** (`crates/khive-mcp/src/args.rs`):

```
--config <PATH>    (env: KHIVE_CONFIG)
```

Default search path: `.khive/config.toml` relative to the server's working directory
(project-local). `~/.khive/config.toml` is reserved for personal/global defaults and is NOT
searched automatically; the project-local default keeps config co-located with the KG database
that already lives under `.khive/`.

**Validation** (in `KhiveConfig::validate`):

- Exactly one engine with `default = true` (error: `ConfigError::DefaultCount`).
- Unique engine names (error: `ConfigError::DuplicateName`).
- `fusion_weight` finite and > 0 when present (error: `ConfigError::InvalidFusionWeight`).
  Until per-engine fusion is wired, a valid explicit weight is then refused with
  `ConfigError::UnsupportedFusionWeight` rather than silently ignored.

**Backward compatibility**: when no config file is present, the env-var path is used
automatically. `RuntimeConfig::default()` continues to read `KHIVE_EMBEDDING_MODEL` and
`KHIVE_ADDITIONAL_EMBEDDING_MODELS`. If both file and env vars are present, the file wins and
a `tracing::warn!` is emitted.

**The env-var path is now "fallback for testing/dev"**: in production, the `[[engines]]` TOML
config is the authoritative surface. The env-var path has no roadmap for removal — it handles
containerised/CI deployments where file-based config is inconvenient — but it is no longer
the primary interface.

**`fusion_weight` integration note**: when engines declare `fusion_weight`, the values are
available on each `EngineConfig` for pack handlers to inject into `FusionStrategy::Weighted`.
For pure rank-based unweighted RRF the weights are ignored (as stated in D5). Pack handlers
are responsible for reading `EngineConfig.fusion_weight` and building the appropriate fusion
strategy; no automatic wiring exists yet. In the current implementation,
`RuntimeConfig` retains the engine models but drops their weights, and no retrieval handler
reads them. The loader therefore rejects explicit `fusion_weight` until that integration is
implemented. This is a fail-closed implementation status, not a change to D5's target semantics.

**Example config**: `docs/khive-config-example.toml` ships as a reference.

### Files

| File                                        | Change                                                                              |
| ------------------------------------------- | ----------------------------------------------------------------------------------- |
| `crates/khive-runtime/src/engine_config.rs` | New — `EngineConfig`, `KhiveConfig`, `ConfigError`, `config_from_env`, 9 unit tests |
| `crates/khive-runtime/src/runtime.rs`       | Added `runtime_config_from_khive_config`                                            |
| `crates/khive-runtime/src/lib.rs`           | `pub mod engine_config`; re-exports                                                 |
| `crates/khive-runtime/Cargo.toml`           | Added `toml = { workspace = true }`                                                 |
| `crates/Cargo.toml`                         | Added `toml = "0.8"` to workspace deps                                              |
| `crates/khive-mcp/src/main.rs`              | `--config` / `KHIVE_CONFIG` flag; `resolve_embedding_config`                        |
| `crates/khive-mcp/Cargo.toml`               | `tempfile` in dev-deps                                                              |
| `crates/khive-mcp/tests/integration.rs`     | `engine_config_three_engines_all_registered` test                                   |
| `docs/khive-config-example.toml`            | New — annotated example config                                                      |

The table above records the files this ADR's implementation touched at the time.
`crates/khive-mcp/src/main.rs` was removed in `2f2f3ca7` when the binaries were
unified; the `--config` / `KHIVE_CONFIG` flag now lives in
`crates/khive-mcp/src/args.rs`.

---

## Addendum — Pack-extensible EmbedderRegistry (PR #397, 2026-05-25)

### Motivation

ADR-031 §D2 describes registering multiple lattice embedding models at boot time via
`RuntimeConfig::additional_embedding_models`. However, the registry was a closed `HashMap<String,
EmbedderEntry>` wrapping only `lattice_embed::EmbeddingModel` variants — packs could not
contribute non-lattice embedding backends.

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
cached). Last-writer-wins on duplicate name registration (pack order is not guaranteed).

**KhiveRuntime integration**:

- `embedder_registry: Arc<RwLock<EmbedderRegistry>>` replaces `embedders: Arc<HashMap<...>>`.
- `KhiveRuntime::register_embedder(provider)` — public, callable post-construction.
- Existing `embedder(name)`, `resolve_embedding_model(name)`, `registered_embedding_model_names()`
  continue to work: alias resolution still normalises lattice short-names before registry lookup;
  custom (non-lattice) provider names bypass alias resolution and look up the registry directly.
- `RwLockGuard` is never held across `await` — entries are cloned before `OnceCell::get_or_init`.

**Pack extension hook**:

```rust
// PackRuntime trait (khive-runtime/src/pack.rs)
fn register_embedders(&self, _runtime: &KhiveRuntime) {}   // default no-op
```

Packs that provide custom embedding backends implement this method; the transport should call it
during pack initialisation before the first verb dispatch.

**Backwards compatibility**: `RuntimeConfig::embedding_model` and
`additional_embedding_models` remain. Built-in lattice models are pre-registered as
`LatticeEmbedderProvider` instances during `KhiveRuntime::new` / `from_backend`. No callers
need changes.

### Files

| File                                                       | Change                                                                                                                                       |
| ---------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------- |
| `crates/khive-runtime/src/embedder_registry.rs`            | New — `EmbedderProvider`, `EmbedderRegistry`, `LatticeEmbedderProvider`, unit tests                                                          |
| `crates/khive-runtime/src/runtime.rs`                      | Refactored — `embedder_registry` field, `register_embedder`, updated `embedder`/`resolve_embedding_model`/`registered_embedding_model_names` |
| `crates/khive-runtime/src/pack.rs`                         | `PackRuntime::register_embedders` default no-op added                                                                                        |
| `crates/khive-runtime/src/lib.rs`                          | `pub mod embedder_registry`; re-exports                                                                                                      |
| `crates/khive-runtime/tests/integration.rs`                | 4 new integration tests in `embedder_registry_tests` module                                                                                  |
| — Chinese-blindspot crisis; per-engine calibration history |                                                                                                                                              |

---

## Amendment 2 (2026-09-25): implementation record for D1, D2 and D6

Status: Accepted (2026-09-25)

### Context

D1 and D2 place the embedder trait, the lattice adapter and the registry in a new `khive-embed`
crate (`crates/khive-embed/src/trait.rs`, `lattice.rs`, `registry.rs`), Migration Phase A creates
that crate, D3 moves `vec_model_key` into it, and D4 says `khive-runtime` drops its direct
`lattice-embed` dependency. No `khive-embed` crate exists in this repository or in its history.
The registry was built inside `khive-runtime`, as the Addendum "Pack-extensible EmbedderRegistry
(PR #397)" above records:

- `crates/khive-runtime/src/embedder_registry.rs` defines `EmbedderProvider` (the provider trait:
  `name`, `dimensions`, and an async `build` returning an `EmbeddingService`), `EmbedderRegistry`,
  and `LatticeEmbedderProvider`. No trait named `Embedder` exists.
- `vec_model_key` stays in `khive-runtime` (`crates/khive-runtime/src/config.rs`, crate-private).
- `lattice-embed` is a direct, non-optional dependency of `khive-runtime`
  (`crates/khive-runtime/Cargo.toml`). There is no `lattice` feature, so the lattice-free build
  described in D1 and D4 is not available.

D6 specifies a two-method `SparseStore` in `crates/khive-storage/src/traits.rs`, implemented in
`khive-db-ruvector`. The trait is defined in `crates/khive-storage/src/sparse.rs` with a different
shape: `insert_sparse(subject_id, kind, namespace, field, vector)`,
`insert_batch(Vec<SparseRecord>)`, `delete(subject_id)`, `search_sparse(SparseSearchRequest)` and
`count()`. Its implementation is `SqliteSparseStore` in `crates/khive-db/src/stores/sparse.rs`.
There is no `khive-db-ruvector` crate.

### Decision

Record the implemented placement as the current state of D1, D2 and D6:

1. The provider-agnostic embedding seam D1 calls for is `EmbedderProvider` together with
   `EmbeddingService`, and the process-wide registry D2 calls for is `EmbedderRegistry`, both in
   `khive-runtime`. This ADR no longer plans the separate `khive-embed` crate, the `Embedder` trait
   name or the `lattice` feature. Extracting the registry into its own crate later needs its own
   amendment.
2. The `SparseStore` contract is the trait in `crates/khive-storage/src/sparse.rs`. The code block
   in D6 is the original sketch and stays in place as the record of what was first proposed.

This amendment changes no code and does not restate or change D3's `[[engines]]` schema, D4's
`model_id` routing or D5's fan-out.

### Alternatives considered

- **Extract `khive-embed` as Phase A specifies.** This moves the registry and its lattice adapter
  into a new crate and changes every import of the registry types. No current consumer needs a
  lattice-free build, and the Addendum's pack-extensible registry already admits non-lattice
  providers without it.
- **Leave D1, D2 and D6 as written.** A reader following them looks for a crate, a trait and a
  file that do not exist.

### Consequences

- Readers find the embedder seam in `khive-runtime` and the sparse contract in
  `crates/khive-storage/src/sparse.rs`.
- Migration Phase A reads as delivered by the Addendum's registry rather than by a new crate.

### Refs

- The Addendum "Pack-extensible EmbedderRegistry (PR #397)" above.

---

## Amendment 3 (2026-09-25): a configured `fusion_weight` is refused until retrieval applies it

Status: Accepted (2026-09-25)

### Context

The `[[engines]]` Addendum above validates "`fusion_weight` > 0 when present (error:
`ConfigError::InvalidFusionWeight`)" and notes that "Pack handlers are responsible for reading
`EngineConfig.fusion_weight` and building the appropriate fusion strategy; no automatic wiring
exists yet."

At this revision:

- `KhiveConfig::validate` (`crates/khive-runtime/src/engine_config.rs`) refuses a non-finite or
  non-positive `fusion_weight` with `ConfigError::InvalidFusionWeight` and accepts any finite
  positive value.
- `runtime_config_from_khive_config` (`crates/khive-runtime/src/config.rs`) turns each engine into
  `RuntimeConfig::embedding_model` or an entry of `additional_embedding_models` and never reads
  `fusion_weight`. `RuntimeConfig` holds no per-engine weight, so no pack handler can reach the
  value, and no retrieval path reads it.
- The doc comment on `EngineConfig::fusion_weight` says the weights are injected into
  `FusionStrategy::Weighted`, and `docs/khive-config-example.toml` describes the weight as scaling
  each engine's contribution. That example sets `fusion_weight = 0.5` on both of its active
  engines.

A weight an operator sets is therefore validated and then has no effect, while the documentation
says it is applied (#3246).

### Decision

1. Validation keeps its existing first step: a non-finite or non-positive `fusion_weight` fails with
   `ConfigError::InvalidFusionWeight`.
2. While no retrieval path applies the value, a finite positive `fusion_weight` on any engine is
   then refused at load with `ConfigError::UnsupportedFusionWeight`, which names the engine and
   states that per-engine weights are not wired into retrieval. A configuration that omits the
   key loads as before.
3. The `EngineConfig::fusion_weight` doc comment and `docs/khive-config-example.toml` state that
   the key is refused until it is applied, and the example config no longer sets it.
4. The change that wires per-engine weights into multi-engine fusion (D5) removes the refusal and
   records the applied semantics in a further amendment.

### Alternatives considered

- **Load the value and warn that it is not applied.** Nothing breaks on upgrade, but a key that
  loads and then does nothing tells the operator the ranking follows their weights when it does
  not. A warning in a log is easy to miss; a refusal at load is not. Rejected.
- **Apply the weights now (D5).** Carry per-engine weights into `RuntimeConfig` and fuse
  multi-engine results with them. This is the intended end state and stays open, but it is a
  larger change than the reporting defect.
- **Keep accepting the value silently.** This leaves #3246 in place.

### Consequences

- A configuration that sets `fusion_weight` on any engine, including a copy of the previous
  `docs/khive-config-example.toml`, stops loading after the upgrade until the key is removed. The
  error names the first engine found that sets it.
- Rankings do not change: the value was never applied.
- Wiring the weights (D5) lifts the refusal; no stored data depends on this rule.

### Refs

- #3246

---

## Amendment 4 (2026-09-30): disclose bounded document-embedding input at the Rust runtime boundary

Status: Proposed (awaiting project-owner ratification)

### Context

The runtime preserves full entity descriptions and note content while bounding the text passed to an
embedder. A caller that receives only a vector or a stored record cannot tell whether the embedding
represents all of that text. This matters for direct Rust consumers as well as MCP handlers. The
`DocumentEmbeddingOutcome` and `EmbeddingTruncationReport` types already carry the runtime's observed
source byte count, embedded byte count, truncated count, and discarded byte count; older public
methods discarded those values.

This amendment governs the public Rust API for runtime-managed document embedding and writes. It
does not describe or certify truncation that may occur inside an independently registered provider.

### Decision

1. Direct document-embedding methods that can bound input expose an outcome carrying `truncated`,
   `source_bytes`, and `embedded_bytes`. Batch methods expose ordered outcomes; write methods expose
   `EmbeddingTruncationReport`. A report counts each bounded embedding input and its omitted bytes;
   the stored entity or note content remains complete.
2. Five public Rust methods that previously returned only vectors or records cease to be public
   entry points. Direct consumers migrate from `embed_document_with_model` to
   `embed_document_with_model_outcome`, from `create_entity` to
   `create_entity_with_embedding_report`, from `update_entity` to
   `update_entity_with_embedding_report`, from `update_note` to
   `update_note_with_embedding_report`, and from the exported `create_notes_atomic` to
   `create_notes_atomic_with_report`. The atomic replacement preserves the same all-or-none note
   write and adds the aggregate report; it does not alter the atomic note-write semantics.
3. Widely used legacy-return methods remain callable. `embed_document`,
   `embed_document_batch_with_model`, and its default-model delegate `embed_document_batch` return
   an explicit error when runtime input bounding occurs. Consumers that need the bounded vectors
   call `embed_document_outcome`, `embed_document_batch_with_model_outcomes`, or
   `embed_document_batch_outcomes` respectively.
4. `create_entity_with_attachments`, `create_note`, `create_note_with_embedding_content`,
   `create_note_with_decay`, `create_note_with_decay_for_embedding_model`, and
   `update_entity_if_unchanged` preserve their ordinary success values when no input is bounded.
   On a bounded embedding, the write may already be
   committed, so they return a structured, non-retryable `embedding_input_truncated` error with
   `committed=true`, the committed `record_id`, and the serialized truncation report. The error also
   retains any post-commit degradation diagnostics. Callers must inspect or reconcile that ID and
   must not retry the mutation as if it rolled back. Report-aware alternatives are
   `create_entity_with_attachments_and_report`,
   `create_note_with_embedding_content_and_report` (pass `None` for the default note path),
   `create_note_with_decay_and_report`,
   `create_note_with_decay_for_embedding_model_and_report`, and
   `update_entity_if_unchanged_with_embedding_report`. Those alternatives return the committed
   record and report together when no independent post-commit failure occurs.
5. A source census may name the known compatibility methods and their alternatives, but must not
   claim to discover every future public method. Behavioral tests exercise oversized inputs at the
   vector-only and record-only boundaries. A new public wrapper that can bound input must either
   return its outcome or report, or disclose truncation with an explicit error.
6. `web.refresh` uses the report-aware guarded-update method for entity promotions and aggregates
   those embedding reports for the refresh settlement. When at least one such input is bounded,
   the reply exposes `embedding_truncation_report` and the stored receipt's `request` object carries
   the same key and value. Both keys are absent when no input is bounded. The report contains the
   existing `truncated` input count and `discarded_bytes` count; it does not add an aggregate
   `embedded_bytes` field. This disclosure is distinct from the network-body `truncated` flag and
   does not change capture ownership, deduplication or replay decisions. Tests assert the exact
   report on both surfaces, its absence on ordinary input, and the exact bytes passed to the
   provider; removing the receipt key must make the receipt-disclosure test fail.
7. The entity-type backfill scan aggregates the reports from successful guarded updates in its
   serialized `embedding_truncation_report`. A bounded embedding does not turn a committed
   promotion into a scan failure; independent update failures retain the existing failure behavior.
   The stock command disables embedding providers and reports zero counters. A registered-provider
   scan fixture separately proves bounded-input disclosure and continued promotion of later rows.

### Boundary of this amendment

The compatibility rule above covers the named direct vector, create, and guarded-update methods.
Claim and reindex operations, entity and note restore, stream batch results, best-effort/trusted
ingest, and backfills other than the entity-type scan above have different success and receipt
contracts. They require a separate design
for aggregate or per-member disclosure; this amendment makes no assertion that their current
return values disclose all truncation. The private atomic post-commit mapper is likewise outside
the public API census.

Further surfaces still drop the embedding report and are not yet disclosing. Each reaches a
bounded embedding only when its embedded text exceeds the document-embedding budget: 32768 UTF-8
bytes, less the model's document-instruction prefix for a model that defines one.

- The web pack's shared entity helpers `get_or_create` and `patch`, reached from `web.fetch` and
  `web.extract` and redirect settlement in `web.refresh`, call the report-aware update and
  discard its report. For a document entity, `get_or_create` embeds the entity name, which is the canonical URL, so a canonical URL
  over the budget is embedded truncated without any signal from that helper in the verb reply.
  `patch` re-embeds the stored name and description when it changes `entity_type`. Section 6
  covers `web.refresh`'s guarded promotion and metadata updates; it does not cover the reports
  discarded by these shared helpers when refresh creates redirect endpoints.
- `create_web_receipt_note`, called by the web receipt writer, returns the stored receipt without
  its report. The receipt summary embeds a requested URL or a search query, so a summary over the
  budget is embedded truncated without any signal.
- The public `keyed_memory::create_keyed_memory` returns `Ok` with the stored memory when its
  content exceeds the budget, and drops the report that `create_keyed_memory_with_report`
  returns. Nothing in a production path calls it: `memory.remember` uses
  `create_keyed_memory_with_receipt_and_report` to retain both the original visibility fences and
  truncation accounting. An external Rust consumer that calls it keeps the silent behavior.
- The receipt-only memory APIs `create_keyed_memory_with_receipt` and
  `create_note_with_decay_for_embedding_model_with_visibility` keep their existing return shapes
  and omit the truncation report. `memory.remember` uses the combined receipt-and-report forms
  instead; callers that need both disclosures can use those forms directly.

Bringing these under the rule in decision 5 needs its own change, because each changes a return
shape or a verb reply.

### Consequences

- External Rust consumers of the five narrowed methods must migrate to the named report-aware
  alternatives. MCP verbs and parameters are unchanged; `web.refresh` adds the conditional reply
  and receipt-request report described above.
- Legacy write callers that previously received `Ok(record)` for bounded input now receive an
  error identifying the committed record. Normal-length input keeps its previous return shape.
- Runtime byte accounting does not attest to provider-internal preprocessing or token limits.

### Related decisions

- [ADR-011](ADR-011-embedding-and-inference.md) established the original vector-only runtime
  examples; this amendment supersedes those examples for document embedding.
- [ADR-099](ADR-099-bulk-apply-atomic-units.md) governs cross-operation atomic writes; the
  report-aware note export does not alter its transaction semantics.

## Amendment 5 (2026-10-08): ordered peer engines and explicit retrieval strategies

**Status**: proposed.

### Context and scope

The implemented configuration splits embedding models into one default and additional models. The registry is unordered, configured fusion weights are refused, and only memory recall currently fans out over registered engines. Other retrieval paths select one model. The `Weighted` fusion variant performs normalized linear score blending; it does not implement weighted reciprocal rank fusion.

This amendment establishes one ordered list of peer engines and a mandatory per-query strategy. It covers memory retrieval and its retrieval subhandlers, entity/note search, knowledge retrieval, and runtime hybrid retrieval, including composite retrieval entry points. It does not add a sparse leg, change write transaction guarantees, introduce remote-provider services, or alter pack-specific scoring objectives.

### Changes to earlier decisions

1. D2/D3 and the TOML addendum change from a primary/additional configuration to ordered peers. Accepted Amendment 2's provider and registry placement in `khive-runtime` stands.
2. D4 changes to permit runtime adapters to invoke registered providers and delegate candidate execution to `khive-retrieval`. Model-aware physical routing remains required. The unused metadata-only runtime sketch is not the target API for this work.
3. D5 replaces its mistaken use of `Weighted` for weighted RRF with `WeightedRrf`. It retains two fusion stages, but moves their reusable execution into `khive-retrieval`; pack scoring remains outside. D5's pack-only orchestration rationale, Alternative E, and Open Question 1 are superseded to this extent.
4. D4's optional strategy and D6's optional per-call strategy wording become the mandatory contract below. Existing `Weighted`, `Rrf`, `Union`, `VectorOnly`, `KeywordOnly`, and runtime-dispatched `Custom` meanings are not renamed or repurposed.
5. Amendment 1's knowledge write/read symmetry remains a migration gate. Its assertion that every entity/note read already fans out is corrected to describe the intended contract, not the current implementation. The corresponding knowledge ADR must be updated with the activation change.
6. Amendment 3's refusal is lifted only when the configured weights can reach the checked engine-fusion path for every activated retrieval surface. Intermediate releases must continue to refuse unused settings.
7. Proposed Amendment 4's existing truncation reports and provider-attestation distinctions are preserved by this implementation; this amendment does not ratify or enlarge that proposal.

### Decision A — Engine identity, order, and configuration

`[[engines]]` declares an ordered list of peers. Each entry has a stable `name`, `weight` defaulting to `1.0`, optional `dims` as a checked assertion, and optional calibration fields. There is no canonical `default` flag or `[retrieval].default_engine` setting. A configured name resolves to an existing `EmbedderProvider`; built-in names select lattice adapters and other names require a registered host/pack implementation.

```toml
[[engines]]
name = "bge-small-en-v1.5"
weight = 1.0
dims = 384

[[engines]]
name = "team-multilingual-v1" # supplied by a registered provider
weight = 0.8
dims = 768
```

The registry preserves declaration order independently of its lookup map. Provider enumeration, named subsets, results, and resolved weights use that same order. Request order does not rebind weights. No model-count cap is derived from the lattice enum; concurrency, memory, and request budgets remain finite and explicit. Provider registration makes an implementation available; an explicit configured list determines participation. Programmatic registration-only hosts must finalize an ordered participating list before serving. Mutation may not replace a provider underneath an in-flight query.

Names must be unique after built-in alias canonicalization. Duplicate names, alias collisions, unresolved providers, nonpositive dimensions, and dimension mismatches are errors. Duplicate provider registration must not silently replace a serving embedding space. Multiple instances using identical model weights may have distinct provider names, but must have distinct verified storage bindings if their preprocessing or vector spaces differ.

Weights are finite and strictly positive. Zero is not an engine-disable switch; use explicit selection. The first peer is the compatibility choice only for a documented single-engine API and `DefaultModel` note policy. Multi-engine retrieval selects all applicable configured peers unless the request names a subset. An explicitly empty list disables vector participation; a vector-requiring request then errors. A missing configuration follows the existing deployment fallback, converted into a one-entry peer list; this amendment does not silently change the fallback model.

Logical labels, provider identity, and existing physical index keys must not be conflated. Legacy conversion preserves the actual canonical model-to-index binding even when the old configuration used a decorative `name`. Changing order or weight never renames, rewrites, or re-embeds stored vectors. Different dimensions or preprocessing require a new verified embedding-space binding and reindex; matching dimensions alone do not establish compatibility. Until an accepted replacement identity design is available, collisions under the current sanitizer must be refused, not merged.

Calibration is applied within each engine before rank fusion. For cosine-producing adapters, absent values mean `noise_floor=-1`, `max_similarity=1`, `threshold=0`. Require finite `-1 <= noise_floor < max_similarity <= 1` and `0 <= threshold <= 1`. Discard raw similarity below the floor; calculate `u=clamp((s-noise_floor)/(max_similarity-noise_floor),0,1)`; retain `u >= threshold`. Preserve the engine's descending raw-similarity order with stable ID ties, then compact the ranks of retained distinct IDs. Thus a normalization cap does not create an accidental ID ordering among originally unequal high scores. An adapter must expose the agreed cosine/similarity contract before using these settings; it must not label an arbitrary vendor score as cosine. No historical calibration numbers are installed without a provider/corpus-specific basis.

### Decision B — Complete the existing provider seam

Keep `EmbedderProvider`, `EmbedderRegistry`, and the lattice adapter in `khive-runtime`. Configuration binding is finalized after host/pack provider registration and before the first request. Parsing configuration can check syntax first; it cannot reject a custom name merely because lattice's enum cannot parse it.

Named query and document invocation must dispatch the correct retrieval role through the registered provider. Extend the existing provider contract with role-aware methods as needed, using the registry's cached service; do not create a second provider abstraction or rebuild the service for every request. Built-in implementations retain their current prefix conventions. A custom implementation owns its role preparation and must not receive accidental lattice prefixes from a placeholder enum. Legacy generic services may remain callable by their old API, but cannot be declared conforming to the new role-aware retrieval contract without the adapter capability and its tests.

Validate batch cardinality, ordering, finite coordinates, and exact declared dimensions before indexing/search. Preserve runtime input-bounding reports; do not claim visibility into preprocessing or truncation inside custom providers. Query caches and index handles must be keyed by the resolved engine identity and relevant role/preparation identity, not by “default”.

### Decision C — Required request contract and response evidence

Each retrieval request supplies a `strategy` value containing **both** `engine_fusion` and `hybrid_fusion`. This is a composition of existing `FusionStrategy` values, not another competing strategy hierarchy. Engine weights resolve by name from configuration, with optional explicit name-keyed request overrides; positional arrays are produced internally only after name validation.

Example wire contract:

```json
{
  "query": "multilingual research",
  "engines": ["bge-small-en-v1.5", "team-multilingual-v1"],
  "strategy": {
    "engine_fusion": { "weighted_rrf": { "k": 60 } },
    "hybrid_fusion": { "weighted": { "weights": [0.7, 0.3] } }
  }
}
```

`engine_fusion` supports `weighted_rrf` and explicitly unweighted `rrf` in this lane. Both require `k >= 1`. For `rrf`, the response states that effective engine weights are all one; configured weights are deliberately not applied under that explicit choice. Name-keyed weight overrides are rejected for an unweighted selection. Other engine-stage variants are rejected until their multi-engine semantics are specified. `engine_fusion: null` is required with `hybrid_fusion: "keyword_only"`; it is invalid for a vector-using request. One-engine requests still select a strategy. A vector-only request skips text but still applies its selected engine fusion.

The hybrid stage accepts the existing strategies, including the new rank-weighted variant over exactly `[combined_vector, text]`. Weighted hybrid strategies require exactly two finite positive weights. Rank strategies require an explicit `k`; there is no hidden per-pack value. Custom hybrid fusion resolves through the existing runtime `FusionExecutor` registry; unsupported or unregistered custom strategies return an error, never RRF fallback. `Union` retains its existing max-score meaning and carries that score kind; it must not be described as scale-independent.

Missing/null/incomplete `strategy`, unknown engines, duplicate selections, unknown weight-map keys, nonfinite/nonpositive weights, unsupported stage combinations, and ambiguous legacy/new fields fail before embedding or ANN work. Omitting `engines` means the configured applicable peer set, not the first entry. Explicit selection outside the applicable configured set fails.

The successful retrieval outcome is an envelope containing `results` and `retrieval`. `retrieval` contains the resolved two-stage strategy and its version, requested/selected/used engines in canonical order, effective weights, per-engine requested and returned candidate counts, per-arm status/reason, text participation, and degraded status. It distinguishes an arm that ran with zero hits from one that was skipped, unavailable, or failed. Scores identify the fusion stage/score kind; they are not relabelled as raw cosine or probability. Zero-result responses carry the same evidence. Diagnostic envelopes carry no raw query vectors.

`memory.recall`, KG `search` for entities and notes, `knowledge.search`, and runtime hybrid retrieval obey this contract directly. Retrieval subhandlers (`recall_embed`, `recall_candidates`, `recall_fuse`, `recall_rerank`, `recall_score`) consume the explicit strategy or a validated upstream envelope that already carries it; no subhandler can recreate a default. Pure vector generation is not fusion, but the retrieval pipeline's embed stage must preserve the selected plan.

Composite retrieval APIs such as query-driven `context`, natural-language `resolve`, knowledge composition/suggestion, and tool suggestion propagate the caller's strategy. Deterministic by-ID access, SQL/structured listing, graph traversal without search, and writes are not retrieval-strategy requests. A write performing advisory similarity search supplies its own explicit internal policy at the call site and discloses that policy with the advisory result. Pure by-ID resolution does not require irrelevant strategy, but a request that may use similarity fallback must provide it before that fallback executes. The release census must classify all such entry points; four updated flagship verbs alone do not establish coverage.

### Decision D — Fusion semantics

Add `FusionStrategy::WeightedRrf { k, weights }` to `khive-fusion`. For ordered engine lists `L_i`, each containing distinct IDs with one-based ranks:

```text
vector_score(d) = sum_i [d in L_i] * w_i / (k_engine + rank_i(d))
```

Weights are not normalized in this primitive. Equal weights of one reproduce ordinary RRF; multiplying all engine weights by the same positive scalar scales the aggregate score. Duplicate occurrences in a source count once at their best rank. Ties use stable ascending IDs. Validate weight/source cardinality and numeric validity even for empty results. Preserve an empty positional slot for failed or empty selected engines so weights cannot shift. Unknown/missing weights must not be padded, zeroed, clamped, or replaced with uniform weights. Arithmetic outside the representable deterministic-score range is an explicit error, not silent saturation; tests cover boundary rounding and accumulation.

Engine candidate depth is distinct from `k_engine`: `k` smooths rank contribution; it is not top-k, a pool size, or an engine-count correction. Use the same requested pool depth for peers by default. Unequal realized depths due to filtering, smaller corpora, or failures leave missing contributions at zero. Do not renormalize by returned pool size or number of successful engines. Any explicit per-engine pool override is part of the resolved evidence and evaluation configuration.

Fuse engine rankings, then fuse exactly two modality slots `[combined_vector, text]` under `hybrid_fusion`, retaining empty slots. Engine-stage and hybrid-stage `k` values are separate. Retain the complete bounded candidate union through both fusion stages and the pack's eligibility/scoring steps; apply the final result limit afterward. An additional intermediate cap must be explicit and tested for recall loss, not accidentally inherited from the final limit.

For example, with engine lists `[a,b]` and `[b,a]`, weights `[1,3]`, and `k_engine=10`, scores are `a=1/11+3/12` and `b=1/12+3/11`; `b` wins. Swapping the named weights makes `a` win. A second hybrid RRF uses the rank of this vector aggregate, not those magnitudes. With identical singleton vector lists containing `x` and a text-only singleton `y`, linear hybrid weights `[0.7,0.3]` give `y` a positive text contribution of `0.3` before pack scoring regardless of the number of vector arms.

All new runtime/pack paths use checked fusion. The existing unvalidated retrieval helper's silent fallback is not permitted on these paths. Existing two-slot linear blending remains available; N-engine weighted RRF does not pass through its two-slot weight conversion.

### Decision E — Shared execution and failure boundaries

`khive-retrieval` owns one reusable executor for ordered engine work, bounded concurrency, candidate aggregation, and the two fusion stages. Runtime supplies adapters carrying named provider invocation and store/search access; retrieval must not depend on runtime or a pack. Extend the existing `HybridSearcher` surface for named per-engine inputs and detailed outcomes. Its single-vector operation remains single-space: an unlabelled precomputed vector cannot be broadcast across peers. A multi-engine request with precomputed vectors must bind every supplied vector to its selected engine and validate dimensions.

Packs determine retrieval scope, eligibility, candidate budgets, model-specific ANN/fresh-tail adapters, and post-fusion scoring. Memory decay/salience, knowledge section aggregation, reranking, and session/freshness requirements remain owned by their existing layers. The executor returns per-engine evidence so those policies are not reduced to one anonymous score. Common scheduling/fusion logic is not copied into each pack.

Snapshot the ordered selected engines, identities, parameters, and weights once per request. Preserve existing request cancellation, deadline, Gate, and query-filter semantics. Ordinary provider/index failures can degrade individual arms and must be disclosed; they cannot rebind names or weights. If at least one selected vector engine completes successfully, including a valid empty result, vector execution has succeeded. If all selected vector engines fail, return an error for a vector-only request. A hybrid request may return successful text results with an explicit all-vector-arms-failed degradation, extending D2's previous all-engines-failed rule for that case. A lexical failure in a hybrid request remains an error, matching the restored runtime's explicit text-leg error contract. Cancellation, exhausted request deadlines, invalid configuration, and unmet requested consistency guarantees are fatal, not ordinary arm degradation.

Concurrent fan-out does not promise O(1) cost: total embedding/index work scales with N, and shared accelerator contention can increase latency. Enforce existing request/embedding admission and bound outstanding engine work. Never create unbounded tasks because configuration permits arbitrary engine counts.

### Decision F — Wiring, symmetry, and deferred policy

`hybrid_search_with_strategy` and `FusionStrategy` are required integration points. Evolve the restored runtime entry point into the explicit composite-strategy route and connect production callers; an unused wrapper does not satisfy this amendment. `hybrid_search` must take/forward an explicit strategy in the new API. Default-only raw-vector/rerank operations remain explicitly single-engine and require a named space in their replacement API. No vector can be inferred to belong to the first configured engine solely from its dimension.

`DualIndexRouter` remains an optional per-engine index-generation migration mechanism. Its primary/legacy pair is not the peer-engine list; its independent weight ordering remains unchanged. The restored `query_ir`, metrics, and persistence surfaces remain available. This lane does not require speculative rewrites of those modules. Existing relevant filters, diagnostics, and persisted engine/index identity must survive adapter integration; versioned query caches include the resolved strategy and engine selection. No restored surface is deleted as cleanup.

Knowledge activation is paired: model-aware reads, ANN warming, fresh-tail handling, exact rerank where used, and per-engine index identity must be ready before enabling multi-engine knowledge writes. Existing default-model vectors remain valid for that engine. Backfill other engines with readiness/coverage evidence; do not report them as complete because their indexes exist. Search must distinguish partial coverage from a healthy complete corpus. Before activation, knowledge retains its current single-engine write/read policy. Update its governing ADR with the same cutover contract.

Retain `AllModels` and `DefaultModel` note policies. The latter selects the first configured peer for new writes and reindex, with the migration consequence documented. Do not delete historical vectors merely because a current write policy excludes their engine. Defer arbitrary named subsets; `[note_kinds.<kind>].engines` is unsupported and rejected until a later amendment specifies the full lifecycle.

### Migration and compatibility

1. Land this amendment and the matching knowledge contract before dependent implementation merges. Amend the existing document; do not duplicate an accepted amendment number.
2. Prepare additive checked primitives and adapter APIs before switching public handlers. Keep intermediate support internal or explicitly incomplete; do not accept config weights that deployed readers ignore.
3. Provide a deterministic legacy-config conversion: canonicalize each legacy `model`; place the single old `default=true` model first; preserve remaining distinct model order; preserve actual index keys; map `fusion_weight` to `weight`; default absent weights to one. Duplicate model aliases that previously collapsed require an explicit migration diagnostic. Conflicting old/new keys are errors. The canonical emitted form has no `default` or primary/additional fields. A bounded legacy-input adapter may warn and perform this conversion for the migration release; it does not become a second runtime authority.
4. Preserve the existing no-file/environment fallback through the same conversion. Do not switch existing MiniLM deployments to BGE because an older ADR example named BGE. Custom-only deployments are valid once their providers and explicit ordered list are resolved.
5. Audit the `vec_default` shim before activation. Its implementation was not established by the implementation census. Never reinterpret `vec_default` as whichever engine is first today. Only migrate a legacy table when its original embedding identity is established and compatible with the destination. Unknown provenance or both old/new tables present requires a diagnostic and an explicit reindex/migration decision; never silently merge, overwrite, drop, or relabel it. Preserve data until that decision is applied. This narrows the unconditional rename sketch in D3.
6. Clients migrate to an explicit strategy and the result envelope before strict activation. The old `fusion_strategy` string does not identify both stages and cannot silently stand in for the new object. Old implicit-default calls fail with a migration example in the new contract. If staging needs the old interface, serve it only under its explicitly old version; do not describe it as compliant.
7. Adding a variant to the currently exhaustive published `FusionStrategy` enum is a Rust source break for downstream exhaustive matches, despite being syntactically additive. Runtime signatures and result envelopes are also breaking changes. Release affected published crates with a compatible coordinated minor bump from the 0.10 line and document all source/wire/config changes. Version numbers must be checked at release time. Changelog and dependency updates precede publication; this decision is not publication approval.
8. Reindex only for added/changed embedding spaces or uncovered corpora. Weight, strategy, and declaration-order changes alone do not invalidate stored vectors. Rollback retains old indexes and pins an explicit old client/config version; mixed clients cannot rely on server inference.

### Verification required for acceptance

- **Config/order:** zero/one/two/five engines; more than ten distinct fake providers; non-first legacy default; ordered subsets; alias/name/sanitizer collisions; mixed legacy/canonical keys; provider-not-registered; wrong dimensions; custom-only startup; ignored reserved key refused. No paid network dependency.
- **Provider contract:** fake provider outside the lattice enum, distinct query/document prefixes observed exactly once, correct batch order/cardinality, invalid/NaN vectors rejected, cached construction single-flight, no default lattice loading, truncation reports preserved.
- **Fusion math:** the two-engine weight-reversal example; all-one equivalence with RRF; common scalar scaling; deterministic ties; duplicates; empty/failed middle arm; invalid source/weight arity; invalid k/weights; representability boundaries. A fixture with genuinely different vector spaces/dimensions proves vectors are never broadcast.
- **Text regression:** text-only ID absent from every vector arm; two and five agreeing engines; empty vector or text slot; positive two-arm linear and rank weights. Assert text contribution and selected score, not merely that a jointly retrievable note appears. Also assert that explicit vector-only skips text and keyword-only makes no embedding calls.
- **Request contract:** every classified retrieval entry point rejects omitted strategy before provider calls; complete strategy appears on populated, empty, degraded, and composed responses; unknown custom fusion fails rather than falling back. Dotted stages carry the same validated plan. Name-bound weights survive completion-order permutations and a failed middle engine.
- **Executor/packs:** one provider/index outage; all vectors failed with successful text; all vectors failed in vector-only; text failure; cancellation/deadline; readiness/freshness failure. Preserve filters, Gate context, evidence, pack scoring, and final-limit ordering. Custom-only runtime hybrid retrieval must run vectors even though legacy `embedding_model` is absent.
- **Knowledge/lifecycle:** pre-cutover writes remain single-engine; post-cutover read/write/backfill/restart/warming/fresh-tail paths see each ready engine. Existing vectors remain queryable after config conversion. Reindex respects each kind's policy and provider identity. Test known and unknown `vec_default` provenance without destroying either table.
- **Quality:** freeze a small judged English/CJK/exact-keyword corpus and compare the measured baseline against both two-stage choices with fixed pools. Report candidate recall and ranking separately, by slice; establish acceptance tolerances before inspecting results. Math conformance alone is not evidence of a retrieval-quality improvement.
- **Release:** direct Rust caller compilation for affected APIs, schema/help snapshots, updated client examples, workspace format/check/clippy/tests required by repository policy, and an end-to-end wire matrix. Uncovered indirect retrieval entry points block the compliance claim.

### Risks and explicit unknowns

Two-stage fusion intentionally limits cross-engine consensus to its effect on the vector aggregate; relevance evaluation must assess the cost. Config order affects compatibility-only selection and therefore requires migration disclosure. Remote-provider latency/rate limits remain implementation-specific. Exact storage identity integration with proposed ADR-160 and existing `vec_default` provenance must be resolved before their dependent activation; this amendment provides no evidence that either is already solved. The existing compensated write behavior is not upgraded to cross-engine transaction atomicity by this work.

### Implementation fences

**MAY:** extend the existing provider/registry/strategy/searcher types; add ordinary request/outcome data records and one concrete executor; stage additive APIs; preserve existing indexes and scoring; use registered custom adapters with explicit budgets.

**MAY NOT:** infer an omitted strategy; substitute linear blending for weighted RRF; reinterpret `Weighted`; drop or shift arm weights; broadcast unlabelled vectors; use HashMap iteration as weight order; silently replace providers or accept unused config; move pack policy into the executor; create a new embedder crate; remove restored surfaces; claim knowledge fan-out before paired indexing/readiness; silently change physical space identity.

**VERIFY BY:** the acceptance matrix above plus source/wire migration examples and a production call path through the restored strategy runtime entry point. Proposed status persists until architectural sign-off; implementation passing tests is not substitute ratification.
