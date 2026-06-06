# khive-bm25 Design

## ADR Compliance

### ADR-003: BM25 Configuration Defaults

- This crate implements the BM25 (Okapi BM25) keyword index with the Robertson-Walker IDF variant.
- Default parameters: `k1 = 1.2` (term saturation), `b = 0.75` (length normalization).
- These defaults reflect the canonical IR literature recommendations and are validated on
  construction — invalid (non-finite, negative `k1`, or out-of-range `b`) values are rejected.
- A memory budget (`Bm25Config::memory_budget`) is optional; when set, `index_document` rejects
  insertions that would exceed the limit while bypassing the check for re-indexing existing docs.

## Consistency Notes

- The `search/mod.rs` file exceeds 700 lines (including ~334 lines of inline SIMD parity tests).
  The tests require `pub(super)` access to the simd scoring helpers and are co-located by design.
  Production code is ~714 lines — marginally over the 700-line target but documented in-file.
- No code-vs-ADR discrepancies were found for ADR-003 during the June 2026 sweep.
