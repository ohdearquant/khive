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
#[path = "fuse_tests.rs"]
mod tests;
