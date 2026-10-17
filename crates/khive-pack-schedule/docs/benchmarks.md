# khive-pack-schedule Benchmark Ledger

## Run command

```bash
# from workspace root
cd crates && cargo bench -p khive-pack-schedule --bench schedule_bench

# compile + smoke-check only (no timing)
cd crates && cargo bench -p khive-pack-schedule --bench schedule_bench -- --test
```

HTML reports land in `target/criterion/schedule/`.

---

## Scenarios

| Benchmark             | Description                                                                          | Setup                                                                |
| --------------------- | ------------------------------------------------------------------------------------ | -------------------------------------------------------------------- |
| `schedule/remind`     | Write latency for a single `schedule.remind` call.                                   | One fixture runtime built outside the timed loop via `iter_batched`. |
| `schedule/schedule`   | Write latency for a single `schedule.schedule` call (includes DSL parse validation). | One fixture runtime built outside the timed loop via `iter_batched`. |
| `schedule/agenda/10`  | `schedule.agenda(limit=20)` over a corpus of 10 seeded events.                       | 10 events seeded once before the group.                              |
| `schedule/agenda/100` | `schedule.agenda(limit=20)` over a corpus of 100 seeded events.                      | 100 events seeded once before the group.                             |
| `schedule/cancel`     | `schedule.remind` + `schedule.cancel` — inline create+cancel per iteration.          | No pre-seeding; each iteration creates then cancels.                 |

---

## Recurrence and 1,000-event agenda coverage

The additive `schedule_scale` target exercises public recurrence advancement and
registry dispatch over an isolated, fully verified 1,000-event store. The
original `schedule_bench` and historical ledger remain unchanged. New baselines
are pending qualified-host measurement; historical values are not current
performance promises.

```sh
cd crates
cargo bench -p khive-pack-schedule --benches -- --quick --noplot
cargo bench -p khive-pack-schedule --bench schedule_scale -- --noplot
cargo bench -p khive-pack-schedule --bench schedule_scale -- --test
cargo bench -p khive-pack-schedule --bench schedule_scale -- --list
```

| Scene                                | Input                                                                                | Timed work                                                                     | Untimed checks                                                                                                |
| ------------------------------------ | ------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------- |
| `recurrence/cron_next_after/1000`    | Publicly parsed `0 * * * *`, UTC base 2100-01-01, 1,000 successive steps             | Actual `Repeat::next_after` calls and writes to preallocated timestamp slots   | Every timestamp equals base + step hours; exact final timestamp and increasing order                          |
| `recurrence/cron_first_after/1000`   | Same cron, each `now` is 30 minutes after the previous trigger                       | Actual `Repeat::first_after` calls, `now` arithmetic and timestamp-slot writes | Same exact 1,000 hourly occurrences, each strictly after its supplied `now`                                   |
| `schedule_scale/agenda/1000_limit20` | 1,000 real `schedule.remind` writes, unique hourly future timestamps, explicit actor | Current-thread executor entry and public `schedule.agenda(limit=20)` dispatch  | Exact first 20 IDs, content, status, timestamps, actor and continuation; full corpus pagination before timing |

Cron parsing and its occurrence validation happen once outside the timing
boundary. Each recurrence iteration starts at the same fixed UTC base; it never
continues drifting through the calendar across samples. Output slots are allocated
before the timer and overwritten on every call. All 1,000 timestamps are checked
afterward, rather than just checking that one timestamp advanced.

Each selected recurrence scene first prints one actual 1,000-step batch with
`single_batch_average`, total nanoseconds and `ns_per_step`. This is an elapsed
batch cost divided by 1,000, not the median or p99 of separately timed steps.
Criterion then measures full 1,000-step batches and reports throughput for 1,000
elements. Divide its batch timing estimate by 1,000 for an average-step estimate;
the harness does not divide the duration supplied to Criterion or inflate sample
counts. Clock and timestamp-slot overhead are included.

The agenda fixture creates its memory runtime under an entered current-thread
Tokio executor, capped at two blocking workers. Models, mounts, sidecar files and
ambient credentials are disabled. It registers KG, comm and schedule packs; comm
registration supplies the reminder capability, but no message is delivered and no
schedule executor runs. Every seed uses the real reminder handler, including
provenance and activation. The fixture asserts no file/ANN root, embedding model
or queued writer is active. Registry audit work is drained at normal teardown.

An untimed census follows `next.after` and `next.after_id` verbatim through five
200-item pages and a final empty page. It verifies all 1,000 receipt UUIDs exactly
once and all event fields used by this fixture. Agenda's `count` is the page count,
not a corpus total; `limit=1000` is not supported. The measured request still asks
for 20, and its complete expected first page and cursor are checked after each
call. A repeated read reuses the same event corpus; no reminders are written in
the timed loop.

