# khive-pack-gtd Benchmark Ledger

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

| Benchmark                    | Description                                                                              | Setup                                                    |
| ---------------------------- | ---------------------------------------------------------------------------------------- | -------------------------------------------------------- |
| `gtd/assign`                 | Write latency for a single `gtd.assign` call.                                            | Fresh in-memory runtime per measurement.                 |
| `gtd/next/10`                | `gtd.next(limit=10)` over a corpus of 10 seeded tasks (mixed statuses/priorities).       | 10 tasks seeded once before the group.                   |
| `gtd/next/100`               | `gtd.next(limit=10)` over a corpus of 100 seeded tasks.                                  | 100 tasks seeded once before the group.                  |
| `gtd/tasks/filter_by_status` | `gtd.tasks(status="next", limit=50)` over 100 seeded tasks.                              | 100 tasks seeded once.                                   |
| `gtd/transition`             | `gtd.assign` + `gtd.transition(status="next")` — inline create+transition per iteration. | No pre-seeding; each iteration creates then transitions. |

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
