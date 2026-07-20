# khive-hnsw

HNSW (Hierarchical Navigable Small World) vector index with INT8 quantized
two-phase search, tombstone-based deletion, and snapshot persistence.

## Features

- **Incremental insert/delete** — lazy tombstone deletion; `rebuild()` compacts
  storage and repairs the graph once the tombstone ratio crosses a threshold
- **INT8 quantized two-phase search** — an INT8 pre-filter screens candidates,
  full `f32` distance ranks the survivors; toggle at runtime with no rebuild cost
- **Exact-scan fallback** — indexes at or below ~3K nodes use brute-force SIMD
  scanning, which beats graph traversal at that scale for Cosine/Dot metrics
- **Deterministic scoring** via `khive-score`'s fixed-point `DeterministicScore`
- **Snapshot persistence** — `snapshot()` / `restore_from_snapshot[_embedded]`
  for warm-start restores; `checkpoint` feature adds `HnswCheckpointStore`
  integration with `khive-fold`
- **Memory budgets** — cap heap usage per index with configurable limits
- **Metrics** — optional `MetricsSink` for insert/search/rebuild observability

## Usage

```rust
use khive_hnsw::{HnswConfig, HnswIndex, NodeId};

let mut index = HnswIndex::with_config(HnswConfig::with_dimensions(384));

index.insert(NodeId::new([1u8; 16]), vec![0.1_f32; 384]).unwrap();

let results = index.search(&vec![0.1_f32; 384], 10).unwrap();
for (id, score) in &results {
    println!("{id}: {}", score.to_f64());
}

index.delete(NodeId::new([1u8; 16]));
if index.needs_rebuild() {
    index.rebuild();
}
```

`HnswIndex::new(dimensions)` builds with defaults; `try_with_config` returns a
`Result` instead of panicking on an invalid `HnswConfig` (useful when the config
comes from deserialized/external input).

## Configuration

| Parameter           | Default  | Description                                             |
| ------------------- | -------- | ------------------------------------------------------- |
| `m`                 | 20       | Max connections per node per layer.                     |
| `m_max0`            | 40       | Max connections at layer 0 (typically `2*m`).           |
| `ef_construction`   | 200      | Candidate list size during insert.                      |
| `ef_search`         | 80       | Candidate list size during search.                      |
| `dimensions`        | 384      | Vector width; must match the embedding model.           |
| `metric`            | `Cosine` | `Cosine`, `Dot`, or `L2`.                               |
| `rebuild_threshold` | 0.10     | Tombstone ratio above which `rebuild()` is recommended. |
| `seed`              | `None`   | Seeded RNG for reproducible level assignment.           |
| `memory_budget`     | `None`   | Byte cap; inserts over budget return `BudgetExceeded`.  |

`HnswConfig::validate()` runs on every deserialization and rejects zero `m`,
zero `ef_construction`/`ef_search`, `m_max0 < m`, non-finite `ml`, and an
out-of-range `rebuild_threshold`. Presets `HnswConfig::high_recall()`,
`fast_build()`, and `low_memory()` cover the common op-point trade-offs.

## Where this sits

`khive-hnsw` is the vector-search engine consumed by `khive-retrieval`
(`https://crates.io/crates/khive-retrieval`), which composes it with
`khive-bm25` keyword search and `khive-fusion` rank fusion into the hybrid
retrieval layer. It depends only on `khive-score` (deterministic scoring),
`khive-types` (shared `DistanceMetric`), and `lattice-embed` (SIMD distance
kernels); the optional `checkpoint` feature adds a `khive-fold` dependency for
persisted snapshots.

Governing ADRs:
[ADR-052](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-052-ann-production-lifecycle.md)
(SQ8 quantization, tombstone delete, consolidation, crash-safe persistence across
both ANN engines) and
[ADR-079](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-079-ann-persistence-warm-path-integration.md)
(wiring persisted snapshots into the daemon warm path).

## License

BUSL-1.1. See the repository [LICENSE](https://github.com/ohdearquant/khive/blob/main/LICENSE).
