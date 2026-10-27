use super::*;
use std::time::Duration;

// One fixed real-clock budget encloses each entire authored fixture. Its
// alarm is independent of ambient paused Tokio time. Dropping the sender
// disarms recv_timeout; the watchdog retains no runtime, job or ANN owner.
const CASE_BUDGET: Duration = Duration::from_secs(10);
pub(super) struct WallClock {
    _disarm: std::sync::mpsc::Sender<()>,
    expired: tokio::sync::watch::Receiver<bool>,
}
impl WallClock {
    pub(super) fn new() -> Self {
        let (disarm, cancellation) = std::sync::mpsc::channel();
        let (alarm, expired) = tokio::sync::watch::channel(false);
        let watcher = std::thread::Builder::new()
            .name("memory-test-watchdog".into())
            .spawn(move || {
                if matches!(
                    cancellation.recv_timeout(CASE_BUDGET),
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout)
                ) {
                    let _ = alarm.send(true);
                }
            })
            .expect("real-wall fixture watchdog thread");
        drop(watcher);
        Self {
            _disarm: disarm,
            expired,
        }
    }

    pub(super) async fn wait<F: std::future::Future>(&self, future: F) -> F::Output {
        let mut expired = self.expired.clone();
        tokio::select! {
            biased;
            _ = async {
                loop {
                    if *expired.borrow_and_update() { break; }
                    expired.changed().await.expect("fixture alarm remains armed");
                }
            } => panic!("memory fixture exceeded its real-wall-clock watchdog"),
            result = future => result,
        }
    }
}

// Failure cleanup signals only; actual CPU release remains with ReleaseCpu,
// and this destructor performs no async writer or blocking infrastructure join.
pub(super) struct CloseOnDrop(khive_runtime::resource_admission::BackgroundDomainControl);
impl CloseOnDrop {
    pub(super) fn new(rt: &KhiveRuntime) -> Self {
        Self(rt.background_domain().control())
    }
}
impl Drop for CloseOnDrop {
    fn drop(&mut self) {
        let _ = self.0.close_with_policy();
    }
}

// Keep the fresh-claim synchronous fixture bounded without putting a possibly
// blocked std::sync lock on a Tokio worker. Only local fixture state is owned.
pub(super) fn run_sync_case(body: impl FnOnce() + Send + 'static) {
    let (completed, completion) = std::sync::mpsc::sync_channel(1);
    let worker = std::thread::Builder::new()
        .name("memory-sync-fixture".into())
        .spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(body));
            let _ = completed.send(result);
        })
        .expect("synchronous fixture thread");
    drop(worker);
    match completion.recv_timeout(CASE_BUDGET) {
        Ok(Ok(())) => {}
        Ok(Err(panic)) => std::panic::resume_unwind(panic),
        Err(error) => {
            panic!("synchronous memory fixture did not return within its real-wall budget: {error}")
        }
    }
}

// All callers are enclosed by their one WallClock case budget. This keeps
// existing call sites without restarting a timeout or using paused Tokio time.
async fn bounded<F: std::future::Future>(future: F) -> F::Output {
    future.await
}

async fn phase_rows(rt: &KhiveRuntime, token: &NamespaceToken) -> Vec<khive_storage::Event> {
    rt.events(token)
        .unwrap()
        .query_events(
            khive_storage::EventFilter {
                verbs: vec!["memory.ann_warm".into()],
                ..Default::default()
            },
            khive_storage::types::PageRequest {
                limit: 100,
                offset: 0,
            },
        )
        .await
        .unwrap()
        .items
}

#[tokio::test]
async fn interactive_ensure_bypasses_both_saturated_classes() {
    let wall = WallClock::new();
    wall.wait(async {
        let rt = KhiveRuntime::memory().unwrap();
        let _close_rt = CloseOnDrop::new(&rt);
        let token = rt.authorize(Namespace::local()).unwrap();
        let ann = new_shared_for_role(true);
        let handle = rt.background_domain();
        let tracking = TrackedJob::register("memory_interactive_bypass_test");
        let warm = handle.admit(WorkClass::Warm, &tracking).await.unwrap();
        let maintenance = handle
            .admit(WorkClass::Maintenance, &tracking)
            .await
            .unwrap();
        let status = bounded(ensure_ann_for_model(
            &rt,
            &token,
            &ann,
            "interactive-budget-model",
        ))
        .await
        .unwrap();
        assert_eq!(
            status,
            AnnEnsureStatus::EmptyCorpus,
            "interactive entry must not queue behind either class"
        );
        assert_eq!(phase_rows(&rt, &token).await.len(), 2);
        drop((warm, maintenance));
    })
    .await;
}

