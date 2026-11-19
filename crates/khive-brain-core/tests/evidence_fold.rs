use std::collections::BTreeMap;

use khive_brain_core::{BetaPosterior, EntityPosteriors};
use khive_types::EdgeRelation;
use uuid::Uuid;

fn id(value: u128) -> Uuid {
    Uuid::from_u128(value)
}

fn observed(state: &EntityPosteriors) -> (Vec<u8>, Vec<Uuid>, usize) {
    let values: BTreeMap<_, _> = state
        .to_snapshot()
        .into_iter()
        .map(|(id, p)| (id, (p.alpha().to_bits(), p.beta().to_bits())))
        .collect();
    (
        serde_json::to_vec(&values).unwrap(),
        state.order(),
        state.capacity(),
    )
}

fn full_map() -> EntityPosteriors {
    let mut state = EntityPosteriors::new(2);
    state.get_or_insert(id(1), || BetaPosterior::new(2.5, 4.5));
    state.get_or_insert(id(2), || BetaPosterior::new(6.0, 8.0));
    assert_eq!(state.len(), state.capacity());
    assert_eq!(state.order(), [id(1), id(2)]);
    state
}

fn assert_rows(state: &EntityPosteriors, expected: &[(u128, f64, f64)]) {
    assert_eq!(state.len(), expected.len());
    for &(key, alpha, beta) in expected {
        let posterior = state.get(&id(key)).expect("expected claim must exist");
        assert_eq!(posterior.alpha().to_bits(), alpha.to_bits());
        assert_eq!(posterior.beta().to_bits(), beta.to_bits());
        posterior.validate().unwrap();
    }
}

#[test]
fn evidence_targets_one_claim_without_resetting_its_existing_prior() {
    let mut state = full_map();
    state
        .fold_evidence(EdgeRelation::Supports, 0.5, id(1))
        .unwrap();
    assert_rows(&state, &[(1, 3.0, 4.5), (2, 6.0, 8.0)]);
    assert_eq!(state.order(), [id(2), id(1)]);
    state
        .fold_evidence(EdgeRelation::Refutes, 0.25, id(2))
        .unwrap();
    assert_rows(&state, &[(1, 3.0, 4.5), (2, 6.0, 8.25)]);
    assert_eq!(state.order(), [id(1), id(2)]);
}

#[test]
fn every_other_relation_refuses_without_insertion_promotion_or_eviction() {
    for relation in EdgeRelation::ALL {
        if matches!(relation, EdgeRelation::Supports | EdgeRelation::Refutes) {
            continue;
        }
        for claim in [id(1), id(2), id(3)] {
            let mut state = full_map();
            let before = observed(&state);
            let error = state.fold_evidence(relation, 0.5, claim).unwrap_err();
            assert!(error.contains("supports or refutes"), "{error}");
            assert!(error.contains(&relation.to_string()), "{error}");
            assert_eq!(observed(&state), before, "{relation}, {claim}");
        }
    }
}

#[test]
fn invalid_weights_refuse_without_changing_full_map_or_lru() {
    let invalid = [
        f64::NAN,
        f64::INFINITY,
        f64::NEG_INFINITY,
        -f64::from_bits(1),
        -f64::MIN_POSITIVE,
        -1.0,
        f64::from_bits(1.0_f64.to_bits() + 1),
        1.1,
        f64::MAX,
    ];
    for relation in [EdgeRelation::Supports, EdgeRelation::Refutes] {
        for weight in invalid {
            for claim in [id(1), id(2), id(3)] {
                let mut state = full_map();
                let before = observed(&state);
                let error = state.fold_evidence(relation, weight, claim).unwrap_err();
                assert!(error.contains("weight must be finite"), "{error}");
                assert!(error.contains("[0.0, 1.0]"), "{error}");
                assert_eq!(observed(&state), before, "{relation}, {weight:?}, {claim}");
            }
        }
    }
}

#[test]
fn relation_validation_precedes_weight_validation_and_zero_short_circuit() {
    for weight in [f64::NAN, f64::INFINITY, 0.0, -0.0] {
        let mut state = full_map();
        let before = observed(&state);
        assert_eq!(
            state
                .fold_evidence(EdgeRelation::DependsOn, weight, id(3))
                .unwrap_err(),
            "fold_evidence: relation must be supports or refutes, got depends_on"
        );
        assert_eq!(observed(&state), before);
    }
}

#[test]
fn signed_zero_is_mutation_free_for_missing_and_present_claims() {
    for relation in [EdgeRelation::Supports, EdgeRelation::Refutes] {
        for weight in [0.0, -0.0] {
            for claim in [id(1), id(2), id(3)] {
                let mut state = full_map();
                let before = observed(&state);
                state.fold_evidence(relation, weight, claim).unwrap();
                assert_eq!(observed(&state), before, "{relation}, {weight:?}, {claim}");
            }
            for capacity in [0, 2] {
                let mut state = EntityPosteriors::new(capacity);
                let before = observed(&state);
                state.fold_evidence(relation, weight, id(1)).unwrap();
                assert!(state.is_empty());
                assert_eq!(observed(&state), before);
            }
        }
    }
}

