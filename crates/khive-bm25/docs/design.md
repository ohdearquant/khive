# khive-bm25 Design

## ADR Compliance

### BM25 Configuration Defaults (ADR-030)

- This crate implements the BM25 (Okapi BM25) keyword index with the Robertson-Walker IDF
  variant, ported as part of the `khive-retrieval` stack (ADR-030: Retrieval Stack Port).
- Default parameters: `k1 = 1.2` (term saturation), `b = 0.75` (length normalization).
- These defaults reflect the canonical IR literature recommendations and are validated on
  construction — invalid (non-finite, negative `k1`, or out-of-range `b`) values are rejected.
- A memory budget (`Bm25Config::memory_budget`) is optional; when set, `index_document` rejects
  insertions that would exceed the limit while bypassing the check for re-indexing existing docs.

## Module Structure

The `search/` module is split into focused files:

- `mod.rs` — re-exports only
- `engine.rs` — BMW search engine and brute-force fallback
- `context.rs` — search context and block scoring
- `idf.rs` — IDF computation
- `simd.rs` — NEON/SSE2 SIMD scoring
- `helpers.rs` — posting list utilities
