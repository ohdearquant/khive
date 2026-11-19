//! Selector: many → subset under budget.
//!
//! Collapses a set of inputs into a compressed representation that fits a
//! target budget (tokens, bytes, count). Pure in-memory, synchronous collapse.

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

use khive_score::DeterministicScore;

use crate::error::FoldError;

mod diversity;

/// A single input item to a selector operation.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct SelectorInput<T> {
    /// Stable string identifier for deterministic tie-breaking.
    pub id: String,
    /// The item payload carried through selection.
    pub content: T,
    /// Size in the unit of the caller's budget (tokens, bytes, count).
    pub size: usize,
    /// Pre-computed relevance score.
    pub score: f32,
    /// Optional category for diversity and category-weight scoring.
    #[cfg_attr(feature = "serde", serde(default))]
    pub category: Option<String>,
    /// Pre-computed information gain (KL divergence proxy) for this candidate.
    ///
    /// Defaults to 0.0 when `None`. Only affects selection when
    /// `SelectorWeights.epistemic_weight > 0.0`. See
    /// crates/khive-fold/docs/design.md#adr-024-bayesian-extensions-selector-budget-packing-and-precision-weighted-scoring
    /// for why this is caller-supplied rather than computed by the Selector.
    #[cfg_attr(feature = "serde", serde(default))]
    pub information_gain: Option<f32>,
    /// Higher-precision pre-conversion effective score used for rank
    /// comparisons, when available.
    ///
    /// When `None`, ranking widens `score` back to `f64` via
    /// `DeterministicScore::from_f32`. Does not affect `min_score` filtering,
    /// which always operates on `score`. `category_weights` multipliers scale
    /// `rank_score` by the same weight applied to `score`. See
    /// crates/khive-fold/docs/api/selector.md#rank-score-precision-pr-535 for
    /// why callers need this instead of relying on the narrowed `score` field.
    #[cfg_attr(feature = "serde", serde(default))]
    pub rank_score: Option<f64>,
}

/// Result of a selector operation.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct SelectorOutput<T> {
    /// Selected inputs in final order.
    pub selected: Vec<SelectorInput<T>>,
    /// Total budget consumed.
    pub total_size: usize,
    /// Budget cap the caller requested.
    pub budget: usize,
}

/// Learned weights that a selector implementation may use.
///
/// Callers persist this across sessions.
#[derive(Debug, Clone, Default)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct SelectorWeights {
    /// Weight multiplier by input category.
    pub category_weights: std::collections::BTreeMap<String, f32>,
    /// Minimum score threshold (inputs below this are excluded even if budget allows).
    pub min_score: f32,
    /// Preference for diversity vs. relevance (0.0 = pure relevance, 1.0 = pure diversity).
    /// Values outside the inclusive `[0.0, 1.0]` range are rejected.
    pub diversity_bias: f32,
    /// Weight for epistemic (uncertainty-reducing) selection.
    ///
    /// The effective selection score is `pragmatic_score + epistemic_weight * information_gain`.
    /// Default 0.0 (pure pragmatic). Higher values prefer candidates that reduce uncertainty.
    /// When 0.0, behavior is identical to pure pragmatic selection (backwards-compatible).
    #[cfg_attr(feature = "serde", serde(default))]
    pub epistemic_weight: f32,
}

/// The Selector primitive.
///
/// An implementation collapses N inputs into a subset that fits a budget,
/// using weights and an optional query for relevance context.
pub trait Selector<T>: Send + Sync {
    /// Select a budget-constrained subset from `inputs`.
    fn select(
        &self,
        inputs: Vec<SelectorInput<T>>,
        budget: usize,
        weights: &SelectorWeights,
    ) -> Result<SelectorOutput<T>, FoldError>;
}

// ── GreedySelector ──────────────────────────────────────────────────────────

/// Budget-constrained greedy packer.
///
/// Filters by `SelectorWeights.min_score`, applies `category_weights` multipliers
/// to adjust scores, then greedily packs until the budget is exhausted. When
/// `diversity_bias > 0`, uses indexed category winners instead of a single
/// sort pass. Tie-breaking is deterministic: effective score descending, size
/// ascending, then id ascending. This is an ordering heuristic, not a knapsack
/// optimizer, and provides no optimality or approximation-ratio guarantee. See
/// crates/khive-fold/docs/design.md#adr-024-bayesian-extensions-selector-budget-packing-and-precision-weighted-scoring
/// for the diversity-penalty formula.
#[derive(Debug, Clone, Copy, Default)]
pub struct GreedySelector;

/// Widen `score` (or `rank_score` if set) to `f64` for rank comparisons.
/// See crates/khive-fold/docs/api/selector.md#rank-score-precision-pr-535.
#[inline]
fn rank_base<T>(item: &SelectorInput<T>) -> f64 {
    item.rank_score.unwrap_or(item.score as f64)
}

