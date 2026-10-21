# khive-pack-comm Benchmark Ledger

<!-- bench-reference:begin -->

Generated from `perf/reference.json` by `scripts/render-bench-reference.py`; do not edit this block.

Reference baselines for `khive-pack-comm`. Values are the committed reference median for each benchmark, in the unit shown; A/A is the ratio between the two same-commit runs. The date is the committer date of the baseline commit.

| Benchmark                          | Reference median | A/A ratio | Baseline commit                            | Commit date |
| ---------------------------------- | ---------------- | --------- | ------------------------------------------ | ----------- |
| `comm/inbox/10`                    | 309.641 µs       | 1.003     | `212bf72e498de9fbbda42cc0e2902906c730e3ea` | 2026-10-17  |
| `comm/inbox/100`                   | 440.808 µs       | 1.007     | `212bf72e498de9fbbda42cc0e2902906c730e3ea` | 2026-10-17  |
| `comm/read`                        | 141.573 µs       | 1.018     | `212bf72e498de9fbbda42cc0e2902906c730e3ea` | 2026-10-17  |
| `comm/reply`                       | 722.030 µs       | 1.004     | `212bf72e498de9fbbda42cc0e2902906c730e3ea` | 2026-10-17  |
| `comm/send`                        | 411.994 µs       | 1.042     | `212bf72e498de9fbbda42cc0e2902906c730e3ea` | 2026-10-17  |
| `comm/thread/5`                    | 196.582 µs       | 1.007     | `212bf72e498de9fbbda42cc0e2902906c730e3ea` | 2026-10-17  |
| `comm/thread_in_busy_mailbox/2000` | 201.326 µs       | 1.001     | `212bf72e498de9fbbda42cc0e2902906c730e3ea` | 2026-10-17  |

<!-- bench-reference:end -->

## Benchmark Inventory

| Name         | File                    | Purpose                                                                                                                                                                                                                      |
| ------------ | ----------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `comm_bench` | `benches/comm_bench.rs` | Criterion suite — `send` write latency, `inbox` listing over 10/100 seeded messages, `read` mark-as-read on a fresh inbound copy, `reply` write latency (threaded), `thread/5` full thread retrieval over a 5-message thread |

## Scenarios

| Scenario         | What it measures                                                                                    |
| ---------------- | --------------------------------------------------------------------------------------------------- |
| `comm/send`      | Single `comm.send` write latency — dual-write path (outbound + inbound copy)                        |
| `comm/inbox/10`  | `comm.inbox(status=all, limit=20)` over a 10-message inbox                                          |
| `comm/inbox/100` | `comm.inbox(status=all, limit=20)` over a 100-message inbox — measures SQL filter + pagination cost |
| `comm/read`      | `comm.read` on a fresh inbound message — note fetch + property merge + upsert                       |
| `comm/reply`     | `comm.reply` write latency — root note fetch + `dual_write_message`                                 |
| `comm/thread/5`  | `comm.thread` retrieval of a 5-message thread — SQL property filter + chronological sort            |

## Run Commands

```bash
# Smoke-test mode — compile and single iteration, no timing output
cd crates && cargo bench -p khive-pack-comm --bench comm_bench -- --test

# Full Criterion run with HTML reports
cd crates && cargo bench -p khive-pack-comm --bench comm_bench

# Single scenario
cd crates && cargo bench -p khive-pack-comm --bench comm_bench -- comm/send
```

## Release Ledger

### v0.2.8 - 2026-06-08

- Commit: `d3629501c550fd2f3bb7ed350a2b60309d596465`
- Crate version: `0.2.8`
- Khive version: `0.2.8`
- Toolchain: `rustc 1.94.1 (e408947bf 2026-03-25)`, release profile (Criterion)
- Machine: Apple M-series arm64, macOS Darwin 25.5.0, 16 GB
- Feature flags: default
- Command: `cd crates && cargo bench -p khive-pack-comm --bench comm_bench`
- Dataset: in-memory SQLite; inbox seeded with 10 / 100 messages; thread seeded with 5 messages;
  sample size 50
- vs prior: first formal release ledger entry — no prior comparable baseline

