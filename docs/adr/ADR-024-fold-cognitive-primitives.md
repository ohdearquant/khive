# ADR-024: Fold Cognitive Primitives

**Status**: accepted\
**Date**: 2026-05-23\
**Authors**: khive maintainers

## Context

Memory recall, retrieval ranking, context-window selection, and brain profile evolution
all share the same shape: a deterministic derivation of state or ranked output from a
stream of typed inputs under explicit scoring and budget rules. Without a shared
foundation, every consumer reimplements the scoring loop, the tie-breaker, and the
budget pack — each with its own determinism bug.

The architecture must satisfy:

1. **Pure-math foundation.** No IO, no async, no domain types. Cognitive primitives
   compose with any consumer; consumers do the IO.
2. **Deterministic ordering across platforms.** Scores enter and leave the primitive
   layer as `DeterministicScore` (ADR-006). Tie-breaking is canonical (score desc,
   UUID asc). Same inputs → same output bytes on every CPU.
3. **Composable.** Objectives compose into weighted/priority/threshold combinators.
   Folds compose into sequential, dual, filter, and map pipelines. New combinators are
   built from existing ones, not bolted on at consumer sites.
4. **Hoare-structured.** Each fold pass corresponds to a Hoare triple `{P} c {Q}`:
   precondition is the anchor state, program is select-via-objective, postcondition is
   the ranked output. This structural correspondence enables replay verification:
   re-run the fold on frozen inputs and confirm the output matches.

These four primitives — Fold, Anchor, Objective, Selector — together cover every
cognitive computation khive performs. Brain (ADR-032) composes them into profiles.
Retrieval (ADR-031) uses Objective as its scoring contract. Recall (ADR-033) uses
Selector for budget-constrained top-k. Memory (ADR-021) ranks via Objective.

## Decision

### Add `khive-fold` crate to the foundation layer

```text
khive-types        (domain types, no_std)
  ├── khive-score  (DeterministicScore)
  └── khive-fold   (Fold + Anchor + Objective + Selector)  ← this ADR
```

Foundation-layer. Depends only on `khive-types` and `khive-score`. No IO, no async, no
domain knowledge. Downstream crates (`khive-runtime`, `khive-pack-memory`,
`khive-pack-brain`, future packs) consume the trait surface.

### Four core traits

```rust
/// Deterministic state derivation: entries → state.
/// Same entries + same context = same state, on every platform.
///
/// **Replay invariant**: same ordered `entries` + same serialized `FoldContext` produce
/// the same final state and same `entries_processed` count. No IO, no async, no clock.
pub trait Fold<L, S>: Send + Sync {
    fn init(&self, ctx: &FoldContext) -> S;
    fn reduce(&self, state: S, entry: &L, ctx: &FoldContext) -> S;
    fn finalize(&self, state: S, ctx: &FoldContext) -> S { state }
    fn derive<'a, I>(&self, entries: I, ctx: &FoldContext) -> FoldOutcome<S>
    where Self: Sized, I: IntoIterator<Item = &'a L>, L: 'a;
}

/// Causal graph traversal: provenance chains and credit assignment.
/// AnchorGraph is materialized by the caller (runtime layer); Anchor traverses it.
pub trait Anchor: Send + Sync {
    fn trace(&self, graph: &AnchorGraph, start: &AnchorRef, max_depth: usize)
        -> Result<Vec<AnchorRef>, FoldError>;
    fn credit(&self, graph: &AnchorGraph, outcome: &AnchorRef, max_depth: usize)
        -> Result<Vec<(AnchorRef, f64)>, FoldError>;
}

/// Scoring and selection: candidates → ranked output.
/// Pure math; receives pre-computed features via ObjectiveContext.
pub trait Objective<T>: Send + Sync {
    fn score(&self, candidate: &T, context: &ObjectiveContext) -> f64;
    fn select<'a>(&self, candidates: &'a [T], context: &ObjectiveContext)
        -> Vec<Selection<&'a T>>;
    fn select_top<'a>(&self, candidates: &'a [T], n: usize, context: &ObjectiveContext)
        -> Vec<Selection<&'a T>>;
}

/// Budget-constrained subset selection: many → fewer under a hard budget.
/// Budget can be token count, byte count, or candidate count.
pub trait Selector<T>: Send + Sync {
    fn select(&self, inputs: Vec<SelectorInput<T>>, budget: usize,
              weights: &SelectorWeights) -> Result<SelectorOutput<T>, FoldError>;
}
```

