# ADR-166: Hot-Path Latency Regression Guard

**Status**: proposed\
**Date**: 2026-08-17\
**Authors**: khive maintainers\
**Depends on**:

- [ADR-103](ADR-103-resource-attribution-model.md) — per-dispatch resource accounting
- [ADR-133](ADR-133-incidental-writes-off-the-request-hot-path.md) — writer-acquisition
  reduction and acquisition-site instrumentation (D8)
- [ADR-165](ADR-165-read-path-performance-recovery.md) — the recovery program whose
  mechanisms this guard pins

---

## Context

The read-path regression that ADR-165 recovers was not a single event. This area has
been fixed repeatedly and has regressed repeatedly, because nothing in CI observes the
properties the fixes restore. A 300-800x latency regression accumulated across months
of merges, each individually green: the test suite asserts correctness, and correctness
was never violated — a search that takes 5 seconds returns the same rows as one that
takes 5 milliseconds.

Wall-clock benchmarks in CI are the obvious answer and the wrong first one. Shared CI
runners have noisy neighbours, variable page cache state, and no admission control;
a timing gate tight enough to catch a 2x regression false-fails weekly, gets a retry
button, and is dead within a month. A gate loose enough to never false-fail admits the
regression this repository actually experienced — three orders of magnitude, arriving
a factor at a time.

The investigation behind ADR-165 exposes the alternative. Every mechanism of the
regression is visible in **deterministic counters**, independent of machine speed:

- recall's latency inheritance is _writer-task acquisitions per read dispatch_ (measured
  live: +1-4 per recall against an idle-path expectation of 0);
- connection churn is _standalone reader opens per read operation_ (measured: one per
  file-backed store read against a pooled expectation of 0);
- the vector-scan cost is _which route served the query_ (ANN vs full scan);
- fan-out waste is _dispatch count per backend per kind_ (session backend receiving
  note searches).

A counter is exact on the slowest runner. The defect classes that produced the
regression are all countable. Timing remains useful as a trend instrument; it is not
fit to be the gate.

## Decision

Two tiers with different gating authority: deterministic mechanism invariants gate
every PR; timed trend measurement informs but never gates.

### D1 — Mechanism invariants: per-PR, deterministic, gating

A dedicated integration-test suite (`hot_path_guard`) runs the canonical read verbs
against a seeded file-backed store and asserts counter deltas from `db_diagnostics`
before/after each operation. The suite is ordinary `cargo test` — no timing, no
statistics, no machine dependence — and a violated invariant fails CI like any other
test.

Initial invariant set (each lands with the ADR-165 slice that makes it true; until
that slice merges, the invariant is present but marked expected-fail so the suite
documents the known-bad state instead of hiding it):

| Invariant | Property asserted                                                                                                                                                                                                                                                     | Guards          |
| --------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------- |
| G1        | One `memory.recall` performs 0 synchronous writer-task acquisitions on its request path (post ADR-133 D1/D7: the dispatch may wait on a shared batch commit, but acquisition-counter delta attributable to the recall itself is bounded by the batch, not per-target) | ADR-165 Slice 1 |
| G2        | One `search` / `memory.recall` performs 0 standalone reader opens (pooled counter carries the traffic)                                                                                                                                                                | ADR-165 Slice 2 |
| G3        | On a store with installed ANN graphs, `search`'s vector leg is ANN-served; full-scan fallback count is 0                                                                                                                                                              | ADR-165 Slice 3 |
| G4        | A `kind=note` search dispatches 0 operations to a registered backend that declares it does not serve notes                                                                                                                                                            | ADR-165 Slice 4 |
| G5        | A read verb on the seeded corpus touches a bounded row population: candidate hydration row count ≤ the documented overfetch bound for the requested limit                                                                                                             | overfetch creep |

Suite contract:

- **Counters, not clocks.** An invariant may count acquisitions, opens, dispatches,
  routes, rows, and bytes. It may not assert a duration.
- **The counter must be attributable.** Each assertion isolates its operation (single
  in-process runtime, no concurrent traffic) so a delta belongs to the asserted
  operation alone. Where a background task contributes (ANN maintenance, batch
  flushes), the invariant either drains it first or asserts on a counter the
  background path does not touch — the guard must not intermittently fail from its
  own harness concurrency.
- **New hot-path mechanisms add an invariant in the same PR.** A change that
  introduces a new class of work on `search`/`recall`/`get` (a new per-dispatch write,
  a new fan-out target, a new retrieval pass) must extend the suite with the counter
  that makes the new work visible, or state in the PR why the existing set already
  bounds it. Reviewers hold this line; the suite's own docs list the classes.
- **Expected-fail is temporary and dated.** An invariant in expected-fail state names
  the slice that will flip it. Expected-fail entries older than one release cycle are
  a release-blocking finding.

### D2 — Timed trend lane: informative, never gating

A benchmark suite (criterion) over the same seeded corpus runs on a schedule (not per
PR): `search` and `memory.recall` p50/p95 at fixed corpus sizes, plus an A/A
calibration pair (the same benchmark run twice) whose spread measures the runner's
noise floor for that execution.

- Results append to a tracked history (per commit: metric, value, A/A spread).
- A run whose A/A spread exceeds the declared noise ceiling reports UNMEASURED for
  that execution — visibly, not as a pass. UNMEASURED twice consecutively is a
  finding about the bench environment, filed, not ignored.
- A sustained trend breach (p50 above the recorded baseline by the declared multiplier
  across 3 consecutive measured runs) files an issue automatically with the counter
  snapshot attached. It does not block merges: by construction, any real mechanism
  regression should have tripped a D1 invariant first — a trend breach with all
  invariants green means a mechanism class is missing from D1, and the issue's job is
  to name it and add the invariant.

### D3 — The budget is versioned with the code

The per-verb latency budgets (currently: unified verb 10-15 ms; `search` and
`memory.recall` server-side p50 at the seeded reference scale) live in a checked-in
budget file read by the trend lane, not in prose. Changing a budget is a reviewed
diff with the measurement that justifies it — a budget that only exists in
documentation regresses by being forgotten, which is how the 10-15 ms budget and a
5-second reality coexisted without any instrument noticing.

## Consequences

- The gate that protects the hot path is exact, fast, and runs on every PR at zero
  flake cost; the expensive noisy instrument is demoted to trend duty where its noise
  cannot burn trust.
- The guard is only as complete as its invariant list. D1's same-PR extension rule
  and D2's invariants-green-but-trend-breached escalation are the two mechanisms that
  grow the list; both produce named findings rather than silence.
- Expected-fail entries make the current known-bad state executable documentation:
  the suite tells the truth about today while gating tomorrow.
- Marginal CI cost is one integration-test binary per PR and a scheduled bench job.

## Verification

- The suite exists, runs in `make ci`, and G1-G5 are present (expected-fail where the
  corresponding slice has not merged).
- Reverting any merged ADR-165 slice's core change locally flips its invariant to
  red (mutation check recorded in the introducing PR).
- The trend lane produces history entries with A/A spread on its first scheduled run,
  and an artificially throttled run reports UNMEASURED rather than passing.