#[test]
fn unit_weight_is_accepted_for_both_polarities() {
    let mut state = EntityPosteriors::new(2);
    state
        .fold_evidence(EdgeRelation::Supports, 1.0, id(1))
        .unwrap();
    state
        .fold_evidence(EdgeRelation::Refutes, 1.0, id(2))
        .unwrap();
    assert_rows(&state, &[(1, 2.0, 1.0), (2, 1.0, 2.0)]);
}

#[test]
fn the_smallest_positive_weight_is_not_treated_as_zero() {
    let tiny = f64::from_bits(1);
    for relation in [EdgeRelation::Supports, EdgeRelation::Refutes] {
        let mut state = EntityPosteriors::new(2);
        state.get_or_insert(id(1), || BetaPosterior::new(tiny, tiny));
        state.fold_evidence(relation, tiny, id(1)).unwrap();
        let posterior = state.get(&id(1)).unwrap();
        let expected = if relation == EdgeRelation::Supports {
            (2, 1)
        } else {
            (1, 2)
        };
        assert_eq!(posterior.alpha().to_bits(), expected.0);
        assert_eq!(posterior.beta().to_bits(), expected.1);
        state.fold_evidence(relation, tiny, id(2)).unwrap();
        assert_eq!(state.len(), 2, "positive evidence creates an absent claim");
        let default = state.get(&id(2)).unwrap();
        assert_eq!((default.alpha(), default.beta()), (1.0, 1.0));
        assert_eq!(state.order(), [id(1), id(2)]);
    }
}

#[test]
fn mixed_sequence_matches_literal_snapshots_at_every_prefix() {
    let mut state = EntityPosteriors::new(2);
    assert!(state.is_empty());
    struct Step {
        relation: EdgeRelation,
        weight: f64,
        claim: u128,
        expected: &'static [(u128, f64, f64)],
        order: &'static [u128],
    }
    let steps = [
        Step {
            relation: EdgeRelation::Supports,
            weight: 0.5,
            claim: 1,
            expected: &[(1, 1.5, 1.0)],
            order: &[1],
        },
        Step {
            relation: EdgeRelation::Refutes,
            weight: 0.25,
            claim: 2,
            expected: &[(1, 1.5, 1.0), (2, 1.0, 1.25)],
            order: &[1, 2],
        },
        Step {
            relation: EdgeRelation::Refutes,
            weight: 0.5,
            claim: 1,
            expected: &[(1, 1.5, 1.5), (2, 1.0, 1.25)],
            order: &[2, 1],
        },
        Step {
            relation: EdgeRelation::Supports,
            weight: 1.0,
            claim: 3,
            expected: &[(1, 1.5, 1.5), (3, 2.0, 1.0)],
            order: &[1, 3],
        },
        Step {
            relation: EdgeRelation::Refutes,
            weight: 1.0,
            claim: 3,
            expected: &[(1, 1.5, 1.5), (3, 2.0, 2.0)],
            order: &[1, 3],
        },
        Step {
            relation: EdgeRelation::Supports,
            weight: 0.25,
            claim: 1,
            expected: &[(1, 1.75, 1.5), (3, 2.0, 2.0)],
            order: &[3, 1],
        },
    ];
    for step in steps {
        state
            .fold_evidence(step.relation, step.weight, id(step.claim))
            .unwrap();
        assert_rows(&state, step.expected);
        assert_eq!(
            state.order(),
            step.order.iter().copied().map(id).collect::<Vec<_>>()
        );
    }
}

#[test]
fn snapshot_restore_preserves_evidence_and_the_next_eviction() {
    let mut live = full_map();
    live.fold_evidence(EdgeRelation::Supports, 0.5, id(1))
        .unwrap();
    let snapshot = live.to_snapshot();
    let bytes = serde_json::to_vec(&snapshot).unwrap();
    let decoded = serde_json::from_slice(&bytes).unwrap();
    let mut restored = EntityPosteriors::from_snapshot(decoded, live.order(), live.capacity());
    assert_eq!(observed(&restored), observed(&live));
    for state in [&mut live, &mut restored] {
        state
            .fold_evidence(EdgeRelation::Refutes, 0.75, id(3))
            .unwrap();
        assert_rows(state, &[(1, 3.0, 4.5), (3, 1.0, 1.75)]);
        assert!(state.get(&id(2)).is_none());
        assert_eq!(state.order(), [id(1), id(3)]);
    }
    assert_eq!(observed(&restored), observed(&live));
}

#[test]
fn zero_requested_capacity_keeps_existing_single_entry_policy() {
    let mut state = EntityPosteriors::new(0);
    assert_eq!(state.capacity(), 1);
    state
        .fold_evidence(EdgeRelation::Supports, 0.5, id(1))
        .unwrap();
    state
        .fold_evidence(EdgeRelation::Refutes, 0.5, id(2))
        .unwrap();
    assert_rows(&state, &[(2, 1.0, 1.5)]);
    assert_eq!(state.order(), [id(2)]);
    assert!(state.get(&id(1)).is_none());
}
