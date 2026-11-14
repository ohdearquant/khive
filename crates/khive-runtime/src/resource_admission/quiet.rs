//! Voluntary process-wide deferral of background work at cooperative boundaries.

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use tokio::sync::watch;

use crate::RuntimeError;

/// Maximum duration of one quiet request. A live caller may renew the window.
pub const MAX_QUIET_TTL_SECS: u64 = 300;

#[derive(Clone, Copy)]
struct Deadline {
    monotonic: Instant,
    utc: DateTime<Utc>,
}

struct QuietState {
    deadline: Mutex<Option<Deadline>>,
    changed: watch::Sender<()>,
}

fn state() -> &'static QuietState {
    static STATE: OnceLock<QuietState> = OnceLock::new();
    STATE.get_or_init(|| QuietState {
        deadline: Mutex::new(None),
        changed: watch::channel(()).0,
    })
}

/// Request quiet for 1 through 300 seconds, preserving a later active deadline.
/// Validation precedes mutation. The returned UTC value belongs to the winning
/// monotonic deadline; wall-clock changes cannot shorten or prolong the wait.
pub fn request_quiet(ttl_secs: u64) -> Result<DateTime<Utc>, RuntimeError> {
    if !(1..=MAX_QUIET_TTL_SECS).contains(&ttl_secs) {
        return Err(RuntimeError::InvalidInput(
            "comm.quiet: ttl_secs must be an integer from 1 through 300".into(),
        ));
    }
    let next = Deadline {
        monotonic: Instant::now() + Duration::from_secs(ttl_secs),
        utc: Utc::now() + chrono::Duration::seconds(ttl_secs as i64),
    };
    let state = state();
    let mut current = super::lock(&state.deadline);
    if let Some(current) = *current {
        if current.monotonic >= next.monotonic {
            return Ok(current.utc);
        }
    }
    *current = Some(next);
    // Retain the revision even with no receivers. Publishing while holding the
    // short state lock preserves concurrent request ordering; no await occurs.
    state.changed.send_replace(());
    Ok(next.utc)
}

/// Observe the active quiet deadline, or None immediately after monotonic expiry.
pub fn quiet_until() -> Option<DateTime<Utc>> {
    super::lock(&state().deadline)
        .filter(|deadline| deadline.monotonic > Instant::now())
        .map(|deadline| deadline.utc)
}

