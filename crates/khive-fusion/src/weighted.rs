//! Weighted linear combination fusion with per-source min-max normalization.

use khive_score::{weighted_sum, DeterministicScore};
use std::cmp::Ordering;
use std::collections::HashMap;
use std::hash::Hash;

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
/// Negative/non-finite weights become zero and all-zero input becomes equal weights. Results sort
/// by descending score, breaking ties by ascending ID. See
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

    let normalized: Vec<f64> = if weight_sum <= 0.0 {
        vec![1.0 / sources.len() as f64; sources.len()]
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

    fused.sort_by(
        |(id_a, score_a), (id_b, score_b)| match score_b.cmp(score_a) {
            Ordering::Equal => id_a.cmp(id_b),
            other => other,
        },
    );
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

    // Sanitize before summing so NaN/Inf cannot enter arithmetic (matches weighted_fusion).
    let weight_sum: f64 = weights.iter().filter(|w| w.is_finite() && **w > 0.0).sum();

    if weight_sum <= 0.0 {
        vec![1.0 / weights.len() as f64; weights.len()]
    } else {
        weights
            .iter()
            .map(|w| {
                if w.is_finite() && *w > 0.0 {
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
mod tests {
    use super::*;

    fn make_results<Id: Clone>(items: Vec<(Id, f64)>) -> Vec<(Id, DeterministicScore)> {
        items
            .into_iter()
            .map(|(id, score)| (id, DeterministicScore::from_f64(score)))
            .collect()
    }

    #[test]
    fn test_weighted_basic() {
        let source1 = make_results(vec![("doc_a", 1.0)]);
        let source2 = make_results(vec![("doc_a", 1.0)]);

        let fused = weighted_fusion(vec![source1, source2], &[0.7, 0.3]);

        assert!((fused[0].1.to_f64() - 1.0).abs() < 0.01);
    }

    #[test]
    fn test_weighted_normalization() {
        let source1 = make_results(vec![("doc_a", 1.0)]);
        let source2 = make_results(vec![("doc_a", 1.0)]);

        let fused = weighted_fusion(vec![source1, source2], &[7.0, 3.0]);

        assert!((fused[0].1.to_f64() - 1.0).abs() < 0.01);
    }

    #[test]
    fn test_weighted_zero_weights() {
        let source1 = make_results(vec![("doc_a", 1.0)]);
        let source2 = make_results(vec![("doc_a", 1.0)]);

        let fused = weighted_fusion(vec![source1, source2], &[0.0, 0.0]);

        assert!((fused[0].1.to_f64() - 1.0).abs() < 0.01);
    }

    #[test]
    fn test_weighted_disjoint_results() {
        let source1 = make_results(vec![("doc_a", 0.9)]);
        let source2 = make_results(vec![("doc_b", 0.8)]);

        let fused = weighted_fusion(vec![source1, source2], &[0.6, 0.4]);

        let doc_a = fused.iter().find(|(id, _)| *id == "doc_a").unwrap();
        let doc_b = fused.iter().find(|(id, _)| *id == "doc_b").unwrap();

        // After per-source min-max normalization, single-element sources map to 1.0.
        // doc_a contributes 1.0 * 0.6 = 0.6, doc_b contributes 1.0 * 0.4 = 0.4.
        assert!((doc_a.1.to_f64() - 0.6).abs() < 0.01);
        assert!((doc_b.1.to_f64() - 0.4).abs() < 0.01);
    }

    #[test]
    fn test_weighted_empty_sources() {
        let fused: Vec<(&str, DeterministicScore)> = weighted_fusion(vec![], &[]);
        assert!(fused.is_empty());
    }

    #[test]
    fn test_weighted_single_source() {
        let source = make_results(vec![("doc_a", 0.9)]);
        let fused = weighted_fusion(vec![source], &[1.0]);

        assert_eq!(fused.len(), 1);
        // Single-element source normalizes to 1.0; weight is 1.0 → final = 1.0.
        assert!((fused[0].1.to_f64() - 1.0).abs() < 0.01);
    }

    // RETRIEVAL-07: Normalization behavior tests

    #[test]
    fn test_normalization_already_normalized() {
        let source1 = make_results(vec![("doc_a", 1.0)]);
        let source2 = make_results(vec![("doc_b", 1.0)]);

        // Weights already sum to 1.0
        let fused = weighted_fusion(vec![source1, source2], &[0.6, 0.4]);

        let doc_a = fused.iter().find(|(id, _)| *id == "doc_a").unwrap();
        let doc_b = fused.iter().find(|(id, _)| *id == "doc_b").unwrap();

        assert!((doc_a.1.to_f64() - 0.6).abs() < 0.01);
        assert!((doc_b.1.to_f64() - 0.4).abs() < 0.01);
    }

    #[test]
    fn test_normalization_scaled_weights() {
        let source1 = make_results(vec![("doc_a", 1.0)]);
        let source2 = make_results(vec![("doc_b", 1.0)]);

        // Weights sum to 100, should be normalized to 0.6, 0.4
        let fused = weighted_fusion(vec![source1, source2], &[60.0, 40.0]);

        let doc_a = fused.iter().find(|(id, _)| *id == "doc_a").unwrap();
        let doc_b = fused.iter().find(|(id, _)| *id == "doc_b").unwrap();

        assert!((doc_a.1.to_f64() - 0.6).abs() < 0.01);
        assert!((doc_b.1.to_f64() - 0.4).abs() < 0.01);
    }

    #[test]
    fn test_normalization_negative_weights() {
        let source1 = make_results(vec![("doc_a", 1.0)]);
        let source2 = make_results(vec![("doc_b", 1.0)]);

        // Negative weight should be treated as 0
        let fused = weighted_fusion(vec![source1, source2], &[1.0, -0.5]);

        let doc_a = fused.iter().find(|(id, _)| *id == "doc_a").unwrap();
        let doc_b = fused.iter().find(|(id, _)| *id == "doc_b");

        // doc_a gets full weight (1.0 normalized to 1.0)
        assert!((doc_a.1.to_f64() - 1.0).abs() < 0.01);
        // doc_b should have 0 contribution from second source
        assert!(doc_b.is_none() || doc_b.unwrap().1.to_f64() < 0.01);
    }

    #[test]
    fn test_normalization_three_sources_equal() {
        let source1 = make_results(vec![("doc_a", 1.0)]);
        let source2 = make_results(vec![("doc_b", 1.0)]);
        let source3 = make_results(vec![("doc_c", 1.0)]);

        // Equal weights
        let fused = weighted_fusion(vec![source1, source2, source3], &[1.0, 1.0, 1.0]);

        for (_, score) in &fused {
            // Each should get 1/3 weight = 0.333...
            assert!((score.to_f64() - 1.0 / 3.0).abs() < 0.01);
        }
    }

    #[test]
    fn test_normalization_consistent_across_scales() {
        let source1 = make_results(vec![("doc_a", 0.8), ("doc_b", 0.6)]);
        let source2 = make_results(vec![("doc_a", 0.9), ("doc_c", 0.7)]);

        // Same ratio, different scales
        let fused1 = weighted_fusion(vec![source1.clone(), source2.clone()], &[0.7, 0.3]);
        let fused2 = weighted_fusion(vec![source1.clone(), source2.clone()], &[7.0, 3.0]);
        let fused3 = weighted_fusion(vec![source1, source2], &[70.0, 30.0]);

        // All should produce identical results
        assert_eq!(fused1.len(), fused2.len());
        assert_eq!(fused2.len(), fused3.len());

        for i in 0..fused1.len() {
            assert_eq!(fused1[i].0, fused2[i].0);
            assert_eq!(fused2[i].0, fused3[i].0);
            assert!(
                (fused1[i].1.to_f64() - fused2[i].1.to_f64()).abs() < 1e-10,
                "Score mismatch at position {}: {} vs {}",
                i,
                fused1[i].1.to_f64(),
                fused2[i].1.to_f64()
            );
            assert!(
                (fused2[i].1.to_f64() - fused3[i].1.to_f64()).abs() < 1e-10,
                "Score mismatch at position {}: {} vs {}",
                i,
                fused2[i].1.to_f64(),
                fused3[i].1.to_f64()
            );
        }
    }

    // Helper function tests

    #[test]
    fn test_weights_are_normalized() {
        assert!(weights_are_normalized(&[0.5, 0.5], 1e-6));
        assert!(weights_are_normalized(&[0.7, 0.3], 1e-6));
        assert!(weights_are_normalized(&[1.0], 1e-6));
        assert!(weights_are_normalized(&[0.25, 0.25, 0.25, 0.25], 1e-6));

        assert!(!weights_are_normalized(&[0.5, 0.6], 1e-6)); // > 1
        assert!(!weights_are_normalized(&[0.3, 0.3], 1e-6)); // < 1
        assert!(!weights_are_normalized(&[10.0, 10.0], 1e-6)); // = 20
    }

    #[test]
    fn test_normalize_weights() {
        let normalized = normalize_weights(&[6.0, 4.0]);
        assert!((normalized[0] - 0.6).abs() < 1e-10);
        assert!((normalized[1] - 0.4).abs() < 1e-10);

        let normalized = normalize_weights(&[1.0, 1.0, 1.0]);
        for w in &normalized {
            assert!((w - 1.0 / 3.0).abs() < 1e-10);
        }

        let normalized = normalize_weights(&[0.0, 0.0]);
        assert!((normalized[0] - 0.5).abs() < 1e-10);
        assert!((normalized[1] - 0.5).abs() < 1e-10);

        let normalized = normalize_weights(&[1.0, -1.0]);
        assert!((normalized[0] - 1.0).abs() < 1e-10);
        assert!((normalized[1] - 0.0).abs() < 1e-10);
    }

    #[test]
    fn test_normalize_weights_empty() {
        let normalized = normalize_weights(&[]);
        assert!(normalized.is_empty());
    }

    #[test]
    fn test_normalize_weights_non_finite() {
        let normalized = normalize_weights(&[f64::INFINITY, 1.0]);
        assert!(normalized.iter().all(|w| w.is_finite()));
        assert!((normalized[0] - 0.0).abs() < 1e-10);
        assert!((normalized[1] - 1.0).abs() < 1e-10);

        let normalized = normalize_weights(&[f64::NAN, 2.0, 2.0]);
        assert!(normalized.iter().all(|w| w.is_finite()));
        assert!((normalized[0] - 0.0).abs() < 1e-10);
        assert!((normalized[1] - 0.5).abs() < 1e-10);
        assert!((normalized[2] - 0.5).abs() < 1e-10);

        let normalized = normalize_weights(&[f64::INFINITY, f64::NAN]);
        assert!(normalized.iter().all(|w| w.is_finite()));
        assert!((normalized[0] - 0.5).abs() < 1e-10);
        assert!((normalized[1] - 0.5).abs() < 1e-10);
    }

    // ── #2496 / #2639: per-source min-max normalization before fusion ──────

    #[test]
    fn test_weighted_fusion_mixed_scales_bm25_vs_cosine() {
        // BM25-like: unbounded scores (0..100)
        let bm25 = vec![
            ("doc_a", DeterministicScore::from_f64(80.0)),
            ("doc_b", DeterministicScore::from_f64(20.0)),
        ];
        // Cosine-like: [0,1] scores
        let cosine = vec![
            ("doc_a", DeterministicScore::from_f64(0.9)),
            ("doc_b", DeterministicScore::from_f64(0.1)),
        ];

        let result = weighted_fusion(vec![bm25, cosine], &[0.5, 0.5]);
        // Both sources agree doc_a is more relevant — it must rank first.
        assert_eq!(result[0].0, "doc_a");
        assert_eq!(result[1].0, "doc_b");
        // After normalization, doc_a should score close to 1.0 (top in both).
        assert!(result[0].1.to_f64() > 0.8);
    }

    #[test]
    fn test_weighted_fusion_inverted_scale_normalizes_correctly() {
        // If one source has negative/inverted semantics, min-max still works.
        let src1 = vec![
            ("x", DeterministicScore::from_f64(100.0)),
            ("y", DeterministicScore::from_f64(1.0)),
        ];
        let src2 = vec![
            ("x", DeterministicScore::from_f64(0.9)),
            ("y", DeterministicScore::from_f64(0.1)),
        ];

        let result = weighted_fusion(vec![src1, src2], &[0.6, 0.4]);
        assert_eq!(result[0].0, "x");
        // x must score strictly higher than y
        assert!(result[0].1.to_f64() > result[1].1.to_f64());
    }

    #[test]
    fn test_min_max_normalize_source_single() {
        let src = vec![("a", DeterministicScore::from_f64(42.0))];
        let out = min_max_normalize_source(src);
        assert!((out[0].1.to_f64() - 1.0).abs() < 1e-10);
    }

    #[test]
    fn test_min_max_normalize_source_uniform() {
        let src = vec![
            ("a", DeterministicScore::from_f64(5.0)),
            ("b", DeterministicScore::from_f64(5.0)),
        ];
        let out = min_max_normalize_source(src);
        // All equal → all 1.0
        assert!((out[0].1.to_f64() - 1.0).abs() < 1e-10);
        assert!((out[1].1.to_f64() - 1.0).abs() < 1e-10);
    }
}
