//! Pool-lifetime accounting for the runtime's note-search vector route.

use std::sync::atomic::{AtomicU64, Ordering};

use super::ConnectionPool;

/// Why note search selected its exact vector fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoteSearchFallbackCause {
    /// No default embedding model is configured at route selection.
    NoModel,
    /// No installed provider serves the actual opened backend.
    NoProvider,
    /// The provider has no eligible graph for this model and consumer.
    NoEligibleGraph,
}

/// Note-search routes selected for this opened pool, shared by all its handles.
///
/// Values reset only when the pool is reconstructed. A fallback counts the
/// selected route even if the exact search then fails. Embedding and provider
/// errors before a route is selected do not count. Concurrent reads are relaxed
/// snapshots; the fallback total is the saturating sum of the captured causes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct NoteSearchRouteSnapshot {
    /// ANN provider successes, including empty candidate results.
    pub ann_route_total: u64,
    /// Exact routes selected, the saturating sum of the captured causes.
    pub fallback_route_total: u64,
    /// Exact routes selected without a default embedding model.
    pub fallback_no_model_total: u64,
    /// Exact routes selected without a provider for this backend.
    pub fallback_no_provider_total: u64,
    /// Exact routes selected after the provider returned no eligible graph.
    pub fallback_no_eligible_graph_total: u64,
}

#[derive(Default)]
pub(super) struct NoteSearchRouteCounters {
    ann: AtomicU64,
    no_model: AtomicU64,
    no_provider: AtomicU64,
    no_eligible_graph: AtomicU64,
}

fn increment(counter: &AtomicU64) {
    let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
        Some(value.saturating_add(1))
    });
}

impl NoteSearchRouteCounters {
    fn record_ann(&self) {
        increment(&self.ann);
    }

    fn record_fallback(&self, cause: NoteSearchFallbackCause) {
        increment(match cause {
            NoteSearchFallbackCause::NoModel => &self.no_model,
            NoteSearchFallbackCause::NoProvider => &self.no_provider,
            NoteSearchFallbackCause::NoEligibleGraph => &self.no_eligible_graph,
        });
    }

    fn snapshot(&self) -> NoteSearchRouteSnapshot {
        let no_model = self.no_model.load(Ordering::Relaxed);
        let no_provider = self.no_provider.load(Ordering::Relaxed);
        let no_eligible_graph = self.no_eligible_graph.load(Ordering::Relaxed);
        NoteSearchRouteSnapshot {
            ann_route_total: self.ann.load(Ordering::Relaxed),
            fallback_route_total: no_model
                .saturating_add(no_provider)
                .saturating_add(no_eligible_graph),
            fallback_no_model_total: no_model,
            fallback_no_provider_total: no_provider,
            fallback_no_eligible_graph_total: no_eligible_graph,
        }
    }
}

impl ConnectionPool {
    /// Record an ANN route on the actual opened backend serving note search.
    pub fn record_note_search_ann_route(&self) {
        self.note_search_routes.record_ann();
    }

    /// Record one exact fallback route and its cause on this opened backend.
    pub fn record_note_search_fallback_route(&self, cause: NoteSearchFallbackCause) {
        self.note_search_routes.record_fallback(cause);
    }

    /// Snapshot this pool's note-search route counters without resetting them.
    pub fn note_search_route_snapshot(&self) -> NoteSearchRouteSnapshot {
        self.note_search_routes.snapshot()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_snapshot_counts_each_fallback_cause() {
        let counters = NoteSearchRouteCounters::default();
        counters.record_ann();
        counters.record_ann();
        counters.record_fallback(NoteSearchFallbackCause::NoModel);
        for _ in 0..2 {
            counters.record_fallback(NoteSearchFallbackCause::NoProvider);
        }
        for _ in 0..3 {
            counters.record_fallback(NoteSearchFallbackCause::NoEligibleGraph);
        }
        let expected = NoteSearchRouteSnapshot {
            ann_route_total: 2,
            fallback_route_total: 6,
            fallback_no_model_total: 1,
            fallback_no_provider_total: 2,
            fallback_no_eligible_graph_total: 3,
        };
        assert_eq!(counters.snapshot(), expected);
        assert_eq!(
            counters.snapshot(),
            expected,
            "snapshots must not reset counts"
        );
    }

    #[test]
    fn route_counters_and_fallback_sum_saturate() {
        let counters = NoteSearchRouteCounters::default();
        counters.ann.store(u64::MAX, Ordering::Relaxed);
        counters.no_model.store(u64::MAX, Ordering::Relaxed);
        counters.record_ann();
        counters.record_fallback(NoteSearchFallbackCause::NoModel);
        counters.record_fallback(NoteSearchFallbackCause::NoProvider);
        counters.record_fallback(NoteSearchFallbackCause::NoEligibleGraph);
        assert_eq!(
            counters.snapshot(),
            NoteSearchRouteSnapshot {
                ann_route_total: u64::MAX,
                fallback_route_total: u64::MAX,
                fallback_no_model_total: u64::MAX,
                fallback_no_provider_total: 1,
                fallback_no_eligible_graph_total: 1,
            }
        );
    }
}
