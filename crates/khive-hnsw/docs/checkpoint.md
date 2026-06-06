# HNSW Checkpointing

## Architecture

`HnswSnapshot` and `HnswCheckpointConfig` are always available and carry no extra dependencies
— they are plain serializable data.

When the `checkpoint` feature is enabled, the module also provides type aliases that integrate
with `khive-fold`'s `Checkpoint` and `InMemoryCheckpointStore` for a complete checkpoint
lifecycle.

```text
HnswIndex ──snapshot──> HnswSnapshot ──wrap──> Checkpoint<HnswSnapshot>
                                                      │
                                        CheckpointStore::save(...)
```

## Tombstone Tracking

Snapshots track both live and tombstoned nodes for accurate restore:

- `total_nodes`: All nodes (live + tombstoned)
- `live_nodes`: Non-tombstoned nodes only
- `tombstone_count`: Number of tombstoned nodes
- `tombstoned_ids`: IDs of tombstoned vectors for restore

Invariant: $\text{total\_nodes} = \text{live\_nodes} + \text{tombstone\_count}$, enforced by `HnswSnapshot::verify`.

## Determinism

All ID lists (`indexed_ids`, `tombstoned_ids`) and layer node entries are stored in sorted
order by `NodeId` bytes to ensure deterministic snapshots across runs. This is critical for:

- Reproducible checkpoint hashes
- Stable index-based encoding (e.g., tombstone bitsets)
- Test reproducibility

Use `HnswSnapshot::canonicalize` before serialization, or `HnswSnapshot::is_canonical` to
verify ordering.
