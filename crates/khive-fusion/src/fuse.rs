//! Main fusion entry point.

use khive_score::DeterministicScore;
use std::cmp::Ordering;
use std::hash::Hash;

use super::rrf::reciprocal_rank_fusion;
use super::strategy::FusionStrategy;
use super::union::union_fusion;
use super::weighted::weighted_fusion;

/// Fuse multiple ranked result lists into a single ranked list.
///
/// Main entry point for rank fusion. Generic over the ID type (`Eq + Hash + Clone + Ord`).
/// Sources are `(Id, DeterministicScore)` pairs, sorted by score descending.
/// Returns at most `top_k` results sorted by fused score descending.
///
/// See `docs/algorithm.md` for strategy details, the RRF formula, and weight normalization.
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
        // VectorOnly / KeywordOnly: exactly one source is required.
        // Multiple sources contradict the "raw single-retriever passthrough" semantics.
        // Return empty in both debug and release builds so callers get consistent,
        // detectable behavior instead of silently incorrect results.
        FusionStrategy::VectorOnly | FusionStrategy::KeywordOnly => {
            if sources.len() != 1 {
                // Invalid source count for passthrough strategies: return empty so
                // callers can detect the wiring error in both debug and release builds.
                return Vec::new();
            }
            // Return the single source sorted by score desc (union_fusion is correct for 1 source).
            let first = sources.into_iter().next().unwrap_or_default();
            union_fusion(vec![first])
        }
    };

    truncate_top_k(fused, top_k)
}

/// Truncate a fused result list to at most `top_k` items, sorted by score
/// descending with ID-ascending tie-breaking.
///
/// Uses `select_nth_unstable_by` (O(n)) to partition the top-k elements, then
/// sorts only the prefix (O(k log k)), avoiding a full O(n log n) sort when
/// `top_k << n` (finding #6).
fn truncate_top_k<Id: Ord>(
    mut fused: Vec<(Id, DeterministicScore)>,
    top_k: usize,
) -> Vec<(Id, DeterministicScore)> {
    if top_k == 0 || fused.is_empty() {
        return Vec::new();
    }

    let cmp = |(id_a, score_a): &(Id, DeterministicScore),
               (id_b, score_b): &(Id, DeterministicScore)| {
        match score_b.cmp(score_a) {
            Ordering::Equal => id_a.cmp(id_b),
            other => other,
        }
    };

    if top_k < fused.len() {
        // Partition so that fused[..top_k] are the top-k elements (unsorted).
        fused.select_nth_unstable_by(top_k - 1, cmp);
        fused.truncate(top_k);
    }

    // Sort only the (now small) prefix.
    fused.sort_by(cmp);
    fused
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