| Scenario       | Low      | Median   | High     | Outliers   |
| -------------- | -------- | -------- | -------- | ---------- |
| comm/send      | 6.346 ms | 7.200 ms | 8.162 ms | 5/50 (10%) |
| comm/inbox/10  | 127.7 µs | 131.5 µs | 136.5 µs | 4/50 (8%)  |
| comm/inbox/100 | 417.4 µs | 424.2 µs | 431.1 µs | 4/50 (8%)  |
| comm/read      | 85.79 µs | 93.64 µs | 103.5 µs | 2/50 (4%)  |
| comm/reply     | 4.262 ms | 4.602 ms | 4.903 ms | 1/50 (2%)  |
| comm/thread/5  | 144.6 µs | 151.4 µs | 161.3 µs | 1/50 (2%)  |

- Notes: none

Last reviewed: v0.2.8 (2026-06-08)

## Dispatch lifecycle coverage

The `comm_lifecycle` target adds generated, isolated in-process dispatch workloads.
Every operation gets a fresh no-embedding in-memory runtime, a configured
`benchmark:comm` actor and the real KG + comm registry with its trusted channel-ingest
capability and audit sink. It does not contact a mailbox or transport. The real
`CommPack` owns its private `InboxSignal`; the benchmark does not construct a stand-in.
`ingest`, quarantine cleanup and the cursor pair are subhandlers reachable through
public Rust registry dispatch, not public MCP verbs.

| Group                    | Scenario                         | Input and timed operation                                                                     | Untimed result checks                                                                            |
| ------------------------ | -------------------------------- | --------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------ |
| `comm_lifecycle`         | `ingest`                         | One generated inbound message; one `comm.ingest` dispatch                                     | Fresh, non-deduplicated UUID; no committed-write degradation; actual inbound note and recipient  |
| `comm_lifecycle`         | `cleanup_expired_quarantine/100` | 100 notes ingested with quarantine metadata; one cleanup dispatch                             | All 100 hard-deleted; no routed-owner detachment; each UUID absent                               |
| `comm_lifecycle`         | `cursor_get`                     | One seeded cursor at generation 1 / high-water 100; one get dispatch                          | Returned checkpoint remains at 100 and equals an additional persisted read                        |
| `comm_lifecycle`         | `cursor_commit`                  | One seeded cursor at generation 1 / high-water 100; one commit to high-water 101               | Receipt is at 101 and equals an additional persisted read                                         |
| `comm_lifecycle`         | `cursor_get_commit`              | One seeded cursor at generation 1 / high-water 100; get then commit high-water 101            | Old read, new receipt and an additional persisted read agree                                     |
| `comm_lifecycle`         | `mark_read/200`                  | 200 distinct unread inbound notes; one `comm.mark_read(atomic=false)` dispatch                | Requested, unique and marked counts all 200; no failures or unknowns; all stored read flags true |
| `comm_lifecycle`         | `suppression_list/100`           | 100 unique outbound address suppressions seeded via `comm.suppression_add`; one list dispatch | Exact 100-entry identity/value set, with limit 100 and offset 0                                  |
| `comm` (existing target) | `thread_in_busy_mailbox/2000`    | 2000 unrelated messages plus the existing five-message thread                                 | Existing thread benchmark is preserved                                                           |

The quarantine cutoff is one microsecond after the latest actual stored retention
deadline. No sleep or row rewrite makes it eligible. This workload fits the cleanup
handler's 128-row page and contains no retained attachment originals, so it measures
note cleanup rather than attachment garbage collection. Cursor get and commit both
execute the handler's writer-side schema bootstrap; the get is not a read-only SQL
operation. The isolated get and commit scenarios use the same seeded checkpoint
and handler paths as the existing pair, separating their costs without replacing
the round-trip measurement. They do not measure first-time schema creation or
persistent-store writer-queue contention. Suppression operations use the configured
maintainer identity.

Fixture construction, all seeding, JSON argument preparation, receipt/state checks
and destruction are outside each timer. The timer includes executor entry, registry
dispatch, handler work and the normal dispatch audit submission/wait path. Each
operation finishes its untimed checks, drains and joins its audit batch, and drops
the registry/runtime before the next fixture is created. The memory backend has no
queued writer; the benchmark asserts that invariant. Only one fixture is live at a time, using a
current-thread executor with at most two blocking threads.

