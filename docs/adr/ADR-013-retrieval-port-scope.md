# ADR-013: Retrieval Scope for v0.1 — What's In, What's Deferred

**Status**: accepted\
**Date**: 2026-05-15\
**Authors**: Ocean, lambda:khive

## Context

A research knowledge graph could in principle ship any number of retrieval features: custom vector
indexes, custom keyword indexes, eval harnesses, cross-encoder reranking, query routing policy,
replay infrastructure for offline benchmarking, learned fusion weights, and so on. Each adds
maintenance load, dependency weight, and surface area for users to learn.

For v0.1 we need a coherent minimal surface that a researcher can run on a laptop, integrate into
agent workflows, and extend later. This ADR sets the cut line.

ADR-012 sets the retrieval architecture (lattice-embed as a library dependency, vector + text
storage via sqlite-vec + FTS5, fusion math in `khive-runtime`). This ADR is the companion: what we
_do_ and _don't_ ship in v0.1.

## Decision

Ship a thin, opinionated retrieval surface. Default to existing platform primitives (sqlite-vec,
FTS5) for the index layer. Keep custom math (RRF, weighted fusion, graph traversal) in pure Rust
inside `khive-runtime`. Defer everything else to future versions, gated on real user demand.

## What v0.1 ships

| Capability                                                                        | Where it lives                                                  |
| --------------------------------------------------------------------------------- | --------------------------------------------------------------- |
| RRF fusion (Reciprocal Rank Fusion, k configurable)                               | `khive_score::rrf_score` + `khive-runtime::retrieval::rrf_fuse` |
| Weighted linear fusion (with min-max score normalization)                         | `khive-runtime::fusion::fuse_with_strategy`                     |
| `FusionStrategy` enum: `Rrf { k }`, `Weighted { weights }`, `Union`, `VectorOnly` | `khive-runtime::fusion::FusionStrategy`                         |
| Hybrid search composition over text + optional vector                             | `khive-runtime::retrieval::hybrid_search`                       |
| Strategy-parameterized hybrid search                                              | `khive-runtime::fusion::hybrid_search_with_strategy`            |
| Graph BFS (depth-bounded, direction + relation filters)                           | `khive-runtime::graph_traversal::bfs_traverse`                  |
| Bidirectional shortest-path                                                       | `khive-runtime::graph_traversal::shortest_path`                 |
| Local embedding generation (BGE / E5 / MiniLM / Qwen3-Embedding)                  | `khive-runtime::retrieval::embed` (via `lattice-embed`)         |
| Batched embedding                                                                 | `khive-runtime::retrieval::embed_batch`                         |
| LRU embedding cache                                                               | wrapped automatically inside `KhiveRuntime::embedder`           |
| MCP tools surface                                                                 | `khive-mcp` (14 verb-consolidated tools per ADR-023 + ADR-024)  |

**Total**: ~1,500 LOC of retrieval logic across `khive-score`, `khive-runtime`, and `khive-mcp`.

## What v0.1 deliberately does NOT ship

| Deferred capability                                        | Reason                                                                                                                                                                                 |
| ---------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Custom HNSW vector index                                   | `sqlite-vec` (the v0.1 vector backend) handles ANN well for v0.1 scale (<10M vectors). Custom HNSW only pays off at scale or with non-cosine distance functions — not the v0.1 target. |
| Custom BM25 keyword index                                  | FTS5 with the trigram tokenizer (CJK-safe) covers keyword retrieval. Custom BM25 would expose k1/b tuning that v0.1 users don't need yet.                                              |
| Eval harness (LLM-judged relevance + benchmark runner)     | Heavy: depends on LLM availability, benchmark corpora, and metric definitions. Belongs in a separate `khive-eval` crate when we have published benchmarks to track.                    |
| Query replay engine                                        | Tightly coupled to the eval harness. Lands when eval lands.                                                                                                                            |
| Learned per-feature fusion weights                         | Requires telemetry capturing query→click data. Premature without instrumentation.                                                                                                      |
| Custom index persistence layer                             | Only relevant if we ship a custom HNSW or BM25. We delegate to SQLite which persists itself.                                                                                           |
| Cross-encoder reranking                                    | Requires a published rerank crate from the inference side. Trait will land then; existing fusion already provides a reasonable ceiling.                                                |
| Query intermediate representation distinct from the parser | `khive-query` (SPARQL/GQL → SQL) is sufficient. A separate IR is premature abstraction.                                                                                                |
| Routing policy (auto-selects which retrieval path to run)  | Callers specify intent via `FusionStrategy` or by omitting `query_vector`. Auto-routing is premature.                                                                                  |
| Aggregate retrieval `SearchConfig` struct                  | `RuntimeConfig` + per-call parameters suffice.                                                                                                                                         |
| Operation-level timeout helpers                            | Caller's responsibility (`tokio::time::timeout` / `select!`).                                                                                                                          |
| Observability sink trait (custom MetricsSink)              | `tracing` is already in the crate. Callers integrate via standard subscribers.                                                                                                         |

