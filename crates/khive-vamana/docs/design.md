# khive-vamana Design

**Scope:** In-process Vamana ANN index for batch-built approximate nearest
neighbor search over pre-normalized vectors. Used by the knowledge pack as the
graph-based retrieval engine (ADR-048).

**ADR refs:**

- [ADR-048: Knowledge Section Profiles](../../../docs/adr/ADR-048-knowledge-section-profiles.md)
  -- recall/latency targets, snapshot validation, production defaults
- [ADR-009: Backend Architecture](../../../docs/adr/ADR-009-backend-architecture.md)
  -- in-process retrieval engine boundary
- [ADR-012: Retrieval Composition](../../../docs/adr/ADR-012-retrieval-composition.md)
  -- hybrid retrieval stack integration
- [ADR-030: Retrieval Stack Port](../../../docs/adr/ADR-030-retrieval-stack-port.md)
  -- Rust retrieval layer

**Primary modules:**

- [`src/config.rs`](../src/config.rs) -- `VamanaConfig` algorithm parameters
- [`src/graph.rs`](../src/graph.rs) -- graph construction, greedy search, robust
  prune
- [`src/index.rs`](../src/index.rs) -- build, search, save/load, snapshot
  serialization
- [`src/distance.rs`](../src/distance.rs) -- L2 squared distance kernel
- [`src/error.rs`](../src/error.rs) -- error types

**Tests:**

- [`src/config.rs`](../src/config.rs) -- inline config validation tests
- [`src/graph.rs`](../src/graph.rs) -- inline graph construction and search tests
- [`src/index.rs`](../src/index.rs) -- inline persistence and snapshot tests
- [`tests/benchmark.rs`](../tests/benchmark.rs) -- integration recall tests

**Benchmarks:**

- [`benches/vamana_bench.rs`](../benches/vamana_bench.rs) -- Criterion benches
  for distance, build, search, free functions, and snapshot round-trip

**Related docs:**

- [algorithm.md](algorithm.md) -- build/search/prune algorithm details
- [benchmarks.md](benchmarks.md) -- benchmark ledger and ADR-048 pass criteria
- [persistence.md](persistence.md) -- binary file format and snapshot contract
- [testing.md](testing.md) -- test organization and adversarial invariants

---

## ADR Compliance

### ADR-048: Knowledge Section Profiles - Vamana ANN Engine

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
  rebuild. `kkernel reindex` actively deletes stale snapshots as a second line of
  defence.

- **Recall and latency targets**: `recall@10 >= 0.80` for N=1000x384 (CI);
  `recall@10 >= 0.85` for N=5000x384 (manual); single-query search latency
  target < 3 ms at N=10k.

- **Non-finite float rejection**: `NaN` and `Infinity` are rejected at every
  public boundary (`build`, `search`, `from_snapshot`) before entering graph
  construction or distance computation.

- **Unit normalization contract**: All vectors must be unit-normalized before
  insertion. Dimensionality is validated at every public boundary; unit-norm is
  not enforced (the adjacent bridge normalizes before calling here).

---

## Invariants and Failure Modes

**Invariants:**

- No self-loops in adjacency lists (enforced during build and snapshot restore).
- No duplicate neighbors per node (enforced by `sort_dedup_u32` and validation).
- Degree bound: `adjacency[i].len() <= max_degree` after build and load.
- Deterministic output: seeded RNG + deterministic sort produces identical graphs
  for identical inputs.
- All vector values must be finite at every public boundary.

**Failure modes:**

- `VamanaError::EmptyInput` -- zero vectors or zero queries supplied.
- `VamanaError::DimensionMismatch` -- query/vector length does not match config.
- `VamanaError::InvalidConfig` -- invalid algorithm parameters (alpha < 1.0,
  search_list_size < max_degree, zero dimensions/degree).
- `VamanaError::InvalidFormat` -- corrupted binary file or snapshot (bad magic,
  truncated data, out-of-range neighbors, duplicate neighbors, self-loops).
- `VamanaError::NonFiniteFloat` -- NaN or Infinity in vectors or queries.
- `VamanaError::TooManyVectors` -- corpus exceeds u32 node-ID limit.
- `VamanaError::Io` -- file system errors during save/load.

---

## Consistency Notes

- The `Vec<Vec<u32>>` adjacency layout is intentional for build-phase pruning
  flexibility. A CSR flat layout would improve memory locality and serialization
  size; migration is tracked in `docs/persistence.md` for when N > 1M or
  mmap-graph streaming is needed.

- The single `unsafe` block in `mmap_vectors` maps `vectors.bin` read-only. The
  contract: callers must not mutate or truncate the file while the index is live.

---

Last reviewed: 2026-06-06
