use khive_bm25::error::{ErrorKind, RetrievalError};
use khive_bm25::{Bm25Config, Bm25Index};

#[test]
fn test_no_budget_allows_unlimited_indexing() {
    let mut index = Bm25Index::default();
    for i in 0..100 {
        index
            .index_document(format!("doc{i}"), &format!("content words number {i}"))
            .expect("index should succeed without budget");
    }
    assert_eq!(index.doc_count(), 100);
}

#[test]
fn test_budget_blocks_new_document_when_exceeded() {
    let config = Bm25Config::default().with_memory_budget(1_100);
    let mut index = Bm25Index::try_new(config).expect("valid config");

    // First doc should succeed (index starts empty)
    index
        .index_document("doc1", "hello world")
        .expect("first doc should succeed");

    // Keep indexing until budget is hit
    let mut rejected = false;
    for i in 2..=200 {
        let result = index.index_document(
            format!("doc{i}"),
            &format!("some content words for document number {i} with extra text"),
        );
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
    assert!(
        rejected,
        "Budget should have rejected an index_document call"
    );
}

#[test]
fn test_budget_reindex_bypasses_check() {
    let config = Bm25Config::default().with_memory_budget(2_000);
    let mut index = Bm25Index::try_new(config).expect("valid config");

    // Index initial doc
    index
        .index_document("doc1", "initial content")
        .expect("first doc");

    // Fill until budget hit
    for i in 2..=500 {
        if index
            .index_document(format!("doc{i}"), &format!("fill content {i}"))
            .is_err()
        {
            break;
        }
    }

    // Re-indexing an existing document should bypass the budget
    index
        .index_document("doc1", "updated content with more words")
        .expect("re-index should bypass budget");
}

#[test]
fn test_memory_usage_increases_with_documents() {
    let mut index = Bm25Index::default();

    let before = index.memory_usage();
    // Empty index has fixed overhead only
    assert!(before >= 128, "Empty index should have fixed overhead");

    index.index_document("doc1", "hello world").unwrap();
    let after_one = index.memory_usage();
    assert!(after_one > before, "Usage should increase after indexing");

    index
        .index_document("doc2", "another document here")
        .unwrap();
    let after_two = index.memory_usage();
    assert!(
        after_two > after_one,
        "Usage should increase with more docs"
    );
}

#[test]
fn test_estimate_document_cost_is_positive() {
    let index = Bm25Index::default();
    let cost = index.estimate_document_cost("some test document with words");
    assert!(cost > 0, "Document cost should be positive");
}

#[test]
fn test_estimate_document_cost_empty_text() {
    let index = Bm25Index::default();
    let cost = index.estimate_document_cost("");
    assert_eq!(cost, 0, "Empty document should have zero cost");
}

#[test]
fn test_memory_budget_getter_setter() {
    let mut index = Bm25Index::default();

    // Default: no budget
    assert_eq!(index.memory_budget(), None);

    // Set budget at runtime
    index.set_memory_budget(Some(50_000));
    assert_eq!(index.memory_budget(), Some(50_000));

    // Clear budget
    index.set_memory_budget(None);
    assert_eq!(index.memory_budget(), None);
}

#[test]
fn test_budget_from_config() {
    let config = Bm25Config::default().with_memory_budget(10_000);
    let index = Bm25Index::try_new(config).expect("valid config");
    assert_eq!(index.memory_budget(), Some(10_000));
}

#[test]
fn test_budget_exceeded_error_details() {
    let config = Bm25Config::default().with_memory_budget(1);
    let mut index = Bm25Index::try_new(config).expect("valid config");

    // Budget of 1 byte is too small for any document
    let result = index.index_document("doc1", "hello world");
    assert!(result.is_err());

    let err = result.unwrap_err();
    match err {
        RetrievalError::BudgetExceeded {
            current_usage,
            item_size,
            limit,
        } => {
            assert!(item_size > 0, "Item should have non-zero cost");
            assert_eq!(limit, 1, "Limit should match config");
            assert!(current_usage + item_size > limit, "Should genuinely exceed");
        }
        other => panic!("Expected BudgetExceeded, got: {other:?}"),
    }
}

#[test]
fn test_search_unaffected_by_budget() {
    let config = Bm25Config::default().with_memory_budget(100_000);
    let mut index = Bm25Index::try_new(config).expect("valid config");

    index.index_document("doc1", "quick brown fox").unwrap();
    index.index_document("doc2", "lazy brown dog").unwrap();

    // Search should work regardless of budget
    let results = index.search("brown", 10);
    assert_eq!(results.len(), 2);
}

#[test]
fn test_budget_allows_removal_then_insert() {
    let config = Bm25Config::default().with_memory_budget(3_000);
    let mut index = Bm25Index::try_new(config).expect("valid config");

    // Fill the index
    let mut last_success = 0;
    for i in 1..=500 {
        if index
            .index_document(format!("doc{i}"), &format!("content {i}"))
            .is_ok()
        {
            last_success = i;
        } else {
            break;
        }
    }
    assert!(last_success > 0, "Should have indexed at least one doc");

    // Remove some documents to free memory
    for i in 1..=(last_success / 2) {
        index.remove_document(&format!("doc{i}"));
    }

    // Now we should be able to insert again
    let result = index.index_document("new_doc", "newly inserted content");
    assert!(
        result.is_ok(),
        "Should be able to insert after removing docs"
    );
}
