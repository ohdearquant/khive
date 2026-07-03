//! Selector: many → subset under budget.
//!
//! Collapses a set of inputs into a compressed representation that fits a
//! target budget (tokens, bytes, count). Pure in-memory, synchronous collapse.

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

use crate::error::FoldError;

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
    /// Callers pre-compute this because the Selector is pure-math and has no
    /// access to the embedding space required to estimate KL divergence. When
    /// `None` (the default), the value is treated as 0.0. Only has an effect
    /// when `SelectorWeights.epistemic_weight > 0.0`.
    #[cfg_attr(feature = "serde", serde(default))]
    pub information_gain: Option<f32>,
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
/// to adjust scores, then greedily packs until the budget is exhausted.
///
/// When `diversity_bias > 0`, uses a pick-best-remaining loop: at each step the
/// item with the highest *effective* score (after diversity penalty) is selected.
/// The penalty is `score * (1 - bias * n / (n + 1))` where `n` is the number of
/// already-selected items in the same category. At bias=0 this collapses to a
/// single-pass sort (backward-compatible).
///
/// Tie-breaking is deterministic: size ascending, then id ascending.
#[derive(Debug, Clone, Copy, Default)]
pub struct GreedySelector;

/// Compute the base pragmatic score adjusted for epistemic weight.
///
/// `base` is the pragmatic score (after category-weight multipliers).
/// `epistemic_weight * information_gain` is the epistemic bonus.
#[inline]
fn pragmatic_plus_epistemic<T>(item: &SelectorInput<T>, epistemic_weight: f32) -> f32 {
    if epistemic_weight == 0.0 {
        return item.score;
    }
    item.score + epistemic_weight * item.information_gain.unwrap_or(0.0)
}

