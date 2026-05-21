//! Fold outcome type

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::FoldContext;

/// Outcome of a fold operation.
///
/// Contains the derived state along with metadata about the fold execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FoldOutcome<S> {
    /// The derived state
    pub state: S,

    /// Number of entries processed
    pub entries_processed: usize,

    /// When the fold started
    pub started_at: DateTime<Utc>,

    /// When the fold completed
    pub completed_at: DateTime<Utc>,

    /// Context used for the fold
    pub context: FoldContext,

    /// Optional metadata
    #[serde(default)]
    pub metadata: serde_json::Value,
}

impl<S> FoldOutcome<S> {
    /// Create a new fold result with identical start and completion timestamps.
    pub fn new(state: S, entries_processed: usize, context: FoldContext) -> Self {
        let now = Utc::now();
        Self {
            state,
            entries_processed,
            started_at: now,
            completed_at: now,
            context,
            metadata: serde_json::Value::Null,
        }
    }

    /// Create with timing information.
    pub fn with_timing(
        state: S,
        entries_processed: usize,
        context: FoldContext,
        started_at: DateTime<Utc>,
    ) -> Self {
        Self {
            state,
            entries_processed,
            started_at,
            completed_at: Utc::now(),
            context,
            metadata: serde_json::Value::Null,
        }
    }

    /// Create with timing information derived from a monotonic elapsed duration.
    ///
    /// Avoids a second `Utc::now()` call by computing `completed_at` from
    /// `started_at + elapsed`.
    pub fn with_elapsed(
        state: S,
        entries_processed: usize,
        context: FoldContext,
        started_at: DateTime<Utc>,
        elapsed: std::time::Duration,
    ) -> Self {
        let completed_at = started_at
            + chrono::Duration::from_std(elapsed).unwrap_or_else(|_| chrono::Duration::zero());

        Self {
            state,
            entries_processed,
            started_at,
            completed_at,
            context,
            metadata: serde_json::Value::Null,
        }
    }

    /// Set metadata.
    pub fn with_metadata(mut self, metadata: serde_json::Value) -> Self {
        self.metadata = metadata;
        self
    }

    /// Get duration of the fold.
    pub fn duration(&self) -> chrono::Duration {
        self.completed_at - self.started_at
    }

    /// Map the state to a different type.
    pub fn map<T, F: FnOnce(S) -> T>(self, f: F) -> FoldOutcome<T> {
        FoldOutcome {
            state: f(self.state),
            entries_processed: self.entries_processed,
            started_at: self.started_at,
            completed_at: self.completed_at,
            context: self.context,
            metadata: self.metadata,
        }
    }
}

impl<S: Default> Default for FoldOutcome<S> {
    fn default() -> Self {
        Self::new(S::default(), 0, FoldContext::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fold_outcome_creation() {
        let result = FoldOutcome::new(42, 10, FoldContext::new());
        assert_eq!(result.state, 42);
        assert_eq!(result.entries_processed, 10);
    }

    #[test]
    fn test_fold_outcome_map() {
        let result = FoldOutcome::new(42, 10, FoldContext::new());
        let mapped = result.map(|x| x.to_string());
        assert_eq!(mapped.state, "42");
        assert_eq!(mapped.entries_processed, 10);
    }

    #[test]
    fn test_fold_outcome_with_elapsed() {
        let started_at = Utc::now();
        let outcome = FoldOutcome::with_elapsed(
            7usize,
            2,
            FoldContext::new(),
            started_at,
            std::time::Duration::from_millis(5),
        );
        assert!(outcome.completed_at >= outcome.started_at);
    }

    #[test]
    fn test_fold_outcome_with_elapsed_exact_arithmetic() {
        let started_at = Utc::now();
        let elapsed = std::time::Duration::from_millis(123);
        let outcome =
            FoldOutcome::with_elapsed("state", 5, FoldContext::new(), started_at, elapsed);
        let expected_completed = started_at + chrono::Duration::from_std(elapsed).unwrap();
        assert_eq!(outcome.completed_at, expected_completed);
        assert_eq!(outcome.started_at, started_at);
    }

    #[test]
    fn test_fold_outcome_with_elapsed_zero_duration() {
        let started_at = Utc::now();
        let outcome = FoldOutcome::with_elapsed(
            0u32,
            0,
            FoldContext::new(),
            started_at,
            std::time::Duration::ZERO,
        );
        assert_eq!(outcome.completed_at, outcome.started_at);
    }

    #[test]
    fn test_fold_outcome_with_elapsed_large_duration() {
        let started_at = Utc::now();
        let elapsed = std::time::Duration::from_secs(3600);
        let outcome =
            FoldOutcome::with_elapsed(42u64, 100, FoldContext::new(), started_at, elapsed);
        let expected = started_at + chrono::Duration::from_std(elapsed).unwrap();
        assert_eq!(outcome.completed_at, expected);
        assert_eq!(outcome.state, 42u64);
    }
}
