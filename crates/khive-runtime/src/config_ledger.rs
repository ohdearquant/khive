//! Runtime-owned config ledgers are introduced alongside the transitional global lane.
//! The legacy API below remains until all five migration steps are complete.
//!
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

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

/// One runtime construction's config-lock observations.
///
/// Fields remain private so only these two operations mutate the runtime
/// ledger. The atomic is read without the mutex, but every store holds it.
#[derive(Default)]
pub(crate) struct ConfigLockLedger {
    state: Mutex<State>,
    pending: AtomicBool,
}

#[derive(Default)]
struct State {
    seen: HashSet<&'static str>,
    pending: Vec<(&'static str, String)>,
}

impl ConfigLockLedger {
    pub(crate) fn record_config_locked<F>(&self, key: &'static str, value: F)
    where
        F: FnOnce() -> String,
    {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.seen.contains(key) {
            return;
        }
        // Format before marking seen: an unwind leaves seen and pending intact.
        // The production callers format primitives and never reenter this ledger.
        let value = value();
        if state.seen.insert(key) {
            state.pending.push((key, value));
            self.pending.store(true, Ordering::Release);
        }
    }

    // Transitional: Step 2 connects the bound registry and removes this allowance.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn drain_config_locked(&self) -> Vec<(&'static str, String)> {
        if !self.pending.load(Ordering::Acquire) {
            return Vec::new();
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let batch = std::mem::take(&mut state.pending);
        self.pending.store(false, Ordering::Release);
        batch
    }
}

// Transitional global lane: untouched producers and dispatch still use it.
// Remove the flag, queue, free functions and legacy fixtures in complete Step 5.

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

    /// Mirrors the dispatch fast path: rows drain exactly once via the atomic
    /// swap-and-check, and a second dispatch observes no pending work without
    /// needing to lock `LEDGER`.
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

#[cfg(test)]
mod runtime_ledger_tests {
    use super::*;
    use serial_test::serial;
    use std::cell::Cell;
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use std::sync::atomic::AtomicUsize;
    use std::sync::{Arc, Barrier};

    fn assert_state(ledger: &ConfigLockLedger, seen: usize, queued: usize) {
        let state = ledger
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert_eq!(state.seen.len(), seen);
        assert_eq!(state.pending.len(), queued);
        assert_eq!(
            ledger.pending.load(Ordering::Acquire),
            !state.pending.is_empty()
        );
    }

    #[test]
    #[serial(config_ledger)]
    fn first_values_are_kept_and_one_drain_takes_every_pending_name() {
        let ledger = ConfigLockLedger::default();
        assert_state(&ledger, 0, 0);
        ledger.record_config_locked("ledger.first", || "first".to_string());
        ledger.record_config_locked("ledger.second", || "second".to_string());
        ledger.record_config_locked("ledger.first", || panic!("repeat must not format"));
        assert_state(&ledger, 2, 2);
        assert_eq!(
            ledger.drain_config_locked(),
            vec![
                ("ledger.first", "first".to_string()),
                ("ledger.second", "second".to_string()),
            ]
        );
        assert_state(&ledger, 2, 0);
        ledger.record_config_locked("ledger.first", || panic!("draining must retain seen names"));
        assert!(ledger.drain_config_locked().is_empty());
        assert_state(&ledger, 2, 0);
    }

    #[test]
    #[serial(config_ledger)]
    fn repeated_observation_does_not_invoke_the_lazy_value() {
        let ledger = ConfigLockLedger::default();
        let calls = Cell::new(0);
        for _ in 0..32 {
            ledger.record_config_locked("ledger.lazy", || {
                calls.set(calls.get() + 1);
                "value".to_string()
            });
        }
        assert_eq!(calls.get(), 1);
        assert_eq!(
            ledger.drain_config_locked(),
            vec![("ledger.lazy", "value".to_string())]
        );
        ledger.record_config_locked("ledger.lazy", || panic!("a taken name is still seen"));
        assert!(ledger.drain_config_locked().is_empty());
    }

