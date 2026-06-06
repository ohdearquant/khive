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
| distance/l2_squared/384d       | 37.3 ns  | 2026-06-06 | ca7d72d | arm64   | 8-wide unrolled |
| distance/cosine_from_l2sq      | 786 ps   | 2026-06-06 | ca7d72d | arm64   | scalar          |
| build/VamanaIndex::build/1000  | 41.8 ms  | 2026-06-06 | ca7d72d | arm64   | R=32, L=64      |
| build/VamanaIndex::build/5000  | 965 ms   | 2026-06-06 | ca7d72d | arm64   | R=64, L=128     |
| build/VamanaIndex::build/10000 | 3.69 s   | 2026-06-06 | ca7d72d | arm64   | R=64, L=128     |
| search/n=1000/k=10             | 94.7 us  | 2026-06-06 | ca7d72d | arm64   |                 |
| search/n=1000/k=50             | 96.3 us  | 2026-06-06 | ca7d72d | arm64   |                 |
| search/n=5000/k=10             | 446.9 us | 2026-06-06 | ca7d72d | arm64   |                 |
| search/n=5000/k=50             | 440.5 us | 2026-06-06 | ca7d72d | arm64   |                 |
| search/n=10000/k=10            | 563.0 us | 2026-06-06 | ca7d72d | arm64   | < 3ms SLO pass  |
| search/n=10000/k=50            | 599.2 us | 2026-06-06 | ca7d72d | arm64   | < 3ms SLO pass  |
| free_fns/build/1k              | 48.8 ms  | 2026-06-06 | ca7d72d | arm64   |                 |
| free_fns/search/1k/k10         | 91.9 us  | 2026-06-06 | ca7d72d | arm64   |                 |
| snapshot/to_snapshot/1000      | 42.6 us  | 2026-06-06 | ca7d72d | arm64   | iter_batched    |
| snapshot/to_snapshot/5000      | 337.9 us | 2026-06-06 | ca7d72d | arm64   | iter_batched    |
| snapshot/from_snapshot/1000    | 63.8 us  | 2026-06-06 | ca7d72d | arm64   |                 |
| snapshot/from_snapshot/5000    | 410.7 us | 2026-06-06 | ca7d72d | arm64   |                 |

**Toolchain:** rustc 1.94.1 (e408947bf 2026-03-25)
**Command:** `cargo bench -p khive-vamana --bench vamana_bench`
**Dataset:** seeded random unit vectors (SEED=42, DIM=384)

---

## ADR-048 pass criteria

- `recall@10 >= 0.80` for N=1000x384 (integration test, always runs)
- `recall@10 >= 0.85` for N=5000x384 (ignored; run manually)
- Single-query search latency target: < 3 ms at N=10k (from perf/recall-fts SLO)
