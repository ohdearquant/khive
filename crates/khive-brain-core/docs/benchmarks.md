# khive-brain-core Benchmark Ledger

<!-- bench-reference:begin -->

Generated from `perf/reference.json` by `scripts/render-bench-reference.py`; do not edit this block.

Reference baselines for `khive-brain-core`. Values are the committed reference median for each benchmark, in the unit shown; A/A is the ratio between the two same-commit runs. The date is the committer date of the baseline commit.

| Benchmark                                                       | Reference median | A/A ratio | Baseline commit                            | Commit date |
| --------------------------------------------------------------- | ---------------- | --------- | ------------------------------------------ | ----------- |
| `core_primitives/apply_signal/feedback_useful`                  | 54.861 ns        | 1.003     | `212bf72e498de9fbbda42cc0e2902906c730e3ea` | 2026-10-17  |
| `core_primitives/apply_signal/recall_hit`                       | 54.505 ns        | 1.010     | `212bf72e498de9fbbda42cc0e2902906c730e3ea` | 2026-10-17  |
| `core_primitives/apply_signal/recall_miss`                      | 14.800 ns        | 1.020     | `212bf72e498de9fbbda42cc0e2902906c730e3ea` | 2026-10-17  |
| `core_primitives/ess_cap/section_posterior_capped/100`          | 9.027 µs         | 1.010     | `212bf72e498de9fbbda42cc0e2902906c730e3ea` | 2026-10-17  |
| `core_primitives/ess_cap/section_posterior_capped/200`          | 17.946 µs        | 1.010     | `212bf72e498de9fbbda42cc0e2902906c730e3ea` | 2026-10-17  |
| `core_primitives/ess_cap/section_posterior_capped/50`           | 4.316 µs         | 1.006     | `212bf72e498de9fbbda42cc0e2902906c730e3ea` | 2026-10-17  |
| `core_primitives/section_weights/deterministic/after_50_events` | 746.442 ns       | 1.008     | `212bf72e498de9fbbda42cc0e2902906c730e3ea` | 2026-10-17  |
| `core_primitives/section_weights/deterministic/default_priors`  | 747.646 ns       | 1.010     | `212bf72e498de9fbbda42cc0e2902906c730e3ea` | 2026-10-17  |
| `core_primitives/snapshot/brain_state/roundtrip`                | 2.032 µs         | 1.021     | `212bf72e498de9fbbda42cc0e2902906c730e3ea` | 2026-10-17  |
| `core_primitives/snapshot/from_snapshot/restore`                | 285.792 ns       | 1.013     | `212bf72e498de9fbbda42cc0e2902906c730e3ea` | 2026-10-17  |
| `core_primitives/snapshot/to_snapshot/100_events`               | 63.871 ns        | 1.015     | `212bf72e498de9fbbda42cc0e2902906c730e3ea` | 2026-10-17  |
| `core_primitives/snapshot/to_snapshot/fresh`                    | 15.964 ns        | 1.018     | `212bf72e498de9fbbda42cc0e2902906c730e3ea` | 2026-10-17  |

<!-- bench-reference:end -->

These Criterion measurements cover in-memory primitives. They exclude registry
dispatch, authorization, persistence, event append and transport; they do not
represent the public `brain.*` verb cost. Use the brain pack benchmarks for that
boundary.

From `crates/`, run:

```sh
cargo bench -p khive-brain-core --bench brain_core_bench -- --quick --noplot
```

## Scenario ledger

The `brain_core_bench` target contains 15 scenarios in five groups. Names below
are relative to `core_primitives/`.