#[tokio::test]
async fn maintenance_attempt_uses_one_lease_but_keeps_warm_payload_and_actor() {
    let wall = WallClock::new();
    wall.wait(async {
        let rt = KhiveRuntime::memory().unwrap();
        let _close_rt = CloseOnDrop::new(&rt);
        let token = rt.authorize(Namespace::local()).unwrap();
        let ann = new_shared_for_role(true);
        let handle = rt.background_domain();
        let tracking = TrackedJob::register("memory_maintenance_attribution_test");
        let warm = handle.admit(WorkClass::Warm, &tracking).await.unwrap();
        let maintenance = handle
            .admit(WorkClass::Maintenance, &tracking)
            .await
            .unwrap();
        assert_eq!(maintenance.class(), WorkClass::Maintenance);
        let view = rt.background_view();
        let owned_token = token.clone();
        let background = handle
            .spawn_supervised(maintenance, move |job| async move {
                ensure_ann_for_model_admitted(
                    &view,
                    &owned_token,
                    &ann,
                    "maintenance-budget-model",
                    &job,
                )
                .await
            })
            .unwrap();
        assert!(
            matches!(
                bounded(background.join()).await.unwrap(),
                JobOutcome::Completed(Ok(AnnEnsureStatus::EmptyCorpus))
            ),
            "Maintenance must not acquire the occupied Warm permit"
        );
        let rows = phase_rows(&rt, &token).await;
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows.iter()
                .filter(|row| row.kind == khive_types::EventKind::PhaseStarted)
                .count(),
            1
        );
        assert_eq!(
            rows.iter()
                .filter(|row| row.kind == khive_types::EventKind::PhaseCompleted)
                .count(),
            1
        );
        for row in rows {
            assert_eq!(row.verb, "memory.ann_warm");
            assert_eq!(
                row.actor,
                format!("{}:{}", token.actor().kind, token.actor().id)
            );
            assert_eq!(row.payload["work_class"], "warm");
            assert_eq!(row.payload["phase"], "ann_warm");
        }
        drop(warm);
    })
    .await;
}

#[tokio::test]
async fn cancelled_real_memory_supervisor_attempts_one_cancelled_terminal() {
    let wall = WallClock::new();
    wall.wait(async {
        let rt = KhiveRuntime::memory().unwrap();
        let _close_rt = CloseOnDrop::new(&rt);
        let token = rt.authorize(Namespace::local()).unwrap();
        let ann = new_shared_for_role(true);
        let handle = rt.background_domain();
        let tracking = TrackedJob::register("memory_cancel_phase_test");
        let admitted = handle.admit(WorkClass::Warm, &tracking).await.unwrap();
        let gate = Arc::new(AnnAfterStart::default());
        let owned_gate = gate.clone();
        let view = rt.background_view();
        let owned_token = token.clone();
        let background = handle
            .spawn_supervised(admitted, move |job| async move {
                ensure_ann_for_model_admitted_inner(
                    &view,
                    &owned_token,
                    &ann,
                    "cancel-phase-model",
                    &job,
                    Some(&owned_gate),
                )
                .await
            })
            .unwrap();
        bounded(gate.entered.notified()).await;
        background.request_cancel();
        background.request_cancel();
        assert!(matches!(
            bounded(background.join()).await.unwrap(),
            JobOutcome::Cancelled
        ));
        let rows = phase_rows(&rt, &token).await;
        assert_eq!(
            rows.len(),
            2,
            "one start plus one terminal attempt on the actual adapter"
        );
        assert_eq!(
            rows.iter()
                .filter(|row| row.kind == khive_types::EventKind::PhaseStarted)
                .count(),
            1
        );
        assert_eq!(
            rows.iter()
                .filter(|row| row.kind == khive_types::EventKind::PhaseCancelled)
                .count(),
            1
        );
        assert!(!rows
            .iter()
            .any(|row| row.kind == khive_types::EventKind::PhaseCompleted));
        for row in rows {
            assert_eq!(row.payload["work_class"], "warm");
        }
    })
    .await;
}

