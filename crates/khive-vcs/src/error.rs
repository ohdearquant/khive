// Copyright 2026 khive contributors. Licensed under Apache-2.0.
//
//! Error types for the VCS layer.

use thiserror::Error;

use crate::types::SnapshotId;

#[derive(Debug, Error)]
pub enum VcsError {
    /// A snapshot with this ID already exists in the database.
    /// This should only occur on SHA-256 hash collision (computationally infeasible)
    /// or if `commit()` is called twice with identical namespace state.
    #[error("snapshot already exists: {0}")]
    SnapshotAlreadyExists(SnapshotId),

    /// The requested snapshot archive is not in the local database.
    /// Callers must `pull` from a remote to fetch it.
    #[error("snapshot not found: {0}")]
    SnapshotNotFound(SnapshotId),

    /// No branch with this name in the namespace.
    #[error("branch not found: {namespace}/{name}")]
    BranchNotFound { namespace: String, name: String },

    /// The remote branch HEAD is not an ancestor of the local HEAD.
    /// Caller must `pull`, merge, commit, then push.
    #[error("non-fast-forward: local={local_head}, remote={remote_head}")]
    NonFastForward {
        local_head: SnapshotId,
        remote_head: SnapshotId,
    },

    /// The remote khive-sync server could not be reached.
    #[error("remote unreachable: {url} — {cause}")]
    RemoteUnreachable { url: String, cause: String },

    /// The remote rejected the request due to authentication failure.
    #[error("authentication failed for remote: {url}")]
    AuthFailed { url: String },

    /// The archive stored at the remote has a different hash than expected.
    /// Indicates corruption or tampering.
    #[error("hash mismatch: expected {expected}, actual {actual}")]
    HashMismatch {
        expected: SnapshotId,
        actual: SnapshotId,
    },

    /// The remote has diverged from local history; a merge is required.
    #[error("merge required: remote history has diverged from local")]
    MergeRequired,

    /// `checkout` was blocked because there are uncommitted changes.
    /// Pass `force: true` to discard them.
    #[error("uncommitted changes: {count} entities/edges modified since last commit")]
    UncommittedChanges { count: usize },

    /// `merge_branch` was called but no `MergeEngine` has been registered.
    /// Ships as the default until `khive-merge` is linked.
    #[error("merge not implemented: link khive-merge to enable three-way merge")]
    MergeNotImplemented,

    /// A `SnapshotId` string failed validation.
    #[error("invalid snapshot id: {0}")]
    InvalidSnapshotId(String),

    /// An underlying storage operation failed.
    #[error("storage: {0}")]
    Storage(String),

    /// JSON serialization or deserialization failed.
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),

    /// An I/O operation failed (file system, network).
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    /// An unexpected internal error.
    #[error("internal: {0}")]
    Internal(String),
}

impl From<khive_runtime::error::RuntimeError> for VcsError {
    fn from(e: khive_runtime::error::RuntimeError) -> Self {
        VcsError::Storage(e.to_string())
    }
}