| Group                 | Scenarios                                                       | Input and measured operation                                                            | Baseline |
| --------------------- | --------------------------------------------------------------- | --------------------------------------------------------------------------------------- | -------- |
| `snapshot`            | `to_snapshot/fresh`, `to_snapshot/100_events`                   | Snapshot fresh state or 50 hits + 50 misses, capacity 100                               | pending  |
| `snapshot`            | `from_snapshot/restore`                                         | Restore a snapshot of the 100-event state; snapshot clone is setup                      | pending  |
| `snapshot`            | `brain_state/roundtrip`                                         | Snapshot and restore complete BrainState after 50 hits                                  | pending  |
| `ess_cap`             | `section_posterior_capped/50`, `/100`, `/200`                   | Apply N opposing section signals after N positive signals prepared outside timing       | pending  |
| `apply_signal`        | `recall_hit`, `recall_miss`, `feedback_useful`                  | Apply one signal to fresh capacity-100 state                                            | pending  |
| `section_weights`     | `deterministic/default_priors`, `deterministic/after_50_events` | Derive section weights; see the unchanged legacy setup caveat below                     | pending  |
| `feedback_posteriors` | `useful`, `not_useful`, `wrong`                                 | One posterior-only feedback call after exactly 50 events, capacity 100, existing target | pending  |

Every row awaits a qualified-host `storage-brain` component run. Each
baseline must record the measured commit, Rust toolchain, machine, command,
scenario and result. No historical measurement has been inferred from a passing
smoke run. Timing is an advisory comparison against rolling and fixed references.

## Timing boundaries and state

The new `feedback_posteriors` group uses target UUID 42 and 25 recall hits
interleaved with 25 misses. It retains one entity posterior in capacity-100 state.
Each measured call starts from that same snapshot; cloning and restoring the
state, allocating its map and dropping it are outside the timer. Criterion
`iter_batched_ref` with `PerIteration` keeps one input per iteration and avoids
posterior growth across samples. Timer overhead remains part of these small-call
measurements. The three cases cover the legacy useful/not-useful/wrong vocabulary
on an existing entity; semantic weighted feedback and new-entity admission are
different workloads and are not represented by these cases.

A setup assertion checks the complete resulting snapshot: salience and the
target entity posterior change once, while total events, exploration epoch,
relevance and temporal posteriors remain unchanged. In this primitive both
`not_useful` and `wrong` add one failure; the section-feedback weighting rule is a
different path. Criterion supplies per-call latency distributions, typically
displayed in nanoseconds for this operation.

Existing scenarios retain their prior code and measurement boundaries:

- `to_snapshot/100_events` contains 100 events, not 50: 50 hits followed by 50
  misses. `brain_state/roundtrip` includes both snapshot and restore.
- The ESS group times all N opposing events together, including construction of
  each signal's section map and destruction of the local state. It is not a
  single-event latency. The existing untimed 200-positive/200-opposing assertion
  requires a posterior mean shift of at least 0.3. The 50-event setup does not
  start at the ESS cap; its later opposing events cross it.
- The legacy `section_weights/deterministic/after_50_events` setup calls
  `apply_signal` 50 times with `section_signals: None`. The section handler ignores
  those signals, so this scenario still derives weights from unchanged priors;
  its name must not be cited as evidence of a learned section posterior. It is
  documented here without changing the existing comparison series.

## Performance guarantees

| Operation                                 | Input                                                 | Metric         | Bound                                        |
| ----------------------------------------- | ----------------------------------------------------- | -------------- | -------------------------------------------- |
| `to_snapshot` (100 events)                | BalancedRecallState, 50 hits + 50 misses              | latency median | Advisory trend against the recorded baseline |
| `from_snapshot` restore                   | BalancedRecallSnapshot, capacity 100                  | latency median | Advisory trend against the recorded baseline |
| `apply_signal` (recall_hit)               | BalancedRecallState::new(100), single signal          | latency median | Advisory trend against the recorded baseline |
| `section_posterior_capped` (200 opposing) | SectionPosteriorState, 200 positive then 200 opposing | latency median | Advisory trend against the recorded baseline |
| `apply_feedback_posteriors`               | BalancedRecallState, 50 events, single feedback call  | latency median | Advisory trend against the recorded baseline |

All five rows describe timing trends, not fixed percentage failures. The section
row measures a batch of 200 opposing events. Setup assertions establish workload
identity; this table introduces no new deterministic count or allocation bound.
