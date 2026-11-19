use super::*;
use crate::HnswIndex;

fn make_test_index() -> HnswIndex {
    let mut index = HnswIndex::new(4);
    // Insert 10 vectors
    for i in 0..10u8 {
        let id = NodeId::new([i; 16]);
        let vec = vec![i as f32; 4];
        index.insert(id, vec).unwrap();
    }
    index
}

#[test]
fn test_noop_validator() {
    let index = make_test_index();
    let validator = NoopValidator;
    assert!(validator.validate(&index).is_ok());
}

#[test]
fn test_recall_validator_empty_golden() {
    let index = make_test_index();
    let validator = RecallValidator::new(vec![], 5, 0.95);
    assert!(validator.validate(&index).is_ok());
}

#[test]
fn test_recall_validator_perfect_recall() {
    let index = make_test_index();

    // Search for the actual results first, then use those as golden truth
    let query = vec![5.0f32; 4];
    let results = index.search(&query, 3).unwrap();
    let expected: Vec<NodeId> = results.iter().map(|(id, _)| *id).collect();
    assert!(!expected.is_empty(), "search should return results");

    // Validator with the actual results as golden truth should pass
    let validator = RecallValidator::new(vec![(query, expected)], 3, 0.95);
    assert!(validator.validate(&index).is_ok());
}

#[test]
fn test_recall_validator_fails_low_recall() {
    let index = make_test_index();

    // Expect IDs that don't exist in the index
    let query = vec![5.0f32; 4];
    let fake_ids: Vec<NodeId> = (100..110u8).map(|i| NodeId::new([i; 16])).collect();

    let validator = RecallValidator::new(vec![(query, fake_ids)], 5, 0.95);
    let result = validator.validate(&index);
    assert!(result.is_err());

    match result.unwrap_err() {
        AliasError::ValidationFailed {
            recall, min_recall, ..
        } => {
            assert_eq!(recall, Some(0.0));
            assert_eq!(min_recall, Some(0.95));
        }
        other => panic!("Expected ValidationFailed, got: {other:?}"),
    }
}
