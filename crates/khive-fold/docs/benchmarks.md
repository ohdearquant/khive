# khive-fold Benchmarks

<!-- bench-reference:begin -->

Generated from `perf/reference.json` by `scripts/render-bench-reference.py`; do not edit this block.

Reference baselines for `khive-fold`. Values are the committed reference median for each benchmark, in the unit shown; A/A is the ratio between the two same-commit runs. The date is the committer date of the baseline commit.

| Benchmark                       | Reference median | A/A ratio | Baseline commit                            | Commit date |
| ------------------------------- | ---------------- | --------- | ------------------------------------------ | ----------- |
| `fold/count/10`                 | 3.888 ns         | 1.000     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `fold/count/100`                | 45.190 ns        | 1.001     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `fold/count/500`                | 273.978 ns       | 1.000     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `fold/derive/10`                | 6.926 ns         | 1.000     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `fold/derive/100`               | 47.860 ns        | 1.001     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `fold/derive/500`               | 276.900 ns       | 1.000     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `fold/fn_closure/10`            | 2.826 ns         | 1.000     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `fold/fn_closure/100`           | 14.976 ns        | 1.000     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `fold/fn_closure/500`           | 71.768 ns        | 1.000     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `fold/sum_i64/10`               | 19.635 ns        | 1.014     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `fold/sum_i64/100`              | 173.057 ns       | 1.001     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `fold/sum_i64/500`              | 855.016 ns       | 1.000     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `objective/batch_score/10`      | 24.031 ns        | 1.019     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `objective/batch_score/100`     | 147.290 ns       | 1.002     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `objective/batch_score/500`     | 631.554 ns       | 1.001     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `objective/select_all/10`       | 231.538 ns       | 1.017     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `objective/select_all/100`      | 2.017 µs         | 1.032     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `objective/select_all/500`      | 11.741 µs        | 1.008     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `objective/select_top/10`       | 119.949 ns       | 1.018     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `objective/select_top/100`      | 1.630 µs         | 1.005     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `objective/select_top/500`      | 7.182 µs         | 1.028     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `ordering/cmp_desc/scalar`      | 16.334 ns        | 1.000     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `ordering/heap_top_k/100`       | 798.806 ns       | 1.035     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `ordering/heap_top_k/500`       | 4.294 µs         | 1.017     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `ordering/sort/10`              | 48.211 ns        | 1.003     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `ordering/sort/100`             | 1.277 µs         | 1.000     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `ordering/sort/500`             | 9.514 µs         | 1.003     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `selector/greedy/10`            | 538.735 ns       | 1.047     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `selector/greedy/100`           | 5.251 µs         | 1.010     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `selector/greedy/500`           | 30.332 µs        | 1.008     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `selector/greedy_diversity/10`  | 1.630 µs         | 1.037     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `selector/greedy_diversity/100` | 110.148 µs       | 1.018     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `selector/greedy_diversity/500` | 2.758 ms         | 1.017     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |

<!-- bench-reference:end -->

Benchmark suite: `benches/fold_bench.rs`. Run with:

```bash
cd crates && cargo bench -p khive-fold --bench fold_bench
```

For a quick compile/correctness check (no timing):

```bash
cd crates && cargo bench -p khive-fold --bench fold_bench -- --test
```

## Groups

| Group                       | Scenarios          | What is measured                                            |
| --------------------------- | ------------------ | ----------------------------------------------------------- |
| `fold/derive`               | 10, 100, 500 items | `CommonFold::count` traversal via `Fold::derive`            |
| `fold/sum_i64`              | 10, 100, 500 items | `CommonFold::sum_i64` projection + accumulation             |
| `fold/fn_closure`           | 10, 100, 500 items | `fold_fn` closure fold — measures closure dispatch overhead |
| `fold/count`                | 10, 100, 500 items | `CountFold` (zero-alloc, no state boxing)                   |
| `objective/batch_score`     | 10, 100, 500 items | `Objective::batch_score` — score + filter pass              |
| `objective/select_top`      | 10, 100, 500 items | `Objective::select_top(n/5)` — top-k heap path              |
| `objective/select_all`      | 10, 100, 500 items | `Objective::select` — full ranked selection                 |
| `ordering/sort`             | 10, 100, 500 items | `ScoredEntry` sort via `sort_unstable_by`                   |
| `ordering/cmp_desc`         | scalar             | `cmp_desc_score_then_id` — single comparison call           |
| `ordering/heap_top_k`       | 100, 500 items     | BinaryHeap top-10 extraction over `ScoredEntry<Uuid>`       |
| `selector/greedy`           | 10, 100, 500 items | `GreedySelector` fast path (diversity_bias = 0)             |
| `selector/greedy_diversity` | 10, 100, 500 items | `GreedySelector` diversity path (bias = 0.5, 8 categories)  |

