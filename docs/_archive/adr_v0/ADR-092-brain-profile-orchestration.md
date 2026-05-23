# ADR-092: Brain as Strategy-Profile Orchestration over Fold + Objective

**Status**: proposed\
**Date**: 2026-05-22 (drafted; renumbered from ADR-080 → ADR-092 on 2026-05-22 to avoid
collision with the SubstrateCoordinator ADR series at 080/086-089)\
**Authors**: Ocean, lambda:khive\
**Depends on**: ADR-036 (Memory pack semantics), ADR-058 (Fold Cognitive Primitives)\
**Composed with**: ADR-090 (khive-retrieval port), ADR-091 (Multi-engine retrieval
composition), ADR-064 (Brain Architecture — this ADR is the richer profile-orchestration
direction that supersedes ADR-064's scalar-weight-only design)

## Context

khive's brain pack today learns three scalar parameters (`recall::relevance_weight`,
`recall::importance_weight`, `recall::temporal_weight`) via Bayesian Beta posteriors,
updated by in-place `brain.emit` calls. This works for the small scalar parameter
space but does not extend to the architecture we are heading toward:

- ADR-091 introduces a much larger calibration surface (engine weights, strategy
  weights, per-context buckets).
- Future work will add per-note salience adjustments, multi-dimensional decay
  matrices, fusion-strategy parameters, retrieval-rerank weights, possibly RL
  Q-values for action selection. The space is open-ended.
- Operators need to evolve the calibration model itself — not just its parameter
  values. Today there is no way to ship a new feedback definition without losing
  history.
- Multiple operators / contexts want different calibrations. A single global
  posterior surface cannot represent these.

The right framing is **quantitative-finance backtesting**: a profile is a
_strategy_; the event log is _market data_; live evolution is _paper trading_;
backtest is _historical performance evaluation_ under a quality metric;
promotion is the _deploy decision_ based on backtest evidence.

The unifying insight: **khive already has the abstractions for this in
`khive-fold`**. The four cognitive primitives are exactly the strategy-profile
shape generalized:

- `Fold<L, S>` (entries → derived state) **is the profile's evolution rule**.
  Folding events into profile state collapses the event-state possibility space
  to one trajectory.
- `Objective<T>` (score + select) **is both the profile's ranker at retrieval
  time AND the backtest quality metric**.
- `Anchor` (causal graph traversal) **handles cursor look-ahead / look-behind**
  for evolution rules that depend on temporal context.
- `Selector` (budget-constrained pack) **handles top-k under
  budget / diversity / context constraints**.

`khive-fold` also ships **objective composition combinators** (`objective/compose.rs`):
weighted sum, fallback, threshold gating, score modification. Multi-objective
quality metrics for backtests fall out of these.

This ADR does **not** introduce new abstractions. It orchestrates the existing
ones into a brain that supports event-sourced strategy profiles with replay-based
backtesting.

## Decision

**A profile is a typed composition of `khive-fold` primitives.** Brain provides
the orchestration platform — event log, snapshot persistence, lifecycle,
backtest execution — but does **not** define new primitive traits.

```rust
pub struct Profile {
    pub id: String,
    pub description: String,
    pub metadata: ProfileMetadata,

    // The four cognitive primitives, composed:
    pub evolver: Box<dyn Fold<Event, StateBlob>>,       // events → state
    pub anchor:  Option<Box<dyn Anchor>>,                // optional cursor lookup
    pub ranker:  Box<dyn Objective<RetrievalCandidate>>, // recall-time ranking
    pub selector: Box<dyn Selector<RetrievalResult>>,    // top-k with constraints

    // Serialization adapter for the evolver's state type:
    pub snapshot_adapter: Box<dyn SnapshotAdapter>,
}
```

`StateBlob` is opaque to brain. The profile's `Fold` knows its concrete state
type; the snapshot adapter knows how to serialize that type via
`ruvector-snapshot::SnapshotManager`.

### The complete data flow

```
EVENT LOG (system-wide, append-only, schema-versioned)
   │
   ├── live tick: profile.evolver.step(state, event)  →  state'
   │   (every active profile gets every event)
   │
   ├── backtest: profile.evolver.derive(history)  →  trajectory
   │   ├── for each historical recall event:
   │   │     candidates = recall_event.candidates
   │   │     counterfactual = profile.ranker.select_top(candidates, k)
   │   │     score = quality_objective.score(counterfactual, comparison_target)
   │   └── aggregate scores → backtest result
   │
   ├── recall: state = profile.evolver.derive(events_since_snapshot)
   │           ranked = profile.ranker.select_top(candidates, k * 3)
   │           top_k  = profile.selector.pack(ranked, k, constraints)
   │           emit RecallExecuted event (closes the loop)
   │
   └── snapshot: profile.snapshot_adapter.serialize(state)  →  ruvector-snapshot
```

