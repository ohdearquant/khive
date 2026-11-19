use super::*;
use crate::ordering::HasId;
use uuid::Uuid;

include!("tests/selection_values.rs");

#[test]
fn test_nan_score_never_selected() {
    let objective = objective_fn(
        |n: &i32, _ctx: &ObjectiveContext| {
            if *n == 5 {
                f64::NAN
            } else {
                *n as f64
            }
        },
    );

    let candidates = vec![1, 5, 3];
    let selection = objective
        .select(&candidates, &ObjectiveContext::new())
        .into_iter()
        .next()
        .unwrap();

    assert_eq!(*selection.item, 3);
    assert_eq!(selection.score, 3.0);
    assert_eq!(selection.passed, 2);
}

#[test]
fn test_infinite_score_never_selected() {
    let objective = objective_fn(
        |n: &i32, _ctx: &ObjectiveContext| {
            if *n == 5 {
                f64::INFINITY
            } else {
                *n as f64
            }
        },
    );

    let candidates = vec![1, 5, 3];
    let selection = objective
        .select(&candidates, &ObjectiveContext::new())
        .into_iter()
        .next()
        .unwrap();

    assert_eq!(*selection.item, 3);
    assert_eq!(selection.score, 3.0);
    assert_eq!(selection.passed, 2);
}

#[test]
fn test_max_candidates_respected() {
    let objective = objective_fn(|n: &i32, _ctx: &ObjectiveContext| *n as f64);

    let candidates = vec![1, 5, 3, 8, 2];
    let context = ObjectiveContext::new().with_max_candidates(2);
    let selection = objective
        .select(&candidates, &context)
        .into_iter()
        .next()
        .unwrap();

    assert_eq!(*selection.item, 5);
    assert_eq!(selection.considered, 2);
}

// ========================================================================
// DeterministicObjective Tests
// ========================================================================

#[derive(Debug, Clone)]
struct TestCandidate {
    id: Uuid,
    value: i32,
}

impl TestCandidate {
    fn new(value: i32) -> Self {
        Self {
            id: Uuid::new_v4(),
            value,
        }
    }

    fn with_id(id: Uuid, value: i32) -> Self {
        Self { id, value }
    }
}

impl HasId for TestCandidate {
    fn id(&self) -> Uuid {
        self.id
    }
}

include!("tests/deterministic_selection.rs");

// ========================================================================
// Precision (predictive coding) Tests
// ========================================================================

#[test]
fn precision_default_returns_one() {
    // The closure-based Objective inherits the default precision() → 1.0.
    let objective = objective_fn(|n: &i32, _ctx: &ObjectiveContext| *n as f64);
    let ctx = ObjectiveContext::new();
    assert_eq!(objective.precision(&42, &ctx), 1.0);
}

#[test]
fn precision_one_leaves_ranking_unchanged() {
    // When all precisions are 1.0, select behaves identically to raw score ranking.
    let objective = objective_fn(|n: &i32, _ctx: &ObjectiveContext| *n as f64);
    let candidates = vec![1, 5, 3, 8, 2];
    let sel = objective
        .select(&candidates, &ObjectiveContext::new())
        .into_iter()
        .next()
        .unwrap();
    assert_eq!(*sel.item, 8);
    assert_eq!(sel.precision, 1.0);
}

#[test]
fn precision_reorders_candidates_when_lower() {
    // Candidate with score 10.0 and precision 0.1 → effective 1.0.
    // Candidate with score 3.0 and precision 1.0 → effective 3.0.
    // The lower-score but precise candidate should win.
    struct PrecisionObjective;
    impl Objective<(f64, f64)> for PrecisionObjective {
        fn score(&self, c: &(f64, f64), _ctx: &ObjectiveContext) -> f64 {
            c.0
        }
        fn precision(&self, c: &(f64, f64), _ctx: &ObjectiveContext) -> f64 {
            c.1
        }
    }

    let candidates = vec![(10.0f64, 0.1f64), (3.0f64, 1.0f64)];
    let sel = PrecisionObjective
        .select(&candidates, &ObjectiveContext::new())
        .into_iter()
        .next()
        .unwrap();
    // 3.0 * 1.0 = 3.0  >  10.0 * 0.1 = 1.0
    assert_eq!(sel.item.0, 3.0);
    assert_eq!(sel.precision, 1.0);
}

#[test]
fn selection_stores_precision_from_winning_candidate() {
    // After F130: select delegates to select_top which scores by effective (score*precision)
    // but stores effective in selection.score; precision field defaults to 1.0.
    struct HalfPrecision;
    impl Objective<i32> for HalfPrecision {
        fn score(&self, n: &i32, _ctx: &ObjectiveContext) -> f64 {
            *n as f64
        }
        fn precision(&self, _n: &i32, _ctx: &ObjectiveContext) -> f64 {
            0.5
        }
    }
    let candidates = vec![1, 2, 3];
    let sel = HalfPrecision
        .select(&candidates, &ObjectiveContext::new())
        .into_iter()
        .next()
        .unwrap();
    // Best by effective score (3 * 0.5 = 1.5).
    assert_eq!(*sel.item, 3);
    // select_top stores effective score, not raw score.
    assert!((sel.score - 1.5).abs() < 1e-10);
}

#[test]
fn non_finite_precision_treated_as_one() {
    // Non-finite precision should not panic and should behave as if precision = 1.0.
    struct NanPrecision;
    impl Objective<i32> for NanPrecision {
        fn score(&self, n: &i32, _ctx: &ObjectiveContext) -> f64 {
            *n as f64
        }
        fn precision(&self, _n: &i32, _ctx: &ObjectiveContext) -> f64 {
            f64::NAN
        }
    }
    let candidates = vec![1, 5, 3];
    let sel = NanPrecision
        .select(&candidates, &ObjectiveContext::new())
        .into_iter()
        .next()
        .unwrap();
    // NaN precision → treat as 1.0 → raw score ordering → 5 wins.
    assert_eq!(*sel.item, 5);
}
