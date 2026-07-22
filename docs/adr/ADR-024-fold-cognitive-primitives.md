# ADR-024: Deterministic Fold Primitives

**Status**: accepted\
**Date**: 2026-05-23\
**Authors**: khive maintainers

## Context

Retrieval ranking, budgeted selection, and deterministic state derivation share one
computational shape: derive state or ranked output from typed inputs under explicit
scoring, ordering, and budget rules. Implementing that loop independently in every
consumer would duplicate tie-breaking and replay semantics.

The shared layer must satisfy four constraints:

1. **Pure computation.** The layer performs no IO, asynchronous work, or implicit clock
   reads. Consumers materialize all inputs before calling it.
2. **Deterministic ordering.** Ranking crosses a fixed-point comparison boundary through
   `DeterministicScore` from ADR-006 and uses stable tie-breakers.
3. **Composition.** Objectives and folds combine through reusable algebraic operators.
4. **Replayability.** The same ordered inputs and serialized context produce the same
   state and processed-entry count.

## Decision

### `khive-fold` foundation crate

`khive-fold` contains the `Fold`, `Anchor`, `Objective`, and `Selector` primitives. It
depends on `khive-types` and `khive-score` and exposes no storage or transport API.
Runtime and pack crates may compose these primitives after they have completed IO.

```text
khive-types
  ├── khive-score
  └── khive-fold
```

### Core traits

```rust
pub trait Fold<L, S>: Send + Sync {
    fn init(&self, ctx: &FoldContext) -> S;
    fn reduce(&self, state: S, entry: &L, ctx: &FoldContext) -> S;
    fn finalize(&self, state: S, ctx: &FoldContext) -> S { state }

    fn derive<'a, I>(&self, entries: I, ctx: &FoldContext) -> FoldOutcome<S>
    where
        Self: Sized,
        I: IntoIterator<Item = &'a L>,
        L: 'a;
}

pub trait Anchor: Send + Sync {
    fn trace(
        &self,
        graph: &AnchorGraph,
        start: &AnchorRef,
        max_depth: usize,
    ) -> Result<Vec<AnchorRef>, FoldError>;

    fn credit(
        &self,
        graph: &AnchorGraph,
        outcome: &AnchorRef,
        max_depth: usize,
    ) -> Result<Vec<(AnchorRef, f64)>, FoldError>;
}

pub trait Objective<T>: Send + Sync {
    fn score(&self, candidate: &T, context: &ObjectiveContext) -> f64;
    fn precision(&self, _candidate: &T, _context: &ObjectiveContext) -> f64 { 1.0 }
    fn select<'a>(
        &self,
        candidates: &'a [T],
        context: &ObjectiveContext,
    ) -> Vec<Selection<&'a T>>;
    fn select_top<'a>(
        &self,
        candidates: &'a [T],
        n: usize,
        context: &ObjectiveContext,
    ) -> Vec<Selection<&'a T>>;
}

pub trait Selector<T>: Send + Sync {
    fn select(
        &self,
        inputs: Vec<SelectorInput<T>>,
        budget: usize,
        weights: &SelectorWeights,
    ) -> Result<SelectorOutput<T>, FoldError>;
}
```

`Objective::precision` is a code-level reliability multiplier and defaults to the
identity value. `SelectorInput::information_gain` and
`SelectorWeights::epistemic_weight` are optional auxiliary ranking inputs in the shipped
API. Their defaults preserve the base score. This ADR does not prescribe a probabilistic
model or a domain interpretation for those fields.

### Supporting types

```rust
pub struct AnchorRef {
    pub id: Uuid,
    pub kind: String,
    pub stable_id: Option<String>,
}

pub struct AnchorGraph {
    pub nodes: Vec<AnchorRef>,
    pub edges: Vec<(Uuid, Uuid, String)>,
}

pub struct FoldContext {
    pub extra: serde_json::Value,
}

pub struct FoldOutcome<S> {
    pub state: S,
    pub entries_processed: usize,
}

pub struct FoldTelemetry {
    pub duration_micros: u64,
    pub started_at_unix_us: i64,
}

pub struct ObjectiveContext {
    pub min_score: Option<f64>,
    pub max_candidates: Option<usize>,
    pub extra: serde_json::Value,
}

pub struct Selection<T> {
    pub item: T,
    pub score: f64,
    pub precision: f64,
    pub index: usize,
}

pub struct SelectorInput<T> {
    pub id: String,
    pub content: T,
    pub size: usize,
    pub score: f32,
    pub category: Option<String>,
    pub information_gain: Option<f32>,
}

pub struct SelectorWeights {
    pub category_weights: BTreeMap<String, f32>,
    pub min_score: f32,
    pub diversity_bias: f32,
    pub epistemic_weight: f32,
}
```

`FoldTelemetry` is intentionally separate from `FoldOutcome`. Timing values are
run-local and must not enter replay hashes.

### Built-in objectives

The crate ships the following common strategies:

| Objective                           | Behavior                              |
| ----------------------------------- | ------------------------------------- |
| `MaxScoreObjective`                 | Select by raw score                   |
| `ThresholdObjective`                | Require a score threshold             |
| `FirstMatchObjective`               | Select the first matching candidate   |
| `RecencyObjective<T: HasTimestamp>` | Weight by an explicit reference time  |
| `SalienceObjective<T: HasSalience>` | Weight by the candidate's salience    |
| `RelevanceObjective`                | Combine relevance inputs from context |

