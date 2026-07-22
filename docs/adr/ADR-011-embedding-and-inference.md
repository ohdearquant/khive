# ADR-011: Embedding and Inference Architecture

**Status**: accepted\
**Date**: 2026-05-23\
**Authors**: khive maintainers

## Context

khive turns text into vectors for retrieval and scoring. Two upstream concerns
sit between caller input and the vector storage layer:

1. **Embedding generation**: converting text to one or more dense vectors via a model.
2. **Inference composition**: model loading, caching, batching, and (future) cross-encoder
   reranking.

The architecture must satisfy:

1. **Zero-service deployment.** khive ships as a single Rust binary. No separate embedding
   service, no HTTP wire, no managed inference endpoint.
2. **Pure-Rust inference.** No ONNX runtime, no Python interpreter, no C++ libtorch.
   Embedding must work on every platform khive supports without per-platform binary
   downloads beyond the model weights themselves.
3. **Multi-vector capability.** A subject may need one vector (sentence embedding), many
   vectors (per-token, per-section, per-modality), or learned aggregations. The embedding
   layer must support all three without forcing a closed taxonomy on downstream consumers.
4. **Method neutrality.** khive's own retrieval methods are first-class peers of any
   third-party method. The architecture must not bake in a specific aggregation taxonomy
   (ColBERT MaxSim, AvgSim, SumMax) at the trait/contract layer.
5. **Independent model evolution.** Models evolve faster than khive. Adding `bge-large-v2`
   or `qwen3-embedding-8B` must not require a khive release.

## Decision

### Direct in-process embedding dependency

`khive-runtime` depends on a published embedding library as a normal Rust crate.
Embedding runs in process through its native service implementation. There is no
duplicate embedding service trait inside khive; the runtime uses the upstream trait
directly.

```rust
// In khive-runtime
pub struct KhiveRuntime {
    backend: Arc<StorageBackend>,
    config: RuntimeConfig,
    embedder: Arc<OnceCell<Arc<dyn EmbeddingService>>>,
    ...
}

impl KhiveRuntime {
    pub async fn embed(&self, text: &str) -> RuntimeResult<Vec<f32>> { ... }
    pub async fn embed_batch(&self, texts: &[String]) -> RuntimeResult<Vec<Vec<f32>>> { ... }
}
```

The boundary is an ordinary Cargo dependency, not a network service contract.

### `EmbeddingService` is the contract

The embedding library defines `EmbeddingService`. `khive-runtime` treats that trait as
the embedding contract; no khive-side trait wraps it.

```rust
// From the embedding library
#[async_trait]
pub trait EmbeddingService: Send + Sync {
    async fn embed(&self, texts: &[String], model: EmbeddingModel)
        -> Result<Vec<Vec<f32>>>;
    async fn embed_one(&self, text: &str, model: EmbeddingModel)
        -> Result<Vec<f32>>;
    fn model_config(&self, model: EmbeddingModel) -> ModelConfig;
}
```

When the upstream library extends this trait with token-level, image, or multi-vector
outputs, khive can adopt those methods without maintaining a translation layer.

### Model selection

The active model is configured via `RuntimeConfig.embedding_model: Option<EmbeddingModel>`,
overridable via `KHIVE_EMBEDDING_MODEL` environment variable. The default is
`AllMiniLmL6V2` (384d, ~80MB weights).

```rust
pub struct RuntimeConfig {
    pub embedding_model: Option<EmbeddingModel>,
    ...
}
```

Setting `embedding_model` to `None` alone does not disable embedding: `additional_embedding_models`
is populated independently and those models are still registered. Both `embedding_model` and
`additional_embedding_models` must be empty to disable built-in embedding model registration -
use `RuntimeConfig::no_embeddings()`, the canonical constructor for this. With both cleared,
direct vector search returns `Unconfigured("embedding_model")`, hybrid search skips the vector
leg and falls back to text-only results, and text-only search / graph traversal still work. This is the "KG without semantic search" deployment. Custom embedder
providers registered later by packs are not affected by this setting.

