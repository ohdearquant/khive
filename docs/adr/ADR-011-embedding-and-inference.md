# ADR-011: Embedding and Inference Architecture

**Status**: accepted\
**Date**: 2026-05-23\
**Authors**: Ocean, lambda:khive

## Context

khive turns text into vectors for retrieval, scoring, and recall. Two upstream concerns
sit between agent input and the vector storage layer:

1. **Embedding generation** — converting text to one or more dense vectors via a model.
2. **Inference composition** — model loading, caching, batching, and (future) cross-encoder
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

### Direct lattice-embed dependency

`khive-runtime` depends on `lattice-embed` as a normal Rust crate. Embedding runs
in-process through `lattice-embed::NativeEmbeddingService`. There is no embedding
service trait inside khive — `khive-runtime` calls `lattice-embed` directly.

```rust
// In khive-runtime
pub struct KhiveRuntime {
    backend: Arc<StorageBackend>,
    config: RuntimeConfig,
    embedder: Arc<OnceCell<Arc<dyn lattice_embed::EmbeddingService>>>,
    ...
}

impl KhiveRuntime {
    pub async fn embed(&self, text: &str) -> RuntimeResult<Vec<f32>> { ... }
    pub async fn embed_batch(&self, texts: &[String]) -> RuntimeResult<Vec<Vec<f32>>> { ... }
}
```

Lattice is khive's inference engine. They are designed to evolve together. The boundary
is a normal cargo dependency, not a service contract.

### `lattice-embed::EmbeddingService` is the contract

`lattice-embed` defines its own `EmbeddingService` trait. khive-runtime treats this as
the embedding contract — no khive-side trait wraps it.

```rust
// From lattice-embed
#[async_trait]
pub trait EmbeddingService: Send + Sync {
    async fn embed(&self, texts: &[String], model: EmbeddingModel)
        -> Result<Vec<Vec<f32>>>;
    async fn embed_one(&self, text: &str, model: EmbeddingModel)
        -> Result<Vec<f32>>;
    fn model_config(&self, model: EmbeddingModel) -> ModelConfig;
}
```

When lattice extends this trait (token-level outputs, image embedding, multi-vector
outputs), khive picks up the new methods automatically. No translation layer to maintain.

### Model selection

The active model is configured via `RuntimeConfig.embedding_model: Option<EmbeddingModel>`,
overridable via `KHIVE_EMBEDDING_MODEL` environment variable. The default is
`AllMiniLmL6V2` (384d, ~80MB weights).

```rust
pub struct RuntimeConfig {
    pub embedding_model: Option<lattice_embed::EmbeddingModel>,
    ...
}
```

`None` disables embedding. Vector and hybrid search return `Unconfigured("embedding_model")`.
Text-only search and graph traversal still work. This is the "KG without semantic search"
deployment.

Model overrides at the call site are not currently supported — the runtime is configured
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

Cold start cost is paid once per process lifetime. Idle deployments pay nothing —
weights are not loaded until needed.

### Multi-vector capability at the storage boundary

A subject may carry one or many vectors. ADR-005's `VectorStore::insert` accepts
`Vec<Vec<f32>>` — one vector is the common case, many vectors is the late-interaction
or multi-modal case.

The embedding pipeline produces whatever shape the consumer needs:

```rust
// Single sentence embedding (the v1 default)
let vec: Vec<f32> = runtime.embed(text).await?;
storage.insert(ns, id, "body", vec![vec]).await?;

// Per-section embedding (future, when lattice exposes section tokenization)
let vecs: Vec<Vec<f32>> = runtime.embed_sections(document).await?;
storage.insert(ns, id, "body", vecs).await?;

// Per-token embedding (future, when lattice exposes last-hidden-state output)
let vecs: Vec<Vec<f32>> = runtime.embed_tokens(text).await?;
storage.insert(ns, id, "body", vecs).await?;
```

The trait is open. Today's khive ships single-vector embedding by default; the
multi-vector path requires (a) lattice-embed exposing token/section outputs, and
(b) a backend that stores and searches multi-vector records.

### Method neutrality

khive does not standardize multi-vector aggregation. ADR-005's `VectorSearchRequest`
carries an opaque `backend_hints: Option<serde_json::Value>` field. Each backend
defines its own hint vocabulary:

```text
ruvector-core HNSW + ColBERT mode:
  backend_hints = {"scoring": "max_sim" | "avg_sim" | "sum_max"}

khive-db single-vector mode:
  backend_hints = null  (cosine/dot, no aggregation needed)

khive hand-rolled recall:
  backend_hints = {"decay_lambda": 0.05, "salience_weight": 0.7, ...}

future:
  backend_hints = whatever the backend documents
```

