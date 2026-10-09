//! Main fusion entry point.

use khive_score::{rrf_score, DeterministicScore};
use std::collections::{HashMap, HashSet};
use std::hash::Hash;

use super::ordering::cmp_desc_then_id;
use super::rrf::reciprocal_rank_fusion;
use super::strategy::FusionStrategy;
use super::union::union_fusion;
use super::weighted::weighted_fusion;

/// Fuse ranked sources and retain at most `top_k` results.
///
/// RRF, weighted RRF, weighted, and union results sort by score then ID; pass-through modes
/// preserve source order. Weighted RRF validates its weights against the ordered source slots and
/// returns an error for scores outside the finite deterministic-score range. `Custom` strategies
/// are the openness mechanism ADR-012 (§`FusionStrategy`) reserves for
/// runtime-registered executors: this crate has no runtime context to dispatch them, so `fuse`
/// returns [`FuseError::CustomRequiresRuntime`] and the caller (`khive-runtime`'s
/// `KhiveRuntime::register_fusion_strategy`/dispatch boundary) resolves the name instead. See
/// `crates/khive-fusion/docs/api/fusion-functions.md`.
pub fn fuse<Id: Eq + Hash + Clone + Ord>(
    sources: Vec<Vec<(Id, DeterministicScore)>>,
    strategy: &FusionStrategy,
    top_k: usize,
) -> Result<Vec<(Id, DeterministicScore)>, FuseError> {
    if sources.is_empty() || top_k == 0 {
        if let FusionStrategy::WeightedRrf { k, weights } = strategy {
            validate_weighted_rrf(*k, weights, sources.len())?;
        }
        return Ok(Vec::new());
    }

    let fused = match strategy {
        FusionStrategy::Rrf { k } => reciprocal_rank_fusion(sources, *k),
        FusionStrategy::Weighted { weights } => weighted_fusion(sources, weights),
        FusionStrategy::WeightedRrf { k, weights } => weighted_rrf_fusion(sources, *k, weights)?,
        FusionStrategy::Union => union_fusion(sources),
        FusionStrategy::VectorOnly => passthrough_source(sources, 0),
        FusionStrategy::KeywordOnly => passthrough_source(sources, 1),
        FusionStrategy::Custom { name, .. } => {
            return Err(FuseError::CustomRequiresRuntime(name.clone()));
        }
    };

    Ok(fused.into_iter().take(top_k).collect())
}

fn validate_weighted_rrf(k: usize, weights: &[f64], source_count: usize) -> Result<(), FuseError> {
    FusionStrategy::try_weighted_rrf(k, weights.to_vec())
        .map_err(FuseError::InvalidWeightedRrfStrategy)?;
    if source_count != weights.len() {
        return Err(FuseError::WeightedRrfWeightCountMismatch {
            source_count,
            weight_count: weights.len(),
        });
    }
    Ok(())
}

fn weighted_rrf_fusion<Id: Eq + Hash + Clone + Ord>(
    sources: Vec<Vec<(Id, DeterministicScore)>>,
    k: usize,
    weights: &[f64],
) -> Result<Vec<(Id, DeterministicScore)>, FuseError> {
    validate_weighted_rrf(k, weights, sources.len())?;
    let estimated_capacity = sources
        .iter()
        .map(Vec::len)
        .fold(0usize, usize::saturating_add);
    let mut combined: HashMap<Id, i64> = HashMap::with_capacity(estimated_capacity);

    for (source_index, results) in sources.into_iter().enumerate() {
        let weight = weights[source_index];
        let mut seen_in_source = HashSet::with_capacity(results.len());

        for (rank_index, (id, _)) in results.into_iter().enumerate() {
            if !seen_in_source.insert(id.clone()) {
                continue;
            }

            let contribution = rrf_score(rank_index + 1, k).to_raw() as f64 * weight;
            let contribution = contribution.round();
            if !contribution.is_finite() || contribution >= i64::MAX as f64 {
                return Err(FuseError::WeightedRrfScoreOverflow);
            }
            let contribution = contribution as i64;

            let score = combined.entry(id).or_default();
            let Some(total) = score.checked_add(contribution) else {
                return Err(FuseError::WeightedRrfScoreOverflow);
            };
            if total == i64::MAX {
                return Err(FuseError::WeightedRrfScoreOverflow);
            }
            *score = total;
        }
    }

    let mut fused: Vec<_> = combined
        .into_iter()
        .map(|(id, score)| (id, DeterministicScore::from_raw(score)))
        .collect();
    fused.sort_by(cmp_desc_then_id);
    Ok(fused)
}

