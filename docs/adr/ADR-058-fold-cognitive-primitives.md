# ADR-058: Fold Cognitive Primitives — Hoare-Structured Decisions

**Status**: proposed\
**Date**: 2026-05-21\
**Authors**: Ocean, lambda:khive

## Context

Four cognitive primitives — Fold, Anchor, Objective, Selector — are the foundation under every
memory recall, lore composition, context-window selection, and retrieval ranking operation in the
system. They currently live in `foundation/fold/` (~5.2K LOC) with their own ADR history
(fold ADR-004 "Paper-Folding Primitives", ADR-005 "Unified Cognitive Primitives Home") that
established the design: all four primitives co-located in one crate at the foundation layer,
pure-math, no IO, no async. `khive-score` (which fold depends on) is already in this repo;
fold itself is not yet.

This ADR adds `khive-fold` to the crate graph and introduces one structural claim absent from the
earlier fold ADRs: **each fold pass forms a Hoare triple**, making fold passes formally equivalent
to decisions in the canonsys Decision Anatomy sense. This claim has consequences for audit
completeness, formal verification, and the Brain Phase 7 compose pipeline.

### The paper-folding metaphor (fold ADR-004, preserved)

The substrate — notes, entities, and edges (ADR-004 observables) — is the paper. The cognitive
operation is:

1. **Anchor** = fold-lines. Reference points that establish context. Any UUID can serve as an
   anchor — "anchorness" is conferred by the fold's configuration, not by a type field.
2. **Objective** = fold-rules. Scoring and selection criteria: which candidates to consider, how
   to weight them, how to break ties deterministically.
3. **Fold** = the fold act. Collapse a sequence of entries into a derived state. Same entries +
   same context = same state (deterministic).
4. **Selector** = fold under budget. A budget-constrained variant that selects the best candidates
   within a token, byte, or count limit.

The result is a structure — a context window, a retrieval result set, a ranked selection.
Fold-lines + fold-rules + fold-act = paper plane.

### The Hoare structure (new claim)

Each fold pass has the shape `{P} c {Q}`:

| Hoare component | Fold component | Type |
|-----------------|---------------|------|
| **Precondition {P}** | Anchor state — what role, what context, what provenance chain | `AnchorGraph` |
| **Program c** | Selector picks candidates under budget; Objective scores them | `Selector<T>` + `Objective<T>` |
| **Postcondition {Q}** | Ranked/scored output, deterministic | `FoldOutcome<S>` / `Selection<T>` |

This structural correspondence maps to the Decision Anatomy Invariant (a compliance
framework pattern where every decision = Facts + Evidence + Policy + Verdict + Certificate):

| Decision Anatomy | Fold pass |
|-----------------|-----------|
| Facts | Anchor state (what is already known) |
| Evidence | Selected candidates (what was considered) |
| Policy | Objective function (the scoring/selection rule) |
| Verdict | Ranked output (the decision) |
| Certificate | `FoldOutcome` provenance (entries_processed, timing, context) |

The correspondence is structural: if a fold pass satisfies the Hoare triple (precondition holds,
program terminates, postcondition verified), then Decision Anatomy validity follows. Compliance
becomes verification: replay the fold on frozen inputs and confirm the output matches.

Note: this is a structural correspondence, not a formal isomorphism. Anchor state is data, not a
logical predicate; FoldOutcome is a value, not a postcondition assertion. Promoting this to a
full Hoare-calculus equivalence (with composition rules over fold combinators) is a formalization
goal, not a current claim.

## Decision

### 1. Add `khive-fold` crate

New crate at `crates/khive-fold/` with the four cognitive primitives. Pure-math, no IO, no async.
Foundation-layer dependency: depends on `khive-types` and `khive-score` only.

### 2. Core traits

