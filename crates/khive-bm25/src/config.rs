//! BM25 configuration types.
//!
//! See ADR-003 for recommended parameter values.

use serde::{Deserialize, Serialize};

/// BM25 configuration parameters.
///
/// Default values (k1=1.2, b=0.75) from ADR-003 work well for most use cases.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bm25Config {
    /// Term saturation parameter.
    ///
    /// Higher values = diminishing returns for repeated terms.
    /// Range: typically 1.2-2.0
    /// Default: 1.2
    pub k1: f64,

    /// Length normalization parameter.
    ///
    /// - 0 = no length normalization (favor longer docs)
    /// - 1 = full length normalization (favor shorter docs)
    ///
    /// Range: 0.0-1.0, Default: 0.75
    pub b: f64,

    /// Maximum memory budget in bytes for the index.
    /// If None, no memory limit is enforced (default).
    /// If Some(limit), `index_document()` calls that would exceed the budget
    /// are rejected with `RetrievalError::BudgetExceeded`. Re-indexing an
    /// existing document bypasses the budget check.
    #[serde(default)]
    pub memory_budget: Option<usize>,
}

impl Default for Bm25Config {
    fn default() -> Self {
        Self {
            k1: 1.2,
            b: 0.75,
            memory_budget: None,
        }
    }
}

impl Bm25Config {
    /// Create a new BM25 configuration.
    pub fn new(k1: f64, b: f64) -> Self {
        Self {
            k1,
            b,
            memory_budget: None,
        }
    }

    /// Set memory budget in bytes.
    ///
    /// When set, `index_document()` calls that would cause the estimated
    /// memory usage to exceed this limit are rejected with `BudgetExceeded`.
    /// Re-indexing an existing document bypasses the budget check.
    #[must_use]
    pub fn with_memory_budget(mut self, budget: usize) -> Self {
        self.memory_budget = Some(budget);
        self
    }

    /// Validate configuration parameters.
    pub fn validate(&self) -> Result<(), &'static str> {
        if !self.k1.is_finite() || self.k1 < 0.0 {
            return Err("k1 must be finite and non-negative");
        }
        if !self.b.is_finite() || !(0.0..=1.0).contains(&self.b) {
            return Err("b must be finite and in range [0.0, 1.0]");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_default() {
        let config = Bm25Config::default();
        assert!((config.k1 - 1.2).abs() < f64::EPSILON);
        assert!((config.b - 0.75).abs() < f64::EPSILON);
    }

    #[test]
    fn test_config_validation() {
        assert!(Bm25Config::new(1.2, 0.75).validate().is_ok());
        assert!(Bm25Config::new(-0.1, 0.75).validate().is_err());
        assert!(Bm25Config::new(1.2, -0.1).validate().is_err());
        assert!(Bm25Config::new(1.2, 1.5).validate().is_err());
    }

    #[test]
    fn test_config_nan_rejected() {
        assert!(
            Bm25Config::new(f64::NAN, 0.75).validate().is_err(),
            "NaN k1 must be rejected"
        );
        assert!(
            Bm25Config::new(1.2, f64::NAN).validate().is_err(),
            "NaN b must be rejected"
        );
    }

    #[test]
    fn test_config_inf_rejected() {
        assert!(
            Bm25Config::new(f64::INFINITY, 0.75).validate().is_err(),
            "Inf k1 must be rejected"
        );
        assert!(
            Bm25Config::new(f64::NEG_INFINITY, 0.75).validate().is_err(),
            "NegInf k1 must be rejected"
        );
        assert!(
            Bm25Config::new(1.2, f64::INFINITY).validate().is_err(),
            "Inf b must be rejected"
        );
    }

    #[test]
    fn test_config_custom() {
        let config = Bm25Config::new(2.0, 0.5);
        assert!((config.k1 - 2.0).abs() < f64::EPSILON);
        assert!((config.b - 0.5).abs() < f64::EPSILON);
    }
}