Model overrides at the call site are not currently supported: the runtime is configured
with one active model. Multi-model serving (different models for different namespaces or
substrates) is deferred to a future ADR.

### Lazy load and LRU cache

The embedding service is initialized on first use via `OnceCell`. Model weights load on
the first `embed()` call (cold start). Subsequent calls reuse loaded weights. The native
service is wrapped in `CachedEmbeddingService` which provides LRU caching of repeated
inputs.

```rust
pub async fn embedder(&self) -> RuntimeResult<Arc<dyn EmbeddingService>> {
    let model = self.config.embedding_model.ok_or(Unconfigured("embedding_model"))?;
    self.embedder.get_or_init(|| async move {
        let native = Arc::new(NativeEmbeddingService::with_model(model));
        let cached = CachedEmbeddingService::with_default_cache(native);
        Arc::new(cached) as Arc<dyn EmbeddingService>
    }).await.clone()
}
```

Cold start cost is paid once per process lifetime. Idle deployments pay nothing -
weights are not loaded until needed.

### Multi-vector capability at the storage boundary

A subject may carry one or many vectors. ADR-005's `VectorStore::insert` accepts
`Vec<Vec<f32>>`: one vector is the common case, many vectors is the late-interaction
or multi-modal case.

The embedding pipeline produces whatever shape the consumer needs:

```rust
// Single sentence embedding (the v1 default)
let vec: Vec<f32> = runtime.embed(text).await?;
storage.insert(ns, id, "body", vec![vec]).await?;

// Per-section embedding (future, when the provider exposes section tokenization)
let vecs: Vec<Vec<f32>> = runtime.embed_sections(document).await?;
storage.insert(ns, id, "body", vecs).await?;

// Per-token embedding (future, when the provider exposes last-hidden-state output)
let vecs: Vec<Vec<f32>> = runtime.embed_tokens(text).await?;
storage.insert(ns, id, "body", vecs).await?;
```

The trait is open. Today's khive ships single-vector embedding by default; the
multi-vector path requires an embedding provider that exposes token or section outputs
and a backend that stores and searches multi-vector records.

### Method neutrality

khive does not standardize multi-vector aggregation. ADR-005's `VectorSearchRequest`
carries an opaque `backend_hints: Option<serde_json::Value>` field. Each backend
defines its own hint vocabulary:

```text
HNSW + ColBERT mode:
  backend_hints = {"scoring": "max_sim" | "avg_sim" | "sum_max"}

khive-db single-vector mode:
  backend_hints = null  (cosine/dot, no aggregation needed)

section-aware backend:
  backend_hints = {"pooling": "mean", "normalize": true}

future:
  backend_hints = whatever the backend documents
```

ColBERT-style late interaction is one option among many. Mean pooling, per-section
scoring, and learned combinations are equally valid. The architecture remains open
by design.

### Cross-encoder reranking

Cross-encoder rerank (query, candidate → score) uses an in-process inference
implementation. See ADR-042 for the concrete call site and latency budget. The same
boundary as embedding applies: a direct library call with no HTTP service abstraction.

When cross-encoder reranking is not configured, hybrid retrieval uses bi-encoder fusion
(RRF over text + vector). This remains a supported lower-cost configuration.

### What lives where