fn effective_score<T>(
    item: &SelectorInput<T>,
    counts: &std::collections::BTreeMap<String, usize>,
    bias: f32,
    epistemic_weight: f32,
) -> f32 {
    let base = pragmatic_plus_epistemic(item, epistemic_weight);
    if bias == 0.0 {
        return base;
    }
    let count = item
        .category
        .as_ref()
        .and_then(|c| counts.get(c))
        .copied()
        .unwrap_or(0);
    base * (1.0 - bias * count as f32 / (count as f32 + 1.0))
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
        }

        // Filter non-finite and below min_score.
        inputs.retain(|i| i.score.is_finite() && i.score >= weights.min_score);

        // Apply category_weights multipliers.
        if !weights.category_weights.is_empty() {
            for item in &mut inputs {
                if let Some(ref cat) = item.category {
                    if let Some(&w) = weights.category_weights.get(cat.as_str()) {
                        item.score *= w.max(0.0);
                    }
                }
            }
            inputs.retain(|i| i.score.is_finite() && i.score >= weights.min_score);
        }

        let ew = weights.epistemic_weight;

        // Initial sort: effective score (pragmatic + epistemic bonus) desc, size asc, id asc —
        // deterministic across platforms. Non-finite effective scores are rejected rather
        // than silently sorted, since a checked score can still overflow to non-finite.
        let mut ranked = Vec::with_capacity(inputs.len());
        for input in inputs {
            let effective = pragmatic_plus_epistemic(&input, ew);
            if !effective.is_finite() {
                return Err(FoldError::InvalidInput(format!(
                    "effective score for '{}' must be finite",
                    input.id
                )));
            }
            ranked.push((input, effective));
        }
        ranked.sort_by(|(a, a_eff), (b, b_eff)| {
            b_eff
                .total_cmp(a_eff)
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
            // Diversity path: pick-best-remaining with per-step effective score.
            let mut remaining = inputs;
            let mut category_counts: std::collections::BTreeMap<String, usize> =
                std::collections::BTreeMap::new();

            while !remaining.is_empty() && total_size < budget {
                let mut candidates = Vec::with_capacity(remaining.len());
                for (i, item) in remaining.iter().enumerate() {
                    if item.size > budget.saturating_sub(total_size) {
                        continue;
                    }
                    let eff = effective_score(item, &category_counts, weights.diversity_bias, ew);
                    if !eff.is_finite() {
                        return Err(FoldError::InvalidInput(format!(
                            "effective score for '{}' must be finite",
                            item.id
                        )));
                    }
                    candidates.push((i, eff));
                }

                let best_idx = candidates
                    .into_iter()
                    .max_by(|&(i, a_eff), &(j, b_eff)| {
                        a_eff
                            .total_cmp(&b_eff)
                            .then_with(|| remaining[j].size.cmp(&remaining[i].size))
                            .then_with(|| remaining[i].id.cmp(&remaining[j].id))
                    })
                    .map(|(i, _)| i);

                match best_idx {
                    Some(idx) => {
                        let item = remaining.swap_remove(idx);
                        if let Some(ref cat) = item.category {
                            *category_counts.entry(cat.clone()).or_default() += 1;
                        }
                        total_size += item.size;
                        selected.push(item);
                    }
                    None => break,
                }
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
mod tests {
    use super::*;

    fn input(id: &str, size: usize, score: f32) -> SelectorInput<()> {
        SelectorInput {
            id: id.to_string(),
            content: (),
            size,
            score,
            category: None,
            information_gain: None,
        }
    }

    fn input_cat(id: &str, size: usize, score: f32, cat: &str) -> SelectorInput<()> {
        SelectorInput {
            id: id.to_string(),
            content: (),
            size,
            score,
            category: Some(cat.to_string()),
            information_gain: None,
        }
    }

    fn weights(min_score: f32) -> SelectorWeights {
        SelectorWeights {
            min_score,
            ..Default::default()
        }
    }

    #[test]
    fn empty_input() {
        let inputs: Vec<SelectorInput<()>> = vec![];
        let out = GreedySelector.select(inputs, 1000, &weights(0.0)).unwrap();
        assert!(out.selected.is_empty());
        assert_eq!(out.total_size, 0);
        assert_eq!(out.budget, 1000);
    }

    #[test]
    fn packs_highest_scores_first() {
        let inputs = vec![
            input("a", 100, 0.5),
            input("b", 100, 0.9),
            input("c", 100, 0.7),
        ];
        let out = GreedySelector.select(inputs, 200, &weights(0.0)).unwrap();
        assert_eq!(out.selected.len(), 2);
        assert_eq!(out.selected[0].id, "b");
        assert_eq!(out.selected[1].id, "c");
        assert_eq!(out.total_size, 200);
    }

    #[test]
    fn respects_budget() {
        let inputs = vec![
            input("a", 300, 0.9),
            input("b", 300, 0.8),
            input("c", 300, 0.7),
        ];
        let out = GreedySelector.select(inputs, 500, &weights(0.0)).unwrap();
        assert_eq!(out.selected.len(), 1);
        assert_eq!(out.selected[0].id, "a");
        assert_eq!(out.total_size, 300);
    }

    #[test]
    fn filters_below_min_score() {
        let inputs = vec![
            input("a", 10, 0.8),
            input("b", 10, 0.1),
            input("c", 10, 0.5),
        ];
        let out = GreedySelector.select(inputs, 1000, &weights(0.3)).unwrap();
        assert_eq!(out.selected.len(), 2);
        assert_eq!(out.selected[0].id, "a");
        assert_eq!(out.selected[1].id, "c");
    }

    #[test]
    fn filters_nan_and_inf() {
        let inputs = vec![
            input("nan", 10, f32::NAN),
            input("inf", 10, f32::INFINITY),
            input("neg_inf", 10, f32::NEG_INFINITY),
            input("ok", 10, 0.5),
        ];
        let out = GreedySelector.select(inputs, 1000, &weights(0.0)).unwrap();
        assert_eq!(out.selected.len(), 1);
        assert_eq!(out.selected[0].id, "ok");
    }

    #[test]
    fn tie_break_size_ascending() {
        let inputs = vec![input("big", 200, 0.5), input("small", 50, 0.5)];
        let out = GreedySelector.select(inputs, 1000, &weights(0.0)).unwrap();
        assert_eq!(out.selected[0].id, "small");
        assert_eq!(out.selected[1].id, "big");
    }

    #[test]
    fn tie_break_id_ascending() {
        let inputs = vec![input("z", 100, 0.5), input("a", 100, 0.5)];
        let out = GreedySelector.select(inputs, 1000, &weights(0.0)).unwrap();
        assert_eq!(out.selected[0].id, "a");
        assert_eq!(out.selected[1].id, "z");
    }

    #[test]
    fn skips_oversized_items_takes_smaller() {
        let inputs = vec![
            input("huge", 900, 0.9),
            input("small1", 40, 0.3),
            input("small2", 40, 0.2),
        ];
        let out = GreedySelector.select(inputs, 100, &weights(0.0)).unwrap();
        assert_eq!(out.selected.len(), 2);
        assert_eq!(out.selected[0].id, "small1");
        assert_eq!(out.selected[1].id, "small2");
        assert_eq!(out.total_size, 80);
    }

    #[test]
    fn zero_budget() {
        let inputs = vec![input("a", 1, 0.9)];
        let out = GreedySelector.select(inputs, 0, &weights(0.0)).unwrap();
        assert!(out.selected.is_empty());
    }

    #[test]
    fn deterministic_across_input_order() {
        let a = vec![
            input("x", 50, 0.7),
            input("y", 50, 0.7),
            input("z", 50, 0.7),
        ];
        let b = vec![
            input("z", 50, 0.7),
            input("x", 50, 0.7),
            input("y", 50, 0.7),
        ];
        let out_a = GreedySelector.select(a, 100, &weights(0.0)).unwrap();
        let out_b = GreedySelector.select(b, 100, &weights(0.0)).unwrap();
        let ids_a: Vec<&str> = out_a.selected.iter().map(|i| i.id.as_str()).collect();
        let ids_b: Vec<&str> = out_b.selected.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids_a, ids_b);
        assert_eq!(ids_a, vec!["x", "y"]);
    }

    #[test]
    fn exact_budget_fit() {
        let inputs = vec![input("a", 50, 0.9), input("b", 50, 0.8)];
        let out = GreedySelector.select(inputs, 100, &weights(0.0)).unwrap();
        assert_eq!(out.selected.len(), 2);
        assert_eq!(out.total_size, 100);
    }

    #[test]
    fn category_weights_boost_preferred_category() {
        let inputs = vec![
            input_cat("a", 100, 0.9, "low"),
            input_cat("b", 100, 0.5, "high"),
        ];
        let w = SelectorWeights {
            category_weights: [("high".to_string(), 2.0f32), ("low".to_string(), 1.0f32)]
                .into_iter()
                .collect(),
            ..Default::default()
        };
        let out = GreedySelector.select(inputs, 100, &w).unwrap();
        assert_eq!(out.selected.len(), 1);
        assert_eq!(out.selected[0].id, "b");
    }

    #[test]
    fn category_weights_can_push_below_min_score() {
        let inputs = vec![
            input_cat("a", 10, 0.4, "bad"),
            input_cat("b", 10, 0.8, "good"),
        ];
        let w = SelectorWeights {
            min_score: 0.3,
            category_weights: [("bad".to_string(), 0.5f32)].into_iter().collect(),
            ..Default::default()
        };
        let out = GreedySelector.select(inputs, 1000, &w).unwrap();
        assert_eq!(out.selected.len(), 1);
        assert_eq!(out.selected[0].id, "b");
    }

    #[test]
    fn diversity_bias_zero_identical_to_greedy() {
        let make = || {
            vec![
                input_cat("a", 100, 0.9, "x"),
                input_cat("b", 100, 0.8, "x"),
                input_cat("c", 100, 0.7, "y"),
            ]
        };
        let w_greedy = SelectorWeights {
            ..Default::default()
        };
        let w_bias0 = SelectorWeights {
            diversity_bias: 0.0,
            ..Default::default()
        };
        let out_g = GreedySelector.select(make(), 200, &w_greedy).unwrap();
        let out_b = GreedySelector.select(make(), 200, &w_bias0).unwrap();
        let ids_g: Vec<&str> = out_g.selected.iter().map(|i| i.id.as_str()).collect();
        let ids_b: Vec<&str> = out_b.selected.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids_g, ids_b);
    }

    #[test]
    fn diversity_bias_prefers_different_categories() {
        let inputs = vec![
            input_cat("a", 100, 0.9, "x"),
            input_cat("b", 100, 0.8, "x"),
            input_cat("c", 100, 0.7, "y"),
        ];
        let w = SelectorWeights {
            diversity_bias: 1.0,
            ..Default::default()
        };
        let out = GreedySelector.select(inputs, 200, &w).unwrap();
        assert_eq!(out.selected.len(), 2);
        let ids: Vec<&str> = out.selected.iter().map(|i| i.id.as_str()).collect();
        assert!(ids.contains(&"a"), "a should always be selected");
        assert!(
            ids.contains(&"c"),
            "c should be preferred over b due to diversity"
        );
    }

    #[test]
    fn no_overflow_near_usize_max() {
        // Items with near-usize::MAX sizes must not overflow when checking budget.
        let large = usize::MAX - 1;
        let inputs = vec![
            SelectorInput {
                id: "a".to_string(),
                content: (),
                size: large,
                score: 0.9,
                category: None,
                information_gain: None,
            },
            SelectorInput {
                id: "b".to_string(),
                content: (),
                size: 10,
                score: 0.8,
                category: None,
                information_gain: None,
            },
        ];
        // Budget is 100 — only item "b" fits.
        let out = GreedySelector.select(inputs, 100, &weights(0.0)).unwrap();
        assert_eq!(out.selected.len(), 1);
        assert_eq!(out.selected[0].id, "b");
    }

    #[test]
    fn diversity_bias_no_categories_unaffected() {
        let inputs = vec![
            input("a", 100, 0.9),
            input("b", 100, 0.8),
            input("c", 100, 0.7),
        ];
        let w = SelectorWeights {
            diversity_bias: 1.0,
            ..Default::default()
        };
        let out = GreedySelector.select(inputs, 200, &w).unwrap();
        assert_eq!(out.selected.len(), 2);
        assert_eq!(out.selected[0].id, "a");
        assert_eq!(out.selected[1].id, "b");
    }

    // ── epistemic weight tests ────────────────────────────────────────────────

    fn input_with_gain(id: &str, size: usize, score: f32, gain: f32) -> SelectorInput<()> {
        SelectorInput {
            id: id.to_string(),
            content: (),
            size,
            score,
            category: None,
            information_gain: Some(gain),
        }
    }

    #[test]
    fn epistemic_weight_zero_preserves_behavior() {
        // With epistemic_weight=0, result must be identical to the default (no epistemic).
        let make = || {
            vec![
                input_with_gain("a", 100, 0.9, 10.0),
                input_with_gain("b", 100, 0.8, 0.0),
                input_with_gain("c", 100, 0.7, 5.0),
            ]
        };
        let w_default = SelectorWeights {
            ..Default::default()
        };
        let w_zero = SelectorWeights {
            epistemic_weight: 0.0,
            ..Default::default()
        };
        let out_d = GreedySelector.select(make(), 200, &w_default).unwrap();
        let out_z = GreedySelector.select(make(), 200, &w_zero).unwrap();
        let ids_d: Vec<&str> = out_d.selected.iter().map(|i| i.id.as_str()).collect();
        let ids_z: Vec<&str> = out_z.selected.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids_d, ids_z);
        // Pure score order: a (0.9), b (0.8).
        assert_eq!(ids_d, vec!["a", "b"]);
    }

    #[test]
    fn epistemic_weight_positive_reorders_by_gain() {
        // a: score=0.5, gain=10.0  → effective = 0.5 + 1.0 * 10.0 = 10.5
        // b: score=0.9, gain=0.0   → effective = 0.9 + 1.0 * 0.0  = 0.9
        // With epistemic_weight=1.0, a should be selected first.
        let inputs = vec![
            input_with_gain("a", 100, 0.5, 10.0),
            input_with_gain("b", 100, 0.9, 0.0),
        ];
        let w = SelectorWeights {
            epistemic_weight: 1.0,
            ..Default::default()
        };
        let out = GreedySelector.select(inputs, 100, &w).unwrap();
        assert_eq!(out.selected.len(), 1);
        assert_eq!(out.selected[0].id, "a");
    }

    #[test]
    fn information_gain_none_equivalent_to_zero() {
        // None and Some(0.0) must produce identical ordering.
        let with_none = vec![
            input("a", 100, 0.9), // information_gain: None
            input("b", 100, 0.8),
        ];
        let with_zero = vec![
            input_with_gain("a", 100, 0.9, 0.0),
            input_with_gain("b", 100, 0.8, 0.0),
        ];
        let w = SelectorWeights {
            epistemic_weight: 1.0,
            ..Default::default()
        };
        let out_none = GreedySelector.select(with_none, 200, &w).unwrap();
        let out_zero = GreedySelector.select(with_zero, 200, &w).unwrap();
        let ids_none: Vec<&str> = out_none.selected.iter().map(|i| i.id.as_str()).collect();
        let ids_zero: Vec<&str> = out_zero.selected.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids_none, ids_zero);
    }

    #[test]
    fn epistemic_weight_works_with_diversity_bias() {
        // Combines epistemic and diversity: the effective score incorporates both.
        // a: score=0.5, gain=10.0, category=x → base effective = 0.5 + 1.0 * 10.0 = 10.5
        // b: score=0.8, gain=0.0,  category=x → base effective = 0.8
        // c: score=0.3, gain=0.0,  category=y → base effective = 0.3
        // Budget=200, bias=0.5: a selected first (10.5 wins), then after a is in x,
        // b's diversity penalty is 0.8*(1-0.5*1/2)=0.8*0.75=0.6 vs c at 0.3 — b wins.
        let inputs = vec![
            {
                let mut i = input_with_gain("a", 100, 0.5, 10.0);
                i.category = Some("x".to_string());
                i
            },
            {
                let mut i = input_with_gain("b", 100, 0.8, 0.0);
                i.category = Some("x".to_string());
                i
            },
            {
                let mut i = input_with_gain("c", 100, 0.3, 0.0);
                i.category = Some("y".to_string());
                i
            },
        ];
        let w = SelectorWeights {
            epistemic_weight: 1.0,
            diversity_bias: 0.5,
            ..Default::default()
        };
        let out = GreedySelector.select(inputs, 200, &w).unwrap();
        assert_eq!(out.selected.len(), 2);
        assert_eq!(out.selected[0].id, "a");
        // b (eff=0.8*0.75=0.6) > c (eff=0.3) after a is placed in category x.
        assert_eq!(out.selected[1].id, "b");
    }

    // ── non-finite validation tests (FOLD-AUD-003) ─────────────────────────────

    #[test]
    fn greedy_selector_rejects_nan_information_gain() {
        let inputs = vec![
            input_with_gain("a", 100, 0.1, f32::NAN),
            input_with_gain("b", 100, 0.9, 0.0),
        ];
        let w = SelectorWeights {
            epistemic_weight: 1.0,
            ..Default::default()
        };
        let err = GreedySelector.select(inputs, 100, &w).unwrap_err();
        assert!(matches!(err, FoldError::InvalidInput(_)));
    }

    #[test]
    fn greedy_selector_rejects_non_finite_epistemic_weight() {
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let inputs = vec![input("a", 100, 0.5)];
            let w = SelectorWeights {
                epistemic_weight: bad,
                ..Default::default()
            };
            let err = GreedySelector.select(inputs, 100, &w).unwrap_err();
            assert!(matches!(err, FoldError::InvalidInput(_)));
        }
    }

    #[test]
    fn greedy_selector_rejects_non_finite_diversity_bias() {
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let inputs = vec![input("a", 100, 0.5)];
            let w = SelectorWeights {
                diversity_bias: bad,
                ..Default::default()
            };
            let err = GreedySelector.select(inputs, 100, &w).unwrap_err();
            assert!(matches!(err, FoldError::InvalidInput(_)));
        }
    }

    #[test]
    fn greedy_selector_rejects_non_finite_category_weight() {
        let inputs = vec![input_cat("a", 100, 0.5, "x")];
        let w = SelectorWeights {
            category_weights: [("x".to_string(), f32::NAN)].into_iter().collect(),
            ..Default::default()
        };
        let err = GreedySelector.select(inputs, 100, &w).unwrap_err();
        assert!(matches!(err, FoldError::InvalidInput(_)));
    }

    #[test]
    fn greedy_selector_rejects_non_finite_effective_score_before_compare() {
        let inputs = vec![input_with_gain("a", 100, f32::MAX, f32::MAX)];
        let w = SelectorWeights {
            epistemic_weight: f32::MAX,
            ..Default::default()
        };
        let err = GreedySelector.select(inputs, 100, &w).unwrap_err();
        assert!(matches!(err, FoldError::InvalidInput(_)));
    }
}
