# khive-fusion Algorithm Reference

**Scope:** Documents the rank fusion algorithms implemented in this crate.
Does not cover storage, embedding, or retrieval engine internals.

**Governing ADRs:** ADR-012 (retrieval composition), ADR-030 (hybrid retrieval), ADR-031 (multi-engine retrieval)

**Primary modules:** `src/rrf.rs`, `src/weighted.rs`, `src/union.rs`, `src/fuse.rs`, `src/strategy.rs`

**Tests:** `tests/integration.rs` (integration, property, regression)

**Benchmarks:** `benches/fusion_bench.rs` — see `docs/benchmarks.md` for the ledger and run command

**Last reviewed:** 2026-06-06

---

## Supported Strategies

- **RRF (Reciprocal Rank Fusion)**: Default and recommended. Uses only ranks, making it robust
  to score distribution differences between retrieval sources.
- **Weighted**: Linear combination of per-source min-max normalized scores with configurable weights.
- **Union**: Takes the maximum score per ID across all sources.
- **VectorOnly**: Passes through vector search results without fusion (exactly one source required).
- **KeywordOnly**: Passes through keyword search results without fusion (exactly one source required).

---

## RRF Formula

$$\text{score}(d) = \sum_{i} \frac{1}{k + \text{rank}_i(d)}$$

where:

- $k = 60$ (standard dampening constant — reduces dominance of rank-1 results)
- $\text{rank}_i(d)$ = position of document $d$ in retriever $i$'s results (1-indexed)
- If $d$ does not appear in retriever $i$, its contribution is 0

### Why k=60?

The constant `k=60` is empirically established in the IR literature (Cormack et al. 2009).
It dampens the advantage of very high ranks, preventing a single top-ranked result from
dominating when another source also returns it at a lower rank. Smaller `k` → more aggressive
rank-1 boosting; larger `k` → flatter distribution.

### Score range

Raw RRF scores range from near 0 (low-ranked in one source) to $n\_\text{sources} / (k+1)$ (rank-1 in
all sources). The `fuse()` dispatcher applies a `top_k` truncation after fusion; individual
`reciprocal_rank_fusion` calls return the full fused list without truncation.

### Properties

- **Better rank → higher contribution**: rank $r_1 < r_2$ implies $\frac{1}{k+r_1} > \frac{1}{k+r_2}$.
- **Present > absent**: any ranked document outscores one absent from all sources (score 0).
- **Commutative**: source list order does not affect final scores (addition is order-independent).

---

## Weighted Fusion

Weighted fusion linearly combines per-source min-max normalized scores:

$$\text{score}(d) = \sum_{i} w_i \cdot \hat{s}_i(d)$$

where $\hat{s}_i(d)$ is the min-max normalized score for document $d$ from source $i$, and $w_i$ are the normalized weights.

### Weight normalization (RETRIEVAL-07)

Weights are processed in this order before use:

1. Non-finite values (`NaN`, `+Inf`, `-Inf`) are treated as `0.0`
2. Negative values are treated as `0.0`
3. If all effective weights are `<= 0`, equal distribution is applied
4. Otherwise, weights are divided by their sum to normalize to 1.0

Use `try_normalize_weights` at public API boundaries to reject non-finite inputs with an error
instead of silently sanitizing them. `normalize_weights` is a lossy helper for internal use.

### Per-source score normalization

Each source is min-max normalized to $[0, 1]$ independently before weighted combination:

$$\hat{s}_i(d) = \frac{s_i(d) - \min_i}{\max_i - \min_i}$$

When all scores in a source are equal (or the source has one element), every entry receives
$1.0$ so it still contributes to the weighted combination. This ensures BM25 (unbounded) and
cosine similarity ($[0, 1]$) contribute proportionally to their configured weights regardless
of original score scale.

### Weight/source length mismatch

- Extra weights beyond `sources.len()` are excluded from the normalization denominator.
- Extra sources beyond `weights.len()` receive weight `0.0` (excluded from output).

---

## Union Fusion

Takes the maximum score per ID across all sources. Useful when you want the best score from
any retriever, without rank-based aggregation.

---

## VectorOnly / KeywordOnly

Passthrough strategies: exactly one source must be supplied. Supplying multiple sources is
a wiring error; `fuse()` returns an empty vector in both debug and release builds so the
error is detectable without panicking on caller input.

---

## `fuse()` dispatcher

`fuse()` in `src/fuse.rs` is the main entry point. It accepts any `FusionStrategy`, delegates
to the appropriate algorithm, then applies `top_k` truncation using a partial sort
(`select_nth_unstable_by`) that is O(n) for partitioning and O(k log k) for the final sort.
This avoids a full O(n log n) sort when `top_k << n`.

Individual algorithm functions (`reciprocal_rank_fusion`, `weighted_fusion`, `union_fusion`)
do not apply `top_k` truncation — they return the full fused list.

---

## Failure modes

| Condition                                    | Behavior                                                      |
| -------------------------------------------- | ------------------------------------------------------------- |
| Empty sources                                | Returns empty vec                                             |
| `top_k == 0`                                 | Returns empty vec                                             |
| VectorOnly/KeywordOnly with multiple sources | Returns empty vec (wiring error)                              |
| All-zero or all-negative weights             | Falls back to equal weight distribution                       |
| Non-finite weights in `weighted_fusion`      | Treated as 0.0 (lossy); use `try_normalize_weights` to reject |

---

## Example

```rust
use khive_fusion::{fuse, FusionStrategy, reciprocal_rank_fusion};
use khive_score::DeterministicScore;

// Two retrieval sources with different rankings
let vector_results = vec![
    ("doc_a", DeterministicScore::from_f64(0.95)),
    ("doc_b", DeterministicScore::from_f64(0.90)),
    ("doc_c", DeterministicScore::from_f64(0.85)),
];

let keyword_results = vec![
    ("doc_b", DeterministicScore::from_f64(0.88)),
    ("doc_c", DeterministicScore::from_f64(0.75)),
    ("doc_d", DeterministicScore::from_f64(0.70)),
];

// Fuse using RRF with k=60
let fused = fuse(
    vec![vector_results, keyword_results],
    &FusionStrategy::Rrf { k: 60 },
    5,
);

// doc_b appears in both sources → highest RRF score
assert_eq!(fused[0].0, "doc_b");
```