| Concern                                             | Owner                                                          | Why                                                   |
| --------------------------------------------------- | -------------------------------------------------------------- | ----------------------------------------------------- |
| Embedding model inference                           | Upstream embedding library                                     | Pure-Rust SIMD, model weights, tokenization           |
| EmbeddingService trait                              | Upstream embedding library                                     | The contract lives with the implementer               |
| Transformer forward inference (rerank / generation) | In-process inference library                                   | Pure-Rust transformer execution                       |
| Model selection (which model is active)             | `khive-runtime` (RuntimeConfig)                                | Deployment configuration                              |
| Lazy load + LRU cache                               | `khive-runtime` (wrapping embedding primitives)                | Process-level singleton                               |
| Vector storage                                      | `khive-db` via `khive-storage::VectorStore`                    | Persistence layer                                     |
| Multi-vector aggregation                            | Backend implementation (khive-db or an external engine)        | Backend's responsibility, exposed via `backend_hints` |
| Cross-encoder reranking                             | `khive-runtime` calling the inference implementation (ADR-042) | Small-model reranking                                 |
| Retrieval composition (RRF, hybrid)                 | `khive-runtime` (ADR-012)                                      | Pure math + fusion                                    |
| Aggregation taxonomy                                | NOT standardized                                               | Open extension point per backend                      |

## Rationale

### Why direct dependency (not HTTP service)?

An HTTP embedding service (OpenAI API, Cohere API, a local Triton server) adds a network
hop, a service dependency, deployment complexity, and a failure mode that doesn't exist
in-process. For a research KG that runs on a laptop or in a container, this is pure
overhead. Pure-Rust in-process embedding is the right default.

A remote embedding provider can implement the same `EmbeddingService` trait with a
network-backed implementation. The architecture stays unchanged; only provider selection
changes.

### Why no khive-side EmbeddingService trait?

A wrapper trait would translate the upstream `EmbeddingService` to a khive-specific trait
with identical signatures. Every upstream method would require a wrapper update, and every
wrapper type change would become a khive breaking change. The duplicate trait would add no
capability.

khive depends on the implementation as a normal crate. The dependency arrow is explicit,
and upstream compatibility is handled through Cargo versioning.

### Why multi-vector at the capability layer (not a separate trait)?

Multi-vector is a record-shape concern, not a capability concern. A vector store is a
store of vectors: whether each record carries one vector or many is a property of the
record, not of the storage capability. Adding a `MultiVectorStore` trait would force
backends to implement two parallel traits for the same underlying capability, with
near-identical methods that differ only in cardinality.

The trait signature uses `Vec<Vec<f32>>` for the value. Single-vector records use a
single-element outer vec: this is `O(1)` overhead. Multi-vector records use N elements.
Backends that only support single-vector reject multi-vector inserts with
`StorageError::Unsupported`. Backends that support multi-vector handle both.

### Why no closed aggregation taxonomy?

Aggregation methods for multi-vector retrieval are an open research area. ColBERT introduced
MaxSim. ColBERTv2 added PLAID quantization. ColPali extended to visual tokens. Late
interaction with learned aggregations, section-aware pooling, and hybrid token-sentence
methods all exist. khive should not commit to any single engine's taxonomy at the trait
level: that locks every backend into one vocabulary.

`backend_hints: Option<serde_json::Value>` is the open extension point. Each backend
documents what hints it accepts. An HNSW+ColBERT backend may document `{"scoring":
"max_sim"|...}`, while a section-aware backend documents its own knobs. Future
backends document theirs.

The cost is type-level looseness: hints are JSON, not strongly-typed enums. This is
acceptable because the consumer (a specific backend) validates them at the boundary.
The benefit is openness: khive's own methods are first-class, not deviations from
some engine's standard.

### Why in-process inference by default?

Pure-Rust, in-process, no API keys, no rate limits, no network. Laptop-grade deployment
is khive's default. A remote API is another implementation of the same trait, so adding
one does not change the khive architecture.

### Why lazy load (not eager load at startup)?

Idle deployments are common: a khive instance may run for hours without an embedding call.
Loading 80MB of model weights at startup for a session that never embeds is wasted memory
and wasted startup time. Lazy load pays the cost only when needed, and only once.

The first-call latency cost (cold start) is acceptable because it happens at most once
per process and is bounded by model weight size (small for MiniLM, larger for BGE-large).

## Alternatives Considered

