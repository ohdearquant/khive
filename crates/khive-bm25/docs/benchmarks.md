# khive-bm25 Benchmark Ledger

## Benchmark Targets

| Target | Harness | Command |
| --- | --- | --- |
| Criterion suite | `cargo bench` | `cargo bench -p khive-bm25` |
| WAND vs brute-force | `#[ignore]` test | `cargo test -p khive-bm25 bench_bm25_wand_vs_bruteforce_zipf_matrix -- --ignored --nocapture` |

## Release Ledger

### v0.2.6-post (2026-06-06, post-refactor)

- **Commit**: `perf/recall-fts-candgather-v2` branch, post search/ module split
- **Toolchain**: rustc 1.94.1 (e408947bf 2026-03-25), release profile (Criterion)
- **Machine**: arm64 (Apple Silicon), macOS Darwin 25.5.0

#### Indexing

| Benchmark | Median |
| --- | --- |
| index_document/100 docs | 5.45 ms |
| index_document/1K docs | 54.2 ms |
| index_document/5K docs | 278.9 ms |
| index_single/50 words | 4.16 ms |
| index_single/200 words | 4.26 ms |
| index_single/500 words | 4.35 ms |

#### Search (1K corpus, k=10)

| Query Terms | Median |
| --- | --- |
| 1-term | 4.65 µs |
| 2-term | 8.30 µs |
| 3-term | 11.0 µs |
| 4-term | 14.1 µs |
| 5-term | 16.9 µs |

#### Corpus Scale (2-term, k=10)

| Corpus | Median |
| --- | --- |
| 100 docs | 2.20 µs |
| 500 docs | 6.07 µs |
| 1K docs | 11.2 µs |

#### Top-K Sensitivity (1K corpus, 3-term)

| k | Median |
| --- | --- |
| 1 | 11.0 µs |
| 10 | 11.0 µs |
| 50 | 11.8 µs |

#### Context Reuse (1K corpus, 3-term)

| Mode | Median |
| --- | --- |
| Fresh context | 8.37 µs |
| Reused context | 7.53 µs |

#### Memory & Mutation

| Benchmark | Median |
| --- | --- |
| memory_usage/100 docs | 6.59 µs |
| memory_usage/500 docs | 30.2 µs |
| memory_usage/1K docs | 61.6 µs |
| remove_document/1K corpus | 56.3 ms |

#### WAND vs Brute-Force (64 queries, k=10, debug profile)

| Corpus | Query Terms | Brute-Force (ms) | BMW (ms) | Speedup |
| --- | --- | --- | --- | --- |
| 10K docs | 1 | 47.1 | 47.4 | 0.99x |
| 10K docs | 2 | 67.2 | 138.5 | 0.49x |
| 10K docs | 3 | 96.3 | 95.9 | 1.00x |
| 50K docs | 1 | 189.1 | 429.7 | 0.44x |
| 50K docs | 2 | 361.9 | 197.3 | 1.83x |
| 50K docs | 3 | 524.2 | 336.8 | 1.56x |
| 100K docs | 1 | 394.4 | 797.0 | 0.49x |
| 100K docs | 2 | 783.8 | 353.4 | 2.22x |
| 100K docs | 3 | 917.3 | 406.1 | 2.26x |

**Regression notes**: No regressions from the search/ module split. Criterion numbers are new
(first formal Criterion run); WAND numbers carried forward from pre-split baseline.

Last reviewed: 2026-06-06
