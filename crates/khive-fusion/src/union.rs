//! Union fusion (max score per ID).

use khive_score::DeterministicScore;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::hash::Hash;

/// Return every ID at its maximum source score, ordered by score then ID.
///
/// See `crates/khive-fusion/docs/api/fusion-functions.md`.
pub fn union_fusion<Id: Eq + Hash + Clone + Ord>(
    sources: Vec<Vec<(Id, DeterministicScore)>>,
) -> Vec<(Id, DeterministicScore)> {
    if sources.is_empty() {
        return Vec::new();
    }

    let estimated_capacity: usize = sources.iter().map(|s| s.len()).sum();
    let mut combined: HashMap<Id, DeterministicScore> = HashMap::with_capacity(estimated_capacity);

    for results in sources {
        for (id, score) in results {
            combined
                .entry(id)
                .and_modify(|existing| {
                    if score > *existing {
                        *existing = score;
                    }
                })
                .or_insert(score);
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
    fn test_union_takes_max_score() {
        let source1 = make_results(vec![("doc_a", 0.7)]);
        let source2 = make_results(vec![("doc_a", 0.9)]);

        let fused = union_fusion(vec![source1, source2]);

        assert_eq!(fused.len(), 1);
        assert!((fused[0].1.to_f64() - 0.9).abs() < 0.01);
    }

    #[test]
    fn test_union_disjoint_sources() {
        let source1 = make_results(vec![("doc_a", 0.8)]);
        let source2 = make_results(vec![("doc_b", 0.6)]);

        let fused = union_fusion(vec![source1, source2]);

        assert_eq!(fused.len(), 2);
        assert_eq!(fused[0].0, "doc_a");
        assert_eq!(fused[1].0, "doc_b");
    }

    #[test]
    fn test_union_empty_sources() {
        let fused: Vec<(&str, DeterministicScore)> = union_fusion(vec![]);
        assert!(fused.is_empty());
    }

    #[test]
    fn test_union_single_source() {
        let source = make_results(vec![("doc_a", 0.9), ("doc_b", 0.7)]);
        let fused = union_fusion(vec![source]);

        assert_eq!(fused.len(), 2);
        assert_eq!(fused[0].0, "doc_a");
        assert_eq!(fused[1].0, "doc_b");
    }
}
