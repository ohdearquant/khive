//! Per-runtime config-lock observations for ADR-094.
//!
//! Runtime clones share one ledger; independent constructions do not.
//! State mutations use one blocking mutex, and pending-flag stores hold it.
//! Dispatch takes an owned batch before awaiting the bound runtime's
//! best-effort event appends.

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
    #[cfg(test)]
    test_hooks: Mutex<TestRecordHooks>,
}

#[derive(Default)]
struct State {
    seen: HashSet<&'static str>,
    pending: Vec<(&'static str, String)>,
}

#[cfg(test)]
#[derive(Clone, Copy)]
pub(crate) enum TestRecordPoint {
    BeforeLock,
    AfterUnlock,
}

#[cfg(test)]
type TestRecordHook = std::sync::Arc<dyn Fn() + Send + Sync>;

#[cfg(test)]
#[derive(Default)]
struct TestRecordHooks {
    before_lock: Option<TestRecordHook>,
    after_unlock: Option<TestRecordHook>,
}

impl ConfigLockLedger {
    pub(crate) fn record_config_locked<F>(&self, key: &'static str, value: F)
    where
        F: FnOnce() -> String,
    {
        #[cfg(test)]
        self.run_test_hook(TestRecordPoint::BeforeLock);
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.seen.contains(key) {
            return;
        }
        drop(state);
        // User code may reenter the ledger. An unwind makes no state change here.
        let value = value();
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // Another first reader may have recorded this name while we formatted it.
        if state.seen.insert(key) {
            state.pending.push((key, value));
            self.pending.store(true, Ordering::Release);
        }
        drop(state);
        #[cfg(test)]
        self.run_test_hook(TestRecordPoint::AfterUnlock);
    }

    #[cfg(test)]
    pub(crate) fn install_test_hook(&self, point: TestRecordPoint, hook: TestRecordHook) {
        let mut hooks = self.test_hooks.lock().unwrap();
        let slot = match point {
            TestRecordPoint::BeforeLock => &mut hooks.before_lock,
            TestRecordPoint::AfterUnlock => &mut hooks.after_unlock,
        };
        assert!(slot.replace(hook).is_none(), "one pause per boundary");
    }

    #[cfg(test)]
    fn run_test_hook(&self, point: TestRecordPoint) {
        let hook = {
            let mut hooks = self.test_hooks.lock().unwrap();
            match point {
                TestRecordPoint::BeforeLock => hooks.before_lock.take(),
                TestRecordPoint::AfterUnlock => hooks.after_unlock.take(),
            }
        };
        if let Some(hook) = hook {
            hook();
        }
    }

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

#[cfg(test)]
mod runtime_ledger_tests {
    use super::*;
    use serial_test::serial;
    use std::cell::Cell;
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use std::sync::atomic::AtomicUsize;
    use std::sync::{mpsc, Arc, Barrier};
    use std::time::Duration;

    const TEST_TIMEOUT: Duration = Duration::from_secs(10);

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
    fn concurrent_same_name_observations_enqueue_once() {
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
        assert!((1..=8).contains(&calls.load(Ordering::Relaxed)));
        assert_eq!(
            ledger.drain_config_locked(),
            vec![("ledger.concurrent", "value".to_string())]
        );
        assert_state(&ledger, 1, 0);
    }

    #[test]
    #[serial(config_ledger)]
    fn a_runtime_formatter_can_record_a_distinct_name() {
        let runtime = crate::KhiveRuntime::memory().expect("reentrant runtime");
        let (finished_tx, finished_rx) = mpsc::channel();
        // The watchdog reports the old implementation's deadlock as a test failure.
        // Do not join: a regression must not make this test wait without a bound.
        let _worker = std::thread::spawn(move || {
            runtime.record_config_locked("runtime.reentrant_outer", || {
                runtime.record_config_locked("runtime.reentrant_inner", || "inner".to_string());
                "outer".to_string()
            });
            let _ = finished_tx.send(runtime.drain_config_locked());
        });
        let batch = finished_rx
            .recv_timeout(TEST_TIMEOUT)
            .expect("a runtime formatter must complete distinct-name reentry before the deadline");
        assert_eq!(
            batch,
            vec![
                ("runtime.reentrant_inner", "inner".to_string()),
                ("runtime.reentrant_outer", "outer".to_string()),
            ]
        );
    }

    #[test]
    #[serial(config_ledger)]
    fn concurrent_runtime_first_readers_can_both_format_but_enqueue_once() {
        let runtime = crate::KhiveRuntime::memory().expect("concurrent runtime");
        let calls = Arc::new(AtomicUsize::new(0));
        let (entered_tx, entered_rx) = mpsc::channel();
        let (finished_tx, finished_rx) = mpsc::channel();
        let mut release_senders = Vec::new();
        for _ in 0..2 {
            let runtime = runtime.clone();
            let calls = calls.clone();
            let entered = entered_tx.clone();
            let finished = finished_tx.clone();
            let (release_tx, release_rx) = mpsc::channel();
            release_senders.push(release_tx);
            // Every gate and completion wait has a deadline; no worker is joined.
            let _worker = std::thread::spawn(move || {
                runtime.record_config_locked("runtime.concurrent_first", || {
                    calls.fetch_add(1, Ordering::Relaxed);
                    entered.send(()).expect("formatter barrier observer");
                    release_rx.recv_timeout(TEST_TIMEOUT).expect(
                        "both first-reader formatters must reach the barrier before release",
                    );
                    "value".to_string()
                });
                let _ = finished.send(());
            });
        }
        drop(entered_tx);
        drop(finished_tx);
        // Hold both closures here before either can finish or insert the name.
        for _ in 0..2 {
            entered_rx
                .recv_timeout(TEST_TIMEOUT)
                .expect("both first-reader formatters must enter before either is released");
        }
        for release in release_senders {
            release.send(()).expect("release a first-reader formatter");
        }
        for _ in 0..2 {
            finished_rx
                .recv_timeout(TEST_TIMEOUT)
                .expect("both runtime observations must finish before the deadline");
        }
        assert_eq!(calls.load(Ordering::Relaxed), 2);
        assert_eq!(
            runtime.drain_config_locked(),
            vec![("runtime.concurrent_first", "value".to_string())]
        );
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