/// Pragmatic score plus the epistemic (information-gain) bonus, in `f64`.
#[inline]
fn pragmatic_plus_epistemic<T>(item: &SelectorInput<T>, epistemic_weight: f32) -> f64 {
    let base = rank_base(item);
    if epistemic_weight == 0.0 {
        return base;
    }
    base + epistemic_weight as f64 * item.information_gain.unwrap_or(0.0) as f64
}

fn validate_selector_weights(weights: &SelectorWeights) -> Result<(), FoldError> {
    if !weights.min_score.is_finite() {
        return Err(FoldError::InvalidInput(
            "SelectorWeights.min_score must be finite".to_string(),
        ));
    }
    if !weights.diversity_bias.is_finite() {
        return Err(FoldError::InvalidInput(
            "SelectorWeights.diversity_bias must be finite".to_string(),
        ));
    }
    if !(0.0..=1.0).contains(&weights.diversity_bias) {
        return Err(FoldError::InvalidInput(
            "SelectorWeights.diversity_bias must be within [0.0, 1.0]".to_string(),
        ));
    }
    if !weights.epistemic_weight.is_finite() {
        return Err(FoldError::InvalidInput(
            "SelectorWeights.epistemic_weight must be finite".to_string(),
        ));
    }
    for (category, weight) in &weights.category_weights {
        if !weight.is_finite() {
            return Err(FoldError::InvalidInput(format!(
                "SelectorWeights.category_weights['{category}'] must be finite"
            )));
        }
    }
    Ok(())
}

impl<T: Clone> Selector<T> for GreedySelector {
    fn select(
        &self,
        mut inputs: Vec<SelectorInput<T>>,
        budget: usize,
        weights: &SelectorWeights,
    ) -> Result<SelectorOutput<T>, FoldError> {
        validate_selector_weights(weights)?;
        for input in &inputs {
            if let Some(gain) = input.information_gain {
                if !gain.is_finite() {
                    return Err(FoldError::InvalidInput(format!(
                        "information_gain for '{}' must be finite",
                        input.id
                    )));
                }
            }
            if let Some(rank_score) = input.rank_score {
                if !rank_score.is_finite() {
                    return Err(FoldError::InvalidInput(format!(
                        "rank_score for '{}' must be finite",
                        input.id
                    )));
                }
            }
        }

        // Filter non-finite and below min_score.
        inputs.retain(|i| i.score.is_finite() && i.score >= weights.min_score);

        // rank_score must be scaled by the same category weight as score, or
        // it silently defeats category_weights (khive PR #535; see design.md).
        if !weights.category_weights.is_empty() {
            for item in &mut inputs {
                if let Some(ref cat) = item.category {
                    if let Some(&w) = weights.category_weights.get(cat.as_str()) {
                        let w = w.max(0.0);
                        item.score *= w;
                        if let Some(rank_score) = item.rank_score {
                            item.rank_score = Some(rank_score * w as f64);
                        }
                    }
                }
            }
            inputs.retain(|i| i.score.is_finite() && i.score >= weights.min_score);
        }

        let ew = weights.epistemic_weight;

        // Sort by effective score desc, size asc, id asc via DeterministicScore
        // (not raw f32::total_cmp) — see design.md#rank-score-precision-pr-535.
        let mut ranked = Vec::with_capacity(inputs.len());
        for input in inputs {
            let effective = pragmatic_plus_epistemic(&input, ew);
            if !effective.is_finite() {
                return Err(FoldError::InvalidInput(format!(
                    "effective score for '{}' must be finite",
                    input.id
                )));
            }
            let det_score = DeterministicScore::from_f64(effective);
            ranked.push((input, det_score));
        }
        ranked.sort_by(|(a, a_det), (b, b_det)| {
            b_det
                .cmp(a_det)
                .then_with(|| a.size.cmp(&b.size))
                .then_with(|| a.id.cmp(&b.id))
        });
        let inputs: Vec<_> = ranked.into_iter().map(|(input, _)| input).collect();

        let mut selected = Vec::new();
        let mut total_size = 0usize;

        if weights.diversity_bias == 0.0 {
            // Fast path: single-pass greedy.
            for input in inputs {
                if input.size <= budget.saturating_sub(total_size) {
                    total_size += input.size;
                    selected.push(input);
                }
            }
        } else {
            let indices = diversity::select(&inputs, budget, weights);
            let mut inputs: Vec<_> = inputs.into_iter().map(Some).collect();
            for index in indices {
                let input = inputs[index].take().expect("selected once");
                total_size += input.size;
                selected.push(input);
            }
        }

        Ok(SelectorOutput {
            selected,
            total_size,
            budget,
        })
    }
}

// INLINE TEST JUSTIFICATION: selector tests exercise private helper functions
// (pragmatic_plus_epistemic, effective_score) and internal sort logic that are
// not accessible from a separate crate-level tests/ file. Consolidating here
// avoids duplicating the SelectorInput construction scaffolding.
#[cfg(test)]
#[path = "selector_tests.rs"]
mod tests;
