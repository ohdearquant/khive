//! HNSW configuration types.

use serde::{Deserialize, Deserializer, Serialize};

use crate::error::{Result, RetrievalError};

/// Maximum allowed level in the HNSW graph.
/// Prevents unbounded memory allocation from malformed random values.
/// For 1 billion vectors with typical ml, expected max level is ~16-18.
pub const MAX_LEVEL: usize = 64;

/// Default threshold for triggering a rebuild (10% tombstones).
/// At this ratio the index query recall degrades measurably; rebuild restores full quality.
pub const DEFAULT_REBUILD_THRESHOLD: f64 = 0.10;

// Re-export from canonical location (foundation/types).
// Canonical variants: Cosine, Dot, L2.
// Serde aliases on canonical handle backward compat: "euclidean" -> L2, "dot_product" -> Dot.
pub use khive_types::vector::DistanceMetric;

/// HNSW index configuration parameters.
/// Deserialization validates all invariants; invalid configs are rejected with a descriptive error.
#[derive(Debug, Clone, Serialize)]
pub struct HnswConfig {
    /// Maximum connections per node per layer (M).
    pub m: usize,

    /// Maximum connections for layer 0 (typically 2*M).
    pub m_max0: usize,

    /// Dynamic candidate list size during construction.
    pub ef_construction: usize,

    /// Level normalization factor: 1/ln(M).
    pub ml: f64,

    /// Dynamic candidate list size during search.
    pub ef_search: usize,

    /// Vector dimensions (must match embedding model).
    pub dimensions: usize,

    /// Distance metric for similarity computation.
    pub metric: DistanceMetric,

    /// Tombstone ratio threshold; above this, rebuild() is recommended.
    pub rebuild_threshold: f64,

    /// Seed for reproducible level generation; `None` uses OS entropy.
    #[serde(default)]
    pub seed: Option<u64>,

    /// Memory budget in bytes; inserts exceeding it return `BudgetExceeded`.
    #[serde(default)]
    pub memory_budget: Option<usize>,
}

/// Wire-format mirror for deserialization; validated in the `HnswConfig::Deserialize` impl.
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
    /// M=20, ef_construction=200, ef_search=80, dimensions=384.
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
        if self.ef_construction == 0 {
            return Err(RetrievalError::Configuration(
                "ef_construction: must be greater than zero".to_string(),
            ));
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

    /// Create config with custom dimensions. Panics if `dimensions` is 0.
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
    #[must_use]
    pub fn with_seed(mut self, seed: u64) -> Self {
        self.seed = Some(seed);
        self
    }

    /// Set memory budget in bytes; inserts exceeding it return `BudgetExceeded`.
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
    fn test_validate_rejects_ef_construction_zero() {
        let config = HnswConfig {
            ef_construction: 0,
            ..Default::default()
        };
        assert!(
            config.validate().is_err(),
            "ef_construction = 0 must be rejected"
        );
    }

    #[test]
    fn test_serde_rejects_ef_construction_zero() {
        let json = r#"{"m":20,"m_max0":40,"ef_construction":0,"ml":0.2886751345948129,"ef_search":80,"dimensions":384,"metric":"cosine","rebuild_threshold":0.1}"#;
        let result: std::result::Result<HnswConfig, _> = serde_json::from_str(json);
        assert!(
            result.is_err(),
            "ef_construction = 0 must be rejected at deserialization"
        );
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
