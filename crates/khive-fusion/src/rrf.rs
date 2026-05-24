//! Reciprocal Rank Fusion (RRF) algorithm.
//!
//! # Properties
//!
//! - Better rank → higher contribution: r1 < r2 → contrib(r1) > contrib(r2)
//! - Present documents always outscore absent
//! - Contribution upper bound: ≤ 1/(k+1)
//! - Total score ≤ number of sources
//! - Sum is permutation invariant (order-independent)
//! - Ties broken by ID for deterministic cross-platform ordering

use khive_score::{rrf_score, DeterministicScore};
use std::cmp::Ordering;
use std::collections::HashMap;
use std::hash::Hash;

/// Reciprocal Rank Fusion (RRF) algorithm.
///
/// Combines ranked lists using only rank information, ignoring original scores.
/// This makes it robust to different score distributions and outliers.
///
/// # Formula
///
/// For each document d across all sources:
/// ```text
/// score(d) = Σ 1/(k + rank_i(d))
/// ```
/// where rank_i(d) is the 1-indexed position of d in source i.
///
/// # Arguments
///
/// * `sources` - Vector of result lists. Each list should be sorted by
///   score descending (best first). The scores are ignored; only positions matter.
/// * `k` - Smoothing constant. Standard value is 60.
///
/// # Returns
///
/// A vector of `(Id, DeterministicScore)` pairs sorted by RRF score descending.
///
/// # Example
///
/// ```rust
/// use khive_fusion::reciprocal_rank_fusion;
/// use khive_score::DeterministicScore;
///
/// let source1 = vec![
///     ("doc_a", DeterministicScore::from_f64(0.9)),  // rank 1
///     ("doc_b", DeterministicScore::from_f64(0.8)),  // rank 2
/// ];
/// let source2 = vec![
///     ("doc_b", DeterministicScore::from_f64(0.95)), // rank 1
///     ("doc_a", DeterministicScore::from_f64(0.7)),  // rank 2
/// ];
///
/// let fused = reciprocal_rank_fusion(vec![source1, source2], 60);
///
/// // doc_a: 1/(60+1) + 1/(60+2) = 1/61 + 1/62 ≈ 0.0326
/// // doc_b: 1/(60+2) + 1/(60+1) = 1/62 + 1/61 ≈ 0.0326
/// // Scores are equal since both appear at ranks 1 and 2 (just swapped)
/// // Ties are broken by ID (lexicographic order) for determinism
/// ```
///
/// **Property**: better rank → higher contribution.
/// Better rank yields higher contribution: r1 < r2 implies 1/(k+r1) > 1/(k+r2).
///
/// **Property**: present > absent.
/// Documents present in any source always outscore documents absent from all sources
/// (absent documents have score 0, present documents have score > 0).
pub fn reciprocal_rank_fusion<Id: Eq + Hash + Clone + Ord>(
    sources: Vec<Vec<(Id, DeterministicScore)>>,
    k: usize,
) -> Vec<(Id, DeterministicScore)> {
    if sources.is_empty() {
        return Vec::new();
    }

    // Ensure k >= 1 to avoid division issues
    let k = k.max(1);

    // Estimate capacity as sum of all source lengths (upper bound on unique IDs)
    let estimated_capacity: usize = sources.iter().map(|s| s.len()).sum();
    let mut combined: HashMap<Id, DeterministicScore> = HashMap::with_capacity(estimated_capacity);

    for results in sources {
        for (rank_0_indexed, (id, _score)) in results.into_iter().enumerate() {
            // rank is 1-indexed per ADR-002
            let rank_1_indexed = rank_0_indexed + 1;
            let contribution = rrf_score(rank_1_indexed, k);
            let entry = combined.entry(id).or_insert(DeterministicScore::ZERO);
            *entry = *entry + contribution;
        }
    }

    // Sort descending by fixed-point score; permutation-invariant since DeterministicScore
    // addition is order-independent (i128 accumulation in Add impl).
    let mut fused: Vec<(Id, DeterministicScore)> = combined.into_iter().collect();

    // Sort by score descending, then by ID ascending for deterministic tie-breaking
    // This ensures cross-platform consistency when scores are equal
    fused.sort_by(
        |(id_a, score_a), (id_b, score_b)| match score_b.cmp(score_a) {
            Ordering::Equal => id_a.cmp(id_b),
            other => other,
        },
    );

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
    fn test_rrf_basic_two_sources() {
        let source1 = make_results(vec![("doc_a", 0.9), ("doc_b", 0.8)]);
        let source2 = make_results(vec![("doc_b", 0.95), ("doc_c", 0.7)]);

        let fused = reciprocal_rank_fusion(vec![source1, source2], 60);

        // doc_b appears in both, should have highest score
        assert_eq!(fused[0].0, "doc_b");
        assert_eq!(fused.len(), 3);
    }

    #[test]
    fn test_rrf_score_calculation() {
        let source = make_results(vec![("doc_a", 0.9)]);
        let fused = reciprocal_rank_fusion(vec![source], 60);

        let expected = 1.0 / 61.0;
        assert!((fused[0].1.to_f64() - expected).abs() < 1e-9);
    }

    #[test]
    fn test_rrf_cumulative_scores() {
        let source1 = make_results(vec![("doc_a", 0.9)]);
        let source2 = make_results(vec![("doc_a", 0.8)]);

        let fused = reciprocal_rank_fusion(vec![source1, source2], 60);

        let expected = 2.0 / 61.0;
        assert!((fused[0].1.to_f64() - expected).abs() < 1e-9);
    }

    #[test]
    fn test_rrf_ignores_scores() {
        let source1_high = make_results(vec![("doc_a", 0.99), ("doc_b", 0.01)]);
        let source1_low = make_results(vec![("doc_a", 0.6), ("doc_b", 0.5)]);

        let fused_high = reciprocal_rank_fusion(vec![source1_high], 60);
        let fused_low = reciprocal_rank_fusion(vec![source1_low], 60);

        assert_eq!(fused_high[0].1, fused_low[0].1);
        assert_eq!(fused_high[1].1, fused_low[1].1);
    }

    #[test]
    fn test_rrf_empty_sources() {
        let fused: Vec<(&str, DeterministicScore)> = reciprocal_rank_fusion(vec![], 60);
        assert!(fused.is_empty());
    }

    #[test]
    fn test_rrf_single_source_passthrough() {
        let source = make_results(vec![("doc_a", 0.9), ("doc_b", 0.8), ("doc_c", 0.7)]);
        let fused = reciprocal_rank_fusion(vec![source], 60);

        assert_eq!(fused.len(), 3);
        assert_eq!(fused[0].0, "doc_a");
        assert_eq!(fused[1].0, "doc_b");
        assert_eq!(fused[2].0, "doc_c");
    }

    #[test]
    fn test_rrf_k_minimum_enforced() {
        let source = make_results(vec![("doc_a", 0.9)]);
        let fused = reciprocal_rank_fusion(vec![source], 0);

        let expected = 1.0 / 2.0;
        assert!((fused[0].1.to_f64() - expected).abs() < 1e-9);
    }

    #[test]
    fn test_rrf_many_sources() {
        let sources: Vec<Vec<(&str, DeterministicScore)>> =
            (0..5).map(|_| make_results(vec![("doc_a", 0.9)])).collect();

        let fused = reciprocal_rank_fusion(sources, 60);

        let expected = 5.0 / 61.0;
        assert!((fused[0].1.to_f64() - expected).abs() < 1e-9);
    }
}
