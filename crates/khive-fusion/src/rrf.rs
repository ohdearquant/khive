//! Reciprocal Rank Fusion (RRF) algorithm.

use khive_score::{rrf_score, DeterministicScore};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::hash::Hash;

/// Reciprocal Rank Fusion: `score(d) = sum 1/(k + rank_i(d))` over all sources.
/// `sources` sorted score-descending; `k` >= 1 enforced internally.
pub fn reciprocal_rank_fusion<Id: Eq + Hash + Clone + Ord>(
    sources: Vec<Vec<(Id, DeterministicScore)>>,
    k: usize,
) -> Vec<(Id, DeterministicScore)> {
    if sources.is_empty() {
        return Vec::new();
    }

    // Ensure k >= 1 to avoid division issues
    let k = k.max(1);

    // Estimate capacity as sum of all source lengths (upper bound on unique IDs).
    // Use saturating_add to avoid usize overflow on adversarial inputs (finding #6).
    let estimated_capacity: usize = sources
        .iter()
        .map(|s| s.len())
        .fold(0usize, |acc, n| acc.saturating_add(n));
    let mut combined: HashMap<Id, DeterministicScore> = HashMap::with_capacity(estimated_capacity);

    for results in sources {
        // Deduplicate IDs within the same source: keep only the best (lowest) rank
        // so one retriever cannot vote multiple times for the same document
        // (finding #4). We iterate in rank order (best first) and skip duplicates.
        let mut seen_in_source: HashSet<Id> = HashSet::with_capacity(results.len());
        for (rank_0_indexed, (id, _score)) in results.into_iter().enumerate() {
            if !seen_in_source.insert(id.clone()) {
                // Already seen: a later (worse) occurrence — skip it.
                continue;
            }
            // rank is 1-indexed: position 0 in the input list → rank 1
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
