# khive-pack-schedule Benchmark Ledger

## Run command

```bash
# from workspace root
cargo bench -p khive-pack-schedule --bench schedule_bench

# compile + smoke-check only (no timing)
cargo bench -p khive-pack-schedule --bench schedule_bench -- --test
```

HTML reports land in `target/criterion/schedule/`.

---

## Scenarios

| Benchmark | Description | Setup |
|-----------|-------------|-------|
| `schedule/remind` | Write latency for a single `schedule.remind` call. | Fresh in-memory runtime per measurement. |
| `schedule/schedule` | Write latency for a single `schedule.schedule` call (includes DSL parse validation). | Fresh in-memory runtime per measurement. |
| `schedule/agenda/10` | `schedule.agenda(limit=20)` over a corpus of 10 seeded events. | 10 events seeded once before the group. |
| `schedule/agenda/100` | `schedule.agenda(limit=20)` over a corpus of 100 seeded events. | 100 events seeded once before the group. |
| `schedule/cancel` | `schedule.remind` + `schedule.cancel` — inline create+cancel per iteration. | No pre-seeding; each iteration creates then cancels. |

---

## Baseline (2026-06-06, post-sweep)

**Toolchain:** rustc 1.94.1 (e408947bf 2026-03-25)
**Machine:** arm64 (Apple Silicon), macOS Darwin 25.5.0

| Scenario | Low | Median | High | Outliers |
| --- | --- | --- | --- | --- |
| schedule/remind | 2.822 ms | 3.247 ms | 3.719 ms | 4/50 (8%) |
| schedule/schedule | 2.810 ms | 3.075 ms | 3.363 ms | 4/50 (8%) |
| schedule/agenda/10 | 145.8 µs | 161.6 µs | 181.8 µs | 1/50 (2%) |
| schedule/agenda/100 | 419.1 µs | 424.9 µs | 432.5 µs | 6/50 (12%) |
| schedule/cancel | 2.550 ms | 2.695 ms | 2.826 ms | — |
