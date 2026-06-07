//! Metrics infrastructure for HNSW observability.
//! Pluggable `MetricsSink` trait for insert, search, and rebuild telemetry.

use std::sync::{Arc, Mutex};

/// A single metric value emitted from an HNSW operation.
#[derive(Debug, Clone, PartialEq)]
pub enum MetricValue {
    /// Monotonically increasing counter (e.g., insert count).
    Counter(u64),
    /// Point-in-time gauge (e.g., index size).
    Gauge(f64),
    /// Distribution observation (e.g., operation duration in ms).
    Histogram(f64),
}

/// A single metric event emitted from an HNSW operation.
#[derive(Debug, Clone)]
pub struct MetricEvent {
    /// Metric name (use the constants in [`names`]).
    pub name: &'static str,
    /// The metric value.
    pub value: MetricValue,
    /// Optional key-value label pairs (e.g., `[("metric", "cosine")]`).
    pub labels: Vec<(&'static str, String)>,
}

/// Receiver for metric events from HNSW operations.
pub trait MetricsSink: Send + Sync {
    /// Handle a metric event.
    fn emit(&self, event: MetricEvent);
}

/// Emit a metric event to the attached sink; no-op when `sink` is `None`.
#[inline]
pub fn emit(sink: &Option<Arc<dyn MetricsSink>>, event: MetricEvent) {
    if let Some(s) = sink {
        s.emit(event);
    }
}

/// Canonical metric name constants (`&'static str` to avoid hot-path formatting).
pub mod names {
    /// Duration of a single insert operation in milliseconds (Histogram).
    pub const HNSW_INSERT_DURATION_MS: &str = "hnsw.insert.duration_ms";
    /// Number of insert operations (Counter).
    pub const HNSW_INSERT_COUNT: &str = "hnsw.insert.count";
    /// Current live node count after insert (Gauge).
    pub const HNSW_INDEX_SIZE: &str = "hnsw.index.size";

    /// Duration of a single search operation in milliseconds (Histogram).
    pub const HNSW_SEARCH_DURATION_MS: &str = "hnsw.search.duration_ms";
    /// Number of search operations (Counter).
    pub const HNSW_SEARCH_COUNT: &str = "hnsw.search.count";
    /// Number of results returned by a search (Gauge).
    pub const HNSW_SEARCH_RESULTS: &str = "hnsw.search.results";

    /// Duration of a rebuild operation in milliseconds (Histogram).
    pub const HNSW_REBUILD_DURATION_MS: &str = "hnsw.rebuild.duration_ms";
    /// Number of rebuild operations (Counter).
    pub const HNSW_REBUILD_COUNT: &str = "hnsw.rebuild.count";
    /// Number of nodes removed during a rebuild (Gauge).
    pub const HNSW_REBUILD_NODES_REMOVED: &str = "hnsw.rebuild.nodes_removed";
}

/// A `MetricsSink` that records all events for inspection in tests. Thread-safe via `Mutex`.
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
    pub fn events(&self) -> Vec<MetricEvent> {
        self.events.lock().unwrap().clone()
    }

    /// Clear all recorded events.
    pub fn clear(&self) {
        self.events.lock().unwrap().clear();
    }

    /// Returns `true` if no events have been recorded since the last clear.
    pub fn is_empty(&self) -> bool {
        self.events.lock().unwrap().is_empty()
    }
}

impl Default for RecordingSink {
    fn default() -> Self {
        Self::new()
    }
}

impl MetricsSink for RecordingSink {
    fn emit(&self, event: MetricEvent) {
        self.events.lock().unwrap().push(event);
    }
}
