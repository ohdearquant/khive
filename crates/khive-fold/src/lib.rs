//! khive-fold: Cognitive primitives — Fold, Anchor, Objective, Selector.
//!
//! Fold, Anchor, Objective, and Selector primitives plus deterministic ordering
//! and composition combinators. See [`docs/design.md`](../docs/design.md) for
//! module map, invariants, dependency boundary, and failure modes.
//!
//! ```
//! use khive_fold::{fold_fn, Fold, FoldContext};
//!
//! let counter = fold_fn(
//!     |_ctx| 0usize,
//!     |count, _entry: &i32, _ctx| count + 1,
//! );
//! let result = counter.derive([1, 2, 3, 4, 5].iter(), &FoldContext::new());
//! assert_eq!(result.state, 5);
//! ```

// ── Core fold ───────────────────────────────────────────────────────────

mod compose;
mod context;
mod error;
mod fold;
mod result;

// ── Checkpoint protocol ─────────────────────────────────────────────────

pub mod checkpoint;

pub use compose::{filter, map, DualFold, FilterFold, MapFold, SequentialFold};
pub use context::{FoldContext, SharedJson};
pub use error::{FoldError, FoldResult, FoldResult as FoldResultType};
pub use fold::{
    fold_fn, AnyFold, BoxedFold, CommonFold, CommonFoldState, CountFold, FilterCountFold, FnFold,
    Fold, FoldFailure, SumI64Fold, TryFold,
};
pub use result::FoldOutcome;

// ── Checkpoint re-exports ────────────────────────────────────────────────

pub use checkpoint::{Checkpoint, CheckpointStore, InMemoryCheckpointStore};

// ── Anchor primitive ────────────────────────────────────────────────────

pub mod anchor;

pub use anchor::{Anchor, AnchorGraph, AnchorRef, BfsAnchor};

// ── Selector primitive ──────────────────────────────────────────────────

pub mod selector;

pub use selector::{GreedySelector, Selector, SelectorInput, SelectorOutput, SelectorWeights};

// ── Objective primitive ─────────────────────────────────────────────────

pub mod objective;
pub mod ordering;

pub use khive_score::{cmp_asc_then_id, cmp_desc_then_id, DeterministicScore};
pub use objective::builtin::{
    FirstMatchObjective, HasSalience, HasTimestamp, MaxScoreObjective, RecencyObjective,
    RelevanceObjective, SalienceObjective, ThresholdObjective,
};
pub use objective::compose::{
    ConsensusObjective, NegateObjective, PriorityObjective, ScaleObjective, UnionObjective,
    WeightedObjective,
};
pub use objective::error::{ObjectiveError, ObjectiveResult};
pub use objective::{objective_fn, DeterministicObjective, Objective, ObjectiveContext, Selection};
pub use ordering::{
    canonical_f32, canonical_f64, cmp_asc_score_then_id, cmp_desc_score_then_id, HasId, Ranked,
    ScoredEntry,
};

// ── ComposePipeline ─────────────────────────────────────────────────────

/// Pipeline that scores candidates with an objective then packs to budget via a selector.
pub struct ComposePipeline<T> {
    /// Graph anchor used for causal provenance traversal before scoring.
    pub anchor: Box<dyn Anchor>,
    /// Objective that assigns scores to each candidate.
    pub objective: Box<dyn Objective<T>>,
    /// Selector that packs the scored candidates under a budget.
    pub selector: Box<dyn Selector<T>>,
}

impl<T: Clone + Send + Sync + 'static> ComposePipeline<T> {
    /// Score candidates with the objective, then pack under budget with the selector.
    pub fn execute(
        &self,
        _graph: &AnchorGraph,
        candidates: Vec<SelectorInput<T>>,
        budget: usize,
        weights: &SelectorWeights,
        context: &ObjectiveContext,
    ) -> Result<SelectorOutput<T>, FoldError> {
        let scored = candidates
            .into_iter()
            .map(|mut candidate| {
                candidate.score = self.objective.score(&candidate.content, context) as f32;
                candidate
            })
            .collect();
        self.selector.select(scored, budget, weights)
    }
}
