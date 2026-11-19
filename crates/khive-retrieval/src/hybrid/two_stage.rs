//! Checked engine fusion followed by vector/text fusion (ADR-031 Amendment 5).

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::hash::Hash;

use khive_fusion::{fuse, FusionStrategy};
use khive_score::{weighted_sum, DeterministicScore};
use serde_json::Value;

use crate::{Result, RetrievalError};

/// Fuse ordered engine arms, then exactly `[combined_vector, text]`, without a final limit.
///
/// Callers bound each input arm upstream and apply pack scoring and the final limit downstream.
/// Empty engine arms retain their weight positions. Engine fusion accepts only `Rrf` and
/// `WeightedRrf`; `None` is required for `KeywordOnly` and refused for every vector-using mode.
/// Both stages validate parameters even with no hits. Each RRF stage has its own positive `k`.
/// Linear hybrid weights and hybrid RRF weights must have exactly two positive finite entries.
///
/// Rank fusion counts an ID once per source, at its first (best) rank, preserving the original
/// positions of later IDs. Linear fusion uses per-source min-max normalization and maximum
/// contribution per ID. Tiny linear weights may quantize to zero, but their candidates remain
/// in the result. Linear input scores must be finite (no score sentinels). Fused results sort by descending score, then ascending ID. `KeywordOnly`
/// preserves text order and its first occurrence per ID; `VectorOnly` returns the fused vector
/// arm. These single-modality modes intentionally exclude the unused modality.
///
/// For `Custom`, `resolve_custom` receives the name, unchanged parameters, both modality slots
/// (including empty slots), and the distinct candidate-union size as the executor's limit.
/// Return `None` for an unknown name, or a future adapting an existing executor. Lookup occurs
/// even on empty input; errors propagate unchanged, without a fallback. The executor owns its
/// output's scoring, ordering and candidate retention; this helper adds no truncation. Built-in
/// strategies never call the resolver. No runtime registry or runtime dependency is introduced.
pub async fn fuse_two_stage<Id, Resolve, Execution>(
    engine_arms: Vec<Vec<(Id, DeterministicScore)>>,
    engine_strategy: Option<&FusionStrategy>,
    text: Vec<(Id, DeterministicScore)>,
    hybrid_strategy: &FusionStrategy,
    resolve_custom: Resolve,
) -> Result<Vec<(Id, DeterministicScore)>>
where
    Id: Eq + Hash + Clone + Ord,
    Resolve: FnOnce(String, Value, [Vec<(Id, DeterministicScore)>; 2], usize) -> Option<Execution>,
    Execution: Future<Output = Result<Vec<(Id, DeterministicScore)>>>,
{
    // Validate the entire configuration before arithmetic, dispatch, or an empty-result return.
    let keyword_only = matches!(hybrid_strategy, FusionStrategy::KeywordOnly);
    let checked_engine = match (keyword_only, engine_strategy) {
        (true, None) => None,
        (true, Some(_)) => return Err(invalid("engine", "must be absent for KeywordOnly")),
        (false, None) => return Err(invalid("engine", "is required for this hybrid strategy")),
        (false, Some(strategy)) => {
            if !matches!(
                strategy,
                FusionStrategy::Rrf { .. } | FusionStrategy::WeightedRrf { .. }
            ) {
                return Err(invalid("engine", "accepts only Rrf or WeightedRrf"));
            }
            Some(checked_strategy(strategy, engine_arms.len(), "engine")?)
        }
    };
    let checked_hybrid = checked_strategy(hybrid_strategy, 2, "hybrid")?;
    let vector = match checked_engine {
        Some(strategy) => checked_fuse(engine_arms, &strategy, "engine")?,
        None => Vec::new(),
    };
    let sources = [vector, text];
    match checked_hybrid {
        FusionStrategy::Custom { name, params } => {
            let limit = union_size(&sources);
            let execution = resolve_custom(name.clone(), params, sources, limit)
                .ok_or_else(|| invalid("hybrid", &format!("unknown custom strategy '{name}'")))?;
            execution.await
        }
        FusionStrategy::Weighted { weights } => checked_linear(sources, &weights),
        FusionStrategy::KeywordOnly => {
            let [_, text] = sources;
            let mut seen = HashSet::new();
            Ok(text
                .into_iter()
                .filter(|(id, _)| seen.insert(id.clone()))
                .collect())
        }
        strategy => checked_fuse(Vec::from(sources), &strategy, "hybrid"),
    }
}

fn invalid(stage: &str, message: &str) -> RetrievalError {
    RetrievalError::Fusion(format!("{stage} fusion: {message}"))
}