Each new scenario prints a JSON report from 11 independent actual operations: the
median is sorted rank 6, reported in both nanoseconds and milliseconds per operation.
The cleanup operation is the entire 100-note dispatch; mark-read is the entire
200-message dispatch; `cursor_get_commit` includes both dispatch calls, while
`cursor_get` and `cursor_commit` each include exactly one. These are
not per-message latencies or batch-average percentiles. The separate Criterion
trend uses sequential `iter_custom` samples, sample size 10, one-second warmup and
three-second measurement targets. Fixture preparation and checks still consume wall
time even though they are excluded from the returned measured duration. The old
`comm_bench` workloads and their sampling settings are unchanged.

### Execution

```bash
cd crates
# All existing and new targets; each lifecycle scenario also prints its median report.
cargo bench -p khive-pack-comm --benches -- --quick --noplot
# The new dispatch target on its own.
cargo bench -p khive-pack-comm --bench comm_lifecycle -- --quick --noplot
# Test mode executes the target's fixture/receipt checks without a Criterion timing run.
cargo bench -p khive-pack-comm --bench comm_lifecycle -- --test
```

The target's 11-sample JSON reporting pass precedes Criterion dispatch. It still
runs for every scene in test mode and when a Criterion name filter is supplied;
a filtered invocation is not a promise to construct only that scene's fixtures.

## Performance guarantees

| Operation                              | Input                      | Metric         | Bound                                                                           |
| -------------------------------------- | -------------------------- | -------------- | ------------------------------------------------------------------------------- |
| `comm.send`                            | fresh fixture, single send | latency median | Advisory trend; last ledger value 7.200 ms median (`docs/benchmarks.md` v0.2.8) |
| `comm.inbox(100, limit=20)`            | 100 seeded messages        | latency median | Advisory trend; last ledger value 424.2 µs median (`docs/benchmarks.md` v0.2.8) |
| `comm.thread/5` in busy mailbox (2000) | 2000 unrelated + 5-thread  | latency median | Advisory scaling trend: median does not grow with mailbox size                  |
| `comm.ingest`                          | single message             | latency median | Advisory trend against the recorded baseline                                    |
| `comm.cleanup_expired_quarantine`      | 100 quarantined notes      | latency median | Advisory trend against the recorded baseline                                    |
| `comm.cursor_get` + `cursor_commit`    | single round-trip          | latency median | Advisory trend against the recorded baseline                                    |

All rows are advisory timing trends against rolling and fixed references, not fixed
percentage gates. A historical observation is not a latency promise. The busy-mailbox
scaling row is an intended comparison with `comm/thread/5`, not a measured conclusion
in this ledger. Benchmark fixture assertions check that the named work actually
happened; they do not introduce deterministic performance guarantees or ordinary
allocation/count tests. `mark_read/200` and `suppression_list/100` add timing coverage
without inventing additional guarantee rows.

## Pending baseline ledger

Producer: the `bench-track` component owning `khive-pack-comm`, on a qualified host.
A measured entry must record the commit, toolchain, machine, fixture sizes and command.
The v0.2.8 ledger above remains historical; none of its numbers are measurements of
these new workloads. Pipeline selection is maintained by the benchmark workflow's
owning component.

| Group / scenario                                | Reported unit                                              | Baseline | Commit  | Toolchain / machine |
| ----------------------------------------------- | ---------------------------------------------------------- | -------- | ------- | ------------------- |
| `comm_lifecycle/ingest`                         | Median ns per single dispatch                              | pending  | pending | pending             |
| `comm_lifecycle/cleanup_expired_quarantine/100` | Median ms per 100-note cleanup dispatch                    | pending  | pending | pending             |
| `comm_lifecycle/cursor_get`                     | Median ns per single get dispatch                         | pending  | pending | pending             |
| `comm_lifecycle/cursor_commit`                  | Median ns per single commit dispatch                      | pending  | pending | pending             |
| `comm_lifecycle/cursor_get_commit`              | Median ns per get + commit round-trip                      | pending  | pending | pending             |
| `comm_lifecycle/mark_read/200`                  | Median ns per 200-message dispatch                         | pending  | pending | pending             |
| `comm_lifecycle/suppression_list/100`           | Median ns per 100-entry list dispatch                      | pending  | pending | pending             |
| `comm/thread_in_busy_mailbox/2000`              | Existing Criterion estimate, compared with `comm/thread/5` | pending  | pending | pending             |
