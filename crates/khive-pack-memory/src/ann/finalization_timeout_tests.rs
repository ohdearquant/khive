//! Real append-contention regressions; no fixture or native run is claimed here.
use super::resource_admission_tests::{CloseOnDrop, WallClock};
use super::*;
use khive_runtime::resource_admission::{BackgroundRoot, CleanupPolicy, WorkClassLimits};
use std::future::Future;
use std::time::{Duration, Instant};

#[derive(Default)]
struct TerminalGate {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
    append_pending: tokio::sync::Notify,
    armed: AtomicBool,
    pending_polls: AtomicUsize,
    cancel_on_pending: std::sync::Mutex<Option<AdmittedJob>>,
}
tokio::task_local! {
    static TERMINAL_GATE: Arc<TerminalGate>;
}

// Setup precedes finalize's first deadline calculation. No short-grace race
// with acquiring the test writer or constructing its dirty bridge is required.
pub(super) async fn before_terminal() {
    let gate = TERMINAL_GATE.try_with(Arc::clone).ok();
    if let Some(gate) = gate {
        gate.entered.notify_one();
        gate.release.notified().await;
        gate.armed.store(true, Ordering::SeqCst);
    }
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
                gate.append_pending.notify_one();
            }
        }
        result
    })
    .await
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
        gate.entered.notified().await;
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
        let sql = rt.sql();
        let writer = sql.writer().await.unwrap();
        if matches!(case, Case::JobCancelled) {
            *gate.cancel_on_pending.lock().unwrap() = Some(probe.clone());
        }
        gate.release.notify_one();
        gate.append_pending.notified().await;
        assert!(
            gate.pending_polls.load(Ordering::SeqCst) > 0,
            "must poll the actual append Pending under the held writer"
        );
        let outcome = background.join().await.unwrap();
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
        drop(writer);
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
