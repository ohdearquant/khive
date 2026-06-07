# khive-request Benchmark Ledger

## Run command

```bash
cargo bench -p khive-request --bench request_bench
```

Test-only (no timing, validates correctness):

```bash
cargo bench -p khive-request --bench request_bench -- --test
```

HTML reports land in `target/criterion/` when gnuplot or plotters is available.

## Scenarios

| ID | Group | Name | Input description |
|----|-------|------|-------------------|
| 1 | `parse/single` | `simple` | `verb(arg="value")` — minimal single op |
| 2 | `parse/single` | `complex` | `memory.remember(...)` with 5 args including UUID — realistic prod call |
| 3 | `parse/batch` | `3` | `[create(...), search(...), get(...)]` — small parallel batch |
| 4 | `parse/batch` | `10` | Generated 10-op parallel batch |
| 5 | `parse/chain` | `2` | `create(...) | link(..., source_id=$prev.id, ...)` — minimal chain with $prev |
| 6 | `parse/chain` | `5` | Generated 5-op chain with $prev propagation |
| 7 | `parse/json_form` | `3_ops` | `[{"tool":"...","args":{...}},...]` — JSON-form 3-op batch |

## Baseline (2026-06-06, post-sweep)

**Toolchain:** rustc 1.94.1 (e408947bf 2026-03-25)
**Machine:** arm64 (Apple Silicon), macOS Darwin 25.5.0

| Scenario | Low | Median | High | Outliers |
| --- | --- | --- | --- | --- |
| parse/single/simple | 303.9 ns | 338.3 ns | 376.7 ns | 35/200 (18%) |
| parse/single/complex | 1.247 µs | 1.390 µs | 1.551 µs | 29/200 (15%) |
| parse/batch/3 | 1.008 µs | 1.131 µs | 1.275 µs | 10/100 (10%) |
| parse/batch/10 | 6.148 µs | 7.083 µs | 8.111 µs | 16/100 (16%) |
| parse/chain/2 | 1.195 µs | 1.354 µs | 1.533 µs | 8/100 (8%) |
| parse/chain/5 | 2.723 µs | 2.948 µs | 3.212 µs | 19/100 (19%) |
| parse/json_form/3_ops | 1.947 µs | 2.107 µs | 2.301 µs | 17/100 (17%) |