#[tokio::test]
async fn finished_real_driver_error_keeps_completed_terminal_after_cancel() {
    let wall = WallClock::new();
    wall.wait(async {
        let rt = KhiveRuntime::memory().unwrap();
        let _close_rt = CloseOnDrop::new(&rt);
        let token = rt.authorize(Namespace::local()).unwrap();
        let sql = rt.sql();
        let mut writer = sql.writer().await.unwrap();
        let error = writer
            .execute(SqlStatement {
                sql: "INSERT INTO missing_memory_phase_fixture VALUES (1)".into(),
                params: vec![],
                label: Some("memory_phase_real_driver_error".into()),
            })
            .await
            .expect_err("a missing table must return a real storage failure");
        drop(writer);
        let error = RuntimeError::from(error);
        assert!(!is_benign_shutdown_cancellation(&error));
        let tracking = TrackedJob::register("memory_error_phase_test");
        let job = rt
            .background_domain()
            .admit(WorkClass::Warm, &tracking)
            .await
            .unwrap();
        job.request_cancel();
        let result = bounded(finalize_ann_attempt(
            &rt,
            &token,
            "driver-error-model",
            &job,
            JobOutcome::Completed(Err(error)),
            1,
            None,
            AnnWarmDetails::default(),
        ))
        .await;
        assert!(
            matches!(result, JobOutcome::Completed(Err(_))),
            "finished driver failure must remain genuine"
        );
        let rows = phase_rows(&rt, &token).await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind, khive_types::EventKind::PhaseCompleted);
        assert_eq!(rows[0].payload["path"], "failed");
    })
    .await;
}

#[tokio::test]
async fn cancelled_cpu_start_never_invokes_ann_build_body() {
    let wall = WallClock::new();
    wall.wait(async {
        let rt = KhiveRuntime::memory().unwrap();
        let _close_rt = CloseOnDrop::new(&rt);
        let tracking = TrackedJob::register("memory_cpu_refusal_test");
        let job = rt
            .background_domain()
            .admit(WorkClass::Warm, &tracking)
            .await
            .unwrap();
        let entered = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = entered.clone();
        let hook: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            count.fetch_add(1, Ordering::SeqCst);
        });
        job.request_cancel();
        let context = BackgroundAnn {
            job: &job,
            before_build: Some(&hook),
        };
        let result =
            bounded(context.build(vec![1.0, 0.0], 2, vec![Uuid::from_u128(1)], HashSet::new()))
                .await;
        assert!(matches!(result, Err(AnnBackgroundError::Cancelled)));
        assert_eq!(
            entered.load(Ordering::SeqCst),
            0,
            "cancelled start must refuse before the actual AnnBridge closure"
        );
    })
    .await;
}

#[derive(Default)]
struct CpuGate {
    entered: tokio::sync::Notify,
    released: std::sync::Mutex<bool>,
    changed: std::sync::Condvar,
}
impl CpuGate {
    fn enter(&self) {
        self.entered.notify_one();
        let guard = self.released.lock().unwrap();
        let (guard, _) = self
            .changed
            .wait_timeout_while(guard, Duration::from_secs(5), |released| !*released)
            .unwrap();
        assert!(
            *guard,
            "CPU test setup must release its actual child before the bounded watchdog"
        );
    }
    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.changed.notify_all();
    }
}
struct ReleaseCpu(Arc<CpuGate>);
impl Drop for ReleaseCpu {
    fn drop(&mut self) {
        self.0.release();
    }
}

