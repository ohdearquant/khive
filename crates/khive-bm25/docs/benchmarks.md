# khive-bm25 Benchmark Ledger

<!-- bench-reference:begin -->

Generated from `perf/reference.json` by `scripts/render-bench-reference.py`; do not edit this block.

Reference baselines for `khive-bm25`. Values are the committed reference median for each benchmark, in the unit shown; A/A is the ratio between the two same-commit runs. The date is the committer date of the baseline commit.

| Benchmark                         | Reference median | A/A ratio | Baseline commit                            | Commit date |
| --------------------------------- | ---------------- | --------- | ------------------------------------------ | ----------- |
| `index_document/docs/100`         | 6.187 ms         | 1.013     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `index_document/docs/1000`        | 68.417 ms        | 1.004     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `index_document/docs/5000`        | 343.417 ms       | 1.069     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `index_document_single/words/200` | 123.136 µs       | 1.011     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `index_document_single/words/50`  | 42.860 µs        | 1.030     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `index_document_single/words/500` | 220.759 µs       | 1.021     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `memory_usage/docs/100`           | 5.529 µs         | 1.016     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `memory_usage/docs/1000`          | 64.629 µs        | 1.001     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `memory_usage/docs/500`           | 32.612 µs        | 1.010     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `remove_document/1k_corpus`       | 2.012 ms         | 1.014     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `search_1k/1-term`                | 4.577 µs         | 1.001     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `search_1k/2-term`                | 7.899 µs         | 1.010     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `search_1k/3-term`                | 10.196 µs        | 1.010     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `search_1k/4-term`                | 12.694 µs        | 1.012     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `search_1k/5-term`                | 15.046 µs        | 1.000     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `search_context/fresh_ctx`        | 7.920 µs         | 1.002     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `search_context/reused_ctx`       | 7.505 µs         | 1.009     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `search_corpus_scale/docs/100`    | 2.345 µs         | 1.006     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `search_corpus_scale/docs/1000`   | 10.324 µs        | 1.013     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `search_corpus_scale/docs/500`    | 5.841 µs         | 1.023     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `search_topk/k/1`                 | 9.961 µs         | 1.000     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `search_topk/k/10`                | 10.204 µs        | 1.005     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `search_topk/k/50`                | 11.487 µs        | 1.001     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |

<!-- bench-reference:end -->

## Benchmark Targets

| Target              | Harness          | Command                                                                                       |
| ------------------- | ---------------- | --------------------------------------------------------------------------------------------- |
| Criterion suite     | `cargo bench`    | `cd crates && cargo bench -p khive-bm25 --bench bm25_bench`                                   |
| WAND vs brute-force | `#[ignore]` test | `cargo test -p khive-bm25 bench_bm25_wand_vs_bruteforce_zipf_matrix -- --ignored --nocapture` |

## Release Ledger

### v0.2.8 - 2026-06-08

- Commit: `d3629501c550fd2f3bb7ed350a2b60309d596465`
- Crate version: `0.2.8`
- Khive version: `0.2.8`
- Toolchain: `rustc 1.94.1 (e408947bf 2026-03-25)`, release profile (Criterion)
- Machine: Apple M-series arm64, macOS Darwin 25.5.0, 16 GB
- Feature flags: default
- Command: `cd crates && cargo bench -p khive-bm25 --bench bm25_bench`
- Dataset: synthetic in-memory corpus generated per bench group; seed embedded in bench source
- vs prior: first formal release ledger entry — no prior comparable baseline

#### Indexing

| Benchmark               | Median   |
| ----------------------- | -------- |
| index_document/100 docs | 5.45 ms  |
| index_document/1K docs  | 54.2 ms  |
| index_document/5K docs  | 278.9 ms |
| index_single/50 words   | 4.16 ms  |
| index_single/200 words  | 4.26 ms  |
| index_single/500 words  | 4.35 ms  |

#### Search (1K corpus, k=10)

| Query Terms | Median  |
| ----------- | ------- |
| 1-term      | 4.65 µs |
| 2-term      | 8.30 µs |
| 3-term      | 11.0 µs |
| 4-term      | 14.1 µs |
| 5-term      | 16.9 µs |

#### Corpus Scale (2-term, k=10)

| Corpus   | Median  |
| -------- | ------- |
| 100 docs | 2.20 µs |
| 500 docs | 6.07 µs |
| 1K docs  | 11.2 µs |

#### Top-K Sensitivity (1K corpus, 3-term)

| k  | Median  |
| -- | ------- |
| 1  | 11.0 µs |
| 10 | 11.0 µs |
| 50 | 11.8 µs |

#### Context Reuse (1K corpus, 3-term)

| Mode           | Median  |
| -------------- | ------- |
| Fresh context  | 8.37 µs |
| Reused context | 7.53 µs |

#### Memory & Mutation

| Benchmark                 | Median  |
| ------------------------- | ------- |
| memory_usage/100 docs     | 6.59 µs |
| memory_usage/500 docs     | 30.2 µs |
| memory_usage/1K docs      | 61.6 µs |
| remove_document/1K corpus | 56.3 ms |

