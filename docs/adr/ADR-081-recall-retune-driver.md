# ADR-081: Recall Retune Driver — Governed Ingestion of Implicit Feedback

**Status**: Proposed\
**Date**: 2026-07-02\
**Authors**: lambda:khive, lambda:leo (scorer design and evidence)\
**Measurement evidence**: hook scorer v0 dry run over 19 serve ledgers, 7 sessions (2026-07-02)\
**Depends on**: [ADR-021](ADR-021-memory-pack.md) (Memory Pack), [ADR-033](ADR-033-recall-pipeline.md) (Recall Pipeline), [ADR-032](ADR-032-brain-profile-orchestration.md) (Brain Profile Orchestration), [ADR-035](ADR-035-cli-config-and-auto-embed.md) (Feedback Profile Resolution Order), [ADR-055](ADR-055-epistemic-edge-relations.md) (Epistemic Relations)\
**Amends**: the brain feedback weight table (`FeedbackEventKind::update_weight()`, khive-brain-core, issue #268) and the `brain.feedback` / `brain.auto_feedback` parameter surface (additive optional scorer-provenance fields, section 6)\
**GitHub**: #517 (auto_feedback), #394 (recall latency), #391/#393 (resolver legs)

---

## Context

### What ships today

The brain pack maintains per-profile Beta posteriors updated by feedback events. Two
verbs feed them: `brain.feedback` (explicit, caller-judged signals) and
`brain.auto_feedback` (#517), a convenience verb agents call after `memory.recall` to
emit implicit signals without constructing a `brain.feedback` call.

Event weighting is brain-owned. `FeedbackEventKind::update_weight()` in
`khive-brain-core` (`src/signal.rs`) is the single authority: `correction` folds at 2.0,
`explicit_positive`/`explicit_negative` at 1.5, `implicit_positive`/`implicit_negative`
at 0.5.

Profile attribution is resolved at feedback time, not serve time, by two cooperating
records: ADR-032 governs `brain.resolve`, the binding table, and longest-match
resolution; ADR-035 ("Feedback and recall-boost profile resolution order") governs the
memory pack's three-tier discipline — explicit profile from config, then a
namespace-bound `brain.resolve(consumer_kind="recall")` result **only on a real binding
match** (`matched_binding = true`), then the pack-local global tuning prior. The
`matched_binding` distinction is load-bearing: a system-default resolve result must fall
through to tier 3, and only ADR-035 specifies that fallback. `memory.recall` itself does
not consult the brain pack; its response carries no profile identifier.

An out-of-band scorer now exists: it grades each served memory in a completed session as
`used`, `ignored`, or `contradicted`, from serve markers
(`[prefetch-served ids=<8hex,csv> q="<query>"]`) plus transcript evidence. Its v0 dry run
was read-only and validated against manually spot-checked sessions.

### The measured problem

The feedback store is positive-saturated: explicit negative signals are rare and
implicit negatives are never emitted. The v0 dry run made the cost concrete. Across 7
sessions and 14 serves, every serve returned the same two memories on the same static
query. Both were topically unrelated to the active work (fused scores around 0.65, the
junk-relevance tier), and every serve was graded `ignored`. Nothing in the current
pipeline trains those re-serves away:

1. **No governed implicit pathway.** The 0.5 implicit weight predates any real implicit
   emitter; with batch scoring, a single 200-event pass at 0.5 would carry the weight of
   66 explicit judgments. No cap semantics or per-target bound exists.
2. **No cross-session re-serve suppression.** Per-session query dedup exists
   (`recent_queries`), but nothing spans sessions. A memory served and ignored fourteen
   times ranks exactly as it did the first time.
3. **Attribution drift exposure.** Because no profile is stamped at serve time, batch
   scoring must re-resolve attribution at score time and is exposed to binding changes
   between serve and score.

## Decision

### 1. Event weighting: amend the brain weight table

Weighting stays brain-owned. This ADR amends `FeedbackEventKind::update_weight()`:

| Event kind                               | Weight  | Change    |
| ---------------------------------------- | ------- | --------- |
| `correction`                             | 2.0     | unchanged |
| `explicit_positive`, `explicit_negative` | 1.5     | unchanged |
| `implicit_positive`, `implicit_negative` | **0.1** | was 0.5   |

The implicit weight drops before the first high-volume implicit emitter goes live, not
after. 0.5 was set when implicit events were hypothetical; a batch scorer makes them the
dominant event class by count, and the reduction is pinned now because measurement
cannot precede emission (posterior-movement comparison data cannot exist until a lid
emits). Once emission has produced comparison data across profiles, the constant is
re-evaluated. Mis-tuning at 0.1 under-trains rather than corrupts, which is the safe
failure direction.

**The explicit comparator** used by the clamp below is one `explicit_positive` or
`explicit_negative` event: **C = 1.5**.

### 2. The governing invariant and its enforcement

**Invariant (exact scope): at any instant, the decayed implicit feedback mass folded
into a posterior for a given accounting key never exceeds C, the weight of one explicit
event.** The lifetime raw sum of implicit pseudo-counts is intentionally unbounded — ten
non-overlapping decay windows of capped implicit events may in raw total exceed one
explicit judgment — but the rate is bounded: per key, implicit events can move the
posterior by at most C per decay scale, while a single explicit event moves it by C or
more immediately. Explicit feedback therefore always dominates on any horizon where it
is present at all.

Enforcement is structural, at fold time, brain-side:

- **Accounting key**: `(profile_id, namespace, target_id)`, across **all** query
  classes. (Rejected alternative: keying by `(target, query_class)` would let N distinct
  query classes deliver N·C of implicit mass to one target.)
- **Decayed implicit mass**: `M(k) = Σ w_i · 2^(−Δt_i / T)` over prior folded implicit
  events for key `k`, with `T` = 7 days (shared with the serve-ledger half-life).
- **Fold gate**: an incoming implicit event folds at its full weight only if
  `M(k) + w ≤ C`. Otherwise it is recorded in the event log (audit preserved) and folded
  at zero weight. The mass check and the fold execute in one SQLite transaction opened
  with `BEGIN IMMEDIATE`: the write lock is acquired at transaction start, so SQLite's
  database-level single-writer semantics serialize every check-and-fold against all
  concurrent writers. This holds under the current daemon's concurrent task dispatch and
  does not depend on the proposed single write-owner lane (ADR-067); if ADR-067 is
  accepted, its lane subsumes this mechanism without changing the contract.

Because the clamp is enforced at the fold rather than by the emitter, no property of the
emission surface (batching, partial failure, concurrent scorer passes) can breach it.

- **Per-pass budget** (operational, scorer-side, best-effort): a scorer pass emits at
  most 200 events; cap hits are logged, never silently truncated. This bounds cost and
  noise, not correctness — correctness is the fold gate's job.
- **Idempotency**: every scorer event carries a `scorer_run_id` and the id of the serve
  ledger row it grades (`serve_ledger_id`, section 4). The dedup key is
  `(scorer_run_id, serve_ledger_id)`: one run may legitimately grade multiple serve rows
  for the same target — the repeated-serve failure mode this ADR targets — and each row's
  grade folds as its own event. The fold rejects duplicates, so replaying a
  partially-failed batch is the recovery path.

### 3. Signal mapping

| Scorer grade   | Emitted signal      | Weight |
| -------------- | ------------------- | ------ |
| `used`         | `implicit_positive` | 0.1    |
| `ignored`      | `implicit_negative` | 0.1    |
| `contradicted` | none (see below)    | —      |

`contradicted` emits nothing automatically. The `wrong` and `correction` signals remain
caller-judged: they carry strong posterior weight and `correction` pairs with a
`supersedes` write, so both stay coupled to a deliberate curation decision (ADR-014),
never to transcript inference.

### 4. Cross-session serve ledger

A brain-owned ledger records serves and their grades. Serve rows are appended by the
recall path asynchronously off the response path; grades are backfilled by scorer
emission. The recall path consults the ledger with one read at serve time.

**Schema** (normative minimum):

| Column                  | Notes                                                            |
| ----------------------- | ---------------------------------------------------------------- |
| `id`                    | row id                                                           |
| `namespace`             | write-stamp per ADR-007                                          |
| `consumer_kind`         | e.g. `recall`                                                    |
| `served_by_profile_id`  | nullable until the section 5 stamp ships                         |
| `resolved_profile_id`   | score-time resolution result (pre-stamp phase)                   |
| `resolved_at`           | when score-time resolution ran (bounds binding drift, auditable) |
| `accounting_profile_id` | derived: `COALESCE(served_by_profile_id, resolved_profile_id)`   |
| `target_id`             | served memory                                                    |
| `query_class`           | deterministic key, defined below                                 |
| `query_raw`             | the literal query, for audit                                     |
| `served_at`             | serve timestamp                                                  |
| `grade`                 | nullable until graded (`used` / `ignored` / `contradicted`)      |
| `graded_at`             | grade timestamp                                                  |
| `scorer_run_id`         | idempotency and provenance                                       |

Uniqueness `(namespace, target_id, query_class, served_at)` for serve rows; grade
updates are idempotent by `(scorer_run_id, id)`. Indexes on `(target_id, query_class,
served_at)` for suppression reads and on
`(accounting_profile_id, namespace, target_id)` — the section 2 accounting key — for
mass queries.

**Profile attribution rule.** `accounting_profile_id` is the single profile column the
accounting key reads, normatively
`COALESCE(served_by_profile_id, resolved_profile_id)` (a generated column or the
equivalent maintained expression). When both source columns are set,
`served_by_profile_id` wins: the serve-time stamp is authoritative and
`resolved_profile_id` / `resolved_at` are retained as the drift audit trail. Before a
scorer emits for a row, it must resolve attribution and write `resolved_profile_id` +
`resolved_at` (unless the stamp already populated `served_by_profile_id`), so
`accounting_profile_id` is non-null for every row that produces a feedback event. Scorer emission passes the row's
`accounting_profile_id` as `served_by_profile_id` on the feedback call, so the profile
the fold's accounting key reads is exactly the profile the ledger attributes. An
implicit event whose serve row has no resolvable profile is recorded at zero weight —
the same fail-safe path as a clamp excess — never folded under a guessed profile.

**Query class** is a deterministic normalization of the query string: lowercase, strip
punctuation, collapse whitespace, sort unique tokens, join, take the first 16 hex chars
of the SHA-256. Two serves count as the same failure mode exactly when their normalized
token sets match; embedding-cluster or template-family keys were rejected as
non-deterministic and non-auditable.

**Suppression** is keyed `(target_id, query_class)` — per class, so recovery for one
query class is not held hostage by unrelated queries — and is a **score penalty, not a
hard filter**. The ledger records what was served; the ranker decides visibility (the
data-versus-view principle). A target repeatedly served without a `used` grade decays
toward suppression for that query class with the shared 7-day half-life and recovers as
the window decays.

### 5. Serve-time attribution stamp

`memory.recall` responses gain a `served_by_profile_id` field, resolved at serve time
via the ADR-035 three-tier discipline. This is a completion of ADR-032's own declared
surface, not new shape: ADR-032 already specifies `served_by_profile_id:
Option<String>` on profile-served event payloads and requires stage-scoped feedback to
credit exactly the profile that served (ADR-032, "Profile-served events carry
`served_by_profile_id`").

The stamp is specified here but ships as a **separate implementation PR gated on a
latency measurement**: resolution adds a cross-pack dispatch on the hot recall path
(#394). The gate is fed by the section 7 instrumentation plus caller-side failure
records (timestamp, wall-clock, error class per invocation failure) already collected by
the hook integration. Until the stamp ships, batch scoring resolves at score time and
writes `resolved_profile_id` + `resolved_at` into the ledger row, so drift is bounded
and auditable in place.

### 6. Emission surface

Scorer batches emit through the existing surface: DSL batches of `brain.auto_feedback`
(100 ops per request; the 200-event budget is two requests). `memory.recall` and
`brain.auto_feedback` cannot be chained through `$prev` (recall returns a bare array),
so emission is two-step with ids inlined.

**Parameter surface amendment.** The current handlers reject unknown fields
(`deny_unknown_fields`), so the scorer provenance that sections 2 and 4 require cannot
ride the verbs as they stand. This ADR amends `brain.feedback` and
`brain.auto_feedback` with two additive optional parameters:

| Parameter         | Type   | Required | Semantics                                     |
| ----------------- | ------ | -------- | --------------------------------------------- |
| `scorer_run_id`   | string | optional | scorer pass identifier, half of the dedup key |
| `serve_ledger_id` | string | optional | serve row being graded, half of the dedup key |

Both are persisted on the feedback event payload. They must be supplied together: a call
carrying exactly one of the two is rejected as invalid parameters (no silent coercion).
Calls carrying neither remain valid — ordinary non-scorer implicit and explicit feedback
is unchanged, folds without dedup, and is still subject to the section 2 clamp. When
both are present, the fold applies the `(scorer_run_id, serve_ledger_id)` dedup before
the clamp check and backfills the ledger row's grade. The verb-vocabulary and AGENTS.md
updates for the new parameters ride with the implementation PR (additive optional
fields; no existing caller changes shape).

ADR-016 batches are per-op independent with no cross-op transaction; that is acceptable
here **because no correctness property lives on the emission surface**: the clamp and
dedup are enforced at the brain fold (section 2), and a partially-failed batch is
recovered by idempotent replay.

A native bulk verb (`brain.feedback_batch(events=[...])`: one dispatch, single event-log
append, atomic budget accounting) is **carried as an option, not adopted**. Adoption
rule: sustained emission above roughly 400 events per day (two scorer passes), or
queue-depth instrumentation showing two-step DSL batches contending on the daemon. New
verb surface is an ADR-023 amendment when triggered.

### 7. Instrumentation rider (#394)

Three daemon-side signals ship together, motivated by the measured serve-path failure
mode (hook-fire-instant latency under concurrent load, not relevance: an identical
recall batch succeeded in 2.14 s with hits above the serve floor 90 seconds after a hook
timeout; the first instrumented caller-side failure sample is a clean 6.017 s timeout
class, not a fast daemon reject):

1. arrival-overlap logging (concurrent request arrival windows),
2. per-recall wall-clock,
3. daemon queue depth sampled at query start.

These discriminate the failure class conclusively and provide the measurement gate for
the section 5 stamp.

## Rollout

1. Scorer emission enabled on one operator lid first, only after grading precision is
   validated on sessions with knowable ground truth; posterior movement compared across
   profiles before widening.
2. Second lid, then remaining lids.
3. v1 promotes scoring to a khive-side batch job and retires the per-loop pass; design
   review at that point decides the substrate (the session mirror ingests the relevant
   records today, but attachment content lives in the raw column only, so a mirror-side
   text hoist or raw extraction is part of that review).

## Consequences

- Positive: the dominant observed waste (junk-tier re-serves) becomes self-correcting;
  negatives finally train the store; posterior quality is protected at the fold, by
  construction, independent of emitter behavior.
- Positive: attribution becomes exact once the stamp ships, and bounded and auditable in
  the ledger before then.
- Cost: a new brain-owned table, one extra read on the recall path, one async write off
  the response path, and per-key decayed-mass bookkeeping at fold time.
- Cost: the implicit weight change (0.5 → 0.1) also affects #517's serve-time
  `implicit_positive`; that is intended — the same saturation argument applies to it.
- Risk: a mis-tuned fractional weight under-trains (too low) rather than corrupts (too
  high); the revisit-on-data note covers it.

## Alternatives considered

- **Measure the weight before pinning.** Rejected: circular, since measurement requires
  emission. The fold-time clamp reduces the sensitivity of the choice; 0.1 with a
  revisit note unblocks the pipeline.
- **A separate scorer-only weight (new wire field, brain-validated).** Rejected for now:
  two implicit weights complicate the invariant math and the weight table for no current
  consumer; revisit if a second implicit emitter with different trust arrives.
- **Emitter-side clamp enforcement.** Rejected: the emission surface is non-atomic
  (ADR-016) and emitters can race; only the fold sees every event exactly once.
- **Hard filter for re-served targets.** Rejected: mutates visibility at the data layer;
  a decaying score penalty preserves the serve history in the ledger and recovers
  naturally.
- **Auto-emitting `wrong` from `contradicted` grades.** Rejected: `wrong` triggers
  supersession review and must remain a deliberate judgment.
- **Memory-pack-owned ledger.** Rejected: splits epistemic state across two packs and
  turns one seam read into two-sided bookkeeping.
- **Keying the clamp by `(target, query_class)`.** Rejected: unboundedly many query
  classes per target would multiply the per-target implicit mass bound away.
