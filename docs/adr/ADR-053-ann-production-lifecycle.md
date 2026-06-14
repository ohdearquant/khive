# ADR-053: ANN Production Lifecycle (SQ8 + Tombstone + Consolidation)

**Status**: Proposed
**Date**: 2026-06-13
**Origin**: khivedb ADR-107/108 (salvage)

## Context

khive's HNSW and Vamana indexes are batch-built and read-only. Every restart
requires a full O(N^2) rebuild. There is no delete operation, no quantization,
and no incremental insert. This is a research prototype, not a production
index.

khivedb developed production-grade improvements across both index crates:

- **SQ8 two-tier quantization** (ADR-107): approximate distances during
  beam-search acquisition, exact f32 for final neighbor selection. Measured
  1.27-1.41x build speedup at 100K vectors with no recall degradation.
- **Tombstone deletion + consolidation** (ADR-108): soft-delete with
  Wolverine 2-hop edge repair, compaction with old-to-new ID remapping.
  Enables O(1) restart via mmap instead of O(N^2) rebuild.
- **Incremental inserts** in Vamana: add vectors without full rebuild.

Head-to-head comparison: khivedb-hnsw is +292 LOC over khive-hnsw (3%),
justified by SQ8 batch build. khivedb-vamana is +1596 LOC (51%), justified
by tombstone/consolidation/insert lifecycle.

## Decision

Port khivedb's HNSW and Vamana improvements into khive's existing crates.

### HNSW (khive-hnsw)

1. **SQ8 build path**: `build_batch_sq8()` with codec training on full corpus.
   INT8 distances during parallel neighbor acquisition, f32 rerank before
   insertion.
2. **SQ8 search layer**: `search_layer_sq8()` for quantized candidate
   filtering during graph traversal.
3. **Remove checkpoint feature gate**: snapshots are always available (no
   khive-fold dependency for persistence).

### Vamana (khive-vamana)

1. **SQ8 two-tier**: global-scale codec (GsSq8Codec) for greedy search. Pure
   integer kernel: 12.8ns vs 214ns f32 at 384d (16.8x).
2. **Tombstone deletion**: `tombstone(node_id)` with soft-delete bitset.
3. **Consolidation**: `consolidate()` compacts vectors, rewires edges with
   old-to-new remapping, reclaims space.
4. **Incremental insert**: `insert(vector)` without full rebuild.
5. **Binary persistence**: save/load with CorpusFingerprint matching.

### New crate: khive-quant

Port khivedb-quant (Apache-2.0) as khive-quant:

- `Sq8Codec`: per-dim affine quantization (dot/cosine).
- `GsSq8Codec`: global-scale codec (L2, Vamana acquisition).
- NEON + portable fallback dispatch.

## Consequences

- Vamana index survives process restarts without rebuild (mmap).
- Delete operations become possible (tombstone + periodic consolidation).
- Build time improves 1.3-1.4x at 100K+ vectors via SQ8.
- Memory usage drops ~4x for distance comparisons during search (u8 vs f32).
- New khive-quant crate adds ~800 LOC but eliminates the "index demo, not
  database" critique.
