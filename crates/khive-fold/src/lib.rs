//! khive-fold: Cognitive primitives — Fold, Anchor, Objective, Selector.
//!
//! Four cognitive primitives that form the "paper-folding" operation:
//!
//! - **Fold**: `entries → derived state` (deterministic reduce)
//! - **Anchor**: causal graph traversal (provenance chains)
//! - **Objective**: score candidates and select best
//! - **Selector**: budget-constrained pack (many → subset)
//!
//! Plus deterministic ordering primitives, composition combinators,
//! and common strategies (Recency, Relevance, Weighted, etc.).
//!
//! # Quick Start
//!
//! ```
//! use khive_fold::{fold_fn, Fold, FoldContext};
//!
//! let counter = fold_fn(
//!     |_ctx| 0usize,
//!     |count, _entry: &i32, _ctx| count + 1,
//! );
//!
//! let entries = [1, 2, 3, 4, 5];
//! let result = counter.derive(entries.iter(), &FoldContext::new());
//! assert_eq!(result.state, 5);
//! ```

// ── Core fold ───────────────────────────────────────────────────────────

mod compose;
mod context;
mod error;
mod fold;
mod result;

pub use compose::{filter, map, DualFold, FilterFold, MapFold, SequentialFold};
pub use context::{FoldContext, SharedJson};
pub use error::{FoldError, FoldResult, FoldResult as FoldResultType};
pub use fold::{
    fold_fn, AnyFold, BoxedFold, CommonFold, CommonFoldState, CountFold, FilterCountFold, FnFold,
    Fold, FoldFailure, SumI64Fold, TryFold,
};
pub use result::FoldOutcome;

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
    FirstMatchObjective, HasImportance, HasTimestamp, ImportanceObjective, MaxScoreObjective,
    RecencyObjective, RelevanceObjective, ThresholdObjective,
};
pub use objective::compose::{
    ConsensusObjective, NegateObjective, PriorityObjective, ScaleObjective, UnionObjective,
    WeightedObjective,
};
pub use objective::error::{ObjectiveError, ObjectiveResult};
pub use objective::{objective_fn, DeterministicObjective, Objective, ObjectiveContext, Selection};
pub use ordering::{
    canonical_f32, canonical_f64, cmp_asc_score_then_id, cmp_desc_score_then_id, HasId, QuantKey,
    Ranked, ScoredEntry,
};
