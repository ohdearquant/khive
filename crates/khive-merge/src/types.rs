//! Public strategies, results, conflicts, errors, and engine contract.
//!
//! See `crates/khive-merge/docs/api/conflict-and-error-taxonomy.md`.

use khive_runtime::portability::KgArchive;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Selects semantic conflict detection or a last-write-wins branch preference.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SnapshotMergeStrategy {
    /// Three-way merge with conflict detection.
    Auto,
    /// Last-write-wins: ours fields prevail on conflict.
    Ours,
    /// Last-write-wins: theirs fields prevail on conflict.
    Theirs,
}

/// A clean owned archive or typed conflicts requiring resolution.
#[derive(Clone, Debug)]
pub enum MergeResult {
    /// The merge completed without conflicts.
    Clean { merged: KgArchive },
    /// The merge detected one or more conflicts requiring manual resolution.
    Conflicts { conflicts: Vec<MergeConflict> },
}

/// A well-formed but unresolved semantic disagreement between branches.
///
/// See `crates/khive-merge/docs/api/conflict-and-error-taxonomy.md`.
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

/// Strategy-neutral interface for snapshot merge engines.
pub trait MergeEngine {
    /// Merges `ours` and `theirs` against their common `base`.
    ///
    /// # Errors
    ///
    /// Returns [`MergeError`] when the inputs violate archive invariants or
    /// the implementation cannot reconstruct a valid result.
    fn merge_branch(
        &self,
        base: &KgArchive,
        ours: &KgArchive,
        theirs: &KgArchive,
        strategy: SnapshotMergeStrategy,
    ) -> Result<MergeResult, MergeError>;
}

/// Invalid-input and internal failures that prevent merge evaluation.
#[derive(Debug, thiserror::Error)]
pub enum MergeError {
    /// The three archives do not share a namespace.
    #[error("namespace mismatch: base={base}, ours={ours}, theirs={theirs}")]
    NamespaceMismatch {
        base: String,
        ours: String,
        theirs: String,
    },

    /// An archive contains a NaN or infinite edge weight.
    #[error("invalid edge weight: {0}")]
    InvalidEdgeWeight(String),

    /// An archive repeats an entity UUID.
    #[error("duplicate entity IDs in archive: {entity_id}")]
    DuplicateEntityId { entity_id: Uuid },

    /// An archive repeats a semantic `(source, target, relation)` edge key.
    #[error("duplicate edge key in archive: ({edge_source}, {edge_target}, {edge_relation})")]
    DuplicateEdgeKey {
        edge_source: Uuid,
        edge_target: Uuid,
        edge_relation: String,
    },

    /// Another invariant failed, such as a duplicate edge UUID or invalid relation.
    #[error("internal: {0}")]
    Internal(String),
}
