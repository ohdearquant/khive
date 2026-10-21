# khive-request Benchmark Ledger

<!-- bench-reference:begin -->

Generated from `perf/reference.json` by `scripts/render-bench-reference.py`; do not edit this block.

Reference baselines for `khive-request`. Values are the committed reference median for each benchmark, in the unit shown; A/A is the ratio between the two same-commit runs. The date is the committer date of the baseline commit.

| Benchmark               | Reference median | A/A ratio | Baseline commit                            | Commit date |
| ----------------------- | ---------------- | --------- | ------------------------------------------ | ----------- |
| `parse/batch/10`        | 4.702 µs         | 1.005     | `212bf72e498de9fbbda42cc0e2902906c730e3ea` | 2026-10-17  |
| `parse/batch/3`         | 821.109 ns       | 1.011     | `212bf72e498de9fbbda42cc0e2902906c730e3ea` | 2026-10-17  |
| `parse/chain/2`         | 830.479 ns       | 1.021     | `212bf72e498de9fbbda42cc0e2902906c730e3ea` | 2026-10-17  |
| `parse/chain/5`         | 2.094 µs         | 1.007     | `212bf72e498de9fbbda42cc0e2902906c730e3ea` | 2026-10-17  |
| `parse/json_form/3_ops` | 1.592 µs         | 1.034     | `212bf72e498de9fbbda42cc0e2902906c730e3ea` | 2026-10-17  |
| `parse/single/complex`  | 944.605 ns       | 1.002     | `212bf72e498de9fbbda42cc0e2902906c730e3ea` | 2026-10-17  |
| `parse/single/simple`   | 199.715 ns       | 1.000     | `212bf72e498de9fbbda42cc0e2902906c730e3ea` | 2026-10-17  |

<!-- bench-reference:end -->

## Run command

```bash
cd crates && cargo bench -p khive-request --bench request_bench
```

Test-only (no timing, validates correctness):

```bash
cd crates && cargo bench -p khive-request --bench request_bench -- --test
```

HTML reports land in `target/criterion/` when gnuplot or plotters is available.

## Scenarios

