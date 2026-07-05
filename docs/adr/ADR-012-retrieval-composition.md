# ADR-012: Retrieval Composition (High-Level Composition Layer)

**Status**: accepted\
**Date**: 2026-05-23\
**Authors**: khive maintainers

> Relationship to ADR-030: ADR-012 is the legacy-but-live high-level retrieval
> composition record. It governs runtime and pack-level orchestration of storage
> capability signals, `NamespaceToken` enforcement, fusion, filtering, and alive
> checks. ADR-030 supersedes only ADR-012's original deferral of a standalone
> retrieval crate and assigns low-level engine/adapters ownership to
> `khive-retrieval`. Do not read ADR-030 as replacing ADR-012's composition
> decision while the shipped runtime still exposes and uses the ADR-012
> composition path.

## Context

khive retrieves entities, notes, and graph subsets from typed, multi-substrate storage.
A research agent might ask "find me FlashAttention" (vector + text), "what depends on
LoRA?" (graph), "decisions about quantization from last month" (note + temporal filter),
or "concepts similar to this seed, two hops out" (vector + graph). Each of these is a
different composition of storage signals.

A retrieval architecture that hard-codes "vector + text fusion" addresses one slice. A
research KG needs all of them.

The architecture must satisfy:

1. **Capability-agnostic composition.** Any retrieval-participating storage capability
   (vector, sparse, text, graph, SQL filter, entity/note CRUD) should compose into a
   retrieval pipeline. The fusion layer must not privilege one signal over others.
2. **Method neutrality.** Built-in fusion strategies (RRF, weighted) coexist with
   custom khive-native strategies (decay-weighted, salience-mixed, brain-influenced).
   The architecture must not commit to a closed taxonomy.
3. **Single-backend execution, multi-backend orchestration.** Each retrieval primitive
   talks to one backend. Cross-backend fan-out happens above the retrieval layer in
   the SubstrateCoordinator.
4. **Deterministic ranking.** Scores produced by retrieval flow through `DeterministicScore`
   (ADR-006). RRF K is 60 (the standard default).
5. **Composable, not monolithic.** Each retrieval primitive is independently callable.
   Hybrid pipelines compose primitives explicitly. No magic "one search function that
   does everything."

## Decision

### Retrieval is composition of capability signals

Retrieval in khive is the composition of one or more storage-capability signals into a
ranked result set. The live legacy composition layer remains in `khive-runtime` and
pack handlers: it enforces `NamespaceToken` boundaries, orchestrates `VectorStore`,
`TextSearch`, `GraphStore`, `EntityStore`, and `NoteStore` calls, applies fusion,
and performs alive/filter checks. Low-level retrieval engines, reusable fusion/search
traits, and storage adapters live below this layer in `khive-retrieval` per ADR-030.

```text
Storage Capabilities (ADR-005)         Composition (khive-runtime)
─────────────────────────              ──────────────────────────
VectorStore   ───┐
SparseStore   ───┤
TextSearch    ───┤
GraphStore    ───┼──→  Candidate streams ──→ Fusion ──→ Ranked hits
EntityStore   ───┤        + filtering         strategy
NoteStore     ───┤        + expansion
SqlAccess     ───┘        + reranking
```

Of the eight capability traits in ADR-005, seven participate in retrieval. `EventStore`
is the exception — it is audit/observability, not retrieval.

### Five retrieval primitives

khive-runtime exposes five primitive operations. Each calls one storage capability.
Composition builds higher-level pipelines on top.

1. **Candidate generation** — pulls candidate IDs + scores from a single signal.

   | Source     | Method                                          | Returns                                      |
   | ---------- | ----------------------------------------------- | -------------------------------------------- |
   | Vector     | `VectorStore::search`                           | Vector hits (UUID, score, optional metadata) |
   | Sparse     | `SparseStore::search`                           | Sparse hits                                  |
   | Text       | `TextSearch::search`                            | FTS hits                                     |
   | Graph      | `GraphStore::neighbors`, `GraphStore::traverse` | Neighbor / path hits                         |
   | SQL filter | `SqlAccess::query_all`                          | Structured-filter ID rows                    |
   | Note       | `NoteStore::query_notes`                        | Note IDs by kind/temporal filter             |
   | Entity     | `EntityStore::query_entities`                   | Entity IDs by kind/entity_type filter        |

