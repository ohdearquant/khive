# ADR-054: ANN Build Strategy and Scaling Limits

**Status**: Proposed
**Date**: 2026-06-14

## Context

khive ships khive-vamana as its production ANN index: a Vamana/DiskANN-style proximity graph used
by the knowledge pack and hybrid retrieval stack ([ADR-012](ADR-012-retrieval-composition.md),
[ADR-031](ADR-031-multi-engine-retrieval.md)). The index serves as the vector retrieval leg of the
hybrid FTS+vector+graph fusion pipeline.

The driving engineering goal is **sublinear query latency with extreme performance** on khive's
production workload. If accepted, this ADR would establish the scaling contract: which axes are
genuinely sublinear, which are not, what the evidence base is, and where the honest limits lie.
The evidence cited here was gathered on a representative proxy corpus (BeIR/quora); production-
corpus measurement on khive's own knowledge-graph data is required before the query-sublinearity
claim is accepted as binding. Future contributors should not re-litigate these decisions without
new measurements once the ADR is accepted.

### Governing mechanism: intrinsic dimension

The decisive variable for graph ANN scaling is the data's **intrinsic dimension** (the effective
dimensionality of the manifold the vectors lie on), not the ambient vector dimension. The
Indyk-Xu (NeurIPS 2023) doubling-dimension bound gives a query complexity of
O((4α)^δ · log Δ) for graph ANN indexes, where δ is the doubling dimension of the dataset and
Δ is the diameter ratio. This bound applies universally to all DiskANN/Vamana and HNSW-family
indexes. Its significance is qualitative: it proves that graph-ANN query complexity is governed by the
data's intrinsic (doubling) dimension rather than its ambient dimension, so a graph index escapes
the curse of ambient dimension. It is not a useful numeric bound, because the prefactor (4α)^δ is
large at every δ in the range of interest (for α=1.2 it exceeds 10^9 by δ=14). What sets khive's
operating regime is therefore empirical, not the literal bound: the iso-recall beam stays flat at
low intrinsic dimension and begins to grow above intrinsic dimension ~20.

### Iso-recall methodology

All scaling measurements use **iso-recall probes**: at each corpus size N, the minimum
search-list size (beam) required to achieve recall@10 ≥ 0.95 is recorded. The floor is
max(k, max_degree) = max(10, 64) = 64 for the configured production index (R=64, L=128).
Build config is R=64, L=128 throughout. Ground truth at each N is recomputed by brute force on
that subset. Latency is measured warm-cache (p50). Exponents are 3-point log-log OLS fits across
a ≥5× range in N unless otherwise noted.

## Decision

### Scaling contract

The following table records the proposed asymptotic floor for each axis. If this ADR is
accepted, claims beyond these bounds would require new measurements and an ADR amendment.

| Axis                   | Asymptotic floor                                                                           | Sublinear in N?                          |
| ---------------------- | ------------------------------------------------------------------------------------------ | ---------------------------------------- |
| Query latency          | empirical O(log N) in the low-intrinsic-dim regime; Indyk-Xu O((4α)^δ · log Δ) upper bound | Yes, proxy-measured (production pending) |
| Update (insert/delete) | O(log N) insert; O(degree² · d) Wolverine delete, N-independent per op                     | Not yet established                      |
| Build work             | Ω(N)                                                                                       | No                                       |
| Build wall-clock       | Ω(N)/P on P cores                                                                          | No                                       |
| Memory                 | O(N)                                                                                       | No                                       |

The update axis carries a **tracked design obligation** that is not yet discharged: the amortized
cost of periodic consolidation must be measured and confirmed sublinear at production scale before
any "Yes" appears in the sublinear column for insert/delete. See ADR-052 for the consolidation
design. The query-latency "Yes" is scoped to the proxy-corpus measurement described below;
it becomes binding only after production-corpus confirmation.

The build and memory axes deliver only **extreme constant-factor** improvement. No future
contributor should claim asymptotic build or memory sublinearity. The constant-factor levers are
the perf stack described in the Consequences section.

### Representative proxy workload is in the sublinear regime (production measurement pending)

khive's production primary embedding model is `all-minilm-l6-v2` (384-d), verified in
`crates/khive-runtime/src/engine_config.rs` as the configured primary model. Its intrinsic
dimension was measured on a 30K sample from a 522K-passage sentence corpus (BeIR/quora) used
as a representative proxy for khive's note and entity text. This is not khive's own
knowledge-graph corpus; it is a proxy appropriate in kind (short sentence-length passages).
Estimators used: TwoNN and MLE
(`evidence/adr-054/khive-real-intrinsic-dim.json`):

