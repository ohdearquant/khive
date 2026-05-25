//! Tests for BM25 index.

#[cfg(test)]
mod unit_tests {
    use crate::{Bm25Config, Bm25Index, BoxedTokenizer, SimpleTokenizer};
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
}

/// Golden tests for BM25 scoring (RETRIEVAL-04).
///
/// These tests verify known expected values to detect drift in scoring behavior
/// across versions or platforms. The expected values were computed with the
/// standard BM25 formula (k1=1.2, b=0.75) and verified manually.
///
/// # Cross-Platform CI Note
///
/// These tests should run on all CI platforms (Linux, macOS, Windows) to verify
/// consistent scoring. The tolerance (1e-6) accounts for minor FP differences
/// while still catching significant regressions.
///
/// If these tests fail on a specific platform, investigate:
/// 1. FMA instruction availability differences
/// 2. Compiler optimization flags
/// 3. Extended precision (x87) on older x86
#[cfg(test)]
mod golden_tests {
    use crate::{Bm25Config, Bm25Index};

    /// Tolerance for floating-point comparison in golden tests.
    /// 1e-6 is tight enough to catch bugs but loose enough for cross-platform variance.
    const GOLDEN_TOLERANCE: f64 = 1e-6;

    /// Golden test corpus for reproducible scoring.
    fn setup_golden_corpus() -> Bm25Index {
        let mut index = Bm25Index::new(Bm25Config::default());
        // Fixed corpus with known characteristics:
        // doc1: 4 tokens (quick, brown, fox, jumps)
        // doc2: 3 tokens (lazy, brown, dog)
        // doc3: 2 tokens (quick, fox)
        // Total: 9 tokens, avgdl = 3.0
        index
            .index_document("doc1".to_string(), "quick brown fox jumps")
            .unwrap();
        index
            .index_document("doc2".to_string(), "lazy brown dog")
            .unwrap();
        index
            .index_document("doc3".to_string(), "quick fox")
            .unwrap();
        index
    }

    #[test]
    fn golden_single_term_query() {
        let index = setup_golden_corpus();

        // Query for "brown" (appears in doc1 and doc2)
        // IDF("brown") = ln((3 - 2 + 0.5) / (2 + 0.5) + 1) = ln(1.6) ≈ 0.470003629
        let results = index.search("brown", 10);

        assert_eq!(results.len(), 2);

        // Both docs contain "brown" once, but doc2 is shorter (3 tokens vs 4)
        // so doc2 should score slightly higher with length normalization
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

        // Golden values (empirically verified from implementation with k1=1.2, b=0.75, avgdl=3.0)
        // These values are the actual outputs and serve as regression tests.
        // doc1: len=4, higher length penalty
        // doc2: len=3 (at avgdl), no length adjustment
        assert!(
            (doc1_score.to_f64() - 0.4136031938251108).abs() < GOLDEN_TOLERANCE,
            "doc1 score {} differs from golden 0.4136031938251108",
            doc1_score.to_f64()
        );
        assert!(
            (doc2_score.to_f64() - 0.47000362924573563).abs() < GOLDEN_TOLERANCE,
            "doc2 score {} differs from golden 0.47000362924573563",
            doc2_score.to_f64()
        );
    }

    #[test]
    fn golden_multi_term_query() {
        let index = setup_golden_corpus();

        // Query for "quick fox" (doc1 has both, doc3 has both)
        let results = index.search("quick fox", 10);

        assert_eq!(results.len(), 2);

        let doc1_score = results
            .iter()
            .find(|(id, _)| id.as_ref() == "doc1")
            .unwrap()
            .1;
        let doc3_score = results
            .iter()
            .find(|(id, _)| id.as_ref() == "doc3")
            .unwrap()
            .1;

        // doc3 is shorter (2 tokens) and has both terms -> should score higher
        assert!(doc3_score > doc1_score);

        // Golden values for multi-term query (empirically verified)
        // "quick": df=2, "fox": df=2
        // doc3 (len=2): shorter doc gets boost from length normalization
        assert!(
            (doc3_score.to_f64() - 1.088429457275197).abs() < GOLDEN_TOLERANCE,
            "doc3 score {} differs from golden 1.088429457275197",
            doc3_score.to_f64()
        );
    }

