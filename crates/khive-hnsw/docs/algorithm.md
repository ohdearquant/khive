# HNSW Algorithm: Design Notes

## Algorithm Overview

HNSW (Hierarchical Navigable Small World) builds a multi-layer proximity graph:

- **Higher layers** have fewer nodes (exponentially distributed via a level-select function)
- **Search** starts from the top layer's entry point and descends greedily
- **Each layer** uses greedy best-first search to find the nearest neighbors
- **Insertion** builds connections at each assigned level using the same greedy search

Reference: Malkov & Yashunin, "Efficient and robust approximate nearest neighbor search using
Hierarchical Navigable Small World graphs", IEEE TPAMI (2018).

## Complexity

- Insert: $O(\log N)$ amortized
- Search: $O(\log N)$ for the upper layers + $O(ef)$ for the base layer scan
- Space: $O(N \times M)$ where $M$ is the maximum connections per node

## Key Parameters (HnswConfig)

| Parameter         | Default | Effect                                                                   |
| ----------------- | ------- | ------------------------------------------------------------------------ |
| `m`               | 20      | Max connections per node per layer. Higher → better recall, more memory. |
| `m_max0`          | 40      | Max connections in layer 0 (base). Usually `2 × m`.                      |
| `ef_construction` | 200     | Search width during insert. Higher → better quality, slower build.       |
| `ef_search`       | 80      | Search width at query time. Trade-off: recall vs speed.                  |
| `dimensions`      | 384     | Vector dimensionality (must match embedding model).                      |

Defaults are tuned for `m=20` which is optimal for k=10 recall at 384d (empirically measured).
`ef_search=80` is sufficient for corpora under 100K vectors.

## Arena Design

`khive-hnsw` uses an arena allocator for node storage to avoid per-node heap allocation and
improve cache locality during graph traversal. All nodes for a given layer are stored in a
contiguous `Vec`, with inter-node references stored as integer indices rather than pointers.

See `src/arena/` and `docs/api/arena.md` for full design rationale.

## Tombstone Handling

Deleted nodes are marked with a tombstone rather than immediately removed from the graph.
Tombstoned nodes are skipped during search but their edges remain. Periodic rebuild
(triggered when tombstone ratio exceeds a threshold) compacts the graph and removes orphaned
edges. See `TombstoneStats` for monitoring.

## Checkpoint and Snapshot

`HnswSnapshot` provides a serializable point-in-time view of the index for persistence.
`HnswCheckpoint` stores the snapshot plus metadata for incremental recovery. Load a checkpoint
with `HnswCheckpointStore::load` after crash or restart.

See `docs/api/checkpoint.md` for tombstone tracking, determinism, and khive-fold integration.

## Tests and Benchmarks

- Integration tests: `tests/hnsw_tests.rs`
- Unit tests: inline `#[cfg(test)]` modules in each source file
- Benchmarks: `benches/hnsw_bench.rs` (Criterion); see `docs/benchmarks.md` for ledger