`RecencyObjective` and `SalienceObjective` are part of the shipped public crate. The
reference time for recency is passed through `ObjectiveContext`; the objective does not
read the system clock.

### Composition

Objective composition consists of `WeightedObjective`, `PriorityObjective`,
`ConsensusObjective`, `UnionObjective`, `NegateObjective`, and `ScaleObjective`.
Fold composition consists of `SequentialFold`, `DualFold`, `FilterFold`, and `MapFold`.

`ComposePipeline<T>` combines an anchor, objective, and selector:

```rust
pub struct ComposePipeline<T> {
    pub anchor: Box<dyn Anchor>,
    pub objective: Box<dyn Objective<T>>,
    pub selector: Box<dyn Selector<T>>,
}

impl<T> ComposePipeline<T> {
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

### Determinism contract

- `Objective::score` may use `f64`, but comparisons convert through
  `DeterministicScore`.
- Non-finite values are normalized or rejected at the documented boundary.
- Ranked items use score descending, then stable identifier ascending.
- Budget packing uses score descending, size ascending, then identifier ascending.
- `Fold` receives an ordered iterator and does not reorder entries implicitly.
- `FoldContext` contains only caller-supplied serialized data.

The replay contract covers `FoldOutcome`, not `FoldTelemetry`.

### Context and domain boundaries

`FoldContext` is one non-generic container. Domain consumers project typed context into
`extra`; they do not add domain fields to the foundation crate or smuggle context into
the stream item. Validation, event projection, and retrieval ranking may define their
own folds in their owning crates.

The canonical substrate inputs are `Event`, `Note`, and `Entity`, but the traits remain
generic. `EventView` from ADR-041 is a read-side wrapper and is not the canonical event
replay input. A projection that genuinely requires observations defines a separate typed
input rather than changing the meaning of `Event`.

Each domain fold documents:

- its input preconditions;
- the reduction and selection program; and
- the invariants of its output.

These are reviewable contracts and test targets. No machine-checked proof requirement is
part of this ADR.

### Excluded responsibilities

The following remain outside `khive-fold`:

| Responsibility                   | Owner                     |
| -------------------------------- | ------------------------- |
| Named objective registration     | `khive-runtime`           |
| Validation-specific folds        | validation implementation |
| Retrieval algorithms and indexes | `khive-retrieval`         |
| Cross-encoder reranking and BM25 | retrieval implementation  |
| Storage-backed checkpoints       | storage-owning consumer   |

The pure in-memory `Checkpoint<S>` and `InMemoryCheckpointStore<S>` types remain in
`khive-fold` because they perform no IO.

## Rationale

### Why one crate

The four primitives form one composition model. Keeping them together makes the
deterministic ordering rules and combinators available without artificial dependency
boundaries. Algorithm-specific and storage-specific behavior remains in consumers.

### Why a single context type

A generic context parameter would multiply trait-object types across consumers. A single
serialized context keeps `Box<dyn Fold<L, S>>` uniform while leaving domain schemas under
consumer control.

### Why explicit context and no IO

Implicit clocks, services, or storage reads would make replay depend on ambient state.
Materializing inputs and reference values before execution keeps the core synchronous,
testable, and deterministic.

### Why built-in objectives

Common ranking strategies need one implementation of non-finite handling and tie-breaking.
The built-ins provide that reference behavior while the trait remains open to consumer
implementations.

## Alternatives Considered

| Alternative                       | Reason rejected                                                |
| --------------------------------- | -------------------------------------------------------------- |
| Traits only, with no built-ins    | Duplicates ordering and threshold behavior across consumers    |
| Separate crate for each primitive | Adds dependency boundaries without separating responsibilities |
| Generic context type              | Prevents a uniform trait-object surface                        |
| IO-capable or asynchronous folds  | Introduces ambient state and weakens replay guarantees         |
| Domain fields in `FoldContext`    | Couples the foundation crate to particular consumers           |

## Consequences

### Positive

- One deterministic composition surface is shared by validation, event, and retrieval
  consumers.
- `DeterministicScore` centralizes comparison behavior.
- Built-in recency and salience objectives document the shipped public API.
- Replay tests can compare frozen inputs with serialized outcomes.

### Negative

- Consumers must materialize data before invoking the fold layer.
- Domain context requires explicit serialization into `FoldContext.extra`.
- A larger shared crate requires care to keep algorithm-specific dependencies out.

### Neutral

- The MCP wire format is unchanged.
- `khive-types` substrate definitions are unchanged.
- `khive-score` remains the ordering authority.

## References

- [ADR-006](./ADR-006-deterministic-scoring.md): `DeterministicScore` and ordering
- [ADR-022](./ADR-022-events-query-surface.md): event substrate and query surface
- [ADR-030](./ADR-030-retrieval-stack-port.md): retrieval algorithms
- [ADR-031](./ADR-031-multi-engine-retrieval.md): multi-engine composition
- Hoare, C.A.R., "An Axiomatic Basis for Computer Programming" (1969)