/// Select a single source by index, treating a lone source as authoritative
/// regardless of the requested index (e.g. vector-only search with no keyword source).
fn passthrough_source<Id>(
    sources: Vec<Vec<(Id, DeterministicScore)>>,
    source_index: usize,
) -> Vec<(Id, DeterministicScore)> {
    if sources.len() == 1 {
        return sources.into_iter().next().unwrap_or_default();
    }

    sources.into_iter().nth(source_index).unwrap_or_default()
}

/// Error from the [`fuse`] entry point.
#[derive(Debug, Clone, PartialEq)]
pub enum FuseError {
    /// `Custom` strategies must be dispatched through a runtime's registered
    /// `FusionExecutor` (ADR-012) -- this crate has no runtime context.
    CustomRequiresRuntime(String),
    /// A directly constructed WeightedRrf strategy violated its parameter invariants.
    InvalidWeightedRrfStrategy(super::strategy::FusionStrategyError),
    /// WeightedRrf requires exactly one weight per source slot, including empty slots.
    WeightedRrfWeightCountMismatch {
        /// Number of ordered source slots supplied to fusion.
        source_count: usize,
        /// Number of weights supplied by the strategy.
        weight_count: usize,
    },
    /// A weighted RRF score cannot be represented as a finite deterministic score.
    WeightedRrfScoreOverflow,
}

