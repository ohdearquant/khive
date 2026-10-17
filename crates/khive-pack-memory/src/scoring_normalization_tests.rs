use super::*;

fn inputs(scores: &[f32]) -> Vec<(Uuid, f32)> {
    scores
        .iter()
        .enumerate()
        .map(|(index, score)| (Uuid::from_u128(index as u128 + 1), *score))
        .collect()
}

#[test]
fn tied_scores_keep_distinct_source_calibration() {
    let config = ScoringConfig::default();
    let scores = inputs(&[0.02, 0.02]);
    let single = normalize_rank_fusion_scores(scores.clone(), &config);
    let fused = normalize_rrf_scores(scores.clone(), &config);
    for (id, _) in scores {
        assert!((single[&id] - 0.12).abs() < 1e-6);
        assert!((fused[&id] - 0.82).abs() < 1e-6);
    }
}

#[test]
fn weak_non_flat_scores_scale_only_single_source() {
    let config = ScoringConfig::default();
    let scores = inputs(&[0.001, 0.005]);
    let single = normalize_rank_fusion_scores(scores.clone(), &config);
    let fused = normalize_rrf_scores(scores.clone(), &config);
    let lower = scores[0].0;
    let upper = scores[1].0;
    assert!((single[&lower] - 0.03).abs() < 1e-6);
    assert!((single[&upper] - 0.164).abs() < 1e-6);
    assert!((fused[&lower] - 0.15).abs() < 1e-6);
    assert!((fused[&upper] - 0.82).abs() < 1e-6);
}

#[test]
fn both_sources_filter_before_calibrating_the_same_band() {
    let config = ScoringConfig {
        min_rrf_relevance: 0.01,
        ..ScoringConfig::default()
    };
    let scores = inputs(&[
        f32::NAN,
        f32::INFINITY,
        f32::NEG_INFINITY,
        0.005,
        0.02,
        0.04,
    ]);
    let single = normalize_rank_fusion_scores(scores.clone(), &config);
    let fused = normalize_rrf_scores(scores.clone(), &config);
    assert_eq!(single.len(), 2);
    assert_eq!(single, fused);
    assert!((single[&scores[4].0] - 0.15).abs() < 1e-6);
    assert!((single[&scores[5].0] - 0.82).abs() < 1e-6);
}
