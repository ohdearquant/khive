# khive-db benchmarks

The `db_stores` and `db_pool` Criterion targets complement the unchanged
`db_hot_path` and `db_concurrency` workloads. All new fixtures are generated,
use private temporary databases and volume-lock directories, and require no
live store or external dataset. A setup, operation, partial batch, unexpected
row count, truncated event page, or checkout failure aborts the benchmark.

## Scenarios

A full name joins the group and scenario with `/`; braces enumerate separate
scenarios, not a random choice at execution time.

| Group                           | Exact scenario names                                     | Measurement and fixture                                                | New baseline |
| ------------------------------- | -------------------------------------------------------- | ---------------------------------------------------------------------- | ------------ |
| `entity_upsert_batch`           | `records/{100,500,1000}`                                 | One fresh entity insert batch; latency/batch                           | pending      |
| `entity_page`                   | `offset_5000/{20,50,100}`, `seek_after_5000/{20,50,100}` | One page from 10K entities; offset includes exact count, seek does not | pending      |
| `edge_upsert_batch`             | `records/{100,500,1000}`                                 | One fresh edge insert batch, live endpoints prepared first             | pending      |
| `edge_neighbors`                | `one_hop_10k_edges`                                      | One outgoing neighbor call; 10K edges, 8 returned neighbors            | pending      |
| `edge_traverse`                 | `depth/{1,2,3}`                                          | One BFS; 10K edges, 8/72/584 descendants, result cap 1000              | pending      |
| `note_upsert_batch`             | `records/{100,500,1000}`                                 | One fresh note insert batch; latency/batch                             | pending      |
| `note_filtered_query`           | `percent/{1,10,50}`                                      | 10K notes, JSON boolean predicate, page 100 and exact filtered total   | pending      |
| `event_append_batch`            | `records/{100,500,1000}`                                 | One fresh event append batch; latency/batch                            | pending      |
| `event_page`                    | `rows/{100,500,4096}`                                    | First ascending count-free page from 100K events                       | pending      |
| `pooled_reader_checkout_window` | `threads/{1,4,8,16}`                                     | Auxiliary Criterion wall time for 256 acquisitions/thread on 4 readers | pending      |
| `pooled_reader_checkout_p99`    | `threads/{1,4,8,16}`                                     | Separate printed empirical per-acquire p99; 256/1024/2048/4096 samples | pending      |

`GraphStore::neighbors` is one-hop and has no depth parameter. The issue's
“Edge neighbors, depth=2” row below is preserved verbatim as requested; depth 2
is measured by `edge_traverse/depth/2`, not misrepresented as a neighbor call.
All named operations are reachable through existing public APIs; none was
skipped and no production visibility was changed.

## Fixtures and timing boundaries

`db_stores` uses two Tokio workers, at most four blocking workers, and a
file-backed WAL backend with two readers and the writer queue explicitly
requested through the existing test-support constructor. Entity/note payloads
use `StdRng` seeds 42/43. IDs, timestamps, event indices, and graph topology are
deterministic. The graph is an eight-way tree with 10,001 live entity endpoints
and 10,000 edges. Each traversal constructs a new one-shot execution budget;
reusing a consumed budget would change or fail later iterations.

Each write iteration creates exactly one fresh database, installs schemas,
prepares inputs and edge endpoints, then times only the named async store call
through `Runtime::block_on`. These are inserts through the upsert APIs, not
steady-state overwrites. Setup, fixture generation, result validation, writer
join, database close and deletion are excluded. The sequential custom loop
never batches multiple databases in memory. All store/backend handles are
dropped and the public writer-task join is awaited before removing the fixture.

Read scenarios reuse one immutable database per function. Corpus construction,
a warm query, request construction, result validation and destruction are
outside measured intervals. The interval includes async dispatch, SQLite work,
result hydration and `block_on` completion. Entity offset and seek scenarios
both start after 5000 records in their respective API ordering; the continuation
is obtained through real seek pages before timing. Note filters match exactly
100, 1000 or 5000 rows, returning a page of 100 and the exact matching total.
Event setup appends 100 batches of 1000, holding at most one input batch at a
time; its compact rows must fit the API's 1 MiB raw-text budget at page size
4096. No request failure or budget-shortened page is silently accepted.

### Reader acquire p99

`db_pool` creates a fresh four-reader WAL pool for each window and warms all four
connections. There are 256 acquisitions per worker and 1, 4, 8 or 16 synchronized
OS threads. A lease is held for a requested 100 microseconds with
`thread::sleep` before return; scheduler delays can lengthen that hold. The hold
creates contention and is excluded from each acquisition measurement.

Each sample is an `Instant` duration around the public `ConnectionPool::reader`
call, including its admission wait. Lease hold, return/reset, sample storage,
sorting and connection setup are excluded from that sample. Samples are sorted
and the nearest-rank p99 is the sample at one-based rank `ceil(0.99 * n)`:
254/1014/2028/4056 for 256/1024/2048/4096 samples. After each selected Criterion
scenario, a separate fresh fixed-size window prints
`pooled_reader_checkout_p99/threads/N samples=... nearest_rank=... p99_ns=...`.
This is an empirical percentile of individual acquisitions in that window,
not a percentile inferred from Criterion averages, a confidence interval, or
a production tail-latency guarantee.