### Supporting types

```rust
pub struct AnchorRef    { pub id: Uuid, pub kind: String, pub stable_id: Option<String> }
pub struct AnchorGraph  { pub nodes: Vec<AnchorRef>, pub edges: Vec<(Uuid, Uuid, String)> }
pub struct FoldContext  { pub extra: serde_json::Value, /* shared metadata, thread-safe */ }

/// Deterministic outcome — byte-stable across runs given same input.
pub struct FoldOutcome<S>  { pub state: S, pub entries_processed: usize }

/// Run-local telemetry — NOT part of the deterministic outcome.
/// Kept separate so replay hashes cover state + entries only.
pub struct FoldTelemetry  { pub duration_micros: u64, pub started_at_unix_us: i64 }
pub struct ObjectiveContext { pub min_score: Option<f64>, pub max_candidates: Option<usize>,
                              pub extra: serde_json::Value }
pub struct Selection<T> { pub item: T, pub score: f64, pub precision: f64, pub index: usize }
pub struct SelectorInput<T> { pub id: String, pub content: T, pub size: usize,
                              pub score: f32, pub category: Option<String>,
                              pub information_gain: Option<f32> }
pub struct SelectorOutput<T> { pub selected: Vec<SelectorInput<T>>,
                               pub total_size: usize, pub budget: usize }
pub struct SelectorWeights { pub category_weights: BTreeMap<String, f32>,
                             pub min_score: f32, pub diversity_bias: f32,
                             pub epistemic_weight: f32 }
```

### Six built-in objectives (common strategies)

| Objective                           | Behavior                                  |
| ----------------------------------- | ----------------------------------------- |
| `MaxScoreObjective`                 | Highest raw score wins                    |
| `ThresholdObjective`                | Pass/fail gate at a score threshold       |
| `FirstMatchObjective`               | First candidate that passes the predicate |
| `RecencyObjective<T: HasTimestamp>` | Temporal weighting                        |
| `SalienceObjective<T: HasSalience>` | Salience-weighted                         |
| `RelevanceObjective`                | Relevance scoring from context            |

### Six composition combinators (objective algebra)

| Combinator           | Algebraic meaning                              |
| -------------------- | ---------------------------------------------- |
| `WeightedObjective`  | Weighted sum of sub-objectives                 |
| `PriorityObjective`  | Lexicographic — try first, fall back to second |
| `ConsensusObjective` | Geometric mean of sub-objectives               |
| `UnionObjective`     | Max of sub-objectives                          |
| `NegateObjective`    | Invert scores                                  |
| `ScaleObjective`     | Multiply scores by a constant                  |

### Four fold composition combinators

| Combinator       | Behavior                                                       |
| ---------------- | -------------------------------------------------------------- |
| `SequentialFold` | Run fold₁, use its state to build context for fold₂            |
| `DualFold`       | Run two folds independently over the same entries; return both |
| `FilterFold`     | Predicate gate before folding                                  |
| `MapFold`        | Transform entries before folding                               |

### Determinism guarantees

All ordering is deterministic across platforms via `khive-score::DeterministicScore`:

- `Objective::score()` returns `f64` (the math layer).
- Ranking and selection convert to `DeterministicScore` (i64 fixed-point, 2³² scale) at
  the comparison boundary.
- `ScoredEntry<T>` stores `DeterministicScore` internally; `Ord` impl delegates to score's
  deterministic comparison.
- Tie-breaking: score descending, then UUID ascending.
- `canonical_f64` (branchless NaN normalization) cleans up f64s before fixed-point
  conversion.
- `Selector` tie-breaking: score desc, size asc, id asc.

### v1 invariants — what `Fold` is and is not

The trait shape above is the locked v1 contract. Deviations require an ADR amendment.

- **Single non-generic `FoldContext`.** No `Fold<T, S, C>` generic context parameter.
  Domain consumers (brain, validation, audit) wrap their typed context and project into
  `FoldContext.extra`. This keeps `Box<dyn Fold<T, S>>` type-erased uniformly across the
  ecosystem and avoids per-consumer trait-object proliferation.
- **No context smuggling into `T`.** The canonical stream item stays substrate-shaped
  (`Event`, `Note`, `Entity`). Tuples like `(Event, &Context)` are rejected — they
  introduce lifetime complexity, weaken replay clarity, and encourage ambient state
  leakage.
