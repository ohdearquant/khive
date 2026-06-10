# khive-pack-comm Benchmark Ledger

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
