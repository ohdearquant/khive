//! Weighted linear combination fusion with per-source min-max normalization.

use khive_score::{weighted_sum, DeterministicScore};
use std::collections::HashMap;
use std::hash::Hash;

use crate::ordering::cmp_desc_then_id;

/// Min-max normalize scores to `[0, 1]`; equal/single-element sources map to 1.0.
const SCORE_SCALE: i128 = 4_294_967_296; // 2^32 — represents 1.0 in DeterministicScore

fn min_max_normalize_source<Id>(
    source: Vec<(Id, DeterministicScore)>,
) -> Vec<(Id, DeterministicScore)> {
    if source.is_empty() {
        return source;
    }
    let min = source.iter().map(|(_, s)| s.to_raw()).min().unwrap();
    let max = source.iter().map(|(_, s)| s.to_raw()).max().unwrap();
    let span = (max as i128) - (min as i128);
    if span <= 0 {
        return source
            .into_iter()
            .map(|(id, _)| (id, DeterministicScore::from_raw(SCORE_SCALE as i64)))
            .collect();
    }
    source
        .into_iter()
        .map(|(id, s)| {
            let numerator = (s.to_raw() as i128 - min as i128) * SCORE_SCALE;
            let normalized_raw = (numerator / span).clamp(0, i64::MAX as i128);
            (id, DeterministicScore::from_raw(normalized_raw as i64))
        })
        .collect()
}

/// Fuse per-source min-max-normalized scores with lossy normalized weights.
///
/// Negative/non-finite weights become zero; a zero or overflowing effective sum gives equal
/// weight to covered source positions, or to all sources when no weights are provided. Results
/// sort by descending score, breaking ties by ascending ID. See
/// `crates/khive-fusion/docs/api/fusion-functions.md`.
pub fn weighted_fusion<Id: Eq + Hash + Clone + Ord>(
    sources: Vec<Vec<(Id, DeterministicScore)>>,
    weights: &[f64],
) -> Vec<(Id, DeterministicScore)> {
    if sources.is_empty() {
        return Vec::new();
    }

    // Sanitize before fixed-point conversion so NaN/Inf cannot enter arithmetic.
    let sanitized: Vec<f64> = weights
        .iter()
        .map(|&w| if w.is_finite() && w > 0.0 { w } else { 0.0 })
        .collect();

    // Extra weights must not steal probability mass from real sources.
    let active_count = sources.len().min(sanitized.len());
    let weight_sum: f64 = sanitized[..active_count].iter().sum();

    let normalized: Vec<f64> = if !weight_sum.is_finite() || weight_sum <= 0.0 {
        let fallback_count = if active_count == 0 {
            sources.len()
        } else {
            active_count
        };
        let mut equal_weights = vec![0.0; sources.len()];
        equal_weights[..fallback_count].fill(1.0 / fallback_count as f64);
        equal_weights
    } else {
        (0..sources.len())
            .map(|i| sanitized.get(i).map(|&w| w / weight_sum).unwrap_or(0.0))
            .collect()
    };

    // Saturation keeps adversarial length sums from wrapping allocation capacity.
    let estimated_capacity: usize = sources
        .iter()
        .map(|s| s.len())
        .fold(0usize, |acc, n| acc.saturating_add(n));
    let mut combined: HashMap<Id, DeterministicScore> = HashMap::with_capacity(estimated_capacity);

    for (source_idx, results) in sources.into_iter().enumerate() {
        let weight = normalized[source_idx];

        // A zero-weight source must not inject zero-score IDs into the union.
        if weight == 0.0 {
            continue;
        }

        // Normalize source scales and keep one maximum contribution per ID.
        let norm_results = min_max_normalize_source(results);
        let mut source_best: HashMap<Id, DeterministicScore> =
            HashMap::with_capacity(norm_results.len());
        for (id, score) in norm_results {
            source_best
                .entry(id)
                .and_modify(|existing| {
                    if score > *existing {
                        *existing = score;
                    }
                })
                .or_insert(score);
        }

        for (id, score) in source_best {
            // Sanitized finite weights make this fixed-point operation infallible.
            let w = match weighted_sum(&[score], &[weight]) {
                Ok(s) => s,
                Err(_) => continue, // defensive: skip on unexpected error
            };
            let entry = combined.entry(id).or_insert(DeterministicScore::ZERO);
            *entry = *entry + w;
        }
    }

    let mut fused: Vec<(Id, DeterministicScore)> = combined.into_iter().collect();

    fused.sort_by(cmp_desc_then_id);
    fused
}

/// Returns `true` if positive weights sum to within `tolerance` of 1.0.
#[inline]
pub fn weights_are_normalized(weights: &[f64], tolerance: f64) -> bool {
    let sum: f64 = weights.iter().filter(|w| **w > 0.0).sum();
    (sum - 1.0).abs() <= tolerance
}

/// Lossily normalize finite positive weights; all-zero input becomes equal weights.
///
/// See `crates/khive-fusion/docs/api/fusion-functions.md`.
pub fn normalize_weights(weights: &[f64]) -> Vec<f64> {
    if weights.is_empty() {
        return Vec::new();
    }

    // Only finite positive weights participate; a shared predicate keeps the sum filter and the
    // normalization map from drifting apart (matches weighted_fusion).
    let finite_positive = |w: f64| w.is_finite() && w > 0.0;
    let weight_sum: f64 = weights
        .iter()
        .copied()
        .filter(|&w| finite_positive(w))
        .sum();

    // A finite positive sum can still overflow to infinity (e.g. [f64::MAX, f64::MAX]); fall back
    // to a uniform distribution rather than dividing by a non-finite denominator.
    if !weight_sum.is_finite() || weight_sum <= 0.0 {
        vec![1.0 / weights.len() as f64; weights.len()]
    } else {
        weights
            .iter()
            .map(|&w| {
                if finite_positive(w) {
                    w / weight_sum
                } else {
                    0.0
                }
            })
            .collect()
    }
}

/// Reject the first non-finite weight, then apply [`normalize_weights`].
pub fn try_normalize_weights(weights: &[f64]) -> Result<Vec<f64>, usize> {
    for (i, &w) in weights.iter().enumerate() {
        if !w.is_finite() {
            return Err(i);
        }
    }
    Ok(normalize_weights(weights))
}

#[cfg(test)]
#[path = "weighted_tests.rs"]
mod tests;
