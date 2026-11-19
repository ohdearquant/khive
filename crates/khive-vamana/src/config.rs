//! Configuration for the Vamana ANN index.

use crate::error::{Result, VamanaError};

/// Algorithm parameters for the Vamana index build and search phases.
#[derive(Debug, Clone, PartialEq)]
pub struct VamanaConfig {
    /// Dimensionality of vectors; must be > 0.
    pub dimensions: usize,
    /// Maximum out-degree of any graph node; must be > 0.
    pub max_degree: usize,
    /// Greedy-search candidate list capacity; must be >= `max_degree`.
    pub search_list_size: usize,
    /// Robust-prune alpha; must be finite and >= 1.0.
    pub alpha: f64,
}

impl Default for VamanaConfig {
    fn default() -> Self {
        Self {
            dimensions: 384,
            max_degree: 64,
            search_list_size: 128,
            alpha: 1.2,
        }
    }
}

impl VamanaConfig {
    /// Validate all configuration parameters, returning an error on the first violation.
    pub fn validate(&self) -> Result<()> {
        if self.dimensions == 0 {
            return Err(VamanaError::invalid_config("dimensions must be > 0".into()));
        }
        if self.max_degree == 0 {
            return Err(VamanaError::invalid_config("max_degree must be > 0".into()));
        }
        if self.search_list_size == 0 {
            return Err(VamanaError::invalid_config(
                "search_list_size must be > 0".into(),
            ));
        }
        if self.search_list_size < self.max_degree {
            return Err(VamanaError::invalid_config(format!(
                "search_list_size ({}) must be >= max_degree ({})",
                self.search_list_size, self.max_degree
            )));
        }
        if !self.alpha.is_finite() {
            return Err(VamanaError::invalid_config("alpha must be finite".into()));
        }
        if self.alpha < 1.0 {
            return Err(VamanaError::invalid_config("alpha must be >= 1.0".into()));
        }
        Ok(())
    }

    /// Construct and validate a config with the given `dimensions`, returning an error if invalid.
    pub fn try_with_dimensions(dimensions: usize) -> Result<Self> {
        let cfg = Self {
            dimensions,
            ..Self::default()
        };
        cfg.validate()?;
        Ok(cfg)
    }

    /// Construct a config with the given dimensions. Panics on zero dimensions.
    pub fn with_dimensions(dimensions: usize) -> Self {
        assert!(dimensions > 0, "dimensions must be > 0");
        Self {
            dimensions,
            ..Self::default()
        }
    }

    /// Return a copy with `dimensions` replaced; does not re-validate.
    #[must_use]
    pub fn set_dimensions(self, dimensions: usize) -> Self {
        Self { dimensions, ..self }
    }

    /// Return a copy with `max_degree` replaced; does not re-validate.
    #[must_use]
    pub fn with_max_degree(self, max_degree: usize) -> Self {
        Self { max_degree, ..self }
    }

    /// Return a copy with `search_list_size` replaced; does not re-validate.
    #[must_use]
    pub fn with_search_list_size(self, search_list_size: usize) -> Self {
        Self {
            search_list_size,
            ..self
        }
    }

    /// Return a copy with `alpha` replaced; does not re-validate.
    #[must_use]
    pub fn with_alpha(self, alpha: f64) -> Self {
        Self { alpha, ..self }
    }
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;