#### WAND vs Brute-Force (64 queries, k=10, debug profile)

| Corpus    | Query Terms | Brute-Force (ms) | BMW (ms) | Speedup |
| --------- | ----------- | ---------------- | -------- | ------- |
| 10K docs  | 1           | 47.1             | 47.4     | 0.99x   |
| 10K docs  | 2           | 67.2             | 138.5    | 0.49x   |
| 10K docs  | 3           | 96.3             | 95.9     | 1.00x   |
| 50K docs  | 1           | 189.1            | 429.7    | 0.44x   |
| 50K docs  | 2           | 361.9            | 197.3    | 1.83x   |
| 50K docs  | 3           | 524.2            | 336.8    | 1.56x   |
| 100K docs | 1           | 394.4            | 797.0    | 0.49x   |
| 100K docs | 2           | 783.8            | 353.4    | 2.22x   |
| 100K docs | 3           | 917.3            | 406.1    | 2.26x   |

- Notes: No regressions from the search/ module split. Criterion numbers are new (first formal
  Criterion run); WAND numbers carried forward from pre-split baseline.

Last reviewed: v0.2.8 (2026-06-08)

## Lifecycle and tokenizer coverage

`bm25_lifecycle` adds nine generated-data scenarios. From `crates/`, run:

```sh
cargo bench -p khive-bm25 --bench bm25_lifecycle -- --quick --noplot
cargo bench -p khive-bm25 --benches -- --quick --noplot
```

| Group                    | Scenarios                                                          | Timed operation                     | Input                                      | Baseline |
| ------------------------ | ------------------------------------------------------------------ | ----------------------------------- | ------------------------------------------ | -------- |
| `clear`                  | `1000`                                                             | Clear a populated index             | 1,000 documents                            | pending  |
| `ensure_after_insert`    | `forward_index_guard/1000_plus_1`, `doc_lengths_guard/1000_plus_1` | Populated-cache guards              | 1,000 documents plus one insert            | pending  |
| `stats`                  | `100`, `500`, `1000`                                               | Read stored corpus statistics       | 100 / 500 / 1,000 documents                | pending  |
| `tokenize_unicode_chars` | `200`, `2000`, `20000`                                             | Default `SimpleTokenizer::tokenize` | 200 / 2,000 / 20,000 Unicode scalar values | pending  |

The corpus uses 200 words per document, 16 fixed non-stop words and the fixed
formula `(42 + document * 7 + word * 5) % 16`. Setup verifies exact document,
token and vocabulary counts. It needs no external fixture or model. The tokenizer
input cycles `Alpha, café 東京42 beta! 123 kernel` and truncates by `chars()`;
throughput elements mean Unicode scalar values, not UTF-8 bytes or output tokens.

`clear` times only the public reset on a fresh 1K clone. Cloning, empty-state
assertions, a reuse insertion/search and destruction are outside the reported
interval. It removes the contents of containers; this measurement makes no
blanket promise that reusing an index avoids every subsequent allocation.

Both ensure operations start from a fresh clone plus one insertion outside their
timers. `index_document` already updates the forward index and document-length
vectors, so these are guard costs, **not cold materialization costs**. The normal
public deserializer also rebuilds these caches before returning. A cold-cache
rebuild after an ordinary insert is not exposed by this API. The benchmark does
not alter private state to manufacture one. A preflight checks search and removal
of the inserted document. `iter_batched_ref` with `PerIteration` excludes setup
and destruction and retains at most one clone per iteration; timer overhead may
dominate these small guard operations.

`stats` reads stored counts and the average; it does not scan the corpus to
recompute statistics. Its Criterion loop includes the call and result consumption.
Tokenizer timing includes normalization, filtering and output allocation, while
`iter_batched` excludes output destruction. Setup and assertions are outside the
timed region in every group. Criterion reports latency distributions for trends;
no hand-authored latency value is substituted for a measurement.

These additions preserve the original target and the historical observations
above. The producer for the pending rows is the qualified-host `scoring-retrieval`
benchmark run. Each completed baseline must record the measured commit, toolchain,
machine, command, scenario and result. CI component wiring is maintained separately.

## Performance guarantees

| Operation                   | Input                       | Metric               | Bound                                        |
| --------------------------- | --------------------------- | -------------------- | -------------------------------------------- |
| `search`                    | 1K corpus, 3-term, k=10     | median latency       | Advisory trend against the recorded baseline |
| `index_document`            | 200-word doc into 1K corpus | median latency       | Advisory trend against the recorded baseline |
| `clear`                     | 1K corpus                   | median latency       | Advisory trend against the recorded baseline |
| `ensure_forward_index`      | 1K corpus after 1 insert    | median latency       | Advisory trend against the recorded baseline |
| `SimpleTokenizer::tokenize` | 2_000 chars                 | throughput (chars/s) | Advisory trend against the recorded baseline |

All rows are advisory timing trends against a rolling and fixed reference, not
fixed percentage failures. Historical values remain observations. The ensure row
measures the populated-cache guard described above; it must not be interpreted as
a reconstruction-time guarantee. No new deterministic count or allocation bound
is introduced by this table.
