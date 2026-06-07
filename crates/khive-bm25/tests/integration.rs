use khive_bm25::{Bm25Config, Bm25Index, BoxedTokenizer, SimpleTokenizer, Tokenizer};
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Lifecycle: create → index docs → search → verify ranking
// ---------------------------------------------------------------------------

#[test]
fn index_lifecycle_create_index_search_rank() {
    let mut index = Bm25Index::new(Bm25Config::default());
    assert_eq!(index.doc_count(), 0);

    index
        .index_document("doc1", "the quick brown fox jumps")
        .unwrap();
    index.index_document("doc2", "the lazy brown dog").unwrap();
    index.index_document("doc3", "quick fox").unwrap();

    assert_eq!(index.doc_count(), 3);

    // "quick fox" — doc3 has both in a shorter doc, should rank first.
    // doc2 ("the lazy brown dog") has neither "quick" nor "fox", so only 2 results.
    let results = index.search("quick fox", 10);
    assert_eq!(results.len(), 2, "only doc1 and doc3 contain quick or fox");
    assert_eq!(
        &*results[0].0, "doc3",
        "doc3 should rank first (shortest, both terms)"
    );

    // Verify all scores are positive and in descending order
    for i in 1..results.len() {
        assert!(
            results[i - 1].1 >= results[i].1,
            "results must be in non-increasing score order"
        );
        assert!(
            results[i].1.to_f64() > 0.0,
            "all matching scores must be positive"
        );
    }

    // Also verify "brown" finds both doc1 and doc2
    let brown_results = index.search("brown", 10);
    assert_eq!(brown_results.len(), 2, "doc1 and doc2 both contain 'brown'");
}

// ---------------------------------------------------------------------------
// Serde roundtrip: index → serialize → deserialize → search
// Validates the P0 serde fix (doc_lengths_f32 rebuilt on deserialization).
// ---------------------------------------------------------------------------

#[test]
fn serde_roundtrip_preserves_search_results() {
    let mut index = Bm25Index::default();
    index.index_document("doc1", "the quick brown fox").unwrap();
    index.index_document("doc2", "the lazy dog").unwrap();
    index
        .index_document("doc3", "quick fox jumps high")
        .unwrap();

    let original = index.search("quick fox", 10);

    // Serialize then deserialize
    let json = serde_json::to_string(&index).expect("serialize");
    let restored: Bm25Index = serde_json::from_str(&json).expect("deserialize");

    // doc_count must be preserved
    assert_eq!(restored.doc_count(), 3);

    // Search on restored index must return same results
    let after = restored.search("quick fox", 10);
    assert_eq!(
        original.len(),
        after.len(),
        "result count must match after serde roundtrip"
    );
    for (orig, rest) in original.iter().zip(after.iter()) {
        assert_eq!(
            orig.0, rest.0,
            "doc_id ordering must be preserved after serde"
        );
        assert_eq!(orig.1, rest.1, "scores must be identical after serde");
    }
}

#[test]
fn serde_roundtrip_with_4_docs_does_not_panic() {
    // Regression: deserialization did not rebuild doc_lengths_f32, causing
    // panic in the 4-wide NEON SIMD path on aarch64.
    let mut index = Bm25Index::default();
    for i in 0..4 {
        index
            .index_document(format!("doc{i}"), "alpha beta")
            .unwrap();
    }
    let json = serde_json::to_string(&index).unwrap();
    let restored: Bm25Index = serde_json::from_str(&json).unwrap();
    let results = restored.search("alpha", 10);
    assert_eq!(results.len(), 4, "all 4 docs must be found after serde");
}

// ---------------------------------------------------------------------------
// Reindex: index doc → reindex same doc with new content → search
// ---------------------------------------------------------------------------

#[test]
fn reindex_replaces_old_content_in_search() {
    let mut index = Bm25Index::default();
    index
        .index_document("doc1", "original content fox")
        .unwrap();
    index.index_document("doc2", "other document dog").unwrap();

    // Verify original content is found
    let before = index.search("fox", 10);
    assert_eq!(before.len(), 1);
    assert_eq!(&*before[0].0, "doc1");

    // Reindex doc1 with entirely different content
    index
        .index_document("doc1", "completely different words")
        .unwrap();

    // doc_count must stay the same (no duplicates)
    assert_eq!(index.doc_count(), 2);

    // Old term must no longer return doc1
    let after_fox = index.search("fox", 10);
    assert!(
        after_fox.is_empty(),
        "reindexed doc must not match old terms"
    );

    // New term must now return doc1
    let after_new = index.search("completely different", 10);
    assert_eq!(after_new.len(), 1);
    assert_eq!(&*after_new[0].0, "doc1");
}

// ---------------------------------------------------------------------------
// Remove: index docs → remove one → verify removed doc is gone
// ---------------------------------------------------------------------------

#[test]
fn remove_document_stops_appearing_in_search() {
    let mut index = Bm25Index::default();
    index.index_document("doc1", "quick brown fox").unwrap();
    index.index_document("doc2", "quick brown dog").unwrap();
    index.index_document("doc3", "slow lazy cat").unwrap();

    // Both doc1 and doc2 match "quick"
    let before = index.search("quick", 10);
    assert_eq!(before.len(), 2);

    // Remove doc1
    assert!(index.remove_document("doc1"));
    assert_eq!(index.doc_count(), 2);
    assert!(!index.contains_document("doc1"));

    // Only doc2 matches now
    let after = index.search("quick", 10);
    assert_eq!(after.len(), 1);
    assert_eq!(&*after[0].0, "doc2");

    // Removing a non-existent doc returns false
    assert!(!index.remove_document("nonexistent"));
    assert_eq!(index.doc_count(), 2);
}

