//! Real append-contention regressions using a retained physical writer guard.
use super::resource_admission_tests::{CloseOnDrop, WallClock};
use super::*;
use khive_runtime::resource_admission::{BackgroundRoot, CleanupPolicy, WorkClassLimits};
use std::future::Future;
use std::time::{Duration, Instant};

#[derive(Default)]
struct TerminalGate {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
    append_observed: tokio::sync::Notify,
    append_returned: AtomicBool,
    store: std::sync::Mutex<Option<Arc<dyn khive_storage::EventStore>>>,
    armed: AtomicBool,
    pending_polls: AtomicUsize,
    cancel_on_pending: std::sync::Mutex<Option<AdmittedJob>>,
}
tokio::task_local! {
    static TERMINAL_GATE: Arc<TerminalGate>;
}

// Setup precedes finalize's first deadline calculation. No short-grace race
// with acquiring the test writer or constructing its dirty bridge is required.
pub(super) async fn before_terminal(rt: &KhiveRuntime, token: &NamespaceToken) {
    let gate = TERMINAL_GATE.try_with(Arc::clone).ok();
    if let Some(gate) = gate {
        let store = crate::store_access::acquire_store("memory.ann.event_store", {
            let runtime = rt.clone();
            let token = token.clone();
            move || runtime.events(&token)
        })
        .await
        .expect("terminal fixture event-store acquisition task")
        .expect("terminal fixture real token-scoped event store");
        *gate.store.lock().unwrap() = Some(store);
        gate.entered.notify_one();
        gate.release.notified().await;
        gate.armed.store(true, Ordering::SeqCst);
    }
}

// Only the armed terminal append uses the prepared real store. Start events and
// every other fixture retain ordinary acquisition and attribution behavior.
pub(super) fn terminal_store() -> Option<Arc<dyn khive_storage::EventStore>> {
    TERMINAL_GATE
        .try_with(|gate| {
            if gate.armed.load(Ordering::SeqCst) {
                gate.store.lock().unwrap().clone()
            } else {
                None
            }
        })
        .ok()
        .flatten()
}

// Observe the REAL append future. Never replace its result, force Pending,
// delay a poll, or intercept a start append before the terminal gate is armed.
pub(super) async fn observe_append<F: Future>(append: F) -> F::Output {
    let gate = TERMINAL_GATE
        .try_with(|gate| gate.armed.load(Ordering::SeqCst).then(|| gate.clone()))
        .ok()
        .flatten();
    tokio::pin!(append);
    std::future::poll_fn(|cx| {
        let result = append.as_mut().poll(cx);
        if result.is_pending() {
            if let Some(gate) = &gate {
                if let Some(job) = gate.cancel_on_pending.lock().unwrap().take() {
                    job.request_cancel();
                }
                gate.pending_polls.fetch_add(1, Ordering::SeqCst);
                gate.append_observed.notify_one();
            }
        } else if let Some(gate) = &gate {
            gate.append_returned.store(true, Ordering::SeqCst);
            gate.append_observed.notify_one();
        }
        result
    })
    .await
}

// On success or unwind, release the real writer before any blocking-holder or
// infrastructure join. No wait occurs inside an AtomicUnitOp callback.
struct ReleaseWriter(Option<std::sync::mpsc::Sender<()>>);

impl ReleaseWriter {
    fn release(&mut self) {
        if let Some(sender) = self.0.take() {
            let _ = sender.send(());
        }
    }
}

impl Drop for ReleaseWriter {
    fn drop(&mut self) {
        self.release();
    }
}

#[derive(Clone, Copy)]
enum Case {
    Success,
    Error,
    JobCancelled,
}

