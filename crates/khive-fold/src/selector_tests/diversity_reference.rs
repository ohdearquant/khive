//! Frozen reference from main 0dfd6caa for exact differential selection tests.

use super::*;

pub(super) struct ReferenceSelector;

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

fn effective_score<T>(
    item: &SelectorInput<T>,
    counts: &std::collections::BTreeMap<String, usize>,
    bias: f32,
    epistemic_weight: f32,
) -> f64 {
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
    base * (1.0 - bias as f64 * count as f64 / (count as f64 + 1.0))
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

impl<T: Clone> Selector<T> for ReferenceSelector {
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
            // Diversity path: pick-best-remaining with per-step effective score.
            let mut remaining = inputs;
            let mut category_counts: std::collections::BTreeMap<String, usize> =
                std::collections::BTreeMap::new();

            while !remaining.is_empty() {
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
                    candidates.push((i, DeterministicScore::from_f64(eff)));
                }

                let best_idx = candidates
                    .into_iter()
                    .max_by(|&(i, a_det), &(j, b_det)| {
                        a_det
                            .cmp(&b_det)
                            .then_with(|| remaining[j].size.cmp(&remaining[i].size))
                            .then_with(|| remaining[j].id.cmp(&remaining[i].id))
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