    #[test]
    #[serial(config_ledger)]
    fn formatter_panic_preserves_state_and_poison_recovery_can_retry_the_name() {
        for initially_pending in [false, true] {
            let ledger = ConfigLockLedger::default();
            let mut expected = Vec::new();
            if initially_pending {
                ledger.record_config_locked("ledger.before_panic", || "kept".to_string());
                expected.push(("ledger.before_panic", "kept".to_string()));
            }
            let failed = catch_unwind(AssertUnwindSafe(|| {
                ledger.record_config_locked("ledger.panic", || panic!("formatter failed"));
            }));
            assert!(failed.is_err());
            let count = usize::from(initially_pending);
            assert_state(&ledger, count, count);
            {
                let state = ledger
                    .state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                assert!(!state.seen.contains("ledger.panic"));
            }
            ledger.record_config_locked("ledger.panic", || "retried".to_string());
            expected.push(("ledger.panic", "retried".to_string()));
            assert_eq!(ledger.drain_config_locked(), expected);
            assert_state(&ledger, count + 1, 0);
        }
    }

    #[test]
    #[serial(config_ledger)]
    fn distinct_ledgers_keep_same_name_observations_separate() {
        let a = ConfigLockLedger::default();
        let b = ConfigLockLedger::default();
        a.record_config_locked("ledger.shared_name", || "a".to_string());
        b.record_config_locked("ledger.shared_name", || "b".to_string());
        assert_eq!(
            a.drain_config_locked(),
            vec![("ledger.shared_name", "a".to_string())]
        );
        assert_state(&a, 1, 0);
        assert_state(&b, 1, 1);
        assert_eq!(
            b.drain_config_locked(),
            vec![("ledger.shared_name", "b".to_string())]
        );
    }

    #[test]
    #[serial(config_ledger)]
    fn an_owned_drained_batch_does_not_erase_a_later_observation() {
        let ledger = ConfigLockLedger::default();
        ledger.record_config_locked("ledger.taken", || "old".to_string());
        let taken = ledger.drain_config_locked();
        ledger.record_config_locked("ledger.later", || "new".to_string());
        assert_eq!(taken, vec![("ledger.taken", "old".to_string())]);
        assert_state(&ledger, 2, 1);
        assert_eq!(
            ledger.drain_config_locked(),
            vec![("ledger.later", "new".to_string())]
        );
        assert_state(&ledger, 2, 0);
    }

    #[test]
    #[serial(config_ledger)]
    fn concurrent_same_name_observations_enqueue_and_format_once() {
        let ledger = Arc::new(ConfigLockLedger::default());
        let start = Arc::new(Barrier::new(8));
        let calls = Arc::new(AtomicUsize::new(0));
        let mut workers = Vec::new();
        for _ in 0..8 {
            let ledger = ledger.clone();
            let start = start.clone();
            let calls = calls.clone();
            workers.push(std::thread::spawn(move || {
                start.wait();
                ledger.record_config_locked("ledger.concurrent", || {
                    calls.fetch_add(1, Ordering::Relaxed);
                    "value".to_string()
                });
            }));
        }
        for worker in workers {
            worker.join().expect("ledger observation worker");
        }
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert_eq!(
            ledger.drain_config_locked(),
            vec![("ledger.concurrent", "value".to_string())]
        );
        assert_state(&ledger, 1, 0);
    }

    #[test]
    #[serial(config_ledger)]
    fn runtime_clones_and_unrebound_core_share_but_new_construction_is_separate() {
        let a = crate::KhiveRuntime::memory().expect("first runtime");
        let clone = a.clone();
        let core = a.core();
        let b = crate::KhiveRuntime::memory().expect("independent runtime");
        a.record_config_locked("runtime.same_name", || "a".to_string());
        clone.record_config_locked("runtime.same_name", || panic!("clone shares seen names"));
        core.record_config_locked("runtime.core_name", || "core".to_string());
        b.record_config_locked("runtime.same_name", || "b".to_string());
        assert_eq!(
            clone.drain_config_locked(),
            vec![
                ("runtime.same_name", "a".to_string()),
                ("runtime.core_name", "core".to_string()),
            ]
        );
        assert!(core.drain_config_locked().is_empty());
        assert_eq!(
            b.drain_config_locked(),
            vec![("runtime.same_name", "b".to_string())]
        );
        assert!(a.drain_config_locked().is_empty());
    }
}
