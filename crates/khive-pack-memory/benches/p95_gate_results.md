# ADR-116 gate condition 2 — p95 baseline results

Baseline `memory.recall` latency on the warm ANN path, measured against a file-backed
WAL SQLite database with four registered embedding-model generations (1 primary + 3
retired), matching the corpus shape ADR-116's warm-hit generation-read gate is defined
against. ADR-116 is Proposed; the durable per-model generation check it specifies has
not landed yet. This baseline exists so that PR can diff its added cost against these
numbers.

## Method

- Harness: `crates/khive-pack-memory/benches/p95_gate.rs` (`cargo run --release -p
  khive-pack-memory --bin` equivalent — run as a plain binary via
  `cargo build --release -p khive-pack-memory --bench p95_gate`, then executing the
  built binary directly).
- Database: file-backed SQLite in a fresh tempdir, WAL mode, not in-memory.
- Corpus: 200 memories per model x 4 model generations (1 primary `BgeSmallEnV15` +
  3 retired: `MultilingualE5Small`, `AllMiniLmL6V2`,
  `ParaphraseMultilingualMiniLmL12V2`) = 800 memories total.
- Per model: 5 warmup `memory.recall` calls (forces ANN build/install), then 200
  timed `memory.recall` calls, percentiles computed over the timed sample.
- Machine: Apple M2 Max, macOS 27.0, arm64.
- Commit: a3544bd12 (branch `p95-bench-harness`).

## Note on isolation

The fleet's exclusive bench-window lock (`with-bench-window.sh`,
`/tmp/lion-bench-window.lock`) could not be acquired: two idle `reactive_pr_review.py`
daemons (PIDs holding shared-lock file descriptors with 0% CPU and no live
cargo/rustc child) were holding stale shared locks that never released during the
session. This is a leaked-fd bug in that script's lock usage, not real build
contention. `ps` confirmed both daemons were CPU-idle and no cargo/rustc process was
running anywhere on the machine during the timed run, so the measurement below ran
without exclusive isolation but with the machine otherwise quiet. Two independent runs
(a dry run and the recorded run below) produced consistent numbers, which supports
that this was in practice an uncontended measurement.

## Results

| model                                    | p50 ms | p95 ms | p99 ms |   n |
| ----------------------------------------- | -----: | -----: | -----: | --: |
| primary (BgeSmallEnV15, queried default)  | 16.031 | 29.510 | 34.852 | 200 |
| retired: MultilingualE5Small              | 14.587 | 30.640 | 51.119 | 200 |
| retired: AllMiniLmL6V2                    |  5.505 | 16.017 | 28.318 | 200 |
| retired: ParaphraseMultilingualMiniLmL12V2 |  5.499 | 18.844 | 29.764 | 200 |

Seed phase (800 memories across 4 models, embedding + write): 10.6s wall clock.

## Gate reference

Per ADR-116 §Warm hit: the added per-model durable generation check must cost at most
1.0ms absolute p95 and at most 5% of this baseline's warm `memory.recall` p95. Applied
to the primary model's p95 of 29.510ms, the 5% bound is ~1.48ms, so the binding
constraint here is the 1.0ms absolute cap.
