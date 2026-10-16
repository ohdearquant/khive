//! Dual-index query routing for embedding model migration.

use std::hash::Hash;

use khive_score::DeterministicScore;

use khive_fusion::{fuse, FuseError, FusionStrategy};

/// Strategy for routing queries during dual-index operation.
///
/// Controls which indexes are queried and how results are combined.
#[derive(Debug, Clone, PartialEq)]
pub enum DualIndexStrategy {
    /// Query both indexes, fuse results (default during migration).
    ///
    /// Uses the specified fusion strategy to combine results from both
    /// the primary (new) and legacy (old) indexes.
    Both {
        /// Fusion strategy for combining results from both indexes.
        fusion: FusionStrategy,
    },

    /// Query only the primary (new) index.
    ///
    /// Use after migration is complete and all documents have been
    /// re-embedded with the new model.
    PrimaryOnly,

    /// Query only the legacy (old) index.
    ///
    /// Use as a fallback if the new index has issues.
    LegacyOnly,

    /// Weighted preference: primary gets `primary_weight`, legacy gets `1 - primary_weight`.
    ///
    /// Useful during mid-migration when the new index covers most documents
    /// but the old index still has better coverage for some.
    Weighted {
        /// Weight for primary index results, in range [0.0, 1.0].
        /// Legacy index weight is computed as `1.0 - primary_weight`.
        primary_weight: f64,
    },
}

impl Default for DualIndexStrategy {
    fn default() -> Self {
        DualIndexStrategy::Both {
            fusion: FusionStrategy::rrf(),
        }
    }
}

/// Configuration for dual-index query routing.
#[derive(Debug, Clone)]
pub struct DualIndexConfig {
    /// Routing strategy.
    pub strategy: DualIndexStrategy,

    /// Candidate pool multiplier for each index.
    ///
    /// Each index fetches `top_k * pool_multiplier` candidates before fusion.
    /// Default: 3.
    pub pool_multiplier: usize,

    /// Minimum migration progress to auto-switch to `PrimaryOnly`, in range [0.0, 1.0].
    ///
    /// When `migration_progress >= auto_switch_threshold`, the router automatically
    /// skips the legacy index. Set to `None` to disable auto-switching.
    pub auto_switch_threshold: Option<f64>,
}

impl Default for DualIndexConfig {
    fn default() -> Self {
        Self {
            strategy: DualIndexStrategy::default(),
            pool_multiplier: 3,
            auto_switch_threshold: None,
        }
    }
}

impl DualIndexConfig {
    /// Create a config with a specific strategy.
    pub fn with_strategy(mut self, strategy: DualIndexStrategy) -> Self {
        self.strategy = strategy;
        self
    }

    /// Set the candidate pool multiplier.
    pub fn with_pool_multiplier(mut self, multiplier: usize) -> Self {
        self.pool_multiplier = multiplier.max(1);
        self
    }

    /// Set the auto-switch threshold for migration progress.
    pub fn with_auto_switch_threshold(mut self, threshold: f64) -> Self {
        self.auto_switch_threshold = Some(threshold.clamp(0.0, 1.0));
        self
    }
}

/// Routes queries between primary (new) and legacy (old) vector indexes during migration.
pub struct DualIndexRouter<Id> {
    config: DualIndexConfig,
    _marker: std::marker::PhantomData<Id>,
}

impl<Id> DualIndexRouter<Id>
where
    Id: Eq + Hash + Clone + Ord,
{
    /// Create a new dual-index router with the given configuration.
    pub fn new(config: DualIndexConfig) -> Self {
        Self {
            config,
            _marker: std::marker::PhantomData,
        }
    }

    /// Determine whether the primary (new) index should be queried.
    pub fn should_query_primary(&self, _migration_progress: Option<f64>) -> bool {
        !matches!(self.config.strategy, DualIndexStrategy::LegacyOnly)
    }

    /// Determine whether the legacy (old) index should be queried.
    pub fn should_query_legacy(&self, migration_progress: Option<f64>) -> bool {
        match &self.config.strategy {
            DualIndexStrategy::PrimaryOnly => false,
            DualIndexStrategy::LegacyOnly => true,
            DualIndexStrategy::Both { .. } | DualIndexStrategy::Weighted { .. } => {
                // Auto-switch: if migration is nearly complete, skip legacy
                if let (Some(threshold), Some(progress)) =
                    (self.config.auto_switch_threshold, migration_progress)
                {
                    progress < threshold
                } else {
                    true
                }
            }
        }
    }

    /// Get the candidate pool size for each index (`top_k * pool_multiplier`).
    pub fn pool_size(&self, top_k: usize) -> usize {
        top_k.saturating_mul(self.config.pool_multiplier)
    }

    /// Merge results from primary and legacy indexes using the configured strategy.
    ///
    /// Positional strategies use `[primary, legacy]`. This migration-specific
    /// contract is independent of the `[vector, keyword]` order used by
    /// two-arm hybrid search APIs.
    /// Weighted-RRF parameter, source-count and score errors are returned unchanged.
    pub fn merge_results(
        &self,
        primary_results: Vec<(Id, DeterministicScore)>,
        legacy_results: Vec<(Id, DeterministicScore)>,
        top_k: usize,
    ) -> Result<Vec<(Id, DeterministicScore)>, FuseError> {
        match &self.config.strategy {
            DualIndexStrategy::PrimaryOnly => {
                let mut results = primary_results;
                results.truncate(top_k);
                Ok(results)
            }
            DualIndexStrategy::LegacyOnly => {
                let mut results = legacy_results;
                results.truncate(top_k);
                Ok(results)
            }
            DualIndexStrategy::Both { fusion } => {
                let sources = vec![primary_results, legacy_results];
                let safe = match fusion {
                    FusionStrategy::Custom { .. } => &FusionStrategy::default(),
                    s => s,
                };
                fuse(sources, safe, top_k)
            }
            DualIndexStrategy::Weighted { primary_weight } => {
                let w = if primary_weight.is_nan() {
                    0.5
                } else {
                    primary_weight.clamp(0.0, 1.0)
                };
                let strategy = FusionStrategy::try_weighted(vec![w, 1.0 - w])
                    .unwrap_or_else(|_| FusionStrategy::weighted(vec![0.5, 0.5]));
                let sources = vec![primary_results, legacy_results];
                fuse(sources, &strategy, top_k)
            }
        }
    }

    /// Get a reference to the current routing strategy.
    pub fn strategy(&self) -> &DualIndexStrategy {
        &self.config.strategy
    }

    /// Update the routing strategy (e.g., when migration completes).
    pub fn set_strategy(&mut self, strategy: DualIndexStrategy) {
        self.config.strategy = strategy;
    }

    /// Get a reference to the full configuration.
    pub fn config(&self) -> &DualIndexConfig {
        &self.config
    }
}

#[cfg(test)]
#[path = "dual_index/tests.rs"]
mod tests;
