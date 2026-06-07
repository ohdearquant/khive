//! Observability hooks for retrieval indices.
//!
//! Pluggable MetricsSink trait; inject NoopSink for zero overhead or RecordingSink for tests.
//! Decoupled from Prometheus/OpenTelemetry so the crate has no observability stack dependency.
//! // ... perform operations ...
//! let events = sink.events();
//! assert!(!events.is_empty());
//! ```

use std::fmt;
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------
// Core types
// ---------------------------------------------------------------------------

/// A single metric observation.
#[derive(Debug, Clone)]
pub struct MetricEvent {
    /// Well-known metric name (use constants from [`names`]).
    pub name: &'static str,
    /// Observed value.
    pub value: MetricValue,
    /// Dimensional labels for grouping / filtering.
    pub labels: Vec<(&'static str, String)>,
}

/// Metric value kinds.
#[derive(Debug, Clone, PartialEq)]
pub enum MetricValue {
    /// Monotonically increasing count.
    Counter(u64),
    /// Point-in-time measurement (can go up or down).
    Gauge(f64),
    /// Duration or distribution sample (typically seconds or milliseconds).
    Histogram(f64),
}

impl fmt::Display for MetricValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MetricValue::Counter(v) => write!(f, "counter({v})"),
            MetricValue::Gauge(v) => write!(f, "gauge({v})"),
            MetricValue::Histogram(v) => write!(f, "histogram({v})"),
        }
    }
}

// ---------------------------------------------------------------------------
// Sink trait
// ---------------------------------------------------------------------------

/// Receiver of metric events emitted by retrieval indices.
///
/// Implementors bridge to their observability stack (Prometheus counters,
/// OTel meters, StatsD, etc.). The trait is `Send + Sync` so a single
/// `Arc<dyn MetricsSink>` can be shared across threads.
pub trait MetricsSink: Send + Sync + fmt::Debug {
    /// Record a single metric event.
    fn record(&self, event: MetricEvent);
}

// ---------------------------------------------------------------------------
// Built-in sinks
// ---------------------------------------------------------------------------

/// Sink that silently discards every event.
///
/// This is the implicit default when no metrics are configured.
/// All calls compile down to a no-op.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopSink;

impl MetricsSink for NoopSink {
    #[inline]
    fn record(&self, _event: MetricEvent) {
        // intentionally empty
    }
}

/// Thread-safe recording sink for tests. Collects `MetricEvent`s into a `Mutex<Vec>`.
#[derive(Debug, Default)]
pub struct RecordingSink {
    events: Mutex<Vec<MetricEvent>>,
}

impl RecordingSink {
    /// Create a new, empty recording sink.
    pub fn new() -> Self {
        Self {
            events: Mutex::new(Vec::new()),
        }
    }

    /// Return a snapshot of all recorded events.
    ///
    /// Returns an empty vec if the mutex is poisoned (indicates a prior panic).
    pub fn events(&self) -> Vec<MetricEvent> {
        self.events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Discard all recorded events.
    ///
    /// Silently skips clearing if the mutex is poisoned.
    pub fn clear(&self) {
        if let Ok(mut guard) = self.events.lock() {
            guard.clear();
        }
    }

    /// Return the number of recorded events.
    ///
    /// Returns 0 if the mutex is poisoned.
    pub fn len(&self) -> usize {
        self.events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
    }

    /// Check if no events have been recorded.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl MetricsSink for RecordingSink {
    fn record(&self, event: MetricEvent) {
        if let Ok(mut guard) = self.events.lock() {
            guard.push(event);
        }
    }
}

// ---------------------------------------------------------------------------
// Well-known metric names
// ---------------------------------------------------------------------------

/// Well-known metric name constants.
///
/// Using constants prevents typos and allows dashboards to be built once.
/// Names follow the `{subsystem}.{operation}.{measurement}` convention.
pub mod names {
    // -- HNSW --

    /// Duration of a single HNSW search in milliseconds.
    pub const HNSW_SEARCH_DURATION_MS: &str = "hnsw.search.duration_ms";
    /// Number of HNSW search operations completed.
    pub const HNSW_SEARCH_COUNT: &str = "hnsw.search.count";
    /// Number of results returned by an HNSW search.
    pub const HNSW_SEARCH_RESULTS: &str = "hnsw.search.results";

    /// Duration of a single HNSW insert in milliseconds.
    pub const HNSW_INSERT_DURATION_MS: &str = "hnsw.insert.duration_ms";
    /// Number of HNSW insert operations completed.
    pub const HNSW_INSERT_COUNT: &str = "hnsw.insert.count";

    /// Duration of an HNSW rebuild in milliseconds.
    pub const HNSW_REBUILD_DURATION_MS: &str = "hnsw.rebuild.duration_ms";
    /// Number of HNSW rebuild operations completed.
    pub const HNSW_REBUILD_COUNT: &str = "hnsw.rebuild.count";
    /// Number of nodes removed during a rebuild.
    pub const HNSW_REBUILD_NODES_REMOVED: &str = "hnsw.rebuild.nodes_removed";

    /// Current number of live vectors in the HNSW index.
    pub const HNSW_INDEX_SIZE: &str = "hnsw.index.size";

    // -- BM25 --

    /// Duration of a single BM25 search in milliseconds.
    pub const BM25_SEARCH_DURATION_MS: &str = "bm25.search.duration_ms";
    /// Number of BM25 search operations completed.
    pub const BM25_SEARCH_COUNT: &str = "bm25.search.count";
    /// Number of results returned by a BM25 search.
    pub const BM25_SEARCH_RESULTS: &str = "bm25.search.results";

    /// Duration of a single BM25 index_document call in milliseconds.
    pub const BM25_INDEX_DURATION_MS: &str = "bm25.index_document.duration_ms";
    /// Number of BM25 index_document operations completed.
    pub const BM25_INDEX_COUNT: &str = "bm25.index_document.count";

    /// Current number of documents in the BM25 index.
    pub const BM25_INDEX_SIZE: &str = "bm25.index.size";
}

// ---------------------------------------------------------------------------
// Helper: emit to optional sink
// ---------------------------------------------------------------------------

/// Convenience function to emit a metric event to an optional sink.
///
/// This avoids repeating `if let Some(sink) = &self.metrics { ... }` in
/// every instrumented method.
#[inline]
pub fn emit(sink: &Option<Arc<dyn MetricsSink>>, event: MetricEvent) {
    if let Some(s) = sink {
        s.record(event);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::approx_constant)]
mod tests {
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
}
