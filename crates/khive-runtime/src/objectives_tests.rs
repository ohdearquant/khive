use super::*;
use khive_fold::objective::{Objective, ObjectiveContext};
use khive_fold::WeightedObjective;
use uuid::Uuid;

fn ctx() -> ObjectiveContext {
    ObjectiveContext::new()
}

fn candidate(
    vector: Option<f64>,
    text: Option<f64>,
    dist: Option<u32>,
    rrf: Option<f64>,
) -> RetrievalCandidate {
    RetrievalCandidate {
        id: Uuid::new_v4(),
        vector_score: vector,
        text_score: text,
        graph_distance: dist,
        rrf_score: rrf,
    }
}

fn note_candidate(
    rrf: Option<f64>,
    salience: f64,
    decay_factor: f64,
    age_days: f64,
) -> NoteCandidate {
    // Mirrors the caller-side DecayModel::apply() default (Exponential) for test data.
    let effective_salience = salience * (-decay_factor * age_days).exp();
    NoteCandidate {
        id: Uuid::new_v4(),
        rrf_score: rrf,
        salience,
        decay_factor,
        age_days,
        effective_salience,
        rerank_scores: HashMap::new(),
    }
}

// ── VectorSimilarityObjective ────────────────────────────────────────

#[test]
fn vector_present_returns_signal() {
    let c = candidate(Some(0.85), None, None, None);
    let score = VectorSimilarityObjective.score(&c, &ctx());
    assert!((score - 0.85).abs() < 1e-12);
}

#[test]
fn vector_absent_returns_zero() {
    let c = candidate(None, None, None, None);
    assert_eq!(VectorSimilarityObjective.score(&c, &ctx()), 0.0);
}

#[test]
fn vector_zero_score_returns_zero() {
    let c = candidate(Some(0.0), None, None, None);
    assert_eq!(VectorSimilarityObjective.score(&c, &ctx()), 0.0);
}

// ── TextRelevanceObjective ───────────────────────────────────────────

#[test]
fn text_present_returns_signal() {
    let c = candidate(None, Some(0.6), None, None);
    let score = TextRelevanceObjective.score(&c, &ctx());
    assert!((score - 0.6).abs() < 1e-12);
}

#[test]
fn text_absent_returns_zero() {
    let c = candidate(None, None, None, None);
    assert_eq!(TextRelevanceObjective.score(&c, &ctx()), 0.0);
}

// ── GraphProximityObjective ──────────────────────────────────────────

#[test]
fn graph_anchor_hit_scores_one() {
    let c = candidate(None, None, Some(0), None);
    let obj = GraphProximityObjective { max_distance: 3 };
    assert!((obj.score(&c, &ctx()) - 1.0).abs() < 1e-12);
}

#[test]
fn graph_midpoint_scores_half() {
    let c = candidate(None, None, Some(1), None);
    let obj = GraphProximityObjective { max_distance: 2 };
    assert!((obj.score(&c, &ctx()) - 0.5).abs() < 1e-12);
}

#[test]
fn graph_at_boundary_scores_zero() {
    let c = candidate(None, None, Some(3), None);
    let obj = GraphProximityObjective { max_distance: 3 };
    assert_eq!(obj.score(&c, &ctx()), 0.0);
}

#[test]
fn graph_beyond_boundary_scores_zero() {
    let c = candidate(None, None, Some(10), None);
    let obj = GraphProximityObjective { max_distance: 3 };
    assert_eq!(obj.score(&c, &ctx()), 0.0);
}

#[test]
fn graph_absent_scores_zero() {
    let c = candidate(None, None, None, None);
    let obj = GraphProximityObjective { max_distance: 3 };
    assert_eq!(obj.score(&c, &ctx()), 0.0);
}

#[test]
fn graph_max_distance_zero_always_scores_zero() {
    // Guards the divide-by-zero case: max_distance=0 must not panic.
    let c = candidate(None, None, Some(0), None);
    let obj = GraphProximityObjective { max_distance: 0 };
    assert_eq!(obj.score(&c, &ctx()), 0.0);
}

// ── RrfFusionObjective ───────────────────────────────────────────────

#[test]
fn rrf_present_returns_signal() {
    let c = candidate(None, None, None, Some(0.0327));
    let score = RrfFusionObjective.score(&c, &ctx());
    assert!((score - 0.0327).abs() < 1e-12);
}

#[test]
fn rrf_absent_returns_zero() {
    let c = candidate(None, None, None, None);
    assert_eq!(RrfFusionObjective.score(&c, &ctx()), 0.0);
}

// ── WeightedObjective composition ───────────────────────────────────

