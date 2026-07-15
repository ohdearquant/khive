# Search execution

BM25 search chooses between exhaustive SIMD scoring and Block-Max WAND while preserving one
deterministic ranking contract.

## `search` and `search_with_context`

`Bm25Index::search(query_text, k)` returns at most `k` `(document_id, DeterministicScore)` pairs in
descending score order with document ID as the deterministic tie-breaker. Empty queries, `k == 0`,
and queries whose terms are absent return an empty vector.

`search_with_context` has the same result contract and accepts reusable `SearchContext` scratch
storage. The context is cleared at the beginning of each search without releasing capacity;
`SearchContext::with_capacity` preallocates for an expected match count.

## Route selection

Queries with fewer than 16,384 total postings use exhaustive scoring; queries with 16,384 or
more use Block-Max WAND: cursors are ordered by current document ID, suffix bounds identify a pivot, block
bounds skip noncompetitive regions, and surviving candidates receive exact scores. Equality with
the current threshold remains competitive so exact ties are retained for deterministic ordering.

## SIMD dispatch and precision

The exhaustive path selects its scorer once per term. AArch64 uses four-wide NEON (baseline for
ARMv8-A); x86_64 selects AVX2+FMA, AVX2, or scalar after runtime feature detection; other targets
use scalar code. Unsafe helpers accept fixed-size arrays, and callers establish the matching CPU
feature before invocation.

FMA performs one rounding where multiply-plus-add performs two, so parity tests allow a small
floating-point tolerance. Scores cross the public boundary as `DeterministicScore`, giving stable
comparison and serialization even when intermediate values differ by a few ULPs.

## Concurrency and exact fallback

Search takes `&self`. IDF and block-max caches use interior locks, permitting concurrent readers;
rebuilds double-check epochs under the write lock. `search_brute_force` exposes exhaustive scoring
for validation and diagnostics with the same ordering and truncation rules.