2. **Filter / alive-check** — verifies candidates exist, aren't soft-deleted, match
   substrate-level filters (entity_kind, entity_type, note_kind). Uses `EntityStore`
   or `NoteStore` batch queries.

3. **Graph expansion** — given a seed set, walks N hops via `GraphStore` to expand or
   constrain the candidate set. `direction` ∈ `{In, Out, Both}`, `relations` filter
   from the 15 canonical EdgeRelation values (ADR-002).

4. **Fusion** — combines candidate streams into one ranked stream. Strategies are
   first-class objects (see below).

5. **Reranking** — optional post-fusion pass. v1 ships `rerank` (cosine reranking
   over a candidate subset). Cross-encoder rerank is deferred to a future lattice
   rerank crate (ADR-011).

### `FusionStrategy`: open enum + extension point

`FusionStrategy` carries the strategy at the call site. Built-in variants are normative;
custom strategies extend the enum without amending this ADR.

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FusionStrategy {
    /// Reciprocal Rank Fusion. K=60 by default (ADR-006).
    Rrf { k: usize },
    /// Weighted linear combination. Min-max normalizes each signal to [0, 1] first.
    /// Weights normalized to sum to 1.0; negatives clamped to 0; all-zero falls
    /// back to equal weights.
    Weighted { weights: Vec<f64> },
    /// Take all hits; keep the max score per subject.
    Union,
    /// Drop non-vector hits; return vector hits only.
    VectorOnly,
    /// Pack-defined or user-defined custom strategy, dispatched by name.
    Custom {
        name: String,
        params: serde_json::Value,
    },
}
```

`Custom` is the openness mechanism. khive's memory pack registers a `decay_weighted`
strategy that weights candidates by salience and time decay. A brain pack may register
`brain_influenced` that weights by posterior confidence. Future packs register their
own.

Custom strategies register at runtime through `KhiveRuntime::register_fusion_strategy`.
Unknown names return `FusionError::UnknownStrategy(name)`. The strategy executor receives
candidate streams and returns ranked hits — same shape as built-in strategies.

```rust
pub trait FusionExecutor: Send + Sync + 'static {
    async fn fuse(
        &self,
        streams: Vec<CandidateStream>,
        params: &serde_json::Value,
        limit: usize,
    ) -> RuntimeResult<Vec<RankedHit>>;
}
```

The trait is khive-runtime's, not a storage trait. Packs may register executors at
runtime via the same composition that registers verb handlers.

### NamespaceToken at all composition entry points

All retrieval composition entry points take `&NamespaceToken`. The namespace is derived
from the token; raw namespace strings are not trusted at the composition boundary. Sealed
`NamespaceScope` is passed down to `ScopedBackendRoute` per ADR-029.

```rust
pub async fn hybrid_search(
    &self,
    token: &NamespaceToken,   // replaces namespace: Option<&str>
    query_text: &str,
    query_vector: Option<Vec<f32>>,
    limit: u32,
    entity_kind: Option<&str>,
) -> RuntimeResult<Vec<SearchHit>>;

pub async fn hybrid_search_with_strategy(
    &self,
    token: &NamespaceToken,   // replaces namespace: Option<&str>
    query_text: &str,
    query_vector: Option<Vec<f32>>,
    strategy: FusionStrategy,
    limit: u32,
) -> RuntimeResult<Vec<SearchHit>>;
```

### Retrieval layering

This ADR is the **high-level composition layer** above `khive-retrieval` (ADR-030). The
canonical pipeline flows:

```text
request
  → ADR-012 composition          (this ADR — orchestration + NamespaceToken enforcement)
  → ADR-031 candidate generation (multi-engine policy)
  → ADR-030 retrieval engines    (khive-retrieval crate — HNSW, BM25, hybrid, fusion primitives)
  → ADR-042 reranking            (reranker traits + local rerank config)
  → final result assembly
