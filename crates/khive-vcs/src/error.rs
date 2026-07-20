//! Error types for the VCS layer.
//!
//! Remote-server and custom-push/pull error variants (`RemoteUnreachable`,
//! `AuthFailed`, `NonFastForward`, `MergeRequired`) were removed: git is the
//! remote protocol; there is no custom `khive-sync` server.
//! `MergeNotImplemented` was removed because the custom merge engine is
//! superseded for v1.

use thiserror::Error;

use crate::types::SnapshotId;

/// Errors that can occur in the khive VCS layer (snapshot hashing, NDJSON sync, remote fetch).
#[derive(Debug, Error)]
pub enum VcsError {
    /// The archive stored at the remote has a different hash than expected.
    /// Indicates corruption or tampering.
    #[error("hash mismatch: expected {expected}, actual {actual}")]
    HashMismatch {
        expected: SnapshotId,
        actual: SnapshotId,
    },

    /// `checkout` was blocked because there are uncommitted changes.
    /// Pass `force: true` to discard them.
    #[error("uncommitted changes: {count} entities/edges modified since last commit")]
    UncommittedChanges { count: usize },

    /// A `SnapshotId` string failed validation.
    #[error("invalid snapshot id: {0}")]
    InvalidSnapshotId(String),

    /// A branch name failed validation (must match `^[a-zA-Z0-9_-]{1,64}$`).
    #[error("invalid branch name: {0:?}")]
    InvalidBranchName(String),

    /// A remote name failed validation (must be a single path segment
    /// matching `[A-Za-z0-9._-]+`, and not `.` or `..`). Rejected before any
    /// filesystem path is built from it, to prevent path traversal into
    /// `.khive/kg/remotes/`.
    #[error("invalid remote name {0:?}: must be one path segment [A-Za-z0-9._-]+ and not . or ..")]
    InvalidRemoteName(String),

    /// An underlying storage operation failed.
    #[error("storage: {0}")]
    Storage(String),

    /// JSON serialization or deserialization failed.
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),

    /// An I/O operation failed (file system).
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
