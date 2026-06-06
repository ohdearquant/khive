# khive-fusion Benchmark Ledger

## Benchmark Suite: `fusion_bench`

**Source:** `crates/khive-fusion/benches/fusion_bench.rs`
**Harness:** Criterion 0.5 (`harness = false`)

## Run Command

```bash
# From workspace root
cargo bench -p khive-fusion --bench fusion_bench

# With HTML report (requires gnuplot)
cargo bench -p khive-fusion --bench fusion_bench -- --output-format bencher

# Single group only
cargo bench -p khive-fusion --bench fusion_bench -- rrf
```

## Benchmark Groups

| Group | What it measures | Scenarios |
|-------|-----------------|-----------|
| `rrf` | `reciprocal_rank_fusion` throughput | (2 src, 50/150/500 items), (3 src, 150/500 items) |
| `weighted` | `weighted_fusion` throughput | same matrix as rrf |
| `union` | `union_fusion` throughput | same matrix as rrf |
| `fuse_dispatcher` | `fuse()` dispatch overhead per strategy | all 5 strategies + top_k sensitivity at k=10/50/100 |
| `weight_utils` | `normalize_weights`, `weights_are_normalized` | 2/3/20-element weight vectors |

## Dataset Shape

Sources are generated deterministically via a linear congruential generator (seed 42).
Each source has 30% overlap IDs across sources to exercise the HashMap merge path.

Input IDs are `u64`; scores are `DeterministicScore` derived from LCG output in `[0, 1)`.

## Timing Methodology Note

Clone cost is intentionally included in the measured path (sources are cloned inside
`b.iter`). This reflects the real call site where `fuse()` takes ownership of source
vectors. The benchmark measures allocation + fusion together to give a realistic end-to-end
number. If algorithm-only timing is needed in future, use `iter_batched` to pre-clone
outside the measured path and document the change here.

## Environment Fields

| Field | Value to record at each baseline run |
|-------|--------------------------------------|
| Rust toolchain | (e.g., `rustc 1.78.0 (9b00956e5 2024-04-29)`) |
| Machine | (e.g., Apple M4 Pro, 24 GB) |
| OS | (e.g., macOS 15.5) |
| Criterion sample size | 50 (rrf/weighted/union/dispatcher), 200 (weight_utils) |

## Regression Policy

A >10% wall-time regression in `rrf` or `weighted` at the 2×150-item scenario
requires a comment in the PR explaining the cause before merge.

## Baseline Table

| Scenario | Metric | Baseline | Date | Commit | Machine |
|----------|--------|----------|------|--------|---------|
| rrf/2src/150items | time/iter | (not yet recorded) | — | — | — |
| weighted/2src/150items | time/iter | (not yet recorded) | — | — | — |
| union/2src/150items | time/iter | (not yet recorded) | — | — | — |
| fuse_dispatcher/Rrf | time/iter | (not yet recorded) | — | — | — |
| fuse_dispatcher/Weighted | time/iter | (not yet recorded) | — | — | — |
