//! Selection result from objective functions

use serde::{Deserialize, Serialize};

/// A selection result from an objective function
#[derive(Debug, Clone, Serialize, Deserialize)]
#[must_use = "selections should be used after creation"]
pub struct Selection<T> {
    /// The selected item
    pub item: T,
    /// Score of the selection
    pub score: f64,
    /// Index in the original candidates
    pub index: usize,
    /// Number of candidates considered
    pub considered: usize,
    /// Number of candidates that passed threshold
    pub passed: usize,
    /// Reason for selection
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl<T> Selection<T> {
    /// Create a new selection
    pub fn new(item: T, score: f64, index: usize) -> Self {
        Self {
            item,
            score,
            index,
            considered: 1,
            passed: 1,
            reason: None,
        }
    }

    /// Set the considered count
    pub fn with_considered(mut self, n: usize) -> Self {
        self.considered = n;
        self
    }

    /// Set the passed count
    pub fn with_passed(mut self, n: usize) -> Self {
        self.passed = n;
        self
    }

    /// Set the reason
    pub fn with_reason(mut self, reason: impl Into<String>) -> Self {
        self.reason = Some(reason.into());
        self
    }

    /// Map the selected value
    pub fn map<U, F: FnOnce(T) -> U>(self, f: F) -> Selection<U> {
        Selection {
            item: f(self.item),
            score: self.score,
            index: self.index,
            considered: self.considered,
            passed: self.passed,
            reason: self.reason,
        }
    }
}