    #[test]
    fn golden_rare_term_high_idf() {
        let index = setup_golden_corpus();

        // "jumps" only in doc1 (df=1), "lazy" only in doc2 (df=1)
        // Both have high IDF = ln((3-1+0.5)/(1+0.5)+1) = ln(2.667) ≈ 0.981
        let results = index.search("jumps", 10);

        assert_eq!(results.len(), 1);
        assert_eq!(&*results[0].0, "doc1");

        // Golden value for rare term (empirically verified)
        // "jumps" has high IDF due to appearing in only 1 document
        // doc1 has length penalty (len=4, avgdl=3)
        assert!(
            (results[0].1.to_f64() - 0.8631297426763922).abs() < GOLDEN_TOLERANCE,
            "rare term score {} differs from golden 0.8631297426763922",
            results[0].1.to_f64()
        );
    }

    #[test]
    fn golden_term_frequency_saturation() {
        // Test that repeated terms show saturation (TF component approaches k1+1=2.2)
        let mut index = Bm25Index::new(Bm25Config::default());

        // doc1 has "test" once, doc2 has it 5 times
        index.index_document("doc1".to_string(), "test").unwrap();
        index
            .index_document("doc2".to_string(), "test test test test test")
            .unwrap();

        let results = index.search("test", 10);
        assert_eq!(results.len(), 2);

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

        // doc2 has higher TF but saturation limits the boost
        // The score ratio should be much less than 5x
        let ratio = doc2_score.to_f64() / doc1_score.to_f64();
        assert!(
            ratio < 2.5,
            "TF saturation not working: ratio {ratio} should be < 2.5"
        );
        assert!(
            ratio > 1.0,
            "Higher TF should still score higher: ratio {ratio}"
        );

        // Golden: with avgdl=3, k1=1.2, b=0.75:
        // doc1 (tf=1, len=1, L=0.333): denom=1+1.2*(0.25+0.75*0.333)=1.6, TF=2.2/1.6=1.375
        // doc2 (tf=5, len=5, L=1.667): denom=5+1.2*(0.25+0.75*1.667)=6.8, TF=11/6.8=1.618
        // Score ratio ≈ 1.618/1.375 ≈ 1.177
        assert!(
            (ratio - 1.17682).abs() < 0.01,
            "TF saturation ratio {ratio} differs from golden 1.177"
        );
    }

    #[test]
    fn golden_length_normalization() {
        // Test length normalization with same term frequency
        let mut index = Bm25Index::new(Bm25Config::default());

        // Both have "test" once, but different lengths
        index.index_document("short".to_string(), "test").unwrap();
        index
            .index_document("long".to_string(), "test padding padding padding padding")
            .unwrap();

        let results = index.search("test", 10);
        assert_eq!(results.len(), 2);

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

        // Shorter doc should score higher (b=0.75 applies length penalty)
        assert!(
            short_score > long_score,
            "Short doc should score higher than long doc"
        );

        // Golden: avgdl=3, k1=1.2, b=0.75
        // short (len=1, L=0.333): denom=1+1.2*(0.25+0.25)=1.6, TF=2.2/1.6=1.375
        // long (len=5, L=1.667): denom=1+1.2*(0.25+1.25)=2.8, TF=2.2/2.8=0.786
        let ratio = short_score.to_f64() / long_score.to_f64();
        assert!(
            (ratio - 1.75).abs() < 0.1,
            "Length normalization ratio {ratio} differs from expected ~1.75"
        );
    }

    #[test]
    fn golden_deterministic_across_runs() {
        // Verify that multiple searches produce identical results
        let index = setup_golden_corpus();

        let results1 = index.search("quick brown", 10);
        let results2 = index.search("quick brown", 10);
        let results3 = index.search("quick brown", 10);

        assert_eq!(results1.len(), results2.len());
        assert_eq!(results2.len(), results3.len());

        for i in 0..results1.len() {
            assert_eq!(
                results1[i].0, results2[i].0,
                "Doc ID mismatch at position {i}"
            );
            assert_eq!(
                results1[i].1, results2[i].1,
                "Score mismatch at position {i}"
            );
            assert_eq!(
                results2[i].1, results3[i].1,
                "Score mismatch at position {i}"
            );
        }
    }
}

