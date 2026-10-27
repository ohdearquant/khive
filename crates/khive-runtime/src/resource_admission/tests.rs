//! Source-only native test candidates. They are not executed in this packet.
use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::oneshot;

fn deadline() -> Instant {
    Instant::now().checked_add(Duration::from_secs(5)).unwrap()
}
fn root(warm: usize, maintenance: usize) -> BackgroundRoot {
    // Bound even the synchronous constructor's startup rendezvous. The helper
    // owns no observer join obligation; on timeout the receiver drops and any
    // eventual returned root drops/closes when send fails. This is fixture
    // containment, not evidence that production startup has a deadline.
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
    let initializer = std::thread::spawn(move || {
        let result = BackgroundRoot::start_inner(
            WorkClassLimits::validate(warm, maintenance).unwrap(),
            CleanupPolicy::new(Duration::from_secs(2)),
            Some(1),
        );
        let _ = ready_tx.send(result);
    });
    drop(initializer);
    ready_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("root startup fixture timed out")
        .unwrap()
}
// Tests use real (unpaused) Tokio time. Timeout is harness failure, never a
// semantic pass. Every blocked CPU fixture also has its own finite recv bound.
async fn bounded<F: Future>(future: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(5), future)
        .await
        .expect("test rendezvous exceeded wall-clock budget")
}
struct ReleaseOnDrop(Option<std::sync::mpsc::Sender<()>>);
impl ReleaseOnDrop {
    fn send(mut self, value: ()) -> Result<(), std::sync::mpsc::SendError<()>> {
        self.0.take().expect("one release").send(value)
    }
}
impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        if let Some(sender) = self.0.take() {
            let _ = sender.send(());
        }
    }
}
async fn observe_join(control: &BackgroundDomainControl) -> ShutdownReport {
    // Test-only inspection of independent publication after the domain deadline
    // expired. This does not request another cleanup window or mutate state.
    let mut changed = control.domain.changed.subscribe();
    bounded(async {
        loop {
            let joined = { lock(&control.domain.join).joined.clone() };
            if let Some(result) = joined {
                result.unwrap();
                return control.domain.report();
            }
            changed.changed().await.expect("control retains sender");
        }
    })
    .await
}
async fn assert_settled(root: &BackgroundRoot) {
    let control = root.control();
    let report = bounded(control.shutdown(deadline())).await.unwrap();
    let report = if report.coordinator_joined {
        report
    } else {
        observe_join(&control).await
    };
    assert!(report.workers_exited && report.coordinator_joined);
    assert_eq!(report.outstanding_jobs, 0);
}

