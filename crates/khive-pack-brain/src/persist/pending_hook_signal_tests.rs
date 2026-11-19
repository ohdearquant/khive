use super::{PersistenceTracker, MAX_PENDING_HOOK_SIGNALS_PER_NAMESPACE};
use khive_brain_core::BrainSignal;

#[test]
fn cold_namespace_drops_irrelevant_and_keeps_recent_bounded_signals() {
    let mut tracker = PersistenceTracker::new();
    tracker.route_signal("cold", &BrainSignal::Irrelevant, 10);
    assert!(!tracker.pending_hook_signals.contains_key("cold"));

    for sequence in 0..MAX_PENDING_HOOK_SIGNALS_PER_NAMESPACE + 3 {
        tracker.route_signal(
            "cold",
            &BrainSignal::SearchCompleted {
                latency_us: sequence as i64,
            },
            10,
        );
    }
    let pending = tracker.drain_pending_signals("cold");
    assert_eq!(pending.len(), MAX_PENDING_HOOK_SIGNALS_PER_NAMESPACE);
    assert_eq!(tracker.pending_hook_signals_dropped(), 3);
    assert!(matches!(
        pending.first(),
        Some(BrainSignal::SearchCompleted { latency_us: 3 })
    ));
    assert!(matches!(
        pending.last(),
        Some(BrainSignal::SearchCompleted { latency_us })
            if *latency_us == (MAX_PENDING_HOOK_SIGNALS_PER_NAMESPACE + 2) as i64
    ));
    assert!(!tracker.is_loaded("cold"));
}
