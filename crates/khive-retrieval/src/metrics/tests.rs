use super::*;

#[test]
fn noop_sink_does_not_panic() {
    let sink = NoopSink;
    sink.record(MetricEvent {
        name: names::HNSW_SEARCH_COUNT,
        value: MetricValue::Counter(1),
        labels: vec![],
    });
}

#[test]
fn recording_sink_captures_events() {
    let sink = RecordingSink::new();
    assert!(sink.is_empty());

    sink.record(MetricEvent {
        name: names::HNSW_SEARCH_DURATION_MS,
        value: MetricValue::Histogram(1.5),
        labels: vec![("k", "10".to_string())],
    });
    sink.record(MetricEvent {
        name: names::HNSW_SEARCH_COUNT,
        value: MetricValue::Counter(1),
        labels: vec![],
    });

    assert_eq!(sink.len(), 2);
    assert!(!sink.is_empty());

    let events = sink.events();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].name, names::HNSW_SEARCH_DURATION_MS);
    assert_eq!(events[1].name, names::HNSW_SEARCH_COUNT);
}

#[test]
fn recording_sink_clear() {
    let sink = RecordingSink::new();
    sink.record(MetricEvent {
        name: names::HNSW_INSERT_COUNT,
        value: MetricValue::Counter(1),
        labels: vec![],
    });
    assert_eq!(sink.len(), 1);

    sink.clear();
    assert!(sink.is_empty());
}

#[test]
fn metric_value_display() {
    assert_eq!(MetricValue::Counter(42).to_string(), "counter(42)");
    assert_eq!(MetricValue::Gauge(3.14).to_string(), "gauge(3.14)");
    assert_eq!(MetricValue::Histogram(1.5).to_string(), "histogram(1.5)");
}

#[test]
fn emit_helper_with_none() {
    // Should not panic
    emit(
        &None,
        MetricEvent {
            name: names::HNSW_SEARCH_COUNT,
            value: MetricValue::Counter(1),
            labels: vec![],
        },
    );
}

#[test]
fn emit_helper_with_some() {
    let sink = Arc::new(RecordingSink::new());
    let opt: Option<Arc<dyn MetricsSink>> = Some(sink.clone());

    emit(
        &opt,
        MetricEvent {
            name: names::BM25_SEARCH_COUNT,
            value: MetricValue::Counter(1),
            labels: vec![],
        },
    );

    assert_eq!(sink.len(), 1);
}

#[test]
fn recording_sink_is_send_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<RecordingSink>();
}

#[test]
fn metrics_sink_is_object_safe() {
    // Prove we can construct an Arc<dyn MetricsSink>
    let _: Arc<dyn MetricsSink> = Arc::new(NoopSink);
    let _: Arc<dyn MetricsSink> = Arc::new(RecordingSink::new());
}