## Input generation

All inputs are generated with a simple LCG (no external `rand` dependency) seeded with fixed
constants so runs are fully reproducible. Scores are uniform in [0.0, 1.0).

`iter_batched(SmallInput)` is used for benchmarks that sort or consume the input vector to
avoid measuring clone overhead inside the timed region.

## Additional workload coverage

The current suite also includes `fold_workloads` and `fold_selector_scale`.
The original target and the release ledger below are retained. The original
CountFold zero-allocation description is a source/API characterization; these
wall-clock benchmarks do not measure allocation counts. Historical values below
are observations from their recorded commit, not a current baseline or promise.

```sh
cd crates
cargo bench -p khive-fold --benches -- --quick --noplot
cargo bench -p khive-fold --bench fold_workloads -- --noplot
cargo bench -p khive-fold --bench fold_selector_scale -- --noplot
cargo bench -p khive-fold --bench fold_selector_scale -- --list
```

| Target / group                         | Inputs                                                          | Timed public work                                                                                                             | Untimed fixture and output checks                                                                                                                             |
| -------------------------------------- | --------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `fold_workloads` / compose             | 500 integers                                                    | `SequentialFold::execute` for two stages, or composed five stages; `FilterFold::derive` and `MapFold::derive`                 | Context carries the prior state; exact sums/counts and entries processed for every stage                                                                      |
| `fold_workloads` / checkpoint          | 10 and 100 distinct checkpoints, each with 32 i64 state entries | All synchronous `save` calls followed by all `load` calls, including serialization, hashing, lock and stored-state clone work | Fresh store and owned checkpoint clones before each timer; exact stored IDs, state, hash, UUID, context, version, timestamp and processed count               |
| `fold_workloads` / anchor              | 100 and 500 nodes, edges i→i+1 and i→i+2                        | `BfsAnchor::trace` from the first node or `credit` from the last, depth cap N                                                 | Graph/UUID generation; every other node exactly once, full node fields, exact credit weight 2^-shortest_distance                                              |
| `fold_selector_scale` / epistemic gain | 100 and 500 inputs, weight 0.5                                  | `GreedySelector::select` with nonzero information gains and explicit rank scores                                              | Input clone; exact selected order and record fields, budget accounting; weight-zero control must select the opposite end of the input                         |
| `fold_selector_scale` / diversity      | 1,000 and 5,000 inputs, 8 categories, bias 0.5                  | Actual diversity selection with the existing LCG, item-size and budget shape                                                  | Input clone; exact repeatable selected identities, no duplicates, record fields, budget accounting, all eight categories and a different order from bias zero |

Fixtures are created lazily in selected callbacks. Listing or selecting another
group does not build the 5,000-item fixture. Every consumed selector input is
cloned before its timer, and each iteration completes and drops its output before
the next begins; the harness never batches many large input clones in memory.
Result checks and output destruction are outside the timer. Each measurement uses
`Instant`, so per-call clock overhead is included rather than subtracted.

`fold_workloads` requests 10 Criterion samples, one second of warmup and three
seconds of measurement. The selector target requests flat sampling with 10
samples, 100 ms warmup and one second of measurement. These are requested sampling
parameters, not wall-time guarantees: a slow call or minimum sample count can
exceed them. `--quick` changes Criterion's sampling, not input sizes or output
checks. Timing medians are Criterion sample estimates, not invented single-call
percentiles.

The diversity implementation rescans remaining inputs after each selection.
With N inputs and k selected items, its candidate loop visits
`k * (2*N - k + 1) / 2` items, plus `N-k` for a final unsuccessful scan when items
remain. The upper bound is `N*(N+1)/2`, or 12,502,500 at N=5,000. This describes
source work, not elapsed time, and excludes initial sorting and record validation.
The fixture prints the actual selected k, total budget use and source-derived
visit count once per selected size. It checks at least half the inputs are selected,
so a tiny budget cannot accidentally turn this into a cheap path. Both sizes run;
no result is extrapolated from the old 500-item case.