async fn append_timeout_case(case: Case) {
    let wall = WallClock::new();
    wall.wait(async {
        // Bound waiting for synchronous startup with the same real-wall alarm.
        let (rt, root) = tokio::task::spawn_blocking(|| {
            (
                KhiveRuntime::memory().expect("isolated in-memory runtime"),
                BackgroundRoot::start(
                    WorkClassLimits::validate(1, 1).unwrap(),
                    CleanupPolicy::new(Duration::from_millis(100)),
                )
                .expect("short-grace admission domain"),
            )
        })
        .await
        .unwrap();
        let _close_rt = CloseOnDrop::new(&rt);
        assert!(rt.backend().pool().canonical_path().is_none());
        let control = root.control();
        let rt_control = rt.background_domain().control();
        let token = rt.authorize(Namespace::local()).unwrap();
        let mut ann = new_shared_for_role(true);
        let state = Arc::get_mut(&mut ann).expect("fixture has the sole ANN owner");
        state.builds_corpus_indexes = true;
        state.checkpoint_timers_enabled = true;
        assert!(ann.builds_corpus_indexes && ann.checkpoint_timers_enabled);
        let key = AnnKey::from_token("terminal-timeout-model");
        ann.checkpoint_policy.write().unwrap().interval = Duration::from_secs(300);
        let tracking = TrackedJob::register("memory_terminal_timeout_test");
        let job = root
            .handle()
            .admit(WorkClass::Warm, &tracking)
            .await
            .unwrap();
        let probe = job.clone();
        let gate = Arc::new(TerminalGate::default());
        let task_gate = gate.clone();
        let view = rt.background_view();
        let task_ann = ann.clone();
        let background = root
            .handle()
            .spawn_supervised(job, move |job| async move {
                TERMINAL_GATE
                    .scope(task_gate, async {
                        if matches!(case, Case::Error) {
                            // A genuine storage error, not a cancellation-shaped stub.
                            let sql = view.sql();
                            let mut writer = sql.writer().await.unwrap();
                            let error = writer
                                .execute(SqlStatement {
                                    sql: "INSERT INTO absent_terminal_timeout_fixture VALUES (1)"
                                        .into(),
                                    params: vec![],
                                    label: Some("memory_terminal_timeout_driver_error".into()),
                                })
                                .await
                                .expect_err("missing table returns a genuine driver error");
                            drop(writer);
                            let error = RuntimeError::from(error);
                            assert!(!is_benign_shutdown_cancellation(&error));
                            finalize_ann_attempt(
                                &view,
                                &token,
                                "terminal-timeout-model",
                                &job,
                                JobOutcome::Completed(Err(error)),
                                1,
                                None,
                                AnnWarmDetails::default(),
                            )
                            .await
                        } else {
                            ensure_ann_for_model_admitted(
                                &view,
                                &token,
                                &task_ann,
                                "terminal-timeout-model",
                                &job,
                            )
                            .await
                        }
                    })
                    .await
            })
            .unwrap();
        let joined = background.join();
        tokio::pin!(joined);
        tokio::select! {
            result = &mut joined => {
                panic!("ANN supervisor completed before terminal fixture entry: {result:?}");
            }
            () = gate.entered.notified() => {}
        }
        assert!(!control.is_closed());
        assert!(!rt_control.is_closed());
        assert!(!probe.is_cancelled());

        // The actual body has completed. Model a dirty bridge published before
        // terminal telemetry finishes; the real scheduling path must observe it.
        let mut bridge = AnnBridge::build(
            vec![1.0, 0.0, 0.0, 1.0],
            2,
            vec![Uuid::from_u128(1), Uuid::from_u128(2)],
            HashSet::new(),
        )
        .unwrap();
        bridge.dirty_ops = 1;
        bridge.last_checkpoint = Instant::now();
        ann.indexes.write().await.insert(key.clone(), bridge);
        assert!(!ann.checkpoint_timers.lock().unwrap().contains(&key));
        // An in-memory SqlWriter facade retains only an Arc, not exclusion.
        // The public pool guard retains the physical writer on this blocking
        // thread, after the actual token-scoped event store is ready.
        let holder_view = rt.background_view();
        let (writer_ready, ready) = tokio::sync::oneshot::channel();
        let (writer_release, release) = std::sync::mpsc::channel();
        let mut release_writer = ReleaseWriter(Some(writer_release));
        let writer_held = Arc::new(AtomicBool::new(false));
        let held = writer_held.clone();
        let holder = tokio::task::spawn_blocking(move || {
            let backend = holder_view.backend();
            let pool = backend.pool();
            let writer = pool.writer().expect("physical fixture writer guard");
            held.store(true, Ordering::SeqCst);
            writer_ready.send(()).expect("fixture still awaits writer readiness");
            // Same ten-second ceiling as the existing WallClock; no unbounded
            // blocking job can survive cancellation of the async fixture.
            let released = release.recv_timeout(Duration::from_secs(10));
            held.store(false, Ordering::SeqCst);
            drop(writer);
            released.expect("physical writer must be released before its wall-clock ceiling");
        });
        tokio::select! {
            result = ready => result.expect("physical writer holder reached its ready barrier"),
            result = &mut joined => {
                panic!("ANN supervisor completed before writer holder readiness: {result:?}");
            }
        }
        assert!(writer_held.load(Ordering::SeqCst));
        if matches!(case, Case::JobCancelled) {
            *gate.cancel_on_pending.lock().unwrap() = Some(probe.clone());
        }
        gate.release.notify_one();
        tokio::select! {
            biased;
            () = gate.append_observed.notified() => {}
            result = &mut joined => {
                panic!("ANN supervisor completed before observing its actual terminal append: {result:?}");
            }
        }
        assert!(
            !gate.append_returned.load(Ordering::SeqCst),
            "actual terminal append returned while the physical writer was retained"
        );
        assert!(
            gate.pending_polls.load(Ordering::SeqCst) > 0,
            "must poll the actual append Pending under the held writer"
        );
        let outcome = joined.await.unwrap();
        assert!(
            writer_held.load(Ordering::SeqCst),
            "physical writer must remain held through terminal finalization"
        );
        assert!(
            !gate.append_returned.load(Ordering::SeqCst),
            "actual terminal append must still be pending when finalization ends"
        );
        match case {
            Case::Success => {
                assert!(!probe.is_cancelled());
                assert!(!control.is_closed());
                assert!(!rt_control.is_closed());
                assert!(
                    matches!(
                        outcome,
                        JobOutcome::Completed(Ok(AnnEnsureStatus::EmptyCorpus))
                    ),
                    "R5-A: open-domain terminal append timeout must retain Completed(Ok)"
                );
                assert!(
                    ann.checkpoint_timers.lock().unwrap().contains(&key),
                    "R5-A: successful real ANN adapter must schedule its dirty checkpoint"
                );
            }
            Case::Error => {
                assert!(!probe.is_cancelled());
                assert!(!control.is_closed());
                match outcome {
                    JobOutcome::Completed(Err(error)) => {
                        assert!(!is_benign_shutdown_cancellation(&error));
                        assert!(error
                            .to_string()
                            .contains("absent_terminal_timeout_fixture"));
                    }
                    other => {
                        panic!("R5-A: open-domain append timeout replaced body error: {other:?}")
                    }
                }
                assert!(!ann.checkpoint_timers.lock().unwrap().contains(&key));
            }
            Case::JobCancelled => {
                assert!(probe.is_cancelled());
                assert!(
                    matches!(outcome, JobOutcome::FinalizationTimedOut),
                    "shutdown/cancelled timeout outcome remains unchanged"
                );
                assert!(!ann.checkpoint_timers.lock().unwrap().contains(&key));
            }
        }
        release_writer.release();
        holder.await.expect("physical writer holder exited after explicit release");
        assert!(!writer_held.load(Ordering::SeqCst));
        drop((probe, tracking));
        // Cleanup is inside the same WallClock budget; no timer or child is
        // allowed to stand in for a test oracle by hanging the runner.
        rt_control.close_with_policy().unwrap();
        while ann.checkpoint_timers.lock().unwrap().contains(&key) {
            tokio::task::yield_now().await;
        }
        control.close_with_policy().unwrap();
        let deadline = Instant::now().checked_add(Duration::from_secs(2)).unwrap();
        let report = control.observe_exit_until(deadline).await.unwrap();
        assert!(report.workers_exited && report.coordinator_joined);
        assert_eq!(report.outstanding_jobs, 0);
        assert_eq!(
            report.incomplete_finalizations, 1,
            "one actual append timeout must remain counted exactly once"
        );
        let report = rt_control.observe_exit_until(deadline).await.unwrap();
        assert!(report.workers_exited && report.coordinator_joined);
    })
    .await;
}

#[tokio::test]
async fn open_domain_terminal_append_timeout_keeps_success_and_schedules_checkpoint() {
    append_timeout_case(Case::Success).await;
}

#[tokio::test]
async fn open_domain_terminal_append_timeout_keeps_body_error() {
    append_timeout_case(Case::Error).await;
}

#[tokio::test]
async fn cancelled_terminal_append_timeout_keeps_shutdown_outcome() {
    append_timeout_case(Case::JobCancelled).await;
}
