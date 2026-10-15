//! Fold outcome type

/// Outcome of a fold operation.
///
/// Deterministic: contains only derived state and entry count. No wall-clock timing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoldOutcome<S> {
    /// The derived state.
    pub state: S,

    /// Number of entries processed.
    pub entries_processed: usize,
}

impl<S> FoldOutcome<S> {
    /// Create a new fold outcome.
    pub fn new(state: S, entries_processed: usize) -> Self {
        Self {
            state,
            entries_processed,
        }
    }

    /// Map the state to a different type.
    pub fn map<T, F: FnOnce(S) -> T>(self, f: F) -> FoldOutcome<T> {
        FoldOutcome::new(f(self.state), self.entries_processed)
    }
}

impl<S: Default> Default for FoldOutcome<S> {
    fn default() -> Self {
        Self::new(S::default(), 0)
    }
}

#[cfg(test)]
#[path = "result_tests.rs"]
mod tests;