- TwoNN: 12.3
- MLE: 15.6
- Consensus: ~14 (ESS excluded; it overestimates on clustered high-dimensional distributions)

The `intrinsic_dim_approx: "~20"` field stamped in `evidence/adr-054/probe-results-khive-real-3pt.json`
is a stale value carried over from the GloVe probe template. The measured value for this proxy
workload is ~14, recorded in `evidence/adr-054/khive-real-intrinsic-dim.json`.

This places khive's workload in the intermediate-low intrinsic-dim regime, substantially closer
to SIFT-1M (~10) than to GloVe-100-angular (~20). Sentence embeddings cluster by semantic topic,
and that anisotropy makes effective intrinsic dimension lower than the raw estimator and
actively helps greedy graph traversal.

### Evidence

**Proxy workload headline result (BeIR/quora, representative for khive's sentence corpus)**
(`evidence/adr-054/probe-results-khive-real-3pt.json`,
α=1.0, R=64/L=128, 384-d L2-normalized, 3-point fit at N = 100K/200K/500K):

| N    | iso-recall beam | recall@10 | warm p50 | brute p50 | speedup |
| ---- | --------------- | --------- | -------- | --------- | ------- |
| 100K | 64 (floor)      | 0.991     | 283 µs   | 3507 µs   | 12.4×   |
| 200K | 64 (floor)      | 0.990     | 331 µs   | 7040 µs   | 21.3×   |
| 500K | 64 (floor)      | 0.987     | 307 µs   | 17624 µs  | 57.4×   |

3-point fits: beam-growth exponent 0.000 (R²=1.0), query exponent 0.043 (R²=0.20; the low R²
is correct here: at near-flat latency, slope noise dominates, so report latency as flat rather
than the point estimate), brute-force exponent 1.003 (R²=1.0, confirms the harness is
measuring N-linear work). The beam is pinned at the floor across the full 5× N range. Recall
holds ≥0.987 at 500K. The speedup widens with N because brute force is linear and query is flat.

**SIFT-1M corroboration (gold-standard benchmark)** (`evidence/adr-054/probe-results-sift-3pt.json`, R=64/L=128,
128-d raw L2, 3-point fit at N = 100K/300K/1M):

| N    | iso-recall beam | recall@10 | warm p50 | brute p50 | speedup |
| ---- | --------------- | --------- | -------- | --------- | ------- |
| 100K | 64 (floor)      | 0.999     | 166 µs   | 1105 µs   | 6.7×    |
| 300K | 64 (floor)      | 0.993     | 248 µs   | 3418 µs   | 13.8×   |
| 1M   | 64 (floor)      | 0.986     | 334 µs   | 11510 µs  | 34.4×   |

3-point fits: beam-growth exponent 0.000 (R²=1.0), query exponent 0.303 (R²=0.988), brute-force
exponent 1.017 (R²=1.0), build wall-clock exponent 1.449 (R²=1.0, Ω(N) as expected).

**Regime boundary: GloVe-100-angular** (`evidence/adr-054/probe-results-glove.json`, 2-point fit at N = 100K/300K):

| N    | iso-recall beam | recall@10 | warm p50 | brute p50 | speedup |
| ---- | --------------- | --------- | -------- | --------- | ------- |
| 100K | 203             | 0.950     | 898 µs   | 1041 µs   | 1.16×   |
| 300K | 378             | 0.950     | 2787 µs  | 3171 µs   | 1.14×   |

2-point fits: beam-growth exponent 0.566, query exponent 1.031. At intrinsic dim ~20 the
flat-beam regime does not hold: the beam must grow as ~N^0.57 to maintain recall, which drags
query to near-linear and collapses the speedup to ~1.1×. GloVe-100 uses word vectors; khive
indexes sentence vectors, and sentence embeddings sit at lower intrinsic dimension.

**α-sweep on GloVe** (`evidence/adr-054/probe-results-glove-alpha.json`, R=64/L=128, N=100K and 300K): at α=1.0
the iso-recall beam is 132 at 100K versus 203 at α=1.2, and the 2-point beam-growth exponent
drops from 0.566 to 0.341. The direction matters: khive-vamana uses the standard DiskANN
RobustPrune α-squared condition (`graph.rs`), in which α=1.0 is the most aggressive pruning
(sparsest graph) and α>1 retains more edges. A sparser graph reaching the recall target with a
smaller beam is counterintuitive, since DiskANN normally benefits from α>1, and this rests on
only two points of out-of-regime word-vector data. It is a preliminary observation, not a
recommendation: the mechanism is unconfirmed and no production α change is implied. khive's
sentence-embedding workload sits at intrinsic dim ~14 and is already flat-beam at the default
config, so α tuning is off khive's critical path.

## Consequences

### What this ADR proposes to claim (pending acceptance and production-corpus confirmation)

1. **Query sublinearity is measured on a representative proxy corpus.** The proxy (BeIR/quora,
   intrinsic dim ~14) uses the same model (`all-minilm-l6-v2`, 384-d) as khive's production
   configuration and produces flat-beam behavior across 5× (100K to 500K) with recall
   ≥0.987. The speedup widens with N (57× at 500K vs brute force). The same regime is
   corroborated at larger scale on SIFT-1M (34× at 1M). Production-corpus measurement on
   khive's own knowledge-graph data is required before this claim becomes binding.

2. **The claim is not a blanket statement.** It holds for sentence-embedding workloads at
   intrinsic dim ≤ ~16. It does not hold for word-vector or other high-intrinsic-dim data.
   The regime boundary sits near intrinsic dim ~20; the precise crossover depends on manifold
   geometry and is not a fixed number.

3. **Build and memory are Ω(N) and O(N).** The "extreme performance" delivery on those axes is
   the constant-factor harvest stack, not asymptotic improvement:
   - SQ8 scalar quantization: ~4× memory reduction (GsSq8Codec for the Vamana acquisition tier,
     specified in ADR-052). The kernel speedup is 16.8× on the distance computation alone
     (ADR-052), but the end-to-end build improvement is only 1.3-1.4× (ADR-052): build is
     distance-bound, so random DRAM fetches for neighbor IDs cap the gain. Lifting that ceiling
     would require an access-aware memory layout that stores neighbor IDs contiguously with the
     quantized codewords (the approach taken by the Flash index). That is an identified future
     optimization, not yet specified in any khive ADR.
   - Wolverine 2-hop eager-delete repair (ADR-052): per-delete repair cost is bounded by degree,
     not corpus size, so the hot-path delete cost is N-independent. Periodic consolidation
     handles compaction off the critical path.

4. **Corpus scope caveat.** The intrinsic-dimension measurement and the scaling probe use
   BeIR/quora as a representative proxy for khive's note and entity text, not khive's own
   knowledge-graph corpus. The proxy is appropriate in kind (short sentence-length text passages)
   but it is not khive's actual data. The flat-beam result at N ≤ 500K on the proxy supports
   the hypothesis that khive's real corpus is also in the sublinear regime, but it does not
   prove it: a proxy corpus with intrinsic dim ~14 cannot serve as falsification evidence for
   khive's own corpus. A production measurement on khive's own entity and note vectors is
   required to discharge this obligation.

5. **Query-exponent interpretation.** The measured query exponent (0.043 on the BeIR/quora proxy,
   0.303 on SIFT-1M) is directional. At near-flat latency the 3-point fit is noise-dominated
   (R²=0.20 for the proxy fit). Report query latency as **flat** at the production scale
   range, with the exponent as directional confirmation of sublinearity, not a precise estimate.

6. **Consolidation obligation (update axis not yet established).** The update-axis sublinearity
   target (O(log N) amortized) rests on amortized consolidation staying sublinear at scale.
   This has not been measured. The update row in the scaling table above is marked "Not yet
   established" precisely because of this gap. The tracked obligation: measure consolidation
   throughput as a function of N before any "Yes" is entered for the update axis.

7. **α tuning is an open question, not guidance.** A preliminary 2-point GloVe probe showed α=1.0
   reaching the recall target with a smaller beam than α=1.2 on that out-of-regime word-vector
   data. This is counterintuitive for DiskANN (where α>1 usually helps) and the mechanism is
   unconfirmed, so it is not a recommendation. The production default is unchanged; any future
   change requires a probe on the target corpus confirming recall is maintained.

## Related ADRs

- [ADR-011](ADR-011-embedding-and-inference.md): Embedding model selection and the primary model
  configuration that determines intrinsic dimension.
- [ADR-012](ADR-012-retrieval-composition.md): Retrieval composition and hybrid search pipeline.
- [ADR-031](ADR-031-multi-engine-retrieval.md): Multi-engine retrieval and fusion.
- [ADR-044](ADR-044-vector-store-extensions.md): VectorStore trait extensions.
- [ADR-052](ADR-052-ann-production-lifecycle.md): SQ8 quantization, tombstone delete,
  consolidation, and crash-safe persistence: the lifecycle operations and constant-factor perf
  stack referenced in this ADR's Consequences section.
