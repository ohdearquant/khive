use super::*;
use crate::objective_fn;

#[test]
fn test_weighted_objective() {
    let obj1 = objective_fn(|n: &i32, _ctx: &ObjectiveContext| *n as f64);
    let obj2 = objective_fn(|n: &i32, _ctx: &ObjectiveContext| (*n * 2) as f64);

    let weighted = WeightedObjective::new()
        .add(Box::new(obj1), 1.0)
        .add(Box::new(obj2), 1.0);

    let context = ObjectiveContext::new();

    assert_eq!(weighted.score(&5, &context), 7.5);
}

#[test]
fn test_weighted_objective_ignores_invalid_weights() {
    let negative = objective_fn(|_n: &i32, _ctx: &ObjectiveContext| 100.0);
    let positive = objective_fn(|_n: &i32, _ctx: &ObjectiveContext| 4.0);

    let weighted = WeightedObjective::new()
        .add(Box::new(negative), -1.0)
        .add(Box::new(positive), 1.0);

    assert_eq!(weighted.score(&5, &ObjectiveContext::new()), 4.0);
}

#[test]
fn test_weighted_objective_requires_positive_finite_denominator() {
    let negative = objective_fn(|_n: &i32, _ctx: &ObjectiveContext| 100.0);
    let non_finite = objective_fn(|_n: &i32, _ctx: &ObjectiveContext| 4.0);

    let weighted = WeightedObjective::new()
        .add(Box::new(negative), -1.0)
        .add(Box::new(non_finite), f64::INFINITY);

    assert_eq!(weighted.score(&5, &ObjectiveContext::new()), 0.0);
}

#[test]
fn test_priority_objective() {
    let obj1 = objective_fn(
        |n: &i32, _ctx: &ObjectiveContext| {
            if *n > 10 {
                *n as f64
            } else {
                0.0
            }
        },
    );

    let obj2 = objective_fn(|n: &i32, _ctx: &ObjectiveContext| *n as f64 / 2.0);

    let priority = PriorityObjective::new()
        .add(Box::new(obj1), 5.0)
        .add(Box::new(obj2), 0.0)
        .with_fallback(-1.0);

    let context = ObjectiveContext::new();

    assert_eq!(priority.score(&15, &context), 15.0);
    assert_eq!(priority.score(&5, &context), 2.5);
}

#[test]
fn test_consensus_objective() {
    let obj1 = objective_fn(|n: &i32, _ctx: &ObjectiveContext| *n as f64);
    let obj2 = objective_fn(|n: &i32, _ctx: &ObjectiveContext| (*n * 2) as f64);

    let consensus = ConsensusObjective::new(5.0)
        .with_objective(Box::new(obj1))
        .with_objective(Box::new(obj2));

    let context = ObjectiveContext::new();

    // scores are 10 and 20 → geometric mean = sqrt(10 * 20) = sqrt(200) ≈ 14.142
    let score = consensus.score(&10, &context);
    let expected = (10.0f64 * 20.0f64).sqrt();
    assert!(
        (score - expected).abs() < 1e-9,
        "expected {expected}, got {score}"
    );

    // candidate 2 → scores 2 and 4, both below threshold 5 → 0.0
    assert_eq!(consensus.score(&2, &context), 0.0);
}

#[test]
fn test_consensus_objective_empty() {
    let consensus: ConsensusObjective<i32> = ConsensusObjective::new(0.0);
    assert_eq!(consensus.score(&10, &ObjectiveContext::new()), 0.0);
}

#[test]
fn test_consensus_objective_zero_score_returns_zero() {
    let obj1 = objective_fn(|_n: &i32, _ctx: &ObjectiveContext| 0.0f64);
    let obj2 = objective_fn(|n: &i32, _ctx: &ObjectiveContext| *n as f64);

    let consensus = ConsensusObjective::new(0.0)
        .with_objective(Box::new(obj1))
        .with_objective(Box::new(obj2));

    assert_eq!(consensus.score(&10, &ObjectiveContext::new()), 0.0);
}

#[test]
fn test_union_objective() {
    let obj1 = objective_fn(|n: &i32, _ctx: &ObjectiveContext| *n as f64);
    let obj2 = objective_fn(|n: &i32, _ctx: &ObjectiveContext| 100.0 - *n as f64);

    let union = UnionObjective::new()
        .with_objective(Box::new(obj1))
        .with_objective(Box::new(obj2));

    let context = ObjectiveContext::new();

    assert_eq!(union.score(&30, &context), 70.0);
    assert_eq!(union.score(&80, &context), 80.0);
}

#[test]
fn test_negate_objective() {
    let obj = objective_fn(|n: &i32, _ctx: &ObjectiveContext| *n as f64 / 100.0);
    let negated = NegateObjective::new(Box::new(obj));

    let context = ObjectiveContext::new();

    assert!((negated.score(&30, &context) - 0.7).abs() < 0.001);
}

#[test]
fn test_scale_objective() {
    let obj = objective_fn(|n: &i32, _ctx: &ObjectiveContext| *n as f64);
    let scaled = ScaleObjective::new(obj, 2.0);

    let context = ObjectiveContext::new();

    assert!((scaled.score(&0, &context) - 0.0).abs() < 0.001);
    assert!((scaled.score(&5, &context) - 10.0).abs() < 0.001);
    assert!((scaled.score(&10, &context) - 20.0).abs() < 0.001);
}

#[test]
fn test_scale_objective_negative_factor() {
    let obj = objective_fn(|n: &i32, _ctx: &ObjectiveContext| *n as f64);
    let scaled = ScaleObjective::new(obj, -1.0);

    let context = ObjectiveContext::new();

    assert!((scaled.score(&5, &context) - (-5.0)).abs() < 0.001);
}
