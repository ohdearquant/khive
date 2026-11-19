//! BM25 configuration types. Invariants enforced at deserialization via `#[serde(try_from)]`.

use serde::{Deserialize, Serialize};

/// Raw wire format for [`Bm25Config`], used by `TryFrom` validation.
#[derive(Deserialize)]
struct RawBm25Config {
    k1: f64,
    b: f64,
    #[serde(default)]
    memory_budget: Option<usize>,
}

impl TryFrom<RawBm25Config> for Bm25Config {
    type Error = String;

    fn try_from(raw: RawBm25Config) -> Result<Self, Self::Error> {
        let config = Bm25Config {
            k1: raw.k1,
            b: raw.b,
            memory_budget: raw.memory_budget,
        };
        config.validate().map_err(|e| e.to_string())?;
        Ok(config)
    }
}

/// BM25 parameters with validated `k1 >= 0` and `b` in `[0, 1]`; defaults are 1.2 and 0.75.
///
/// See `crates/khive-bm25/docs/api/index-lifecycle.md`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(try_from = "RawBm25Config")]
pub struct Bm25Config {
    /// Term saturation parameter (default 1.2, typically 1.2-2.0).
    pub k1: f64,
    /// Length normalization parameter (default 0.75, range 0.0-1.0).
    pub b: f64,
    /// Optional memory budget in bytes; rejects new docs that would exceed it.
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

    /// Create a validated BM25 configuration, returning an error if parameters are invalid.
    pub fn try_new(k1: f64, b: f64) -> Result<Self, &'static str> {
        let config = Self::new(k1, b);
        config.validate()?;
        Ok(config)
    }

    /// Set memory budget in bytes; new docs that would exceed it are rejected.
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
#[path = "config_tests.rs"]
mod tests;
