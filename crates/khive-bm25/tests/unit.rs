use khive_bm25::{Bm25Config, Bm25Index, BoxedTokenizer, SimpleTokenizer};
use std::sync::Arc;

#[test]
fn test_new_index() {
    let index = Bm25Index::new(Bm25Config::default());
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
    let mut index = Bm25Index::new(config);
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

#[test]
fn test_custom_tokenizer() {
    // Create a custom tokenizer with minimum length 4
    let tokenizer: BoxedTokenizer = Arc::new(SimpleTokenizer::new(true, 4));
    let mut index = Bm25Index::with_tokenizer(Bm25Config::default(), tokenizer);

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

#[test]
fn test_concurrent_search() {
    use std::thread;

    let mut index = Bm25Index::default();
    index
        .index_document("doc1".to_string(), "the quick brown fox")
        .unwrap();
    index
        .index_document("doc2".to_string(), "the lazy dog")
        .unwrap();
    index
        .index_document("doc3".to_string(), "quick fox jumps")
        .unwrap();

    // Wrap in Arc for sharing across threads (search takes &self now)
    let index = Arc::new(index);

    // Spawn multiple threads doing concurrent searches
    let handles: Vec<_> = (0..4)
        .map(|i| {
            let index = Arc::clone(&index);
            thread::spawn(move || {
                // Each thread does multiple searches
                for _ in 0..100 {
                    let query = if i % 2 == 0 { "quick fox" } else { "lazy dog" };
                    let results = index.search(query, 10);
                    assert!(!results.is_empty());
                }
            })
        })
        .collect();

    // Wait for all threads to complete
    for handle in handles {
        handle.join().expect("Thread panicked");
    }
}
