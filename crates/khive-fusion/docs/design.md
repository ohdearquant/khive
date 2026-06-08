# khive-fusion Design

## ADR Compliance

### ADR-006: Deterministic Scoring — RRF rank indexing convention

- The RRF implementation uses 1-indexed ranks throughout. Position 0 in an input list maps to
  rank 1 in the RRF formula, consistent with the `rrf_score` function in `khive-score`
  (ADR-006: Deterministic Scoring).

### ADR-012: Retrieval Composition

- This crate provides the fusion layer that combines ranked result lists from multiple retrieval
  sources. It is consumed by higher-level retrieval pipelines to aggregate dense (vector) and
  lexical (BM25) signals into a single ranked output.

### ADR-030: Hybrid Retrieval

- The five `FusionStrategy` variants (RRF, Weighted, Union, VectorOnly, KeywordOnly) implement
  the hybrid retrieval strategy options specified for the system. RRF is the default as it is
  robust to score distribution differences between sources.

### ADR-031: Multi-Engine Retrieval

- The `fuse()` dispatcher accepts results from any number of retrieval sources and routes them
  through the appropriate strategy. VectorOnly and KeywordOnly are single-source passthrough
  strategies — supplying multiple sources for these returns an empty vector so wiring errors are
  detectable without panicking.

## Consistency Notes

- The RETRIEVAL-07 constraint (weight normalization) is implemented in `weighted_fusion` and
  documented in `docs/algorithm.md`. Weights are sanitized (non-finite → 0.0, negative → 0.0)
  before normalization; if all effective weights are zero, equal distribution is applied.
- Per-source min-max normalization (issues #2496/#2639) ensures BM25 unbounded scores and cosine
  similarity [0, 1] scores contribute proportionally to their configured weights after fusion.