ColBERT-style late interaction is one option among many. khive's hand-rolled methods
(decay-weighted aggregation, salience mixing, learned weights) are first-class peers,
not exceptions to a "standard" taxonomy. The architecture is open by design.

### Cross-encoder reranking

Cross-encoder rerank (query, candidate → score) routes through `lattice-inference`
running a small reasoning model (Qwen3-class, ADR-009 in lattice). See ADR-042 for
the concrete call site, latency budget, and brain-resolved adapter wiring. This ADR
fixes the contract: `khive-runtime` calls `lattice-inference` directly — same pattern
as embedding. No HTTP, no service abstraction.

Until ADR-042 lands the rerank path, hybrid retrieval ceilings are determined by
bi-encoder fusion (RRF over text + vector). For most khive workloads (research KG,
~10K-1M records), this is the right tradeoff.

### Adapter-aware inference (forward-pass hook)

`lattice-inference` exposes the `LoraHook` trait (lattice ADR-008,
`lattice-inference::lora_hook::LoraHook`) as the forward-pass adapter injection
point. The trait is `Send + Sync` and gives the inference loop a per-layer
per-module hook `apply(layer_idx, module: &str, x: &[f32], output: &mut [f32])`
that adds `scale·B@(A@x)` in-place after the base projection. A
`NoopLoraHook` with `#[inline(always)]` empty body achieves zero overhead when
no adapter is bound.

**The embedding model is NOT a LoRA-adapter consumer.** Embeddings define the
corpus's vector geometry; LoRA-adapting the embedding model would silently misalign
already-stored vectors against newly-produced ones. Embedding-model changes go
through re-indexing (ADR-019 backfill pipeline in lattice), not through online
adaptation. `EmbeddingService::embed` deliberately does not accept a hook
parameter — and adding one is not on the roadmap.

khive consumes `LoraHook` at the **derivable-model** call sites — small LLMs running
via `lattice-inference` whose outputs feed the harness pipeline:

- **Rerank** (ADR-042): the rerank model's forward pass takes `Option<&dyn LoraHook>`.
  Brain (ADR-032) resolves a profile per call; if the resolved profile is
  LoRA-class (`ProfileStateClass::LoraAdapter`), its state implements `LoraHook`
  (via `lattice-tune::LoraAdapter`'s impl, gated behind the
  `lattice-tune/inference-hook` feature flag — ADR-031 in lattice) and is
  passed to the rerank forward. Otherwise `None` and the rerank runs unadapted.
- **Query paraphraser** (future ADR): same shape as rerank — small LLM forward
  pass with optional LoRA hook from a brain-resolved profile. Adapts to
  deployment-specific phrasing patterns.
- **Synthesizer / consolidator** (future ADR): same shape — small LLM that merges
  multiple memory hits into a single summary; LoRA-adapted toward "good synthesis"
  per deployment.

All three are harness adaptations — they tune how khive _uses_ lattice models, not
how the models themselves think. Each call site validates that the resolved
profile's `target_model_id` matches the active lattice model id; mismatches drop the
hook (ADR-032 §5b, ADR-033 §8.2).

The string-keyed module names (`"q_proj"`, `"k_proj"`, …) are forward-compatible
with new architectures lattice adds — see ADR-008 in lattice for the rationale
and the risk of silent no-ops if module names drift (mitigated by the versioned
module-name registry — see ADR-032 §6).

### What lives where

| Concern                                             | Owner                                                            | Why                                                                                                                                 |
| --------------------------------------------------- | ---------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------- |
| Embedding model inference                           | `lattice-embed`                                                  | Pure-Rust SIMD, model weights, tokenization                                                                                         |
| EmbeddingService trait                              | `lattice-embed`                                                  | The contract lives with the implementer                                                                                             |
| Transformer forward inference (rerank / generation) | `lattice-inference`                                              | Pure-Rust transformer (lattice ADR-001), GQA/RoPE/KV cache/paged-KV/continuous-batching                                             |
| Forward-pass adapter injection                      | `lattice-inference::lora_hook::LoraHook`                         | Per-layer per-module hook (lattice ADR-008); consumed by brain LoRA-class profiles (ADR-032 §5b) and the rerank call site (ADR-042) |
| LoRA adapter format + load + apply                  | `lattice-tune::lora` (lattice ADR-031, feature `inference-hook`) | PEFT/MLX SafeTensors I/O; A/B matrix storage as `Vec<f32>`; `LoraAdapter: LoraHook` bridge                                          |
| Adapter lifecycle (live / shadow / rollback)        | `lattice-tune::registry` (lattice ADR-029)                       | RegisteredModel + ModelStatus state machine (Pending→Production→Archived)                                                           |
| Model selection (which model is active)             | `khive-runtime` (RuntimeConfig)                                  | Deployment configuration                                                                                                            |
| Lazy load + LRU cache                               | `khive-runtime` (wrapping lattice-embed primitives)              | Process-level singleton                                                                                                             |
| Vector storage                                      | `khive-db` via `khive-storage::VectorStore`                      | Persistence layer                                                                                                                   |
| Multi-vector aggregation                            | Backend implementation (khive-db or ruvector-core)               | Backend's responsibility, exposed via `backend_hints`                                                                               |
| Cross-encoder reranking                             | `khive-runtime` calling `lattice-inference` (ADR-042)            | Small-model rerank with brain-resolved LoRA hook                                                                                    |
| Retrieval composition (RRF, hybrid)                 | `khive-runtime` (ADR-012)                                        | Pure math + fusion                                                                                                                  |
| Aggregation taxonomy                                | NOT standardized                                                 | Open extension point per backend                                                                                                    |

