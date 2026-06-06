// Copyright 2026 khive contributors. Licensed under Apache-2.0.
//
//! KG three-way merge — forward-deployed v2 infrastructure.
//!
//! This crate implements a semantic three-way merge for `KgArchive` snapshots.
//! It is not yet in the workspace member list because the VCS integration layer
//! must be extended to expose snapshot ancestry before the merge engine is wired
//! into the production pack. The design is retained for v2 promotion.
//!
//! # Crate layout
//!
//! - [`merge_types`] — `MergeStrategy`, `MergeConflict`, `MergeResult`, `MergeEngine` trait
//! - [`lca`] — `find_lca()`: snapshot ancestry walk (requires a `SnapshotReader`)
//! - `diff_local` — minimal entity+edge diff (private implementation detail)
//! - `entity` — entity categorization and field-level conflict analysis (private)
//! - `edge` — edge categorization and dangling-edge validation (private)
//! - `strategy` — last-write-wins shortcuts (private)
//! - [`merge`] — `three_way_merge()` top-level function + `ThreeWayMergeEngine`
//!
//! # Supported operations
//!
//! ```text
//! three_way_merge(base, ours, theirs, MergeStrategy::Auto)
//!   → MergeResult::Clean   { merged: KgArchive }
//!   → MergeResult::Conflicts { conflicts: Vec<MergeConflict> }
//! ```
//!
//! # Invariants
//!
//! - All three input archives must share the same `namespace`.
//! - All edge weights must be finite (`f64::is_finite`).
//! - Merged entity and edge output is sorted by UUID for deterministic ordering.

pub mod lca;
pub mod merge;
pub mod merge_types;

// Implementation modules — private; callers use the public re-exports below.
pub(crate) mod diff_local;
pub(crate) mod edge;
pub(crate) mod entity;
pub(crate) mod strategy;

pub use merge::ThreeWayMergeEngine;
pub use merge_types::{BranchSide, MergeConflict, MergeEngine, MergeResult, MergeStrategy};
