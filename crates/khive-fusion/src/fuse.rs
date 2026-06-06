//! Main fusion entry point.

use khive_score::DeterministicScore;
use std::hash::Hash;

use super::rrf::reciprocal_rank_fusion;
use super::strategy::FusionStrategy;
use super::union::union_fusion;
use super::weighted::weighted_fusion;

/// Fuse multiple ranked result lists into a single list sorted by fused score descending.
///
/// Dispatches by `strategy`; `Custom` strategies return [`FuseError::CustomRequiresRuntime`].
pub fn fuse<Id: Eq + Hash + Clone + Ord>(
    sources: Vec<Vec<(Id, DeterministicScore)>>,
    strategy: &FusionStrategy,
    top_k: usize,
) -> Result<Vec<(Id, DeterministicScore)>, FuseError> {
    if sources.is_empty() || top_k == 0 {
        return Ok(Vec::new());
    }

    let fused = match strategy {
        FusionStrategy::Rrf { k } => reciprocal_rank_fusion(sources, *k),
        FusionStrategy::Weighted { weights } => weighted_fusion(sources, weights),
        FusionStrategy::Union => union_fusion(sources),
        FusionStrategy::VectorOnly | FusionStrategy::KeywordOnly => union_fusion(sources),
        FusionStrategy::Custom { name, .. } => {
            return Err(FuseError::CustomRequiresRuntime(name.clone()));
        }
    };

    Ok(fused.into_iter().take(top_k).collect())
}

/// Error from the [`fuse`] entry point.
#[derive(Debug, Clone, PartialEq)]
pub enum FuseError {
    /// Custom strategies must be dispatched through the runtime registry.
    CustomRequiresRuntime(String),
}

impl std::fmt::Display for FuseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CustomRequiresRuntime(name) => {
                write!(
                    f,
                    "custom strategy '{}' requires runtime FusionRegistry dispatch",
                    name
                )
            }
        }
    }
}

impl std::error::Error for FuseError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_results<Id: Clone>(items: Vec<(Id, f64)>) -> Vec<(Id, DeterministicScore)> {
        items
            .into_iter()
            .map(|(id, score)| (id, DeterministicScore::from_f64(score)))
            .collect()
    }

    #[test]
    fn test_fuse_rrf_strategy() {
        let source = make_results(vec![("doc_a", 0.9), ("doc_b", 0.8)]);
        let fused = fuse(vec![source], &FusionStrategy::rrf(), 10).unwrap();

        assert_eq!(fused.len(), 2);
    }

    #[test]
    fn test_fuse_weighted_strategy() {
        let source = make_results(vec![("doc_a", 1.0)]);
        let fused = fuse(vec![source], &FusionStrategy::weighted(vec![1.0]), 10).unwrap();

        assert_eq!(fused.len(), 1);
    }

    #[test]
    fn test_fuse_union_strategy() {
        let source = make_results(vec![("doc_a", 0.9)]);
        let fused = fuse(vec![source], &FusionStrategy::union(), 10).unwrap();

        assert_eq!(fused.len(), 1);
    }

    #[test]
    fn test_fuse_top_k_truncation() {
        let source = make_results(vec![
            ("doc_a", 0.9),
            ("doc_b", 0.8),
            ("doc_c", 0.7),
            ("doc_d", 0.6),
            ("doc_e", 0.5),
        ]);

        let fused = fuse(vec![source], &FusionStrategy::rrf(), 3).unwrap();

        assert_eq!(fused.len(), 3);
        assert_eq!(fused[0].0, "doc_a");
        assert_eq!(fused[1].0, "doc_b");
        assert_eq!(fused[2].0, "doc_c");
    }

    #[test]
    fn test_fuse_top_k_zero() {
        let source = make_results(vec![("doc_a", 0.9)]);
        let fused = fuse(vec![source], &FusionStrategy::rrf(), 0).unwrap();

        assert!(fused.is_empty());
    }

    #[test]
    fn test_fuse_empty_sources() {
        let fused: Vec<(&str, DeterministicScore)> =
            fuse(vec![], &FusionStrategy::rrf(), 10).unwrap();
        assert!(fused.is_empty());
    }

    #[test]
    fn test_fuse_top_k_larger_than_results() {
        let source = make_results(vec![("doc_a", 0.9), ("doc_b", 0.8)]);
        let fused = fuse(vec![source], &FusionStrategy::rrf(), 100).unwrap();

        assert_eq!(fused.len(), 2);
    }

    #[test]
    fn test_fuse_with_string_ids() {
        let source: Vec<(String, DeterministicScore)> = vec![
            ("doc_a".to_string(), DeterministicScore::from_f64(0.9)),
            ("doc_b".to_string(), DeterministicScore::from_f64(0.8)),
        ];

        let fused = fuse(vec![source], &FusionStrategy::rrf(), 10).unwrap();

        assert_eq!(fused.len(), 2);
        assert_eq!(fused[0].0, "doc_a");
    }

    #[test]
    fn test_fuse_with_integer_ids() {
        let source: Vec<(u64, DeterministicScore)> = vec![
            (1, DeterministicScore::from_f64(0.9)),
            (2, DeterministicScore::from_f64(0.8)),
        ];

        let fused = fuse(vec![source], &FusionStrategy::rrf(), 10).unwrap();

        assert_eq!(fused.len(), 2);
        assert_eq!(fused[0].0, 1);
    }

    #[test]
    fn test_fuse_custom_returns_error() {
        let source = make_results(vec![("doc_a", 0.9)]);
        let strategy =
            FusionStrategy::try_custom("decay_weighted".to_string(), serde_json::json!({}))
                .unwrap();
        let result = fuse(vec![source], &strategy, 10);
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err(),
            FuseError::CustomRequiresRuntime("decay_weighted".to_string())
        );
    }
}
