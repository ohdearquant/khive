# ADR-012: Retrieval Architecture — Inference in Lattice, Storage + Fusion in khive

**Status**: accepted\
**Date**: 2026-05-15\
**Authors**: Ocean, lambda:khive

## Context

A research KG needs retrieval — semantic search over content, hybrid search combining vector
similarity with keyword matching, and optionally reranking via cross-encoders or LLM scorers.

The temptation is to build a unified `khive-retrieval` crate that owns the whole stack: embedding
generation, vector indexing, BM25, fusion, reranking — a single ~30K LOC monolith.

But that bundling collapses two distinct concerns:

1. **Inference** — turning text into vectors, scoring (query, candidate) pairs. Needs models, GPU,
   batching, hot-loaded weights.
2. **Storage + Composition** — storing vectors, retrieving k-nearest, fusing result lists. Needs
   SQL, in-memory math, deterministic ordering.

Putting both in one crate forces every consumer to pull in inference dependencies even when they
only need composition. It also blurs the line between what's khive's job vs. what's `lattice`'s job
(the inference engine).

## Decision

**Split the retrieval stack along the inference/storage boundary.**

| Capability                              | Owner                                    | Why                                               |
| --------------------------------------- | ---------------------------------------- | ------------------------------------------------- |
| Store vectors, retrieve k-nearest       | **khive** (`khive-db` via `VectorStore`) | sqlite-vec / pgvector. Pure storage.              |
| Store text, search by keyword           | **khive** (`khive-db` via `TextSearch`)  | FTS5 trigram. Pure storage.                       |
| Embedding generation (text → vector)    | **lattice-embed** (Rust crate dep)       | Inference. Pure-Rust on-device, SIMD-accelerated. |
| Cross-encoder reranking                 | **lattice-embed** (when added)           | Inference. Same reason.                           |
| RRF fusion of result lists              | **khive** (`khive-score`)                | Pure math, ~50 LOC. Already implemented.          |
| Weighted score fusion                   | **khive** (`khive-score`)                | Pure math. Already implemented.                   |
| Heuristic reranking (recency, salience) | **khive** (`khive-runtime`)              | No inference. Score arithmetic.                   |
| Hybrid search composition               | **khive** (`khive-runtime`)              | Composition of VectorStore + TextSearch + fusion. |
| Natural language → query string         | **lattice-embed** (when used)            | Inference.                                        |
| Query parser (SPARQL/GQL → AST)         | **khive** (`khive-query`)                | Pure parsing. Designed in ADR-008 (v0.2 phase 2). |

khive-runtime depends on **`lattice-embed`** (public crate, on crates.io) as a normal Rust
dependency. No HTTP, no separate service — embedding runs in-process via SIMD-accelerated pure-Rust
inference. Loaded lazily on first use.

## Rationale

### Why split along inference/storage

1. **Crate-level decoupling**. `lattice-embed` is a focused Rust crate (pure-Rust BGE/E5/Qwen
   inference with SIMD). khive's storage crates depend on no inference machinery. Pulling
   lattice-embed only in `khive-runtime` keeps the storage core free of ML deps.

2. **Optional inference**. Even with lattice-embed linked, embedding only runs when
   `RuntimeConfig.embedding_model` is set. Without it: keyword search via FTS5, structured queries
   via SPARQL/GQL, graph traversal all still work. Vector and hybrid search return `Unconfigured`.
   This matches the "local research KG" use case where users may not need semantic search.

3. **Independent versioning**. lattice-embed evolves with model releases. khive's data model changes
   slowly. They version independently via cargo.

4. **No language SDK drift** (per ADR-011). The Deno surface speaks MCP. A research agent calling
   khive doesn't know or care that the embedding is happening in lattice-embed inside `khive-mcp` —
   it's just another MCP tool.

### Why not a `khive-retrieval` crate

The composition logic (RRF, weighted fusion, hybrid search) is **~250 LOC total**. Most of it (RRF,
weighted_sum) is already in `khive-score`. The remaining ~150 LOC for `hybrid_search` operation goes
into `khive-runtime`.

Adding a new crate for 250 LOC is dependency bloat with no payoff:

- One more `Cargo.toml` to maintain.
- One more entry in the workspace.
- No clear boundary — `khive-vector` would either depend on `khive-score` (and become a thin
  wrapper) or duplicate the math.

Better: keep retrieval logic in the existing crates where it naturally fits.

### Default model: all-MiniLM-L6-v2

`RuntimeConfig::default()` selects `EmbeddingModel::AllMiniLmL6V2` (384 dims,
sentence-transformers/all-MiniLM-L6-v2) when `KHIVE_EMBEDDING_MODEL` is unset. Rationale:

- **Small footprint** — ~80 MB weights, fast cold start, runs comfortably on a laptop.
- **Established baseline** — the canonical sentence-transformers model with widespread benchmark
  coverage.
- **384 dimensions** — matches BGE-small for storage compatibility; lets users swap to BGE-small
  without re-indexing dimensionality.

Override with `KHIVE_EMBEDDING_MODEL=bge-base-en-v1.5` (or any value lattice-embed's `FromStr`
accepts: `small`, `bge-large`, `multilingual-e5-base`, `qwen3-embedding-0.6b`, etc.) when you need
different quality/multilingual/size trade-offs.

### How lattice integration works

khive-runtime exposes:

```rust
pub async fn embed(&self, text: &str) -> RuntimeResult<Vec<f32>>;
pub async fn hybrid_search(
    &self,
    namespace: Option<&str>,
    query_text: &str,
    query_vector: Option<Vec<f32>>,   // pre-computed, or None to fall back to text-only
    limit: u32,
) -> RuntimeResult<Vec<SearchHit>>;
```

