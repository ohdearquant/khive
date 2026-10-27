use super::KnowledgePack;
use async_trait::async_trait;
use khive_runtime::pack::PackRuntime;
use khive_runtime::resource_admission::{BackgroundDomainControl, TrackedJob, WorkClass};
use khive_runtime::{
    EmbedderProvider, EngineConfig, KhiveRuntime, Namespace, RuntimeConfig, RuntimeError,
    RuntimeResult,
};
use khive_storage::{Event, EventFilter, PageRequest};
use khive_types::EventKind;
use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};
use tokio::sync::Notify;

const WAIT: Duration = Duration::from_secs(10);
const MODEL: &str = "warm-admission-probe";
const LABEL: &str = "knowledge.embedder_warm";
const PHASE: &str = "knowledge_embedder_warm";

// One real-clock alarm per fixture. Paused ambient Tokio time cannot
// auto-advance this deadline. Dropping the sender disarms recv_timeout;
// the detached watchdog retains no runtime, provider or domain ownership.
struct WallClock {
    _disarm: std::sync::mpsc::Sender<()>,
    expired: tokio::sync::watch::Receiver<bool>,
}
impl WallClock {
    fn new() -> Self {
        let (disarm, cancellation) = std::sync::mpsc::channel();
        let (alarm, expired) = tokio::sync::watch::channel(false);
        let watchdog = std::thread::Builder::new()
            .name("warm-test-watchdog".into())
            .spawn(move || {
                if matches!(
                    cancellation.recv_timeout(WAIT),
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout)
                ) {
                    let _ = alarm.send(true);
                }
            })
            .expect("wall-clock watchdog thread");
        drop(watchdog);
        Self {
            _disarm: disarm,
            expired,
        }
    }

    async fn wait<F: std::future::Future>(&self, future: F) -> F::Output {
        let mut expired = self.expired.clone();
        tokio::select! {
            biased;
            _ = async {
                loop {
                    if *expired.borrow_and_update() { break; }
                    expired.changed().await.expect("wall-clock watchdog remains armed");
                }
            } => panic!("warm fixture exceeded its real-wall-clock watchdog"),
            result = future => result,
        }
    }
}

// A failed assertion or watchdog still closes this fixture's isolated domain.
// Cleanup is signalled only; no asynchronous writer or blocking join in Drop.
struct CloseOnDrop(BackgroundDomainControl);
impl Drop for CloseOnDrop {
    fn drop(&mut self) {
        let _ = self.0.close_with_policy();
    }
}