| Alternative                                                                         | Why rejected                                                                                                              |
| ----------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------- |
| HTTP embedding service (Triton, OpenAI, etc.)                                       | Adds network hop and service dependency. Violates zero-service deployment.                                                |
| khive-side `EmbeddingService` trait wrapping the upstream trait                     | Pure indirection. Every upstream change requires a wrapper update. No value.                                              |
| Separate `khive-embed` crate                                                        | Re-exports of the embedding library plus configuration. Adds a crate without adding value.                                |
| `MultiVectorStore` trait separate from `VectorStore`                                | Multi-vector is record shape, not capability. Two parallel traits for the same capability is wrong.                       |
| Closed aggregation taxonomy (`enum AggregationStrategy { MaxSim, AvgSim, SumMax }`) | Locks every backend into one engine's vocabulary. Excludes khive's hand-rolled methods.                                   |
| ONNX runtime                                                                        | Adds a C++ dependency, per-platform binary downloads, and the entire ONNX surface. The Pure-Rust implementation suffices. |
| Eager model load at startup                                                         | Wastes memory in idle deployments. Lazy load is correct.                                                                  |
| Single-vector only                                                                  | Forecloses multi-vector retrieval. Wrong long-term call.                                                                  |
| Per-call model override                                                             | Adds complexity for an unproven use case. Single active model per runtime is enough.                                      |

## Consequences

### Positive

- Zero-service deployment. Single binary, in-process embedding, no external dependencies.
- Direct control over model selection and execution.
- Multi-vector capability without architectural commitment to a specific aggregation taxonomy.
- khive's own retrieval methods are first-class peers of external engines', not exceptions.
- Pure-Rust binary. No ONNX, no Python, no libtorch.
- Cold-start cost paid at most once per process.
- LRU cache makes repeated queries cheap.

### Negative

- Binary size grows through in-process inference dependencies. Acceptable for a server binary.
- Model weights download on first use. Mitigated: weights cached in the standard HuggingFace cache.
- Cold start cost for the first `embed()` call. Mitigated: lazy load means idle deployments pay nothing.
- `backend_hints` is JSON, not strongly typed. Mitigated: each backend documents its own hint vocabulary.
- Only one active embedding model per runtime instance. Mitigated: future multi-model ADR can extend without breaking the trait.

### Neutral

- Cross-encoder reranking is specified separately in ADR-042.
- Remote-API embedding is supported by adding a provider implementation, not by changing the khive architecture.
- The embedder is a process-level singleton; when a process serves multiple actors, the model weights are shared across all actors. This is correct for a Rust binary: there is nothing actor-specific about the model itself.

## Implementation

- `crates/khive-runtime/src/runtime.rs`: `KhiveRuntime.embedder()`: lazy `OnceCell` initialization, returns `Arc<dyn EmbeddingService>`.
- `crates/khive-runtime/src/retrieval.rs`: `embed()`, `embed_batch()` call the registered embedding service.
- `crates/khive-runtime/Cargo.toml`: the embedding implementation is a normal dependency.
- `RuntimeConfig.embedding_model: Option<EmbeddingModel>`: model selection.
- `KHIVE_EMBEDDING_MODEL` env var: deployment override.
- No khive-side embedding crate. No wrapper trait. Direct dependency only.

## References

- ADR-005: Storage Capability Traits: `VectorStore` is multi-capable; `backend_hints` is the aggregation extension point.
- ADR-006: Deterministic Scoring: `DeterministicScore` carries scores from embedding-derived similarity into khive's fusion math.
- ADR-012: Retrieval Architecture: composes embeddings (from this ADR) with vector and text storage (ADR-005) into hybrid retrieval.
- ADR-042: Local rerank, the concrete khive-side cross-encoder call site.
- ColBERT-style multi-vector retrieval (MaxSim/AvgSim/SumMax): one aggregation family among many; not normative for khive.