Backtest is **literally `derive + score`**. No separate engine, no new
primitive. The infrastructure is event log management, snapshot persistence,
profile lifecycle. Everything else is composition of existing primitives.

### System-wide event log

All packs emit structured events to a shared, append-only, time-ordered,
schema-versioned log. Default-on for every pack. Today's `brain.events` table
generalizes into this store.

```rust
pub struct Event {
    pub id: Uuid,
    pub timestamp: DateTime<Utc>,
    pub namespace: String,
    pub actor: Option<String>,
    pub kind: EventKind,                      // RecallExecuted | LinkCreated | TaskTransitioned | ...
    pub payload: serde_json::Value,           // schema versioned per kind
    pub payload_schema_version: u32,
    pub profile_state_version: Option<u64>,   // for recall events: pin the state version used
}
```

Replay must handle every historical payload schema — per-kind migration registry
upgrades old payloads to the latest shape before profile evolvers see them.

The event log is the system of record. Profile states are derivable from it.

### Profile state — opaque to brain, owned by the Fold

Each profile's `Fold<Event, S>` defines its own `S`. Brain never inspects `S`.
Examples of what `S` can be:

- **Scalar weights** — today's `recall::*_weight` triple, migrated as the
  default `BalancedRecallProfile`'s state.
- **Engine + strategy weight matrices** — ADR-091 calibration state.
- **Per-note salience adjustments** with TTL — stored via
  `ruvector-temporal-tensor` (Hot/Warm/Cold tiering by access).
- **Per-(namespace, kind, language) decay tensors** — multi-dimensional.
- **Neural rerank weights** — small models trained from feedback.
- **RL Q-values** — discrete action selection.
- **Conformal calibration sets** — uncertainty quantification.

State _schema_ evolution = registering a new profile id with a new `Fold`
impl. The old profile keeps working on old data; the new profile starts fresh
or migrates explicitly. Brain never breaks because schema lives in profile code.

### Snapshot + delta substrate

Profile state is persisted via `ruvector-snapshot::SnapshotManager` with
`ruvector-delta-core` for delta-encoded intermediate snapshots:

```rust
pub struct ProfileSnapshot {
    pub profile_id: String,
    pub at_event_id: Uuid,
    pub at_timestamp: DateTime<Utc>,
    pub state_blob: Vec<u8>,                   // adapter-serialized Fold state
    pub is_delta: bool,
    pub base_snapshot_id: Option<Uuid>,
}
```

Storage policy: full snapshot every N events, delta-encoded snapshots between
them. Per-profile tuning. Cold storage / archival for old snapshots is a
follow-up operational concern, not an ADR-level decision.

To reconstruct state at any historical event id:

1. Find latest full snapshot ≤ event_id.
2. Apply intervening deltas in order.
3. Apply remaining events sequentially via `evolver.step`.

This is just `Fold::derive` starting from a non-initial state. No new code path.

### Backtest = derive + score

```rust
pub struct BacktestRequest {
    pub profile_id: String,
    pub from_event_id: Uuid,
    pub to_event_id: Option<Uuid>,
    pub starting_snapshot_id: Option<Uuid>,
    pub quality: Box<dyn Objective<TrajectoryOutcome>>,
    pub comparison: ComparisonTarget,
}

pub enum ComparisonTarget {
    AnotherProfile(String),
    ActualHistory,
    SyntheticGroundTruth(Vec<RankedResult>),
}

pub struct BacktestResult {
    pub profile_id: String,
    pub events_replayed: u64,
    pub recall_events_scored: u64,
    pub aggregate_score: f64,
    pub performance_curve: Vec<(Uuid, f64)>,
    pub divergences: Vec<Divergence>,
}
```

Implementation in ~12 lines of Rust:

