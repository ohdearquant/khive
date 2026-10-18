# khive-changeset benchmark ledger

## Run and record

```bash
cd crates
cargo bench -p khive-changeset --benches -- --quick --noplot
```

The two native Criterion targets are `ndjson_codec` and `invert`, with ten
samples, one-second warmup and three-second measurement defaults. Command-line
settings may override these. Criterion is a native-target dev-dependency; WASM
bench entries do no work and produce no measurements. The existing native/WASM
library and test-suite parity obligations remain separate from benchmarking.

| Target         | Group               | Scenarios                        | Sizes (ops)   | Metrics                             |
| -------------- | ------------------- | -------------------------------- | ------------- | ----------------------------------- |
| `ndjson_codec` | `ndjson_round_trip` | `entity_create`, `entity_update` | 1, 100, 10000 | ns/op, serialized bytes/op, ops/sec |
| `invert`       | `changeset_invert`  | `entity_create`, `entity_update` | 1, 100, 10000 | ns/op, ops/sec                      |

The exact ID is `<group>/<scenario>/<size>`, for twelve scenes total. One
Criterion iteration processes the entire N-op list. Divide the median
nanoseconds/batch estimate in `target/criterion/<group>/<scenario>/<size>/new/estimates.json`
by N to obtain ns/op; `Throughput::Elements(N)` supplies operation throughput.
For a common median-based comparison, ops/sec is N × 1e9 / median ns/batch.
These are normalized batch estimates, not percentiles of individually timed
operations within the list.

Each selected scene prints `changeset_payload_report` with the exact UTF-8
serialized length and length/N. This includes the envelope header and every
newline, including the final newline. Bytes/op is encoded payload size, not an
allocation count or memory-footprint measurement. Listing or filtering scenarios
does not construct unrelated fixtures or print their reports.

## Workloads and timing boundaries

Each fixture contains exactly N independent entity creates or N independent
entity name updates. UUIDs, timestamps, namespaces, names, properties and tag
order are fixed. Create descriptions contain an escaped newline and Unicode;
updates carry explicit before/after names. These two shapes do not claim
exhaustive coverage of every operation or payload size.

All current records are populated using public record types in memory.
The inverse envelope differs from the forward envelope. Inversion is a pure
stage-time check against supplied `CurrentState`, not database replay or an
atomic landing check. No graph, database, clock, model or network access occurs.

Fixture construction and correctness preflight run once outside measurement for
each selected scene. Preflight requires exact envelope and op count, canonical
byte-identical encode/decode/re-encode, and an exact independently constructed
reverse-order inverse. Create inverses include the complete original record and
timestamps in soft-delete preimages; update inverses swap before/after names.
The forward change-set and current records must remain unchanged.

Round-trip timing includes encoding, parsing/validation, decoded result allocation
and destruction, and temporary encoded-string destruction. It excludes fixture
generation, reporting and correctness assertions. Invert uses
`iter_batched(..., BatchSize::PerIteration)` to exclude caller envelope cloning
and returned-result destruction while bounding retained output memory. Its
internal current-state clone, validation, reverse traversal and output allocation
are included. Criterion's timing overhead remains part of the estimate; there is
no additional per-operation timer inside either API.

## Performance guarantees

| Operation                              | Input   | Metric               | Bound                                        |
| -------------------------------------- | ------- | -------------------- | -------------------------------------------- |
| `to_ndjson` + `from_ndjson` round-trip | 10k ops | ns/op                | Advisory trend against the recorded baseline |
| `to_ndjson` + `from_ndjson` round-trip | 10k ops | bytes/op             | Advisory trend against the recorded baseline |
| `to_ndjson` + `from_ndjson` round-trip | 100 ops | throughput (ops/sec) | Advisory trend against the recorded baseline |
| `ChangeSet::invert`                    | 10k ops | ns/op                | Advisory trend against the recorded baseline |

Compare each workload shape separately against rolling and fixed references.
Timing and payload-size rows are advisory, with no fixed percentage gate. A past
measurement is an observation, not a latency promise. Correctness preflight
assertions are independent of these performance trends.

## Baselines

| Group               | Baseline | Required record                                                                                               |
| ------------------- | -------- | ------------------------------------------------------------------------------------------------------------- |
| `ndjson_round_trip` | Pending  | Commit, full toolchain, machine, features, command, each scene's median/throughput and printed payload length |
| `changeset_invert`  | Pending  | Commit, full toolchain, machine, features, command and each scene's median                                    |

This change contains no benchmark measurements. Record results on a qualified
host together with ordinary test, lint and native/WASM parity outcomes. A
successful source inspection alone does not establish compilation or runtime
correctness.
