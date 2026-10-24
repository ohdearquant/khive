# Brain pack benchmarks

<!-- bench-reference:begin -->

Generated from `perf/reference.json` by `scripts/render-bench-reference.py`; do not edit this block.

Reference baselines for `khive-pack-brain`. Values are the committed reference median for each benchmark, in the unit shown; A/A is the ratio between the two same-commit runs. The date is the committer date of the baseline commit.

| Benchmark                                                                                         | Reference median | A/A ratio | Baseline commit                            | Commit date |
| ------------------------------------------------------------------------------------------------- | ---------------- | --------- | ------------------------------------------ | ----------- |
| `khive-pack-brain:brain_pack_bench/brain_pack/feedback_plain/useful/median_ns`                    | 719.028 µs       | 1.028     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `khive-pack-brain:brain_pack_bench/brain_pack/feedback_sections/capped_200_opposing/median_ns`    | 45.498 ms        | 1.012     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `khive-pack-brain:brain_pack_bench/brain_pack/profile_read/balanced_recall_v1/median_ns`          | 43.624 µs        | 1.001     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `khive-pack-brain:brain_pack_bench/brain_pack/reset/balanced_recall_v1/median_ns`                 | 605.254 µs       | 1.010     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `khive-pack-brain:brain_pack_bench/brain_pack/section_posterior_gate/mean_shift_ge_0_3/median_ns` | 0.035 ns         | 1.000     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `khive-pack-brain:brain_pack_bench/brain_pack/snapshot_roundtrip/state_read_twice/median_ns`      | 90.948 µs        | 1.006     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |
| `khive-pack-brain:brain_resolve/brain_pack/resolve/100_bindings/median_ns`                        | 42.078 µs        | 1.000     | `46b7d9c5f8dd0b019a0af0ab2410aa0242b4122d` | 2026-10-24  |

<!-- bench-reference:end -->

These benchmarks call public verbs through a real BrainPack, VerbRegistry and private in-memory
KhiveRuntime. They measure dispatch behavior rather than the isolated khive-brain-core primitives.
Fixtures are generated locally; no external corpus, model provider or network service is needed.

## Scenarios and baselines

| Target / group                                       | Scenario            | Actual timed work                                                                     | Input / setup                                                                   | Baseline                                     |
| ---------------------------------------------------- | ------------------- | ------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------- | -------------------------------------------- |
| brain_pack_bench / brain_pack/feedback_plain         | useful              | One brain.feedback dispatch and owned fixture destruction                             | Fresh registry and real target entity prepared outside timing                   | pending latency                              |
| brain_pack_bench / brain_pack/feedback_sections      | capped_200_opposing | 200 opposing feedback dispatches, argument construction and owned fixture destruction | Fresh section profile and 200 positive setup events                             | pending time per 200 dispatches              |
| brain_pack_bench / brain_pack/section_posterior_gate | mean_shift_ge_0_3   | Trivial unit-value Criterion loop; correctness assertion runs before it               | 200 positive then 200 opposing section events; required mean shift at least 0.3 | correctness gate, no useful latency baseline |
| brain_pack_bench / brain_pack/profile_read           | balanced_recall_v1  | One profile read, argument construction and result destruction                        | Reused registry and default profile                                             | pending latency                              |
| brain_pack_bench / brain_pack/reset                  | balanced_recall_v1  | One reset dispatch and owned registry destruction                                     | Fresh registry; empty arguments select the default profile                      | pending latency                              |
| brain_pack_bench / brain_pack/snapshot_roundtrip     | state_read_twice    | Two state serializations and result destruction                                       | Reused registry                                                                 | pending time per two reads                   |
| brain_resolve / brain_pack/resolve                   | 100_bindings        | One public resolution with the real dispatch, load guard, state lock and binding scan | 100 distinct actor bindings; last actor is the configured caller                | pending latency and calls/sec                |

Baselines are pending the qualified-host storage-brain producer. Record the full commit, toolchain,
machine, load and command with each future measurement. There is no measured baseline supplied here.
The existing workloads, correctness assertions and timing boundaries are retained.

## Selection and execution modes

Both targets construct executors and database fixtures only inside selected Criterion callbacks.
`--list` and unmatched filters perform no brain fixture construction or verb dispatch. A selected
default or `--test` run executes one Criterion correctness iteration without supplemental timing
samples; `--test` takes precedence over `--bench`.

The section-posterior gate remains a separate selectable scenario. Selecting
`brain_pack/section_posterior_gate/mean_shift_ge_0_3` runs its unchanged 200 positive and 200 opposing
feedback assertions once, outside its trivial timed loop, including in correctness and profile
modes. Selecting another scenario does not run this independent gate. The capped section-feedback
scenario still primes 200 events and executes 200 opposing events per Criterion iteration.

