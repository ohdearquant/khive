# Historical warm ANN p95 baseline results

Baseline `memory.recall` latency on the warm ANN path, measured against file-backed WAL
SQLite databases at one and three queried models. These measurements were originally
recorded for the retired ADR-116 proposal and remain as historical data. The maintained
performance-gate contract is the self-contained threshold and methodology in
`p95_gate.rs`; ADR-107 governs the ANN lifecycle behavior, not this numeric threshold.

## Method

- Harness: `crates/khive-pack-memory/benches/p95_gate.rs`.
- Execution: `cd crates && cargo bench -p khive-pack-memory --bench p95_gate` (a
  `harness = false` bench target — `cargo bench` compiles and runs it directly).
- Isolation: run on a quiet machine with no concurrent builds, benchmarks, or heavy I/O in
  progress. Run the suite twice and require consistent numbers across both runs before
  recording or refreshing baselines.
- Database: file-backed SQLite in a fresh tempdir per configuration, WAL mode, not
  in-memory.
- The maintained harness compares one and three queried models — the number of embedding
  models a single `memory.recall` call _queries_ (M), not the number registered on the
  runtime. It measures those two gate configurations, each against its own
  runtime/database, plus one informational beyond-gate row:
  - **one-model** (M=1 queried): one model registered (`BgeSmallEnV15`),
    `memory.recall` called with that model explicit.
  - **three-model fan-out** (M=3 queried): three models registered
    (`BgeSmallEnV15` + `MultilingualE5Small` + `AllMiniLmL6V2`), `memory.recall` called
    with no `embedding_model` so it fans out to all three.
  - **four-model fan-out** (M=4 queried, beyond the maintained gate — informational): four
    models registered (adds `ParaphraseMultilingualMiniLmL12V2`), same fan-out call.
    This row is not a primary-only baseline or a gate reference.
- Corpus: 200 memories per registered model, per configuration — 200 total for one-model,
  600 for three-model fan-out, 800 for four-model fan-out.
- Per configuration: seed, poll `memory.recall` until 5 consecutive responses are clean
  (see "Warm-route assertion" below), sleep past the internal durable-epoch debounce
  interval plus one settle call, then 200 timed `memory.recall` calls. Every timed sample
  is asserted clean (non-degraded, non-empty); the ANN-warm event count is snapshotted
  immediately before and after the 200-call window and asserted unchanged. The harness
  panics immediately if either check fails. The settle call can enqueue but does not
  await a detached rebuild; if that work overlaps timing and its `memory.ann_warm`
  events persist, the event-count check rejects the run as potentially contaminated
  and it must be repeated after quiescence. Event emission is best-effort, per arm: a
  missing event store returns without logging (the harness's own count oracle requires
  the store, so that arm aborts the run rather than passing it), a serialization
  failure logs a warning and returns, and an append failure logs a warning while the
  rebuild completes without a persisted event. An overlap whose events fail to persist
  is therefore an evidence gap this check cannot see, limited to failures that leave
  the count oracle usable.
- Machine: Apple M2 Max, macOS 27.0, arm64.
- Commit: 7b55beea3 (branch `p95-bench-harness`, the harness code measured below).

## Warm-route assertion

`memory.recall`'s response marks every result with `"degraded": "ann_unavailable"` when
at least one queried model's vector leg missed its bounded ANN-readiness wait and was
served FTS-only instead. The harness asserts every timed sample carries no such marker
and is non-empty — this positively rules out the bounded-wait degradation fallback.

That marker does not cover the internal sqlite-vec exact-fallback path (taken only after
an ANN search error), which returns valid, non-degraded results. To positively assert
against that path too, the harness uses a second, independent signal: `memory.ann_warm`
phase-started/completed events, recorded through `KhiveRuntime::events` (a `pub` accessor
returning `khive_storage::EventStore`; `count_events` is a `pub` trait method). Every ANN
rebuild attempts one of these events (emission is best-effort; see the caveat in the
methodology note above), and the sqlite-vec fallback clears the
model's cached graph as a side effect — the _next_ recall for that model would trigger
exactly such a rebuild. The harness snapshots this event count immediately before and
after the 200-call timed window and asserts it is unchanged, which is strong (though not
airtight) evidence that none of the timed samples took the exact-fallback path.

Residual gap: an exact-fallback on the very last timed sample, with no subsequent call in
the window to reveal the resulting rebuild, would not be caught by this check. Closing
that gap requires the crate's per-model route counter (`ann::AnnState::warm_route_count`),
currently `pub(crate)`, to become visible outside the crate — tracked as issue #1084
(verb-surface route observability). This run recorded zero warm-route-assertion failures
and zero ANN-warm-event-count assertion failures across all three configurations.

## Note on isolation

The measuring machine ran concurrent `rustc` compiles from unrelated work for most of
this session; `ps aux` confirmed this repeatedly during the run. Repeated attempts at
this baseline showed the three-model row reproducing tightly (p95 6.809ms and 6.802ms
across two attempts) while the one-model and, especially, the four-model rows varied
by up to roughly 2-3x across attempts (e.g. one-model p95 ranged ~4.3-10.9ms; four-model
p95 ranged ~8.5-32.4ms across four attempts) whenever contention was present at
measurement time. The numbers recorded below are from the attempt with the smoothest
internal percentile progression (p50 < p95 < p99 without a disproportionate jump) across
all three rows and zero assertion failures, and should be treated as directional rather
than a fully isolated result. A future refresh on a genuinely idle machine should
supersede this baseline.

## Results

| configuration                     | p50 ms | p95 ms | p99 ms |   n | note                             |
| --------------------------------- | -----: | -----: | -----: | --: | -------------------------------- |
| one-model (M=1 queried)           |  3.964 |  4.343 |  4.903 | 200 | maintained gate reference        |
| three-model fan-out (M=3 queried) |  5.961 |  6.809 |  6.940 | 200 | maintained gate reference        |
| four-model fan-out (M=4 queried)  |  6.379 | 21.801 | 33.440 | 200 | beyond gate — informational only |

## Gate reference

The maintained harness contract allows a warm-route change to add at most 1.0ms absolute
p95 **and** at most 5% of the matching M=1 or M=3 baseline's warm `memory.recall` p95
above. Both conditions must hold, so the permitted regression is whichever cap is
_smaller_ at each baseline.

- Against the M=1 baseline (4.343ms p95): 5% is ~0.217ms, well under the 1.0ms absolute
  cap, so the 5% bound is the binding constraint.
- Against the M=3 baseline (6.809ms p95): 5% is ~0.340ms, again well under the 1.0ms
  absolute cap, so the 5% bound is again the binding constraint.
- The 1.0ms absolute cap only binds on its own if a baseline's warm p95 exceeds 20ms
  (5% x 20ms = 1.0ms) — neither measured baseline is near that.

The M=4 row (21.801ms p95) is beyond the maintained gate configurations and is not a
constraint reference; it is kept for context only, and — per the isolation note above —
its absolute value here should be read as noisy rather than a tight bound.