The `compose/sequential_composed_five_stages/500` scene is an explicit public
composition: two `SequentialFold::execute` pairs followed by one terminal
`Fold::derive`. `SequentialFold` itself accepts exactly two folds and does not
implement `Fold`, so this is not a nonexistent nested five-fold constructor.
Every stage traverses the same 500 entries, with the previous stage's result
carried through `FoldContext`. All five states (124,750 through 623,750) and all
five processed counts are checked. Context mapping and the public composition's
five full traversals are inside the timer. The two-stage scene uses one pair.

`FilterFold` and `MapFold` have no `execute` method; their public entry is
`Fold::derive`. Filtering still visits all 500 entries, so the even-count result
is 250 while `entries_processed` is 500. Mapping doubles 0..499 for a sum of
249,500. Checkpoint storage is synchronous, not an async or persistence benchmark;
its in-memory map is fresh on every iteration.

The selector's `epistemic_weight` adds a caller-provided information-gain bonus.
The measured path supplies nonzero gains and proves that weight 0.5 changes the
selected identities relative to weight zero. It does not measure an objective's
precision computation or an embedding/model operation. The higher-precision rank
score field is also populated, as required by the real selector API.

## Current baseline ledger

The owning `storage-brain` bench-track component produces the new baselines on its
qualified host. Record the measured commit, toolchain, CPU/OS, command and output
artifact. New baselines and a rerun of all existing groups are pending measurement;
no numbers are inferred from source inspection or copied forward as current.

| Group                                            | Current baseline                     | Commit / toolchain / machine |
| ------------------------------------------------ | ------------------------------------ | ---------------------------- |
| Original fold/objective/ordering/selector groups | Pending qualified-host remeasurement | Pending                      |
| Sequential, filter and map folds                 | Pending qualified-host measurement   | Pending                      |
| Checkpoint save/load                             | Pending qualified-host measurement   | Pending                      |
| Anchor trace/credit                              | Pending qualified-host measurement   | Pending                      |
| Epistemic-gain selector                          | Pending qualified-host measurement   | Pending                      |
| Diversity at 1k/5k                               | Pending qualified-host measurement   | Pending                      |

## Performance guarantees

| Operation                       | Input                              | Metric         | Bound                                                                                      |
| ------------------------------- | ---------------------------------- | -------------- | ------------------------------------------------------------------------------------------ |
| Fold count (500 items)          | 500 i64 entries                    | latency median | Advisory trend; historical ledger value 935.8 ns                                           |
| Objective select_top (500, n/5) | 500 f64 candidates                 | latency median | Advisory trend; historical ledger value 6.192 µs                                           |
| Ordering sort (500)             | 500 ScoredEntry                    | latency median | Advisory trend; historical ledger value 9.869 µs                                           |
| Selector greedy (500)           | 500 inputs, bias=0                 | latency median | Advisory trend; historical ledger value 22.32 µs                                           |
| Selector greedy_diversity (500) | 500 inputs, 8 categories, bias=0.5 | latency median | Advisory trend; historical ledger value 10.99 ms                                           |
| SequentialFold                  | 500 inputs, chain=5                | latency median | Advisory trend against the recorded baseline                                               |
| Precision-weighted selector     | 500 inputs, epistemic_weight=0.5   | latency median | Advisory trend against the recorded baseline; actual caller-provided information-gain path |

No timing threshold is a hard test gate. The benchmark output checks validate the
fixture and selected public path, rather than turning historical performance
observations into requirements.

## Release Ledger

### v0.2.8 - 2026-06-08

- Commit: `d3629501c550fd2f3bb7ed350a2b60309d596465`
- Crate version: `0.2.8`
- Khive version: `0.2.8`
- Toolchain: `rustc 1.94.1 (e408947bf 2026-03-25)`, release profile (Criterion)
- Machine: Apple M-series arm64, macOS Darwin 25.5.0, 16 GB
- Feature flags: default
- Command: `cd crates && cargo bench -p khive-fold --bench fold_bench`
- Dataset: LCG-generated inputs, seed fixed in bench source; item counts 10 / 100 / 500
- vs prior: first formal release ledger entry — no prior comparable baseline

#### Fold Operations

