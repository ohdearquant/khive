# khive-vamana Design

## ADR Compliance

### ADR-048: Vamana ANN Engine

This crate implements the Vamana ANN index as the knowledge-pack approximate
nearest-neighbor engine.

Key design decisions and constraints:

- **Production defaults**: `dimensions=384`, `max_degree=64`,
  `search_list_size=128`, `alpha=1.2`. These values are the defaults for
  `VamanaConfig` and are validated by the integration test
  `default_matches_adr048_values`.

- **Snapshot validation**: Every `VamanaSnapshot` carries a `CorpusFingerprint`
  (`vector_count`, `dimensions`) that must match the live embedding store before
  the snapshot is installed into memory. A fingerprint mismatch causes a silent
  rebuild. `kkernel reindex` actively deletes stale snapshots as a second line
  of defence.

- **Recall and latency targets**: `recall@10 >= 0.80` for N=1000×384 (CI);
  `recall@10 >= 0.85` for N=5000×384 (manual); single-query search latency
  target < 3 ms at N=10k.

- **Non-finite float rejection**: `NaN` and `Infinity` are rejected at every
  public boundary (`build`, `search`, `from_snapshot`) before entering graph
  construction or distance computation.

- **Unit normalization contract**: All vectors must be unit-normalized before
  insertion. Dimensionality is validated at every public boundary; unit-norm is
  not enforced (the adjacent bridge normalizes before calling here).

## Consistency Notes

- The `Vec<Vec<u32>>` adjacency layout is intentional for build-phase pruning
  flexibility. A CSR flat layout would improve memory locality and serialization
  size; migration is tracked in `docs/persistence.md` for when N > 1M or
  mmap-graph streaming is needed.

- The single `unsafe` block in `mmap_vectors` maps `vectors.bin` read-only.
  The contract: callers must not mutate or truncate the file while the index
  is live.
