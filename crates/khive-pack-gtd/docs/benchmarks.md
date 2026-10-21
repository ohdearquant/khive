# khive-pack-gtd Benchmark Ledger

<!-- bench-reference:begin -->

Generated from `perf/reference.json` by `scripts/render-bench-reference.py`; do not edit this block.

Reference baselines for `khive-pack-gtd`. Values are the committed reference median for each benchmark, in the unit shown; A/A is the ratio between the two same-commit runs. The date is the committer date of the baseline commit.

| Benchmark                    | Reference median | A/A ratio | Baseline commit                            | Commit date |
| ---------------------------- | ---------------- | --------- | ------------------------------------------ | ----------- |
| `gtd/assign`                 | 289.588 µs       | 1.005     | `212bf72e498de9fbbda42cc0e2902906c730e3ea` | 2026-10-17  |
| `gtd/next/10`                | 78.478 µs        | 1.017     | `212bf72e498de9fbbda42cc0e2902906c730e3ea` | 2026-10-17  |
| `gtd/next/100`               | 183.852 µs       | 1.003     | `212bf72e498de9fbbda42cc0e2902906c730e3ea` | 2026-10-17  |
| `gtd/tasks/filter_by_status` | 134.356 µs       | 1.004     | `212bf72e498de9fbbda42cc0e2902906c730e3ea` | 2026-10-17  |
| `gtd/transition`             | 413.637 µs       | 1.062     | `212bf72e498de9fbbda42cc0e2902906c730e3ea` | 2026-10-17  |

<!-- bench-reference:end -->

## Run command

```bash
# from workspace root
cd crates && cargo bench -p khive-pack-gtd --bench gtd_bench

# compile + smoke-check only (no timing)
cd crates && cargo bench -p khive-pack-gtd --bench gtd_bench -- --test
```

HTML reports land in `target/criterion/gtd/`.

---

## Scenarios

| Benchmark                    | Description                                                                              | Setup                                                      |
| ---------------------------- | ---------------------------------------------------------------------------------------- | ---------------------------------------------------------- |
| `gtd/assign`                 | Write latency for a single `gtd.assign` call.                                            | One in-memory fixture; tasks accumulate across iterations. |
| `gtd/next/10`                | `gtd.next(limit=10)` over a corpus of 10 seeded tasks (mixed statuses/priorities).       | 10 tasks seeded once for this benchmark case.              |
| `gtd/next/100`               | `gtd.next(limit=10)` over a corpus of 100 seeded tasks.                                  | 100 tasks seeded once for this benchmark case.             |
| `gtd/tasks/filter_by_status` | `gtd.tasks(status="next", limit=50)` over 100 seeded tasks.                              | 100 tasks seeded once.                                     |
| `gtd/transition`             | `gtd.assign` + `gtd.transition(status="next")` — inline create+transition per iteration. | One fixture; each iteration adds and transitions a task.   |

The write benchmarks reuse their in-memory fixture, so their task corpus grows during warmup and measurement. The read benchmarks use a fixed, pre-seeded corpus for each case.

---

## Release Ledger

### v0.2.8 - 2026-06-08

- Commit: `d3629501c550fd2f3bb7ed350a2b60309d596465`
- Crate version: `0.2.8`
- Khive version: `0.2.8`
- Toolchain: `rustc 1.94.1 (e408947bf 2026-03-25)`, release profile (Criterion)
- Machine: Apple M-series arm64, macOS Darwin 25.5.0, 16 GB
- Feature flags: default
- Command: `cd crates && cargo bench -p khive-pack-gtd --bench gtd_bench`
- Dataset: in-memory SQLite; task corpus seeded with 10 / 100 tasks; sample size 50
- vs prior: first formal release ledger entry — no prior comparable baseline


| Scenario                   | Low      | Median   | High     | Outliers   |
| -------------------------- | -------- | -------- | -------- | ---------- |
| gtd/assign                 | 2.787 ms | 3.030 ms | 3.272 ms | 2/50 (4%)  |
| gtd/next/10                | 81.48 µs | 86.18 µs | 92.38 µs | —          |
| gtd/next/100               | 340.5 µs | 385.5 µs | 425.1 µs | 7/50 (14%) |
| gtd/tasks/filter_by_status | 264.3 µs | 280.4 µs | 304.2 µs | 7/50 (14%) |
| gtd/transition             | 2.029 ms | 2.211 ms | 2.427 ms | 2/50 (4%)  |

- Notes: none

Last reviewed: v0.2.8 (2026-06-08)


## Current maintenance scenarios

`gtd_maintenance` adds independent dispatch measurements using generated fixtures.
The existing `gtd_bench` and the release ledger above are preserved. Their old
measurements describe the named historical commit, not the current implementation.

| Group             | Scenario           | Measured work                                                                                    | Input                | Baseline |
| ----------------- | ------------------ | ------------------------------------------------------------------------------------------------ | -------------------- | -------- |
| `gtd_maintenance` | `complete`         | One `gtd.complete` after an untimed `gtd.assign`                                                 | One inbox task       | pending  |
| `gtd_maintenance` | `assign_complete`  | `gtd.assign`, response-ID extraction and `gtd.complete`                                          | One fresh task       | pending  |
| `gtd_maintenance` | `census/1000`      | One full `gtd.census` histogram dispatch, without candidate paging                               | 1000 live tasks      | pending  |
| `gtd_maintenance` | `repair_apply/100` | One `gtd.repair(apply=true)` dispatch, including 100 guarded updates and mandatory audit inserts | 100 historical tasks | pending  |

