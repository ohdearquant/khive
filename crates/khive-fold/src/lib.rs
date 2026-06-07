//! Fold, Anchor, Objective, and Selector primitives with deterministic ordering and composition.

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

mod pipeline;
pub use pipeline::ComposePipeline;