#[test]
fn weighted_composition_vector_and_text() {
    let c = candidate(Some(0.8), Some(0.6), None, None);

    let obj = WeightedObjective::<RetrievalCandidate>::new()
        .add(Box::new(VectorSimilarityObjective), 0.5)
        .add(Box::new(TextRelevanceObjective), 0.5);

    let score = obj.score(&c, &ctx());
    // Weights here already sum to 1.0, so normalization is a no-op.
    assert!((score - 0.7).abs() < 1e-12);
}

#[test]
fn weighted_composition_with_graph() {
    let c = candidate(Some(1.0), Some(0.0), Some(1), None);

    let obj = WeightedObjective::<RetrievalCandidate>::new()
        .add(Box::new(VectorSimilarityObjective), 0.4)
        .add(Box::new(TextRelevanceObjective), 0.3)
        .add(Box::new(GraphProximityObjective { max_distance: 4 }), 0.3);

    let score = obj.score(&c, &ctx());
    assert!((score - 0.625).abs() < 1e-12);
}

#[test]
fn weighted_all_absent_returns_zero() {
    let c = candidate(None, None, None, None);

    let obj = WeightedObjective::<RetrievalCandidate>::new()
        .add(Box::new(VectorSimilarityObjective), 0.5)
        .add(Box::new(TextRelevanceObjective), 0.5);

    // 0.0 * 0.5 + 0.0 * 0.5 = 0.0
    assert_eq!(obj.score(&c, &ctx()), 0.0);
}

// ── HasId ────────────────────────────────────────────────────────────

#[test]
fn has_id_returns_candidate_uuid() {
    let id = Uuid::new_v4();
    let c = RetrievalCandidate {
        id,
        vector_score: None,
        text_score: None,
        graph_distance: None,
        rrf_score: None,
    };
    assert_eq!(c.id(), id);
}

// ── select_top via DeterministicObjective ────────────────────────────

#[test]
fn select_top_orders_by_vector_score() {
    use khive_fold::DeterministicObjective;

    let candidates = vec![
        candidate(Some(0.3), None, None, None),
        candidate(Some(0.9), None, None, None),
        candidate(Some(0.6), None, None, None),
    ];

    let top = VectorSimilarityObjective.select_top_deterministic(&candidates, 2, &ctx());

    assert_eq!(top.len(), 2);
    assert!((top[0].score - 0.9).abs() < 1e-12);
    assert!((top[1].score - 0.6).abs() < 1e-12);
}

// ── NoteCandidate: HasId ─────────────────────────────────────────────

#[test]
fn note_candidate_has_id_returns_uuid() {
    let id = Uuid::new_v4();
    let c = NoteCandidate {
        id,
        rrf_score: None,
        salience: 0.5,
        decay_factor: 0.01,
        age_days: 0.0,
        effective_salience: 0.5,
        rerank_scores: HashMap::new(),
    };
    assert_eq!(c.id(), id);
}

// ── DecayAwareSalienceObjective ──────────────────────────────────────

#[test]
fn decay_aware_zero_age_returns_full_salience() {
    let obj = DecayAwareSalienceObjective::new(0.01);
    let c = note_candidate(None, 0.8, 0.01, 0.0);
    let score = obj.score(&c, &ctx());
    assert!((score - 0.8).abs() < 1e-12, "got {score}");
}

#[test]
fn decay_aware_configured_rates_produce_expected_distinct_scores() {
    let slow = DecayAwareSalienceObjective::new(0.01);
    let fast = DecayAwareSalienceObjective::new(0.1);
    let c = note_candidate(None, 0.8, 0.99, 10.0);

    let slow_score = slow.score(&c, &ctx());
    let fast_score = fast.score(&c, &ctx());
    let expected_slow = 0.8 * (-0.01_f64 * 10.0).exp();
    let expected_fast = 0.8 * (-0.1_f64 * 10.0).exp();

    assert!(
        (slow_score - expected_slow).abs() < 1e-12,
        "got {slow_score}, expected {expected_slow}"
    );
    assert!(
        (fast_score - expected_fast).abs() < 1e-12,
        "got {fast_score}, expected {expected_fast}"
    );
    assert!(
        slow_score > fast_score,
        "slower configured decay should score higher: {slow_score} vs {fast_score}"
    );
}

#[test]
fn decay_aware_does_not_read_candidate_decay_factor() {
    let obj = DecayAwareSalienceObjective::new(0.05);
    let slow = note_candidate(None, 1.0, 0.001, 100.0);
    let fast = note_candidate(None, 1.0, 0.1, 100.0);
    let score_slow = obj.score(&slow, &ctx());
    let score_fast = obj.score(&fast, &ctx());
    assert!(
        (score_slow - score_fast).abs() < 1e-12,
        "candidate decay_factor changed fixed-rate score: {score_slow} vs {score_fast}"
    );
}

#[test]
fn decay_aware_rejects_invalid_configured_rates() {
    for bad in [-0.1, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        let result = std::panic::catch_unwind(|| DecayAwareSalienceObjective::new(bad));
        assert!(result.is_err(), "accepted invalid decay_rate {bad}");
    }
}

