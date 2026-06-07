# BM25 Algorithm: Design Notes

## Scoring Formula

$$
\text{score}(D, Q) = \sum_{t \in Q} \text{IDF}(t) \cdot \frac{f(t, D) \cdot (k_1 + 1)}{f(t, D) + k_1 \cdot \left(1 - b + b \cdot \frac{|D|}{\text{avgdl}}\right)}
$$

where:

- $f(t, D)$ — term frequency of $t$ in document $D$
- $|D|$ — document length (token count)
- $\text{avgdl}$ — average document length across the corpus
- $k_1 = 1.2$ — term frequency saturation parameter
- $b = 0.75$ — length normalization parameter

IDF uses the Robertson-Walker variant with `+1` smoothing:

$$
\text{IDF}(t) = \ln\!\left(\frac{N - \text{df}(t) + 0.5}{\text{df}(t) + 0.5} + 1\right)
$$

where $N$ is total document count and $\text{df}(t)$ is the number of documents containing $t$.

## Scoring Properties

- $\text{IDF}(t) \geq 0$ for all terms (guaranteed by `+1` inside $\ln$)
- TF component $\in [0,\; k_1 + 1)$ — saturation bound
- Total BM25 score $\geq 0$
- Length factor $= 1$ at average doc length (no adjustment)
- Long documents penalized; short documents boosted

**Proof correspondence**: `khive.Retrieval.BM25.idf_nonneg`, `khive.Retrieval.BM25.tf_bounded`,
`khive.Retrieval.BM25.bm25_nonneg`

## Floating-Point Considerations (RETRIEVAL-04)

`f64` is used for internal BM25 score calculations because:

- Logarithmic operations in IDF computation require floating-point
- Intermediate calculations require full precision
- Standard practice in IR systems (Lucene, Elasticsearch use f64)

### Cross-Platform Behavior

While `f64` follows IEEE 754 on all supported platforms, minor variance may occur:

- FMA (fused multiply-add) availability differs across CPUs
- Compiler optimizations may reorder floating-point operations
- Extended precision (x87) on older x86 may affect intermediate results

**Mitigation**: Scores are converted to `DeterministicScore` at the API boundary
(in `Bm25Index::search`), which provides:

- Canonical representation for storage and comparison
- Consistent serialization across platforms
- Protection against NaN propagation

### Golden Tests

Golden tests in the `tests::golden_tests` module verify known expected values using a controlled
corpus. These tests use fixed documents and queries with hand-calculated expected scores to catch
any drift in scoring behavior across versions or platforms.

## SIMD Acceleration (search.rs)

The brute-force scoring path uses architecture-specific SIMD to process postings in parallel:

- **aarch64 (NEON)**: 4-wide batches using 128-bit NEON registers.
- **x86_64 (AVX2)**: 8-wide batches using 256-bit YMM registers, with optional FMA for fused
  multiply-add in the denominator computation. Detected at runtime via
  `is_x86_feature_detected!`.
- **Scalar fallback**: Used on all other targets or when AVX2 is not available at runtime.

Dispatch happens once per term (not per batch) to avoid repeated feature checks in the hot loop.

The `score_batch_avx2_fma` variant uses `_mm256_fmadd_ps` for the denominator, giving a
single-rounding result. The performance difference is marginal since `_mm256_div_ps` dominates,
but FMA is free when available.

## Block-Max WAND

For large queries (total postings ≥ `SMALL_QUERY_POSTINGS_THRESHOLD` = 16,384), the index uses
Block-Max WAND instead of brute-force scoring.

Each posting list is divided into blocks of `block_size` (default 128). For each block, the
maximum possible BM25 contribution of that term is precomputed and stored as
`max_score_contribution`. A suffix-max array (`suffix_max_score`) allows skipping entire tails of
a posting list when the upper bound cannot beat the current threshold.

The WAND algorithm:

1. Sort cursors by their current document ID
2. Find a pivot document: the first doc where the sum of `suffix_max_score` for all preceding
   cursors exceeds the current threshold
3. Apply block-level upper-bound pruning: skip blocks where the sum of `max_score_contribution`
   for the block containing the pivot cannot beat the threshold
4. If the block bound passes, align all cursors to the pivot and compute the exact score
5. Update the top-k heap if the score beats the threshold

Empirically, the brute-force SIMD path matches or beats WAND on aarch64 (Apple M-series) up to
~10K–16K total postings. Above 16K, WAND's block-skip savings overcome its per-cursor overhead.

## IDF Cache Design

The IDF cache is keyed by document frequency (`df`) rather than term string.

IDF depends only on `df` and `N` (total document count). Multiple terms sharing the same `df`
produce identical IDF values, so keying by `df` (a `usize`) is both more compact and more
correct than keying by term string.

When `N` changes (any add/remove), the entire cache is invalidated by comparing `cached_doc_count`
against the current `doc_count()`. This eliminates the stale-IDF bug where targeted per-term
eviction left entries computed with the old `N` in the cache.

## Thread Safety (RETRIEVAL-08)

`search()` takes `&self` (not `&mut self`) to enable concurrent reads. The only mutable state
accessed during search is the IDF cache and block-max metadata, which use `RwLock` for interior
mutability.

Design alternatives considered:

| Approach                 | Pros                          | Cons                     | Decision   |
| ------------------------ | ----------------------------- | ------------------------ | ---------- |
| `&mut self` for search   | No interior mutability        | Blocks concurrent reads  | Rejected   |
| `RefCell` for cache      | Simple, no sync               | Not thread-safe          | Rejected   |
| `RwLock` for cache       | Thread-safe, concurrent reads | Overhead on cache access | **Chosen** |
| No cache (recompute IDF) | Pure `&self`                  | ~10–20% slower search    | Rejected   |

The internal `RwLock` on the IDF cache provides fine-grained locking for cache updates during
search, avoiding exclusive locking on the entire index.
