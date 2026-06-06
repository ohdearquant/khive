// Copyright 2026 khive contributors. Licensed under Apache-2.0.
//
//! Core merge types: conflict taxonomy, strategy enum, and `MergeEngine` trait.
//!
//! These types are defined here (not in `khive-vcs`) because the VCS crate
//! currently ships only the git-native v1 surface. The three-way merge conflict
//! taxonomy is forward-deployed v2 infrastructure that will be promoted to a
//! shared crate when the VCS integration layer is extended.

use khive_runtime::portability::KgArchive;
use khive_vcs::VcsError;
use uuid::Uuid;

/// Which branch side an operation occurred on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BranchSide {
    /// The local ("ours") branch.
    Ours,
    /// The remote ("theirs") branch.
    Theirs,
}

/// Strategy for resolving a three-way merge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeStrategy {
    /// Run the full conflict-detection pass; return `MergeResult::Conflicts` if any found.
    Auto,
    /// Last-write-wins shortcut: ours wins on every field.
    Ours,
    /// Last-write-wins shortcut: theirs wins on every field.
    Theirs,
}

/// A detected conflict between two branches.
#[derive(Debug, Clone)]
pub enum MergeConflict {
    /// Both branches modified the same entity's name to different values.
    NameConflict {
        entity_id: Uuid,
        ours: String,
        theirs: String,
    },
    /// Both branches changed the same entity's kind to different values.
    KindConflict {
        entity_id: Uuid,
        ours: String,
        theirs: String,
    },
    /// One branch deleted an entity that the other branch modified.
    ModifyDelete {
        entity_id: Uuid,
        modified_in: BranchSide,
        deleted_in: BranchSide,
    },
    /// Both branches modified the same property key to different values.
    PropertyMismatch {
        entity_id: Uuid,
        key: String,
        ours: serde_json::Value,
        theirs: serde_json::Value,
    },
    /// One branch deleted an edge that the other branch modified (weight change).
    EdgeModifyDelete {
        source_id: Uuid,
        target_id: Uuid,
        relation: String,
        modified_in: BranchSide,
        deleted_in: BranchSide,
    },
    /// A merged edge references an endpoint UUID not present in the merged entity set.
    DanglingEdge {
        source_id: Uuid,
        target_id: Uuid,
        relation: String,
        missing_endpoint: Uuid,
    },
}

/// Outcome of a three-way merge operation.
#[derive(Debug)]
pub enum MergeResult {
    /// Merge completed with no unresolvable conflicts.
    Clean {
        /// The resulting merged archive, ready to import.
        merged: KgArchive,
    },
    /// Merge detected one or more conflicts that require manual resolution.
    Conflicts {
        /// All detected conflicts.
        conflicts: Vec<MergeConflict>,
    },
}

/// Trait for a pluggable merge engine.
///
/// The default implementation is `ThreeWayMergeEngine` in the `merge` module.
pub trait MergeEngine {
    /// Merge `ours` and `theirs` branches, using `base` as the common ancestor.
    fn merge_branch(
        &self,
        base: &KgArchive,
        ours: &KgArchive,
        theirs: &KgArchive,
        strategy: MergeStrategy,
    ) -> Result<MergeResult, VcsError>;
}
