// Copyright 2026 khive contributors. Licensed under Apache-2.0.
//
//! `MergeEngine` trait — pluggable merge implementation (ADR-042 §3).
//!
//! `khive-vcs` ships with `NoOpMergeEngine` which returns `VcsError::MergeNotImplemented`
//! for all calls. `khive-merge` (v0.5, ADR-043) provides `ThreeWayMergeEngine` which
//! implements the full algorithm.

use khive_runtime::portability::KgArchive;

use crate::error::VcsError;

/// Strategy passed to `merge_branch`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeStrategy {
    /// Compute diffs and auto-merge where safe; report conflicts otherwise.
    #[default]
    Auto,
    /// Prefer `ours` on all conflicts. Always produces a `Clean` result.
    Ours,
    /// Prefer `theirs` on all conflicts. Always produces a `Clean` result.
    Theirs,
}

/// Output of a three-way merge operation.
#[derive(Debug)]
pub enum MergeResult {
    /// All changes merged without conflict. `merged` is the resulting archive.
    Clean { merged: KgArchive },
    /// One or more conflicts detected. No merged archive is produced.
    /// The caller must resolve conflicts and call again with `force=true`.
    Conflicts { conflicts: Vec<MergeConflict> },
}

/// A structured conflict that prevented auto-merge.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MergeConflict {
    /// Two different names for the same entity.
    NameConflict {
        entity_id: uuid::Uuid,
        ours: String,
        theirs: String,
    },
    /// Incompatible `kind` values for the same entity.
    KindConflict {
        entity_id: uuid::Uuid,
        ours: String,
        theirs: String,
    },
    /// Same property key set to different values in ours and theirs.
    PropertyMismatch {
        entity_id: uuid::Uuid,
        key: String,
        ours: serde_json::Value,
        theirs: serde_json::Value,
    },
    /// One branch modified an entity; the other deleted it.
    ModifyDelete {
        entity_id: uuid::Uuid,
        modified_in: BranchSide,
        deleted_in: BranchSide,
    },
    /// One branch modified an edge; the other deleted it.
    EdgeModifyDelete {
        source_id: uuid::Uuid,
        target_id: uuid::Uuid,
        relation: String,
        modified_in: BranchSide,
        deleted_in: BranchSide,
    },
    /// An edge in the merged set references a deleted endpoint.
    DanglingEdge {
        source_id: uuid::Uuid,
        target_id: uuid::Uuid,
        relation: String,
        missing_endpoint: uuid::Uuid,
    },
}

/// Which branch side an attribute comes from in a conflict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BranchSide {
    Ours,
    Theirs,
}

/// Pluggable merge engine.
///
/// `khive-vcs` calls this trait from `merge_branch`. The default implementation
/// (`NoOpMergeEngine`) returns `VcsError::MergeNotImplemented`. Register a
/// `ThreeWayMergeEngine` from `khive-merge` at startup to enable full merge.
pub trait MergeEngine: Send + Sync {
    fn merge_branch(
        &self,
        base: &KgArchive,
        ours: &KgArchive,
        theirs: &KgArchive,
        strategy: MergeStrategy,
    ) -> Result<MergeResult, VcsError>;
}

/// Default no-op engine shipped with `khive-vcs`.
///
/// Returns `VcsError::MergeNotImplemented` for all calls.
/// Replace with `ThreeWayMergeEngine` from `khive-merge` in production.
pub struct NoOpMergeEngine;

impl MergeEngine for NoOpMergeEngine {
    fn merge_branch(
        &self,
        _base: &KgArchive,
        _ours: &KgArchive,
        _theirs: &KgArchive,
        _strategy: MergeStrategy,
    ) -> Result<MergeResult, VcsError> {
        Err(VcsError::MergeNotImplemented)
    }
}