- **No `precision` on `Fold`.** Reliability scoring lives on `Objective::precision`;
  uncertainty lives in the state type (e.g., `BetaPosterior::variance()`). A future
  `FoldReliability` extension trait is the escape hatch if a concrete need appears.
- **No IO, no async, no clock.** `Fold` is pure math. `duration_micros` lives in
  `FoldTelemetry`, separate from `FoldOutcome`, so the deterministic replay hash covers
  `(state, entries_processed)` only.
- **`init / reduce / finalize`.** Public method names are stable. `finalize` defaults to
  identity. Legacy `step` / `initial` naming in pack-internal code is acceptable but
  the public trait uses `reduce` / `init`.

### Bayesian extensions (precision and epistemic_weight)

Two extensions for Bayesian/Active-Inference grounding, both default-identity so existing
code is unaffected.

**Precision-weighted Objective output**:

```rust
pub trait Objective<T>: Send + Sync {
    fn score(&self, candidate: &T, context: &ObjectiveContext) -> f64;

    /// Precision estimate for the score. Default 1.0 (fully trusted).
    /// Implementations override when reliability varies across candidates.
    fn precision(&self, _candidate: &T, _context: &ObjectiveContext) -> f64 { 1.0 }
}
```

Effective ranking score = `score * precision`. When `precision = 1.0` (default), behavior
is identical to a scalar-score objective. Predictive Coding (Rao & Ballard 1999, Friston
2005) formalizes this: not all inputs are equally reliable; precision (inverse variance)
weights each signal.

**Epistemic Selector weight**:

```rust
pub struct SelectorWeights {
    pub category_weights: BTreeMap<String, f32>,
    pub min_score: f32,
    pub diversity_bias: f32,
    pub epistemic_weight: f32,  // 0.0 = pure pragmatic (default)
}
```

Effective selection score = `pragmatic_score + epistemic_weight * information_gain`. When
`epistemic_weight = 0.0`, behavior is identical to pragmatic-only. Active Inference
(Friston et al. 2015) formalizes this: intelligent selection balances pragmatic value
(reach preferred states) and epistemic value (reduce uncertainty).

`information_gain` is pre-computed by the caller (typically as KL divergence between
prior and posterior). The Selector is pure-math and has no embedding-space access;
preserving the no-IO invariant means the caller must materialize the gain estimate.

### Hoare-structure documentation requirement

Every domain-specific fold implementation (memory scoring, lore composition, retrieval
ranking, brain profile evolution) MUST document its Hoare triple in its module doc:

- **Precondition**: what anchor state / context is required
- **Program**: what objective function is applied, what selector budget
- **Postcondition**: what invariants the output satisfies

This is a documentation convention, not a compile-time check. The formal verification
path (styx/) can later promote these to machine-checked properties.

### ComposePipeline — the canonical fold pass

```rust
pub struct ComposePipeline<T> {
    pub anchor:    Box<dyn Anchor>,
    pub objective: Box<dyn Objective<T>>,
    pub selector:  Box<dyn Selector<T>>,
}

impl<T> ComposePipeline<T> {
    /// Execute the full fold pass.
    /// Precondition: anchor graph is materialized.
    /// Postcondition: output is deterministically ranked within budget.
    pub fn execute(
        &self,
        graph: &AnchorGraph,
        candidates: Vec<SelectorInput<T>>,
        budget: usize,
        weights: &SelectorWeights,
        context: &ObjectiveContext,
    ) -> Result<SelectorOutput<T>, FoldError>;
}
```

Memory recall (ADR-033), retrieval pipelines (ADR-031), and brain profiles (ADR-032) all
instantiate `ComposePipeline` with their own `Anchor`, `Objective`, and `Selector`
implementations. The pipeline is the bridge between the foundation layer and consumers.

### Excluded from `khive-fold` (lives in consumers)

