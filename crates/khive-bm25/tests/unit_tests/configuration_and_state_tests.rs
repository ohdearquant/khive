#[test]
fn test_case_insensitive() {
    let mut index = Bm25Index::default();
    index
        .index_document("doc1".to_string(), "The QUICK Brown FOX")
        .unwrap();

    let results = index.search("quick fox", 10);
    assert_eq!(results.len(), 1);
    assert_eq!(&*results[0].0, "doc1");
}

#[test]
fn test_punctuation_handling() {
    let mut index = Bm25Index::default();
    index
        .index_document("doc1".to_string(), "Hello, World! How are you?")
        .unwrap();

    let results = index.search("hello world", 10);
    assert_eq!(results.len(), 1);
    assert_eq!(&*results[0].0, "doc1");
}

#[test]
fn test_config_custom() {
    let config = Bm25Config::new(2.0, 0.5);
    let mut index = Bm25Index::try_new(config).expect("valid config");
    index
        .index_document("doc1".to_string(), "test document")
        .unwrap();

    assert!((index.config().k1 - 2.0).abs() < f64::EPSILON);
    assert!((index.config().b - 0.5).abs() < f64::EPSILON);

    // Should still work
    let results = index.search("test", 10);
    assert_eq!(results.len(), 1);
}

#[test]
fn test_idf_caching() {
    let mut index = Bm25Index::default();
    index.index_document("doc1".to_string(), "test").unwrap();
    index.index_document("doc2".to_string(), "test").unwrap();

    // First search populates cache
    let _results1 = index.search("test", 10);

    // IDF cache should be populated
    assert!(!index.is_idf_cache_empty());

    // Second search uses cache (verified by consistent results)
    let results2 = index.search("test", 10);
    assert_eq!(results2.len(), 2);
}

#[test]
fn test_consistent_ordering() {
    let mut index = Bm25Index::default();
    index
        .index_document("doc1".to_string(), "fox quick")
        .unwrap();
    index
        .index_document("doc2".to_string(), "fox slow")
        .unwrap();
    index
        .index_document("doc3".to_string(), "quick quick fox")
        .unwrap();

    // Multiple searches should produce consistent ordering
    let results1 = index.search("quick fox", 10);
    let results2 = index.search("quick fox", 10);

    assert_eq!(results1.len(), results2.len());
    for i in 0..results1.len() {
        assert_eq!(results1[i].0, results2[i].0);
        assert_eq!(results1[i].1, results2[i].1);
    }
}

#[test]
fn test_serde_roundtrip() {
    let mut index = Bm25Index::default();
    index
        .index_document("doc1".to_string(), "the quick brown fox")
        .unwrap();
    index
        .index_document("doc2".to_string(), "the lazy dog")
        .unwrap();

    // Serialize
    let json = serde_json::to_string(&index).unwrap();

    // Deserialize
    let restored: Bm25Index = serde_json::from_str(&json).unwrap();

    // Should work the same
    assert_eq!(restored.doc_count(), 2);
    let results = restored.search("fox", 10);
    assert_eq!(results.len(), 1);
    assert_eq!(&*results[0].0, "doc1");
}
