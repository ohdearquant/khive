# Vamana Benchmark Ledger

**Crate:** `khive-vamana`
**ADR refs:** ADR-048 (recall and latency targets)

---

## Benchmark targets (`benches/vamana_bench.rs`)

| Target                           | Group    | Description                                          |
| -------------------------------- | -------- | ---------------------------------------------------- |
| `distance/l2_squared/384d`       | distance | Throughput of 8-wide unrolled L2² on 384-dim vectors |
| `distance/cosine_from_l2sq`      | distance | Scalar cosine conversion                             |
| `build/VamanaIndex::build/1000`  | build    | Full build, N=1000, DIM=384, R=32                    |
| `build/VamanaIndex::build/5000`  | build    | Full build, N=5000, DIM=384, R=64                    |
| `build/VamanaIndex::build/10000` | build    | Full build, N=10000, DIM=384, R=64                   |
| `search/n=1000/k=10`             | search   | Single-query search latency                          |
| `search/n=1000/k=50`             | search   | Single-query search latency                          |
| `search/n=5000/k=10`             | search   | Single-query search latency                          |
| `search/n=5000/k=50`             | search   | Single-query search latency                          |
| `search/n=10000/k=10`            | search   | Single-query search latency                          |
| `search/n=10000/k=50`            | search   | Single-query search latency                          |
| `free_fns/build/1k`              | free_fns | `khive_vamana::build` free function                  |
| `free_fns/search/1k/k10`         | free_fns | `khive_vamana::search` free function                 |
| `snapshot/to_snapshot/1000`      | snapshot | Snapshot serialization, N=1000                       |
| `snapshot/to_snapshot/5000`      | snapshot | Snapshot serialization, N=5000                       |
| `snapshot/from_snapshot/1000`    | snapshot | Snapshot restore, N=1000                             |
| `snapshot/from_snapshot/5000`    | snapshot | Snapshot restore, N=5000                             |

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

| Scenario                   | Baseline | Date | Commit | Machine | Notes                                         |
| -------------------------- | -------- | ---- | ------ | ------- | --------------------------------------------- |
| (no baseline recorded yet) | --       | --   | --     | --      | First ledger entry pending a CI benchmark run |

> **Note:** Baselines need measuring. Run
> `cargo bench -p khive-vamana --bench vamana_bench` on a consistent machine and
> record the results here before the next release.

---

## ADR-048 pass criteria

- `recall@10 >= 0.80` for N=1000×384 (integration test, always runs)
- `recall@10 >= 0.85` for N=5000×384 (ignored; run manually)
- Single-query search latency target: < 3 ms at N=10k (from perf/recall-fts SLO)
