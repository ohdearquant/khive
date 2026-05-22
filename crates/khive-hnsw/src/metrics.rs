//! Metrics infrastructure for HNSW observability.
//!
//! Consumers attach a `MetricsSink` implementation to an `HnswIndex` to
//! receive structured telemetry from insert, search, and rebuild operations.
//!
//! # Design
//!
//! The trait is object-safe (`Arc<dyn MetricsSink>`) so a single sink can be
//! shared across multiple index instances. The `emit` helper handles the
//! `None` case (no sink attached) at call sites.

use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------
// Metric value types
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Sink trait
// ---------------------------------------------------------------------------

/// Receiver for metric events from HNSW operations.
///
/// Implement this trait to bridge HNSW telemetry to your observability stack
/// (e.g., Prometheus, OpenTelemetry, tracing spans).
///
/// # Thread Safety
///
/// The trait requires `Send + Sync` so that `Arc<dyn MetricsSink>` can be
/// shared across threads.
pub trait MetricsSink: Send + Sync {
    /// Handle a metric event.
    fn emit(&self, event: MetricEvent);
}

// ---------------------------------------------------------------------------
// Emit helper
// ---------------------------------------------------------------------------

/// Emit a metric event to the attached sink, if any.
///
/// This is the call-site helper used by `HnswIndex` internals. It is a no-op
/// when `sink` is `None`.
#[inline]
pub fn emit(sink: &Option<Arc<dyn MetricsSink>>, event: MetricEvent) {
    if let Some(s) = sink {
        s.emit(event);
    }
}

// ---------------------------------------------------------------------------
// Metric name constants
// ---------------------------------------------------------------------------

/// Canonical metric name constants.
///
/// Using `&'static str` constants avoids string formatting on the hot path.
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

// ---------------------------------------------------------------------------
// Recording sink (test helper)
// ---------------------------------------------------------------------------

/// A `MetricsSink` that records all events for inspection in tests.
///
/// Thread-safe: uses an internal `Mutex`.
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