`embed()` calls `lattice-embed`'s `NativeEmbeddingService::embed_one` directly — in-process,
SIMD-accelerated, pure Rust. The model is loaded lazily on first call. `hybrid_search()` uses the
vector if provided, otherwise falls back to text-only search.

The Deno server / CLI / external agents typically:

1. Call `embed(text)` to get a query vector.
2. Call `hybrid_search(text, Some(vector))` to get fused results.

Or skip step 1 entirely and just use text search via `hybrid_search(text, None)`.

### What about cross-encoder reranking?

Same pattern: when reranking is needed, khive-runtime will depend on the relevant lattice crate (or
a future lattice-rerank crate) and expose a `rerank(query, candidates)` operation. Stays as an
additive change.

## Alternatives Considered

| Alternative                                        | Pros                     | Cons                                                                      | Why rejected                          |
| -------------------------------------------------- | ------------------------ | ------------------------------------------------------------------------- | ------------------------------------- |
| New `khive-retrieval` crate                        | Self-contained           | 250 LOC doesn't justify a crate                                           | Premature splitting                   |
| Treat lattice as an HTTP service                   | Loose coupling, polyglot | lattice is published as a Rust library, not a service                     | Wrong abstraction for what lattice is |
| Everything in lattice (storage too)                | Single dep               | Conflates inference and storage                                           | Wrong abstraction                     |
| Optional `embedding` feature flag in khive-runtime | Smaller default binary   | Two build modes to test, embedding is a core capability for a research KG | Embedding belongs in default builds   |

## Consequences

### Positive

- khive-runtime ships embedding out of the box — no setup, no HTTP wiring.
- lattice-embed evolves on its own cargo cadence; semver protects us.
- Pure-Rust in-process means no IPC overhead, no separate process to manage.
- All four retrieval signals (text, vector, fusion, ranking) work without any external service.
- Easy local development: `cargo run -p khive-mcp` and embeddings just work.

### Negative

- Binary size grows by ~10-15 MB (lattice-inference deps). Acceptable for a server binary.
- Model weights download on first use. Mitigated: weights are cached in the standard HuggingFace
  cache.
- Cold start cost for the first `embed()` call. Mitigated: model load is lazy; idle deployments pay
  nothing.

### Neutral

- Embedding cache (avoid re-embedding the same query) lives in lattice-embed already
  (`CachedEmbeddingService`). We can wrap it later if needed.
- Remote embedding models (e.g., OpenAI's `text-embedding-3-small`) are listed in
  `lattice_embed::EmbeddingModel` but not wired through `NativeEmbeddingService` yet. When wired, no
  API change to `embed()` — same call site.

## Implementation

### Already in place

- `khive-score::ops::rrf_score(rank, k) -> DeterministicScore` ✓
- `khive-score::ops::weighted_sum(scores, weights) -> DeterministicScore` ✓
- `khive-storage::VectorStore` + `khive-db` SQLite impl ✓
- `khive-storage::TextSearch` + `khive-db` FTS5 impl ✓

### Shipped in `khive-runtime`

```rust
// src/retrieval.rs
pub async fn embed(&self, text: &str) -> RuntimeResult<Vec<f32>>;

pub async fn hybrid_search(
    &self,
    namespace: Option<&str>,
    query_text: &str,
    query_vector: Option<Vec<f32>>,
    limit: u32,
) -> RuntimeResult<Vec<SearchHit>>;
```

`SearchHit` and `SearchSource` are public in `khive_runtime`:

```rust
pub struct SearchHit {
    pub entity_id: Uuid,
    pub score: DeterministicScore,
    pub source: SearchSource,  // Vector | Text | Both
    pub title: Option<String>,
    pub snippet: Option<String>,
}

pub enum SearchSource { Vector, Text, Both }
```

Configuration:

```rust
pub struct RuntimeConfig {
    pub db_path: Option<PathBuf>,
    pub default_namespace: String,
    /// Local embedding model. Defaults to `AllMiniLmL6V2` if unset.
    /// Override with env var `KHIVE_EMBEDDING_MODEL`.
    pub embedding_model: Option<lattice_embed::EmbeddingModel>,
}
```

The vector store's per-model table name is derived from `EmbeddingModel::to_string()`, sanitized to
alphanumeric+underscore. Dimensions come from `EmbeddingModel::dimensions()`.

### Library-only, not MCP-exposed

`embed` and `hybrid_search` are runtime library functions. They are NOT exposed as MCP tools. The
MCP surface stays focused on agent-callable KG operations; agents use `search(kind="entity"|"note")`
per ADR-023 to get hybrid retrieval, which composes `embed` + vector store + text store internally.
Embedding generation is a runtime concern, not a tool agents call directly.

When no `embedding_model` is configured, hybrid retrieval falls back to text-only (FTS5). The
default config sets `AllMiniLmL6V2`, so out-of-the-box deployments have semantic search immediately.

## References

- ADR-005: Storage Capability Traits — defines `VectorStore` and `TextSearch`
- ADR-011: Deno + MCP-Only — agents reach retrieval through the MCP `search` verb, not a separate
  `embed` tool
- ADR-023: Verb-Consolidated MCP Surface — `search(kind=...)` is the agent-facing retrieval verb
- `khive-score::ops` — RRF + weighted fusion already implemented
- `lattice-embed` (public crate on crates.io): the inference library this ADR coordinates with