## Rationale

### Why direct dependency (not HTTP service)?

An HTTP embedding service (OpenAI API, Cohere API, a local Triton server) adds a network
hop, a service dependency, deployment complexity, and a failure mode that doesn't exist
in-process. For a research KG that runs on a laptop or in a container, this is pure
overhead. Pure-Rust in-process embedding is the right default.

If a deployment wants OpenAI-quality embeddings, it ships lattice-embed's remote model
support (when added) — still through the same `EmbeddingService` trait, just with a
network-backed implementation. The architecture stays unchanged; only the model selection
changes.

### Why no khive-side EmbeddingService trait?

A wrapper trait would translate `lattice-embed::EmbeddingService` to `khive::EmbeddingService`
with identical signatures. Every new lattice method requires a wrapper update. Every wrapper
type change is a breaking change. The wrapper provides no value — lattice's trait is already
the right abstraction.

khive depends on lattice as a normal crate. When lattice's trait changes, khive picks up
the change. The dependency arrow is explicit; the boundary is honest.

### Why multi-vector at the capability layer (not a separate trait)?

Multi-vector is a record-shape concern, not a capability concern. A vector store is a
store of vectors — whether each record carries one vector or many is a property of the
record, not of the storage capability. Adding a `MultiVectorStore` trait would force
backends to implement two parallel traits for the same underlying capability, with
near-identical methods that differ only in cardinality.

The trait signature uses `Vec<Vec<f32>>` for the value. Single-vector records use a
single-element outer vec — this is `O(1)` overhead. Multi-vector records use N elements.
Backends that only support single-vector reject multi-vector inserts with
`StorageError::Unsupported`. Backends that support multi-vector handle both.

### Why no closed aggregation taxonomy?

Aggregation methods for multi-vector retrieval are an open research area. ColBERT introduced
MaxSim. ColBERTv2 added PLAID quantization. ColPali extended to visual tokens. Late
interaction with learned aggregations, decay-weighted aggregations, and hybrid token-sentence
methods all exist. khive should not commit to ruvector's specific taxonomy at the trait
level — that locks every backend into ruvector's vocabulary.

