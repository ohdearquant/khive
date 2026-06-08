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
