# Code-pack benchmarks

<!-- bench-reference:begin -->

Generated from `perf/reference.json` by `scripts/render-bench-reference.py`; do not edit this block.

Reference baselines for `khive-pack-code`. Values are the committed reference median for each benchmark, in the unit shown; A/A is the ratio between the two same-commit runs. The date is the committer date of the baseline commit.

| Benchmark                                                                       | Reference median | A/A ratio | Baseline commit                            | Commit date |
| ------------------------------------------------------------------------------- | ---------------- | --------- | ------------------------------------------ | ----------- |
| `khive-pack-code:code_findings/pack_code_findings/clippy_jsonl/100/median_ns`   | 2.883 ms         | 1.012     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `khive-pack-code:code_findings/pack_code_findings/clippy_jsonl/10000/median_ns` | 310.410 ms       | 1.010     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `khive-pack-code:code_findings/pack_code_findings/json/100/median_ns`           | 1.581 ms         | 1.007     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `khive-pack-code:code_findings/pack_code_findings/json/10000/median_ns`         | 164.379 ms       | 1.002     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `khive-pack-code:code_ingest/pack_code_ingest/fresh_l1_l15/10/median_ns`        | 15.694 ms        | 1.000     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `khive-pack-code:code_ingest/pack_code_ingest/fresh_l1_l15/1000/median_ns`      | 1.306 s          | 1.006     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `khive-pack-code:code_ingest/pack_code_ingest/fresh_l1_l15/10000/median_ns`     | 14.469 s         | 1.003     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |

<!-- bench-reference:end -->

The latency targets call the public ingest functions with generated deterministic
inputs. They never open an existing map, invoke a compiler, fetch a model, or
read a git-ignored fixture. Input creation, output checks and destruction are
outside measured intervals. Missing or incomplete fixtures fail the benchmark.

The two Criterion timing targets create inputs and execute operations only after
Criterion selects a scene. Listing or an unmatched filter creates no source
tree, database, executor or findings fixture and performs no ingest/conversion.
Default and `--test` modes run the selected correctness workload without the
supplemental timing report; `--test` also wins when combined with `--bench`.
Each selected ingest scene performs one complete ingest in correctness mode.
Each selected findings scene retains its identity preflight and one checked
conversion, so it performs two conversions.

Ordinary `--bench` mode prints one supplemental operation report per selected
scene, before Criterion's measured iterations. `--profile-time` (both spellings)
suppresses that report. Lazy setup and the findings identity preflight remain
outside the returned sample duration, but can appear in a whole-process profile.
Mode detection stops at `--`, so mode-looking text in a positional filter does
not change report behavior. The allocation executable has its separate contract
below.

| Target / group                         | Scenario                       | Input                                                          | Measurement                                      |
| -------------------------------------- | ------------------------------ | -------------------------------------------------------------- | ------------------------------------------------ |
| `code_ingest` / `pack_code_ingest`     | `fresh_l1_l15/{10,1000,10000}` | One manifest plus N generated Rust files; new WAL map per call | Whole function wall time and source files/second |
| `code_findings` / `pack_code_findings` | `json/{100,10000}`             | JSON audit document with N distinct findings                   | Findings/second and nanoseconds/finding          |
| `code_findings` / `pack_code_findings` | `clippy_jsonl/{100,10000}`     | N distinct Clippy diagnostics and a successful terminal marker | Findings/second and nanoseconds/finding          |

## Source ingest boundary

One package contains exactly N source files: `src/lib.rs` and N-1 uniquely named
modules, with deterministic constant declarations and no imports. The manifest
is an additional file and is excluded from the throughput denominator. This is
an intentionally sparse dependency corpus, not a representative repository or
a benchmark of dense import resolution. Ingestion still creates N `contains`
edges, one project, and N modules, performs real secret/guarded-write and FTS work,
and stamps source/coverage evidence. L1 and L1.5 are enabled; L2 is disabled.

The source tree is generated once per size and reused. Every iteration creates a
fresh private WAL database and its schema outside timing, with two reader slots,
a queued writer and a private volume-lock directory outside the walked tree.
One current-thread Tokio executor admits at most two blocking workers. The public
Rust source-reader code can additionally start and join its own bounded scanner
thread. No model is registered and no ANN corpus is created. Exactly one map
fixture is retained at a time; runtime/backend ownership is dropped and its
writer joined before removing the directory or starting another iteration.

The measured interval includes executor entry, the actual public
`run_code_ingest`, its Git metadata probes, walks, Rust parsing, worker scheduling,
secret checks, entity/FTS/edge writes and coverage processing. Filesystem and SQLite
caches may be warm; a fresh map does not imply cold OS caches. The generated tree
must be outside a Git worktree. Its exact expected `unversioned` metadata warning
is checked; any additional warning, refusal, missed stamp, missing module or
unexpected edge count fails. Untimed SQL accounting confirms the stored project,
module and edge counts. Teardown, report validation and returned-data destruction
are untimed. Source scans execute only when the compiled benchmark is run.