#[test]
fn decay_objectives_preserve_candidate_boundaries() {
    for age in [
        -f64::INFINITY,
        -30.0,
        -0.0,
        0.0,
        30.0,
        f64::INFINITY,
        f64::NAN,
    ] {
        for value in [-0.0, 0.8, f64::MAX, f64::INFINITY, f64::NAN] {
            let candidate = note_candidate(None, value, 0.01, age);
            for rate in [0.0, 0.01, f64::MAX] {
                let actual = DecayAwareSalienceObjective::new(rate).score(&candidate, &ctx());
                let expected = value * (-rate * age).exp();
                assert!(
                    actual.to_bits() == expected.to_bits()
                        || (actual.is_nan() && expected.is_nan())
                );
            }
            for half_life_days in [0.0, 30.0, f64::INFINITY, f64::NAN] {
                let actual = TemporalRecencyObjective { half_life_days }.score(&candidate, &ctx());
                let expected =
                    (-(std::f64::consts::LN_2 / half_life_days.max(f64::EPSILON)) * age).exp();
                assert!(
                    actual.to_bits() == expected.to_bits()
                        || (actual.is_nan() && expected.is_nan())
                );
            }
        }
    }
}

// ── TemporalRecencyObjective ─────────────────────────────────────────

#[test]
fn temporal_score_one_at_zero_age() {
    let obj = TemporalRecencyObjective {
        half_life_days: 30.0,
    };
    let c = note_candidate(None, 0.5, 0.01, 0.0);
    let score = obj.score(&c, &ctx());
    assert!((score - 1.0).abs() < 1e-12, "got {score}");
}

#[test]
fn temporal_score_half_at_half_life() {
    let half_life = 30.0;
    let obj = TemporalRecencyObjective {
        half_life_days: half_life,
    };
    let c = note_candidate(None, 0.5, 0.01, half_life);
    let score = obj.score(&c, &ctx());
    assert!(
        (score - 0.5).abs() < 1e-10,
        "expected 0.5 at half_life, got {score}"
    );
}

#[test]
fn temporal_score_decreases_with_age() {
    let obj = TemporalRecencyObjective {
        half_life_days: 30.0,
    };
    let young = note_candidate(None, 1.0, 0.01, 10.0);
    let old = note_candidate(None, 1.0, 0.01, 100.0);
    let score_young = obj.score(&young, &ctx());
    let score_old = obj.score(&old, &ctx());
    assert!(
        score_young > score_old,
        "younger note should score higher: {score_young} vs {score_old}"
    );
}

// ── RerankerObjective ────────────────────────────────────────────────

#[test]
fn reranker_returns_named_score() {
    let mut c = note_candidate(None, 0.5, 0.01, 0.0);
    c.rerank_scores.insert("cross_encoder".to_string(), 0.9);
    let obj = RerankerObjective::new("cross_encoder");
    let score = obj.score(&c, &ctx());
    assert!((score - 0.9).abs() < 1e-12, "got {score}");
}

#[test]
fn reranker_absent_key_returns_zero() {
    let c = note_candidate(None, 0.5, 0.01, 0.0);
    let obj = RerankerObjective::new("cross_encoder");
    let score = obj.score(&c, &ctx());
    assert_eq!(score, 0.0);
}

#[test]
fn reranker_different_keys_independent() {
    let mut c = note_candidate(None, 0.5, 0.01, 0.0);
    c.rerank_scores.insert("salience".to_string(), 0.7);
    let obj_ce = RerankerObjective::new("cross_encoder");
    let obj_sal = RerankerObjective::new("salience");
    assert_eq!(obj_ce.score(&c, &ctx()), 0.0);
    assert!((obj_sal.score(&c, &ctx()) - 0.7).abs() < 1e-12);
}

// ── Weighted composition of memory objectives ────────────────────────

#[test]
fn memory_pipeline_weighted_composition() {
    // Verifies WeightedObjective reproduces the same formula MemoryRecallPipeline builds.
    let c = NoteCandidate {
        id: Uuid::new_v4(),
        rrf_score: Some(0.5),
        salience: 0.8,
        decay_factor: 0.01,
        age_days: 0.0,
        effective_salience: 0.8,
        rerank_scores: HashMap::new(),
    };
    let pipeline = WeightedObjective::<NoteCandidate>::new()
        .add(Box::new(RrfFusionObjective), 0.70)
        .add(Box::new(DecayAwareSalienceObjective::new(0.0)), 0.20)
        .add(
            Box::new(TemporalRecencyObjective {
                half_life_days: 30.0,
            }),
            0.10,
        );
    let score = pipeline.score(&c, &ctx());
    assert!((score - 0.61).abs() < 1e-10, "got {score}");
}
