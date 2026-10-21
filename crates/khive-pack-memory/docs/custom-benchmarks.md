# Reducible memory benchmarks

These targets use generated data and require no ignored corpus or production database.
Every default benchmark ID contains `synthetic-v1`. The synthetic embedder hashes
whitespace-separated tokens with fixed seed `0x57136d656d6f7279` and FNV-1a updates,
then normalizes the resulting nonnegative vector. It measures retrieval, storage,
and dispatch work with inexpensive synthetic embeddings. It does not measure
real-model inference cost or semantic embedding quality.

`KHIVE_BENCH_REAL_MODEL=1` explicitly selects real embedding inference for
`e2e_recall` and `p95_gate`; those IDs contain `real-model` instead. Model files and
inference costs belong to that opt-in cohort. Never compare its timings with
synthetic IDs or compare synthetic timings with the historical real-model ledger.
`fts_gather` has no embedding leg and always uses its synthetic text corpus.

| Target       | Default workload                                                                           | Measurement                                                                     |
| ------------ | ------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------- |
| `e2e_recall` | 256 generated memories, 32 rotating queries, ten FTS/fusion configurations                 | Criterion dispatch latency; cloning parameters and checking results are untimed |
| `fts_gather` | 2,048 generated FTS documents, 32 queries, ten gather strategies                           | Criterion gather latency plus untimed full-term baseline recall@10              |
| `p95_gate`   | 200 memories per registered model, separate M=1/M=3/M=4 stores, 200 measured calls per row | Nearest-rank p95; M=1/M=3 gate and informational M=4                            |

Default Criterion settings are ten samples, 100 ms warmup, and 300 ms measurement
per case. The default workload is intended to finish within five minutes per
target; fixture setup and ANN readiness remain real work, so measure total elapsed
time on the reference host. Larger Criterion measurement budgets are explicit
Criterion arguments. Real-model inference is never part of the default cohort.

The text fixture mixes universal function words, a half-corpus token, eight topics,
and 32 rare-token bands. IDs and timestamps in the FTS index are fixed. Memory
notes use fixed generated contents and fresh runtime-assigned IDs/timestamps.
Synthetic stores and volume locks are private temporary files. Their setup and
joined writer teardown are outside the timers. The explicit real-model p95 mode
retains its original temporary database, `RuntimeConfig::default()` constructor,
registry setup, multi-thread Tokio executor, and lifecycle; its volume-lock policy
comes from that existing runtime configuration. No benchmark reads the user's data.

Synthetic `e2e_recall` and `p95_gate` processes default the chained ANN rebuild
coalescing delay to 5 ms before starting their executor. The finite generated write
burst does not need the production 30-second coalescing window. An explicit
`KHIVE_ANN_REBUILD_DEBOUNCE_MS` remains unchanged; set it to `30000` to measure the
production delay. Real-model mode keeps the production default. This reduces
coalescing waits paid during setup and teardown; background scheduling may also
affect timings. The existing readiness checks, p95 settle interval, event-count
and per-sample guards, and joined writer teardown remain. Compare runs with the
same cohort and debounce setting; this change supplies no new reference measurements.

## Listing and running

Run from the repository root:

```sh
cargo bench --manifest-path crates/Cargo.toml -p khive-pack-memory --bench e2e_recall -- --list
cargo bench --manifest-path crates/Cargo.toml -p khive-pack-memory --bench fts_gather -- --list
cargo bench --manifest-path crates/Cargo.toml -p khive-pack-memory --bench p95_gate -- --list
cargo bench --manifest-path crates/Cargo.toml -p khive-pack-memory --bench e2e_recall
cargo bench --manifest-path crates/Cargo.toml -p khive-pack-memory --bench fts_gather
cargo bench --manifest-path crates/Cargo.toml -p khive-pack-memory --bench p95_gate
KHIVE_BENCH_REAL_MODEL=1 cargo bench --manifest-path crates/Cargo.toml -p khive-pack-memory --bench p95_gate
```