Criterion separately reports `pooled_reader_checkout_window/threads/N`: the
whole acquisition wave, including holds, barrier release and worker joins;
pool/thread creation, sorting and cleanup are excluded. Fresh windows reset
recycling state. Reader maximum age/count are explicitly 300 seconds/5000;
at most 4100 checkouts including warming occur in any window. This workload
measures contention before operation-count recycling, not recycling cost.
See [reader recycling](api/reader-recycling.md) for the production policy.

## Running and recording results

Run from `crates/` on the measurement machine:

```bash
cargo bench -p khive-db --features vectors --benches -- --quick --noplot
cargo bench -p khive-db --bench db_stores -- entity_page
cargo bench -p khive-db --bench db_pool -- pooled_reader_checkout_window
```

Quick mode is a smoke run; record a full measurement for baseline comparisons.
Capture commit, Rust toolchain, machine/OS, features, effective `KHIVE_*` pool
settings and exact command. The scene rows above are all **pending**: no new
latency result has been measured for this change. Baseline commit, toolchain
and machine are also **pending** until the owner measures after the gate.
The [release ledger](api/benchmarks.md) preserves older observations and contains
pending current-main rows for the two existing targets and both new targets.

### Edge invalidation trigger comparison

`db_edge_mutations` measures singleton insertion, 100/1000-edge bulk insertion,
weight/metadata update, endpoint update, and soft/hard deletion. Each operation
uses a fresh file-backed WAL fixture with live memory endpoints and a required
writer task. Preparation and persisted-result checks are outside its timer.

`runtime_edge_mutations` measures the actual entity and note merge APIs with
eight outgoing incident edges each, verifying all eight retained edge IDs and
rewritten endpoints after timing. It also runs 4 and 16 concurrent producers,
each issuing 64 product link calls through a barrier. That throughput interval
includes task launch, calls and joins; fixture setup, diagnostic snapshots,
percentile calculation and result checks are outside it. Both targets use one
fixture at a time and drain its writer before deleting its private directory.

Each `writer-queue:` JSON line reports the individual caller-latency nearest-rank
p95 and the existing writer-admission queue-age and service histograms. The
histograms subtract the fixture-setup snapshot bucket by bucket and require
exactly one accepted and serviced admission per composed link call. Their p95
is a power-of-two bucket upper bound; compare it as a bound. Caller-latency p95
uses all successful per-call nanosecond samples and can support a percentage
comparison. Missing telemetry, incomplete admissions, operation errors and
incorrect persisted row counts fail the workload.

Run the identical benchmark source on the pre-migration and post-migration
trees with the same features, toolchain, machine and settings:

```bash
cargo bench -p khive-db --bench db_edge_mutations --locked
cargo bench -p khive-runtime --bench runtime_edge_mutations --locked
```

These are generic writes with governance inactive and no stamped markers.
They measure installed-trigger overhead for the dormant schema; they do not
measure future active-governance decision or invalidation-ledger populations.
All timing observations are pending native execution. No speedup or bound is
claimed from benchmark source alone.

## Performance guarantees

| Operation               | Input                       | Metric         | Bound                                                                     |
| ----------------------- | --------------------------- | -------------- | ------------------------------------------------------------------------- |
| FTS5 AnyTerm search     | 10K docs, 3 terms, top_k=20 | latency median | Advisory trend; last ledger value 14.87 ms (`docs/api/benchmarks.md:71`)  |
| FTS5 batch upsert       | 100 docs                    | latency median | Advisory trend; last ledger value 7.19 ms (`docs/api/benchmarks.md:86`)   |
| sqlite-vec KNN search   | 10K × 384-dim, top_k=100    | latency median | Advisory trend; last ledger value 10.60 ms (`docs/api/benchmarks.md:96`)  |
| sqlite-vec batch insert | 1000 records × 384-dim      | latency median | Advisory trend; last ledger value 27.24 ms (`docs/api/benchmarks.md:104`) |
| Entity batch upsert     | 1000 entities               | latency median | Advisory trend against the recorded baseline                              |
| Edge neighbors          | 10K edges, depth=2          | latency median | Advisory trend against the recorded baseline                              |
| Concurrent FTS reads    | 8 tasks × 8 searches        | wall-time      | Advisory trend against the recorded baseline                              |

All rows are advisory timing trends against a rolling and a fixed reference.
Historical values are observations, not promises or fixed percentage gates.
This issue defines no deterministic allocation/count guarantee; there are no
new count unit tests or must-fail mutations. Fixture integrity assertions in
the benchmark ensure it measures the declared work; they are not performance
contracts. Workflow and proposed guarantee-schema wiring are separate work.