// ---------------------------------------------------------------------------
// Edge cases: empty query, empty document, very long document
// ---------------------------------------------------------------------------

#[test]
fn empty_query_returns_no_results() {
    let mut index = Bm25Index::default();
    index.index_document("doc1", "some content").unwrap();

    assert!(
        index.search("", 10).is_empty(),
        "empty query must return no results"
    );
    assert!(
        index.search("   ", 10).is_empty(),
        "whitespace query must return no results"
    );
}

#[test]
fn empty_document_is_not_indexed() {
    let mut index = Bm25Index::default();
    index.index_document("empty", "").unwrap();
    index.index_document("whitespace", "   ").unwrap();
    index.index_document("real", "actual content").unwrap();

    // Empty/whitespace-only documents produce no tokens and must not be indexed
    assert_eq!(index.doc_count(), 1, "only 'real' should be indexed");
    assert!(!index.contains_document("empty"));
    assert!(!index.contains_document("whitespace"));
    assert!(index.contains_document("real"));
}

#[test]
fn very_long_document_is_indexed_and_searchable() {
    let mut index = Bm25Index::default();

    // Build a 10 000-token document
    let long_text: String = (0..10_000).map(|i| format!("word{i} ")).collect();
    index.index_document("long", &long_text).unwrap();
    index.index_document("short", "word0 word1").unwrap();

    assert_eq!(index.doc_count(), 2);

    // Both must be findable; short should score higher for rare terms in long doc
    let results = index.search("word0 word1", 10);
    assert_eq!(results.len(), 2);
    // "short" doc (2 tokens) vs "long" doc (10 000 tokens) — length normalization
    // means short should score much higher
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
    assert!(
        short_score > long_score,
        "short doc must score higher than very long doc"
    );
}

// ---------------------------------------------------------------------------
// Custom tokenizer: verify Tokenizer trait works with custom impl
// ---------------------------------------------------------------------------

struct UppercasePreservingTokenizer;

impl Tokenizer for UppercasePreservingTokenizer {
    fn tokenize(&self, text: &str) -> Vec<String> {
        text.split_whitespace().map(|s| s.to_owned()).collect()
    }
}

#[test]
fn custom_tokenizer_is_called_for_indexing_and_search() {
    let tokenizer: BoxedTokenizer = Arc::new(UppercasePreservingTokenizer);
    let mut index = Bm25Index::with_tokenizer(Bm25Config::default(), tokenizer);

    // With the custom tokenizer "Rust" and "rust" are different tokens
    index
        .index_document("doc1", "Rust programming language")
        .unwrap();
    index
        .index_document("doc2", "rust oxidation metal")
        .unwrap();

    // "Rust" (capitalized) should only match doc1
    let results_upper = index.search("Rust", 10);
    assert_eq!(results_upper.len(), 1);
    assert_eq!(&*results_upper[0].0, "doc1");

    // "rust" (lowercase) should only match doc2
    let results_lower = index.search("rust", 10);
    assert_eq!(results_lower.len(), 1);
    assert_eq!(&*results_lower[0].0, "doc2");
}

#[test]
fn custom_tokenizer_min_length_filters_short_tokens() {
    // SimpleTokenizer with min_length=4 filters stop words and short tokens
    let tokenizer: BoxedTokenizer = Arc::new(SimpleTokenizer::new(true, 4));
    let mut index = Bm25Index::with_tokenizer(Bm25Config::default(), tokenizer);

    index.index_document("doc1", "the quick brown fox").unwrap();

    // "the" (3 chars) must be filtered — no results
    assert!(index.search("the", 10).is_empty());

    // "quick" (5 chars) must be indexed
    assert_eq!(index.search("quick", 10).len(), 1);
}

// ---------------------------------------------------------------------------
// Tokenizer Tokenizer trait: verify tokenize() contract via SimpleTokenizer
// ---------------------------------------------------------------------------

#[test]
fn simple_tokenizer_contract() {
    let tok = SimpleTokenizer::default();
    let tokens = tok.tokenize("Hello, World! How are you?");
    // SimpleTokenizer lowercases and strips punctuation; filters stop words
    assert!(tokens.contains(&"hello".to_owned()), "must contain 'hello'");
    assert!(tokens.contains(&"world".to_owned()), "must contain 'world'");
    // stop words "how", "are", "you" may be filtered depending on the stop-word list
    // — just verify no panic and result is non-empty
    assert!(!tokens.is_empty());
}

// ---------------------------------------------------------------------------
// Stats: verify Bm25Stats reflects current index state
// ---------------------------------------------------------------------------

#[test]
fn stats_reflect_current_state() {
    let mut index = Bm25Index::default();
    let s0 = index.stats();
    assert_eq!(s0.doc_count, 0);
    assert_eq!(s0.unique_terms, 0);
    assert_eq!(s0.total_tokens, 0);

    index.index_document("doc1", "quick brown fox").unwrap();
    index.index_document("doc2", "lazy dog").unwrap();

    let s2 = index.stats();
    assert_eq!(s2.doc_count, 2);
    // "quick", "brown", "fox", "lazy", "dog" = 5 unique terms (stop words removed)
    assert_eq!(s2.unique_terms, 5);
    assert_eq!(s2.total_tokens, 5);
    assert!((s2.avg_doc_length - 2.5).abs() < f64::EPSILON);

    // After removing one document, stats must decrease
    index.remove_document("doc1");
    let s1 = index.stats();
    assert_eq!(s1.doc_count, 1);
}