#[tokio::test]
async fn started_ann_cpu_retains_admission_after_awaiter_is_dropped() {
    let wall = WallClock::new();
    wall.wait(async {
        let rt = KhiveRuntime::memory().unwrap();
        let _close_rt = CloseOnDrop::new(&rt);
        let handle = rt.background_domain();
        let tracking = TrackedJob::register("memory_cpu_actual_exit_test");
        let job = handle.admit(WorkClass::Warm, &tracking).await.unwrap();
        let gate = Arc::new(CpuGate::default());
        let release = ReleaseCpu(gate.clone());
        let owned_gate = gate.clone();
        let task = tokio::spawn(async move {
            let hook: Arc<dyn Fn() + Send + Sync> = Arc::new(move || owned_gate.enter());
            BackgroundAnn {
                job: &job,
                before_build: Some(&hook),
            }
            .build(
                vec![1.0, 0.0, 0.0, 1.0],
                2,
                vec![Uuid::from_u128(1), Uuid::from_u128(2)],
                HashSet::new(),
            )
            .await
        });
        bounded(gate.entered.notified()).await;
        task.abort();
        assert!(matches!(task.await, Err(error) if error.is_cancelled()));
        let next_tracking = TrackedJob::register("memory_cpu_waiter_test");
        let mut waiting = Box::pin(handle.admit(WorkClass::Warm, &next_tracking));
        std::future::poll_fn(|cx| {
            assert!(
                std::future::Future::poll(waiting.as_mut(), cx).is_pending(),
                "actual started CPU child must retain the original permit after proxy cancellation"
            );
            std::task::Poll::Ready(())
        })
        .await;
        release.0.release();
        let next = bounded(waiting).await.unwrap();
        drop((next, release));
    })
    .await;
}

#[tokio::test]
async fn read_only_memory_emitter_returns_before_store_and_serialization() {
    let wall = WallClock::new();
    wall.wait(async {
        struct MustNotSerialize;
        impl serde::Serialize for MustNotSerialize {
            fn serialize<S: serde::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
                panic!("read-only memory phase must return before serialization")
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let config = khive_runtime::RuntimeConfig {
            db_path: Some(dir.path().join("read-only-memory-phase.db")),
            ..khive_runtime::RuntimeConfig::no_embeddings()
        };
        // Keep the writable source alive; inspect its live WAL through the public
        // read-only constructor without a snapshot-sidecar helper dependency.
        let creator = KhiveRuntime::new(config.clone()).unwrap();
        let _close_creator = CloseOnDrop::new(&creator);
        let rt = KhiveRuntime::new_readonly_live(config).unwrap();
        let _close_rt = CloseOnDrop::new(&rt);
        let token = rt.authorize(Namespace::local()).unwrap();
        let before = rt.backend().pool().writer_acquisition_snapshot();
        emit_ann_warm_phase_event(
            &rt,
            &token,
            "read-only-model",
            khive_types::EventKind::PhaseStarted,
            MustNotSerialize,
        )
        .await;
        assert_eq!(
            rt.backend().pool().writer_acquisition_snapshot(),
            before,
            "read-only memory phase must not enter the writer plane"
        );
        drop((rt, creator));
    })
    .await;
}

#[tokio::test]
async fn cancellation_before_first_start_poll_emits_no_phase_rows() {
    let wall = WallClock::new();
    wall.wait(async {
    let rt = KhiveRuntime::memory().unwrap();
    let _close_rt = CloseOnDrop::new(&rt);
    let token = rt.authorize(Namespace::local()).unwrap();
    let ann = new_shared_for_role(true);
    let handle = rt.background_domain();
    let tracking = TrackedJob::register("memory_before_start_poll_test");
    let job = handle.admit(WorkClass::Warm, &tracking).await.unwrap();
    let gate = AnnAfterStart::default();
    gate.cancel_before_start.store(true, Ordering::SeqCst);
    let view = rt.background_view();
    let owned_token = token.clone();
    let background = handle
        .spawn_supervised(job, move |job| async move {
            ensure_ann_for_model_admitted_inner(
                &view,
                &owned_token,
                &ann,
                "before-start-poll-model",
                &job,
                Some(&gate),
            )
            .await
        })
        .unwrap();
    assert!(matches!(
        bounded(background.join()).await.unwrap(),
        JobOutcome::Cancelled
    ));
    assert!(phase_rows(&rt, &token).await.is_empty(),
        "cancellation before the emitter's first poll must not fabricate a terminal without a start attempt");
    }).await;
}
