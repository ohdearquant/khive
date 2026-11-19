# Graph-state parsing and comparison benchmarks

These targets exercise the public, deterministic `GraphState::from_ndjson` and
`diff` APIs with generated NDJSON. No database, file import, network, daemon or
model participates. Timing and allocation observations are advisory; this crate
has no historical benchmark baseline to replace.

## Running the targets

From `crates/`, on a qualified measurement host, run serially:

```sh
cargo bench -p khive-graph-diff --benches -- --quick --noplot
cargo bench -p khive-graph-diff --bench graph_parse -- --list
cargo bench -p khive-graph-diff --bench graph_allocations -- --list
cargo bench -p khive-graph-diff --bench graph_parse -- parse/entity/500000
cargo bench -p khive-graph-diff --bench graph_parse -- --test
cargo bench -p khive-graph-diff --bench graph_allocations -- --test
cargo test -p khive-graph-diff --test graph_allocation_observer
```

The first command executes all three parse sizes once per substrate in each of
the fixed-report binaries, plus the Criterion diff cases. `--quick` affects
Criterion sampling; it never changes fixed-report sizes or semantic checks.
The fixed-report programs accept Cargo's `--bench`, `--test`, `--quick`,
`--noplot`, `--list`, `--help` and one optional **literal substring** filter.
Unknown flags fail explicitly. A direct executable invocation requires `--bench`
to select the full sizes. `--test`, or absence of `--bench`, selects only 1,000
records. Selected larger scenes emit `UNMEASURED` with a smoke-mode reason; they
are not run in test mode and cannot supply large-workload evidence.

`--list` lists scene IDs without constructing fixtures or running the observer.
Each target creates one selected fixture at a time and releases it before the next
size. Full 500,000-record runs need enough memory for the actual graph and its
public validation copy; do not run these targets concurrently on a small host.
No memory capacity or runtime guarantee is inferred from source inspection.

Criterion is a native-only development dependency. All benchmark binaries have
an empty WASM entry point, and the observer test is native-only. The existing
`graph_diff` test suite and golden bytes remain the native/WASM parity contract.

## Workloads and identities

Each generated record has a deterministic ascending UUID and four non-identity
fields: a substrate label, a distinct record label, a nested payload containing
an ordinal, Unicode, null and a boolean, and a version. Entity and note streams
use `id`; edge streams use `edge_id`. The same UUIDs deliberately coexist across
substrates. The parser retains arbitrary supplied fields and does not supply
domain defaults, so these are typed JSON fixtures, not assertions about a domain
schema. No random source or live export is required.

| Target              | Scene IDs                                                                    | Sizes and meaning                                  | Observations                                                                                  |
| ------------------- | ---------------------------------------------------------------------------- | -------------------------------------------------- | --------------------------------------------------------------------------------------------- |
| `graph_parse`       | `parse/{entity,edge,note}/{1000,50000,500000}`                               | Exactly one nonempty substrate, N records          | One actual parse call; ns/call, ns/record, input UTF-8 bytes                                  |
| `graph_diff`        | `graph_diff/{matched,modified,disjoint}/{1000,50000}`                        | N records in each of three substrates on each side | Criterion whole-call estimates; throughput denominator is 3 × N input records on one side     |
| `graph_allocations` | The nine `parse/...` IDs and `diff/{matched,modified,disjoint}/{1000,50000}` | Same fixtures, one call per scene                  | Successful Rust allocator requests/call and /input record; peak live Rust heap above baseline |

There are nine fixed timing reports, six Criterion diff scenes and fifteen fixed
allocation reports in a full run. Fixed smoke mode executes three parse reports
and six allocation reports (three parse and three diff at 1,000). The other fixed
rows remain explicitly unmeasured. Criterion `--test` independently executes its
six declared diff scenes once; it does not turn the fixed parser smoke into a
50,000- or 500,000-record measurement.