```

- **ADR-012**: orchestrates retrieval at the runtime level. Owns NamespaceToken enforcement
  and high-level composition. Does NOT own HNSW/BM25 implementation details.
- **ADR-030**: owns `khive-retrieval` crate — engines + low-level fusion primitives.
- **ADR-031**: candidate-set policy across embedding engines (weighted engine-level RRF runs
  here, INSIDE the pack/backend runtime).
- **ADR-042**: reranker traits, local rerank config, lattice-inference integration.

### `hybrid_search` is one composition, not "the" composition

`hybrid_search` is the canonical entry point for the common case: text + vector + RRF.
It is NOT the architectural ceiling.

Other compositions exist as peers (all take `&NamespaceToken` as the first argument):

- `knn(token, query_vector, top_k)` — exact KNN over a namespace
- `rerank(token, query_vector, candidate_ids, top_k)` — exact rerank of a set
- `vector_search(token, query_text|embedding, top_k, kind)` — vector-only
- Graph composition: `neighbors`, `traverse`, `bfs_traverse`, `shortest_path`
- Note retrieval: `search_notes` (kind-aware, supersession-aware per ADR-013)
- Memory pack `recall` (decay-weighted hybrid, registered via Custom strategy)

Each composition is a top-level method on `KhiveRuntime`. There is no single
`KhiveRuntime::retrieve(everything)` god-method.

### Graph is a retrieval primitive

Graph traversal is not "a separate feature." It is a retrieval signal. `GraphStore::neighbors`
returns candidate IDs reachable in one hop; `GraphStore::traverse` returns paths from a
seed set up to `max_depth`.

These compose with vector/text fusion:

```text
1. Vector + text fusion → candidate entities (top-K)
2. Graph expansion → 1-hop neighbors of each candidate (collect IDs)
3. Union with original candidates
4. Re-rerank against query
5. Return top-K
```

This is "graph-aware retrieval." It is a custom composition pattern, not a built-in
method. v1 ships the primitives; packs and callers compose them as needed.

### Filter pushdown via SqlAccess

Structured filters (e.g., "concepts with `entity_type = 'algorithm'` created after
2026-01-01") use `SqlAccess` for filter pushdown. The query produces a candidate ID
set that downstream fusion can intersect with vector/text candidates.

```text
SQL filter → ID set S
Vector hits → ID set V
Result: V ∩ S, ordered by V's rank
```

This is the same composition pattern as graph expansion — one capability produces
candidates, another constrains.

### Alive-check is mandatory after fusion

Fused candidates must be alive-checked before return. Soft-deleted entities (deleted_at
IS NOT NULL) and superseded notes must not appear in retrieval results. The alive-check
is a single batch query against `EntityStore` or `NoteStore` after fusion:

```rust
// After fusion produces candidate IDs (namespace derived from NamespaceToken)
let alive_set = entities.query_entities(
    token.namespace(),
    EntityFilter { ids: candidate_ids, ..Default::default() },
    PageRequest { offset: 0, limit: candidates.len() as u32 },
).await?;
fused.retain(|h| alive_set.contains(&h.entity_id));
```

This is the right layer for the check. Storage filters by `deleted_at IS NULL`; the
runtime composes the check after fusion so the candidate pool stays large enough that
filtering doesn't deplete top-K.

### Cross-substrate retrieval

Notes and entities are different substrates (ADR-004). Retrieval may return mixed
results when the caller opts in:

```rust
pub async fn search_mixed(
    &self,
    token: &NamespaceToken,   // replaces namespace: Option<&str>
    query: &str,
    kinds: &[SubstrateKind],  // [Entity], [Note], [Entity, Note]
    limit: u32,
) -> RuntimeResult<Vec<MixedHit>>;
```

`MixedHit` carries the substrate discriminant. Fusion across substrates uses the same
`FusionStrategy` — the substrate doesn't change the math. The pack that owns each kind
documents how cross-substrate ranking behaves (entities and notes use comparable
DeterministicScore values per ADR-006).

### Index vs strategy: orthogonal layers

Vector indexes (Flat, HNSW, IVF, IVF-PQ, ScaNN, DiskANN, GNN-based learned indexes) are
**backend implementations of `VectorStore`** — they live below the retrieval composition
layer. Retrieval strategies (RRF, Weighted, Custom, decay-weighted, etc.) are **fusion
methods over candidate streams** — they live in `khive-runtime` and consume hits
indifferent to which index produced them.

```text
Index choice (backend)              Strategy choice (call-site)
──────────────────────              ──────────────────────────
Flat / brute-force        ┐         Rrf { k }
HNSW                      │         Weighted { weights }
IVF-PQ                    ├─→  hits ─→  Union
ScaNN / DiskANN           │         VectorOnly
GNN-based learned         ┘         Custom { name, params }
                                    pack-registered strategies
