//! Voluntary process-wide deferral of background work at cooperative boundaries.

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use tokio::sync::watch;

use crate::RuntimeError;

/// Maximum duration of one quiet request.
pub const MAX_QUIET_TTL_SECS: u64 = 300;
const MAX_QUIET_EPOCH: Duration = Duration::from_secs(900);
const RESUMED_WORK_INTERVAL: Duration = Duration::from_secs(60);

/// The active deadline and the next opening after the continuous quiet cap.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct QuietStatus {
    /// The winning UTC deadline while quiet is active.
    pub until: Option<DateTime<Utc>>,
    /// The estimated UTC time when the cap stops preventing renewal.
    pub retry_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Copy)]
struct Deadline {
    monotonic: Instant,
    utc: DateTime<Utc>,
}

#[derive(Clone, Copy)]
struct QuietEpoch {
    cap: Instant,
    deadline: Deadline,
}

impl QuietEpoch {
    fn resumed_at(self) -> Instant {
        self.deadline.monotonic + RESUMED_WORK_INTERVAL
    }

    fn retry_at(self) -> DateTime<Utc> {
        self.deadline.utc
            + chrono::Duration::from_std(RESUMED_WORK_INTERVAL)
                .expect("the resumed-work interval fits a UTC duration")
    }

    fn cap_prevents_renewal(self, now: Instant) -> bool {
        now < self.resumed_at() && self.cap <= now.max(self.deadline.monotonic)
    }
}

struct QuietState {
    epoch: Mutex<Option<QuietEpoch>>,
    changed: watch::Sender<()>,
}

impl QuietState {
    fn new() -> Self {
        Self {
            epoch: Mutex::new(None),
            changed: watch::channel(()).0,
        }
    }

    fn request(
        &self,
        ttl_secs: u64,
        clock: impl FnOnce() -> (DateTime<Utc>, Instant),
    ) -> Result<DateTime<Utc>, RuntimeError> {
        if !(1..=MAX_QUIET_TTL_SECS).contains(&ttl_secs) {
            return Err(RuntimeError::InvalidInput(
                "comm.quiet: ttl_secs must be an integer from 1 through 300".into(),
            ));
        }
        let mut epoch = super::lock(&self.epoch);
        let (utc, now) = clock();
        let requested = now + Duration::from_secs(ttl_secs);
        let cap = match *epoch {
            Some(current) if now < current.resumed_at() => {
                if requested <= current.deadline.monotonic {
                    return Ok(current.deadline.utc);
                }
                if current.cap_prevents_renewal(now) {
                    return Err(khive_types::KhiveError::conflict(format!(
                        "comm.quiet: continuous quiet cap reached. Retry at {}",
                        current.retry_at().to_rfc3339()
                    ))
                    .into());
                }
                current.cap
            }
            _ => now + MAX_QUIET_EPOCH,
        };
        let monotonic = requested.min(cap);
        let utc = utc
            + chrono::Duration::from_std(monotonic.duration_since(now))
                .expect("a quiet request fits a UTC duration");
        *epoch = Some(QuietEpoch {
            cap,
            deadline: Deadline { monotonic, utc },
        });
        // Retain the revision even with no receivers. Publishing while holding the
        // short state lock preserves concurrent request ordering; no await occurs.
        self.changed.send_replace(());
        Ok(utc)
    }

    fn status(&self, now: impl FnOnce() -> Instant) -> QuietStatus {
        let epoch = super::lock(&self.epoch);
        let Some(epoch) = *epoch else {
            return QuietStatus::default();
        };
        let now = now();
        QuietStatus {
            until: (epoch.deadline.monotonic > now).then_some(epoch.deadline.utc),
            retry_at: epoch.cap_prevents_renewal(now).then(|| epoch.retry_at()),
        }
    }

    async fn wait_until_not_quiet(&self, now: impl Fn() -> Instant) {
        // Subscribe before reading the deadline: a concurrent renewal cannot fall
        // between the observation and registration for the next notification.
        let mut changed = self.changed.subscribe();
        loop {
            let epoch = *super::lock(&self.epoch);
            let Some(deadline) = epoch
                .map(|epoch| epoch.deadline)
                .filter(|deadline| deadline.monotonic > now())
            else {
                return;
            };
            tokio::select! {
                _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline.monotonic)) => {},
                result = changed.changed() => {
                    // The process singleton retains the sender for its lifetime; a
                    // closed channel can bring no renewal, so the wait ends.
                    if result.is_err() {
                        return;
                    }
                }
            }
        }
    }
}

fn state() -> &'static QuietState {
    static STATE: OnceLock<QuietState> = OnceLock::new();
    STATE.get_or_init(QuietState::new)
}

/// Request quiet for 1 through 300 seconds, preserving a later active deadline.
/// Quiet windows separated by less than 60 seconds share one 900-second cap.
/// The returned UTC value belongs to the winning monotonic deadline.
pub fn request_quiet(ttl_secs: u64) -> Result<DateTime<Utc>, RuntimeError> {
    state().request(ttl_secs, || (Utc::now(), Instant::now()))
}

/// Report the current quiet deadline and an estimated UTC retry time when capped.
pub fn quiet_status() -> QuietStatus {
    state().status(Instant::now)
}

