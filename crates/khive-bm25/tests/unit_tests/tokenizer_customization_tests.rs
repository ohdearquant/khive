#[test]
fn test_custom_tokenizer() {
    // Create a custom tokenizer with minimum length 4
    let tokenizer: BoxedTokenizer = Arc::new(SimpleTokenizer::new(true, 4));
    let mut index =
        Bm25Index::try_with_tokenizer(Bm25Config::default(), tokenizer).expect("valid config");

    // "the", "a" will be filtered out (< 4 chars)
    index
        .index_document("doc1".to_string(), "the quick brown fox")
        .unwrap();
    index
        .index_document("doc2".to_string(), "a lazy brown dog")
        .unwrap();

    // "the" and "a" should not be indexed
    let results = index.search("the", 10);
    assert!(results.is_empty(), "Short words should not be indexed");

    // "quick" and "brown" should be indexed
    let results = index.search("quick", 10);
    assert_eq!(results.len(), 1);
    assert_eq!(&*results[0].0, "doc1");

    // "brown" in both docs
    let results = index.search("brown", 10);
    assert_eq!(results.len(), 2);
}

#[test]
fn test_tokenizer_accessor() {
    let index = Bm25Index::default();
    let tokenizer = index.tokenizer();

    // Should tokenize correctly
    let tokens = tokenizer.tokenize("Hello, World!");
    assert_eq!(tokens, vec!["hello", "world"]);
}

#[test]
fn test_set_tokenizer() {
    let mut index = Bm25Index::default();

    // Index with default tokenizer (min_length=1, stop words on)
    // Use "ox" — not a stop word, not filtered by default min_length=1
    index
        .index_document("doc1".to_string(), "ox quick fox")
        .unwrap();
    let results = index.search("ox", 10);
    assert_eq!(results.len(), 1, "Default tokenizer should index 'ox'");

    // Change tokenizer to min_length=3 (this won't re-index existing docs)
    let new_tokenizer: BoxedTokenizer = Arc::new(SimpleTokenizer::new(true, 3));
    index.set_tokenizer(new_tokenizer);

    // New document with new tokenizer
    index
        .index_document("doc2".to_string(), "ox slow fox")
        .unwrap();

    // doc1 still has "ox" indexed, but search tokenizer now filters "ox" (len < 3)
    // Since query "ox" becomes empty after tokenization, no results
    let results = index.search("ox", 10);
    assert!(
        results.is_empty(),
        "Query 'ox' should be filtered by min_length=3"
    );

    // "fox" should find both docs
    let results = index.search("fox", 10);
    assert_eq!(results.len(), 2);
}
