# ADR-090: khive-retrieval as Ported Verified Implementation

**Status**: accepted (port executed in commit `e097bc8` — crates `khive-retrieval`,
`khive-hnsw`, `khive-bm25`, `khive-fusion`, `khive-fold` exist; formal proof relocation
pending)\
**Date**: 2026-05-22 (drafted; renumbered from ADR-078 → ADR-090 on 2026-05-22 to avoid
collision with the multi-engine embedding ADR series at 078/081-084)\
**Authors**: Ocean, lambda:khive\
**Depends on**: ADR-005 (Storage Capability Traits)\
**Composed with**: ADR-091 (Multi-engine retrieval composition), ADR-092 (Brain profile
orchestration)\
**Note**: This ADR documents the design that the retrieval port followed. The port has
executed; this ADR ratifies the decision in the design record.

## Context

khive-oss inherited the storage trait architecture (ADR-005). The vector
backend currently in place behind that trait is sqlite-vec — brute-force,
non-scaling. The cognitive layer (`khive-fold`) defines the right
abstractions for retrieval composition (Fold, Objective, Anchor, Selector) but
needs a real retrieval implementation underneath.

Two existing sources of retrieval capability are in scope:

**Mature internal stack** at `khive-internal/platform/retrieval/`:
~29K LOC, 5000+ LOC of tests, with HNSW, BM25, RRF fusion, INT8 quantized
two-phase search, checkpointing, persistence, deterministic scoring (i64
fixed-point), and cross-platform consistency invariants. The stack is
**formally verified in Lean4** — 146 theorems across HNSW level distribution
and complexity, BM25 score properties, RRF deterministic ordering, distance
metric correctness, quantization error bounds, and skip-condition soundness.

**`lattice-embed`** at `lattice/crates/embed/`: SIMD distance kernels
(AVX-512F / AVX-512 VNNI / AVX2+FMA / ARM NEON / scalar fallback),
quantization tiering (f32 / Int8 / Int4 / Binary with age-based Hot/Warm/Cold
policy), embedding inference, and canonical-bytes-for-deterministic-hashing
support.

**RuVector** (MIT, ruv is collaborator) provides additional retrieval
techniques not present in our stack: ColBERT-style multi-vector late
interaction, Matryoshka adaptive-dimension search, conformal prediction,
spectral coherence metrics. These are research-grade primitives worth having
access to but are not core to the default retrieval surface.

The previous direction of "adopt RuVector as the backend" was based on an
incomplete view — it would have duplicated mature internal work that
already exists with stronger guarantees (deterministic scoring, formal
proofs) than RuVector provides.

## Decision

**Port `khive-internal/platform/retrieval/` into the OSS workspace as
`khive-retrieval`. Use `lattice-embed` as the SIMD foundation. Treat
RuVector as an opt-in source of supplementary techniques delivered through
focused adapter packs.**

### Crate layout after the port

```
khive/khive/crates/
├── khive-storage          — trait contract (unchanged)
├── khive-fold             — Fold / Objective / Anchor / Selector (unchanged)
├── khive-score            — DeterministicScore (unchanged)
├── khive-gate             — Gate trait + AllowAllGate default (unchanged)
├── khive-gate-rego        — Rego policy sample implementation (unchanged)
├── khive-db               — SQLite backend; drops sqlite-vec dependency
├── khive-retrieval        — NEW: ported from khive-internal/platform/retrieval
│   ├── hnsw/              — HNSW index, INT8 quantized two-phase search
│   ├── bm25/              — BM25 keyword index
│   ├── fusion/            — RRF / Weighted / Union strategies
│   ├── hybrid/            — combined dense + keyword search
│   ├── graph/             — relationship-aware retrieval
│   ├── query_ir/          — query intermediate representation
│   ├── adapters/          — StorageKeywordSearch, StorageVectorSearch
│   └── persist/, hnsw/checkpoint/ — index persistence
└── (existing packs)        — consume khive-retrieval through trait surface

lattice/crates/embed/       — SIMD distance kernels, quantization tiering, inference (existing)

khive/khive/proofs/
├── Retrieval/              — HNSW.lean, BM25.lean, RRF.lean, RRFAnalysis.lean,
│                              QuantizationBounds.lean, SkipCondition.lean,
│                              Distance.lean, Cosine.lean, Graph.lean,
│                              RetrievalAlgorithms.lean
├── Scoring/                — Score.lean and related determinism proofs
└── (future)                — additional proof trees as we verify more domains
```

### Dependency rewiring during port

The internal crate's dependencies map to OSS as follows:

| khive-internal                    | khive-oss equivalent                                     |
| --------------------------------- | -------------------------------------------------------- |
| `foundation/score`                | `khive-score` (already in OSS)                           |
| `foundation/embed`                | **`lattice-embed`** (the strategic switch)               |
| `foundation/types`                | `khive-types` (already in OSS)                           |
| `foundation/fold`                 | `khive-fold` (already in OSS)                            |
| `platform/db`                     | `khive-db` (already in OSS)                              |
| `platform/storage-traits`         | `khive-storage` (already in OSS)                         |
| `platform/policy`                 | `khive-gate` + `khive-gate-rego` (existing OSS gate API) |
| `foundation/inference` (optional) | reuse `lattice-inference` (already exists)               |

