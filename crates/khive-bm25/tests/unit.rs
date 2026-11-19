use khive_bm25::{Bm25Config, Bm25Index, BoxedTokenizer, SimpleTokenizer};
use std::sync::Arc;

include!("unit_tests/construction_and_query_tests.rs");

include!("unit_tests/ranking_and_corpus_statistics_tests.rs");

include!("unit_tests/configuration_and_state_tests.rs");

include!("unit_tests/tokenizer_customization_tests.rs");

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