## Findings boundary

Both findings functions are pure validated record converters; neither persists
records in a database. The fixtures have unique paths and messages, so Clippy
fingerprint deduplication cannot silently reduce N diagnostics to fewer findings.
All source bytes, options, provenance and expected note identities are prepared
before a timed sample. The conversion call includes JSON/JSONL parsing, validation,
canonical identity construction and output record allocation. The small options
clone and dynamic-call dispatch are included. After every sample, the benchmark
checks one entity, exactly N distinct finding notes, N annotation edges and the
same identities as preflight. Output validation and destruction are outside timing.

Criterion uses bounded sequential `iter_custom` calls: each timed result is
checked and dropped before another conversion, without retaining a batch of
large outputs. Its throughput denominator is N findings. A single actual
operation report also prints findings/second and nanoseconds/finding; it is not
a latency percentile, regression threshold or historical baseline.

## Allocation reports

`code_allocations` is a separate instrumented executable. It always runs seven
fixed reports: `pack_code_allocations/fresh_l1_l15/{10,1000,10000}` and
`pack_code_allocations/{json,clippy_jsonl}/{100,10000}`, including quick/test mode.
It does not publish Criterion latency estimates; the two timing executables
have no custom global allocator. Each operation runs once per report, with the
same inputs, fresh-map setup, result checks and teardown boundaries described
above. This is a point observation, not a distribution or regression gate.

The metrics are **Rust global-allocator call counts** (successful `alloc`,
`alloc_zeroed` and `realloc` requests, excluding deallocations) and **peak live
Rust heap above the pre-call baseline**, measured in requested bytes. Source
reports also divide calls by N source files. SQLite C allocations, mmap, RSS,
allocator metadata/fragmentation and temporary allocator-internal realloc copies
are excluded. These figures must never be labeled process peak memory.

A process-wide System forwarding allocator observes all threads, including the
queued writer and source-reader workers. It tracks live requested bytes from
process start, so freeing a pre-window allocation reduces live heap correctly.
A short allocation-free spin lock serializes bookkeeping and observation
start/stop. System calls occur outside that lock. Successful realloc replaces
the old size with the new size and counts once; failure retains the old size.
Unwinding disables the observer. There is only one active observation.

Window membership follows bookkeeping completion, not causal ownership of an
allocation. In-flight operations can straddle its boundaries, and unrelated
background work in the same process can contribute. Frees of pre-existing
allocations can reduce the net live increase. These are all-thread process-window
Rust heap observations, not exclusive per-function allocation accounting.

The ordinary `observer_counts_worker_allocations_and_peak` test and the report
binary's preflight start a worker before observation, then hold an 8192-byte Vec
grown to 16384 bytes across the observation boundary. It must observe positive
request counts and that live growth. Lower bounds allow synchronization noise;
they are not an exact two-request oracle. Disabling either request increments
or peak updates must fail this integrity test. No hard per-ingest allocation
ceiling is introduced.

## Performance guarantees

| Operation                  | Input        | Metric                                                  | Bound                                        |
| -------------------------- | ------------ | ------------------------------------------------------- | -------------------------------------------- |
| `run_code_ingest`          | 10k files    | throughput (files/sec)                                  | Advisory trend against the recorded baseline |
| `run_code_ingest`          | 10k files    | peak live Rust heap above the pre-call baseline (bytes) | Advisory trend against the recorded baseline |
| `run_code_ingest`          | 1k files     | Rust global-allocator call counts / source file         | Advisory trend against the recorded baseline |
| `ingest_findings_json`     | 10k findings | findings/sec                                            | Advisory trend against the recorded baseline |
| `ingest_clippy_json_lines` | 10k findings | ns/finding                                              | Advisory trend against the recorded baseline |

These clarified Rust-only allocation labels follow the metric boundary above.
All five rows are advisory trends, not hard timing or allocation ceilings. No
production behavior, public API or existing correctness gate is changed.

## Baselines and execution

| Group                | Commit  | Toolchain | Machine | Baseline | Producer                                    |
| -------------------- | ------- | --------- | ------- | -------- | ------------------------------------------- |
| `pack_code_ingest`   | pending | pending   | pending | pending  | Owning benchmark component / qualified host |
| `pack_code_findings` | pending | pending   | pending | pending  | Owning benchmark component / qualified host |

Run named targets from `crates/` on the qualified host:

```sh
cargo bench -p khive-pack-code --benches -- --quick --noplot
```

The allocation report baseline also remains pending.
The largest source corpus performs real per-file writes and is substantially more
expensive than pure JSON conversion. Use one benchmark process at a time and
record filesystem, load, exact commit and toolchain alongside results. The first
qualified run must populate these pending cells; no local native measurements
are implied. Workflow component wiring and `perf/guarantees.toml` belong to their
separate integration/format work.
