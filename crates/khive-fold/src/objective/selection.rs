//! Selection result from objective functions

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// A selection result from an objective function
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[must_use = "selections should be used after creation"]
pub struct Selection<T> {
    /// The selected item
    pub item: T,
    /// Score of the selection
    pub score: f64,
    /// Precision (inverse variance) of the score estimate. Default 1.0 (fully trusted).
    ///
    /// The effective ranking score is `score * precision`. When precision is 1.0 (the
    /// default), ranking is identical to raw score ordering.
    #[cfg_attr(feature = "serde", serde(default = "default_precision"))]
    pub precision: f64,
    /// Index in the original candidates
    pub index: usize,
    /// Number of candidates considered
    pub considered: usize,
    /// Number of candidates that passed threshold
    pub passed: usize,
    /// Reason for selection
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub reason: Option<String>,
}

fn default_precision() -> f64 {
    1.0
}

impl<T> Selection<T> {
    /// Create a new selection
    pub fn new(item: T, score: f64, index: usize) -> Self {
        Self {
            item,
            score,
            precision: 1.0,
            index,
            considered: 1,
            passed: 1,
            reason: None,
        }
    }

    /// Set the precision (reliability estimate for the score).
    ///
    /// Values in (0, 1] are typical; 1.0 means fully trusted (the default).
    pub fn with_precision(mut self, precision: f64) -> Self {
        self.precision = precision;
        self
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
            precision: self.precision,
            index: self.index,
            considered: self.considered,
            passed: self.passed,
            reason: self.reason,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn precision_default_is_one() {
        let sel = Selection::new(42i32, 0.8, 0);
        assert_eq!(sel.precision, 1.0);
    }

    #[test]
    fn with_precision_sets_field() {
        let sel = Selection::new(42i32, 0.8, 0).with_precision(0.5);
        assert_eq!(sel.precision, 0.5);
    }

    #[test]
    fn map_propagates_precision() {
        let sel = Selection::new(42i32, 0.8, 0).with_precision(0.75);
        let mapped = sel.map(|v| v.to_string());
        assert_eq!(mapped.precision, 0.75);
        assert_eq!(mapped.item, "42");
        assert_eq!(mapped.score, 0.8);
    }

    #[test]
    fn map_preserves_all_stats() {
        let sel = Selection::new(1i32, 0.5, 2)
            .with_precision(0.6)
            .with_considered(10)
            .with_passed(7)
            .with_reason("test");
        let mapped = sel.map(|v| v * 2);
        assert_eq!(mapped.item, 2);
        assert_eq!(mapped.score, 0.5);
        assert_eq!(mapped.precision, 0.6);
        assert_eq!(mapped.index, 2);
        assert_eq!(mapped.considered, 10);
        assert_eq!(mapped.passed, 7);
        assert_eq!(mapped.reason.as_deref(), Some("test"));
    }
}