#[test]
fn limits_refuse_zero_and_dependency_capacity_overflow() {
    assert!(matches!(
        WorkClassLimits::validate(0, 1),
        Err(ResourceError::InvalidLimit {
            class: WorkClass::Warm,
            ..
        })
    ));
    assert!(matches!(
        WorkClassLimits::validate(1, 0),
        Err(ResourceError::InvalidLimit {
            class: WorkClass::Maintenance,
            ..
        })
    ));
    let too_large = Semaphore::MAX_PERMITS.checked_add(1).unwrap();
    assert!(matches!(
        WorkClassLimits::validate(too_large, 1),
        Err(ResourceError::InvalidLimit { .. })
    ));
    let limits = WorkClassLimits::validate(2, 3).unwrap();
    assert_eq!((limits.warm(), limits.maintenance()), (2, 3));
}
#[test]
fn cleanup_overflow_is_typed_and_zero_is_valid() {
    assert!(matches!(
        CleanupPolicy::new(Duration::MAX).deadline_from(Instant::now()),
        Err(ResourceError::DeadlineOverflow)
    ));
    let now = Instant::now();
    assert_eq!(
        CleanupPolicy::new(Duration::ZERO)
            .deadline_from(now)
            .unwrap(),
        now
    );
}

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
#[tokio::test]
async fn domains_and_classes_are_independent_but_each_class_waits() {
    let first = root(1, 1);
    let second = root(1, 1);
    assert!(first.same_domain(&first.clone()));
    assert!(!first.same_domain(&second));
    let tracking = TrackedJob::register("resource_test_class_domains");
    let first_handle = first.handle();
    let warm = bounded(first_handle.admit(WorkClass::Warm, &tracking))
        .await
        .unwrap();
    let second_handle = second.handle();
    let independent = bounded(second_handle.admit(WorkClass::Warm, &tracking))
        .await
        .unwrap();
    let maintenance = bounded(first_handle.admit(WorkClass::Maintenance, &tracking))
        .await
        .unwrap();
    let mut waiter = Box::pin(first_handle.admit(WorkClass::Warm, &tracking));
    tokio::select! {
        biased;
        _ = &mut waiter => panic!("Warm over-admitted while its real lease was held"),
        _ = tokio::task::yield_now() => {},
    }
    drop(warm);
    let admitted = tokio::time::timeout(Duration::from_secs(2), &mut waiter)
        .await
        .unwrap()
        .unwrap();
    drop(waiter);
    drop((admitted, independent, maintenance, tracking));
    assert_settled(&first).await;
    assert_settled(&second).await;
}

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
#[tokio::test]
async fn queued_child_cancelled_before_start_never_invokes_user_body() {
    let root = root(2, 1);
    let handle = root.handle();
    let tracking = TrackedJob::register("resource_test_queued_cpu");
    let first = bounded(handle.admit(WorkClass::Warm, &tracking))
        .await
        .unwrap();
    let second = bounded(handle.admit(WorkClass::Warm, &tracking))
        .await
        .unwrap();
    drop(tracking);
    let (started_tx, started_rx) = oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let release_tx = ReleaseOnDrop(Some(release_tx));
    let first_child = first
        .spawn_blocking(move || {
            let _ = started_tx.send(());
            release_rx
                .recv_timeout(Duration::from_secs(10))
                .expect("CPU fixture release timed out");
        })
        .unwrap();
    drop(first);
    bounded(started_rx).await.unwrap();
    let called = Arc::new(AtomicBool::new(false));
    let body_called = called.clone();
    let queued = second
        .spawn_blocking(move || {
            body_called.store(true, Ordering::SeqCst);
        })
        .unwrap();
    drop(second);
    queued.request_cancel();
    assert_eq!(root.owner.domain.jobs.load(Ordering::SeqCst), 2);
    assert!(crate::daemon::background_task_names()
        .iter()
        .any(|n| n == "resource_test_queued_cpu"));
    release_tx.send(()).unwrap();
    assert!(matches!(
        bounded(first_child.join()).await.unwrap(),
        JobOutcome::Completed(())
    ));
    assert!(matches!(
        bounded(queued.join()).await.unwrap(),
        JobOutcome::Cancelled
    ));
    assert!(!called.load(Ordering::SeqCst));
    assert_settled(&root).await;
    assert!(!crate::daemon::background_task_names()
        .iter()
        .any(|n| n == "resource_test_queued_cpu"));
}

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
#[tokio::test]
async fn started_child_holds_admission_and_real_tracker_after_proxy_drop() {
    let root = root(1, 1);
    let handle = root.handle();
    let tracking = TrackedJob::register("resource_test_started_cpu");
    let job = bounded(handle.admit(WorkClass::Warm, &tracking))
        .await
        .unwrap();
    drop(tracking);
    let (started_tx, started_rx) = oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let release_tx = ReleaseOnDrop(Some(release_tx));
    let child = job
        .spawn_blocking(move || {
            let _ = started_tx.send(());
            release_rx
                .recv_timeout(Duration::from_secs(10))
                .expect("CPU fixture release timed out");
        })
        .unwrap();
    drop(job);
    bounded(started_rx).await.unwrap();
    drop(child); // Only the async/result observer disappears.
    assert_eq!(root.owner.domain.jobs.load(Ordering::SeqCst), 1);
    assert_eq!(root.owner.domain.warm.available_permits(), 0);
    assert!(crate::daemon::background_task_names()
        .iter()
        .any(|n| n == "resource_test_started_cpu"));
    let report = bounded(root.control().shutdown(Instant::now()))
        .await
        .unwrap();
    assert!(!report.workers_exited && !report.coordinator_joined);
    assert_eq!(report.outstanding_jobs, 1);
    release_tx.send(()).unwrap();
    assert_settled(&root).await;
    assert!(!crate::daemon::background_task_names()
        .iter()
        .any(|n| n == "resource_test_started_cpu"));
}

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
#[tokio::test]
async fn posture_refusal_is_checked_inside_actual_blocking_wrapper() {
    let root = root(1, 1);
    // Private test pool has one worker, so the next queued wrapper uses the
    // worker whose entry posture is deliberately refused here.
    bounded(
        root.owner
            .executor
            .spawn_blocking(priority::refuse_current_thread_for_test),
    )
    .await
    .unwrap();
    let tracking = TrackedJob::register("resource_test_posture_refusal");
    let job = bounded(root.handle().admit(WorkClass::Warm, &tracking))
        .await
        .unwrap();
    let called = Arc::new(AtomicBool::new(false));
    let body_called = called.clone();
    let child = job
        .spawn_blocking(move || {
            body_called.store(true, Ordering::SeqCst);
        })
        .unwrap();
    drop((job, tracking));
    assert!(matches!(
        bounded(child.join()).await.unwrap(),
        JobOutcome::Refused(_)
    ));
    assert!(!called.load(Ordering::SeqCst));
    assert_settled(&root).await;
}

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
#[tokio::test]
async fn caller_drop_keeps_supervisor_alive_for_one_terminal_attempt() {
    let root = root(1, 1);
    let tracking = TrackedJob::register("resource_test_finalizer");
    let handle = root.handle();
    let job = bounded(handle.admit(WorkClass::Warm, &tracking))
        .await
        .unwrap();
    drop(tracking);
    let attempts = Arc::new(AtomicUsize::new(0));
    let observed_attempts = attempts.clone();
    let (body_tx, body_rx) = oneshot::channel();
    let (final_tx, final_rx) = oneshot::channel();
    let task = handle
        .spawn_supervised(job, move |job| async move {
            let outcome = job
                .run_body(async {
                    let _ = body_tx.send(());
                    std::future::pending::<()>().await
                })
                .await;
            assert!(matches!(outcome, BodyOutcome::Cancelled));
            assert_eq!(
                job.finalize(async {
                    observed_attempts.fetch_add(1, Ordering::SeqCst);
                })
                .await,
                FinalizationOutcome::AttemptReturned
            );
            assert_eq!(
                job.finalize(async {
                    panic!("duplicate terminal append polled");
                })
                .await,
                FinalizationOutcome::AlreadyFinalized
            );
            let _ = final_tx.send(());
            JobOutcome::<()>::Cancelled
        })
        .unwrap();
    bounded(body_rx).await.unwrap();
    drop(task);
    tokio::time::timeout(Duration::from_secs(2), final_rx)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    assert_settled(&root).await;
}

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
#[tokio::test]
async fn pending_finalization_observes_shortened_domain_deadline() {
    let root = root(1, 1);
    let handle = root.handle();
    let tracking = TrackedJob::register("resource_test_deadline_shortening");
    let job = bounded(handle.admit(WorkClass::Warm, &tracking))
        .await
        .unwrap();
    drop(tracking);
    let (entered_tx, entered_rx) = oneshot::channel();
    let task = handle
        .spawn_supervised(job, move |job| async move {
            let result = job
                .finalize(async {
                    let _ = entered_tx.send(());
                    std::future::pending::<()>().await
                })
                .await;
            JobOutcome::Completed(result)
        })
        .unwrap();
    bounded(entered_rx).await.unwrap();
    // Shutdown may hard-drop the supervisor at the same deadline. Either a
    // returned timeout or a hard cancellation is acceptable only with an
    // incomplete-finalization diagnostic and no duplicate append.
    let report = bounded(root.control().shutdown(Instant::now()))
        .await
        .unwrap();
    let result = bounded(task.join()).await;
    assert!(matches!(
        result,
        Ok(JobOutcome::Completed(FinalizationOutcome::AttemptTimedOut))
            | Err(BackgroundJoinError::Cancelled)
    ));
    assert_settled(&root).await;
    assert!(
        report.incomplete_finalizations > 0
            || root.owner.domain.incomplete.load(Ordering::SeqCst) > 0
    );
}

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
#[tokio::test]
async fn cancelled_first_shutdown_observer_cannot_strand_second_observer() {
    let root = root(1, 1);
    let tracking = TrackedJob::register("resource_test_shutdown_observer");
    let job = bounded(root.handle().admit(WorkClass::Warm, &tracking))
        .await
        .unwrap();
    drop(tracking);
    let (entered_tx, entered_rx) = oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let release_tx = ReleaseOnDrop(Some(release_tx));
    let cpu = job
        .spawn_blocking(move || {
            let _ = entered_tx.send(());
            release_rx
                .recv_timeout(Duration::from_secs(10))
                .expect("CPU fixture release timed out");
        })
        .unwrap();
    drop(job);
    bounded(entered_rx).await.unwrap();
    let control = root.control();
    let mut first = Box::pin(control.shutdown(deadline()));
    tokio::select! {
        biased;
        _ = &mut first => panic!("running CPU child was incorrectly reported joined"),
        _ = tokio::task::yield_now() => {},
    }
    drop(first); // Drop the owning pinned future; the reaper must still publish.
    release_tx.send(()).unwrap();
    assert!(matches!(
        bounded(cpu.join()).await.unwrap(),
        JobOutcome::Completed(())
    ));
    assert_settled(&root).await;
}

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
#[tokio::test]
async fn readonly_real_phase_emitter_never_resolves_writer_or_serializes() {
    struct MustNotSerialize;
    impl serde::Serialize for MustNotSerialize {
        fn serialize<S: serde::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
            panic!("read-only telemetry must not serialize");
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let config = crate::RuntimeConfig {
        db_path: Some(dir.path().join("resource-readonly.db")),
        ..crate::RuntimeConfig::no_embeddings()
    };
    drop(crate::KhiveRuntime::new_for_test(config.clone()).unwrap());
    #[cfg(unix)]
    khive_storage::test_support::freeze_snapshot_sidecars(config.db_path.as_ref().unwrap());
    let runtime = crate::KhiveRuntime::new_readonly_for_test(config).unwrap();
    let before = runtime.backend().pool().writer_acquisition_snapshot();
    let token = runtime.authorize(crate::Namespace::local()).unwrap();
    let root = root(1, 1);
    let handle = root.handle();
    let tracking = TrackedJob::register("resource_test_readonly_finalizer");
    let job = bounded(handle.admit(WorkClass::Warm, &tracking))
        .await
        .unwrap();
    drop(tracking);
    let task = handle
        .spawn_supervised(job, move |job| async move {
            let result = job
                .finalize(crate::emit_phase_event(
                    &runtime,
                    &token,
                    "resource.readonly",
                    khive_types::EventKind::PhaseCancelled,
                    MustNotSerialize,
                ))
                .await;
            assert_eq!(result, FinalizationOutcome::AttemptReturned);
            assert_eq!(
                runtime.backend().pool().writer_acquisition_snapshot(),
                before
            );
            JobOutcome::Completed(())
        })
        .unwrap();
    assert!(matches!(
        bounded(task.join()).await.unwrap(),
        JobOutcome::Completed(())
    ));
    assert_settled(&root).await;
}

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
#[tokio::test]
async fn saturated_supervised_jobs_obey_nondefault_limit_on_actual_executor() {
    let root = root(2, 1);
    let live = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let release = Arc::new(Semaphore::new(0));
    let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut tasks = Vec::new();
    for _ in 0..6 {
        let handle = root.handle();
        let tracking = TrackedJob::register("resource_test_saturated_executor");
        let (live, peak, release, entered_tx) = (
            live.clone(),
            peak.clone(),
            release.clone(),
            entered_tx.clone(),
        );
        tasks.push(tokio::spawn(async move {
            let job = bounded(handle.admit(WorkClass::Warm, &tracking))
                .await
                .unwrap();
            drop(tracking);
            let supervised = handle
                .spawn_supervised(job, move |job| async move {
                    let active = live.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(active, Ordering::SeqCst);
                    entered_tx.send(()).unwrap();
                    let permit = bounded(release.acquire()).await.unwrap();
                    live.fetch_sub(1, Ordering::SeqCst);
                    drop(permit);
                    job.no_telemetry();
                    JobOutcome::Completed(())
                })
                .unwrap();
            bounded(supervised.join()).await.unwrap()
        }));
    }
    tokio::time::timeout(Duration::from_secs(2), entered_rx.recv())
        .await
        .unwrap()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), entered_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(live.load(Ordering::SeqCst), 2);
    assert!(entered_rx.try_recv().is_err());
    // The independent Maintenance class remains immediately admissible while
    // Warm is saturated. Actual interactive dispatch integration is Root's test.
    let tracking = TrackedJob::register("resource_test_maintenance_bypass");
    let maintenance = bounded(root.handle().admit(WorkClass::Maintenance, &tracking))
        .await
        .unwrap();
    drop((maintenance, tracking));
    release.add_permits(2);
    for task in tasks {
        assert!(matches!(
            bounded(task).await.unwrap(),
            JobOutcome::Completed(())
        ));
    }
    assert_eq!(peak.load(Ordering::SeqCst), 2);
    assert_settled(&root).await;
}

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
#[tokio::test]
async fn lightweight_handles_do_not_keep_external_root_alive() {
    let root = root(1, 1);
    let handle = root.handle();
    let control = root.control();
    drop(root);
    assert!(control.is_closed());
    let tracking = TrackedJob::register("resource_test_root_drop");
    assert!(matches!(
        bounded(handle.admit(WorkClass::Warm, &tracking)).await,
        Err(ResourceError::Closed)
    ));
    drop(tracking);
    let report = bounded(control.shutdown(deadline())).await.unwrap();
    assert!(report.coordinator_joined && report.workers_exited);
}

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
#[tokio::test]
async fn closed_wait_resolves_when_close_precedes_waiter() {
    let root = root(1, 1);
    let handle = root.handle();
    let control = root.control();
    control.close(deadline());
    tokio::time::timeout(Duration::from_secs(2), async {
        handle.closed().await;
        control.closed().await;
    })
    .await
    .unwrap();
    assert_settled(&root).await;
}

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
#[tokio::test]
async fn closed_wait_wakes_existing_handle_and_control_subscribers() {
    let root = root(1, 1);
    let handle = root.handle();
    let control = root.control();
    let mut handle_wait = Box::pin(handle.closed());
    let mut control_wait = Box::pin(control.closed());
    // Biased select polls both real observers before yielding. Neither may
    // resolve until close, and the subscription must not consume admission.
    tokio::select! {
        biased;
        _ = &mut handle_wait => panic!("open domain resolved handle.closed"),
        _ = &mut control_wait => panic!("open domain resolved control.closed"),
        _ = tokio::task::yield_now() => {},
    }
    assert_eq!(root.owner.domain.jobs.load(Ordering::SeqCst), 0);
    assert_eq!(root.owner.domain.warm.available_permits(), 1);
    control.close(deadline());
    tokio::time::timeout(Duration::from_secs(2), async {
        handle_wait.await;
        control_wait.await;
    })
    .await
    .unwrap();
    assert_settled(&root).await;
}

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
#[tokio::test]
async fn closed_wait_observes_final_external_root_drop() {
    let root = root(1, 1);
    let handle = root.handle();
    let control = root.control();
    let mut waiting = Box::pin(handle.closed());
    tokio::select! {
        biased;
        _ = &mut waiting => panic!("live root unexpectedly closed"),
        _ = tokio::task::yield_now() => {},
    }
    // Retaining the waiter/handle/control must not retain ExternalOwner.
    drop(root);
    tokio::time::timeout(Duration::from_secs(2), waiting)
        .await
        .unwrap();
    assert!(control.is_closed());
    let report = bounded(control.shutdown(deadline())).await.unwrap();
    assert!(report.workers_exited && report.coordinator_joined);
}

