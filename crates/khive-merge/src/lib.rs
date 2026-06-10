// Copyright 2026 Haiyang Li. Licensed under Apache-2.0.
//
//! KG three-way merge.
//!
//! See `crates/khive-merge/docs/design.md` for architecture and invariants.

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
