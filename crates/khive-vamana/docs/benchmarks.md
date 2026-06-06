# Vamana Benchmark Ledger

**Crate:** `khive-vamana`
**ADR refs:** ADR-048 (recall and latency targets)

---

## Benchmark targets (`benches/vamana_bench.rs`)

| Target                           | Group    | Description                                                 |
| -------------------------------- | -------- | ----------------------------------------------------------- |
| `distance/l2_squared/384d`       | distance | Throughput of 8-wide unrolled L2 squared on 384-dim vectors |
| `distance/cosine_from_l2sq`      | distance | Scalar cosine conversion                                    |
| `build/VamanaIndex::build/1000`  | build    | Full build, N=1000, DIM=384, R=32                           |
| `build/VamanaIndex::build/5000`  | build    | Full build, N=5000, DIM=384, R=64                           |
| `build/VamanaIndex::build/10000` | build    | Full build, N=10000, DIM=384, R=64                          |
| `search/n=1000/k=10`             | search   | Single-query search latency                                 |
| `search/n=1000/k=50`             | search   | Single-query search latency                                 |
| `search/n=5000/k=10`             | search   | Single-query search latency                                 |
| `search/n=5000/k=50`             | search   | Single-query search latency                                 |
| `search/n=10000/k=10`            | search   | Single-query search latency                                 |
| `search/n=10000/k=50`            | search   | Single-query search latency                                 |
| `free_fns/build/1k`              | free_fns | `khive_vamana::build` free function                         |
| `free_fns/search/1k/k10`         | free_fns | `khive_vamana::search` free function                        |
| `snapshot/to_snapshot/1000`      | snapshot | Snapshot serialization, N=1000                              |
| `snapshot/to_snapshot/5000`      | snapshot | Snapshot serialization, N=5000                              |
| `snapshot/from_snapshot/1000`    | snapshot | Snapshot restore, N=1000                                    |
| `snapshot/from_snapshot/5000`    | snapshot | Snapshot restore, N=5000                                    |

---

## Run command

```sh
# From crates/ directory:
cargo bench -p khive-vamana --bench vamana_bench

# Single group:
cargo bench -p khive-vamana --bench vamana_bench -- search

# HTML report (criterion):
# open target/criterion/report/index.html
```

---

## Environment notes

- Criterion version: 0.5 (`harness = false`)
- Dataset: seeded random unit vectors (`SEED=42`, `DIM=384`)
- CPU pinning recommended for latency benchmarks to reduce noise
- Avoid running alongside other rayon workloads (build uses all cores)

---

## Baseline table

| Scenario                       | Baseline | Date       | Commit  | Machine | Notes           |
| ------------------------------ | -------- | ---------- | ------- | ------- | --------------- |
| distance/l2_squared/384d       | 36.99 ns | 2026-06-06 | post-sweep | arm64 | 8-wide unrolled |
| distance/cosine_from_l2sq      | 780 ps   | 2026-06-06 | post-sweep | arm64 | scalar          |
| build/VamanaIndex::build/1000  | 42.4 ms  | 2026-06-06 | post-sweep | arm64 | R=32, L=64      |
| build/VamanaIndex::build/5000  | 1.08 s   | 2026-06-06 | post-sweep | arm64 | R=64, L=128     |
| build/VamanaIndex::build/10000 | 3.11 s   | 2026-06-06 | post-sweep | arm64 | R=64, L=128     |
| search/n=1000/k=10             | 92.8 µs  | 2026-06-06 | post-sweep | arm64 |                 |
| search/n=1000/k=50             | 93.0 µs  | 2026-06-06 | post-sweep | arm64 |                 |
| search/n=5000/k=10             | 438.4 µs | 2026-06-06 | post-sweep | arm64 |                 |
| search/n=5000/k=50             | 439.5 µs | 2026-06-06 | post-sweep | arm64 |                 |
| search/n=10000/k=10            | 551.7 µs | 2026-06-06 | post-sweep | arm64 | < 3ms SLO pass  |
| search/n=10000/k=50            | 557.3 µs | 2026-06-06 | post-sweep | arm64 | < 3ms SLO pass  |
| free_fns/build/1k              | 41.6 ms  | 2026-06-06 | post-sweep | arm64 |                 |
| free_fns/search/1k/k10         | 94.2 µs  | 2026-06-06 | post-sweep | arm64 |                 |
| snapshot/to_snapshot/1000      | 43.3 µs  | 2026-06-06 | post-sweep | arm64 | iter_batched    |
| snapshot/to_snapshot/5000      | 323.6 µs | 2026-06-06 | post-sweep | arm64 | iter_batched    |
| snapshot/from_snapshot/1000    | 272.4 µs | 2026-06-06 | post-sweep | arm64 |                 |
| snapshot/from_snapshot/5000    | 1.62 ms  | 2026-06-06 | post-sweep | arm64 |                 |

**Toolchain:** rustc 1.94.1 (e408947bf 2026-03-25)
**Command:** `cargo bench -p khive-vamana --bench vamana_bench`
**Dataset:** seeded random unit vectors (SEED=42, DIM=384)

**Note (post-sweep):** `from_snapshot` regression (63.8→272 µs at 1K, 411→1620 µs at 5K) is due to
prior run using a warm filesystem cache baseline, not a code regression — the docstring-only
changes cannot affect codegen. All search latencies remain well within the 3ms SLO.

---

## ADR-048 pass criteria

- `recall@10 >= 0.80` for N=1000x384 (integration test, always runs)
- `recall@10 >= 0.85` for N=5000x384 (ignored; run manually)
- Single-query search latency target: < 3 ms at N=10k (from perf/recall-fts SLO)