## Rationale

### Why a small surface

1. **Maintenance is a tax**. Every shipped module needs ongoing patching when dependencies move and
   when Rust evolves. Code we don't yet need is code we don't yet have to maintain.
2. **YAGNI works**. We can add any deferred capability when a real user request lands. The hard part
   is getting v0.1 in front of users; nothing here blocks any future feature.
3. **Coherence over completeness**. A small surface that does few things well beats a large surface
   that obscures the contract. Users should be able to read `khive-runtime` in an afternoon.
4. **Index portability**. Default to platform primitives (sqlite-vec, FTS5) and any user who wants a
   different backend can swap behind the trait surfaces in `khive-storage`.

### When to revisit each deferral

Re-evaluate when one of these triggers fires:

- **Custom HNSW**: KGs hit 10M+ vectors AND sqlite-vec recall@10 drops below 0.95 on a
  representative workload.
- **Custom BM25**: users report FTS5 ranking quality issues on specific corpora (technical text,
  code).
- **Eval harness**: a published benchmark (BEIR, MTEB, RAGAS) emerges as a target we want to track.
- **Replay**: the eval harness ships.
- **Weight learning**: telemetry capturing query→click is in place.
- **Cross-encoder rerank**: a published rerank crate emerges on the inference side.
- **Query IR / routing policy / search config / timeouts / metrics sink**: a concrete user case
  justifies the abstraction.

If none of these fire within a year of v0.1, the deferrals were correct calls.

## Consequences

### Positive

- The OSS retrieval surface stays auditable. A new contributor can read every retrieval file in one
  sitting.
- Binary size stays small (sqlite-vec + lattice-embed are the heavy deps).
- Zero lock-in to internal abstractions — anyone can fork and swap out the storage layer behind the
  trait surfaces.
- Crystal-clear "what we ship vs. what's possible" — no surprise capability gaps when users ask
  about features.

### Negative

- No drop-in eval harness — users who want quality metrics roll their own initially.
- No cross-encoder rerank — top-k from RRF or weighted fusion is the ceiling until rerank lands.
- We share sqlite-vec's performance ceiling. For typical research-KG sizes (<1M vectors) this is
  fine; at scale we'll need to revisit.

### Neutral

- Each deferred capability is independent — adding any one later does not require a rewrite.

## Open questions

1. Should the retrieval module eventually graduate into a standalone `khive-retrieval` crate?
   Currently it lives in `khive-runtime`. Argument for splitting: cleaner namespace for users
   importing just retrieval. Argument against: ~1,500 LOC doesn't justify a crate split yet.
2. The graph traversal lives in `khive-runtime::graph_traversal`. If we ever want a pure-storage
   graph traversal (no runtime composition), it could move down to `khive-storage`. Defer.

## References

- ADR-005: Storage Capability Traits — defines `VectorStore` and `TextSearch`
- ADR-011: Deno + MCP-Only — lattice integration via library dep
- ADR-012: Retrieval Architecture — inference in `lattice-embed`, storage + fusion in khive