impl std::fmt::Display for FuseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CustomRequiresRuntime(name) => {
                write!(
                    f,
                    "custom strategy '{}' requires runtime FusionExecutor dispatch",
                    name
                )
            }
            Self::InvalidWeightedRrfStrategy(error) => {
                write!(f, "invalid WeightedRrf strategy: {error}")
            }
            Self::WeightedRrfWeightCountMismatch {
                source_count,
                weight_count,
            } => write!(
                f,
                "WeightedRrf requires one weight per source slot: got {weight_count} weights for {source_count} sources"
            ),
            Self::WeightedRrfScoreOverflow => {
                write!(f, "WeightedRrf score exceeds the finite deterministic score range")
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
    fn vector_only_two_sources_returns_only_vector_source() {
        let vector = make_results(vec![("vec_only", 0.9)]);
        let keyword = make_results(vec![("kw_only", 1.0)]);
        let out = fuse(vec![vector, keyword], &FusionStrategy::VectorOnly, 10).unwrap();
        let ids: Vec<_> = out.iter().map(|(id, _)| *id).collect();
        assert_eq!(ids, vec!["vec_only"]);
    }

    #[test]
    fn keyword_only_two_sources_returns_only_keyword_source() {
        let vector = make_results(vec![("vec_only", 0.9)]);
        let keyword = make_results(vec![("kw_only", 1.0)]);
        let out = fuse(vec![vector, keyword], &FusionStrategy::KeywordOnly, 10).unwrap();
        let ids: Vec<_> = out.iter().map(|(id, _)| *id).collect();
        assert_eq!(ids, vec!["kw_only"]);
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

    #[test]
    fn weighted_rrf_worked_example_and_weight_swap() {
        let first = make_results(vec![("a", 1.0), ("b", 0.5)]);
        let second = make_results(vec![("b", 1.0), ("a", 0.5)]);

        let weighted = FusionStrategy::try_weighted_rrf(10, vec![1.0, 3.0]).unwrap();
        let fused = fuse(vec![first.clone(), second.clone()], &weighted, 10).unwrap();
        assert_eq!(fused[0].0, "b");
        assert!((fused[0].1.to_f64() - (1.0 / 12.0 + 3.0 / 11.0)).abs() < 1e-8);

        let swapped = FusionStrategy::try_weighted_rrf(10, vec![3.0, 1.0]).unwrap();
        let fused = fuse(vec![first, second], &swapped, 10).unwrap();
        assert_eq!(fused[0].0, "a");
    }

    #[test]
    fn weighted_rrf_all_ones_matches_rrf_and_common_scale_doubles_scores() {
        let sources = vec![
            make_results(vec![("a", 0.9), ("b", 0.8), ("a", 0.7)]),
            make_results(vec![("b", 0.9), ("a", 0.8)]),
            Vec::new(),
        ];
        let rrf = fuse(sources.clone(), &FusionStrategy::Rrf { k: 20 }, 10).unwrap();
        let all_ones = FusionStrategy::try_weighted_rrf(20, vec![1.0, 1.0, 1.0]).unwrap();
        let weighted = fuse(sources.clone(), &all_ones, 10).unwrap();
        assert_eq!(weighted, rrf);

        let scaled = FusionStrategy::try_weighted_rrf(20, vec![2.0, 2.0, 2.0]).unwrap();
        let doubled = fuse(sources, &scaled, 10).unwrap();
        for ((id, score), (doubled_id, doubled_score)) in weighted.iter().zip(&doubled) {
            assert_eq!(id, doubled_id);
            assert_eq!(doubled_score.to_raw(), score.to_raw() * 2);
        }
    }

    #[test]
    fn weighted_rrf_keeps_empty_slots_and_duplicate_best_rank() {
        let sources = vec![
            make_results(vec![("a", 1.0), ("b", 0.5), ("a", 0.25)]),
            Vec::new(),
            make_results(vec![("c", 1.0)]),
        ];
        let strategy = FusionStrategy::try_weighted_rrf(10, vec![1.0, 2.0, 3.0]).unwrap();
        let fused = fuse(sources, &strategy, 10).unwrap();

        let score_a = fused.iter().find(|(id, _)| *id == "a").unwrap().1.to_f64();
        let score_b = fused.iter().find(|(id, _)| *id == "b").unwrap().1.to_f64();
        let score_c = fused.iter().find(|(id, _)| *id == "c").unwrap().1.to_f64();
        assert!((score_a - 1.0 / 11.0).abs() < 1e-9);
        assert!((score_b - 1.0 / 12.0).abs() < 1e-9);
        assert!((score_c - 3.0 / 11.0).abs() < 1e-9);
        assert_eq!(fused[0].0, "c");
    }

    #[test]
    fn weighted_rrf_ties_use_ascending_id() {
        let sources = vec![
            make_results(vec![("z", 1.0)]),
            make_results(vec![("a", 1.0)]),
        ];
        let strategy = FusionStrategy::try_weighted_rrf(10, vec![1.0, 1.0]).unwrap();
        let fused = fuse(sources, &strategy, 10).unwrap();
        assert_eq!(fused[0].0, "a");
        assert_eq!(fused[0].1, fused[1].1);
    }

    #[test]
    fn weighted_rrf_rejects_weight_count_mismatch_even_for_empty_results() {
        let strategy = FusionStrategy::try_weighted_rrf(10, vec![1.0]).unwrap();
        let error = fuse(
            vec![Vec::<(&str, DeterministicScore)>::new(), Vec::new()],
            &strategy,
            0,
        )
        .unwrap_err();
        assert_eq!(
            error,
            FuseError::WeightedRrfWeightCountMismatch {
                source_count: 2,
                weight_count: 1
            }
        );
    }

    #[test]
    fn weighted_rrf_rejects_deterministic_score_overflow() {
        let source = make_results(vec![("a", 1.0)]);
        let strategy = FusionStrategy::try_weighted_rrf(1, vec![f64::MAX]).unwrap();
        assert_eq!(
            fuse(vec![source], &strategy, 10),
            Err(FuseError::WeightedRrfScoreOverflow)
        );

        let source_a = make_results(vec![("a", 1.0)]);
        let source_b = make_results(vec![("a", 1.0)]);
        let weight = 4_294_967_294.0;
        let strategy = FusionStrategy::try_weighted_rrf(1, vec![weight, weight]).unwrap();
        assert_eq!(
            fuse(vec![source_a, source_b], &strategy, 10),
            Err(FuseError::WeightedRrfScoreOverflow)
        );
    }
}