struct Probe {
    entered: Notify,
    release: Notify,
    calls: AtomicUsize,
    dropped: AtomicBool,
    inputs: Mutex<Vec<String>>,
}
impl Probe {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            entered: Notify::new(),
            release: Notify::new(),
            calls: AtomicUsize::new(0),
            dropped: AtomicBool::new(false),
            inputs: Mutex::new(vec![]),
        })
    }
}
struct CallDrop(Arc<Probe>);
impl Drop for CallDrop {
    fn drop(&mut self) {
        self.0.dropped.store(true, Ordering::SeqCst);
    }
}
struct Provider {
    name: String,
    dims: usize,
    probe: Arc<Probe>,
    gated: bool,
    fail_build: bool,
}
struct Service {
    dims: usize,
    probe: Arc<Probe>,
    gated: bool,
}
#[async_trait]
impl EmbedderProvider for Provider {
    fn name(&self) -> &str {
        &self.name
    }
    fn dimensions(&self) -> usize {
        self.dims
    }
    async fn build(&self) -> RuntimeResult<Arc<dyn EmbeddingService>> {
        if self.fail_build {
            return Err(RuntimeError::InvalidInput(
                "warm provider refused fixture".into(),
            ));
        }
        Ok(Arc::new(Service {
            dims: self.dims,
            probe: self.probe.clone(),
            gated: self.gated,
        }))
    }
}
#[async_trait]
impl EmbeddingService for Service {
    async fn embed(
        &self,
        texts: &[String],
        _: EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        let _drop = CallDrop(self.probe.clone());
        self.probe.calls.fetch_add(1, Ordering::SeqCst);
        self.probe.inputs.lock().unwrap().extend_from_slice(texts);
        self.probe.entered.notify_one();
        if self.gated {
            self.probe.release.notified().await;
        }
        Ok(texts.iter().map(|_| vec![1.0; self.dims]).collect())
    }
    async fn embed_query(
        &self,
        texts: &[String],
        model: EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        self.embed(texts, model).await
    }
    fn supports_model(&self, _: EmbeddingModel) -> bool {
        true
    }
    fn name(&self) -> &'static str {
        "warm-admission-service"
    }
}
fn configured(gated: bool, fail_build: bool) -> (KhiveRuntime, Arc<Probe>) {
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        engines: Some(vec![EngineConfig {
            name: MODEL.into(),
            weight: 1.0,
            dims: Some(2),
        }]),
        ..RuntimeConfig::no_embeddings()
    })
    .unwrap();
    let probe = Probe::new();
    runtime
        .try_register_embedder(Provider {
            name: MODEL.into(),
            dims: 2,
            probe: probe.clone(),
            gated,
            fail_build,
        })
        .unwrap();
    runtime.finalize_embedding_engines().unwrap();
    (runtime, probe)
}
async fn entered(wall: &WallClock, probe: &Probe) {
    wall.wait(async {
        while probe.calls.load(Ordering::SeqCst) == 0 {
            probe.entered.notified().await;
        }
    })
    .await;
}
async fn phases(wall: &WallClock, runtime: &KhiveRuntime) -> Vec<Event> {
    let token = runtime.authorize(Namespace::local()).unwrap();
    wall.wait(runtime.events(&token).unwrap().query_events(
        EventFilter::default(),
        PageRequest {
            limit: 100,
            offset: 0,
        },
    ))
    .await
    .unwrap()
    .items
    .into_iter()
    .filter(|event| event.verb == LABEL)
    .collect()
}
async fn terminal(wall: &WallClock, runtime: &KhiveRuntime, kind: EventKind) -> Vec<Event> {
    let events = wall
        .wait(async {
            loop {
                let events = phases(wall, runtime).await;
                if events.iter().any(|event| {
                    matches!(
                        event.kind,
                        EventKind::PhaseCompleted | EventKind::PhaseCancelled
                    )
                }) {
                    break events;
                }
                tokio::task::yield_now().await;
            }
        })
        .await;
    assert_eq!(events.len(), 2, "one actual start and one actual terminal");
    assert_eq!(
        events
            .iter()
            .filter(|event| event.kind == EventKind::PhaseStarted)
            .count(),
        1
    );
    assert_eq!(events.iter().filter(|event| event.kind == kind).count(), 1);
    for event in &events {
        assert_eq!(event.payload["work_class"], "warm");
        assert_eq!(event.payload["phase"], PHASE);
        if event.kind != EventKind::PhaseStarted {
            assert!(event.payload["wall_us"]
                .as_i64()
                .is_some_and(|value| value >= 0));
            if let Some(cpu) = event
                .payload
                .get("cpu_us")
                .and_then(serde_json::Value::as_i64)
            {
                assert!(cpu >= 0);
            }
        }
    }
    events
}
async fn stop(wall: &WallClock, runtime: &KhiveRuntime) {
    let control = runtime.background_domain().control();
    let report = wall
        .wait(control.shutdown(Instant::now() + WAIT))
        .await
        .unwrap();
    assert!(
        report.workers_exited && report.coordinator_joined,
        "real isolated workers must exit"
    );
    assert_eq!(report.outstanding_jobs, 0);
}