| Scenario            | Low      | Median   | High     | Outliers     |
| ------------------- | -------- | -------- | -------- | ------------ |
| fold/derive/10      | 6.877 ns | 7.681 ns | 8.592 ns | 18/100 (18%) |
| fold/derive/100     | 74.19 ns | 85.46 ns | 98.53 ns | 14/100 (14%) |
| fold/derive/500     | 354.0 ns | 384.8 ns | 425.3 ns | 9/100 (9%)   |
| fold/sum_i64/10     | 12.89 ns | 13.78 ns | 14.92 ns | 21/100 (21%) |
| fold/sum_i64/100    | 119.6 ns | 126.3 ns | 134.6 ns | 19/100 (19%) |
| fold/sum_i64/500    | 618.7 ns | 627.9 ns | 640.6 ns | 10/100 (10%) |
| fold/fn_closure/10  | 2.643 ns | 2.737 ns | 2.859 ns | 18/100 (18%) |
| fold/fn_closure/100 | 8.068 ns | 8.445 ns | 8.921 ns | 12/100 (12%) |
| fold/fn_closure/500 | 42.52 ns | 45.13 ns | 48.53 ns | 17/100 (17%) |
| fold/count/10       | 11.15 ns | 12.04 ns | 13.06 ns | 11/200 (6%)  |
| fold/count/100      | 69.41 ns | 73.44 ns | 78.12 ns | 16/200 (8%)  |
| fold/count/500      | 834.1 ns | 935.8 ns | 1.047 µs | 9/200 (5%)   |

#### Objective

| Scenario                  | Low      | Median   | High     | Outliers     |
| ------------------------- | -------- | -------- | -------- | ------------ |
| objective/batch_score/10  | 41.59 ns | 45.71 ns | 50.64 ns | 13/100 (13%) |
| objective/batch_score/100 | 254.9 ns | 285.9 ns | 322.2 ns | 2/100 (2%)   |
| objective/batch_score/500 | 806.3 ns | 811.8 ns | 817.5 ns | 2/100 (2%)   |
| objective/select_top/10   | 193.2 ns | 203.1 ns | 215.2 ns | 8/100 (8%)   |
| objective/select_top/100  | 2.133 µs | 2.135 µs | 2.138 µs | 9/100 (9%)   |
| objective/select_top/500  | 5.937 µs | 6.192 µs | 6.515 µs | 12/100 (12%) |
| objective/select_all/10   | 257.6 ns | 272.4 ns | 290.0 ns | 10/100 (10%) |
| objective/select_all/100  | 1.621 µs | 1.639 µs | 1.661 µs | 1/100 (1%)   |
| objective/select_all/500  | 9.877 µs | 9.952 µs | 10.02 µs | —            |

#### Ordering

| Scenario                 | Low      | Median   | High     | Outliers |
| ------------------------ | -------- | -------- | -------- | -------- |
| ordering/sort/10         | 56.80 ns | 57.63 ns | 58.58 ns | —        |
| ordering/sort/100        | 1.372 µs | 1.382 µs | 1.393 µs | —        |
| ordering/sort/500        | 9.817 µs | 9.869 µs | 9.923 µs | —        |
| ordering/cmp_desc/scalar | 9.541 ns | 9.606 ns | 9.669 ns | —        |
| ordering/heap_top_k/100  | 688.4 ns | 689.5 ns | 690.7 ns | —        |
| ordering/heap_top_k/500  | 3.480 µs | 3.496 µs | 3.510 µs | —        |

#### Selector

| Scenario                      | Low      | Median   | High     | Outliers |
| ----------------------------- | -------- | -------- | -------- | -------- |
| selector/greedy/10            | 267.5 ns | 280.0 ns | 296.2 ns | —        |
| selector/greedy/100           | 5.058 µs | 5.562 µs | 6.193 µs | —        |
| selector/greedy/500           | 21.25 µs | 22.32 µs | 23.73 µs | —        |
| selector/greedy_diversity/10  | 2.387 µs | 2.577 µs | 2.821 µs | —        |
| selector/greedy_diversity/100 | 255.7 µs | 279.7 µs | 304.8 µs | —        |
| selector/greedy_diversity/500 | 10.20 ms | 10.99 ms | 11.79 ms | —        |

- Notes: none

Last reviewed: v0.2.8 (2026-06-08)
