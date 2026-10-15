//! Objective error types

use thiserror::Error;

/// Objective function error type
#[derive(Debug, Error)]
pub enum ObjectiveError {
    /// No candidates to select from
    #[error("No candidates available")]
    NoCandidates,

    /// No candidate met the selection criteria
    #[error("No candidate met criteria: {0}")]
    NoMatch(String),

    /// Invalid objective configuration
    #[error("Invalid configuration: {0}")]
    InvalidConfig(String),

    /// Objective not found in registry
    #[error("Objective not found: {0}")]
    NotFound(String),

    /// Scoring error
    #[error("Scoring error: {0}")]
    Scoring(String),

    /// Fold error
    #[error("Fold error: {0}")]
    Fold(#[from] crate::FoldError),
}

/// Objective result type
pub type ObjectiveResult<T> = Result<T, ObjectiveError>;

#[cfg(test)]
#[path = "error_tests.rs"]
mod tests;
