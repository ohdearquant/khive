# Vamana Benchmark Ledger

<!-- bench-reference:begin -->

Generated from `perf/reference.json` by `scripts/render-bench-reference.py`; do not edit this block.

Reference baselines for `khive-vamana`. Values are the committed reference median for each benchmark, in the unit shown; A/A is the ratio between the two same-commit runs. The date is the committer date of the baseline commit.

Comparison class: `rustc 1.95.0 (59807616e 2026-04-14)`, `bench` profile, AMD Ryzen 7 PRO 8700GE w/ Radeon 780M Graphics, cores 2,3, kernel 6.8.0-142-generic.

| Benchmark                        | Reference median | A/A ratio | Baseline commit                            | Commit date |
| -------------------------------- | ---------------- | --------- | ------------------------------------------ | ----------- |
| `build/VamanaIndex::build/1000`  | 453.218 ms       | 1.001     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `build/VamanaIndex::build/10000` | 30.084 s         | 1.000     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `build/VamanaIndex::build/5000`  | 9.752 s          | 1.000     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `distance/cosine_from_l2sq`      | 0.865 ns         | 1.000     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `distance/l2_squared/384d`       | 46.858 ns        | 1.001     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `free_fns/build/1k`              | 452.963 ms       | 1.001     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `free_fns/search/1k/k10`         | 148.011 µs       | 1.006     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `search/n=1000/k=10`             | 150.513 µs       | 1.008     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `search/n=1000/k=50`             | 150.497 µs       | 1.002     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `search/n=10000/k=10`            | 1.002 ms         | 1.002     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `search/n=10000/k=50`            | 1.002 ms         | 1.003     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `search/n=5000/k=10`             | 715.000 µs       | 1.001     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `search/n=5000/k=50`             | 722.100 µs       | 1.021     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `snapshot/from_snapshot/1000`    | 1.558 ms         | 1.001     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `snapshot/from_snapshot/5000`    | 8.708 ms         | 1.003     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `snapshot/to_snapshot/1000`      | 73.308 µs        | 1.007     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |
| `snapshot/to_snapshot/5000`      | 589.408 µs       | 1.009     | `c82766083139abd22f95e9a34cf778c3cd20b95f` | 2026-10-18  |

<!-- bench-reference:end -->

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
cd crates && cargo bench -p khive-vamana --bench vamana_bench

# Single group:
cd crates && cargo bench -p khive-vamana --bench vamana_bench -- search

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

## Release Ledger

### v0.2.8 - 2026-06-08

- Commit: `d3629501c550fd2f3bb7ed350a2b60309d596465`
- Crate version: `0.2.8`
- Khive version: `0.2.8`
- Toolchain: `rustc 1.94.1 (e408947bf 2026-03-25)`, release profile (Criterion)
- Machine: Apple M-series arm64, macOS Darwin 25.5.0, 16 GB
- Feature flags: default
- Command: `cd crates && cargo bench -p khive-vamana --bench vamana_bench`
- Dataset: seeded random unit vectors, SEED=42, DIM=384; corpus sizes 1K / 5K / 10K
- vs prior: first formal release ledger entry — no prior comparable baseline

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

- Notes: `from_snapshot` numbers (272 µs at 1K, 1.62 ms at 5K) are measured against a cold
  filesystem; a prior run with a warm cache showed lower numbers (63.8 µs / 411 µs). No code
  regression — the difference is measurement methodology. All search latencies pass the 3 ms SLO.

---

## ADR-048 pass criteria

- `recall@10 >= 0.80` for N=1000x384 (integration test, always runs)
- `recall@10 >= 0.85` for N=5000x384 (ignored; run manually)
- Single-query search latency target: < 3 ms at N=10k (from perf/recall-fts SLO)

Last reviewed: v0.2.8 (2026-06-08)

## Lifecycle and persistence suite — baseline pending

The additional targets are `vamana_lifecycle` and `vamana_persistence`.
The latter requires `mmap`, which is enabled by default. The original
`vamana_bench` workloads and historical ledger above are unchanged. No new
measurement is claimed: baseline commit, machine, toolchain and values are
**pending** until the suite is measured. Source provenance is
`d3619dec9e32045f273c61b5a7b2eb70fe6463a3`, not a measured baseline.

