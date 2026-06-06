# khive-score Benchmark Ledger

**Bench target:** `crates/khive-score/benches/score_ops.rs`
**Harness:** Criterion (`harness = false` in `Cargo.toml`)

## Run command

```bash
cargo bench -p khive-score --bench score_ops
```

Results land in `target/criterion/score_ops/`.

## Benchmark suite

| Scenario | What it measures |
| -------- | ---------------- |
| `distance_cosine` | `score_from_distance_lossy` with cosine metric, 1000 samples |
| `distance_l2` | `score_from_distance_lossy` with L2 metric, 1000 samples |
| `distance_dot` | `score_from_distance_lossy` with dot-product metric, 1000 samples |
| `try_distance_cosine` | `try_score_from_distance` with cosine metric, valid inputs |
| `sum_scores_100` | `sum_scores` over 100-element slice |
| `avg_scores_100` | `avg_scores` over 100-element slice |
| `rrf_score_k60` | `rrf_score_one_based` at k=60, ranks 1–100 |
| `weighted_sum_10` | `weighted_sum` over 10 scores/weights |
| `ranked_heap_1000` | `BinaryHeap<Ranked<u64>>` push+pop, 1000 items |

## Environment notes

- Run on a quiet machine (no background load) for reproducible results.
- Pin CPU frequency if available: `sudo cpupower frequency-set -g performance` (Linux).
- Criterion warms up for 3 seconds by default; increase via `Criterion::warm_up_time` if
  variance is high.
- Record toolchain (`rustc --version`) alongside results.

## Baseline table

| Scenario | Baseline (ns/iter) | Date | Commit | Machine |
| -------- | ------------------ | ---- | ------ | ------- |
| _(not yet measured)_ | — | — | — | — |

Last reviewed: 2026-06-06 (v0.2.3)