```rust
async fn backtest(req: BacktestRequest) -> BacktestResult {
    let profile = registry.get(&req.profile_id)?;
    let mut state = restore_state(&profile, req.starting_snapshot_id)?;
    let events = event_log.range(req.from_event_id, req.to_event_id).await?;
    let mut curve = Vec::new();
    for event in &events {
        if let EventKind::RecallExecuted = event.kind {
            let candidates = decode_candidates(&event.payload)?;
            let counterfactual = profile.ranker.select_top(candidates, k);
            let outcome = build_outcome(counterfactual, &req.comparison, event);
            let score = req.quality.score(&outcome);
            curve.push((event.id, score));
        }
        state = profile.evolver.step(state, event, &ctx);
    }
    BacktestResult { performance_curve: curve, aggregate: aggregate(curve), .. }
}
```

Quality is just another `Objective`. Multi-metric quality is composition via
`khive-fold::objective::compose::WeightedSum` / `Fallback` / etc. — no new
combinators needed.

### Live evolution

Live update loop runs continuously:

```
for each new event:
  for each active profile P:
    P.state = P.evolver.step(P.state, event, ctx)
    if snapshot_due(P):
      persist via ruvector-snapshot (full or delta)
```

O(1) per event per profile. Active profile count bounded by operator config.

### Profile lifecycle

```
defined  →  registered  →  active        ⇄  inactive
                              ↓                 ↑
                           canonical        archived
                              ↑
                         (operator promotes)
```

- **Defined**: profile code + metadata exist.
- **Registered**: brain knows about it; backtest-eligible against any window.
- **Active**: live updates run; snapshots persist.
- **Canonical**: the active profile that `recall` (and similar consumers)
  defaults to. Exactly one canonical per consumer-kind.
- **Inactive**: registered but no live updates.
- **Archived**: snapshots and event log retained for audit; not in active set.

Promotion is explicit operator decision informed by backtest results. Brain
emits `ProfilePromotionRecommended` events when a non-canonical profile beats
the canonical by configured margin on a configured window. Auto-promotion is
**out of scope** — runaway feedback risk needs its own ADR.

### Determinism boundary

khive's `khive-score::DeterministicScore` (i64 fixed-point) is the canonical
score type — bit-identical across platforms. Some implementation sources
(notably RuVector) operate in f32 and achieve replay determinism only on the
same machine (via `to_bits()` hashing + sorted iteration). This is a real
asymmetry and we handle it explicitly.

**The rule**: at every boundary where a score enters brain event log or
profile state, convert to `DeterministicScore`. Inside an adapter's inner loop
(HNSW walk, SIMD fusion, MMR scoring) f32 is fine. The score that gets
_stored_ or _compared across profiles_ is fixed-point.

What this guarantees:

- All scores in events are bit-identical across platforms.
- All profile state evolution is bit-identical (state updates work in fixed-point).
- All `Objective::score()` outputs returning fixed-point are bit-identical.
- Backtest aggregate scores are bit-identical.

What this does _not_ guarantee:

- HNSW walk order can vary across SIMD platforms (different reduction orders
  → different candidate enumeration). Top-k membership may differ by 1-2
  items in rare cases. This is baked into HNSW's approximate nature; we
  accept it.
- Profile _promotion decisions_ are not affected — they compare backtest
  scores, which are deterministic.

**Future** (out of scope here): if cross-platform bit-identical _retrieval_
becomes a hard requirement (compliance, audit, research-grade reproducibility),
we glue handrolled deterministic versions of affected primitives into the
same trait surface. The trait abstraction makes this a contained change.
This is not pursued upstream-to-RuVector — too much work for ruv relative to
the benefit; the seam-conversion approach is sufficient for Phase 1+.

### Brain verb surface

| Verb                                          | Purpose                                            |
| --------------------------------------------- | -------------------------------------------------- |
| `brain.profiles`                              | List profiles by lifecycle stage                   |
| `brain.profile(id)`                           | Metadata, latest snapshot, current state summary   |
| `brain.backtest(profile_id, quality, ...)`    | Run backtest with specified quality `Objective`    |
| `brain.compare(profile_a, profile_b, window)` | Head-to-head backtest on shared window             |
| `brain.snapshot(profile_id)`                  | Force snapshot now                                 |
| `brain.activate(profile_id)`                  | Move to Active                                     |
| `brain.deactivate(profile_id)`                | Move to Inactive                                   |
| `brain.promote(profile_id)`                   | Move to Canonical                                  |
| `brain.events`                                | Existing — generalized to all event kinds          |
| `brain.emit`                                  | Existing — manual emission for testing             |
| `brain.config`                                | Existing — profile config, quality metric defaults |

`feedback(target_event_id, score, reason)` (top-level verb) emits a
`FeedbackExplicit` event. Profile evolvers may consume or ignore it.

