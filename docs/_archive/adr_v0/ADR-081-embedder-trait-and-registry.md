# ADR-081: `Embedder` Trait and `EmbedderRegistry`

**Status**: proposed\
**Date**: 2026-05-22\
**Authors**: Ocean, lambda:khive\
**Depends on**: ADR-058 (Fold Cognitive Primitives)\
**Part of**: ADR-078 (Multi-Engine Embedding umbrella)

## Context

ADR-078's umbrella establishes that khive needs to restore the multi-engine embedding shape
the khive-internal predecessor had — N peer embedding services running concurrently, each
producing its own vector index, with results fused via weighted RRF. The umbrella also
establishes that embedding generation is the **caller's** responsibility (not the runtime's),
which means the embedding services and their orchestration need a clear home.

This ADR locks the foundational types: the **`Embedder` trait** that any embedding provider
implements, and the **`EmbedderRegistry`** that holds N configured engines and exposes
filtered views to consumers. ADR-082 owns the TOML config schema that the registry is built
from; ADR-083 owns the runtime API change that consumes pre-computed vectors; ADR-084 owns
the pack-level fan-out pattern that uses the registry's `embed_*_all` methods.

## Decision

### The `Embedder` trait — provider-agnostic embedding contract

```rust
// khive-embed/src/trait.rs
#[async_trait]
pub trait Embedder: Send + Sync {
    /// Canonical engine identifier (e.g., "bge-small-en-v1.5"). Stable string;
    /// used as vector table suffix and as the cache-key component.
    fn model_id(&self) -> &str;

    /// Output vector dimension. Must equal config.dim and the dimension of every
    /// vector returned by embed().
    fn dim(&self) -> usize;

    /// Embed a batch of texts. Returns one Vec<f32> per input, in input order.
    async fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError>;

    /// Asymmetric retrieval — prefix added to query text before embedding.
    /// E5 uses "query: ", Qwen3 uses an instruction prompt. Default None.
    fn query_prefix(&self) -> Option<&'static str> { None }

    /// Asymmetric retrieval — prefix added to document text at storage time.
    fn document_prefix(&self) -> Option<&'static str> { None }
}
```

Three properties this trait carries:

1. **One model per instance.** An `Embedder` is pinned to a single (model_id, dim). N peer
   engines = N `Embedder` instances. This matches khive-internal's
   `NativeEmbeddingService::with_model(model)` pattern.
2. **Provider-agnostic.** `lattice-embed` is one implementation; `OpenAiEmbedder`,
   `CohereEmbedder`, custom providers all implement the same trait without modifying khive's
   runtime or storage.
3. **Asymmetric retrieval built in.** E5 / Qwen3 prefixes are first-class via
   `query_prefix()` / `document_prefix()`. This prevents the bug class documented in
   `khive-internal/deploy/engine.toml` where omitted prefixes caused cosines to cluster at
   0.93–0.95 (the original Chinese-blindspot crisis).

### The `EmbedderRegistry` — process-wide engine container with filtered views

```rust
// khive-embed/src/registry.rs
pub struct EmbedderRegistry { /* internal: Vec<RegisteredEngine> */ }

struct RegisteredEngine {
    config: EngineConfig,         // see ADR-082
    service: Arc<dyn Embedder>,
}

impl EmbedderRegistry {
    pub fn from_config(configs: Vec<EngineConfig>) -> Result<Self, EmbedError>;

    /// All registered engines in declaration order. First entry is "primary"
    /// (used for single-model operations like reranker dispatch).
    pub fn engines(&self) -> &[RegisteredEngine];

    /// Embed query side — applies query_prefix per engine if defined.
    pub async fn embed_query_all(&self, text: &str)
        -> Result<Vec<(EngineConfig, Vec<f32>)>, EmbedError>;

    /// Embed document side — applies document_prefix per engine if defined.
    pub async fn embed_document_all(&self, text: &str)
        -> Result<Vec<(EngineConfig, Vec<f32>)>, EmbedError>;

    /// Embed with a specific engine by model_id. For single-engine paths.
    pub async fn embed_one(&self, model_id: &str, text: &str)
        -> Result<Vec<f32>, EmbedError>;

    /// Look up engine config by model_id (for table routing).
    pub fn get(&self, model_id: &str) -> Option<&EngineConfig>;

    /// Return a new registry exposing ONLY engines whose model_id is in `allow`.
    /// Engine Arcs are SHARED with self — the filter is a view, not a clone.
    pub fn filter(self: &Arc<Self>, allow: &[String]) -> Arc<EmbedderRegistry>;
}
```

