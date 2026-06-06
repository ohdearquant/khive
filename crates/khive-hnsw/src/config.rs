//! HNSW configuration types.
//!
//! See ADR-003 for recommended parameter values (ef_construction, M, max_layers).

use serde::{Deserialize, Deserializer, Serialize};

use crate::error::{Result, RetrievalError};

/// Maximum allowed level in the HNSW graph.
/// Prevents unbounded memory allocation from malformed random values.
/// For 1 billion vectors with typical ml, expected max level is ~16-18.
pub const MAX_LEVEL: usize = 64;

/// Default threshold for triggering a rebuild (10% tombstones).
/// Aligned with ADR-003: Index Management Strategy.
pub const DEFAULT_REBUILD_THRESHOLD: f64 = 0.10;

// Re-export from canonical location (foundation/types).
// Canonical variants: Cosine, Dot, L2.
// Serde aliases on canonical handle backward compat: "euclidean" -> L2, "dot_product" -> Dot.
pub use khive_types::vector::DistanceMetric;

/// HNSW configuration parameters per ADR-003.
///
/// Deserialization validates all invariants before returning. Invalid configs
/// (e.g. `dimensions=0`, non-finite `ml`, `m_max0 < m`) are rejected with a
/// descriptive error at the deserialization boundary.
#[derive(Debug, Clone, Serialize)]
pub struct HnswConfig {
    /// Maximum number of connections per node per layer (M).
    /// Higher = better recall, more memory, slower build.
    /// Recommended: 16 (small), 32 (medium), 64 (large datasets).
    pub m: usize,

    /// Maximum connections for layer 0 (typically 2*M).
    /// Layer 0 is densest, needs more connections for good recall.
    pub m_max0: usize,

    /// Size of dynamic candidate list during construction.
    /// Higher = better graph quality, slower build.
    /// Recommended: 100-500.
    pub ef_construction: usize,

    /// Normalization factor for level generation: 1/ln(M).
    /// Controls how quickly layers thin out.
    pub ml: f64,

    /// Search ef (dynamic candidate list size during search).
    /// Higher = better recall, slower search.
    /// Recommended: 50-200.
    pub ef_search: usize,

    /// Vector dimensions (must match embedding model).
    /// Default: 768 (BGE-base).
    pub dimensions: usize,

    /// Distance metric for similarity computation.
    pub metric: DistanceMetric,

    /// Threshold for automatic rebuild (tombstone ratio).
    /// When tombstones exceed this ratio, rebuild() is recommended.
    pub rebuild_threshold: f64,

    /// Seed for reproducible level generation.
    /// If None, uses OS entropy (non-deterministic).
    /// If Some(seed), uses seeded RNG for reproducible index structure.
    #[serde(default)]
    pub seed: Option<u64>,

    /// Maximum memory budget in bytes for the index.
    /// If None, no memory limit is enforced (default).
    /// If Some(limit), inserts that would exceed the budget are rejected
    /// with `RetrievalError::BudgetExceeded`. Updates to existing entries
    /// bypass the budget check.
    #[serde(default)]
    pub memory_budget: Option<usize>,
}

/// Wire-format mirror used only for deserialization.
///
/// Derives `Deserialize` without invariant checks; the `Deserialize` impl for
/// `HnswConfig` uses this to deserialize then validate in one step.
#[derive(Deserialize)]
struct HnswConfigWire {
    m: usize,
    m_max0: usize,
    ef_construction: usize,
    ml: f64,
    ef_search: usize,
    dimensions: usize,
    metric: DistanceMetric,
    rebuild_threshold: f64,
    #[serde(default)]
    seed: Option<u64>,
    #[serde(default)]
    memory_budget: Option<usize>,
}

impl<'de> Deserialize<'de> for HnswConfig {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = HnswConfigWire::deserialize(deserializer)?;
        let config = HnswConfig {
            m: wire.m,
            m_max0: wire.m_max0,
            ef_construction: wire.ef_construction,
            ml: wire.ml,
            ef_search: wire.ef_search,
            dimensions: wire.dimensions,
            metric: wire.metric,
            rebuild_threshold: wire.rebuild_threshold,
            seed: wire.seed,
            memory_budget: wire.memory_budget,
        };
        config.validate().map_err(serde::de::Error::custom)?;
        Ok(config)
    }
}

impl Default for HnswConfig {
    /// Creates default configuration per ADR-003.
    ///
    /// M=20, ef_construction=200, ef_search=80, dimensions=384.
    /// M=20 is optimal for k=10 recall at 384d (empirically measured).
    /// ef_search=80 sufficient for <100K corpus; 100 was overprovisioned.
    fn default() -> Self {
        Self {
            m: 20,
            m_max0: 40,
            ef_construction: 200,
            ml: 1.0 / (20.0_f64).ln(),
            ef_search: 80,
            dimensions: 384,
            metric: DistanceMetric::Cosine,
            rebuild_threshold: DEFAULT_REBUILD_THRESHOLD,
            seed: None,
            memory_budget: None,
        }
    }
}

impl HnswConfig {
    /// Validate configuration invariants that must hold for every index.
    pub fn validate(&self) -> Result<()> {
        if self.dimensions == 0 {
            return Err(RetrievalError::Configuration(
                "dimensions: HNSW dimensions must be greater than zero".to_string(),
            ));
        }
        if self.m == 0 {
            return Err(RetrievalError::Configuration(
                "m: must be greater than zero".to_string(),
            ));
        }
        if self.m_max0 < self.m {
            return Err(RetrievalError::Configuration(format!(
                "m_max0 ({}) must be >= m ({})",
                self.m_max0, self.m
            )));
        }
        if self.ef_search == 0 {
            return Err(RetrievalError::Configuration(
                "ef_search: must be greater than zero".to_string(),
            ));
        }
        if !self.ml.is_finite() || self.ml <= 0.0 {
            return Err(RetrievalError::Configuration(format!(
                "ml: must be a positive finite value, got {}",
                self.ml
            )));
        }
        if !self.rebuild_threshold.is_finite()
            || self.rebuild_threshold < 0.0
            || self.rebuild_threshold > 1.0
        {
            return Err(RetrievalError::Configuration(format!(
                "rebuild_threshold: must be in [0.0, 1.0], got {}",
                self.rebuild_threshold
            )));
        }
        Ok(())
    }