## Rationale

### Why "profile = composition of existing primitives" instead of new trait?

Because `khive-fold` already ships exactly what's needed. Adding a `Profile`
trait with `apply`/`update`/`snapshot` methods would _re-export_ what
`Fold`, `Objective`, and `ruvector-snapshot` provide, under different names.
Wrong direction — abstractions multiplied beyond necessity.

This also unifies brain with the rest of khive's cognitive surface: anyone
who learned `Fold` and `Objective` for compose / curation / retrieval already
knows how brain works.

### Why use khive-fold's composition combinators for quality metrics?

Because multi-metric quality scoring is exactly what `WeightedSum`,
`Fallback`, `ThresholdGated` were built for. Operator declares:

```toml
[brain.quality.balanced]
type = "weighted_sum"
weights = [
  { objective = "cosine_alignment", weight = 0.5 },
  { objective = "subsequent_action_alignment", weight = 0.3 },
  { objective = "latency_penalty", weight = 0.2 },
]
```

Each named objective is a registered `Objective<TrajectoryOutcome>`. Composition
is just config. Zero new code.

### Why backtest, not just live A/B?

Backtest is deterministic, cheap, risk-free, and produces results in
seconds-to-minutes. Live A/B requires traffic splitting, user-visible behavior
changes, statistical-significant sample on real traffic. Use backtest by
default; reserve live A/B for signals backtest can't capture (rare for
retrieval calibration).

### Why explicit operator promotion, not auto?

Self-tuning systems can game their own quality metrics. Operator-in-the-loop
is the safety hatch. Auto-promotion is a future ADR with its own risk
analysis.

### Why per-profile state stores, not one shared?

Because profiles disagree about success/failure for the same event. State is
strategy-specific; sharing it across profiles defeats the point. Per-profile
storage cost is bounded (parameters × profiles, both small).

### Why event log spans all packs?

Because backtest fidelity requires full history. Recall outcomes depend on
what entities were created, links made, tasks transitioned. Whole-system
event log → whole-system replay fidelity.

### Why profile state is opaque to brain?

Because state schema evolution must not require brain core changes. Today
scalar weights; tomorrow per-note salience matrix; next quarter neural rerank
weights. Each is a new `Fold` impl with its own state type. Brain just
serializes via the snapshot adapter and never inspects the bytes.

## Alternatives Considered

| Alternative                                                | Pros                             | Cons                                               | Why rejected                                   |
| ---------------------------------------------------------- | -------------------------------- | -------------------------------------------------- | ---------------------------------------------- |
| Introduce a new `Profile` trait with apply/update/snapshot | Self-contained brain abstraction | Re-exports what `Fold`+`Objective` already provide | Unnecessary abstraction multiplication         |
| Single canonical update rule (no profiles)                 | Simplest                         | Cannot evolve feedback definition; no A/B          | Doesn't address the real problem               |
| Live A/B for all profile comparisons                       | Real-traffic signal              | Slow, expensive, risk-bearing                      | Backtest covers most cases at fraction of cost |
| In-place posterior updates, no snapshots                   | Cheaper storage                  | No replay, no backtest, no audit                   | Forecloses everything ADR-092 enables          |
| Brain owns state schema; profiles only fill in values      | Brain enforces structure         | Every state shape change = brain breaking change   | Wrong locus of evolution                       |

## Consequences

### Positive

- **Architectural surface stays minimal** — we orchestrate existing primitives
  rather than introducing new traits.
- **Calibration model is versionable** — each (Fold, Objective) pair is a
  registered profile; new state shapes = new registered profile; old still works.
- **Backtest is trivial** — `Fold::derive + Objective::select_top + composition
  combinator`. Twelve lines.
- **Multi-objective quality** — falls out of `compose.rs` for free.
- **Audit and reproducibility** — pin the snapshot id + event range, replay is
  deterministic.
- **RuVector composition** — snapshot, delta, temporal-tensor, coherence,
  conformal already engineered.
- **Existing cognitive-primitives knowledge transfers** — anyone who learned
  `Fold` / `Objective` understands brain immediately.

### Negative

- **Existing scalar-weight Bayesian posteriors must migrate to a Fold** —
  small one-time work; the algorithm doesn't change.
- **Snapshot storage cost** — bounded by delta encoding + tunable interval.
- **More moving parts than today's brain** — but each piece is bounded and
  reuses existing primitives.

### Neutral

- Documentation explains the framework with examples; the new abstractions are
  pre-existing `khive-fold` types.