### D8 — Engine registry is process-wide and shared across packs

The kernel boot path (`kkernel`, per ADR-076) constructs `EmbedderRegistry` **once** from the
`[[engines]]` array in `khive.toml` (per ADR-082). Engines are heavy: a BGE checkpoint is
~150 MB on disk; the loaded model + KV cache + LRU buffer is ~600 MB resident. Loading the
same model twice (once for kg, once for memory) doubles the cost for no benefit.

**The pattern**: one `Arc<EmbedderRegistry>` at the kkernel layer. Each pack declares the
engines it uses (per ADR-079: `engines = ["bge-small-en-v1.5", "multilingual-e5-small"]`).
At pack construction kkernel calls `registry.filter(&pack_cfg.engines)` and passes the
filtered `Arc<EmbedderRegistry>` to the pack's `KhiveRuntime::from_backend(backend,
filtered_engines)`.

Three properties this gives:

- **Memory locality.** BGE loaded once across all consuming packs.
- **Cache locality.** A query against the kg pack warms the BGE LRU cache; a subsequent
  query against the memory pack benefits from that warmup.
- **Pack autonomy preserved.** Each pack sees only its declared engines via the filtered
  view; a pack declaring `engines = []` cannot invoke an unconfigured engine.

`filter()` returns a new `Arc<EmbedderRegistry>` whose internal `Vec<RegisteredEngine>` is a
subset of the parent's. The `Arc<dyn Embedder>` per entry is **the same Arc** as in the
parent. The underlying engine instance is identical; only the surface is filtered. This is
the khive-internal `resolve_embed_models()` pattern (process-wide registry, per-service-
family selection) re-applied to packs.

### Asymmetric retrieval applied at registry boundary

`embed_query_all` and `embed_document_all` are the two main parallel-fan-out methods. For
each engine, before calling `embed()`, the registry composes the input text with the engine's
`query_prefix()` or `document_prefix()` (when defined). This prevents the asymmetric-prefix
bug class — the registry is the right place to apply prefixes because it owns the per-engine
metadata, while individual callers usually don't know whether they're querying or storing.

Callers may still bypass the prefix path via `embed_one` for one-off uses; that method does
**not** apply prefixes. The bypass exists for special cases (testing, embedding a single
caption with no asymmetry); the asymmetric path is the default for retrieval-shaped traffic.

### LatticeEmbedder reference adapter (feature `lattice`)

The first concrete `Embedder` implementation wraps `lattice-embed`'s `CachedEmbeddingService`:

```rust
// khive-embed/src/lattice.rs   (feature "lattice", default-on)
pub struct LatticeEmbedder {
    model: lattice_embed::EmbeddingModel,
    service: Arc<lattice_embed::CachedEmbeddingService>,
    dim: usize,
}

impl LatticeEmbedder {
    pub fn new(model: lattice_embed::EmbeddingModel, output_dim: Option<usize>) -> Self;
}

