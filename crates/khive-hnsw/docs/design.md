# khive-hnsw Design

## ADR Compliance

### ADR-002: SIMD Foundation Layer

- Distance computation (`src/distance.rs`) delegates all vector math to `lattice-embed::simd`
- Cosine, Dot, and L2 metrics use NEON/AVX2/AVX-512 dispatch from the lattice-embed crate
- INT8 dot product (`int8_dot_product_raw`) also routes through `lattice_embed::simd::dot_product_i8_raw`
- The HNSW crate itself contains no SIMD code; it is purely algorithmic

### ADR-003: HNSW Index Management Strategy

- Default parameters: M=20, ef_construction=200, ef_search=80, dimensions=384
  - M=20 is empirically optimal for k=10 recall at 384d
  - ef_search=80 is sufficient for corpora under 100K vectors
- `DEFAULT_REBUILD_THRESHOLD = 0.10` (10% tombstone ratio triggers rebuild recommendation)
  - Below 10%: search quality impact is negligible
  - Above 10%: recall degrades measurably; rebuild restores full quality
- Tombstone-based lazy deletion (never immediate node removal) preserves graph connectivity
- Periodic rebuild via `HnswIndex::rebuild()` compacts the graph and removes orphaned edges
- `HnswConfig` is validated at construction time; invalid configs (dimensions=0, ml≤0, etc.) are rejected
- Named presets: `high_recall()`, `fast_build()`, `low_memory()` cover standard deployment profiles

## Consistency Notes

- The `sort_ids` function in `checkpoint/ckpt_config.rs` sorts by raw `NodeId` byte order.
  This differs from lexicographic hex string ordering, which matters for snapshot determinism.
- `HnswSnapshot::is_canonical()` / `canonicalize()` enforce byte-order sorting before serialization.
- The `is_zero` serde helper in `checkpoint/snapshot.rs` suppresses the legacy `vector_count`
  field in v2 snapshots. On deserialization, `normalize()` must be called to populate the v2
  fields from `vector_count` if present.
- The `metric_to_string` function in `checkpoint/ckpt_config.rs` maps `DistanceMetric::L2`
  to the string `"euclidean"` for checkpoint compatibility (the enum variant was renamed from
  `Euclidean` to `L2` during a refactor; the checkpoint string remains `"euclidean"`).