struct DropProbe(Arc<AtomicBool>);
impl Drop for DropProbe {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
#[tokio::test]
async fn failed_enqueue_reclaims_unstarted_escrow_before_occupied_worker_exits() {
    // Inject after real Tokio enqueue, not before submission and not by
    // inducing OS pressure. A real one-worker pool keeps the wrapper queued.
    let root = root(2, 1);
    let handle = root.handle();
    let tracking = TrackedJob::register("resource_test_failure_occupied");
    let occupied = bounded(handle.admit(WorkClass::Warm, &tracking))
        .await
        .unwrap();
    drop(tracking);
    let (started_tx, started_rx) = oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let release_tx = ReleaseOnDrop(Some(release_tx));
    let running = occupied
        .spawn_blocking(move || {
            let _ = started_tx.send(());
            release_rx
                .recv_timeout(Duration::from_secs(10))
                .expect("CPU fixture release timed out");
        })
        .unwrap();
    drop(occupied);
    bounded(started_rx).await.unwrap();

    let tracking = TrackedJob::register("resource_test_failure_reclaimed");
    let unstarted = bounded(handle.admit(WorkClass::Warm, &tracking))
        .await
        .unwrap();
    drop(tracking);
    let dropped = Arc::new(AtomicBool::new(false));
    let probe = DropProbe(dropped.clone());
    let called = Arc::new(AtomicBool::new(false));
    let body_called = called.clone();
    *lock(&root.owner.domain.after_blocking_enqueue) =
        Some(Box::new(|| panic!("injected post-enqueue failure")));
    let result = unstarted.spawn_blocking(move || {
        body_called.store(true, Ordering::SeqCst);
        drop(probe);
    });
    assert!(matches!(result, Err(ResourceError::ThreadStart(_))));
    drop(unstarted);
    assert!(dropped.load(Ordering::SeqCst));
    assert!(!called.load(Ordering::SeqCst));
    assert_eq!(root.owner.domain.jobs.load(Ordering::SeqCst), 1);
    assert_eq!(root.owner.domain.warm.available_permits(), 1);
    assert!(!crate::daemon::background_task_names()
        .iter()
        .any(|n| n == "resource_test_failure_reclaimed"));
    // Admission settles BEFORE releasing the independently running child.
    let tracking = TrackedJob::register("resource_test_failure_next_waiter");
    let next = bounded(handle.admit(WorkClass::Warm, &tracking))
        .await
        .unwrap();
    drop((next, tracking));
    assert!(!root.control().is_closed());
    release_tx.send(()).unwrap();
    assert!(matches!(
        bounded(running.join()).await.unwrap(),
        JobOutcome::Completed(())
    ));
    // FIFO one-worker sentinel observes the failed inert wrapper's dequeue.
    bounded(root.owner.executor.spawn_blocking(|| ()))
        .await
        .unwrap();
    assert!(!called.load(Ordering::SeqCst));
    assert_settled(&root).await;
}

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
#[tokio::test]
async fn dequeued_wrapper_cannot_start_before_failed_handoff_publication() {
    let root = root(1, 1);
    let tracking = TrackedJob::register("resource_test_failure_dequeue_race");
    let job = bounded(root.handle().admit(WorkClass::Warm, &tracking))
        .await
        .unwrap();
    drop(tracking);
    let (dequeued_tx, dequeued_rx) = std::sync::mpsc::channel();
    *lock(&root.owner.domain.blocking_dequeued) = Some(dequeued_tx);
    let observed = Arc::new(AtomicBool::new(false));
    let fault_observed = observed.clone();
    *lock(&root.owner.domain.after_blocking_enqueue) = Some(Box::new(move || {
        // The actual controlled wrapper signals immediately before taking the
        // same gate. A bounded setup failure is NOT counted as race coverage.
        dequeued_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("wrapper did not dequeue");
        fault_observed.store(true, Ordering::SeqCst);
        panic!("injected failure with real worker waiting at start gate");
    }));
    let called = Arc::new(AtomicBool::new(false));
    let body_called = called.clone();
    let dropped = Arc::new(AtomicBool::new(false));
    let probe = DropProbe(dropped.clone());
    let result = job.spawn_blocking(move || {
        body_called.store(true, Ordering::SeqCst);
        drop(probe);
    });
    assert!(matches!(result, Err(ResourceError::ThreadStart(_))));
    assert!(
        observed.load(Ordering::SeqCst),
        "post-enqueue race setup did not complete"
    );
    drop(job);
    assert!(dropped.load(Ordering::SeqCst));
    assert_eq!(root.owner.domain.jobs.load(Ordering::SeqCst), 0);
    assert_eq!(root.owner.domain.warm.available_permits(), 1);
    bounded(root.owner.executor.spawn_blocking(|| ()))
        .await
        .unwrap();
    assert!(!called.load(Ordering::SeqCst));
    assert_settled(&root).await;
}

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
#[tokio::test]
async fn first_shutdown_observer_honors_shortened_shared_deadline() {
    let root = root(1, 1);
    let tracking = TrackedJob::register("resource_test_shared_shutdown_deadline");
    let job = bounded(root.handle().admit(WorkClass::Warm, &tracking))
        .await
        .unwrap();
    drop(tracking);
    let (started_tx, started_rx) = oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let release_tx = ReleaseOnDrop(Some(release_tx));
    let child = job
        .spawn_blocking(move || {
            let _ = started_tx.send(());
            release_rx
                .recv_timeout(Duration::from_secs(10))
                .expect("CPU fixture release timed out");
        })
        .unwrap();
    drop(job);
    bounded(started_rx).await.unwrap();
    let control = root.control();
    let long_deadline = Instant::now().checked_add(Duration::from_secs(30)).unwrap();
    let mut first = Box::pin(control.shutdown(long_deadline));
    tokio::select! {
        biased;
        _ = &mut first => panic!("live CPU child reported joined before deadline"),
        _ = tokio::task::yield_now() => {},
    }
    control.close(Instant::now());
    let report = bounded(first).await.unwrap();
    assert!(!report.workers_exited && !report.coordinator_joined);
    assert_eq!(report.outstanding_jobs, 1);
    assert_eq!(root.owner.domain.warm.available_permits(), 0);
    assert!(crate::daemon::background_task_names()
        .iter()
        .any(|n| n == "resource_test_shared_shutdown_deadline"));
    // A later caller cannot manufacture a fresh cleanup budget either.
    let later = bounded(control.shutdown(long_deadline)).await.unwrap();
    assert!(!later.coordinator_joined);
    release_tx.send(()).unwrap();
    assert!(matches!(
        bounded(child.join()).await.unwrap(),
        JobOutcome::Completed(())
    ));
    let report = observe_join(&control).await;
    assert!(report.workers_exited && report.coordinator_joined);
    assert_eq!(report.outstanding_jobs, 0);
}

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
#[tokio::test]
async fn domain_close_refuses_real_queued_child_while_started_child_retains_hold() {
    let root = root(2, 1);
    let handle = root.handle();
    let tracking = TrackedJob::register("resource_test_close_start_gate");
    let first = bounded(handle.admit(WorkClass::Warm, &tracking))
        .await
        .unwrap();
    let second = bounded(handle.admit(WorkClass::Warm, &tracking))
        .await
        .unwrap();
    drop(tracking);
    let (started_tx, started_rx) = oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let release_tx = ReleaseOnDrop(Some(release_tx));
    let running = first
        .spawn_blocking(move || {
            let _ = started_tx.send(());
            release_rx
                .recv_timeout(Duration::from_secs(10))
                .expect("CPU fixture release timed out");
        })
        .unwrap();
    drop(first);
    bounded(started_rx).await.unwrap();
    let called = Arc::new(AtomicBool::new(false));
    let body_called = called.clone();
    let queued = second
        .spawn_blocking(move || {
            body_called.store(true, Ordering::SeqCst);
        })
        .unwrap();
    drop(second);
    root.control().close(deadline());
    // Runtime may already have dropped the queued wrapper, but the started
    // body must still own its hold until the explicit release below.
    assert!(root.owner.domain.jobs.load(Ordering::SeqCst) >= 1);
    release_tx.send(()).unwrap();
    assert!(matches!(
        bounded(running.join()).await.unwrap(),
        JobOutcome::Completed(())
    ));
    // Tokio may drop a nonmandatory queued task during Runtime shutdown, or
    // dequeue our wrapper, which must refuse its body at the closed gate.
    assert!(matches!(
        bounded(queued.join()).await,
        Ok(JobOutcome::Cancelled) | Err(BackgroundJoinError::Cancelled)
    ));
    assert!(!called.load(Ordering::SeqCst));
    assert_settled(&root).await;
}

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
#[tokio::test]
async fn exit_observer_survives_expired_cleanup_and_cancelled_prior_waiter() {
    let root = root(1, 1);
    let tracking = TrackedJob::register("resource_test_exit_observer_after_idle_drain");
    let job = bounded(root.handle().admit(WorkClass::Warm, &tracking))
        .await
        .unwrap();
    let cancellation = job.lease.cancel.clone();
    drop(tracking);
    let (started_tx, started_rx) = oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let release_tx = ReleaseOnDrop(Some(release_tx));
    let child = job
        .spawn_blocking(move || {
            let _ = started_tx.send(());
            release_rx
                .recv_timeout(Duration::from_secs(10))
                .expect("CPU fixture release timed out");
        })
        .unwrap();
    drop(job);
    bounded(started_rx).await.unwrap();
    let control = root.control();
    let expired = Instant::now();
    control.close(expired);
    let cleanup_deadline = lock(&control.domain.gate).deadline;
    let job_deadline = *lock(&cancellation.deadline);
    let requested = cancellation.requested.load(Ordering::SeqCst);
    assert_eq!(cleanup_deadline, Some(expired));
    assert_eq!(control.domain.jobs.load(Ordering::SeqCst), 1);

    let mut first = Box::pin(control.observe_exit_until(deadline()));
    tokio::select! {
        biased;
        _ = &mut first => panic!("exit observation reused expired cleanup budget or claimed live CPU child joined"),
        _ = tokio::task::yield_now() => {},
    }
    assert!(lock(&control.domain.join).joining);
    drop(first); // Reaper ownership must not be in this cancelled observer.
    assert!(lock(&control.domain.join).joining);
    assert_eq!(lock(&control.domain.gate).deadline, cleanup_deadline);
    assert_eq!(*lock(&cancellation.deadline), job_deadline);
    assert_eq!(cancellation.requested.load(Ordering::SeqCst), requested);

    // A second observer with a fresh observation budget must remain pending
    // while the actual child survives the already-expired cleanup deadline.
    let mut second = Box::pin(control.observe_exit_until(deadline()));
    tokio::select! {
        biased;
        _ = &mut second => panic!("second observer failed to wait for actual worker exit"),
        _ = tokio::task::yield_now() => {},
    }
    assert_eq!(control.domain.jobs.load(Ordering::SeqCst), 1);
    assert!(crate::daemon::background_task_names()
        .iter()
        .any(|n| n == "resource_test_exit_observer_after_idle_drain"));
    release_tx.send(()).unwrap();
    let report = bounded(second).await.unwrap();
    assert!(report.workers_exited && report.coordinator_joined);
    assert_eq!(report.outstanding_jobs, 0);
    assert!(matches!(
        bounded(child.join()).await.unwrap(),
        JobOutcome::Completed(())
    ));
    assert_eq!(lock(&control.domain.gate).deadline, cleanup_deadline);
    assert_eq!(*lock(&cancellation.deadline), job_deadline);
    assert_eq!(cancellation.requested.load(Ordering::SeqCst), requested);
}

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
#[tokio::test]
async fn exit_observation_does_not_close_an_open_domain_or_stamp_cleanup() {
    let root = root(1, 1);
    let control = root.control();
    let report = bounded(control.observe_exit_until(Instant::now()))
        .await
        .unwrap();
    assert!(!report.workers_exited && !report.coordinator_joined);
    assert!(!control.is_closed());
    assert_eq!(lock(&control.domain.gate).deadline, None);
    let tracking = TrackedJob::register("resource_test_open_exit_observer");
    let admitted = bounded(root.handle().admit(WorkClass::Warm, &tracking))
        .await
        .unwrap();
    assert!(!admitted.is_cancelled());
    assert_eq!(*lock(&admitted.lease.cancel.deadline), None);
    drop((admitted, tracking));
    assert_settled(&root).await;
}

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
#[tokio::test]
async fn whole_supervisor_guard_stays_counted_until_actual_task_completion() {
    let root = root(1, 1);
    let handle = root.handle();
    let tracking = TrackedJob::register("resource_test_whole_supervisor_guard");
    let job = bounded(handle.admit(WorkClass::Warm, &tracking))
        .await
        .unwrap();
    drop(tracking);
    let (entered_tx, entered_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let task = handle
        .spawn_supervised(job, move |job| async move {
            let _ = entered_tx.send(());
            // Sender Drop on assertion unwinding is also a cleanup release.
            // A rendezvous timeout is fixture failure, not a passing control.
            let _ = bounded(release_rx).await;
            job.no_telemetry();
            JobOutcome::Completed(())
        })
        .unwrap();
    bounded(entered_rx)
        .await
        .expect("actual supervisor entered");

    // The actual body is still blocked and has not finalized. These counters,
    // not merely child execution or successful join, detect disjoint capture.
    assert_eq!(
        root.owner.domain.supervisors.load(Ordering::SeqCst),
        1,
        "pending actual supervisor must retain its entire Drop guard"
    );
    assert_eq!(
        root.owner.domain.incomplete.load(Ordering::SeqCst),
        0,
        "a blocked live supervisor is not incomplete finalization"
    );

    release_tx.send(()).expect("supervisor is still waiting");
    assert!(matches!(
        bounded(task.join()).await.unwrap(),
        JobOutcome::Completed(())
    ));
    assert_eq!(root.owner.domain.supervisors.load(Ordering::SeqCst), 0);
    assert_eq!(root.owner.domain.incomplete.load(Ordering::SeqCst), 0);
    assert_settled(&root).await;
}
