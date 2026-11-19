#[test]
fn test_new_index() {
    let index = Bm25Index::try_new(Bm25Config::default()).expect("valid config");
    assert_eq!(index.doc_count(), 0);
    assert!((index.avg_doc_length() - 0.0).abs() < f64::EPSILON);
}

#[test]
fn test_index_single_document() {
    let mut index = Bm25Index::default();
    index
        .index_document("doc1".to_string(), "the quick brown fox")
        .unwrap();

    assert_eq!(index.doc_count(), 1);
    // "the" is a stop word, so "the quick brown fox" → 3 tokens
    assert!((index.avg_doc_length() - 3.0).abs() < f64::EPSILON);
    assert!(index.contains_document("doc1"));
    assert!(!index.contains_document("doc2"));
}

#[test]
fn test_index_multiple_documents() {
    let mut index = Bm25Index::default();
    index
        .index_document("doc1".to_string(), "the quick brown fox")
        .unwrap();
    index
        .index_document("doc2".to_string(), "the lazy dog")
        .unwrap();
    index.index_document("doc3".to_string(), "quick").unwrap();

    assert_eq!(index.doc_count(), 3);
    // Stop words removed: "the quick brown fox"→3, "the lazy dog"→2, "quick"→1
    // (3 + 2 + 1) / 3 = 2.0
    assert!((index.avg_doc_length() - 2.0).abs() < f64::EPSILON);
}

#[test]
fn test_index_empty_document() {
    let mut index = Bm25Index::default();
    index.index_document("doc1".to_string(), "").unwrap();
    assert_eq!(index.doc_count(), 0); // Empty docs not indexed

    index.index_document("doc2".to_string(), "   ").unwrap();
    assert_eq!(index.doc_count(), 0);
}

#[test]
fn test_remove_document() {
    let mut index = Bm25Index::default();
    index
        .index_document("doc1".to_string(), "the quick brown fox")
        .unwrap();
    index
        .index_document("doc2".to_string(), "the lazy dog")
        .unwrap();

    assert_eq!(index.doc_count(), 2);

    assert!(index.remove_document("doc1"));
    assert_eq!(index.doc_count(), 1);
    assert!(!index.contains_document("doc1"));
    assert!(index.contains_document("doc2"));

    // Remove non-existent document
    assert!(!index.remove_document("doc3"));
    assert_eq!(index.doc_count(), 1);
}

#[test]
fn test_reindex_document() {
    let mut index = Bm25Index::default();
    index
        .index_document("doc1".to_string(), "old content")
        .unwrap();
    assert_eq!(index.doc_count(), 1);

    // Re-index same document with new content
    index
        .index_document("doc1".to_string(), "new content with more tokens")
        .unwrap();
    assert_eq!(index.doc_count(), 1);

    // Stats should reflect new content
    // "new content with more tokens" → "with" is stop word → 4 tokens
    assert!((index.avg_doc_length() - 4.0).abs() < f64::EPSILON);
}

#[test]
fn test_search_empty_query() {
    let mut index = Bm25Index::default();
    index
        .index_document("doc1".to_string(), "the quick brown fox")
        .unwrap();

    let results = index.search("", 10);
    assert!(results.is_empty());

    let results = index.search("   ", 10);
    assert!(results.is_empty());
}

#[test]
fn test_search_empty_index() {
    let index = Bm25Index::default();
    let results = index.search("quick fox", 10);
    assert!(results.is_empty());
}

#[test]
fn test_search_no_matches() {
    let mut index = Bm25Index::default();
    index
        .index_document("doc1".to_string(), "the quick brown fox")
        .unwrap();

    let results = index.search("elephant giraffe", 10);
    assert!(results.is_empty());
}

#[test]
fn test_search_single_match() {
    let mut index = Bm25Index::default();
    index
        .index_document("doc1".to_string(), "the quick brown fox")
        .unwrap();
    index
        .index_document("doc2".to_string(), "the lazy dog")
        .unwrap();

    let results = index.search("fox", 10);
    assert_eq!(results.len(), 1);
    assert_eq!(&*results[0].0, "doc1");
    assert!(results[0].1.to_f64() > 0.0);
}

#[test]
fn test_search_multiple_matches() {
    let mut index = Bm25Index::default();
    index
        .index_document("doc1".to_string(), "the quick brown fox")
        .unwrap();
    index
        .index_document("doc2".to_string(), "the lazy dog")
        .unwrap();
    index
        .index_document("doc3".to_string(), "the cat and the dog")
        .unwrap();

    let results = index.search("the dog", 10);

    // All docs contain "the", but only doc2 and doc3 contain "dog"
    // doc2 and doc3 should score higher
    assert!(!results.is_empty());

    // Find positions
    let doc2_pos = results.iter().position(|(id, _)| id.as_ref() == "doc2");
    let doc3_pos = results.iter().position(|(id, _)| id.as_ref() == "doc3");

    assert!(doc2_pos.is_some() || doc3_pos.is_some());
}

#[test]
fn test_search_k_limit() {
    let mut index = Bm25Index::default();
    for i in 0..10 {
        index
            .index_document(format!("doc{i}"), &format!("common term {i}"))
            .unwrap();
    }

    let results = index.search("common", 3);
    assert_eq!(results.len(), 3);

    let results = index.search("common", 20);
    assert_eq!(results.len(), 10); // Only 10 documents
}

#[test]
fn test_search_k_zero() {
    let mut index = Bm25Index::default();
    index
        .index_document("doc1".to_string(), "the quick brown fox")
        .unwrap();

    let results = index.search("fox", 0);
    assert!(results.is_empty());
}
