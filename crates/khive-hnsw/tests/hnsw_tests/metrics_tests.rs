use khive_hnsw::metrics::{names, MetricValue, RecordingSink};
use khive_hnsw::HnswIndex;
use khive_hnsw::NodeId;

use std::sync::Arc;

fn make_id(seed: u8) -> NodeId {
    NodeId::new([seed; 16])
}

#[test]
fn insert_emits_metrics() {
    let sink = Arc::new(RecordingSink::new());
    let mut index = HnswIndex::new(3).with_metrics(sink.clone());

    index.insert(make_id(1), vec![1.0, 0.0, 0.0]).unwrap();

    let events = sink.events();
    let event_names: Vec<&str> = events.iter().map(|e| e.name).collect();

    assert!(
        event_names.contains(&names::HNSW_INSERT_DURATION_MS),
        "Missing insert duration metric"
    );
    assert!(
        event_names.contains(&names::HNSW_INSERT_COUNT),
        "Missing insert count metric"
    );
    assert!(
        event_names.contains(&names::HNSW_INDEX_SIZE),
        "Missing index size metric"
    );

    // Index size should be 1 after first insert
    let size_event = events
        .iter()
        .find(|e| e.name == names::HNSW_INDEX_SIZE)
        .unwrap();
    assert_eq!(size_event.value, MetricValue::Gauge(1.0));
}

#[test]
fn search_emits_metrics() {
    let sink = Arc::new(RecordingSink::new());
    let mut index = HnswIndex::new(3).with_metrics(sink.clone());

    index.insert(make_id(1), vec![1.0, 0.0, 0.0]).unwrap();
    index.insert(make_id(2), vec![0.0, 1.0, 0.0]).unwrap();

    // Clear insert metrics
    sink.clear();

    let results = index.search(&[1.0, 0.0, 0.0], 2).unwrap();

    let events = sink.events();
    let event_names: Vec<&str> = events.iter().map(|e| e.name).collect();

    assert!(
        event_names.contains(&names::HNSW_SEARCH_DURATION_MS),
        "Missing search duration metric"
    );
    assert!(
        event_names.contains(&names::HNSW_SEARCH_COUNT),
        "Missing search count metric"
    );
    assert!(
        event_names.contains(&names::HNSW_SEARCH_RESULTS),
        "Missing search results metric"
    );

    // Results count should match actual results
    let results_event = events
        .iter()
        .find(|e| e.name == names::HNSW_SEARCH_RESULTS)
        .unwrap();
    assert_eq!(
        results_event.value,
        MetricValue::Gauge(results.len() as f64)
    );
}

#[test]
fn rebuild_emits_metrics() {
    let sink = Arc::new(RecordingSink::new());
    let mut index = HnswIndex::new(3).with_metrics(sink.clone());

    let id1 = make_id(1);
    let id2 = make_id(2);
    index.insert(id1, vec![1.0, 0.0, 0.0]).unwrap();
    index.insert(id2, vec![0.0, 1.0, 0.0]).unwrap();

    // Tombstone one node
    index.delete(id1);

    // Clear prior metrics
    sink.clear();

    let stats = index.rebuild();

    let events = sink.events();
    let event_names: Vec<&str> = events.iter().map(|e| e.name).collect();

    assert!(
        event_names.contains(&names::HNSW_REBUILD_DURATION_MS),
        "Missing rebuild duration metric"
    );
    assert!(
        event_names.contains(&names::HNSW_REBUILD_COUNT),
        "Missing rebuild count metric"
    );
    assert!(
        event_names.contains(&names::HNSW_REBUILD_NODES_REMOVED),
        "Missing rebuild nodes_removed metric"
    );
    assert!(
        event_names.contains(&names::HNSW_INDEX_SIZE),
        "Missing index size metric after rebuild"
    );

    // nodes_removed should be 1
    let removed_event = events
        .iter()
        .find(|e| e.name == names::HNSW_REBUILD_NODES_REMOVED)
        .unwrap();
    assert_eq!(removed_event.value, MetricValue::Gauge(1.0));
    assert_eq!(stats.nodes_removed, 1);
}

#[test]
fn no_metrics_without_sink() {
    // Ensure no panic when metrics is None (default)
    let mut index = HnswIndex::new(3);
    index.insert(make_id(1), vec![1.0, 0.0, 0.0]).unwrap();
    let _ = index.search(&[1.0, 0.0, 0.0], 1).unwrap();
    index.rebuild();
}

#[test]
fn set_metrics_at_runtime() {
    let mut index = HnswIndex::new(3);
    index.insert(make_id(1), vec![1.0, 0.0, 0.0]).unwrap();

    // Attach sink mid-lifecycle
    let sink = Arc::new(RecordingSink::new());
    index.set_metrics(Some(sink.clone()));

    index.insert(make_id(2), vec![0.0, 1.0, 0.0]).unwrap();

    // Should have metrics from the second insert only
    assert!(!sink.is_empty());

    // Detach
    index.set_metrics(None);
    sink.clear();

    index.insert(make_id(3), vec![0.0, 0.0, 1.0]).unwrap();
    assert!(sink.is_empty(), "No events after detaching sink");
}

#[test]
fn search_on_empty_index_still_emits() {
    let sink = Arc::new(RecordingSink::new());
    let index = HnswIndex::new(3).with_metrics(sink.clone());

    let results = index.search(&[1.0, 0.0, 0.0], 5).unwrap();
    assert!(results.is_empty());

    // Should still emit duration and count (even for empty index)
    let events = sink.events();
    let event_names: Vec<&str> = events.iter().map(|e| e.name).collect();
    assert!(event_names.contains(&names::HNSW_SEARCH_DURATION_MS));
    assert!(event_names.contains(&names::HNSW_SEARCH_COUNT));
}

#[test]
fn insert_duration_is_nonnegative() {
    let sink = Arc::new(RecordingSink::new());
    let mut index = HnswIndex::new(3).with_metrics(sink.clone());

    index.insert(make_id(1), vec![1.0, 0.0, 0.0]).unwrap();

    let duration_event = sink
        .events()
        .into_iter()
        .find(|e| e.name == names::HNSW_INSERT_DURATION_MS)
        .unwrap();

    match duration_event.value {
        MetricValue::Histogram(ms) => assert!(ms >= 0.0, "Duration must be >= 0"),
        other => panic!("Expected Histogram, got {other:?}"),
    }
}
