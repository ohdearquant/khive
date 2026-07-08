//! ADR-094 process-lifetime config lock ledger.
//!
//! Several `OnceLock`-backed config readers across packs (recall profiling,
//! ANN overfetch rounds, context profiling, ...) resolve an environment
//! variable exactly once per process and hold it for the process lifetime.
//! That first resolution is itself an auditable lifecycle event ("this
//! process locked config key K to value V"), but the `OnceLock::get_or_init`
//! closures that produce it run synchronously, off any async/event-store
//! context, and long before a `VerbRegistry` exists.
//!
//! This module bridges the two: `record_config_locked` is a plain sync
//! function any `OnceLock` closure can call to enqueue a `(key, value)` pair
//! into a process-wide queue. The registry's dispatch path drains the queue
//! (`take_config_locked`) at the same gate where it already appends audit
//! events, so `ConfigLocked` rows carry the namespace/actor of whichever
//! dispatch happens to observe the queue non-empty first (ADR-094's accepted
//! provenance quirk) rather than needing every `OnceLock` site to carry its
//! own `EventStore` handle.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

/// `true` once at least one pair has been enqueued and not yet drained.
///
/// Read via `swap(false, Ordering::AcqRel)` on the dispatch hot path so the
/// overwhelmingly common empty-ledger case pays for a single atomic
/// read-and-clear instead of locking `LEDGER`.
pub(crate) static PENDING: AtomicBool = AtomicBool::new(false);

static LEDGER: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());

/// Enqueue a config key/value pair for later `ConfigLocked` event emission.
///
/// Safe to call from a synchronous `OnceLock::get_or_init` closure — this
/// only takes a `std::sync::Mutex`, never awaits, and never fails.
pub fn record_config_locked(key: &'static str, value: impl Into<String>) {
    LEDGER
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push((key.to_string(), value.into()));
    // Set after enqueue so a dispatch that observes `true` is guaranteed to
    // find the pair already in `LEDGER`.
    PENDING.store(true, Ordering::Release);
}

/// Drain every queued config-locked pair for emission as `ConfigLocked` events.
///
/// Crate-private: only the dispatch path that owns event-store persistence
/// should drain this queue.
pub(crate) fn drain_config_locked() -> Vec<(String, String)> {
    std::mem::take(
        &mut *LEDGER
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()),
    )
}

/// `true` if at least one config-locked pair is queued, without draining it.
///
/// Test-only observability into the atomic fast path; the dispatch path
/// itself swaps `PENDING` directly rather than calling this.
#[cfg(test)]
pub(crate) fn has_pending_config_locked() -> bool {
    PENDING.load(Ordering::Acquire)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    // `PENDING`/`LEDGER` are process-wide singletons; serialize these tests
    // against each other so one test's queued pairs never leak into
    // another's assertions.
    #[test]
    #[serial(config_ledger)]
    fn record_then_drain_returns_queued_pairs_in_order() {
        drain_config_locked(); // drain any leftovers from a prior test run
        PENDING.store(false, Ordering::Release);
        assert!(!has_pending_config_locked());

        record_config_locked("recall_profile_enabled", "true");
        record_config_locked("ann_overfetch_max_rounds", "3");

        assert!(has_pending_config_locked());
        let drained = drain_config_locked();
        assert_eq!(
            drained,
            vec![
                ("recall_profile_enabled".to_string(), "true".to_string()),
                ("ann_overfetch_max_rounds".to_string(), "3".to_string()),
            ]
        );
    }

    /// Mirrors the `VerbRegistry::dispatch` fast path (ADR-094): rows drain
    /// exactly once via the atomic swap-and-check, and a second dispatch
    /// observes no pending work without needing to lock `LEDGER`.
    #[test]
    #[serial(config_ledger)]
    fn dispatch_fast_path_drains_exactly_once_then_reports_no_pending() {
        drain_config_locked();
        PENDING.store(false, Ordering::Release);

        record_config_locked("context_profile_enabled", "false");

        assert!(PENDING.swap(false, Ordering::AcqRel));
        let drained = drain_config_locked();
        assert_eq!(
            drained,
            vec![("context_profile_enabled".to_string(), "false".to_string())]
        );

        assert!(
            !PENDING.swap(false, Ordering::AcqRel),
            "a second dispatch must observe the flag already cleared"
        );
        assert!(
            drain_config_locked().is_empty(),
            "nothing should be left to re-emit on a later dispatch"
        );
    }
}
