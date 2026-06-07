//! Dual-index query routing for embedding model migration.

use std::hash::Hash;

use khive_score::DeterministicScore;

use khive_fusion::{fuse, FusionStrategy};

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
    pub fn merge_results(
        &self,
        primary_results: Vec<(Id, DeterministicScore)>,
        legacy_results: Vec<(Id, DeterministicScore)>,
        top_k: usize,
    ) -> Vec<(Id, DeterministicScore)> {
        match &self.config.strategy {
            DualIndexStrategy::PrimaryOnly => {
                let mut results = primary_results;
                results.truncate(top_k);
                results
            }
            DualIndexStrategy::LegacyOnly => {
                let mut results = legacy_results;
                results.truncate(top_k);
                results
            }
            DualIndexStrategy::Both { fusion } => {
                let sources = vec![primary_results, legacy_results];
                let safe = match fusion {
                    FusionStrategy::Custom { .. } => &FusionStrategy::default(),
                    s => s,
                };
                fuse(sources, safe, top_k).expect("non-Custom strategies are infallible")
            }
            DualIndexStrategy::Weighted { primary_weight } => {
                let w = primary_weight.clamp(0.0, 1.0);
                let strategy = FusionStrategy::weighted(vec![w, 1.0 - w]);
                let sources = vec![primary_results, legacy_results];
                fuse(sources, &strategy, top_k).expect("Weighted is infallible")
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
mod tests {
    use super::*;

    /// Helper to build scored result lists from (id, f64) pairs.
    fn make_results(items: Vec<(&str, f64)>) -> Vec<(String, DeterministicScore)> {
        items
            .into_iter()
            .map(|(id, score)| (id.to_string(), DeterministicScore::from_f64(score)))
            .collect()
    }

    // -- Strategy routing tests --

    #[test]
    fn test_primary_only_queries_only_primary() {
        let config = DualIndexConfig::default().with_strategy(DualIndexStrategy::PrimaryOnly);
        let router = DualIndexRouter::<String>::new(config);

        assert!(router.should_query_primary(None));
        assert!(!router.should_query_legacy(None));
    }

    #[test]
    fn test_legacy_only_queries_only_legacy() {
        let config = DualIndexConfig::default().with_strategy(DualIndexStrategy::LegacyOnly);
        let router = DualIndexRouter::<String>::new(config);

        assert!(!router.should_query_primary(None));
        assert!(router.should_query_legacy(None));
    }

    #[test]
    fn test_both_queries_both_indexes() {
        let config = DualIndexConfig::default(); // default is Both { rrf }
        let router = DualIndexRouter::<String>::new(config);

        assert!(router.should_query_primary(None));
        assert!(router.should_query_legacy(None));
    }

    #[test]
    fn test_weighted_queries_both_indexes() {
        let config = DualIndexConfig::default().with_strategy(DualIndexStrategy::Weighted {
            primary_weight: 0.8,
        });
        let router = DualIndexRouter::<String>::new(config);

        assert!(router.should_query_primary(None));
        assert!(router.should_query_legacy(None));
    }

    // -- Auto-switch threshold tests --

    #[test]
    fn test_auto_switch_skips_legacy_when_threshold_exceeded() {
        let config = DualIndexConfig::default().with_auto_switch_threshold(0.95);
        let router = DualIndexRouter::<String>::new(config);

        // Migration at 90% - below threshold, still query legacy
        assert!(router.should_query_legacy(Some(0.90)));

        // Migration at 95% - at threshold, skip legacy (progress >= threshold)
        assert!(!router.should_query_legacy(Some(0.95)));

        // Migration at 99% - above threshold, skip legacy
        assert!(!router.should_query_legacy(Some(0.99)));
    }

    #[test]
    fn test_auto_switch_no_threshold_always_queries_legacy() {
        let config = DualIndexConfig::default(); // no auto_switch_threshold
        let router = DualIndexRouter::<String>::new(config);

        // Even with 100% progress, queries legacy without threshold
        assert!(router.should_query_legacy(Some(1.0)));
    }

    #[test]
    fn test_auto_switch_no_progress_queries_legacy() {
        let config = DualIndexConfig::default().with_auto_switch_threshold(0.95);
        let router = DualIndexRouter::<String>::new(config);

        // No progress info provided - query legacy to be safe
        assert!(router.should_query_legacy(None));
    }

    // -- Pool size tests --

    #[test]
    fn test_pool_size_calculation() {
        let config = DualIndexConfig::default(); // pool_multiplier = 3
        let router = DualIndexRouter::<String>::new(config);

        assert_eq!(router.pool_size(10), 30);
        assert_eq!(router.pool_size(1), 3);
        assert_eq!(router.pool_size(0), 0);
    }

    #[test]
    fn test_pool_size_custom_multiplier() {
        let config = DualIndexConfig::default().with_pool_multiplier(5);
        let router = DualIndexRouter::<String>::new(config);

        assert_eq!(router.pool_size(10), 50);
    }

    // -- Merge results tests --

    #[test]
    fn test_merge_primary_only_returns_primary() {
        let config = DualIndexConfig::default().with_strategy(DualIndexStrategy::PrimaryOnly);
        let router = DualIndexRouter::<String>::new(config);

        let primary = make_results(vec![("a", 0.9), ("b", 0.8)]);
        let legacy = make_results(vec![("c", 0.95), ("d", 0.85)]);

        let merged = router.merge_results(primary, legacy, 10);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].0, "a");
        assert_eq!(merged[1].0, "b");
    }

    #[test]
    fn test_merge_legacy_only_returns_legacy() {
        let config = DualIndexConfig::default().with_strategy(DualIndexStrategy::LegacyOnly);
        let router = DualIndexRouter::<String>::new(config);

        let primary = make_results(vec![("a", 0.9), ("b", 0.8)]);
        let legacy = make_results(vec![("c", 0.95), ("d", 0.85)]);

        let merged = router.merge_results(primary, legacy, 10);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].0, "c");
        assert_eq!(merged[1].0, "d");
    }

    #[test]
    fn test_merge_both_fuses_with_rrf() {
        let config = DualIndexConfig::default(); // Both { Rrf { k: 60 } }
        let router = DualIndexRouter::<String>::new(config);

        let primary = make_results(vec![("a", 0.9), ("b", 0.8)]);
        let legacy = make_results(vec![("b", 0.95), ("c", 0.7)]);

        let merged = router.merge_results(primary, legacy, 10);

        // "b" appears in both sources, should get highest RRF score
        assert_eq!(merged[0].0, "b");
        // All three unique IDs should be present
        assert_eq!(merged.len(), 3);
    }

    #[test]
    fn test_merge_weighted_applies_weights() {
        let config = DualIndexConfig::default().with_strategy(DualIndexStrategy::Weighted {
            primary_weight: 0.8,
        });
        let router = DualIndexRouter::<String>::new(config);

        let primary = make_results(vec![("a", 0.9), ("b", 0.5)]);
        let legacy = make_results(vec![("b", 0.9), ("c", 0.5)]);

        let merged = router.merge_results(primary, legacy, 10);

        // All three unique IDs should appear
        let ids: Vec<&str> = merged.iter().map(|(id, _)| id.as_str()).collect();
        assert!(ids.contains(&"a"));
        assert!(ids.contains(&"b"));
        assert!(ids.contains(&"c"));
    }

    #[test]
    fn test_merge_respects_top_k() {
        let config = DualIndexConfig::default();
        let router = DualIndexRouter::<String>::new(config);

        let primary = make_results(vec![("a", 0.9), ("b", 0.8), ("c", 0.7)]);
        let legacy = make_results(vec![("d", 0.95), ("e", 0.85), ("f", 0.75)]);

        let merged = router.merge_results(primary, legacy, 2);
        assert_eq!(merged.len(), 2);
    }

    #[test]
    fn test_merge_empty_sources() {
        let config = DualIndexConfig::default();
        let router = DualIndexRouter::<String>::new(config);

        let merged = router.merge_results(vec![], vec![], 10);
        assert!(merged.is_empty());
    }

    // -- Strategy mutation tests --

    #[test]
    fn test_set_strategy_updates_routing() {
        let config = DualIndexConfig::default();
        let mut router = DualIndexRouter::<String>::new(config);

        assert!(matches!(router.strategy(), DualIndexStrategy::Both { .. }));

        router.set_strategy(DualIndexStrategy::PrimaryOnly);
        assert!(matches!(router.strategy(), DualIndexStrategy::PrimaryOnly));
    }

    // -- Config builder tests --

    #[test]
    fn test_config_default() {
        let config = DualIndexConfig::default();
        assert!(matches!(config.strategy, DualIndexStrategy::Both { .. }));
        assert_eq!(config.pool_multiplier, 3);
        assert!(config.auto_switch_threshold.is_none());
    }

    #[test]
    fn test_config_builder_chain() {
        let config = DualIndexConfig::default()
            .with_strategy(DualIndexStrategy::Weighted {
                primary_weight: 0.7,
            })
            .with_pool_multiplier(5)
            .with_auto_switch_threshold(0.95);

        assert!(matches!(
            config.strategy,
            DualIndexStrategy::Weighted { primary_weight } if (primary_weight - 0.7).abs() < f64::EPSILON
        ));
        assert_eq!(config.pool_multiplier, 5);
        assert!((config.auto_switch_threshold.unwrap() - 0.95).abs() < f64::EPSILON);
    }

    #[test]
    fn test_pool_multiplier_min_enforced() {
        let config = DualIndexConfig::default().with_pool_multiplier(0);
        assert_eq!(config.pool_multiplier, 1);
    }

    #[test]
    fn test_auto_switch_threshold_clamped() {
        let config = DualIndexConfig::default().with_auto_switch_threshold(1.5);
        assert!((config.auto_switch_threshold.unwrap() - 1.0).abs() < f64::EPSILON);

        let config = DualIndexConfig::default().with_auto_switch_threshold(-0.5);
        assert!((config.auto_switch_threshold.unwrap() - 0.0).abs() < f64::EPSILON);
    }

    // -- Default strategy tests --

    #[test]
    fn test_default_strategy_is_both_rrf() {
        let strategy = DualIndexStrategy::default();
        assert_eq!(
            strategy,
            DualIndexStrategy::Both {
                fusion: FusionStrategy::rrf()
            }
        );
    }
}