The complete-only case answers the single-dispatch question. The separately named
assign/complete case measures the round trip in the guarantee table. Their numbers
are not interchangeable. Census uses fixed microsecond-shaped timestamps and no
archival timestamp, so the expected histogram has 1000 created values in the
16-digit bucket and 1000 absent archival values. Magnitude is not inferred units.

Repair starts with exact generated timestamps `1772323200` and `1772323200000`,
and the legacy status `archived`. Each item explicitly supplies those raw observed
JSON sources and requests `1772323200000000` for both timestamp fields plus `done`.
It measures accepted writes, not a dry run or stale-observation refusal. Seeding
uses the public typed note store because ordinary task assignment correctly
cannot create the legacy status. Every input is read back before measurement.
Afterward, each repaired note must retain all unrelated fields, increment its
version exactly once, and preserve the original values in repair history; all
100 matching lifecycle audit rows are required. Completion checks the persisted
terminal state and audit. Census checks scope and totals and leaves every seeded
note unchanged. A mismatched receipt, partial repair or missing audit fails loudly.

### Timing and resource boundary

Each sample owns a fresh in-memory runtime with embeddings disabled, fixed local
namespace and actor, and only KG/GTD packs. The public configured registry builder
attaches the actual dispatch audit sink. The benchmark measures in-process
dispatch, not MCP transport or server-specific admission. Runtime construction,
fixture insertion/readback, argument construction and final state checks are
outside the timer. Complete and repair include their real lazy lifecycle-schema
bootstrap when reached on the fresh fixture. The round trip also includes the
caller work needed to turn the assignment response into completion arguments.

The timer covers `block_on` of the full dispatch or pair of dispatches and captures
the returned value before checking it. Audit supervisor drain/join, response
destruction and fixture destruction occur afterward and before the next sample.
Storage has no file/ANN root or queued writer; these properties are asserted.
There is one current-thread Tokio executor with at most two blocking threads, one
fixture at a time, no shared model, ambient database or accumulating task corpus.
Asynchronous audit activity may overlap dispatch; full audit drain is untimed.

Every scenario first records 11 actual operation durations and prints the sorted
rank-6 median in nanoseconds and milliseconds. These fixed observations also run
in Criterion test/quick mode. Criterion then measures the same interval through
serial `iter_custom`, reporting one operation per complete dispatch/pair. The
Criterion trend and the individual-operation median are distinct reports. The
1000-task setup is repeated untimed for every census operation, so wall time can
substantially exceed measured dispatch time. No percentile, allocation count or
hard timing threshold is invented.

```bash
# From crates/: new scenarios only.
cargo bench -p khive-pack-gtd --bench gtd_maintenance -- --quick --noplot
cargo bench -p khive-pack-gtd --bench gtd_maintenance -- --test

# Record existing and new targets on the same qualified machine.
cargo bench -p khive-pack-gtd --benches -- --quick --noplot
```

### Current baseline recording

| Target            | Scenarios to record                                                     | Baseline | Commit / toolchain / machine |
| ----------------- | ----------------------------------------------------------------------- | -------- | ---------------------------- |
| `gtd_bench`       | `assign`, `next/10`, `next/100`, `tasks/filter_by_status`, `transition` | pending  | pending                      |
| `gtd_maintenance` | `complete`, `assign_complete`, `census/1000`, `repair_apply/100`        | pending  | pending                      |

The historical target still retains its documented growing-corpus write workloads.
This addition carries no new benchmark measurements. Record each baseline with
its exact commit, toolchain, feature set and machine, retaining historical entries.
Workflow wiring and the proposed cross-crate guarantee format are separate work.

## Performance guarantees

| Operation                     | Input                         | Metric         | Bound                                                                           |
| ----------------------------- | ----------------------------- | -------------- | ------------------------------------------------------------------------------- |
| `gtd.assign`                  | single write, in-memory       | latency median | Advisory trend; last ledger value 3.030 ms median (`docs/benchmarks.md` v0.2.8) |
| `gtd.next(100, limit=10)`     | 100 seeded tasks              | latency median | Advisory trend; last ledger value 385.5 µs median (`docs/benchmarks.md` v0.2.8) |
| `gtd.tasks(filter_by_status)` | 100 seeded tasks, status=next | latency median | Advisory trend; last ledger value 280.4 µs median (`docs/benchmarks.md` v0.2.8) |
| `gtd.complete`                | assign + complete round-trip  | latency median | Advisory trend against the recorded baseline                                    |
| `gtd.census`                  | 1000 tasks                    | latency median | Advisory trend against the recorded baseline                                    |
| `gtd.repair`                  | 100 stale tasks               | latency median | Advisory trend against the recorded baseline                                    |

All six rows are advisory timing trends. The assertions above establish fixture
correctness, not new deterministic performance bounds. Baseline cells stay
`pending` until qualified measurements are recorded.
