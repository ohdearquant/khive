//! Observability sink for BM25 index operations.
//!
//! Provides a pluggable [`MetricsSink`] trait and a [`RecordingSink`] for tests.

use std::sync::Arc;

#[cfg(test)]
use parking_lot::Mutex;

/// A single metric event emitted by the BM25 index.
#[derive(Debug, Clone)]
pub struct MetricEvent {
    /// Event name (use the constants in [`names`]).
    pub name: &'static str,
    /// The value payload.
    pub value: MetricValue,
    /// Optional label key-value pairs.
    pub labels: Vec<(&'static str, String)>,
}

/// Value carried by a metric event.
#[derive(Debug, Clone, PartialEq)]
pub enum MetricValue {
    /// A monotonically increasing counter.
    Counter(u64),
    /// An instantaneous gauge.
    Gauge(f64),
    /// A histogram observation (e.g., duration in ms).
    Histogram(f64),
}

/// Trait for receiving metric events.
///
/// Implementations must be `Send + Sync` to allow the index to hold an
/// `Arc<dyn MetricsSink>` and emit events from `&self` methods.
pub trait MetricsSink: Send + Sync {
    /// Receive a single metric event.
    fn record(&self, event: MetricEvent);
}

/// Emit a metric event to the sink, if one is attached.
#[inline]
pub fn emit(sink: &Option<Arc<dyn MetricsSink>>, event: MetricEvent) {
    if let Some(s) = sink {
        s.record(event);
    }
}

/// Well-known metric name constants.
pub mod names {
    pub const BM25_INDEX_DURATION_MS: &str = "bm25.index_document.duration_ms";
    pub const BM25_INDEX_COUNT: &str = "bm25.index_document.count";
    pub const BM25_INDEX_SIZE: &str = "bm25.index.size";
    pub const BM25_SEARCH_DURATION_MS: &str = "bm25.search.duration_ms";
    pub const BM25_SEARCH_COUNT: &str = "bm25.search.count";
    pub const BM25_SEARCH_RESULTS: &str = "bm25.search.results";
}

/// In-memory sink that records all events. Used in tests.
#[cfg(test)]
pub struct RecordingSink {
    events: Mutex<Vec<MetricEvent>>,
}

#[cfg(test)]
impl RecordingSink {
    /// Create an empty recording sink.
    pub fn new() -> Self {
        Self {
            events: Mutex::new(Vec::new()),
        }
    }

    /// Return a snapshot of all recorded events.
    pub fn events(&self) -> Vec<MetricEvent> {
        self.events.lock().clone()
    }

    /// Clear all recorded events.
    pub fn clear(&self) {
        self.events.lock().clear();
    }

    /// Return `true` if no events have been recorded.
    pub fn is_empty(&self) -> bool {
        self.events.lock().is_empty()
    }
}

#[cfg(test)]
impl MetricsSink for RecordingSink {
    fn record(&self, event: MetricEvent) {
        self.events.lock().push(event);
    }
}
