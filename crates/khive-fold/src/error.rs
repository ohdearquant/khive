//! Error types for khive-fold

use thiserror::Error;
use uuid::Uuid;

/// Result type for fold operations
pub type FoldResult<T> = std::result::Result<T, FoldError>;

/// Errors that can occur in fold operations
#[derive(Error, Debug)]
pub enum FoldError {
    /// Entry not found
    #[error("Entry {0} not found")]
    EntryNotFound(Uuid),

    /// Invalid entry type for this fold
    #[error("Invalid entry type: expected {expected}, got {actual}")]
    InvalidEntryType {
        /// The expected entry type name
        expected: String,
        /// The actual entry type that was provided
        actual: String,
    },

    /// Fold context error
    #[error("Context error: {0}")]
    Context(String),

    /// Serialization error
    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    /// Storage error
    #[error("Storage error: {0}")]
    Storage(String),

    /// Internal lock poisoned (concurrent panic)
    #[error("Internal lock poisoned: {0}")]
    LockPoisoned(String),

    /// Invalid input to a cognitive primitive (Anchor, Selector).
    #[error("invalid input: {0}")]
    InvalidInput(String),

    /// Budget exhausted during selection.
    #[error("budget exhausted: needed {needed}, have {budget}")]
    BudgetExhausted {
        /// Budget needed.
        needed: usize,
        /// Budget available.
        budget: usize,
    },

    /// Anchor not found during graph traversal.
    #[error("anchor not found: {0}")]
    AnchorNotFound(String),

    /// Required component not configured.
    #[error("required component not configured: {0}")]
    ComponentMissing(String),

    /// Checkpoint integrity check failed: stored hash does not match recomputed hash.
    #[error("checkpoint integrity mismatch for '{id}': stored {stored}, computed {computed}")]
    IntegrityMismatch {
        /// Checkpoint id that failed verification.
        id: String,
        /// The hash stored in the checkpoint.
        stored: String,
        /// The hash recomputed from the loaded state.
        computed: String,
    },

    /// A checkpoint with the given id was not found.
    #[error("checkpoint not found: {0}")]
    CheckpointNotFound(String),
}

#[cfg(test)]
#[path = "error_tests.rs"]
mod tests;
