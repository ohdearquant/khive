//! Objective function framework — scoring, selection, composition.

pub mod builtin;
pub mod compose;
mod context;
pub mod error;
mod selection;
mod traits;

pub use context::ObjectiveContext;
pub use error::{ObjectiveError, ObjectiveResult};
pub use selection::Selection;
pub use traits::{objective_fn, DeterministicObjective, Objective};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ordering::HasId;
    use crate::ObjectiveError;
    use uuid::Uuid;

    #[test]
    fn test_simple_objective() {
        let objective = objective_fn(|n: &i32, _ctx: &ObjectiveContext| *n as f64);

        let candidates = vec![1, 5, 3, 8, 2];
        let selection = objective
            .select(&candidates, &ObjectiveContext::new())
            .unwrap();

        assert_eq!(*selection.item, 8);
        assert_eq!(selection.score, 8.0);
        assert_eq!(selection.index, 3);
    }

    #[test]
    fn test_threshold() {
        let objective = objective_fn(|n: &i32, _ctx: &ObjectiveContext| *n as f64);

        let candidates = vec![1, 5, 3, 8, 2];
        let context = ObjectiveContext::new().with_min_score(4.0);
        let selection = objective.select(&candidates, &context).unwrap();

        assert_eq!(*selection.item, 8);
        assert_eq!(selection.passed, 2);
    }

    #[test]
    fn test_no_candidates() {
        let objective = objective_fn(|n: &i32, _ctx: &ObjectiveContext| *n as f64);

        let candidates: Vec<i32> = vec![];
        let result = objective.select(&candidates, &ObjectiveContext::new());

        assert!(matches!(result, Err(ObjectiveError::NoCandidates)));
    }

    #[test]
    fn test_no_match() {
        let objective = objective_fn(|n: &i32, _ctx: &ObjectiveContext| *n as f64);

        let candidates = vec![1, 2, 3];
        let context = ObjectiveContext::new().with_min_score(10.0);
        let result = objective.select(&candidates, &context);

        assert!(matches!(result, Err(ObjectiveError::NoMatch(_))));
    }

    #[test]
    fn test_select_top() {
        let objective = objective_fn(|n: &i32, _ctx: &ObjectiveContext| *n as f64);

        let candidates = vec![1, 5, 3, 8, 2];
        let top = objective.select_top(&candidates, 3, &ObjectiveContext::new());

        assert_eq!(top.len(), 3);
        assert_eq!(*top[0].item, 8);
        assert_eq!(*top[1].item, 5);
        assert_eq!(*top[2].item, 3);
    }

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
        let selection = objective.select(&candidates, &context).unwrap();

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

    #[test]
    fn test_deterministic_select_basic() {
        let objective = objective_fn(|c: &TestCandidate, _ctx: &ObjectiveContext| c.value as f64);

        let candidates = vec![
            TestCandidate::new(1),
            TestCandidate::new(5),
            TestCandidate::new(3),
        ];

        let selection = objective
            .select_deterministic(&candidates, &ObjectiveContext::new())
            .unwrap();

        assert_eq!(selection.item.value, 5);
        assert_eq!(selection.score, 5.0);
    }

    #[test]
    fn test_deterministic_select_equal_scores_uses_uuid_tiebreaker() {
        let objective = objective_fn(|_c: &TestCandidate, _ctx: &ObjectiveContext| 1.0);

        let id1 = Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap();
        let id2 = Uuid::parse_str("00000000-0000-0000-0000-000000000002").unwrap();
        let id3 = Uuid::parse_str("00000000-0000-0000-0000-000000000003").unwrap();

        let candidates = vec![
            TestCandidate::with_id(id2, 100),
            TestCandidate::with_id(id3, 200),
            TestCandidate::with_id(id1, 300),
        ];

        let selection = objective
            .select_deterministic(&candidates, &ObjectiveContext::new())
            .unwrap();

        assert_eq!(selection.item.id, id1);
        assert_eq!(selection.item.value, 300);
    }

    #[test]
    fn test_deterministic_select_top_ordering() {
        let objective = objective_fn(|_c: &TestCandidate, _ctx: &ObjectiveContext| 1.0);

        let id1 = Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap();
        let id2 = Uuid::parse_str("00000000-0000-0000-0000-000000000002").unwrap();
        let id3 = Uuid::parse_str("00000000-0000-0000-0000-000000000003").unwrap();

        let candidates = vec![
            TestCandidate::with_id(id3, 300),
            TestCandidate::with_id(id1, 100),
            TestCandidate::with_id(id2, 200),
        ];

        let top = objective.select_top_deterministic(&candidates, 3, &ObjectiveContext::new());

        assert_eq!(top.len(), 3);
        assert_eq!(top[0].item.id, id1);
        assert_eq!(top[1].item.id, id2);
        assert_eq!(top[2].item.id, id3);
    }

    #[test]
    fn test_deterministic_reproducibility() {
        let objective = objective_fn(|_c: &TestCandidate, _ctx: &ObjectiveContext| 1.0);

        let id1 = Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap();
        let id2 = Uuid::parse_str("00000000-0000-0000-0000-000000000002").unwrap();
        let id3 = Uuid::parse_str("00000000-0000-0000-0000-000000000003").unwrap();

        let candidates = vec![
            TestCandidate::with_id(id2, 1),
            TestCandidate::with_id(id3, 2),
            TestCandidate::with_id(id1, 3),
        ];

        for _ in 0..100 {
            let selection = objective
                .select_deterministic(&candidates, &ObjectiveContext::new())
                .unwrap();
            assert_eq!(selection.item.id, id1, "Determinism violated!");

            let top = objective.select_top_deterministic(&candidates, 3, &ObjectiveContext::new());
            assert_eq!(top[0].item.id, id1);
            assert_eq!(top[1].item.id, id2);
            assert_eq!(top[2].item.id, id3);
        }
    }
}