The HNSW INT8 quantization arena delegates distance kernels to
`lattice-embed::simd` rather than maintaining its own kernels — wider SIMD
coverage (AVX-512 VNNI, multi-accumulator NEON) and consolidated tuning.

### Verb surface

The port preserves the existing internal API where possible. Public surface
that flows up to packs and verbs:

```rust
pub use khive_retrieval::{
    SearchConfig,                            // per-call hybrid search config
    FusionStrategy,                          // Rrf, Weighted, VectorOnly, KeywordOnly, Union
    HnswIndex, HnswConfig, HnswCheckpoint,   // vector index
    Bm25Index, Bm25Config,                   // keyword index
    HybridSearcher,                          // dense + keyword + fusion
    StorageVectorSearch, StorageKeywordSearch, // storage adapters
};
```

Pack handlers (memory, kg, etc.) consume `HybridSearcher` for `recall` and
`search` verbs. Multi-engine composition (ADR-091) wraps multiple
`HnswIndex` instances behind the same surface.

### Formal proofs in OSS

Proof files move from their internal location into
`khive/khive/proofs/Retrieval/` and `khive/khive/proofs/Scoring/`. Each
proof file is mathematically self-contained: HNSW level distribution and
search complexity, BM25 non-negativity and monotonicity, RRF deterministic
ordering, distance metric properties, quantization error bounds. No
runtime-dependency assumptions in the theorem statements — the proofs
characterize the algorithms themselves.

Each Rust module carries a comment header citing the corresponding theorems
(e.g., `// Formal proof: Khive.Retrieval.RRF.deterministic_ordering`). A
top-level `proofs/README.md` indexes theorems to Rust modules.

This gives khive-oss a property no other production vector retrieval system
has: machine-checked correctness for the core algorithms. The differentiation
holds in marketing, technical due diligence, and any downstream commercial
context.

### RuVector — opt-in adapter packs only

Where RuVector provides a technique we do not have, we ship a focused adapter
pack that wires the RuVector primitive into a khive verb. None of these are
required for default operation; each is opt-in.

Capability-by-capability disposition:

| Capability                            | Source                                      | Default-on? |
| ------------------------------------- | ------------------------------------------- | ----------- |
| HNSW vector index                     | ported khive-retrieval (verified)           | yes         |
| BM25 keyword index                    | ported khive-retrieval (verified)           | yes         |
| RRF / Weighted fusion                 | ported khive-retrieval (verified)           | yes         |
| Distance kernels                      | `lattice-embed::simd`                       | yes         |
| Quantization tiering (Hot/Warm/Cold)  | `lattice-embed::simd::tier`                 | yes         |
| HNSW checkpoint + persistence         | ported khive-retrieval                      | yes         |
| Graph traversal for retrieval         | ported khive-retrieval                      | yes         |
| ColBERT multi-vector late interaction | RuVector adapter pack (future)              | no          |
| Matryoshka adaptive-dim retrieval     | RuVector adapter pack (future)              | no          |
| Conformal prediction (uncertainty)    | RuVector adapter pack (future)              | no          |
| Spectral coherence metrics            | RuVector adapter pack (future)              | no          |
| DiskANN out-of-core                   | RuVector adapter pack (future, cloud scale) | no          |

Adapter packs are deferred until a concrete verb-surface use case justifies
one. The default install does not depend on RuVector.

### sqlite-vec retirement

`khive-db` drops the `sqlite-vec` dependency. Vector search routes through
the ported HNSW. Existing OSS users who have data in sqlite-vec format need
a one-time migration: rebuild the HNSW index from stored embeddings (the
`HnswIndex::rebuild` path already handles this).

## Rationale

### Why port instead of adopting RuVector?

The internal stack is mature (5000+ LOC of tests), formally verified (146
theorems), deterministic by construction, and we already wrote it. RuVector
is f32-only with platform-bound determinism and no formal proofs. Adopting
RuVector as the primary backend would have replaced a stronger implementation
with a weaker one.

### Why lattice-embed and not RuVector for SIMD?

lattice-embed has wider SIMD coverage (AVX-512 VNNI for int8 paths,
multi-accumulator NEON), already-built quantization tiering with age-based
policy, and is part of our stack. We own its tuning.

### Why are the formal proofs valuable for OSS?

Two reasons:

1. **Correctness assurance.** A retrieval system you can prove things about
   is qualitatively different from one you can only test. Determinism,
   complexity bounds, and ranking properties are all machine-checked.
2. **Market differentiation.** No production vector database — RuVector,
   Pinecone, Qdrant, Weaviate, pgvector — ships with formal proofs of its
   core algorithms. This is a unique property of khive-oss.

### Why RuVector at all, then?

For collaboration value and access to research-grade techniques we don't
have. ColBERT, Matryoshka, conformal prediction, and spectral coherence are
real and useful. We can offer them as opt-in packs without compromising the
verified core. RuVector's author (ruv) benefits from khive-oss as an
adoption story; khive-oss benefits from access to techniques without
implementing them all ourselves.