A scenario's full name joins its group and suffix with `/`.

| Group                   | Scenario suffixes                         | Measurement and fixture                                                     | Baseline |
| ----------------------- | ----------------------------------------- | --------------------------------------------------------------------------- | -------- |
| `lifecycle_insert`      | `100_into_5000`                           | 100 sequential inserts into a clean 5K index; whole-batch latency           | pending  |
| `lifecycle_tombstone`   | `single_100_of_5000`, `batch_100_of_5000` | 100 single calls versus one 100-ID batch on a fresh 5K index                | pending  |
| `lifecycle_consolidate` | `500_tombstones_of_5000`                  | Consolidate a prepared 5K index with 500 tombstones; 4500-entry remap       | pending  |
| `portable_round_trip`   | `5000`                                    | `to_bytes` plus `from_bytes`, including the full external-ID map            | pending  |
| `recall_at_k`           | `per_query_k10/{1000,5000}`               | Criterion rotating-query evaluation; fixed all-query report described below | pending  |
| `filesystem_round_trip` | `save_load_5000`, `save_atomic_load_5000` | v1 or v2 directory save plus load of a clean 5K index                       | pending  |

### Generated fixtures and resets

All new data is generated; no model, ignored corpus or live index is read.
Vectors have 384 dimensions, are normalized, and use `StdRng` with explicit
seeds. Lifecycle and persistence use corpus seed 42, insert seed 43 and verification
query seed 44. New scenarios use default R=64, L=128, alpha=1.2, including at 1K
for consistency with the existing ordinary quality test. The original 1K
performance bench still uses R=32/L=64 and is a different workload.

Mutation scenarios build their baseline once. Every measured iteration creates
one owned `fork_for_maintenance` outside the timer; it copies full lifecycle
state and starts with fresh search scratch. This prevents repeated inserts from
growing the fixture, repeated deletes from failing, or consolidation from
becoming a no-op. At most the baseline and one working index are live. Sequential
custom loops avoid Criterion batching many multi-megabyte indexes. Each 5K×384
raw vector slab alone is 7.68 MB, before graph/reverse adjacency and SQ8 codes.

The insert batch must return ordinals 5000..5099 and leave 5100 live vectors.
Single and batch tombstones use the same 100 unique ordinals, explicitly excluding
the medoid; these rows measure graph repair without medoid re-election.
Consolidation starts after 500 repaired tombstones and verifies the complete
new-to-old survivor mapping, 4500 live vectors and cleared tombstone/churn state.
A v1 snapshot is not used to reset mutations: it cannot represent tombstones.
Input construction, fork, assertions and output destruction are outside timers;
all 100 requested insert/delete calls, including their graph maintenance, are
inside their respective batch timers. Inserts include any scratch initialization
needed by the fresh working index.

### Recall value and actual median/p99

The 1K fixture uses corpus/query seeds 0xDEAD_BEEF/0xCAFE_BABE, and the 5K fixture
uses 0xFEED_FACE/0xABCD_1234. Corpus and query sets are independent. The selected
scenario warms one query, then evaluates **all 1000 or 5000 queries exactly once**
in a fixed measured pass using `recall_at_k(one_query_slice,10)`. Its arithmetic
mean is the same mean-recall definition as the multi-query API. Every returned
value must be finite and within [0,1]. It prints alongside its Criterion group:

```text
recall_at_k/per_query_k10/N/distribution queries=N recall_at_10=... eval_median_ns=... eval_p99_ns=...
```

The median is the midpoint of the two central duration samples; p99 uses
nearest rank `ceil(0.99*N)` (rank 990 or 4950). These are measured **per-query
evaluation** latencies, including exact brute-force reference search, ANN search
and recall overlap computation. They are not ANN-only search times, full-batch
latencies, or percentiles inferred from Criterion averages. Sample insertion,
validation, averaging and sorting follow the per-query timers. The Criterion
companion repeatedly evaluates one rotating query, including input selection,
using the same fixed fixture; its iteration calibration never reduces the
fixed report's query count, including with `--quick`.

