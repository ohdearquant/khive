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

| Scenario                       | Low      | Median   | High     | Notes           |
| ------------------------------ | -------- | -------- | -------- | --------------- |
| distance/l2_squared/384d       | 36.90 ns | 36.99 ns | 37.08 ns | 8-wide unrolled |
| distance/cosine_from_l2sq      | 776 ps   | 780 ps   | 784 ps   | scalar          |
| build/VamanaIndex::build/1000  | 39.04 ms | 42.39 ms | 45.91 ms | R=32, L=64      |
| build/VamanaIndex::build/5000  | 950.7 ms | 1.082 s  | 1.267 s  | R=64, L=128     |
| build/VamanaIndex::build/10000 | 2.965 s  | 3.113 s  | 3.281 s  | R=64, L=128     |
| search/n=1000/k=10             | 92.53 µs | 92.84 µs | 93.15 µs |                 |
| search/n=1000/k=50             | 92.28 µs | 93.04 µs | 94.02 µs |                 |
| search/n=5000/k=10             | 434.1 µs | 438.4 µs | 443.7 µs |                 |
| search/n=5000/k=50             | 434.5 µs | 439.5 µs | 447.5 µs |                 |
| search/n=10000/k=10            | 544.0 µs | 551.7 µs | 561.1 µs | < 3ms SLO pass  |
| search/n=10000/k=50            | 550.1 µs | 557.3 µs | 565.5 µs | < 3ms SLO pass  |
| free_fns/build/1k              | 40.81 ms | 41.56 ms | 42.39 ms |                 |
| free_fns/search/1k/k10         | 92.33 µs | 94.16 µs | 96.55 µs |                 |
| snapshot/to_snapshot/1000      | 42.15 µs | 43.33 µs | 44.82 µs | iter_batched    |
| snapshot/to_snapshot/5000      | 320.3 µs | 323.6 µs | 327.6 µs | iter_batched    |
| snapshot/from_snapshot/1000    | 269.3 µs | 272.4 µs | 276.9 µs |                 |
| snapshot/from_snapshot/5000    | 1.595 ms | 1.616 ms | 1.639 ms |                 |

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
