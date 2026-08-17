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
against a seeded file-backed store and asserts counter deltas before/after each
operation. The suite is ordinary `cargo test` — no timing, no statistics, no machine
dependence — and a violated invariant fails CI like any other test.

**Counter prerequisites.** `db_diagnostics` today exposes writer-side counters only
(pooled/standalone/writer-task acquisitions, begin-refusal counters, checkpoint/WAL
state). Every counter an invariant reads that does not exist yet is a named
deliverable of the ADR-165 slice that owns the invariant, landing before or with
that slice: standalone-reader opens and pooled-reader checkouts (Slice 2), ANN-route
vs fallback serve counts for the note-search consumer (Slice 3), per-backend
dispatch counts by requested kind at the coordinator (Slice 4), and candidate
hydration row count per operation (Slice 3, hydration seam). An invariant whose
counter has not landed is expected-fail, never silently green.

Initial invariant set (each lands with the ADR-165 slice that makes it true; until
that slice merges, the invariant is present but marked expected-fail so the suite
documents the known-bad state instead of hiding it):

| Invariant | Property asserted                                                                                                                                                                                                                                                                                        | Guards          |
| --------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------- |
| G1        | One `memory.recall` performs no per-target writer-task acquisition and at most ONE writer-task acquisition for the shared batch carrying its rows (ADR-133 D1/D7 semantics: the batch may also carry other dispatches' rows; attribution is to the batch, asserted after the harness quiescence barrier) | ADR-165 Slice 1 |
| G2        | One `search` / `memory.recall` performs 0 standalone-reader opens (pooled-reader checkout counter carries the traffic)                                                                                                                                                                                   | ADR-165 Slice 2 |
| G3        | On a store with installed ANN graphs, a note-substrate `search`'s vector leg is ANN-served; full-scan fallback count is 0                                                                                                                                                                                | ADR-165 Slice 3 |
| G4        | A `kind=note` search dispatches 0 operations to a registered backend whose declaration excludes notes                                                                                                                                                                                                    | ADR-165 Slice 4 |
| G5        | A note-substrate `search` hydrates no more candidate rows than the documented per-arm overfetch times the number of arms (today: `limit x 4` per arm, two arms) plus the registered ANN consumer's fresh-tail contribution, measured at the hydration seam after fusion and fresh-tail merge             | ADR-165 Slice 3 |

G5 is scoped to the note-substrate `search` union deliberately. `memory.recall`'s
hydration is governed by different, configured terms — a per-model ANN candidate
floor of `max(k x 4, k + 32)`, multi-round candidate widening up to a further
factor of four, and fresh-tail merge additions — so the search constant is not its
bound, and a green G5 says nothing about recall. Recall's hydration invariant lands
as a separate entry once Slice 3's hydration counter exists, expressed against
those configured terms rather than the search constant, so the asserted bound and
the configuration that produces it cannot drift apart silently.

Suite contract:

- **Counters, not clocks.** An invariant may count acquisitions, opens, dispatches,
  routes, rows, and bytes. It may not assert a duration.
- **The counter must be attributable, and "no concurrent traffic" is not enough.**
  Read verbs launch detached background work (`memory.recall`'s serve-ledger and
  event task via `track_background_task`; ANN maintenance; batch flushes), so a
  before/after snapshot around the verb call alone can race that work in either
  direction. Each assertion therefore runs on a single in-process runtime with no
  concurrent traffic AND takes its after-snapshot only past a **quiescence
  barrier**: the harness drains host-tracked background tasks (the same
  tracked-task seam the daemon's shutdown drain uses) before reading counters. An
  invariant over work the barrier cannot drain is redesigned onto a counter the
  background path does not touch, or split into a separate deterministic assertion
  — the guard must not intermittently pass or fail from its own harness
  concurrency.
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

### D3 — The budget and thresholds are versioned with the code

A checked-in trend-configuration file — `bench/trend-config.toml`, read by the D2
lane — defines every normative value D2 references; none may live only in prose or
in an implementer's judgment:

- per-metric latency budget (initially: unified verb 10-15 ms; `search` and
  `memory.recall` server-side p50 at the seeded reference scale);
- `noise_ceiling` (initial value `1.15`): the maximum A/A spread ratio under which
  a run may report a measurement; above it the run reports UNMEASURED;
- `breach_multiplier` (initial value `1.5`): the factor over baseline that,
  sustained across 3 consecutive measured runs, files the D2 issue;
- `baseline_window` (initial value `7`): the baseline is the median of the last
  `baseline_window` accepted measured runs on the same runner class, recorded in
  the tracked history; it updates automatically as measured runs accept, while the
  BUDGET changes only by reviewed diff carrying the measurement that justifies it;
- bootstrap and runner-class matching: each history entry records its runner class
  (os, arch, runner label), and the baseline is computed only over entries of the
  same class. With fewer than `baseline_window` but at least 3 accepted same-class
  runs, the median of what exists serves as an interim baseline; below 3 the lane
  reports BASELINE-PENDING and files no breach. A runner-class change starts a
  fresh window rather than comparing across classes.

The initial values above seed the file; after that, the file is the single source
and this ADR's numbers are historical.

A budget that only exists in documentation regresses by being forgotten, which is
how the 10-15 ms budget and a 5-second reality coexisted without any instrument
noticing.

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