`tests/benchmark.rs` retains the ordinary >=0.80 recall gate on 1000×384 vectors
and **50 queries**, and the ignored >=0.85 gate on 5000 vectors and 100 queries.
The new 1000-query benchmark also asserts >=0.80. No costly ordinary 1000-query
test replaces or duplicates that existing gate. The 5K benchmark reports quality;
it does not invent a stronger threshold than the existing policy.

The evaluator scans the corpus for each exact reference query. At 5K/5K this is
25 million vector-distance pairs, so its one fixed pass can dominate smoke-run
wall time. The suite does not repeat 100 full passes to manufacture a p99. With
`parallel`, reference evaluation uses the Rayon global pool; the separate
`KHIVE_ANN_BUILD_THREADS` cap only controls graph builds. Record both that setting
and `RAYON_NUM_THREADS` on the measurement machine.

### Persistence boundaries

`save`, `save_atomic` and `load` operate on a **directory of segments**, not one
temp file. Each iteration creates a fresh private TempDir before timing, saves
the unchanged clean 5K baseline and loads it before stopping the timer. This
includes serialization, publication and load validation; v2 additionally has
its documented crash-safe staging/fsync protocol. It measures a new directory,
not overwrite throughput. v1 save cannot persist tombstones.

After timing, both persistence and portable-codec scenarios verify vector count,
dimensions, configuration, complete vector contents and fixed-query search
results. Portable decoding also verifies all 5000 external IDs. Loaded mmap
indexes are dropped before explicit directory cleanup; cleanup failures fail
the benchmark. No missing fixture, partial workload or operation error is silently
skipped. No cold-cache claim is made: the just-saved files are in the OS cache.
The original in-memory snapshot group remains distinct from filesystem I/O.

### Execution and baseline recording

From `crates/`:

```sh
cargo bench -p khive-vamana --benches -- --quick --noplot
cargo bench -p khive-vamana --bench vamana_lifecycle -- recall_at_k
cargo bench -p khive-vamana --bench vamana_persistence -- filesystem_round_trip
```

The new targets use 10 Criterion samples, 100 ms warmup and a 1 s measurement target;
setup and fixed recall reporting add wall time outside measured intervals.
Record a full run for baselines, naming commit, Rust toolchain, CPU/RAM/OS,
features, commands, thread settings and fixture seeds. For advisory median
latency rows, use Criterion's `new/estimates.json` median estimate rather than
renaming its headline mean. Use the explicit distribution line for recall's
actual per-query median/p99. New values remain pending.

## Performance guarantees

| Operation       | Input                         | Metric         | Bound                                                                                                                                 |
| --------------- | ----------------------------- | -------------- | ------------------------------------------------------------------------------------------------------------------------------------- |
| `search`        | 10K vectors, 384d, k=10       | median latency | < 3 ms (ADR-048 SLO, `crates/khive-vamana/docs/benchmarks.md:100`) — repository-stated timing target, advisory until promoted (#5642) |
| `insert`        | 100 vectors into 5K index     | median latency | Advisory trend against the recorded baseline                                                                                          |
| `consolidate`   | 5K index after 500 tombstones | median latency | Advisory trend against the recorded baseline                                                                                          |
| `recall_at_k`   | 1K vectors, 1K queries, k=10  | recall@10      | >= 0.80 (ADR-048, `crates/khive-vamana/docs/benchmarks.md:98`) — repository-stated quality bar                                        |
| `save` + `load` | 5K index to temp file         | median latency | Advisory trend against the recorded baseline                                                                                          |

The table is the issue #5645 contract. Timing targets are advisory, including
the repository-stated 3 ms search target; no fixed-percentage regression gate is
added. Recall >=0.80 remains a quality gate. The existing ordinary 50-query
fixture and new 1000-query benchmark are distinguished above. No new performance
count/allocation guarantee, workflow change or guarantee-schema file is added.
