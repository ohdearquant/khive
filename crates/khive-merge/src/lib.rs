// Copyright 2026 khive contributors. Licensed under Apache-2.0.
//
//! KG three-way merge — implements `MergeEngine` from `khive-vcs` (ADR-043).
//!
//! # Crate layout
//!
//! - [`lca`] — `find_lca()`: snapshot ancestry walk
//! - [`diff_local`] — minimal entity+edge diff for the merge use case
//! - [`entity`] — entity categorization and field-level conflict analysis
//! - [`edge`] — edge categorization and dangling-edge validation
//! - [`strategy`] — last-write-wins shortcuts (`Ours`/`Theirs`)
//! - [`merge`] — `three_way_merge()` top-level function + `ThreeWayMergeEngine`

pub mod diff_local;
pub mod edge;
pub mod entity;
pub mod lca;
pub mod merge;
pub mod strategy;

pub use merge::ThreeWayMergeEngine;