`backend_hints: Option<serde_json::Value>` is the open extension point. Each backend
documents what hints it accepts. ruvector-core's HNSW+ColBERT documents `{"scoring":
"max_sim"|...}`. khive's hand-rolled recall documents its own knobs. Future
backends document theirs.

The cost is type-level looseness — hints are JSON, not strongly-typed enums. This is
acceptable because the consumer (a specific backend) validates them at the boundary.
The benefit is openness: khive's own methods are first-class, not "deviations from
ruvector's standard."

### Why lattice (not OpenAI / Cohere / Voyage / etc)?

Pure-Rust, in-process, no API keys, no rate limits, no network. Laptop-grade deployment
is khive's default; remote-API deployment is a degenerate case of the same trait. lattice
gives us deterministic behavior and full control over model selection. We can add a
remote-API model to lattice (calling OpenAI behind the `EmbeddingService` trait) without
changing the khive architecture.

### Why lazy load (not eager load at startup)?

Idle deployments are common — a khive instance may run for hours without an embedding call.
Loading 80MB of model weights at startup for a session that never embeds is wasted memory
and wasted startup time. Lazy load pays the cost only when needed, and only once.

The first-call latency cost (cold start) is acceptable because it happens at most once
per process and is bounded by model weight size (small for MiniLM, larger for BGE-large).

## Alternatives Considered

| Alternative                                                                         | Why rejected                                                                                                            |
| ----------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------- |
| HTTP embedding service (Triton, OpenAI, etc.)                                       | Adds network hop and service dependency. Violates zero-service deployment.                                              |
| khive-side `EmbeddingService` trait wrapping lattice                                | Pure indirection. Every lattice change requires a wrapper update. No value.                                             |
| Separate `khive-embed` crate                                                        | Re-exports of lattice-embed plus configuration. Adds a crate without adding value.                                      |
| `MultiVectorStore` trait separate from `VectorStore`                                | Multi-vector is record shape, not capability. Two parallel traits for the same capability is wrong.                     |
| Closed aggregation taxonomy (`enum AggregationStrategy { MaxSim, AvgSim, SumMax }`) | Locks every backend into ruvector's vocabulary. Excludes khive's hand-rolled methods.                                   |
| ONNX runtime                                                                        | Adds a C++ dependency, per-platform binary downloads, and the entire ONNX surface. lattice is pure Rust and sufficient. |
| Eager model load at startup                                                         | Wastes memory in idle deployments. Lazy load is correct.                                                                |
| Single-vector only                                                                  | Forecloses multi-vector retrieval. Wrong long-term call.                                                                |
| Per-call model override                                                             | Adds complexity for an unproven use case. Single active model per runtime is enough.                                    |

## Consequences

### Positive

- Zero-service deployment. Single binary, in-process embedding, no external dependencies.
- Direct control over models. lattice's roadmap is khive's roadmap.
- Multi-vector capability without architectural commitment to a specific aggregation taxonomy.
- khive's own retrieval methods are first-class peers of ruvector's, not exceptions.
- Pure-Rust binary. No ONNX, no Python, no libtorch.
- Cold-start cost paid at most once per process.
- LRU cache makes repeated queries cheap.

### Negative

- Binary size grows by ~10-15 MB (lattice-embed inference deps). Acceptable for a server binary.
- Model weights download on first use. Mitigated: weights cached in the standard HuggingFace cache.
- Cold start cost for the first `embed()` call. Mitigated: lazy load means idle deployments pay nothing.
- `backend_hints` is JSON, not strongly typed. Mitigated: each backend documents its own hint vocabulary.
- Only one active embedding model per runtime instance. Mitigated: future multi-model ADR can extend without breaking the trait.

### Neutral

- Cross-encoder reranking is deferred until lattice ships a rerank crate.
- Remote-API embedding (OpenAI, Cohere) is supported by adding a model variant to lattice-embed, not by changing the khive architecture.
- The embedder is a process-level singleton; when a process serves multiple actors, the model weights are shared across all actors. This is correct for a Rust binary — there is nothing actor-specific about the model itself.

## Implementation

- `crates/khive-runtime/src/runtime.rs`: `KhiveRuntime.embedder()` — lazy `OnceCell` initialization, returns `Arc<dyn EmbeddingService>`.
- `crates/khive-runtime/src/retrieval.rs`: `embed()`, `embed_batch()` — direct calls to lattice-embed.
- `crates/khive-runtime/Cargo.toml`: `lattice-embed = "..."` as a normal dependency.
- `RuntimeConfig.embedding_model: Option<EmbeddingModel>`: model selection.
- `KHIVE_EMBEDDING_MODEL` env var: deployment override.
- No khive-side embedding crate. No wrapper trait. Direct dependency only.

## References

- ADR-005: Storage Capability Traits — `VectorStore` is multi-capable; `backend_hints` is the aggregation extension point.
- ADR-006: Deterministic Scoring — `DeterministicScore` carries scores from embedding-derived similarity into khive's fusion math.
- ADR-012: Retrieval Architecture — composes embeddings (from this ADR) with vector and text storage (ADR-005) into hybrid retrieval.
- `lattice-embed` (crates.io): pure-Rust embedding inference; the upstream this ADR depends on.
- `lattice-inference` (crates.io): pure-Rust transformer inference (Qwen3, BERT, GQA/RoPE/KV cache/paged-KV/continuous-batching); `LoraHook` trait lives here (lattice ADR-008).
- `lattice-tune` (crates.io): adapter training + `LoraAdapter` + `ModelRegistry` lifecycle (lattice ADR-029, ADR-031). Gated behind `inference-hook` feature for the `LoraAdapter: LoraHook` bridge.
- ADR-032: Brain Profile Orchestration — LoRA-class profile state plugs into `LoraHook`.
- ADR-042: Local re-rank via lattice-inference — concrete khive-side call site for `lattice-inference` and the brain-resolved adapter hook.
- `ruvector-core::advanced_features::multi_vector`: reference implementation of one multi-vector retrieval method (ColBERT MaxSim/AvgSim/SumMax). One option among many; not normative for khive.
