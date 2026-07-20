//! KG three-way merge.
//!
//! See `crates/khive-merge/docs/semantic-merge-architecture.md` for design and
//! `crates/khive-merge/docs/api/three-way-merge.md` for the caller contract.

pub mod diff_local;
pub mod edge;
pub mod entity;
pub mod lca;
pub mod merge;
pub mod strategy;
pub mod types;

pub use merge::ThreeWayMergeEngine;
pub use types::{
    BranchSide, MergeConflict, MergeEngine, MergeError, MergeResult, SnapshotMergeStrategy,
};