| ID | Group             | Name      | Input description                                                       |
| -- | ----------------- | --------- | ----------------------------------------------------------------------- |
| 1  | `parse/single`    | `simple`  | `verb(arg="value")` — minimal single op                                 |
| 2  | `parse/single`    | `complex` | `memory.remember(...)` with 5 args including UUID — realistic prod call |
| 3  | `parse/batch`     | `3`       | `[create(...), search(...), get(...)]` — small parallel batch           |
| 4  | `parse/batch`     | `10`      | Generated 10-op parallel batch                                          |
| 5  | `parse/chain`     | `2`       | `create(...)                                                            |
| 6  | `parse/chain`     | `5`       | Generated 5-op chain with $prev propagation                             |
| 7  | `parse/json_form` | `3_ops`   | `[{"tool":"...","args":{...}},...]` — JSON-form 3-op batch              |

## Release Ledger

### v0.2.8 - 2026-06-08

- Commit: `d3629501c550fd2f3bb7ed350a2b60309d596465`
- Crate version: `0.2.8`
- Khive version: `0.2.8`
- Toolchain: `rustc 1.94.1 (e408947bf 2026-03-25)`, release profile (Criterion)
- Machine: Apple M-series arm64, macOS Darwin 25.5.0, 16 GB
- Feature flags: default
- Command: `cd crates && cargo bench -p khive-request --bench request_bench`
- Dataset: inline string literals in bench source; parse-only, no DB; sample sizes 200 (single),
  100 (batch/chain/json_form)
- vs prior: first formal release ledger entry — no prior comparable baseline

| Scenario              | Low      | Median   | High     | Outliers     |
| --------------------- | -------- | -------- | -------- | ------------ |
| parse/single/simple   | 303.9 ns | 338.3 ns | 376.7 ns | 35/200 (18%) |
| parse/single/complex  | 1.247 µs | 1.390 µs | 1.551 µs | 29/200 (15%) |
| parse/batch/3         | 1.008 µs | 1.131 µs | 1.275 µs | 10/100 (10%) |
| parse/batch/10        | 6.148 µs | 7.083 µs | 8.111 µs | 16/100 (16%) |
| parse/chain/2         | 1.195 µs | 1.354 µs | 1.533 µs | 8/100 (8%)   |
| parse/chain/5         | 2.723 µs | 2.948 µs | 3.212 µs | 19/100 (19%) |
| parse/json_form/3_ops | 1.947 µs | 2.107 µs | 2.301 µs | 17/100 (17%) |

- Notes: none

Last reviewed: v0.2.8 (2026-06-08)

## Planning, admission, typed input, and scale

The additive `request_pipeline` target covers downstream request inspection and
larger parser inputs. The original `request_bench` target and its historical
measurements above are retained.

```bash
cd crates
cargo bench -p khive-request --bench request_pipeline -- --quick --noplot
cargo bench -p khive-request --benches -- --quick --noplot
```

| Group              | Scenarios                                                               | Public operation and actual input                                                                                  | Baseline |
| ------------------ | ----------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------ | -------- |
| `request_pipeline` | `plan/single/{simple,complex}`, `plan/batch/{3,10}`, `plan/chain/{2,5}` | `plan_request` over the six unchanged legacy DSL inputs with a fixed benchmark catalog                             | pending  |
| `request_pipeline` | `atomic/{eligible,mixed,rejected}/{10,50,100}`                          | `check_atomic_admissible` on prepared operation vectors, covering admitted names and all four rejection categories | pending  |
| `request_pipeline` | `typed_json/{10,50,100}`                                                | `parse_typed_json_batch` on already-decoded operation vectors with nested JSON scalar/list/object arguments        | pending  |
| `request_pipeline` | `parse_batch/{50,100}`                                                  | `parse_request` with the original generated batch shape, up to `MAX_OPS=100`                                       | pending  |
| `request_pipeline` | `parse_chain/{10,20}`                                                   | `parse_request` with the original generated chain shape and `$prev.id` at every subsequent operation               | pending  |

These are 22 scenarios. Inputs are deterministic source-generated values, require
no external data, and never construct a runtime or execute a tool. The owning
`ann-query` benchmark component produces new baselines on a qualified host; the
record must include commit, toolchain, machine, flags, command, and complete
sample output. All new baselines remain **pending** that measurement.

### Meaning of each operation

`plan_request` takes **raw DSL text**, parses it, and builds a JSON description
using catalog lookups. Its measured time includes that parsing and projection.
It is syntax/catalog inspection: it does not perform dispatch admission, resolve
`$prev`, or execute a request. The catalog contains `create`, `search`, `get`,
`link`, and `memory.remember`; the legacy `verb(...)` fixture deliberately remains
unknown. Checks verify modes, stages, arguments, catalog membership, unit ranges,
and literal previous-result references after timing.

Atomic admission checks only tool names in an already-prepared operation vector.
The eligible pattern cycles `update`, `delete`, `link`; the mixed pattern combines
eligible names with `create`, `search`, `missing.benchmark`, and `merge`; the
rejected pattern cycles those four rejected names. Their categories are
embedding-bearing, read, unlisted, and known-unimplemented, respectively. Every
returned index, tool and reason is checked in input order. There is no atomic
request flag, prepare/apply work, transaction, or database in this measurement.
Arguments are empty because this API does not inspect them.

Typed JSON parsing consumes a fresh owned `Vec<TypedJsonOp>` per call. The vector
is cloned **before** the timer; conversion and structural validation are timed,
including any destruction performed inside the consuming API. JSON-text decoding
is not measured. This trusted-transport seam has no original raw bytes and does
not enforce the raw DSL byte cap; the public DSL scale scenes still use
`parse_request`. Result checks compare all typed arguments, operation order,
mode, and unit ranges. Chain checks preserve `$prev.id` as an unresolved reference.

### Timing boundaries

Each scenario runs one preflight, then prints a `request_call_report` from 1,001
actual single calls. The median is sorted rank 501. A separate Criterion trend
uses 10 samples, a one-second warmup and a three-second measurement target;
`--quick` selects Criterion quick mode. Only 1,001 durations are retained at once,
and setup is sequential rather than an accumulating batch of owned fixtures.

Every per-call timer includes `Instant`/black-box overhead. Input construction or
cloning, result assertions, returned-value destruction, sorting and reporting
are excluded. Criterion receives the sum of individual call durations; untimed
work still consumes wall time. These short-call medians should be compared using
the same harness and host, and are not interchangeable with historical Criterion
estimates. Every valid parsing scene must succeed, and expected rejected atomic
operations must return the full rejection list.

## Performance guarantees

All six rows are advisory latency trends, with no fixed percentage or timing
threshold. No new allocation/count gate is introduced. Historical values describe
the recorded release and are not new measurements or promises.

| Operation              | Input                         | Metric         | Bound                                        | Baseline             |
| ---------------------- | ----------------------------- | -------------- | -------------------------------------------- | -------------------- |
| parse single (simple)  | `verb(arg="value")`           | latency median | Advisory trend; last ledger value 338.3 ns   | historical: 338.3 ns |
| parse single (complex) | memory.remember 5 args + UUID | latency median | Advisory trend; last ledger value 1.390 µs   | historical: 1.390 µs |
| parse batch (10 ops)   | 10-op parallel batch          | latency median | Advisory trend; last ledger value 7.083 µs   | historical: 7.083 µs |
| parse chain (5 ops)    | 5-op chain with $prev         | latency median | Advisory trend; last ledger value 2.948 µs   | historical: 2.948 µs |
| parse batch (100 ops)  | 100-op batch (MAX_OPS)        | latency median | Advisory trend against the recorded baseline | pending              |
| plan_request           | 10-op batch                   | latency median | Advisory trend against the recorded baseline | pending              |