Each target handles `--list` before creating Tokio, a database, or an embedding
provider, including real-model listings. Criterion targets retain Criterion's
selection and baseline arguments; fixtures are initialized only for selected
cases. The custom p95 target accepts and ignores unfamiliar arguments, including
`--save-baseline NAME`, `--noplot`, and `--bench`. It always measures all three
configured rows. Unset `KHIVE_RECALL_FTS_*` overrides for `e2e_recall` and synthetic p95 runs;
they would override the declared request configuration and are refused. Explicit
real-model p95 keeps the original environment/configuration behavior unchanged.

## Output and interpretation

Criterion owns the latency estimates for `e2e_recall` and `fts_gather`.
Each selected FTS case additionally emits one dimensionless quality row:

```text
BENCH-JSON {"id":"fts_gather/synthetic-v1/baseline","statistic":"recall_at_10","value":1.0,"n":32}
```

The reference is the actual full-term ranked top ten for each query, captured
before applying each candidate strategy. Each reference must contain ten unique
IDs. Missing half of those IDs scores 0.5; unrelated IDs score zero. A strategy's
mean overlap is measured, not assumed to clear a production quality threshold.
These results cannot establish a production tuning default.

The `df-*` benchmark selectors exclude zero-frequency terms, including words
removed by FTS query sanitization such as `and`. If all matching terms exceed a
selector's frequency cutoff, it keeps the rarest matching term. This preserves
the nonempty, bounded-result check for the generated cohort. Frequencies come
from the actual trigram index: `rare1` also matches `rare10` through `rare19`, so
its frequency exceeds that of a single rare-token band. Untimed controls cover
both the cutoff and fallback when a zero-frequency term is present.

A successfully measured p95 row emits one `BENCH-JSON` line with the same ID as
its listing, `statistic: "p95_ns"`, a finite value, and `n: 200`. Values come from
actual individual calls, recorded at microsecond resolution and converted to
nanoseconds for the output unit. The percentile is nearest rank `ceil(0.95*n)`.
Human tables remain alongside JSON. Contaminated rows emit no fabricated timing;
a missing row makes a reference set incomplete even when the informational M=4
refusal does not fail the original gate's process exit status.

The original p95 real-model configurations, warm-route checks, settle interval,
background-task completion wait, event-count window, per-sample degradation
refusal, and gating versus informational behavior remain in place. Synthetic
runs use the same guards. The existing event-plane limitations still apply:
best-effort event loss and a last-sample exact fallback can escape that evidence.
The M=1/M=3 comparison remains at most 1 ms absolute and 5% of the matching
cohort/model-count baseline. The harness reports measurements; it does not invent
a historical baseline or compute a cross-cohort comparison.

## Contract controls

Pass an already-built executable to the Python checker:

```sh
python3 crates/khive-pack-memory/benches/reducible_support/check_p95_contract.py /absolute/path/to/p95_gate
python3 crates/khive-pack-memory/benches/reducible_support/check_p95_contract.py /absolute/path/to/p95_gate --measure
python3 -m unittest discover -s crates/khive-pack-memory/benches/reducible_support -p 'test_*.py'
cargo bench --manifest-path crates/Cargo.toml -p khive-pack-memory --bench fts_gather -- --regression-case=quality
```

The checker tests both cohort listings with five-second watchdogs. A list-only
sentinel immediately after the list branch makes accidental workload entry fail
without loading models. `--regression-case=report-contract` feeds known values
through the same JSON emitter with the distinct statistic `report_contract_probe`,
checks the exact ID set, and rejects missing or duplicate rows, nonfinite values,
wrong statistics, and incomplete sample counts. Those known values are protocol
probes, never measured baselines. The checker accepts that statistic only for the
explicit report-contract probe. `--measure` additionally runs the caller-selected
cohort with a 300-second watchdog and requires `p95_ns`; probe rows and mixed
probe/timing output fail measurement validation, even when the values match.

All previous p95 `--regression-case` names remain available, including held build,
held rebuild, chained handoff, task-completion, timeout, and gate-role controls.
The FTS quality case exercises the real generated index plus nonvacuous subset
and unrelated-ID controls without Criterion timing. Two measured reference runs
at one commit, reduced by matching benchmark ID with A/A spread at most 1.15,
remain necessary before recording a benchmark reference.