    /// Create config with custom dimensions, returning an error for invalid values.
    pub fn try_with_dimensions(dimensions: usize) -> Result<Self> {
        let config = Self {
            dimensions,
            ..Default::default()
        };
        config.validate()?;
        Ok(config)
    }

    /// Create config with custom dimensions, keeping ADR-003 defaults.
    ///
    /// # Panics
    /// Panics if `dimensions` is 0.
    pub fn with_dimensions(dimensions: usize) -> Self {
        Self::try_with_dimensions(dimensions).expect("HNSW dimensions must be > 0")
    }

    /// Create config for high recall (slower build, better search).
    pub fn high_recall() -> Self {
        Self {
            m: 32,
            m_max0: 64,
            ef_construction: 400,
            ef_search: 200,
            ..Default::default()
        }
    }

    /// Create config for fast build (faster build, lower recall).
    pub fn fast_build() -> Self {
        Self {
            m: 12,
            m_max0: 24,
            ef_construction: 100,
            ef_search: 50,
            ..Default::default()
        }
    }

    /// Create config optimized for memory efficiency.
    pub fn low_memory() -> Self {
        Self {
            m: 8,
            m_max0: 16,
            ef_construction: 80,
            ef_search: 40,
            ..Default::default()
        }
    }

    /// Set seed for reproducible level generation.
    ///
    /// With the same seed and insertion order, the index structure
    /// will be identical across runs.
    #[must_use]
    pub fn with_seed(mut self, seed: u64) -> Self {
        self.seed = Some(seed);
        self
    }

    /// Set memory budget in bytes.
    ///
    /// When set, inserts that would cause the estimated memory usage
    /// to exceed this limit are rejected with `BudgetExceeded`.
    /// Updates to existing entries bypass the budget check.
    #[must_use]
    pub fn with_memory_budget(mut self, budget: usize) -> Self {
        self.memory_budget = Some(budget);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_defaults() {
        let config = HnswConfig::default();
        assert_eq!(config.m, 20);
        assert_eq!(config.ef_construction, 200);
        assert_eq!(config.ef_search, 80);
        assert_eq!(config.dimensions, 384);
    }

    #[test]
    fn test_config_variants() {
        let high = HnswConfig::high_recall();
        assert_eq!(high.m, 32);
        assert_eq!(high.ef_construction, 400);

        let fast = HnswConfig::fast_build();
        assert_eq!(fast.m, 12);

        let low = HnswConfig::low_memory();
        assert_eq!(low.m, 8);
    }

    #[test]
    fn test_with_dimensions() {
        let config = HnswConfig::with_dimensions(1536);
        assert_eq!(config.dimensions, 1536);
        assert_eq!(config.m, 20); // Other defaults preserved
    }

    #[test]
    fn test_try_with_dimensions_rejects_zero() {
        let result = HnswConfig::try_with_dimensions(0);
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_rejects_m_zero() {
        let config = HnswConfig {
            m: 0,
            ..Default::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validate_rejects_m_max0_less_than_m() {
        let config = HnswConfig {
            m: 20,
            m_max0: 10,
            ..Default::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validate_rejects_ef_search_zero() {
        let config = HnswConfig {
            ef_search: 0,
            ..Default::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validate_rejects_non_finite_ml() {
        let config = HnswConfig {
            ml: f64::NAN,
            ..Default::default()
        };
        assert!(config.validate().is_err());
        let config2 = HnswConfig {
            ml: f64::INFINITY,
            ..Default::default()
        };
        assert!(config2.validate().is_err());
        let config3 = HnswConfig {
            ml: -1.0,
            ..Default::default()
        };
        assert!(config3.validate().is_err());
    }

    #[test]
    fn test_validate_rejects_invalid_rebuild_threshold() {
        let config = HnswConfig {
            rebuild_threshold: f64::NAN,
            ..Default::default()
        };
        assert!(config.validate().is_err());
        let config2 = HnswConfig {
            rebuild_threshold: -0.1,
            ..Default::default()
        };
        assert!(config2.validate().is_err());
        let config3 = HnswConfig {
            rebuild_threshold: 1.1,
            ..Default::default()
        };
        assert!(config3.validate().is_err());
    }

    #[test]
    fn test_validate_accepts_boundary_rebuild_threshold() {
        let config0 = HnswConfig {
            rebuild_threshold: 0.0,
            ..Default::default()
        };
        assert!(config0.validate().is_ok());
        let config1 = HnswConfig {
            rebuild_threshold: 1.0,
            ..Default::default()
        };
        assert!(config1.validate().is_ok());
    }

    #[test]
    #[should_panic(expected = "HNSW dimensions must be > 0")]
    fn test_with_dimensions_rejects_zero() {
        HnswConfig::with_dimensions(0);
    }

    #[test]
    #[should_panic(expected = "HNSW configuration must be valid")]
    fn test_index_with_config_rejects_zero_dimensions() {
        crate::HnswIndex::with_config(HnswConfig {
            dimensions: 0,
            ..Default::default()
        });
    }

    #[test]
    fn test_distance_metric_default() {
        assert_eq!(DistanceMetric::default(), DistanceMetric::Cosine);
    }
}