/// Memory budget enforcement tests for BM25.
#[cfg(test)]
mod memory_budget_tests {
    use crate::error::{ErrorKind, RetrievalError};
    use crate::{Bm25Config, Bm25Index};

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
        let mut index = Bm25Index::new(config);

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
        let mut index = Bm25Index::new(config);

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
        let index = Bm25Index::new(config);
        assert_eq!(index.memory_budget(), Some(10_000));
    }

    #[test]
    fn test_budget_exceeded_error_details() {
        let config = Bm25Config::default().with_memory_budget(1);
        let mut index = Bm25Index::new(config);

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
        let mut index = Bm25Index::new(config);

        index.index_document("doc1", "quick brown fox").unwrap();
        index.index_document("doc2", "lazy brown dog").unwrap();

        // Search should work regardless of budget
        let results = index.search("brown", 10);
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn test_budget_allows_removal_then_insert() {
        let config = Bm25Config::default().with_memory_budget(3_000);
        let mut index = Bm25Index::new(config);

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
}

/// Tests for forward index persistence and O(|doc|) remove behaviour (issue #307).
#[cfg(test)]
mod forward_index_tests {
    use crate::{Bm25Config, Bm25Index};

    /// After a serde round-trip, `ensure_forward_index` must rebuild the forward map
    /// so that subsequent removes take the fast O(|terms_in_doc|) path rather than
    /// the O(|vocabulary|) fallback.
    #[test]
    fn test_forward_index_persisted_across_save_load_cycle() {
        let mut index = Bm25Index::default();
        index.index_document("doc1", "quick brown fox").unwrap();
        index.index_document("doc2", "lazy brown dog").unwrap();
        index.index_document("doc3", "quick fox jumps").unwrap();

        // Serialize (forward_index is #[serde(skip)] — intentionally not in snapshot).
        let json = serde_json::to_string(&index).unwrap();

        // Deserialize — forward_index is empty at this point.
        let mut restored: Bm25Index = serde_json::from_str(&json).unwrap();

        // Forward index is empty after deserialization.
        assert!(
            restored.forward_index.is_empty(),
            "forward_index must be empty right after deserialization"
        );

        // Calling ensure_forward_index must rebuild it from the inverted index.
        restored.ensure_forward_index();

        assert!(
            !restored.forward_index.is_empty(),
            "forward_index must be populated after ensure_forward_index()"
        );

        // Every document that has a doc_lengths entry must appear in the forward index.
        for internal_id in restored.doc_lengths.keys() {
            assert!(
                restored.forward_index.contains_key(internal_id),
                "doc {internal_id} missing from rebuilt forward_index"
            );
        }
    }

    /// `remove_document` on a deserialized index must use the forward index
    /// (O(|terms_in_doc|) path) rather than the O(|vocabulary|) full scan.
    ///
    /// We verify the algorithm by inspecting the forward index state: after the
    /// first remove call on a fresh-deserialized index, `ensure_forward_index`
    /// must have populated the map, and subsequent removes must still work
    /// correctly regardless of vocabulary size.
    #[test]
    fn test_remove_uses_forward_index_not_full_scan() {
        // Build an index with a meaningful vocabulary.
        let mut index = Bm25Index::default();
        let words = [
            "alpha", "beta", "gamma", "delta", "epsilon", "zeta", "eta", "theta", "iota", "kappa",
        ];
        for (i, word) in words.iter().enumerate() {
            index
                .index_document(format!("doc{i}"), &format!("{word} shared_term"))
                .unwrap();
        }

        // Serialize and restore (forward_index stripped by #[serde(skip)]).
        let json = serde_json::to_string(&index).unwrap();
        let mut restored: Bm25Index = serde_json::from_str(&json).unwrap();

        // Precondition: forward index starts empty after deserialization.
        assert!(restored.forward_index.is_empty());

        // Remove a document — this triggers ensure_forward_index() internally.
        let removed = restored.remove_document("doc0");
        assert!(
            removed,
            "remove_document must return true for an existing doc"
        );

        // After removal: doc0 must be gone, forward index populated for remaining docs.
        assert!(!restored.contains_document("doc0"));
        assert_eq!(
            restored.doc_count(),
            words.len() - 1,
            "doc_count must decrease by exactly one"
        );

        // The forward index must now be populated (lazily rebuilt on first remove).
        assert!(
            !restored.forward_index.is_empty(),
            "forward_index must be populated after the first remove on a deserialized index"
        );

        // Remove remaining documents one by one — must all succeed cleanly.
        for i in 1..words.len() {
            let doc_id = format!("doc{i}");
            let ok = restored.remove_document(&doc_id);
            assert!(ok, "remove_document must succeed for {doc_id}");
        }
        assert_eq!(restored.doc_count(), 0);
        assert!(
            restored.inverted_index.is_empty(),
            "inverted index must be empty after all removes"
        );
    }

    /// Search results must be identical before and after a save/load/remove cycle
    /// for documents that remain in the index.
    ///
    /// This is the regression guard: the forward-index change must not alter
    /// the scoring or result ordering for documents that were not removed.
    ///
    /// Strategy: build baseline on an index without doc4, add doc4, round-trip
    /// through serde, remove doc4 — then assert results match the original baseline.
    #[test]
    fn test_search_results_unchanged_after_add_remove_cycle() {
        // Step 1: baseline on a 3-document index (no doc4).
        let mut baseline_index = Bm25Index::new(Bm25Config::default());
        baseline_index
            .index_document("doc1", "quick brown fox")
            .unwrap();
        baseline_index
            .index_document("doc2", "lazy brown dog")
            .unwrap();
        baseline_index
            .index_document("doc3", "quick fox jumps")
            .unwrap();
        let baseline = baseline_index.search("quick brown fox", 10);

        // Step 2: add doc4 to introduce it, then round-trip through serde.
        baseline_index
            .index_document("doc4", "unrelated zebra content")
            .unwrap();
        let json = serde_json::to_string(&baseline_index).unwrap();
        let mut restored: Bm25Index = serde_json::from_str(&json).unwrap();
        restored.ensure_doc_lengths_vec();

        // Step 3: remove doc4 on the restored index.
        let removed = restored.remove_document("doc4");
        assert!(removed, "doc4 must be removable from the restored index");

        // Step 4: results must match the original 3-document baseline exactly.
        let after = restored.search("quick brown fox", 10);

        assert_eq!(
            baseline.len(),
            after.len(),
            "result count must match the original 3-doc baseline after remove cycle"
        );
        for (base, post) in baseline.iter().zip(after.iter()) {
            assert_eq!(
                base.0, post.0,
                "doc_id ordering must be preserved after remove cycle"
            );
            assert_eq!(
                base.1, post.1,
                "BM25 scores must be identical after remove cycle"
            );
        }
    }
}

#[cfg(test)]
mod metrics_tests {
    use crate::metrics::{names, MetricValue, RecordingSink};
    use crate::{Bm25Config, Bm25Index};
    use std::sync::Arc;

    #[test]
    fn index_document_emits_metrics() {
        let sink = Arc::new(RecordingSink::new());
        let mut index = Bm25Index::new(Bm25Config::default()).with_metrics(sink.clone());

        index.index_document("doc1", "the quick brown fox").unwrap();

        let events = sink.events();
        let event_names: Vec<&str> = events.iter().map(|e| e.name).collect();

        assert!(
            event_names.contains(&names::BM25_INDEX_DURATION_MS),
            "Missing index_document duration metric"
        );
        assert!(
            event_names.contains(&names::BM25_INDEX_COUNT),
            "Missing index_document count metric"
        );
        assert!(
            event_names.contains(&names::BM25_INDEX_SIZE),
            "Missing index size metric"
        );

        // Index size should be 1
        let size_event = events
            .iter()
            .find(|e| e.name == names::BM25_INDEX_SIZE)
            .unwrap();
        assert_eq!(size_event.value, MetricValue::Gauge(1.0));
    }

    #[test]
    fn search_emits_metrics() {
        let sink = Arc::new(RecordingSink::new());
        let mut index = Bm25Index::new(Bm25Config::default()).with_metrics(sink.clone());

        index.index_document("doc1", "the quick brown fox").unwrap();
        index.index_document("doc2", "the lazy dog").unwrap();

        // Clear indexing metrics
        sink.clear();

        let results = index.search("quick fox", 10);

        let events = sink.events();
        let event_names: Vec<&str> = events.iter().map(|e| e.name).collect();

        assert!(
            event_names.contains(&names::BM25_SEARCH_DURATION_MS),
            "Missing search duration metric"
        );
        assert!(
            event_names.contains(&names::BM25_SEARCH_COUNT),
            "Missing search count metric"
        );
        assert!(
            event_names.contains(&names::BM25_SEARCH_RESULTS),
            "Missing search results metric"
        );

        // Results count should match
        let results_event = events
            .iter()
            .find(|e| e.name == names::BM25_SEARCH_RESULTS)
            .unwrap();
        assert_eq!(
            results_event.value,
            MetricValue::Gauge(results.len() as f64)
        );
    }

    #[test]
    fn no_metrics_without_sink() {
        // Ensure no panic when metrics is None (default)
        let mut index = Bm25Index::new(Bm25Config::default());
        index.index_document("doc1", "hello world").unwrap();
        let _ = index.search("hello", 5);
    }

    #[test]
    fn set_metrics_at_runtime() {
        let mut index = Bm25Index::new(Bm25Config::default());
        index.index_document("doc1", "hello world").unwrap();

        // Attach sink
        let sink = Arc::new(RecordingSink::new());
        index.set_metrics(Some(sink.clone()));

        index.index_document("doc2", "goodbye world").unwrap();

        assert!(!sink.is_empty());

        // Detach
        index.set_metrics(None);
        sink.clear();

        index.index_document("doc3", "another document").unwrap();
        assert!(sink.is_empty(), "No events after detaching sink");
    }

    #[test]
    fn search_on_empty_index_still_emits() {
        let sink = Arc::new(RecordingSink::new());
        let index = Bm25Index::new(Bm25Config::default()).with_metrics(sink.clone());

        let results = index.search("anything", 5);
        assert!(results.is_empty());

        // Should still emit duration/count/results
        let events = sink.events();
        let event_names: Vec<&str> = events.iter().map(|e| e.name).collect();
        assert!(event_names.contains(&names::BM25_SEARCH_DURATION_MS));
        assert!(event_names.contains(&names::BM25_SEARCH_COUNT));
        assert!(event_names.contains(&names::BM25_SEARCH_RESULTS));
    }

    #[test]
    fn multiple_operations_accumulate_events() {
        let sink = Arc::new(RecordingSink::new());
        let mut index = Bm25Index::new(Bm25Config::default()).with_metrics(sink.clone());

        // 3 index operations
        index.index_document("d1", "alpha beta").unwrap();
        index.index_document("d2", "gamma delta").unwrap();
        index.index_document("d3", "epsilon zeta").unwrap();

        // Count index_document.count events
        let count_events: usize = sink
            .events()
            .iter()
            .filter(|e| e.name == names::BM25_INDEX_COUNT)
            .count();
        assert_eq!(count_events, 3, "Expected 3 index count events");
    }

    #[test]
    fn index_duration_is_nonnegative() {
        let sink = Arc::new(RecordingSink::new());
        let mut index = Bm25Index::new(Bm25Config::default()).with_metrics(sink.clone());

        index
            .index_document("doc1", "test document content")
            .unwrap();

        let duration_event = sink
            .events()
            .into_iter()
            .find(|e| e.name == names::BM25_INDEX_DURATION_MS)
            .unwrap();

        match duration_event.value {
            MetricValue::Histogram(ms) => assert!(ms >= 0.0, "Duration must be >= 0"),
            other => panic!("Expected Histogram, got {other:?}"),
        }
    }
}