#[async_trait]
impl Embedder for LatticeEmbedder { /* delegates to CachedEmbeddingService */ }
```

Future providers (`OpenAiEmbedder`, `CohereEmbedder`, custom) implement `Embedder` without
depending on `lattice-embed`. The `lattice` feature is `default = ["lattice"]` in
`khive-embed/Cargo.toml`; consumers who want lattice-free builds (e.g., a future
remote-API-only deployment) can disable the feature.

## Layering

| Concern                                     | Crate                             | Why                                                 |
| ------------------------------------------- | --------------------------------- | --------------------------------------------------- |
| `Embedder` trait                            | `khive-embed`                     | Provider-agnostic                                   |
| `EmbedderRegistry`                          | `khive-embed`                     | Co-located with the trait it manages                |
| `LatticeEmbedder` adapter                   | `khive-embed` (feature `lattice`) | One implementation among future providers           |
| Engine configuration types (`EngineConfig`) | `khive-embed`                     | Per ADR-082; types live with the registry           |
| Per-(model, dim) vector tables              | `khive-db` (already exists)       | Storage-layer concern via `vec_model_key`           |
| Multi-engine fan-out + fusion               | Pack handlers (per ADR-084)       | Orchestration sits where verb semantics are defined |

The `khive-embed` crate lives in the platform layer (per ADR-078 umbrella's resolved Open
Question 1).

## Alternatives Considered

### A. Registry inside `khive-runtime`

Runtime exposes `embedder()` and `embed_all()` directly. Rejected: runtime gains
`lattice-embed` dependency; runtime can't be used with external embedders without unloading
the registry; the abstraction inversion that prompted this ADR returns.

### B. Single multi-model service via `EmbeddingService::embed(texts, model)` trait

lattice-embed's trait already accepts a `model` parameter per call; in principle one service
could dispatch to multiple loaded models. Rejected: in practice each `NativeEmbeddingService`
is pinned to one model; multi-model-per-service would require lattice-embed restructure;
provider diversity (OpenAI, Cohere) cannot live behind one trait object due to orthogonal
config. Per-engine `Embedder` instances are the simpler unit.

### C. Separate `khive-embed-lattice` crate for the adapter

Foundation-layer `khive-embed` with no lattice dependency; adapter in a sibling crate.
Considered for cleaner layering. Resolved by ADR-078 Open Question 1: platform layer for v1
(one crate with feature flag); refactor to sibling crate if a non-lattice provider ships.

## Consequences

### Positive

- Provider-agnostic — OpenAI, Cohere, custom implementations slot in without runtime change
- Single source for multi-engine semantics (one trait, one registry)
- Engine instances loaded once, shared across packs via Arc + filter (D8)
- Asymmetric retrieval handled at the right layer — registry knows per-engine prefix
- Pack autonomy: filtered view per pack; no pack accidentally invokes engines it didn't
  declare

### Negative

- `khive-embed` adds a new crate to the workspace — one more `Cargo.toml`, one more
  publish step
- The "primary engine = `engines()[0]`" convention is implicit; callers must agree

### Neutral

- `lattice-embed` adapter is feature-gated `default = ["lattice"]` — common deployments get
  it automatically; opt-out for specialized builds
- `EmbedderRegistry::filter` shares Arcs — no double-loading; the filtered view is the
  source of truth for the consuming pack

## Open Questions

1. **Primary-engine convention.** `engines()[0]` is conventionally the "primary" for
   single-model operations (rerank, CLI `khive kg embed`). This is currently a comment in the
   trait docs. Should it be a named field on the registry instead? Default v1: keep as
   convention; promote if confusion arises.
2. **Embedding cache key invariance.** The cache key must include `(model_id, output_dim,
   query_prefix)` to avoid one engine's vector served for another's query. Currently
   lattice-embed's blake3 key includes `model_config.to_embedding_key().canonical_bytes()`
   which already satisfies this. Future Embedder impls must respect the same key
   composition; should be documented as part of the trait contract. Defer to non-lattice
   adapter PR.

## References

- ADR-058 — Fold Cognitive Primitives
- ADR-076 — kkernel/MCP split (registry constructed in kkernel)
- ADR-078 — Multi-engine embedding umbrella
- ADR-079 — Pack-scoped backends (`engines = [...]` per pack drives `filter()`)
- ADR-082 — Engine configuration schema (TOML the registry parses)
- ADR-083 — Runtime API change (consumers of pre-computed vectors)
- ADR-084 — Pack multi-engine orchestration (consumer of `embed_query_all`)
- khive-internal `foundation/embed/DESIGN.md` — invariants INV-1..INV-8
- khive-internal `foundation/embed/src/model.rs` — `EmbeddingModel`, `ModelConfig`, asymmetric
  retrieval prefixes
- khive-internal `foundation/embed/src/service/native.rs` — `NativeEmbeddingService::with_model`
  pattern
