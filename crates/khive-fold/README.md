# khive-fold

Cognitive primitives shared across khive's runtime: `Fold` (streaming state
reduction), `Anchor` (causal provenance graphs), `Objective` (deterministic
candidate scoring and selection), and `Selector` (budget-constrained packing).
Depends only on `khive-types` and `khive-score` — see
[ADR-024](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-024-fold-cognitive-primitives.md).

## Usage

```rust
use khive_fold::{fold_fn, Fold, FoldContext};

let count_positive = fold_fn(
    |_ctx: &FoldContext| 0usize,
    |count, entry: &i32, _ctx| if *entry > 0 { count + 1 } else { count },
);

let entries = [1, -2, 3, 4, -5];
let outcome = count_positive.derive(entries.iter(), &FoldContext::new());
assert_eq!(outcome.state, 3);
assert_eq!(outcome.entries_processed, 5);
```

`Fold<L, S>` reduces a stream of `&L` entries into a state `S` via `init` +
`reduce` (+ optional `finalize`); `fold_fn(initial, step)` builds one from two
closures without a custom type. `derive` runs the fold to completion and
returns a `FoldOutcome<S>` (`state`, `entries_processed`); `derive_filtered`
adds a predicate that skips entries before `reduce` sees them. `TryFold` is
the fallible counterpart, returning `FoldFailure` on error. `CountFold`,
`FilterCountFold`, and `SumI64Fold` are ready-made folds for the common
counting/summing cases.

`FoldContext::new()` defaults `as_of` to the Unix epoch, not wall-clock
time — this crate never calls a clock. Callers that need "as of now" build a
context explicitly with `FoldContext::at(timestamp)`.

## Anchor — causal provenance

`AnchorGraph` is an in-memory graph of `AnchorRef` nodes (`id`, `kind`,
`stable_id`) connected by labeled edges. `Anchor::trace` follows forward edges
from a starting anchor up to `max_depth` hops; `Anchor::credit` walks backward
from an outcome anchor, returning `(AnchorRef, weight)` pairs with the
contribution weight halving per hop. `BfsAnchor` is the provided BFS
implementation of both.

## Objective — deterministic scoring

`Objective<T>` scores and filters candidates (`score`, `passes_score`,
`batch_score`, `select`, `select_top`); `DeterministicObjective<T>` extends it
for `T: HasId`, breaking score ties by ID so `select_top_deterministic` gives
the same ranking regardless of input order. Built-in objectives
(`RelevanceObjective`, `RecencyObjective`, `SalienceObjective`,
`ThresholdObjective`, `MaxScoreObjective`, `FirstMatchObjective`) and combinators
(`WeightedObjective`, `ConsensusObjective`, `PriorityObjective`,
`UnionObjective`, `NegateObjective`, `ScaleObjective`) compose via the
`objective` module; `objective_fn` builds one from a closure.

## Selector — budget-constrained packing

`Selector<T>::select(inputs: Vec<SelectorInput<T>>, budget, weights)` collapses
N scored, sized candidates into a `SelectorOutput<T>` that fits `budget`.
`GreedySelector` filters by `SelectorWeights.min_score`, applies
`category_weights` multipliers, then packs by effective score (score plus an
optional `epistemic_weight * information_gain` term) with deterministic
size-then-ID tie-breaking; `diversity_bias > 0` switches to a pick-best-remaining
loop that penalizes repeated categories.

## Checkpoint — durable fold snapshots

`Checkpoint<S>::new(id, state, uuid, entries_processed, context, fold_version)`
wraps a serializable fold state with a BLAKE3 content hash (`khive_types::Hash32`)
computed from the state, verified on load. Like `FoldContext`, it never calls
a clock — `created_at` defaults to the epoch; callers set it explicitly if wall-clock
time matters. `CheckpointStore` / `InMemoryCheckpointStore` provide a save/load
contract for callers that persist checkpoints across restarts.

## Where this sits

Built on `khive-types` and `khive-score` only — no other khive-* dependency,
by design (ADR-024's boundary). Used by higher layers of the runtime that need
deterministic state reduction, provenance tracing, or budget-constrained
context packing without pulling in storage or query machinery.

## License

BUSL-1.1. See the repository [LICENSE](https://github.com/ohdearquant/khive/blob/main/LICENSE).
