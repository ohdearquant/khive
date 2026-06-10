// Copyright 2026 Haiyang Li. Licensed under Apache-2.0.
//
//! Merge-engine types for the three-way merge algorithm.

use khive_runtime::portability::KgArchive;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Snapshot merge strategy selector (ADR-010).
///
/// Renamed from `MergeStrategy` to avoid collision with `ContentMergeStrategy` in
/// the note-curation layer (ADR-014).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SnapshotMergeStrategy {
    /// Three-way merge with conflict detection.
    Auto,
    /// Last-write-wins: ours fields prevail on conflict.
    Ours,
    /// Last-write-wins: theirs fields prevail on conflict.
    Theirs,
}

/// Result of a three-way merge operation.
#[derive(Clone, Debug)]
pub enum MergeResult {
    /// The merge completed without conflicts.
    Clean { merged: KgArchive },
    /// The merge detected one or more conflicts requiring manual resolution.
    Conflicts { conflicts: Vec<MergeConflict> },
}

/// A conflict detected during three-way merge.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum MergeConflict {
    /// Entity name differs between branches.
    NameConflict {
        entity_id: Uuid,
        ours: String,
        theirs: String,
    },
    /// Entity kind differs between branches.
    KindConflict {
        entity_id: Uuid,
        ours: String,
        theirs: String,
    },
    /// Entity property value differs for the same key.
    PropertyMismatch {
        entity_id: Uuid,
        key: String,
        ours: serde_json::Value,
        theirs: serde_json::Value,
    },
    /// One branch modified an entity, the other deleted it.
    ModifyDelete {
        entity_id: Uuid,
        modified_in: BranchSide,
        deleted_in: BranchSide,
    },
    /// Both branches added the same UUID with different content.
    DuplicateAddition {
        entity_id: Uuid,
        differing_fields: Vec<String>,
    },
    /// One branch modified an edge, the other deleted it.
    EdgeModifyDelete {
        source_id: Uuid,
        target_id: Uuid,
        relation: String,
        modified_in: BranchSide,
        deleted_in: BranchSide,
    },
    /// An edge references a missing entity (dangling reference).
    DanglingEdge {
        source_id: Uuid,
        target_id: Uuid,
        relation: String,
        missing_endpoint: Uuid,
    },
}

/// Identifies which side of the merge a change originates from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BranchSide {
    /// The local branch being merged into the common base.
    Ours,
    /// The remote branch being merged in.
    Theirs,
}

/// Trait for merge engine implementations.
///
/// Register an implementation of this trait in `khive-vcs` at startup to
/// replace the default no-op merge engine.
pub trait MergeEngine {
    /// Run a three-way merge of `ours` and `theirs` against their common `base`.
    fn merge_branch(
        &self,
        base: &KgArchive,
        ours: &KgArchive,
        theirs: &KgArchive,
        strategy: SnapshotMergeStrategy,
    ) -> Result<MergeResult, MergeError>;
}

/// Merge-specific error type.
#[derive(Debug, thiserror::Error)]
pub enum MergeError {
    #[error("namespace mismatch: base={base}, ours={ours}, theirs={theirs}")]
    NamespaceMismatch {
        base: String,
        ours: String,
        theirs: String,
    },

    #[error("invalid edge weight: {0}")]
    InvalidEdgeWeight(String),

    #[error("duplicate entity IDs in archive: {entity_id}")]
    DuplicateEntityId { entity_id: Uuid },

    #[error("duplicate edge key in archive: ({edge_source}, {edge_target}, {edge_relation})")]
    DuplicateEdgeKey {
        edge_source: Uuid,
        edge_target: Uuid,
        edge_relation: String,
    },

    #[error("internal: {0}")]
    Internal(String),
}