/// Wait for quiet to expire, rechecking extensions after every wake.
/// Callers select this wait against their existing cancellation source. Never
/// hold a SQL/index/model guard while waiting, or delay a terminal finalizer.
pub async fn wait_until_not_quiet() {
    let state = state();
    // Subscribe before reading the deadline: a concurrent renewal cannot fall
    // between the observation and registration for the next notification.
    let mut changed = state.changed.subscribe();
    loop {
        let deadline = *super::lock(&state.deadline);
        let Some(deadline) = deadline.filter(|deadline| deadline.monotonic > Instant::now()) else {
            return;
        };
        tokio::select! {
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline.monotonic)) => {},
            result = changed.changed() => {
                // The process singleton retains the sender for its lifetime.
                result.expect("quiet state sender remains alive");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resource_admission::{
        BackgroundRoot, CleanupPolicy, JobOutcome, TrackedJob, WorkClass, WorkClassLimits,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    async fn bounded<F: std::future::Future>(future: F) -> F::Output {
        tokio::time::timeout(Duration::from_secs(8), future)
            .await
            .expect("quiet fixture exceeded its real-time budget")
    }

    async fn waiting(count: usize) {
        bounded(async {
            while state().changed.receiver_count() < count {
                tokio::task::yield_now().await;
            }
        })
        .await;
    }

    #[test]
    fn invalid_and_shorter_requests_preserve_the_effective_deadline() {
        if crate::test_process::run_in_child() {
            return;
        }
        assert_eq!(quiet_until(), None);
        let deadline = request_quiet(300).unwrap();
        assert_eq!(request_quiet(1).unwrap(), deadline);
        for ttl in [0, 301, u64::MAX] {
            assert!(matches!(
                request_quiet(ttl),
                Err(RuntimeError::InvalidInput(_))
            ));
            assert_eq!(quiet_until(), Some(deadline));
        }
    }

    #[tokio::test]
    async fn an_active_wait_observes_renewal_and_expiry() {
        if crate::test_process::run_in_child() {
            return;
        }
        bounded(async {
            request_quiet(1).unwrap();
            let original = super::super::lock(&state().deadline).unwrap().monotonic;
            let mut wait = tokio::spawn(wait_until_not_quiet());
            waiting(1).await;
            let renewed = request_quiet(2).unwrap();
            assert_eq!(quiet_until(), Some(renewed));
            tokio::time::sleep_until(tokio::time::Instant::from_std(
                original + Duration::from_millis(25),
            ))
            .await;
            assert!(
                quiet_until().is_some(),
                "renewal extends the original window"
            );
            assert!(
                !wait.is_finished(),
                "the old timer must not release a renewed wait"
            );
            (&mut wait).await.unwrap();
            assert_eq!(quiet_until(), None);
            wait_until_not_quiet().await;
        })
        .await;
    }

    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    #[tokio::test]
    async fn both_classes_defer_before_producer_entry_and_resume() {
        if crate::test_process::run_in_child() {
            return;
        }
        bounded(async {
            let root = BackgroundRoot::start(
                WorkClassLimits::validate(1, 1).unwrap(),
                CleanupPolicy::new(Duration::from_secs(2)),
            )
            .unwrap();
            let domain = root.handle();
            let tracking = TrackedJob::register("quiet_class_fixture");
            let entered = Arc::new(AtomicUsize::new(0));
            request_quiet(1).unwrap();
            let mut tasks = Vec::new();
            for class in [WorkClass::Warm, WorkClass::Maintenance] {
                let job = domain.admit(class, &tracking).await.unwrap();
                let entered = entered.clone();
                tasks.push(
                    domain
                        .spawn_supervised(job, move |_| async move {
                            assert_eq!(quiet_until(), None);
                            entered.fetch_add(1, Ordering::SeqCst);
                            JobOutcome::Completed(())
                        })
                        .unwrap(),
                );
            }
            waiting(2).await;
            assert_eq!(entered.load(Ordering::SeqCst), 0);
            for task in tasks {
                assert!(matches!(
                    task.join().await.unwrap(),
                    JobOutcome::Completed(())
                ));
            }
            assert_eq!(entered.load(Ordering::SeqCst), 2);
            drop(tracking);
            let report = root
                .control()
                .shutdown(Instant::now() + Duration::from_secs(3))
                .await
                .unwrap();
            assert_eq!(report.outstanding_jobs, 0);
            assert!(report.workers_exited && report.coordinator_joined);
        })
        .await;
    }

    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    #[tokio::test]
    async fn cancellation_while_quiet_releases_both_class_leases_without_entry() {
        if crate::test_process::run_in_child() {
            return;
        }
        bounded(async {
            let root = BackgroundRoot::start(
                WorkClassLimits::validate(1, 1).unwrap(),
                CleanupPolicy::new(Duration::from_secs(2)),
            )
            .unwrap();
            let domain = root.handle();
            let tracking = TrackedJob::register("quiet_cancel_fixture");
            request_quiet(300).unwrap();
            let mut tasks = Vec::new();
            for class in [WorkClass::Warm, WorkClass::Maintenance] {
                let job = domain.admit(class, &tracking).await.unwrap();
                tasks.push(
                    domain
                        .spawn_supervised(job, |_| async {
                            panic!("cancelled quiet job must not enter the producer");
                            #[allow(unreachable_code)]
                            JobOutcome::Completed(())
                        })
                        .unwrap(),
                );
            }
            waiting(2).await;
            for task in &tasks {
                task.request_cancel();
            }
            for task in tasks {
                assert!(matches!(task.join().await.unwrap(), JobOutcome::Cancelled));
            }
            assert!(
                quiet_until().is_some(),
                "cancellation must not wait for expiry"
            );
            for class in [WorkClass::Warm, WorkClass::Maintenance] {
                drop(domain.admit(class, &tracking).await.unwrap());
            }
            drop(tracking);
            let report = root
                .control()
                .shutdown(Instant::now() + Duration::from_secs(3))
                .await
                .unwrap();
            assert_eq!(report.outstanding_jobs, 0);
            assert!(report.workers_exited && report.coordinator_joined);
        })
        .await;
    }
}
