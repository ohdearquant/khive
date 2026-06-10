# khive-fold Benchmarks

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
