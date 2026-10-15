use super::*;

#[test]
fn test_max_score_objective() {
    let objective = MaxScoreObjective::new(|n: &i32| *n as f64);

    let candidates = vec![1, 5, 3, 8, 2];
    let selection = objective
        .select(&candidates, &ObjectiveContext::new())
        .into_iter()
        .next()
        .unwrap();

    assert_eq!(*selection.item, 8);
}

#[test]
fn test_threshold_objective() {
    let objective = ThresholdObjective::new(|n: &i32| *n as f64, 5.0);

    assert!(objective.passes(&10, &ObjectiveContext::new()));
    assert!(!objective.passes(&3, &ObjectiveContext::new()));
}

#[test]
fn test_threshold_objective_rejects_infinite_scores() {
    let objective = ThresholdObjective::new(|_n: &i32| f64::INFINITY, 5.0);

    assert!(!objective.passes(&10, &ObjectiveContext::new()));
}

#[test]
fn test_first_match_objective() {
    let objective = FirstMatchObjective::new(|n: &i32| *n > 5);

    let candidates = vec![1, 3, 7, 9, 2];
    let selection = objective
        .select(&candidates, &ObjectiveContext::new())
        .into_iter()
        .next()
        .unwrap();

    assert_eq!(*selection.item, 7);
    assert_eq!(selection.index, 2);
}

#[test]
fn test_first_match_respects_min_score() {
    let objective = FirstMatchObjective::new(|n: &i32| *n > 5);

    let candidates = vec![7];
    let context = ObjectiveContext::new().with_min_score(2.0);

    assert!(!objective.passes(&7, &context));
    assert!(objective.select(&candidates, &context).is_empty());
}

#[test]
fn test_first_match_respects_max_candidates() {
    let objective = FirstMatchObjective::new(|n: &i32| *n > 5);

    // Match is at index 2 (value 7), but max_candidates=2 limits scan to indices 0..1.
    let candidates = vec![1, 3, 7, 9, 2];
    let context = ObjectiveContext::new().with_max_candidates(2);
    let result = objective.select(&candidates, &context);

    assert!(result.is_empty());
}

#[derive(Clone)]
struct TestItem {
    _value: i32,
    timestamp: chrono::DateTime<chrono::Utc>,
    salience: f64,
}

impl HasTimestamp for TestItem {
    fn timestamp(&self) -> chrono::DateTime<chrono::Utc> {
        self.timestamp
    }
}

impl HasSalience for TestItem {
    fn salience(&self) -> f64 {
        self.salience
    }
}

#[test]
fn test_recency_objective() {
    let objective = RecencyObjective::hours(1.0);
    let now = chrono::Utc::now();
    // Pass current time explicitly — ObjectiveContext::new() defaults to the Unix epoch.
    let context = ObjectiveContext::at(now);

    let old = now - chrono::Duration::hours(2);

    let new_item = TestItem {
        _value: 1,
        timestamp: now,
        salience: 0.5,
    };
    let old_item = TestItem {
        _value: 2,
        timestamp: old,
        salience: 0.5,
    };

    let new_score = objective.score(&new_item, &context);
    let old_score = objective.score(&old_item, &context);

    assert!(new_score > old_score);
    assert!((new_score - 1.0).abs() < 0.1);
}

#[test]
fn recency_preserves_future_clamp_minimum_half_life_and_decay() {
    let now = chrono::DateTime::from_timestamp(1_000_000, 0).unwrap();
    let context = ObjectiveContext::at(now);
    for half_life in [0.25_f64, 1.0, 3_600.0, f64::MAX] {
        let objective = RecencyObjective::new(half_life);
        for age in [-1_i64, 0, 1, 3_600, 7_200] {
            let item = TestItem {
                _value: 1,
                timestamp: now - chrono::Duration::seconds(age),
                salience: 0.5,
            };
            let expected = 0.5_f64.powf(age.max(0) as f64 / half_life.max(1.0));
            assert!((objective.score(&item, &context) - expected).abs() <= f64::EPSILON);
        }
    }
}

#[test]
fn test_relevance_objective() {
    let objective = RelevanceObjective::balanced(3600.0);
    let now = chrono::Utc::now();
    // Pass current time explicitly — ObjectiveContext::new() defaults to the Unix epoch.
    let context = ObjectiveContext::at(now);

    let item = TestItem {
        _value: 1,
        timestamp: now,
        salience: 0.8,
    };

    let score = objective.score(&item, &context);

    assert!(score > 0.8 && score < 1.0);
}

#[test]
fn test_relevance_uses_context_relevance_score() {
    let objective = RelevanceObjective::balanced(3600.0);
    let now = chrono::Utc::now();
    // Pass current time explicitly — ObjectiveContext::new() defaults to the Unix epoch.
    let context =
        ObjectiveContext::at(now).with_extra(serde_json::json!({"relevance_score": 0.42}));

    let item = TestItem {
        _value: 1,
        timestamp: now,
        salience: 0.9,
    };

    // The context relevance_score should override the recency+salience fusion.
    let score = objective.score(&item, &context);
    assert!((score - 0.42).abs() < 1e-9);
}

#[test]
#[should_panic(expected = "recency_weight must be finite and non-negative")]
fn test_relevance_negative_recency_weight_panics() {
    RelevanceObjective::new(3600.0, -0.1, 0.5);
}

#[test]
#[should_panic(expected = "salience_weight must be finite and non-negative")]
fn test_relevance_nan_salience_weight_panics() {
    RelevanceObjective::new(3600.0, 0.5, f64::NAN);
}

#[test]
#[should_panic(expected = "half_life_seconds must be positive and finite")]
fn test_recency_zero_half_life_panics() {
    RecencyObjective::new(0.0);
}

#[test]
#[should_panic(expected = "half_life_seconds must be positive and finite")]
fn test_recency_negative_half_life_panics() {
    RecencyObjective::new(-1.0);
}

#[test]
#[should_panic(expected = "half_life_seconds must be positive and finite")]
fn test_recency_nan_half_life_panics() {
    RecencyObjective::new(f64::NAN);
}

#[test]
fn test_threshold_no_match_below_threshold() {
    let objective = ThresholdObjective::new(|n: &i32| *n as f64, 10.0);

    let candidates = vec![1, 5, 3];
    let result = objective.select(&candidates, &ObjectiveContext::new());

    assert!(result.is_empty());
}

#[test]
fn test_threshold_selects_best_above() {
    let objective = ThresholdObjective::new(|n: &i32| *n as f64, 5.0);

    let candidates = vec![1, 10, 3, 15];
    let selection = objective
        .select(&candidates, &ObjectiveContext::new())
        .into_iter()
        .next()
        .unwrap();

    assert_eq!(*selection.item, 15);
    assert_eq!(selection.score, 15.0);
    assert_eq!(selection.passed, 2);
}