```

Adding HNSW to a backend does not change retrieval composition. Switching from RRF to
a decay-weighted strategy does not change the backend. The two axes evolve independently.

Index-specific tuning (HNSW's `ef_search`, IVF's `nprobe`, GNN walk depth, multi-vector
aggregation method) flows through `VectorSearchRequest.backend_hints` (ADR-005), not
through `FusionStrategy`.

**GNN-based retrieval** is the interesting edge case. Three interpretations, all
expressible through existing layers:

1. **GNN as embedding source** — produces graph-context-aware vectors. Consumed via
   `VectorStore` like any other embedding. lattice could add a GNN-embed variant.
2. **GNN-as-retriever** — graph walks + learned scoring. This is a custom retrieval
   strategy (`FusionStrategy::Custom { name: "gnn_walk", params }`) composing
   `GraphStore` + `VectorStore` signals with learned weights.
3. **Hybrid graph-vector index** (SPANN-like) — the backend fuses graph structure +
   vectors internally. Still a `VectorStore` implementation; the fusion happens
   inside the backend, exposed via `backend_hints` for tuning.

None of these requires a new trait. The capability/strategy split absorbs all three.

### Cross-backend fan-out delegated to SubstrateCoordinator

Retrieval primitives talk to one backend. Cross-backend fan-out (e.g., search runs on
both `main` and `lore` SQLite files) is the SubstrateCoordinator's job (ADR-003,
ADR-080 (planned)). The coordinator:

1. Identifies which backends hold candidate data for the query.
2. Dispatches the retrieval primitive to each backend.
3. Merges per-backend ranked streams via the configured fusion strategy.

`khive-runtime`'s retrieval methods receive backend-resolved arguments — they do not
contain backend-selection logic. This keeps each method single-backend by construction.

### Scope (v1)

Current composition is layered: `khive-runtime` still provides FTS5 + vector RRF paths
over storage; `khive-retrieval` and `khive-fusion` ship reusable hybrid/fusion primitives;
`khive-bm25`, `khive-hnsw`, and `khive-vamana` ship lexical/ANN engines used by pack-specific
retrieval paths.

**What v1 ships:**

| Capability                                                                | Where it lives                                                 |
| ------------------------------------------------------------------------- | -------------------------------------------------------------- |
| RRF fusion (k configurable, default 60)                                   | `khive_score::rrf_score`, `khive-runtime::retrieval::rrf_fuse` |
| Weighted linear fusion (min-max normalized)                               | `khive-runtime::fusion::fuse_with_strategy`                    |
| `FusionStrategy` enum: `Rrf`, `Weighted`, `Union`, `VectorOnly`, `Custom` | `khive-runtime::fusion`                                        |
| Custom strategy registration                                              | `khive-runtime::FusionRegistry`                                |
| Hybrid search composition (vector + text)                                 | `khive-runtime::retrieval::hybrid_search`                      |
| Strategy-parameterized hybrid search                                      | `khive-runtime::fusion::hybrid_search_with_strategy`           |
| Graph BFS (depth-bounded, direction + relation filters)                   | `khive-runtime::graph_traversal::bfs_traverse`                 |
| Bidirectional shortest-path                                               | `khive-runtime::graph_traversal::shortest_path`                |
| Exact KNN                                                                 | `khive-runtime::retrieval::knn`                                |
| Candidate-set rerank                                                      | `khive-runtime::retrieval::rerank`                             |
| Cross-substrate search                                                    | `khive-runtime::retrieval::search_mixed`                       |
| Alive-check after fusion                                                  | `khive-runtime::retrieval` (mandatory in every fusion path)    |

**What v1 deliberately defers:**

| Deferred                                          | Reason                                                                   |
| ------------------------------------------------- | ------------------------------------------------------------------------ |
| Custom HNSW vector index                          | **Superseded**: ADR-030 ports HNSW into `khive-retrieval`. Not deferred. |
| Custom BM25 keyword index                         | **Superseded**: ADR-030 ports BM25 alongside HNSW. Not deferred.         |
| Cross-encoder reranking                           | **Superseded**: ADR-042 introduces the composable rerank pipeline.       |
| Learned per-feature fusion weights                | Requires telemetry capturing query→click data.                           |
| Multi-modal retrieval (image, audio)              | Requires lattice support for non-text embedding.                         |
| Query routing policy (auto-select retrieval path) | Callers specify intent via `FusionStrategy` or explicit primitive calls. |
| Standalone `khive-retrieval` crate                | **Superseded by ADR-030** — see "Superseded stance" below.               |

Re-evaluate each remaining deferral when a concrete user case justifies the cost.

> **Superseded stance on `khive-retrieval`**: This ADR previously stated that no separate
> `khive-retrieval` crate would exist in v1. ADR-030 supersedes that decision and introduces
> `khive-retrieval` as the owning crate for retrieval primitives (HNSW, BM25, hybrid search,
> low-level fusion, storage adapters). ADR-012 is rewritten as the **high-level composition
> layer** above `khive-retrieval`.

## Rationale

### Why composition (not one search function)?

A single "do everything" `search()` function would need to express vector mode, text
mode, hybrid mode, graph mode, cross-substrate mode, strategy selection, filter
pushdown, expansion depth — all as parameters. The signature would balloon, and most
calls would specify a small subset of parameters.

Composable primitives let each call site request exactly what it needs. `hybrid_search`
exists because the vector+text+RRF combination is common enough to deserve a named
entry point. Graph expansion, custom strategies, and cross-substrate retrieval are
their own methods.

### Why `Custom` in `FusionStrategy`?

The built-in fusion variants (RRF, Weighted, Union, VectorOnly) cover the common
cases. They do not cover khive's own recall methods (decay-weighted, salience-mixed,
brain-influenced) or future learned strategies. Without a `Custom` variant, packs
would either fork `FusionStrategy` or bypass it entirely — both bad.

`Custom { name, params }` is the open extension point. The name is the strategy
identifier registered with `FusionRegistry`; `params` is opaque JSON consumed by the
executor. The shape mirrors ADR-005's `backend_hints` philosophy: standard surface,
open content.

### Why graph as a retrieval primitive (not a separate feature)?

Graph traversal IS retrieval. A research agent asking "what does FlashAttention depend
on?" is retrieving nodes — just via a different signal than vector similarity. Treating
graph as separate from search forces callers to learn two APIs (`search` and `traverse`)
when they should be peer primitives composable in the same fusion pipeline.

The benefit: graph-aware retrieval (vector candidates + 1-hop neighbors + re-rerank)
becomes a natural composition, not a special-case feature.

### Why alive-check after fusion?

If the alive-check happens before fusion, the candidate pool may be depleted before
ranking — top-K from a 200-candidate pool that's been pre-filtered to 50 candidates
returns lower-quality results. Filtering after fusion preserves the candidate pool
through ranking.

The cost is one extra batch query per search. Acceptable for the quality gain.

### Why cross-backend fan-out lives in the SubstrateCoordinator?

Retrieval primitives target one backend. The SubstrateCoordinator is the layer that
knows the multi-backend topology (ADR-003, ADR-009). Putting fan-out inside retrieval
methods would couple them to topology — every retrieval method would need to know
about backends. Delegating to the coordinator keeps retrieval clean: it operates on
one backend, the coordinator handles the multi-backend case.

### Why no standalone khive-retrieval crate? (original reasoning — superseded)

**Superseded by ADR-030.** The original reasoning held at ~1,500 LOC. ADR-030 ports
~29K LOC of verified retrieval code (HNSW, BM25, hybrid search, formal Lean4 proofs)
from an earlier internal implementation into a dedicated `khive-retrieval` crate. At that scale the crate
split is not premature — it isolates a distinct algorithmic concern with its own proof
tree, benchmark suite, and dependency surface (`lattice-embed`). ADR-012 is now the
**composition layer above** `khive-retrieval`, not the owner of retrieval primitives.

## Alternatives Considered

| Alternative                                                    | Why rejected                                                            |
| -------------------------------------------------------------- | ----------------------------------------------------------------------- |
| Single `search()` god-method                                   | Signature bloat; most parameters unused per call.                       |
| Closed `FusionStrategy` enum (no `Custom`)                     | Forces packs to fork or bypass. Method neutrality lost.                 |
| Graph traversal in a separate module from retrieval            | Implies graph isn't retrieval. It is.                                   |
| Alive-check before fusion                                      | Depletes candidate pool; degrades top-K quality.                        |
| Cross-backend fan-out in retrieval methods                     | Couples retrieval to topology. SubstrateCoordinator is the right layer. |
| Standalone `khive-retrieval` crate (original deferral)         | **Superseded by ADR-030**: 29K LOC + proof tree justifies the split.    |
| Built-in routing policy that auto-selects retrieval primitives | Speculative. Callers know what they want.                               |
| Query IR distinct from `khive-request` and `khive-query`       | Premature abstraction. The verb-dispatch DSL covers structured calls.   |

## Consequences

### Positive

- Any storage capability composes into retrieval.
- Built-in strategies cover the common case; `Custom` covers everything else.
- khive's own recall methods are first-class peers (memory pack's decay-weighted
  recall, brain pack's posterior-weighted recall, future learned strategies).
- Graph traversal is a retrieval primitive, callable in the same fusion pipeline as
  vector and text.
- Cross-backend fan-out stays in the SubstrateCoordinator; retrieval primitives stay
  single-backend.
- Alive-check ensures soft-deleted records never surface.
- Surface stays small enough that a contributor can read the retrieval code in an
  afternoon.

### Negative

- `Custom` strategies are opaque to the type system — the executor validates `params`
  at runtime, not at compile time.
  Mitigated: each pack documents its own strategy's parameter shape.
- Multiple retrieval methods (no single entry point) means callers must learn the
  taxonomy.
  Mitigated: `hybrid_search` is the named common case; primitives are documented.
- Alive-check adds one extra batch query per fused search.
  Mitigated: it's a single batch query, not a per-candidate roundtrip.
- Cross-backend fan-out is deferred to the SubstrateCoordinator implementation.
  Mitigated: single-backend deployments see zero overhead; ADR-080 (planned) owns the
  fan-out spec.

### Neutral

- RRF K = 60 (ADR-006). No additional decisions here.
- `DeterministicScore` flows through retrieval unchanged.
- Graph expansion depth limit (MAX_TRAVERSAL_DEPTH = 10) is shared with `khive-query`
  (ADR-008).

## Implementation

- `crates/khive-runtime/src/retrieval.rs`: live high-level composition entry points
  such as `hybrid_search`, `vector_search`, `knn`, `rerank`, `search_mixed`, and
  embedding helpers.
- `crates/khive-runtime/src/fusion.rs`: runtime strategy entry points such as
  `FusionStrategy`, `fuse_with_strategy`, and `hybrid_search_with_strategy`.
- `crates/khive-retrieval/src/lib.rs` and `crates/khive-retrieval/src/hybrid/searcher.rs`:
  low-level retrieval/ranking primitives, fusion helpers, engines, and adapters
  owned by ADR-030 and used where wired.
- `crates/khive-runtime/src/graph_traversal.rs`: `bfs_traverse`, `shortest_path`,
  `neighbors`, `traverse`.
- `crates/khive-runtime/src/registry.rs`: `FusionRegistry` for custom strategy
  registration.
- `crates/khive-score/src/ops.rs`: `rrf_score`, `weighted_sum`, `Ranked<T>` — fusion
  math primitives.

## References

- ADR-003: System Architecture — SubstrateCoordinator owns cross-backend fan-out.
- ADR-005: Storage Capability Traits — the eight traits retrieval composes from.
- ADR-006: Deterministic Scoring — `DeterministicScore`, RRF K=60.
- ADR-008: Query Layer Separation — graph query language (`GQL`/`SPARQL`) is a separate
  surface; retrieval composes raw storage capabilities, not query strings.
- ADR-011: Embedding and Inference — produces the vectors retrieval consumes.
- ADR-013 (planned rewrite): Note Kind Taxonomy — supersession filter for note retrieval.
- ADR-080 (planned): Substrate Coordinator — cross-backend fan-out spec.
