# ADR-030: Retrieval Stack Port — khive-retrieval

**Status**: accepted\
**Date**: 2026-05-23\
**Authors**: khive maintainers

## Context

ADR-012 established retrieval as composition of storage-capability signals inside
`khive-runtime`. That ADR explicitly deferred a standalone `khive-retrieval` crate
("~1,500 LOC doesn't justify a crate split") and deferred HNSW ("sqlite-vec handles
ANN well for v1 scale"). Both deferrals were correct at the time.

Two conditions have changed:

1. **A mature retrieval stack was already implemented** (internal path `platform/retrieval/`):
   ~29K LOC, 5000+ LOC of tests, HNSW with INT8 two-phase quantized search,
   BM25 keyword index, RRF/weighted/union fusion, checkpointing, persistence,
   graph-aware retrieval, deterministic i64 fixed-point scoring throughout (ADR-006),
   and 146 Lean4 theorems covering HNSW level distribution and complexity, BM25
   non-negativity and monotonicity, RRF deterministic ordering, distance metric
   properties, quantization error bounds, and skip-condition soundness.

2. **`lattice-embed` provides the SIMD and quantization layer** (ADR-011): AVX-512F,
   AVX-512 VNNI, AVX2+FMA, ARM NEON with multi-accumulator, and scalar fallback; f32 /
   Int8 / Int4 / Binary quantization tiering; age-based Hot/Warm/Cold policy. The
   internal stack's own SIMD kernels are a subset of this coverage.

sqlite-vec (currently behind `VectorStore` in `khive-db`) is brute-force, non-scaling,
and has no quantization. It was always a stopgap. Replacing it with the ported HNSW
closes the performance and scalability gap without a change to the VectorStore trait
(ADR-005).

**RuVector** (MIT, ruv is collaborator) provides supplementary research-grade techniques
not present in the internal stack: ColBERT multi-vector late interaction, Matryoshka
adaptive-dimension search, conformal prediction, spectral coherence. These are real
and useful. The previous direction of adopting RuVector as the primary backend was
based on an incomplete view of what the internal stack already delivers. RuVector is
f32-only, platform-bound in determinism, and ships no formal proofs. It is supplementary,
not foundational.

## Decision

Port the internal retrieval stack (`platform/retrieval/`) into the workspace as `khive-retrieval`.
Use `lattice-embed` (ADR-011) as the SIMD and quantization foundation. The retrieval
crates ship, but `khive-db` still contains sqlite-vec VectorStore compatibility in current
code; sqlite-vec retirement remains a separate implementation issue. Deliver in-process
retrieval techniques as opt-in adapter paths.

### Crate ownership and boundaries

**Ownership**: ADR-030 owns the `khive-retrieval` crate, including:

- Retrieval engines (HNSW, BM25, hybrid)
- Low-level fusion primitives (RRF at the engine-pair level)
- Storage adapters for vector + lexical backends (`StorageVectorSearch`,
  `StorageKeywordSearch`)

**Out of scope**: reranker traits and rerank-stage configuration are owned by ADR-042.
ADR-030 does NOT define `Reranker` traits or rerank weights — those belong in ADR-042.

ADR-012 is the high-level composition layer above this crate. ADR-031 wraps multiple
`HnswIndex` instances for multi-engine candidate-set policy. ADR-042 owns the rerank
stage that consumes ranked candidates produced here.

### Crate layout after the port

```text
khive/crates/
  khive-storage        — VectorStore trait contract (ADR-005, unchanged)
  khive-fold           — Fold / Objective / Anchor / Selector (ADR-024, unchanged)
  khive-score          — DeterministicScore, RRF math (ADR-006, unchanged)
  khive-db             — SQLite backend; still includes sqlite-vec VectorStore compatibility
  khive-retrieval      — retrieval primitives ported from the internal platform/retrieval/
    hnsw/              — HNSW index, INT8 two-phase quantized search
    bm25/              — BM25 keyword index
    fusion/            — RRF / Weighted / Union / VectorOnly strategies
    hybrid/            — combined dense + keyword search
    graph/             — relationship-aware retrieval
    query_ir/          — query intermediate representation
    adapters/          — StorageVectorSearch, StorageKeywordSearch
    persist/           — index persistence and snapshot restore
    hnsw/checkpoint/   — HNSW checkpoint protocol

lattice/crates/embed/  — SIMD kernels, quantization tiering (ADR-011, unchanged)

khive/proofs/
  Retrieval/           — HNSW.lean, BM25.lean, RRF.lean, RRFAnalysis.lean,
                         QuantizationBounds.lean, SkipCondition.lean,
                         Distance.lean, Cosine.lean, Graph.lean,
                         RetrievalAlgorithms.lean
  Scoring/             — Score.lean and determinism proofs
  README.md            — theorem-to-module index
```

### Dependency rewiring

The internal crate's dependencies map to workspace equivalents as follows:

| Internal crate            | khive equivalent                            |
| ------------------------- | ------------------------------------------- |
| `foundation/score`        | `khive-score`                               |
| `foundation/embed`        | `lattice-embed` (the strategic switch)      |
| `foundation/types`        | `khive-types`                               |
| `foundation/fold`         | `khive-fold`                                |
| `platform/db`             | `khive-db`                                  |
| `platform/storage-traits` | `khive-storage`                             |
| `platform/policy`         | `khive-gate` + `khive-gate-rego`            |
| `foundation/inference`    | `lattice-inference` (optional feature flag) |

The HNSW INT8 quantization arena delegates distance kernels to `lattice-embed::simd`
rather than maintaining its own kernels. This gives HNSW access to AVX-512 VNNI and
multi-accumulator NEON paths that the internal kernels did not have.

### Public surface

Pack handlers (memory, kg, future packs) consume `khive-retrieval` through this surface:

```rust
pub use khive_retrieval::{
    SearchConfig,                              // per-call hybrid search config
    FusionStrategy,                            // Rrf, Weighted, VectorOnly, KeywordOnly, Union
    HnswIndex, HnswConfig, HnswCheckpoint,     // vector index
    Bm25Index, Bm25Config,                     // keyword index
    HybridSearcher,                            // dense + keyword + fusion
    StorageVectorSearch, StorageKeywordSearch,  // VectorStore / TextSearch adapters
};
```

`StorageVectorSearch` adapts an ADR-005 `VectorStore` into the retrieval
`VectorSearch` trait, and `StorageKeywordSearch` adapts an ADR-005 `TextSearch`
into the retrieval `KeywordSearch` trait. `khive-runtime` still has FTS5 + VectorStore RRF paths over storage. The retrieval port
crates ship alongside that path; replacing or retiring sqlite-vec remains explicit follow-up
work, not completed current behavior.

`HybridSearcher` provides the common dense+keyword+fusion entry point. ADR-031
(multi-engine composition) wraps multiple `HnswIndex` instances behind the same surface.
ADR-032 (brain profile orchestration) consumes ranked candidates from `HybridSearcher`
and applies fold-based reranking (ADR-024).

### sqlite-vec retirement (deferred)

`khive-db` still contains the sqlite-vec `VectorStore` compatibility path in current
shipped code. The retrieval crates (`khive-retrieval`, `khive-hnsw`, `khive-vamana`)
ship alongside it; they do not replace or remove the sqlite-vec dependency from `khive-db`.

Retiring sqlite-vec from `khive-db` — routing `VectorStore` impls through `HnswIndex`,
providing a migration path (`kkernel migrate-vectors` or auto-detect-and-rebuild) —
is a separate future implementation issue, not completed by this port.

### Formal proofs

Proof files relocate from the internal implementation to `khive/proofs/Retrieval/` and
`khive/proofs/Scoring/`. Each file is self-contained: no runtime-dependency assumptions
in theorem statements. The proofs characterize the algorithms, not the implementation.

Every Rust module in `khive-retrieval` that corresponds to a verified algorithm carries
a header comment citing the theorem:

```rust
// Formal proof: khive.Retrieval.RRF.deterministic_ordering
```

`proofs/README.md` indexes all 146 theorems to their Rust modules.

`lake build` is wired into CI so proofs do not drift from code.

This gives khive-retrieval a property no production vector retrieval system has:
machine-checked correctness for core search algorithms. Downstream consumers can depend on this crate without
inheriting any deployment-specific assumptions.

### RuVector — opt-in adapter packs only

RuVector provides techniques the internal stack does not have. Each becomes a focused
adapter pack that wires the RuVector primitive into a khive verb. None are required for
default operation.

| Capability                                    | Source                           | Default |
| --------------------------------------------- | -------------------------------- | ------- |
| HNSW vector index                             | khive-retrieval (verified)       | yes     |
| BM25 keyword index                            | khive-retrieval (verified)       | yes     |
| RRF / Weighted / Union fusion                 | khive-retrieval (verified)       | yes     |
| Distance kernels (AVX-512 VNNI, NEON)         | `lattice-embed::simd`            | yes     |
| Quantization tiering (Hot/Warm/Cold)          | `lattice-embed::simd::tier`      | yes     |
| HNSW checkpoint + persistence                 | khive-retrieval                  | yes     |
| Graph-aware retrieval                         | khive-retrieval                  | yes     |
| ColBERT multi-vector late interaction         | RuVector adapter pack (deferred) | no      |
| Matryoshka adaptive-dimension retrieval       | RuVector adapter pack (deferred) | no      |
| Conformal prediction (uncertainty bounds)     | RuVector adapter pack (deferred) | no      |
| Spectral coherence metrics                    | RuVector adapter pack (deferred) | no      |
| DiskANN out-of-core (large-scale deployments) | RuVector adapter pack (deferred) | no      |

Adapter packs are deferred until a concrete verb-surface use case justifies one. The
default install does not depend on RuVector.

### Feature flags

The port inherits the internal crate's feature flag structure. `khive-retrieval` ships
with `default = []` — no features are on by default. Consumers opt into exactly the
capability surface they need.

`lattice-embed` is an **unconditional** (non-optional) dependency of `khive-retrieval`:
its core types (`EmbeddingModel`, `EmbeddingService`, `EmbedError`) are always available
via the `khive_retrieval::embed` re-export module. What the `embed` feature gates is the
**native model implementations** (`NativeEmbeddingService`, `CachedEmbeddingService`) that
bundle a live inference backend; those are off by default to keep the base crate lightweight
for consumers that supply their own embedding service.

| Feature            | Default | Notes                                                                                             |
| ------------------ | ------- | ------------------------------------------------------------------------------------------------- |
| `checkpoint`       | off     | HNSW snapshot save/restore                                                                        |
| `persist`          | off     | Index persistence to disk                                                                         |
| `embed`            | off     | Native `lattice-embed` model implementations (`NativeEmbeddingService`, `CachedEmbeddingService`) |
| `storage-adapters` | off     | `StorageVectorSearch`, `StorageKeywordSearch`; also enables sqlite-vec via `khive-db/vectors`     |
| `policy`           | off     | Gate integration (khive-gate); opt-in until gate story is complete                                |

## Rationale

### Why port rather than adopt RuVector as backend

The internal stack is mature (5000+ LOC of tests), formally verified (146 theorems),
deterministic by construction via i64 fixed-point (ADR-006), and already written. RuVector
is f32-only, has platform-bound determinism, and ships no formal proofs. Adopting RuVector
as the primary backend would replace a stronger implementation with a weaker one.

### Why lattice-embed and not RuVector for SIMD

lattice-embed has wider SIMD coverage (AVX-512 VNNI for int8 paths, multi-accumulator
NEON) and already-built quantization tiering with age-based Hot/Warm/Cold policy.
It is part of the existing stack; we own its tuning and can consolidate without a
third-party dependency.

### Why the formal proofs matter

Two reasons, each sufficient on its own:

1. **Correctness assurance.** Determinism, complexity bounds, and ranking properties are
   machine-checked. A retrieval system you can prove things about is qualitatively
   different from one you can only test.
2. **Verifiable correctness.** No production vector database — Pinecone, Qdrant,
   Weaviate, pgvector, RuVector — ships formal proofs of its core algorithms. khive
   does. This is a verifiable, concrete property of the implementation.

### Why RuVector at all

Collaboration value and access to research-grade techniques we do not have. ColBERT,
Matryoshka, conformal prediction, and spectral coherence are real and useful. Adapter
packs let us offer these techniques without compromising the verified core. RuVector's
author benefits from khive as an adoption story; khive benefits from technique
access without full implementation overhead.

### Why the ported HNSW is the preferred path over sqlite-vec

The ported HNSW supersedes sqlite-vec on every axis: performance, quantization,
deterministic scoring, formal verification. A dual-path (sqlite-vec + HNSW) adds
maintenance with no benefit once migration is complete. The migration path is
well-defined via `HnswIndex::rebuild`.

sqlite-vec currently remains in `khive-db` as the active `VectorStore` compatibility
implementation. Retiring it — routing `VectorStore` impls through `HnswIndex` and
providing the `kkernel migrate-vectors` path — is tracked as a separate follow-up.
No section of this ADR should be read as claiming that sqlite-vec has already been
dropped; it has not.

### Why a standalone crate now (vs. ADR-012's deferral)

ADR-012 deferred the crate split at ~1,500 LOC. The port lands ~29K LOC with a
well-bounded internal module structure (hnsw/, bm25/, fusion/, hybrid/, graph/,
query_ir/, adapters/, persist/). At this scale, the crate split is not premature — it
isolates a distinct algorithmic concern that has its own proof tree, its own benchmark
suite, and its own dependency surface (lattice-embed).

## Alternatives Considered

| Alternative                                                 | Pros                              | Cons                                                                                    | Decision            |
| ----------------------------------------------------------- | --------------------------------- | --------------------------------------------------------------------------------------- | ------------------- |
| Adopt RuVector as primary backend                           | Fewer crates maintained ourselves | Loses deterministic scoring + formal proofs; replaces stronger with weaker              | Rejected            |
| Keep sqlite-vec                                             | Already shipping; no migration    | Doesn't scale; no HNSW; no quantization; was always a stopgap                           | Rejected            |
| Port to a new crate from scratch (not from internal)        | Clean slate                       | Reinvents 29K LOC; loses proof correspondence                                           | Rejected            |
| Port internal + push i64 fixed-point upstream into RuVector | "Best of both worlds"             | 4-8 weeks of RuVector work; benefit is specific to khive; upstream acceptance uncertain | Rejected            |
| Keep retrieval in khive-runtime; no separate crate          | ADR-012's deferral path           | Valid at 1,500 LOC; unjustifiable at 29K LOC with its own proof tree                    | Superseded by scale |

## Consequences

### Positive

- Vector retrieval at production scale: HNSW with INT8 two-phase quantized search.
- Deterministic ranking across all platforms: i64 fixed-point throughout (ADR-006).
- 146 formal Lean4 proofs ship with the release as a unique correctness differentiator.
- `lattice-embed` SIMD investment is reused, not duplicated.
- `VectorStore` trait (ADR-005) does its job: implementation replaces without interface change.
- `FusionStrategy` and composition pattern from ADR-012 are unchanged; pack handlers need no updates.
- RuVector techniques remain available as opt-in adapter packs.

### Negative

- 2-3 weeks of focused porting work. Mitigated: the internal code is mature and well-tested;
  the port is mechanical except for dependency rewiring.
- One-time migration for any deployments with sqlite-vec data will be required when
  sqlite-vec retirement ships. Mitigated: the `HnswIndex::rebuild` path is well-tested;
  migration is planned as a single CLI command or startup path. Retirement itself is deferred
  follow-up work (see the "sqlite-vec retirement (deferred)" section above).
- Lean4 CI dependency added to workspace. Mitigated: `lake build` is isolated to the
  `proofs/` tree; Rust build and tests do not require it.

### Neutral

- Crate count grows by one (`khive-retrieval`). Dependency graph remains tractable; no
  circular dependencies.
- `kkernel` binary interface is unchanged; the port is below the verb surface.
- `khive-query` GQL/SPARQL layer is unchanged; it compiles to SQL, not retrieval ops.

## Implementation Phases

### Phase 1 — Code port

1. Create `crates/khive-retrieval/` from the internal `platform/retrieval/`.
2. Rewire dependencies per the mapping table above.
3. Replace `foundation::embed` calls with `lattice-embed`.
4. **Deferred follow-up**: Retire `sqlite-vec` from `khive-db` by routing `VectorStore`
   impls through `HnswIndex` via `StorageVectorSearch`. sqlite-vec remains the active
   `VectorStore` implementation in current shipped code.
5. **Deferred follow-up**: Decide and implement the sqlite-vec migration path
   (`kkernel migrate-vectors` or auto-rebuild on startup).
6. Migrate tests; verify all pass. Run smoke test against ported stack.

### Phase 2 — Proof relocation

1. Move proof files from the internal implementation to `khive/proofs/Retrieval/` and
   `khive/proofs/Scoring/`.
2. Author `proofs/README.md` indexing theorems to Rust modules.
3. Add proof-correspondence header comments to Rust source files.
4. Wire `lake build` into CI.

### Phase 3 — Brain primitives (ADR-032)

Port `Anchor`, `Selector`, and `CandidateRanker` cognitive primitives. Behavioral
monitoring service is deferred.

### Phase 4 — Multi-engine composition (ADR-031)

Wrap multiple `HnswIndex` instances per engine; fuse via the ported `FusionStrategy`
infrastructure.

### Phase 5+ — RuVector adapter packs (opportunistic)

Land an adapter pack when a concrete verb-surface use case justifies it. Candidates:
`khive-pack-ruvector-colbert`, `khive-pack-ruvector-matryoshka`,
`khive-pack-ruvector-conformal`. Each opt-in; each documents the RuVector primitive it
wraps.

## Open Questions

1. **Migration UX**: auto-detect-and-rebuild on startup vs. explicit `kkernel migrate-vectors`
   subcommand. Both are safe; auto is friendlier; explicit is auditable. Decided during
   Phase 1 based on the expected deployment pattern.

2. **HnswConfig defaults**: the internal stack ships multiple presets. Pick one default;
   expose others through `SearchConfig::preset()` variants. Defer until we have
   usage data from early adopters.

3. **Feature-flag rationalization**: decide whether `policy` becomes default-on when
   `khive-gate` lands a concrete policy story, or stays opt-in permanently. Reassess
   at Phase 3.

## References

- [ADR-005](ADR-005-storage-capability-traits.md) — VectorStore and TextSearch traits this port satisfies
- [ADR-006](ADR-006-deterministic-scoring.md) — DeterministicScore and i64 fixed-point throughout
- [ADR-011](ADR-011-embedding-and-inference.md) — lattice-embed SIMD and quantization boundary
- [ADR-012](ADR-012-retrieval-composition.md) — retrieval composition pattern this implements
- [ADR-024](ADR-024-fold-cognitive-primitives.md) — Objective implementations consumed by retrieval scoring
- [ADR-029](ADR-029-substrate-coordinator.md) — SubstrateCoordinator sits above this port for cross-backend fan-out
- [ADR-031](ADR-031-multi-engine-retrieval.md) — multi-engine fusion composes over khive-retrieval
- [ADR-032](ADR-032-brain-profile-orchestration.md) — brain profiles consume ranked candidates from this port
- Internal source ported from: internal `platform/retrieval/`
- SIMD / quantization foundation: `lattice/crates/embed/`
