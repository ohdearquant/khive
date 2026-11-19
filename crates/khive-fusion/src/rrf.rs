//! Reciprocal Rank Fusion (RRF) algorithm.

use khive_score::{rrf_score, DeterministicScore};
use std::collections::{HashMap, HashSet};
use std::hash::Hash;

use crate::ordering::cmp_desc_then_id;

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

    fused.sort_by(cmp_desc_then_id);

    fused
}

#[cfg(test)]
#[path = "rrf_tests.rs"]
mod tests;