### Why drop sqlite-vec entirely?

Because the ported HNSW supersedes it on every axis — performance,
quantization, formal verification, deterministic scoring. Keeping sqlite-vec
as an alternative path would add maintenance for no benefit. One-time
migration on upgrade is straightforward (`rebuild` from stored embeddings).

## Alternatives Considered

| Alternative                                                 | Pros                               | Cons                                                                                       | Why rejected                    |
| ----------------------------------------------------------- | ---------------------------------- | ------------------------------------------------------------------------------------------ | ------------------------------- |
| Adopt RuVector as backend                                   | Fewer crates to maintain ourselves | Loses deterministic scoring + formal proofs; duplicates investment in mature internal code | Worse on every dimension        |
| Keep sqlite-vec                                             | Already shipping                   | Doesn't scale; no HNSW; no quantization                                                    | sqlite-vec was always a stopgap |
| Port to a new crate from scratch, not from internal         | Clean slate                        | Reinvents 29K LOC; loses proof correspondence                                              | Wasted effort                   |
| Port internal + push i64 fixed-point upstream into RuVector | "Best of both"                     | 4-8 weeks of RuVector work; benefit specific to khive                                      | Not justified                   |

## Consequences

### Positive

- Vector retrieval at production scale (HNSW + quantization).
- Deterministic scoring across all platforms (i64 fixed-point throughout).
- Formal proofs ship with the OSS release as a unique differentiator.
- lattice-embed's SIMD investment is reused, not duplicated.
- The trait abstraction (ADR-005) does its job — implementation choice is per-trait, swappable.
- RuVector stays useful as adapter-pack source for niche techniques.

### Negative

- 2-3 weeks of focused porting work. Mitigated: the internal code is mature
  and well-tested; the port is mechanical except for dependency rewiring.
- One-time migration for OSS users with sqlite-vec data. Mitigated: the
  rebuild path is straightforward and well-tested.

### Neutral

- Crate count grows by one (`khive-retrieval`). Crate dependency graph
  remains tractable; no circular dependencies introduced.

## Implementation phases

### Phase 1 — Code port

1. Create `khive/khive/crates/khive-retrieval/` from `khive-internal/platform/retrieval/`.
2. Rewire dependencies per the mapping table above.
3. Replace `khive-internal::foundation::embed` calls with `lattice-embed`.
4. Drop `sqlite-vec` from `khive-db`. Update `VectorStore` trait impls to
   route through `HnswIndex`.
5. Migrate tests; verify all pass.
6. Run smoke test against ported stack.

### Phase 2 — Proof relocation

1. Move proof files: `khive-cloud/proofs/Khive/Retrieval/*.lean` →
   `khive/khive/proofs/Retrieval/`.
2. Move score-related proofs to `khive/khive/proofs/Scoring/`.
3. Author `khive/khive/proofs/README.md` indexing theorems to Rust modules.
4. Add proof-correspondence header comments to Rust source files.
5. Wire `lake build` into a CI check so proofs don't drift.

### Phase 3 — Brain primitives port (ADR-092)

Cognitive primitives only — Anchor, Selector, CandidateRanker. Behavioral
monitoring service stays out of OSS.

### Phase 4 — Multi-engine composition (ADR-091)

Wrap multiple `HnswIndex` instances per engine; fuse via the ported
`FusionStrategy` infrastructure.

### Phase 5+ — RuVector adapter packs (opportunistic)

Land an adapter pack when a concrete verb-surface use case justifies it.
Candidates: `khive-pack-ruvector-colbert`, `khive-pack-ruvector-matryoshka`,
`khive-pack-ruvector-conformal`. Each opt-in, each ships with documentation
of what RuVector primitive it wraps.

## Open questions to resolve during implementation

1. **OSS user migration**: any existing OSS deployments with sqlite-vec data
   need a one-time HNSW rebuild on upgrade. Provide a CLI command
   (`khive-mcp --rebuild-vectors`) or auto-detect-and-rebuild on startup?

2. **HNSW configuration defaults**: the internal stack has multiple
   `HnswConfig` presets. Pick a single default for OSS and document the
   tunable parameters; expose presets through `SearchConfig::default()` /
   `vector_only()` etc.

3. **Feature-flag policy**: internal has `policy`, `checkpoint`, `persist`,
   `embed`, `storage-adapters` features. Decide which are default-on for
   OSS. Lean: default-on for `checkpoint`, `persist`, `embed`,
   `storage-adapters`; keep `policy` opt-in until we have a concrete OSS
   policy story.

## References

- ADR-005 — Storage capability traits
- ADR-091 — Multi-engine retrieval composition (consumer of the ported `HnswIndex`)
- ADR-092 — Brain profile orchestration (consumer of `khive-fold` + `khive-retrieval`)
- Internal source ported: `khive-internal/platform/retrieval/`
- SIMD/quantization foundation: `lattice/crates/embed/`
- Existing OSS gate: `khive/khive/crates/khive-gate/`, `khive-gate-rego/`