#[tokio::test]
#[serial_test::serial(background_tasks)]
async fn configured_knowledge_warm_queues_without_blocking_and_explicitly_detaches() {
    let (runtime, probe) = configured(true, false);
    let _close = CloseOnDrop(runtime.background_domain().control());
    let wall = WallClock::new();
    let domain = runtime.background_domain();
    let tracking = TrackedJob::register("knowledge_warm_fixture_blocker");
    let blocker = wall
        .wait(domain.admit(WorkClass::Warm, &tracking))
        .await
        .unwrap();
    drop(tracking);
    let pack = KnowledgePack::new_with_index_role(runtime.clone(), false);
    wall.wait(pack.warm()).await;
    assert_eq!(probe.calls.load(Ordering::SeqCst), 0);
    assert!(
        phases(&wall, &runtime).await.is_empty(),
        "no phase before Warm admission"
    );
    drop(blocker);
    entered(&wall, &probe).await;
    drop(pack);
    probe.release.notify_one();
    terminal(&wall, &runtime, EventKind::PhaseCompleted).await;
    assert_eq!(*probe.inputs.lock().unwrap(), ["__khive_knowledge_warm__"]);
    stop(&wall, &runtime).await;
}

#[tokio::test]
#[serial_test::serial(background_tasks)]
async fn configured_knowledge_warm_domain_close_finalizes_once_after_detach() {
    let (runtime, probe) = configured(true, false);
    let _close = CloseOnDrop(runtime.background_domain().control());
    let wall = WallClock::new();
    let pack = KnowledgePack::new_with_index_role(runtime.clone(), false);
    wall.wait(pack.warm()).await;
    entered(&wall, &probe).await;
    let control = runtime.background_domain().control();
    let deadline = Instant::now() + WAIT;
    control.close(deadline);
    control.close(deadline);
    terminal(&wall, &runtime, EventKind::PhaseCancelled).await;
    assert!(probe.dropped.load(Ordering::SeqCst));
    stop(&wall, &runtime).await;
    assert_eq!(phases(&wall, &runtime).await.len(), 2);
}

#[tokio::test]
#[serial_test::serial(background_tasks)]
async fn configured_knowledge_warm_real_factory_error_keeps_completed_terminal() {
    let (runtime, probe) = configured(false, true);
    let _close = CloseOnDrop(runtime.background_domain().control());
    let wall = WallClock::new();
    wall.wait(KnowledgePack::new_with_index_role(runtime.clone(), false).warm())
        .await;
    terminal(&wall, &runtime, EventKind::PhaseCompleted).await;
    assert_eq!(probe.calls.load(Ordering::SeqCst), 0);
    stop(&wall, &runtime).await;
}

#[tokio::test]
#[serial_test::serial(background_tasks)]
async fn read_only_configured_knowledge_warm_never_enters_the_writer_plane() {
    let dir = tempfile::tempdir().unwrap();
    let config = RuntimeConfig {
        db_path: Some(dir.path().join("read-only-configured-warm.db")),
        engines: Some(vec![EngineConfig {
            name: MODEL.into(),
            weight: 1.0,
            dims: Some(2),
        }]),
        ..RuntimeConfig::no_embeddings()
    };
    drop(KhiveRuntime::new(config.clone()).unwrap());
    #[cfg(unix)]
    khive_storage::test_support::freeze_snapshot_sidecars(config.db_path.as_ref().unwrap());
    let runtime = KhiveRuntime::new_readonly(config).unwrap();
    let _close = CloseOnDrop(runtime.background_domain().control());
    let wall = WallClock::new();
    let probe = Probe::new();
    runtime
        .try_register_embedder(Provider {
            name: MODEL.into(),
            dims: 2,
            probe: probe.clone(),
            gated: true,
            fail_build: false,
        })
        .unwrap();
    runtime.finalize_embedding_engines().unwrap();
    let pack = KnowledgePack::new_with_index_role(runtime.clone(), false);
    let before = runtime.backend().pool().writer_acquisition_snapshot();
    wall.wait(pack.warm()).await;
    entered(&wall, &probe).await;
    probe.release.notify_one();
    wall.wait(async {
        while !probe.dropped.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await;
    stop(&wall, &runtime).await;
    assert_eq!(
        runtime.backend().pool().writer_acquisition_snapshot(),
        before,
        "actual configured warm phase emission must preserve read-only early return"
    );
    assert_eq!(*probe.inputs.lock().unwrap(), ["__khive_knowledge_warm__"]);
}
