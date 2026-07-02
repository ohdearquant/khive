# ADR-081: Recall Retune Driver — Governed Ingestion of Implicit Feedback

**Status**: Proposed\
**Date**: 2026-07-02\
**Authors**: lambda:khive, lambda:leo (scorer design and evidence)\
**Measurement evidence**: hook scorer v0 dry run over 19 serve ledgers, 7 sessions (2026-07-02)\
**Depends on**: [ADR-021](ADR-021-memory-pack.md) (Memory Pack), [ADR-033](ADR-033-recall-pipeline.md) (Recall Pipeline), [ADR-032](ADR-032-brain-profile-orchestration.md) (Brain Profile Orchestration), [ADR-055](ADR-055-epistemic-edge-relations.md) (Epistemic Relations)\
**GitHub**: #517 (auto_feedback), #394 (recall latency), #391/#393 (resolver legs)

---

## Context

### What ships today

The brain pack maintains per-profile Beta posteriors updated by feedback events. Two
verbs feed them: `brain.feedback` (explicit, caller-judged signals: `useful`,
`not_useful`, `wrong`, `correction`, and the explicit/implicit positive and negative
pairs) and `brain.auto_feedback` (#517), a convenience verb agents call after
`memory.recall` to emit implicit signals without constructing a `brain.feedback` call.

Attribution is resolved at feedback time, not serve time. `memory.recall` does not
consult the brain pack and its response carries no profile identifier. The feedback
handler (`crates/khive-pack-memory/src/handlers/feedback.rs`) resolves the serving
profile three-tier: explicit profile from config, then a namespace-bound
`brain.resolve(consumer_kind="recall")` match (real binding matches only, per the
binding discipline of ADR-032), then the pack-local global tuning prior. (The handler's
doc comments cite "ADR-035" from a pre-renumbering series; the current governing record
is ADR-032.)

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

1. **No governed implicit pathway.** Scorer events have no pinned weight, no cap
   semantics, and no invariant preventing a 200-event batch from swamping a handful of
   explicit judgments. Until that governance exists, enabling scorer emission is unsafe
   for posterior quality.
2. **No cross-session re-serve suppression.** Per-session query dedup exists
   (`recent_queries`), but nothing spans sessions. A memory served and ignored fourteen
   times ranks exactly as it did the first time.
3. **Attribution drift exposure.** Because no profile is stamped at serve time, batch
   scoring must re-resolve attribution at score time and is exposed to binding changes
   between serve and score.

## Decision

### 1. Event weighting and the governing invariant

**Invariant: implicit scorer events must never outweigh explicit feedback, individually
or in aggregate.** Three mechanisms make this structural rather than statistical:

- **Fractional weight.** Explicit signals carry full Beta pseudo-count weight, unchanged.
  Implicit events carry a fractional pseudo-count of **0.1**, defined as a single named
  constant in the memory pack configuration. The value is pinned now rather than measured
  first because measurement requires emission (the profile-comparison data cannot exist
  until a lid emits), and the per-target clamp below makes the exact value second-order.
  A revisit note: once emission has run long enough to produce posterior-movement data on
  a profile comparison, the constant is re-evaluated.
- **Per-batch cap.** A scorer pass emits at most 200 events. Cap hits are logged, never
  silently truncated.
- **Per-target clamp.** Within a decay window, the total implicit pseudo-count
  contribution to any single target cannot exceed the weight of one explicit signal.
  This bounds aggregate swamping per target no matter how many sessions re-serve it.

### 2. Signal mapping

| Scorer grade   | Emitted signal      | Weight     |
| -------------- | ------------------- | ---------- |
| `used`         | `implicit_positive` | fractional |
| `ignored`      | `implicit_negative` | fractional |
| `contradicted` | none (see below)    | —          |

`contradicted` emits nothing automatically. The `wrong` and `correction` signals remain
caller-judged: they carry strong posterior weight and `correction` pairs with a
`supersedes` write, so both stay coupled to a deliberate curation decision (ADR-014),
never to transcript inference.

### 3. Cross-session serve ledger

A serve ledger records (target id, query class, serve timestamp, grade when known). It is
**brain-owned**: serve history and posteriors are both epistemic state, and keeping them
in one pack keeps the memory/brain seam a single read. The recall path consults the
ledger with one read at serve time.

- **Suppression is a score penalty, not a hard filter.** The ledger records what was
  served; the ranker decides visibility. This follows the data-versus-view principle:
  history is preserved, the view layer decides what surfaces.
- **The window is decay-based**, consistent with memory decay semantics, with a 7-day
  half-life as the starting point. A target repeatedly served without a `used` grade
  decays toward suppression for that query class and recovers as the window decays.

### 4. Serve-time attribution stamp

`memory.recall` responses gain a `served_by_profile_id` field, resolved at serve time via
the same three-tier logic the feedback path uses. This removes score-time re-resolution
and its binding-drift exposure.

The stamp is specified in this ADR but ships as a **separate implementation PR gated on a
latency measurement**: resolution adds a cross-pack dispatch on the hot recall path
(#394). The gate is fed by the instrumentation in section 6 plus caller-side failure
records (timestamp, wall-clock, error class per invocation failure) already collected by
the hook integration. Until the stamp ships, batch scoring resolves at score time and
records the resolve timestamp so binding drift is bounded and auditable.

### 5. Emission surface

Scorer batches emit through the existing surface: DSL batches of `brain.auto_feedback`
(100 ops per request; the 200-event cap is two requests). `memory.recall` and
`brain.auto_feedback` cannot be chained through `$prev` (recall returns a bare array), so
emission is two-step with ids inlined.

A native bulk verb (`brain.feedback_batch(events=[...])`: one dispatch, single event-log
append transaction, atomic cap accounting) is **carried as an option, not adopted**.
Adoption rule: sustained emission above roughly 400 events per day (two scorer passes),
or queue-depth instrumentation showing two-step DSL batches contending on the daemon.
New verb surface is an ADR-023 amendment when triggered.

### 6. Instrumentation rider (#394)

Three daemon-side signals ship together, motivated by the measured serve-path failure
mode (hook-fire-instant latency under concurrent load, not relevance: an identical recall
batch succeeded in 2.14 s with hits above the serve floor 90 seconds after a hook
timeout):

1. arrival-overlap logging (concurrent request arrival windows),
2. per-recall wall-clock,
3. daemon queue depth sampled at query start.

These discriminate the failure class conclusively and provide the measurement gate for
the section 4 stamp.

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
  negatives finally train the store; posterior quality is protected by construction, not
  by operator discipline.
- Positive: attribution becomes exact once the stamp ships, and bounded before then.
- Cost: a new brain-owned table (serve ledger) and one extra read on the recall path;
  the clamp adds per-target bookkeeping inside the window.
- Risk: a mis-tuned fractional weight under-trains (too low) rather than corrupts (too
  high), which is the safe failure direction given the clamp.

## Alternatives considered

- **Measure the weight before pinning.** Rejected: circular, since measurement requires
  emission. The clamp reduces the sensitivity of the choice; 0.1 with a revisit note
  unblocks the pipeline.
- **Hard filter for re-served targets.** Rejected: mutates visibility at the data layer
  and destroys the audit trail; a decaying score penalty preserves history and recovers
  naturally.
- **Auto-emitting `wrong` from `contradicted` grades.** Rejected: `wrong` triggers
  supersession review and must remain a deliberate judgment.
- **Memory-pack-owned ledger.** Rejected: splits epistemic state across two packs and
  turns one seam read into two-sided bookkeeping.