| Item                                                       | Lives in                              | Why                                 |
| ---------------------------------------------------------- | ------------------------------------- | ----------------------------------- |
| `ObjectiveRegistry` (named registration + lookup)          | `khive-runtime`                       | Runtime infrastructure, not algebra |
| `Checkpoint<S>` + `InMemoryCheckpointStore`                | `khive-fold`                          | Pure in-memory; no IO or async deps |
| `Scored<T>` + `ObjectiveConfig` (thin wrappers)            | Inline in consumers                   | Not fold-specific                   |
| `FoldContext.actor/role/task/query` (domain fields)        | Consumers put in `extra`              | Couples fold to actor model         |
| Domain folds (`MemoryFold`, `PolicyFold`)                  | Pack crates owning the domain         | Domain-specific                     |
| Retrieval objectives (`VectorSimilarity`, `TextRelevance`) | `khive-runtime::objectives` (ADR-031) | Depend on runtime types             |
| Cross-encoder rerank, BM25 scoring                         | `khive-retrieval` (ADR-030)           | Algorithm-specific                  |

`khive-fold` is pure math: traits, built-in strategies, composition combinators,
deterministic ordering. Everything else composes on top.

### Canonical T values

The traits are generic, but two substrate-level streams are the canonical inputs:

| T        | Source                                                                                 | Example consumer                                                  |
| -------- | -------------------------------------------------------------------------------------- | ----------------------------------------------------------------- |
| `Event`  | [ADR-022](ADR-022-events-query-surface.md) events substrate via `EventStore` (ADR-005) | Brain `Fold<Event, BalancedRecallState>` (ADR-032)                |
| `Note`   | Note substrate via `NoteStore` (ADR-005)                                               | Memory recall scoring `Objective<NoteCandidate>` (ADR-033)        |
| `Entity` | Entity substrate via `EntityStore` (ADR-005)                                           | Validation rule passes `Fold<Entity, ValidationReport>` (ADR-034) |

The relationship to ADR-022's `EventFilter`: it is the SQL-executable concrete form of
`Objective<Event> → bool`. The same predicate set runs server-side as a WHERE clause or
client-side as a Rust function; the abstract shape is the Objective. Pack-side event
aggregators (brain posteriors, audit summaries, future analytics) implement
`Fold<Event, State>` rather than inventing per-consumer reducer types — the combinators
from §132 (`SequentialFold`, `FilterFold`, `MapFold`, `DualFold`) compose naturally
over event streams.

### Canonical Fold input

The canonical event-log fold is `Fold<Event, S>`. `EventView` (defined in ADR-041)
is a read-side wrapper that carries observation rows and session context for consumer
queries — it is NOT the canonical replay input. Using `EventView` as the fold input
would make replay depend on observation/session annotations that are not part of the
ordered event log.

If a projection truly needs observations as part of replay (rare), define a separate
typed input (e.g., `ObservedEvent { event: Event, observation: EventObservation }`)
and parameterize `Fold` over that — but do not reuse `EventView`, which is reserved
for query/read surfaces.

## Rationale

### Why one crate, not four

Fold ADR-005 (in the predecessor `foundation/fold/` directory) rejected splitting into
`khive-fold` + `khive-anchor` + `khive-objective` + `khive-selector`. Same reasoning
here: the four primitives are one concept (paper-folding metaphor — fold-lines + fold-rules

- fold-act + budget). Splitting forces artificial import boundaries and obscures the
  algebraic relationships (Objective composition, ComposePipeline).

### Why Hoare structure as a convention, not a compile-time check

Promoting Hoare triples to machine-checked properties requires either Lean4 proofs (styx/
work) or a runtime contract system. Neither is ready today. The documentation convention
captures intent now; formal verification can land later without API churn.

### Why precision and epistemic_weight default to identity

Backwards compatibility. Every existing Objective implementation inherits `precision() =>
1.0`. Existing `SelectorWeights` gain a new field with `Default` at `0.0`. No call site
changes; consumers opt in when they have calibrated precision estimates or information-gain
inputs.

### Why caller pre-computes information_gain

Computing KL divergence (or Fisher information, or any uncertainty estimate) requires
access to the embedding space or model posterior. The Selector is pure-math and has no
such access. Pushing pre-computation to the caller preserves the no-IO invariant of the
foundation layer; the runtime layer (which has IO) is the right place to materialize the
gain estimate.

## Alternatives Considered

### A. Traits only, no built-in objectives

Ship just the trait surface; consumers implement every strategy themselves. Rejected:
`MaxScoreObjective`, `WeightedObjective`, `GreedySelector` are common strategies every
consumer needs. Without them, every consumer reimplements them — with subtly different
tie-breaking. The built-ins are the canonical reference.

### B. Make precision a separate wrapper type

