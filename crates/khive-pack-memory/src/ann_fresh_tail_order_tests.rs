//! Ordering and scoring rules of the fresh-tail merge that memory recall uses:
//! equal scores are ordered by ascending id, and a stored embedding whose
//! length differs from the query's scores 0.0.

use uuid::Uuid;

use crate::ann::{exact_cosine, merge_fresh_tail};

/// Equal-score tail upserts come back in ascending id order on every call, not
/// in the order a hash map happens to yield them.
#[test]
fn merge_fresh_tail_orders_equal_score_tail_upserts_by_ascending_id() {
    let mut ids: Vec<Uuid> = Vec::new();
    for n in 0..16u128 {
        ids.push(Uuid::from_u128(0xA0 + n));
    }
    for _ in 0..8 {
        let mut ops: Vec<(Uuid, Option<Vec<f32>>)> = Vec::new();
        for id in ids.iter().rev() {
            ops.push((*id, Some(vec![1.0, 0.0])));
        }
        let merged = merge_fresh_tail(Vec::new(), &[1.0, 0.0], ops);
        assert!(merged.iter().all(|(_, score)| *score == 1.0));
        let merged_ids: Vec<Uuid> = merged.iter().map(|(id, _)| *id).collect();
        assert_eq!(merged_ids, ids);
    }
}

/// A carried candidate and a tail upsert with equal scores are ordered by id
/// whichever side supplied them, and candidates with different scores keep
/// descending score order.
#[test]
fn merge_fresh_tail_orders_equal_scores_across_sources_by_id() {
    let tail_low = Uuid::from_u128(1);
    let tail_zero = Uuid::from_u128(2);
    let c_low = Uuid::from_u128(3);
    let c_mid = Uuid::from_u128(5);
    let c_high = Uuid::from_u128(9);
    let carried = vec![(c_high, 1.0), (c_mid, 0.75), (c_low, 0.25)];
    let ops = vec![
        (tail_low, Some(vec![1.0, 0.0])),
        (tail_zero, Some(vec![0.0, 1.0])),
    ];
    let merged = merge_fresh_tail(carried, &[1.0, 0.0], ops);
    let expected = vec![
        (tail_low, 1.0_f32),
        (c_high, 1.0),
        (c_mid, 0.75),
        (c_low, 0.25),
        (tail_zero, 0.0),
    ];
    assert_eq!(merged, expected);
}

/// A stored embedding whose length differs from the query's, and an empty
/// query, score 0.0; equal lengths still score by cosine.
#[test]
fn exact_cosine_scores_length_mismatch_and_empty_query_as_zero() {
    assert_eq!(exact_cosine(&[1.0, 0.0], &[1.0, 0.0, 0.0]), 0.0);
    assert_eq!(exact_cosine(&[1.0, 0.0, 0.0], &[1.0, 0.0]), 0.0);
    assert_eq!(exact_cosine(&[], &[1.0, 0.0]), 0.0);
    assert_eq!(exact_cosine(&[], &[]), 0.0);
    assert_eq!(exact_cosine(&[1.0, 0.0], &[1.0, 0.0]), 1.0);
}

/// A tail upsert whose embedding length differs from the query's scores 0.0,
/// so it ranks below a positively scored candidate instead of matching on the
/// shared prefix.
#[test]
fn merge_fresh_tail_scores_a_mismatched_dimension_upsert_as_zero() {
    let carried = Uuid::from_u128(2);
    let mismatched = Uuid::from_u128(1);
    let merged = merge_fresh_tail(
        vec![(carried, 0.5)],
        &[1.0, 0.0],
        vec![(mismatched, Some(vec![1.0, 0.0, 0.0]))],
    );
    let expected = vec![(carried, 0.5_f32), (mismatched, 0.0)];
    assert_eq!(merged, expected);
}