## Implementation phases

### Phase 0 — System-wide event log (foundation)

1. Move event emission from `khive-pack-brain` to `khive-runtime`. Every pack
   handler emits via the runtime; runtime persists to a shared `events` store.
2. Generalize event payload schema; add `payload_schema_version`.
3. Make log queryable by `(time_range, kind, namespace, actor, target_event_id)`.

### Phase 1 — Profile orchestration + snapshot integration

1. Define `Profile` _struct_ (not trait) in `khive-pack-brain` as the
   composition of `Fold` + `Objective` + `Anchor`? + `Selector` + metadata.
2. Define `SnapshotAdapter` trait — single method, type-erased serialization
   of the Fold's state type.
3. Wire `ruvector-snapshot::SnapshotManager` for `ProfileSnapshot` persistence.
4. Migrate today's 3-scalar Bayesian state into a `BalancedRecallProfile`
   composed of:
   - Fold that updates the 3 Beta posteriors from events
   - Objective that ranks candidates by `(rrf × relevance + salience × importance + decay × temporal)`
   - Selector that picks top-k
   - SnapshotAdapter for the 3 Beta pairs
5. Live update loop: every active profile's `evolver.step` on each event.

### Phase 2 — Backtest execution

1. Implement `brain.backtest` as the `derive + score` flow.
2. Register built-in quality `Objective`s:
   - `cosine_alignment` (via `ruvector-coherence::quality::cosine_similarity`)
   - `subsequent_action_alignment` (cursor look-ahead via `Anchor`)
   - `explicit_feedback_alignment` (consumes `FeedbackExplicit` events)
   - `latency_penalty`
3. `brain.compare` for head-to-head.

### Phase 3 — Delta snapshots

1. Integrate `ruvector-delta-core` for delta-encoded intermediate snapshots.
2. GC old deltas after the next full snapshot supersedes them.
3. Per-profile snapshot interval tuning.

### Phase 4 — Rich-state reference profile

1. Build `PerNoteSalienceProfile` — Fold state is a per-note salience adjustment
   vector stored via `ruvector-temporal-tensor` (Hot/Warm/Cold tiering by access).
2. Backtest against canonical to demonstrate non-trivial state shape.
3. Document the pattern.

### Phase 5 — Promotion workflow

1. `brain.promote`, `brain.activate`, `brain.deactivate`.
2. `ProfilePromotionRecommended` events on backtest-exceeds-margin.
3. Operator CLI hook for review-and-promote.

## Open questions to resolve during implementation

1. **Event log retention vs replay fidelity.** Unbounded log cost vs reduced
   replay fidelity from compaction. Tentative: time-tiered retention (full
   events N months, compacted summaries afterward, summaries sufficient for
   most-but-not-all backtests).

2. **Profile state schema versioning across upgrades.** Schema change ⇒ new
   profile id ⇒ old snapshots keep working with old profile id; new profile
   starts fresh or explicitly migrates.

3. **Anchor cursor look-ahead bounds.** Tentative: K = 24 hours of event-log
   time. Unbounded look-ahead makes replay expensive.

4. **Coordination with multi-actor / cloud tier.** Each tenant has its own
   profile state; profile _definitions_ (Fold + Objective code) shared across
   tenants; backtests are tenant-scoped.

## References

- ADR-036 — Memory pack semantics (current scalar-weight recall implementation)
- ADR-058 — Fold Cognitive Primitives (Fold + Anchor + Objective + Selector this ADR composes)
- ADR-064 — Brain Architecture (this ADR is the richer successor to ADR-064's scalar design)
- ADR-090 — khive-retrieval port (the ported HNSW/BM25/fusion stack)
- ADR-091 — Multi-engine retrieval composition (the calibration surface brain operates on)
- `khive-fold` — the cognitive primitives this ADR orchestrates:
  - `Fold<L, S>` — profile evolver
  - `Objective<T>` — profile ranker and quality metric
  - `Anchor` — cursor lookup for temporal context
  - `Selector` — top-k under constraints
  - `objective::compose` — multi-metric quality composition
- RuVector primitives composed:
  - `ruvector-snapshot` — profile state persistence
  - `ruvector-delta-core` — delta-encoded snapshots
  - `ruvector-temporal-tensor` — time-evolving tiered state for rich-state profiles
  - `ruvector-coherence::quality` — built-in cosine/L2 quality metrics
  - `ruvector-core::conformal_prediction` — uncertainty calibration inside profiles