Only an ordinary selected `--bench` resolution run prints the 101-sample supplemental report, once
before Criterion iterations. Default, `--test`, list, unmatched-filter and `--profile-time` modes
print no supplemental report. Mode options after `--` are filter text rather than mode switches.
Profiling can include lazy setup and the selected correctness gate; the reported Criterion duration
still uses the existing timing boundary.

## Resolution fixture and timing boundary

The new target uses one current-thread executor with at most two blocking threads and one runtime
for its selected run. KhiveRuntime::memory disables embeddings and uses a private in-memory store.
It registers the real KG and brain packs under caller brain-resolve-099. Setup dispatches brain.bind
100 times with distinct actor labels brain-resolve-000 through brain-resolve-099, consumer recall,
wildcard namespace and profile balanced-recall-v1. Each public acknowledgment must match its input.
The storage namespace remains fixed throughout; varying it would switch the brain's namespace
state slot instead of populating one 100-binding fixture.

An untimed brain.state snapshot proves the exact 100 unique actor rows and their profile, namespace,
consumer and priority fields. The caller-filtered brain.bindings view must contain exactly its
one row. A warm resolution and every measured result must identify balanced-recall-v1 with
matched_binding=true and requested/matched consumer recall. A system-default fallback therefore
fails the benchmark even if it names the same profile. The post-run snapshot must preserve all
binding and profile values.

Input JSON cloning, fixture construction, result checks and result destruction are outside the
new target's measured interval. The timer includes executor block_on, actual public dispatch and
response construction. Even a loaded namespace checks its stored snapshot version through SQL
before returning from ensure_loaded, so this is not a pure in-memory binding-scan microbenchmark.
It does not time 100 binding writes or a newly constructed registry per resolve. Each Criterion
iteration is one resolution; the custom timer sums those call intervals. State inspection itself
increments serialization diagnostics, which is why the post-run check compares bindings and
profiles rather than asserting equality of the full diagnostic snapshot.

An ordinary selected benchmark run also prints a fixed report of 101 individually timed calls
before Criterion sampling.
The median is the 51st sorted observation and nearest-rank p99 is the 100th. These are individual
call samples, not percentiles of Criterion batch means. The fixed report runs even with --quick
when ordinary benchmark mode is selected.
No timing threshold or allocation claim is enforced for resolution.

## Reading the existing measurements correctly

The existing section scenario is capped_200_opposing, not a single with_sections call: it prepares
200 positive events and times 200 opposing events. Its separate mean-shift gate is substantive;
the trivial Criterion loop registered after that assertion is not feedback timing.

The existing fresh-fixture scenarios use iter_batched with SmallInput and consume the registry in
the timed closure. Their timing includes owned fixture destruction and their batching may retain
multiple prepared registries. The new target instead retains one registry and times calls only;
these different boundaries must be retained in comparisons.

The existing state_read_twice scenario checks only the optional top-level total_events fields
with debug_assert_eq. The actual snapshot keeps counters inside its state records, so that
comparison is not proof of full snapshot equality and is disabled in optimized builds. The
benchmark is a two-read timing scenario, not an independent stability gate. Its timed body is preserved.

brain.resolve uses the same binding-resolution primitive as the feedback fallback path, but a
known explicit served_by_profile_id bypasses that lookup. This benchmark must not be described as
overhead paid by every feedback call carrying an explicit profile.

## Performance guarantees

| Operation                        | Input                                | Metric         | Bound                                        |
| -------------------------------- | ------------------------------------ | -------------- | -------------------------------------------- |
| `brain.feedback` (plain)         | fresh registry, single useful signal | latency median | Advisory trend against the recorded baseline |
| `brain.feedback` (with sections) | section profile, single feedback     | latency median | Advisory trend against the recorded baseline |
| `brain.profile`                  | balanced-recall-v1 profile           | latency median | Advisory trend against the recorded baseline |
| `brain.reset`                    | fresh registry                       | latency median | Advisory trend against the recorded baseline |
| `brain.state` (read twice)       | fresh registry                       | latency median | Advisory trend against the recorded baseline |
| `brain.resolve`                  | 100 bindings, single resolve         | latency median | Advisory trend against the recorded baseline |

This table retains the requested guarantee rows. The existing section benchmark measures a
200-feedback batch, so it cannot establish the table's single-feedback baseline without a
separately specified measurement. All timing rows remain advisory. The existing posterior
correctness assertion remains active and is not replaced by a timing threshold.

## Execution

```sh
cargo bench -p khive-pack-brain --benches -- --quick --noplot
cargo bench -p khive-pack-brain --bench brain_resolve -- --quick --noplot
```

The first command includes the existing suite and its section-posterior gate; the second isolates
the new resolution target. Workflow selection and qualified-host baseline recording are managed
separately from these benchmark sources.