Current agenda implementation pushes pending/time filters, ordering and keyset
continuation into its count-free storage query. This benchmark measures the real
operation over 1,000 stored events; it does not assert that every call performs a
full linear Rust scan. The original target's `schedule/schedule` case currently
reuses its store across writes, whereas its `remind` and `cancel` cases construct
fixtures. The older ledger and scenario summary above are historical descriptions;
this additive target does not change those workloads or reinterpret their numbers.

Fixtures are lazy within selected callbacks, so `--list` and recurrence-only
selection do not seed an agenda. The new target requests ten flat Criterion
samples, 100 ms warmup and one second of measurement, before command-line
overrides. These parameters are not an elapsed-time bound. Per-call request
construction, output validation and result destruction are outside agenda timing;
`block_on`, dispatch and their internal work remain inside. Iterations complete
sequentially without retaining batches of fixtures or results.

## Current baseline ledger

The owning `packs` bench-track component collects these baselines on a
qualified host. Record the commit, toolchain/profile/features, CPU/OS, command,
fixture and raw report with each measurement. Rerunning original groups is also
required before comparing their historical values with a new baseline.

| Group                                                   | Current baseline                     | Commit / toolchain / machine |
| ------------------------------------------------------- | ------------------------------------ | ---------------------------- |
| Original remind / schedule / agenda 10 and 100 / cancel | Pending qualified-host remeasurement | Pending                      |
| Hourly cron next_after and first_after, 1,000 advances  | Pending qualified-host measurement   | Pending                      |
| Agenda over 1,000 events, limit 20                      | Pending qualified-host measurement   | Pending                      |

## Performance guarantees

| Operation                                | Input                     | Metric                  | Bound                                                      |
| ---------------------------------------- | ------------------------- | ----------------------- | ---------------------------------------------------------- |
| `schedule.remind`                        | single write, in-memory   | latency median          | Advisory trend; last ledger value 3.247 ms median (v0.2.8) |
| `schedule.agenda(100, limit=20)`         | 100 seeded events         | latency median          | Advisory trend; last ledger value 424.9 µs median (v0.2.8) |
| `schedule.cancel`                        | single cancel, in-memory  | latency median          | Advisory trend; last ledger value 2.695 ms median (v0.2.8) |
| `Repeat::next_after` (cron, 1k advances) | `"0 * * * *"` cron string | latency median per step | Advisory trend against the recorded baseline               |
| `schedule.agenda(1000, limit=20)`        | 1000 seeded events        | latency median          | Advisory trend against the recorded baseline               |

The recurrence row is estimated from complete 1,000-step batch timings divided
by 1,000. `first_after` supplements that row. All rows are advisory trends;
fixture count/identity assertions are not new performance gates. The historical
2.695 ms cancel observation includes the original inline reminder creation and
cancel; it is not an isolated cancel measurement. No absolute latency threshold,
allocation guarantee or fabricated current baseline is added.

## Release Ledger

### v0.2.8 - 2026-06-08

- Commit: `d3629501c550fd2f3bb7ed350a2b60309d596465`
- Crate version: `0.2.8`
- Khive version: `0.2.8`
- Toolchain: `rustc 1.94.1 (e408947bf 2026-03-25)`, release profile (Criterion)
- Machine: Apple M-series arm64, macOS Darwin 25.5.0, 16 GB
- Feature flags: default
- Command: `cd crates && cargo bench -p khive-pack-schedule --bench schedule_bench`
- Dataset: in-memory SQLite; event corpus seeded with 10 / 100 events; sample size 50
- vs prior: first formal release ledger entry — no prior comparable baseline

| Scenario            | Low      | Median   | High     | Outliers   |
| ------------------- | -------- | -------- | -------- | ---------- |
| schedule/remind     | 2.822 ms | 3.247 ms | 3.719 ms | 4/50 (8%)  |
| schedule/schedule   | 2.810 ms | 3.075 ms | 3.363 ms | 4/50 (8%)  |
| schedule/agenda/10  | 145.8 µs | 161.6 µs | 181.8 µs | 1/50 (2%)  |
| schedule/agenda/100 | 419.1 µs | 424.9 µs | 432.5 µs | 6/50 (12%) |
| schedule/cancel     | 2.550 ms | 2.695 ms | 2.826 ms | —          |

- Notes: `schedule/remind` and `schedule/schedule` measure a single write against a runtime built
  outside the timed loop; the growing-store effect is not modeled in this baseline.

Last reviewed: v0.2.8 (2026-06-08)
