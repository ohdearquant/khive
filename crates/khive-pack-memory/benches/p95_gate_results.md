# ADR-116 gate condition 2 — p95 baseline results

Baseline `memory.recall` latency on the warm ANN path, measured against file-backed WAL
SQLite databases matching the corpus shapes ADR-116's warm-hit generation-read gate is
defined against. ADR-116 (PR #1080, currently in review, not yet merged) is Proposed; the
durable per-model generation check it specifies has not landed yet. This baseline exists
so that PR can diff its added cost against these numbers.

## Method

- Harness: `crates/khive-pack-memory/benches/p95_gate.rs` — build with `cargo build
  --release -p khive-pack-memory --bench p95_gate`, then execute the built binary
  directly (it is `harness = false`, so `cargo bench`/`cargo run` do not apply to it).
- Database: file-backed SQLite in a fresh tempdir per configuration, WAL mode, not
  in-memory.
- ADR-116's warm-hit gate is defined "at one and three models" — the number of embedding
  models a single `memory.recall` call *queries* (M), not the number of models registered
  on the runtime. The harness measures exactly those two gate configurations, each
  against its own runtime/database, plus one informational beyond-gate row:
  - **one-model** (M=1 queried, ADR-116 gate): one model registered (`BgeSmallEnV15`),
    `memory.recall` called with that model explicit.
  - **three-model fan-out** (M=3 queried, ADR-116 gate): three models registered
    (`BgeSmallEnV15` + `MultilingualE5Small` + `AllMiniLmL6V2`), `memory.recall` called
    with no `embedding_model` so it fans out to all three.
  - **four-model fan-out** (M=4 queried, beyond the gate — informational only): four
    models registered (adds `ParaphraseMultilingualMiniLmL12V2`), same fan-out call.
    ADR-116 does not gate M=4; this row is not a primary-only baseline.
- Corpus: 200 memories per registered model, per configuration — 200 total for one-model,
  600 for three-model fan-out, 800 for four-model fan-out.
- Per configuration: seed, then poll `memory.recall` until 5 consecutive responses are
  clean (see "Warm-route assertion" below), then 200 timed `memory.recall` calls,
  percentiles computed over the timed sample. Every timed sample is asserted clean before
  being recorded; the harness panics immediately if any sample is not.
- Machine: Apple M2 Max, macOS 27.0, arm64.
- Commit: 9e49a3126 (branch `p95-bench-harness`, the harness code measured below).

## Warm-route assertion

`memory.recall`'s response marks every result with `"degraded": "ann_unavailable"` when
at least one queried model's vector leg missed its bounded ANN-readiness wait and was
served FTS-only instead. That marker is the only route-quality signal the verb surface
exposes outside the crate — the harness lives in `benches/`, a separate crate that only
sees `khive-pack-memory`'s `pub` items, so it cannot read the crate-private `ann` module's
per-model freshness state or its internal ANN-vs-sqlite-vec route variable directly.

Given that, the harness positively rules out the FTS-degradation fallback for every
recorded sample (wait-for-warm before timing, assert-clean on every timed call, panic
otherwise). It does **not** positively distinguish a genuine Vamana ANN hit from the
internal sqlite-vec exact-fallback path, since that path only triggers on an ANN search
error and carries no response-visible marker; it is ruled out by construction instead —
the corpus is sized to force a Vamana build and the warm-wait requires a stable clean run
before any sample is timed. This run recorded zero degradation panics across all three
configurations.

## Note on isolation

The fleet's exclusive bench-window lock (`with-bench-window.sh`,
`/tmp/lion-bench-window.lock`) could not be acquired within its bounded wait — a shared
holder never released it during the session. Unlike the prior baseline round, this was
not a leaked-fd artifact: `ps aux` confirmed genuine concurrent `rustc` compiles
(`tokio`, `serde_json`, `futures_util`, `zerocopy`, `serde`) running from another lane on
the shared machine at the time. Two runs taken during that contention (p95 4.7ms/10.7ms
one/three-model in the first, 16.9ms/36.4ms in the second) were **not** consistent with
each other, so they are discarded rather than reported.

The harness was then re-run after polling `ps aux` until zero `rustc` processes remained.
Two consecutive runs under that state were consistent with each other (one-model p95
4.82ms vs 4.35ms; three-model p95 7.60ms vs 9.03ms; four-model p95 8.67ms vs 8.63ms) and
are the results recorded below (second of the two runs). Load average stayed elevated
(15-19 on this machine) throughout from unrelated fleet processes even during the clean
window, so this is a best-effort quiet measurement, not a fully isolated one — the
absence of a live `rustc`/`cargo` compile is what was verified, not absence of all
background load.

## Results

| configuration                          | p50 ms | p95 ms | p99 ms |   n | note                            |
| --------------------------------------- | -----: | -----: | -----: | --: | -------------------------------- |
| one-model (M=1 queried)                 |  3.810 |  4.351 |  4.642 | 200 | ADR-116 gate case                |
| three-model fan-out (M=3 queried)       |  6.508 |  9.032 | 12.594 | 200 | ADR-116 gate case                |
| four-model fan-out (M=4 queried)        |  6.958 |  8.631 | 12.723 | 200 | beyond gate — informational only |

## Gate reference

Per ADR-116 §Warm hit (PR #1080): the added per-model durable generation check must cost
at most 1.0ms absolute p95 and at most 5% of the matching M=1 or M=3 baseline's warm
`memory.recall` p95 above.

- Against the M=1 baseline (4.351ms p95): the 5% bound is ~0.22ms, so the binding
  constraint is the 1.0ms absolute cap.
- Against the M=3 baseline (9.032ms p95): the 5% bound is ~0.45ms, so the binding
  constraint is again the 1.0ms absolute cap.

The M=4 row (8.631ms p95) is beyond ADR-116's stated gate configurations and is not a
constraint reference; it is kept for context only.
