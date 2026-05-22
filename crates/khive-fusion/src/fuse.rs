//! Main fusion entry point.

use khive_score::DeterministicScore;
use std::hash::Hash;

use super::rrf::reciprocal_rank_fusion;
use super::strategy::FusionStrategy;
use super::union::union_fusion;
use super::weighted::weighted_fusion;

/// Fuse multiple ranked result lists into a single ranked list.
///
/// This is the main entry point for rank fusion. It supports multiple fusion
/// strategies and is generic over the ID type.
///
/// # Arguments
///
/// * `sources` - Vector of result lists from different retrievers.
///   Each list contains `(Id, DeterministicScore)` pairs, already sorted
///   by score descending (best first).
/// * `strategy` - The fusion strategy to use.
/// * `top_k` - Maximum number of results to return.
///
/// # Returns
///
/// A vector of `(Id, DeterministicScore)` pairs sorted by fused score descending,
/// truncated to `top_k` results.
///
/// # Type Parameters
///
/// * `Id` - The identifier type. Must implement `Eq`, `Hash`, `Clone`, and `Ord`.
///   Works with `EmbeddingId`, `DocumentId`, `String`, `Uuid`, etc.
///   `Ord` is required for deterministic tie-breaking when scores are equal.
///
/// # Example
///
/// ```rust
/// use khive_fusion::{fuse, FusionStrategy};
/// use khive_score::DeterministicScore;
///
/// let sources = vec![
///     vec![("a", DeterministicScore::from_f64(0.9))],
///     vec![("a", DeterministicScore::from_f64(0.8))],
/// ];
///
/// let results = fuse(sources, &FusionStrategy::default(), 10);
/// assert_eq!(results.len(), 1);
/// ```
pub fn fuse<Id: Eq + Hash + Clone + Ord>(
    sources: Vec<Vec<(Id, DeterministicScore)>>,
    strategy: &FusionStrategy,
    top_k: usize,
) -> Vec<(Id, DeterministicScore)> {
    if sources.is_empty() || top_k == 0 {
        return Vec::new();
    }

    let fused = match strategy {
        FusionStrategy::Rrf { k } => reciprocal_rank_fusion(sources, *k),
        FusionStrategy::Weighted { weights } => weighted_fusion(sources, weights),
        FusionStrategy::Union => union_fusion(sources),
        // VectorOnly / KeywordOnly: the caller is responsible for ensuring only
        // the relevant source list is passed. Within fuse(), we take the union
        // (max-score per ID) which is a no-op when there is a single source.
        FusionStrategy::VectorOnly | FusionStrategy::KeywordOnly => union_fusion(sources),
    };

    // Truncate to top_k
    fused.into_iter().take(top_k).collect()
}

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
        let fused = fuse(vec![source], &FusionStrategy::rrf(), 10);

        assert_eq!(fused.len(), 2);
    }

    #[test]
    fn test_fuse_weighted_strategy() {
        let source = make_results(vec![("doc_a", 1.0)]);
        let fused = fuse(vec![source], &FusionStrategy::weighted(vec![1.0]), 10);

        assert_eq!(fused.len(), 1);
    }

    #[test]
    fn test_fuse_union_strategy() {
        let source = make_results(vec![("doc_a", 0.9)]);
        let fused = fuse(vec![source], &FusionStrategy::union(), 10);

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

        let fused = fuse(vec![source], &FusionStrategy::rrf(), 3);

        assert_eq!(fused.len(), 3);
        assert_eq!(fused[0].0, "doc_a");
        assert_eq!(fused[1].0, "doc_b");
        assert_eq!(fused[2].0, "doc_c");
    }

    #[test]
    fn test_fuse_top_k_zero() {
        let source = make_results(vec![("doc_a", 0.9)]);
        let fused = fuse(vec![source], &FusionStrategy::rrf(), 0);

        assert!(fused.is_empty());
    }

    #[test]
    fn test_fuse_empty_sources() {
        let fused: Vec<(&str, DeterministicScore)> = fuse(vec![], &FusionStrategy::rrf(), 10);
        assert!(fused.is_empty());
    }

    #[test]
    fn test_fuse_top_k_larger_than_results() {
        let source = make_results(vec![("doc_a", 0.9), ("doc_b", 0.8)]);
        let fused = fuse(vec![source], &FusionStrategy::rrf(), 100);

        assert_eq!(fused.len(), 2);
    }

    #[test]
    fn test_fuse_with_string_ids() {
        let source: Vec<(String, DeterministicScore)> = vec![
            ("doc_a".to_string(), DeterministicScore::from_f64(0.9)),
            ("doc_b".to_string(), DeterministicScore::from_f64(0.8)),
        ];

        let fused = fuse(vec![source], &FusionStrategy::rrf(), 10);

        assert_eq!(fused.len(), 2);
        assert_eq!(fused[0].0, "doc_a");
    }

    #[test]
    fn test_fuse_with_integer_ids() {
        let source: Vec<(u64, DeterministicScore)> = vec![
            (1, DeterministicScore::from_f64(0.9)),
            (2, DeterministicScore::from_f64(0.8)),
        ];

        let fused = fuse(vec![source], &FusionStrategy::rrf(), 10);

        assert_eq!(fused.len(), 2);
        assert_eq!(fused[0].0, 1);
    }
}