/// Observe the active quiet deadline, or None immediately after monotonic expiry.
pub fn quiet_until() -> Option<DateTime<Utc>> {
    quiet_status().until
}

/// Wait for quiet to expire, rechecking extensions after every wake.
/// Callers select this wait against their existing cancellation source. Never
/// hold a SQL/index/model guard while waiting, or delay a terminal finalizer.
pub async fn wait_until_not_quiet() {
    state().wait_until_not_quiet(Instant::now).await;
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

    fn clock_at(start: Instant, elapsed: Duration) -> (DateTime<Utc>, Instant) {
        let utc = DateTime::from_timestamp(1_700_000_000, 0).unwrap()
            + chrono::Duration::from_std(elapsed).unwrap();
        (utc, start + elapsed)
    }

    #[test]
    fn cap_clamps_extensions_and_refuses_until_the_exact_resume_boundary() {
        let quiet = QuietState::new();
        let start = Instant::now();
        let utc_at = |seconds| clock_at(start, Duration::from_secs(seconds)).0;
        let request =
            |seconds, ttl| quiet.request(ttl, || clock_at(start, Duration::from_secs(seconds)));
        let status = |seconds| quiet.status(|| start + Duration::from_secs(seconds));
        let mut changed = quiet.changed.subscribe();

        assert_eq!(request(0, 300).unwrap(), utc_at(300));
        assert_eq!(request(299, 300).unwrap(), utc_at(599));
        assert_eq!(request(598, 300).unwrap(), utc_at(898));
        assert_eq!(status(896).retry_at, None);
        assert_eq!(request(897, 300).unwrap(), utc_at(900));
        assert!(changed.has_changed().unwrap());
        drop(changed.borrow_and_update());

        assert_eq!(
            status(897),
            QuietStatus {
                until: Some(utc_at(900)),
                retry_at: Some(utc_at(960)),
            }
        );
        assert_eq!(request(898, 1).unwrap(), utc_at(900));
        assert_eq!(request(899, 1).unwrap(), utc_at(900));
        let (utc, now) = clock_at(start, Duration::from_secs(899));
        assert!(matches!(
            quiet.request(2, || (utc + chrono::Duration::days(1), now)),
            Err(RuntimeError::Khive(error)) if error.kind() == khive_types::ErrorKind::Conflict
        ));
        assert_eq!(status(899).retry_at, Some(utc_at(960)));
        assert!(request(900, 1).is_err());
        assert_eq!(
            status(900),
            QuietStatus {
                until: None,
                retry_at: Some(utc_at(960)),
            }
        );
        assert!(quiet
            .request(1, || clock_at(start, Duration::from_millis(959_999)))
            .is_err());
        assert!(!changed.has_changed().unwrap());
        assert_eq!(status(960), QuietStatus::default());
        assert_eq!(request(960, 300).unwrap(), utc_at(1260));
        assert!(changed.has_changed().unwrap());
        assert_eq!(status(960).retry_at, None);
    }

    #[test]
    fn short_gaps_keep_the_epoch_and_the_last_expiry_counts_toward_recovery() {
        let quiet = QuietState::new();
        let start = Instant::now();
        let utc_at = |seconds| clock_at(start, Duration::from_secs(seconds)).0;
        let request =
            |seconds, ttl| quiet.request(ttl, || clock_at(start, Duration::from_secs(seconds)));
        let status = |seconds| quiet.status(|| start + Duration::from_secs(seconds));

        assert_eq!(request(0, 300).unwrap(), utc_at(300));
        assert_eq!(request(350, 300).unwrap(), utc_at(650));
        assert_eq!(request(670, 210).unwrap(), utc_at(880));
        assert_eq!(status(899), QuietStatus::default());
        assert!(request(900, 300).is_err());
        assert_eq!(status(900).retry_at, Some(utc_at(940)));
        assert!(request(939, 1).is_err());
        assert_eq!(request(940, 300).unwrap(), utc_at(1240));
    }

    #[tokio::test(start_paused = true)]
    async fn a_capped_wait_expires_while_renewal_is_still_refused() {
        let quiet = QuietState::new();
        let now = || tokio::time::Instant::now().into_std();
        let start = now();
        let request = || quiet.request(300, || clock_at(start, now().duration_since(start)));

        request().unwrap();
        for _ in 0..3 {
            tokio::time::advance(Duration::from_secs(299)).await;
            request().unwrap();
        }
        let mut waiter = std::pin::pin!(quiet.wait_until_not_quiet(now));
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(std::future::Future::poll(waiter.as_mut(), &mut context).is_pending());
        tokio::time::advance(Duration::from_secs(3)).await;
        waiter.await;

        assert!(now() >= start + MAX_QUIET_EPOCH);
        assert_eq!(quiet.status(now).until, None);
        assert!(quiet.status(now).retry_at.is_some());
        assert!(request().is_err());
        tokio::time::advance(RESUMED_WORK_INTERVAL).await;
        assert_eq!(quiet.status(now), QuietStatus::default());
        request().unwrap();
        assert!(quiet.status(now).until.is_some());
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
            let original = super::super::lock(&state().epoch)
                .unwrap()
                .deadline
                .monotonic;
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
