use khive_hnsw::error::{ErrorKind, RetrievalError};
use khive_hnsw::NodeId;
use khive_hnsw::{HnswConfig, HnswIndex};

fn make_id(seed: u8) -> NodeId {
    NodeId::new([seed; 16])
}

#[test]
fn test_no_budget_allows_unlimited_inserts() {
    // Without a budget, inserts always succeed
    let mut index = HnswIndex::new(4);
    for i in 0..50 {
        let id = make_id(i);
        index
            .insert(id, vec![i as f32, 0.0, 0.0, 0.0])
            .expect("insert should succeed without budget");
    }
    assert_eq!(index.len(), 50);
}

#[test]
fn test_budget_blocks_insert_when_exceeded() {
    // Set a very tight budget that allows only a few nodes
    let config = HnswConfig::with_dimensions(4).with_memory_budget(2_000);
    let mut index = HnswIndex::with_config(config);

    // First insert should succeed (index starts empty)
    index
        .insert(make_id(1), vec![1.0, 0.0, 0.0, 0.0])
        .expect("first insert should succeed");

    // Keep inserting until we hit the budget
    let mut rejected = false;
    for i in 2..=100u8 {
        let result = index.insert(make_id(i), vec![i as f32, 0.0, 0.0, 0.0]);
        if let Err(err) = result {
            rejected = true;
            assert!(
                matches!(err, RetrievalError::BudgetExceeded { .. }),
                "Expected BudgetExceeded, got: {err:?}"
            );
            assert_eq!(err.kind(), ErrorKind::Permanent);
            assert!(!err.is_retryable());
            break;
        }
    }
    assert!(rejected, "Budget should have rejected an insert");
}

#[test]
fn test_budget_update_bypasses_check() {
    // Set a budget, fill it up, then update an existing entry
    let config = HnswConfig::with_dimensions(4).with_memory_budget(2_000);
    let mut index = HnswIndex::with_config(config);

    let id1 = make_id(1);
    index
        .insert(id1, vec![1.0, 0.0, 0.0, 0.0])
        .expect("first insert");

    // Fill until budget hit
    for i in 2..=100u8 {
        if index
            .insert(make_id(i), vec![i as f32, 0.0, 0.0, 0.0])
            .is_err()
        {
            break;
        }
    }

    // Updating an existing entry should always succeed (bypass budget)
    index
        .insert(id1, vec![9.0, 9.0, 9.0, 9.0])
        .expect("update existing should bypass budget");
}

#[test]
fn test_memory_usage_increases_with_inserts() {
    let mut index = HnswIndex::new(8);

    let before = index.memory_usage();
    assert_eq!(before, 0, "Empty index should have zero usage");

    index.insert(make_id(1), vec![1.0; 8]).expect("insert");
    let after_one = index.memory_usage();
    assert!(after_one > 0, "Usage should increase after insert");

    index.insert(make_id(2), vec![2.0; 8]).expect("insert");
    let after_two = index.memory_usage();
    assert!(
        after_two > after_one,
        "Usage should increase with more inserts"
    );
}

#[test]
fn test_estimate_insert_cost_is_positive() {
    let index = HnswIndex::new(128);
    let cost = index.estimate_insert_cost();
    assert!(cost > 0, "Insert cost should be positive");
    // For 128 dims: 128*4 = 512 bytes for vector alone
    assert!(cost >= 512, "Cost should include at least the vector data");
}

#[test]
fn test_memory_budget_getter_setter() {
    let mut index = HnswIndex::new(4);

    // Default: no budget
    assert_eq!(index.memory_budget(), None);

    // Set budget via runtime setter
    index.set_memory_budget(Some(10_000));
    assert_eq!(index.memory_budget(), Some(10_000));

    // Clear budget
    index.set_memory_budget(None);
    assert_eq!(index.memory_budget(), None);
}

#[test]
fn test_budget_from_config() {
    let config = HnswConfig::with_dimensions(4).with_memory_budget(5_000);
    let index = HnswIndex::with_config(config);
    assert_eq!(index.memory_budget(), Some(5_000));
}

#[test]
fn test_budget_exceeded_error_details() {
    let config = HnswConfig::with_dimensions(4).with_memory_budget(1);
    let mut index = HnswIndex::with_config(config);

    // Budget of 1 byte is too small for any insert
    let result = index.insert(make_id(1), vec![1.0, 0.0, 0.0, 0.0]);
    assert!(result.is_err());

    let err = result.unwrap_err();
    match err {
        RetrievalError::BudgetExceeded {
            current_usage,
            item_size,
            limit,
        } => {
            assert_eq!(current_usage, 0, "Empty index");
            assert!(item_size > 0, "Item should have non-zero cost");
            assert_eq!(limit, 1, "Limit should match config");
            assert!(current_usage + item_size > limit, "Should genuinely exceed");
        }
        other => panic!("Expected BudgetExceeded, got: {other:?}"),
    }
}

#[test]
fn test_search_unaffected_by_budget() {
    // Budget is only checked on insert, never on search
    let config = HnswConfig::with_dimensions(3).with_memory_budget(100_000);
    let mut index = HnswIndex::with_config(config);

    index
        .insert(make_id(1), vec![1.0, 0.0, 0.0])
        .expect("insert");
    index
        .insert(make_id(2), vec![0.0, 1.0, 0.0])
        .expect("insert");

    // Search should work regardless of budget
    let results = index.search(&[1.0, 0.0, 0.0], 2).expect("search");
    assert_eq!(results.len(), 2);
}