`PrecisionWeighted<O>` wraps any Objective; the score is `inner.score() * precision`.
Rejected: every caller must remember to wrap before passing to Selector. Integration into
the trait method (default 1.0) is cheaper and harder to forget.

### C. Compute information_gain inside Selector

Selector takes an `EmbeddingService` dependency and computes KL itself. Rejected:
violates the no-IO invariant; Selector would need async; foundation crate would gain
service dependencies it cannot resolve.

### D. Use Wasserstein distance instead of KL

Wasserstein is a proper metric (triangle inequality); KL is not. The lattice-transport
crate already implements Sinkhorn-regularized OT. Rejected for v1: Wasserstein requires
full distribution access (not just a scalar gain per candidate); `SelectorInput` would
need an embedding vector field, bloating the type. KL-gain per candidate is a scalar that
fits the existing architecture. Wasserstein-scored variants can live in the runtime layer
where distribution access exists.

### E. Skip the Hoare-structure claim — ship types only

Pros: simpler ADR, less theoretical commitment. Cons: loses the structural bridge to
replay verification, formal verification, and brain profile correctness. The Hoare claim
is what makes fold more than a utility library — it's what connects cognitive computation
to provable correctness. Without it, brain profile orchestration (ADR-032) has no formal
grounding.

Rejected. The Hoare structure is the insight that justifies fold as a first-class
architectural concept.

### F. Add a full Predictive Coding hierarchy

Multi-layer precision-weighted prediction errors with belief propagation. Pros: more
faithful to the neuroscience. Cons: massive scope increase; no current consumer exists.
Single-layer precision-weighting (the `precision()` trait method) captures the key
insight with a one-field addition.

Deferred. Multi-layer hierarchy can land when brain profile orchestration demonstrates
need.

## Consequences

### Positive

- **One foundation crate**, four primitives, six built-in objectives, six composition
  combinators, four fold combinators.
- **Cross-platform determinism** via `DeterministicScore`; replay verification is
  feasible without per-consumer scaffolding.
- **Hoare-triple bridge** between cognitive computation and Decision Anatomy /
  audit-completeness arguments.
- **Bayesian-ready** via precision and epistemic_weight defaults; opt-in by callers with
  calibrated estimates.
- **No ordering duplication**: ranking primitives (`QuantKey`, `Ranked`, comparators) are
  re-exported from `khive-score`, not reimplemented.

### Negative

- **One more crate** in the foundation layer. Compile time impact: minor — the crate is
  pure-math with no async or IO.
- **Hoare convention is not enforced at compile time** — until Lean4 proofs land, the
  triples are claims in doc comments. Mitigated: replay verification (run the fold on
  frozen inputs, compare bytes) is mechanically checkable in tests.

### Neutral

- **MCP wire format unchanged** — fold is internal to runtime and packs.
- **`khive-score` unchanged** — fold consumes its existing API.
- **`khive-types` unchanged** — fold defines its own type set, doesn't extend domain
  types.

## Open Questions

1. **Downstream Fold interop.** How should downstream or legacy Fold implementations
   interoperate with the canonical `khive-fold` crate? Specifically: should derived
   projects ship their own Fold variants, or extend the canonical types via
   `inventory`-style registration? Resolution deferred.
2. **Multi-layer Predictive Coding.** When brain profile orchestration (ADR-032) matures,
   should precision propagate across compose stages (fold₁'s precision feeds fold₂'s
   prior)? Defer to operational evidence.

## References

- [ADR-006](ADR-006-deterministic-scoring.md) — `DeterministicScore` and canonical ordering
- [ADR-021](ADR-021-memory-pack.md) — memory recall consumes Objective composition
- [ADR-030](ADR-030-retrieval-stack-port.md) — retrieval objectives are Objective impls
- [ADR-031](ADR-031-multi-engine-retrieval.md) — multi-engine fusion composes Objectives
- [ADR-032](ADR-032-brain-profile-orchestration.md) — brain profiles compose Fold + Anchor + Objective + Selector
- [ADR-033](ADR-033-recall-pipeline.md) — `recall.score` exposes Objective breakdowns
- Hoare, C.A.R., "An Axiomatic Basis for Computer Programming" (1969)
- Predictive Coding: Rao & Ballard, "Predictive coding in the visual cortex" (1999)
- Free Energy Principle: Friston, "A theory of cortical responses" (2005)
- Active Inference: Friston et al., "Active inference and epistemic value" (2015)
