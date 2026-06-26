use khive_bm25::metrics::{names, MetricValue, RecordingSink};
use khive_bm25::{Bm25Config, Bm25Index};
use std::sync::Arc;

#[test]
fn index_document_emits_metrics() {
    let sink = Arc::new(RecordingSink::new());
    let mut index = Bm25Index::try_new(Bm25Config::default())
        .expect("valid config")
        .with_metrics(sink.clone());

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
    let mut index = Bm25Index::try_new(Bm25Config::default())
        .expect("valid config")
        .with_metrics(sink.clone());

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
    let mut index = Bm25Index::try_new(Bm25Config::default()).expect("valid config");
    index.index_document("doc1", "hello world").unwrap();
    let _ = index.search("hello", 5);
}

#[test]
fn set_metrics_at_runtime() {
    let mut index = Bm25Index::try_new(Bm25Config::default()).expect("valid config");
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
    let index = Bm25Index::try_new(Bm25Config::default())
        .expect("valid config")
        .with_metrics(sink.clone());

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
    let mut index = Bm25Index::try_new(Bm25Config::default())
        .expect("valid config")
        .with_metrics(sink.clone());

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
    let mut index = Bm25Index::try_new(Bm25Config::default())
        .expect("valid config")
        .with_metrics(sink.clone());

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
