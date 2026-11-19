#[test]
fn test_term_frequency_matters() {
    let mut index = Bm25Index::default();
    index.index_document("doc1".to_string(), "fox").unwrap();
    index
        .index_document("doc2".to_string(), "fox fox fox")
        .unwrap();

    let results = index.search("fox", 10);
    assert_eq!(results.len(), 2);

    // doc2 has higher TF, should score higher (but with saturation)
    let doc1_score = results
        .iter()
        .find(|(id, _)| id.as_ref() == "doc1")
        .unwrap()
        .1;
    let doc2_score = results
        .iter()
        .find(|(id, _)| id.as_ref() == "doc2")
        .unwrap()
        .1;
    assert!(doc2_score > doc1_score);
}

#[test]
fn test_length_normalization() {
    let mut index = Bm25Index::default();
    // Both have "fox" once, but different lengths
    index.index_document("short".to_string(), "fox").unwrap();
    index
        .index_document(
            "long".to_string(),
            "the quick brown fox jumps over the lazy dog",
        )
        .unwrap();

    let results = index.search("fox", 10);
    assert_eq!(results.len(), 2);

    // Shorter doc should score higher (with b=0.75 normalization)
    let short_score = results
        .iter()
        .find(|(id, _)| id.as_ref() == "short")
        .unwrap()
        .1;
    let long_score = results
        .iter()
        .find(|(id, _)| id.as_ref() == "long")
        .unwrap()
        .1;
    assert!(short_score > long_score);
}

#[test]
fn test_idf_rare_terms() {
    let mut index = Bm25Index::default();
    // "rare" appears in 1 doc, "common" in all
    index
        .index_document("doc1".to_string(), "common rare")
        .unwrap();
    index.index_document("doc2".to_string(), "common").unwrap();
    index.index_document("doc3".to_string(), "common").unwrap();

    // Search for rare term should only return doc1
    let results = index.search("rare", 10);
    assert_eq!(results.len(), 1);
    assert_eq!(&*results[0].0, "doc1");

    // doc1 should score high because "rare" has high IDF
    assert!(results[0].1.to_f64() > 0.0);
}

#[test]
fn test_multi_term_query() {
    let mut index = Bm25Index::default();
    index
        .index_document("doc1".to_string(), "quick brown fox")
        .unwrap();
    index
        .index_document("doc2".to_string(), "quick dog")
        .unwrap();
    index
        .index_document("doc3".to_string(), "brown dog")
        .unwrap();

    let results = index.search("quick brown", 10);

    // doc1 has both terms, should score highest
    assert!(!results.is_empty());
    assert_eq!(&*results[0].0, "doc1");
}

#[test]
fn test_clear() {
    let mut index = Bm25Index::default();
    index
        .index_document("doc1".to_string(), "the quick brown fox")
        .unwrap();
    index
        .index_document("doc2".to_string(), "the lazy dog")
        .unwrap();

    index.clear();

    assert_eq!(index.doc_count(), 0);
    assert!(index.search("fox", 10).is_empty());
}

#[test]
fn test_stats() {
    let mut index = Bm25Index::default();
    index
        .index_document("doc1".to_string(), "the quick brown fox")
        .unwrap();
    index
        .index_document("doc2".to_string(), "the lazy dog")
        .unwrap();

    let stats = index.stats();
    assert_eq!(stats.doc_count, 2);
    // Stop words removed: "the quick brown fox"→3, "the lazy dog"→2 = 5 total
    assert_eq!(stats.total_tokens, 5);
    assert!((stats.avg_doc_length - 2.5).abs() < f64::EPSILON);
    // "quick", "brown", "fox", "lazy", "dog" = 5 unique terms ("the" filtered)
    assert_eq!(stats.unique_terms, 5);
}

#[test]
fn test_deterministic_score_output() {
    let mut index = Bm25Index::default();
    index
        .index_document("doc1".to_string(), "test document")
        .unwrap();

    let results = index.search("test", 10);
    assert_eq!(results.len(), 1);

    // Score should be a DeterministicScore (fixed-point i64; no NaN concept).
    let (_doc_id, score) = &results[0];
    let f = score.to_f64();
    assert!(f > 0.0);
    assert!(f.is_finite());
}