```rust
/// Deterministic state derivation: entries → state.
pub trait Fold<L, S> {
    fn initial(&self, context: &FoldContext) -> S;
    fn step(&self, state: S, entry: &L, context: &FoldContext) -> S;
    fn finalize(&self, state: S, context: &FoldContext) -> S;
    fn derive<'a, I>(&self, entries: I, context: &FoldContext) -> FoldOutcome<S>
    where Self: Sized, I: IntoIterator<Item = &'a L>, L: 'a;
}

/// Causal graph traversal: provenance chains and credit assignment.
pub trait Anchor {
    fn trace(&self, graph: &AnchorGraph, start: &AnchorRef, max_depth: usize)
        -> Result<Vec<AnchorRef>, FoldError>;
    fn credit(&self, graph: &AnchorGraph, outcome: &AnchorRef, max_depth: usize)
        -> Result<Vec<(AnchorRef, f32)>, FoldError>;
}

/// Scoring and selection: candidates → ranked output.
pub trait Objective<T>: Send + Sync {
    fn score(&self, candidate: &T, context: &ObjectiveContext) -> f64;
    fn select<'a>(&self, candidates: &'a [T], context: &ObjectiveContext)
        -> ObjectiveResult<Selection<&'a T>>;
    fn select_top<'a>(&self, candidates: &'a [T], n: usize, context: &ObjectiveContext)
        -> Vec<Selection<&'a T>>;
}

/// Budget-constrained subset selection: many → fewer under budget.
pub trait Selector<T> {
    fn select(&self, inputs: Vec<SelectorInput<T>>, budget: usize, weights: &SelectorWeights)
        -> Result<SelectorOutput<T>, FoldError>;
}
```

### 3. Supporting types

```rust
pub struct AnchorRef { pub id: Uuid, pub kind: String, pub stable_id: Option<String> }
pub struct AnchorGraph { pub nodes: Vec<AnchorRef>, pub edges: Vec<(Uuid, Uuid, String)> }
pub struct FoldContext { /* shared JSON metadata, thread-safe */ }
pub struct FoldOutcome<S> { pub state: S, pub entries_processed: usize, /* timing */ }
pub struct ObjectiveContext { pub min_score: Option<f64>, pub max_candidates: Option<usize>, pub extra: Value }
pub struct Selection<T> { pub item: T, pub score: f64, pub index: usize, /* stats */ }
pub struct SelectorInput<T> { pub id: String, pub content: T, pub size: usize, pub score: f32, pub category: Option<String> }
pub struct SelectorOutput<T> { pub selected: Vec<SelectorInput<T>>, pub total_size: usize, pub budget: usize }
pub struct SelectorWeights { pub category_weights: BTreeMap<String, f32>, pub min_score: f32, pub diversity_bias: f32 }
```

### 4. Built-in objectives and composition combinators

**6 built-in objectives** (common strategies, pure-math):
- `MaxScoreObjective` — highest raw score wins
- `ThresholdObjective` — pass/fail gate at a score threshold
- `FirstMatchObjective` — first candidate that passes
- `RecencyObjective` — temporal weighting (requires `HasTimestamp`)
- `ImportanceObjective` — importance-weighted (requires `HasImportance`)
- `RelevanceObjective` — relevance scoring from context

**6 composition combinators** (objective algebra):
- `WeightedObjective` — weighted sum of sub-objectives
- `PriorityObjective` — lexicographic: try first, fallback to second
- `ConsensusObjective` — geometric mean of sub-objectives
- `UnionObjective` — max of sub-objectives
- `NegateObjective` — invert scores
- `ScaleObjective` — multiply scores by a constant

**Fold composition** (fold algebra):
- `SequentialFold` — run fold₁, use its state to build context for fold₂
- `DualFold` — run two folds independently over same entries, return both results
- `FilterFold` — predicate gate before folding
- `MapFold` — transform entries before folding

### 5. Determinism guarantees

All ordering is deterministic across platforms:
- Scores use `canonical_f64` (branchless NaN normalization, IEEE-754 total order)
- Tie-breaking: score descending, then UUID ascending (`DeterministicObjective<T>`)
- `FoldOutcome` includes `entries_processed` count for replay verification
- `Selector` tie-breaking: score descending, size ascending, id ascending

### 6. Hoare-structure documentation requirement

Every domain-specific fold implementation (memory scoring, lore composition, retrieval ranking)
must document its Hoare triple:
- **Precondition**: what anchor state / context is required
- **Program**: what objective function is applied, what selector budget
- **Postcondition**: what invariants the output satisfies

This is a documentation convention, not a compile-time check. The formal verification path
(Lean4 in styx/) can later promote these to machine-checked proofs.

### 7. Brain Phase 7 compose pipeline interface (locked)

The compose pipeline is `Anchor → Selector(Objective) → Output`:

```rust
pub struct ComposePipeline<T> {
    pub anchor: Box<dyn Anchor>,
    pub objective: Box<dyn Objective<T>>,
    pub selector: Box<dyn Selector<T>>,
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

The `ComposePipeline` is the Brain Phase 7 target. Its interface is locked by this ADR.
Implementation follows in a separate PR. Domain-specific pipelines (memory compose, lore compose,
retrieval compose) instantiate `ComposePipeline` with their own `Anchor`, `Objective`, and
`Selector` implementations.

## Alternatives Considered

### A. Traits only, leave implementations private

Pros: smaller surface. Cons: traits without implementations are unusable. The 6 built-in
objectives and `GreedySelector` are the common strategies every consumer needs. Without them,
every user reimplements `MaxScoreObjective`.

Rejected. Ship traits + common strategies together.

### B. Split into 4 separate crates (khive-fold, khive-anchor, khive-objective, khive-selector)

Pros: fine-grained dependencies. Cons: four crates for four aspects of one concept. The
paper-folding metaphor shows these are one operation — splitting them forces artificial import
boundaries. Fold ADR-005 rejected this for the same reason.

Rejected. One crate, one concept.

### C. Skip the Hoare-structure claim, ship types only

Pros: simpler ADR, less controversial. Cons: loses the structural bridge to Decision Anatomy and
formal verification. The Hoare claim is what makes fold more than a utility library — it's what
connects cognitive computation to provable correctness. Without it, Brain Phase 7 compose has no
formal grounding.

Rejected. The Hoare structure is the insight that justifies fold as a first-class architectural
concept rather than a utility crate.

### D. Add precision and epistemic-weight fields now (Predictive Coding / Active Inference bridges)

Pros: mathematically justified extensions (Objective `precision: f64` for Predictive Coding,
Selector `epistemic_weight: f64` for Active Inference). Cons: no consumer exists yet. Brain
Phase 7 compose is not implemented. Adding fields nothing reads is speculative design.

Deferred. These are natural extensions once compose lands. File as follow-up ADR (ADR-059)
after this ADR is accepted and Brain Phase 7 has a concrete implementation to consume them.

### E. Add IOCTA-derived audit completeness now

Pros: reduces Lean4 proof obligation from O(n) per component to one derived theorem. Cons: the
derivation is conjectured, not formalized. Claiming it as a decision before proving it in styx/
would be premature.

Deferred to styx/ formalization. Note the conjecture in this ADR; file formal proof as follow-up.

## Consequences

### Positive

- **Cognitive primitives available**: `use khive_fold::{Fold, Anchor, Objective, Selector}`
- **Brain Phase 7 interface locked**: `ComposePipeline` contract prevents implementation drift
- **Hoare structure documented**: every domain fold has a stated triple, enabling future formal verification
- **Decision Anatomy bridge explicit**: fold passes are decisions; compliance = replay verification
- **Determinism guaranteed**: cross-platform reproducibility via canonical ordering

### Negative

- **Crate grows to ~10K LOC**: acceptable for foundation-level primitives with comprehensive tests
- **Hoare documentation convention is not enforced**: until Lean4 proofs land, the triples are
  claims in doc comments, not machine-checked properties
- **`parking_lot` in foundation**: `ObjectiveRegistry` uses `RwLock`. Pure synchronization, no IO.

## Open Questions

1. **Precision and epistemic-weight timing**: when should ADR-059 (Bayesian-brain extensions) be
   drafted? After Brain Phase 7 compose lands, or in parallel?
2. **IOCTA derivation**: is the conjecture that fold audit completeness follows from substrate
   axioms sound? Needs styx/ investigation before claiming.
3. **Cloud fold stub**: `khive-cloud/crates/fold` exists as a stub. Should it re-export from
   `khive-fold` or remain a separate cloud-specific layer?

## References

- `foundation/fold/` (~5.2K LOC, 8 fold-local ADRs)
- Fold ADR-004: Paper-Folding Primitives (2026-05-06, Ocean + lambda:khive)
- Fold ADR-005: Unified Cognitive Primitives Home (2026-05-06, Ocean + lambda:khive)
- Decision Anatomy Invariant — compliance pattern where every decision = Facts + Evidence + Policy + Verdict + Certificate
- Insert-Only Closed-Taxonomy Architecture — design pattern: edge relations from fixed typed set, append-only persistence
- Hoare Logic: C.A.R. Hoare, "An Axiomatic Basis for Computer Programming" (1969)
- ADR-004: Substrate Observables — three-observable model (Note, Entity, Event)
- ADR-006: Deterministic Scoring — `DeterministicScore` and canonical ordering