Matched states have equal IDs and complete fields and produce an empty delta.
Modified states have equal IDs and change only version 0 to 1, producing exactly
one named field change for every record. Disjoint states use non-overlapping UUID
ranges, producing N additions and N removals per substrate. Every result checks
exact counts, UUID order, complete fields and absence of unrelated changes.
Fixture construction also validates both source states through the public API.
This prevents an empty matched result from being the only useful-work witness.

## Measurement boundaries

The fixed parser target reads `Instant` immediately before its one real parser
call and stops after the returned state is available. Generation, input capacity,
result validation, JSON reporting and destruction are outside the interval.
`ns_per_record` divides that one call by N; it is a batch average. These results
are labelled `single_call`, not a Criterion distribution, median or p99. The
first call is not promised to represent a controlled cold CPU/allocator state.

The Criterion diff target constructs one fixture lazily for a selected scene and
reuses its immutable states. Ten flat samples, a 100 ms warmup and a one-second
measurement target apply before command-line overrides. A measurement target is
not an execution timeout; expensive calls can exceed it. `PerIteration` batching
retains only one returned delta at a time and drops it outside the timer. The
timed comparison includes output allocation and production temporary cleanup.
Input parsing, correctness preflight and output destruction are excluded. A
Criterion median estimates whole diff-call cost; it is not per-record tail latency.

Allocation reports run in a separate instrumented executable. They count successful
Rust global `alloc`, `alloc_zeroed` and `realloc` requests across all threads. A
successful realloc is one request. Peak live Rust heap is the maximum tracked
live allocation size above the pre-call baseline, measured through the returned
result. It is not total bytes allocated, serialized bytes, native allocator/TLS
heap, mmap, RSS or allocator-internal copy space. No zero-allocation guarantee is
asserted. Uninstrumented binaries alone supply timing observations.

Inputs and expected fixture data are prepared before observation. A parse result
remains live through `finish`; a diff result does likewise. All validation and
reporting happens afterward. Since `GraphState` keeps collections private, exact
parse validation calls `diff(empty, parsed)`, which clones fields into a public
delta. That temporary copy is outside the measurement but still consumes process
memory. It is released before the next scene, and no copied parser or visibility
change substitutes for the real implementation.

The observer has a separate ordinary integrity test covering a worker-thread
allocation, successful reallocation and retained heap. The benchmark performs that
check once before its first selected allocation report. No latency threshold is
part of that test. The observer implementation records its source path to make
later drift discoverable.

## Baseline ledger

The `gate-channel-code` component owns qualification and collection. Record the
exact source revision, toolchain/profile/features, CPU/OS, allocator, command,
fixture/scenario IDs and retained raw output with each observation. Full-size
results cannot be inferred from smoke, source checks or omitted rows. There are
no measured values in this change.

| Group                                                          | Baseline                           | Commit / toolchain / machine |
| -------------------------------------------------------------- | ---------------------------------- | ---------------------------- |
| Per-substrate parse, 1k / 50k / 500k                           | Pending qualified-host measurement | Pending                      |
| Matched / modified / disjoint diff, 1k / 50k per substrate     | Pending qualified-host measurement | Pending                      |
| Parse and diff Rust allocator requests / incremental live heap | Pending qualified-host measurement | Pending                      |

## Performance guarantees

| Operation                 | Input                                                | Metric                                                      | Bound                                                 |
| ------------------------- | ---------------------------------------------------- | ----------------------------------------------------------- | ----------------------------------------------------- |
| `GraphState::from_ndjson` | 500k records per substrate, each measured separately | Single parse ns/call and normalized ns/record               | Advisory trend against the recorded baseline          |
| `GraphState::from_ndjson` | 500k records per substrate, each measured separately | Peak live Rust heap above the pre-call baseline, bytes/call | Advisory trend against the recorded baseline; not RSS |
| `diff`                    | 50k matched records in each substrate                | Whole diff-call timing estimate                             | Advisory trend against the recorded baseline          |
| `diff`                    | 50k disjoint records in each substrate               | Whole diff-call timing estimate                             | Advisory trend against the recorded baseline          |

Modified-state coverage supplements these four rows. A historical observation is
not an absolute performance promise, and these timing/memory rows have no fixed
percentage failure threshold. No new release timing guarantee is activated.