fn checked_strategy(
    strategy: &FusionStrategy,
    arm_count: usize,
    stage: &str,
) -> Result<FusionStrategy> {
    match strategy {
        FusionStrategy::Rrf { k } | FusionStrategy::WeightedRrf { k, .. } if *k == 0 => {
            return Err(invalid(stage, "RRF k must be at least 1"));
        }
        FusionStrategy::Weighted { weights } | FusionStrategy::WeightedRrf { weights, .. } => {
            if weights.len() != arm_count {
                return Err(invalid(
                    stage,
                    "requires exactly one weight per ordered source slot",
                ));
            }
            if weights
                .iter()
                .any(|weight| !weight.is_finite() || *weight <= 0.0)
            {
                return Err(invalid(
                    stage,
                    "weights must be finite and strictly positive",
                ));
            }
        }
        FusionStrategy::Custom { name, .. } if name.is_empty() => {
            return Err(invalid(stage, "custom strategy name must not be empty"));
        }
        _ => {}
    }
    // Use the existing checked rank arithmetic for ordinary RRF too, without its legacy clamp.
    Ok(match strategy {
        FusionStrategy::Rrf { k } => FusionStrategy::WeightedRrf {
            k: *k,
            weights: vec![1.0; arm_count],
        },
        _ => strategy.clone(),
    })
}

fn union_size<Id: Eq + Hash>(sources: &[Vec<(Id, DeterministicScore)>]) -> usize {
    sources
        .iter()
        .flatten()
        .map(|(id, _)| id)
        .collect::<HashSet<_>>()
        .len()
}

fn checked_fuse<Id: Eq + Hash + Clone + Ord>(
    sources: Vec<Vec<(Id, DeterministicScore)>>,
    strategy: &FusionStrategy,
    stage: &str,
) -> Result<Vec<(Id, DeterministicScore)>> {
    // The legacy rank primitive maps a k + rank overflow to zero. Refuse it here instead.
    if let FusionStrategy::Rrf { k } | FusionStrategy::WeightedRrf { k, .. } = strategy {
        if sources
            .iter()
            .any(|source| k.checked_add(source.len()).is_none())
        {
            return Err(invalid(stage, "RRF k plus source rank exceeds usize"));
        }
    }
    let limit = union_size(&sources);
    fuse(sources, strategy, limit).map_err(|error| invalid(stage, &error.to_string()))
}

fn checked_linear<Id: Eq + Hash + Clone + Ord>(
    sources: [Vec<(Id, DeterministicScore)>; 2],
    weights: &[f64],
) -> Result<Vec<(Id, DeterministicScore)>> {
    // Scaling first avoids overflowing the sum and accidentally choosing uniform weights.
    let max_weight = weights[0].max(weights[1]);
    let scaled = [weights[0] / max_weight, weights[1] / max_weight];
    let sum = scaled[0] + scaled[1];
    let normalized_weights = [scaled[0] / sum, scaled[1] / sum];
    let scale = i128::from(DeterministicScore::from_f64(1.0).to_raw());
    let mut combined: HashMap<Id, i64> = HashMap::new();
    for (source, weight) in sources.into_iter().zip(normalized_weights) {
        let Some(min) = source.iter().map(|(_, score)| score.to_raw()).min() else {
            continue;
        };
        let max = source
            .iter()
            .map(|(_, score)| score.to_raw())
            .max()
            .expect("nonempty source");
        let span = i128::from(max) - i128::from(min);
        let mut best: HashMap<Id, DeterministicScore> = HashMap::new();
        for (id, score) in source {
            let raw = score.to_raw();
            if raw <= DeterministicScore::NEG_INF.to_raw()
                || raw == DeterministicScore::MAX.to_raw()
            {
                return Err(invalid("hybrid", "linear input scores must be finite"));
            }
            // Same min-max and equal-source rule as weighted_fusion, with checked accumulation.
            let normalized = if span == 0 {
                scale
            } else {
                (i128::from(raw) - i128::from(min)) * scale / span
            };
            let score = DeterministicScore::from_raw(normalized as i64);
            best.entry(id)
                .and_modify(|held| *held = (*held).max(score))
                .or_insert(score);
        }
        for (id, score) in best {
            let contribution = weighted_sum(&[score], &[weight])
                .map_err(|error| invalid("hybrid", &error.to_string()))?
                .to_raw();
            let held = combined.entry(id).or_default();
            *held = held
                .checked_add(contribution)
                .filter(|raw| *raw < i64::MAX && *raw > DeterministicScore::NEG_INF.to_raw())
                .ok_or_else(|| invalid("hybrid", "linear score exceeds the finite score range"))?;
        }
    }
    let mut results: Vec<_> = combined
        .into_iter()
        .map(|(id, raw)| (id, DeterministicScore::from_raw(raw)))
        .collect();
    results.sort_by(|(a_id, a_score), (b_id, b_score)| {
        b_score.cmp(a_score).then_with(|| a_id.cmp(b_id))
    });
    Ok(results)
}
