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

use std::sync::Mutex;

static PENDING: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());

/// Enqueue a config key/value pair for later `ConfigLocked` event emission.
///
/// Safe to call from a synchronous `OnceLock::get_or_init` closure — this
/// only takes a `std::sync::Mutex`, never awaits, and never fails.
pub fn record_config_locked(key: &'static str, value: impl Into<String>) {
    PENDING
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push((key.to_string(), value.into()));
}

/// Drain every queued config-locked pair for emission as `ConfigLocked` events.
///
/// Crate-private: only the dispatch path that owns event-store persistence
/// should drain this queue.
pub(crate) fn take_config_locked() -> Vec<(String, String)> {
    std::mem::take(
        &mut *PENDING
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()),
    )
}

/// `true` if at least one config-locked pair is queued.
///
/// Lets the dispatch path skip locking the mutex a second time on the common
/// empty-queue path.
pub(crate) fn has_pending_config_locked() -> bool {
    !PENDING
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    // `PENDING` is a process-wide singleton; serialize these tests against
    // each other so one test's queued pairs never leak into another's
    // assertions.
    #[test]
    #[serial(config_ledger)]
    fn record_then_take_returns_queued_pairs_in_order() {
        take_config_locked(); // drain any leftovers from a prior test run
        assert!(!has_pending_config_locked());

        record_config_locked("recall_profile_enabled", "true");
        record_config_locked("ann_overfetch_max_rounds", "3");

        assert!(has_pending_config_locked());
        let drained = take_config_locked();
        assert_eq!(
            drained,
            vec![
                ("recall_profile_enabled".to_string(), "true".to_string()),
                ("ann_overfetch_max_rounds".to_string(), "3".to_string()),
            ]
        );
    }

    #[test]
    #[serial(config_ledger)]
    fn take_config_locked_drains_the_queue() {
        take_config_locked();
        record_config_locked("context_profile_enabled", "false");
        assert!(!take_config_locked().is_empty());
        assert!(
            !has_pending_config_locked(),
            "a second take must observe an empty queue"
        );
        assert_eq!(take_config_locked(), Vec::new());
    }
}
