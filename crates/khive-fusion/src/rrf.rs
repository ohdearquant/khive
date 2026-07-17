//! Reciprocal Rank Fusion (RRF) algorithm.

use khive_score::{rrf_score, DeterministicScore};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::hash::Hash;

/// Fuse score-descending sources by `sum 1/(max(k, 1) + one_based_rank)`.
///
/// Duplicate IDs vote once per source. All IDs are returned by descending score, breaking ties by
/// ascending ID. See
/// `crates/khive-fusion/docs/api/fusion-functions.md`.
pub fn reciprocal_rank_fusion<Id: Eq + Hash + Clone + Ord>(
    sources: Vec<Vec<(Id, DeterministicScore)>>,
    k: usize,
) -> Vec<(Id, DeterministicScore)> {
    if sources.is_empty() {
        return Vec::new();
    }

    let k = k.max(1);

    // Saturation keeps adversarial length sums from wrapping allocation capacity.
    let estimated_capacity: usize = sources
        .iter()
        .map(|s| s.len())
        .fold(0usize, |acc, n| acc.saturating_add(n));
    let mut combined: HashMap<Id, DeterministicScore> = HashMap::with_capacity(estimated_capacity);

    for results in sources {
        // Keep the best rank so one retriever cannot vote twice for one ID.
        let mut seen_in_source: HashSet<Id> = HashSet::with_capacity(results.len());
        for (rank_0_indexed, (id, _score)) in results.into_iter().enumerate() {
            if !seen_in_source.insert(id.clone()) {
                continue;
            }
            let rank_1_indexed = rank_0_indexed + 1;
            let contribution = rrf_score(rank_1_indexed, k);
            let entry = combined.entry(id).or_insert(DeterministicScore::ZERO);
            *entry = *entry + contribution;
        }
    }

    let mut fused: Vec<(Id, DeterministicScore)> = combined.into_iter().collect();

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
