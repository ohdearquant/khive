use super::KgPack;
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
const LABEL: &str = "kg.embedder_warm";
const PHASE: &str = "kg_embedder_warm";

// One real-clock alarm per fixture. Paused ambient Tokio time cannot
// auto-advance this deadline. Dropping the sender disarms recv_timeout;
// the detached watchdog retains no runtime, provider or domain ownership.
#[derive(Clone)]
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

async fn pending<F: std::future::Future>(mut future: std::pin::Pin<&mut F>) {
    std::future::poll_fn(|cx| {
        assert!(
            future.as_mut().poll(cx).is_pending(),
            "KG warm must retain its inline barrier"
        );
        std::task::Poll::Ready(())
    })
    .await;
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
async fn kg_warm_admission_precedes_start_and_preserves_inline_barrier() {
    let (runtime, probe) = configured(true, false);
    let _close = CloseOnDrop(runtime.background_domain().control());
    let wall = WallClock::new();
    let domain = runtime.background_domain();
    let tracking = TrackedJob::register("kg_warm_fixture_blocker");
    let blocker = wall
        .wait(domain.admit(WorkClass::Warm, &tracking))
        .await
        .unwrap();
    drop(tracking);
    let pack = KgPack::new(runtime.clone());
    let mut warm = Box::pin(pack.warm());
    wall.wait(pending(warm.as_mut())).await;
    assert_eq!(probe.calls.load(Ordering::SeqCst), 0);
    assert!(
        phases(&wall, &runtime).await.is_empty(),
        "queued work must not emit a start"
    );
    drop(blocker);
    wall.wait(async {
        tokio::select! {
            _ = warm.as_mut() => panic!("KG warm returned before its gated service completed"),
            _ = entered(&wall, &probe) => {},
        }
    })
    .await;
    wall.wait(pending(warm.as_mut())).await;
    let started = phases(&wall, &runtime).await;
    assert_eq!(started.len(), 1);
    assert_eq!(started[0].kind, EventKind::PhaseStarted);
    probe.release.notify_one();
    wall.wait(warm).await;
    terminal(&wall, &runtime, EventKind::PhaseCompleted).await;
    assert_eq!(*probe.inputs.lock().unwrap(), ["khive warmup"]);
    stop(&wall, &runtime).await;
}

#[tokio::test]
#[serial_test::serial(background_tasks)]
async fn kg_warm_caller_cancellation_finalizes_one_cancelled_phase() {
    let (runtime, probe) = configured(true, false);
    let _close = CloseOnDrop(runtime.background_domain().control());
    let wall = WallClock::new();
    let pack = KgPack::new(runtime.clone());
    let task_wall = wall.clone();
    let task = tokio::spawn(async move {
        task_wall.wait(pack.warm()).await;
    });
    entered(&wall, &probe).await;
    task.abort();
    task.abort();
    assert!(wall.wait(task).await.unwrap_err().is_cancelled());
    terminal(&wall, &runtime, EventKind::PhaseCancelled).await;
    assert!(probe.dropped.load(Ordering::SeqCst));
    assert!(
        !runtime.background_domain().control().is_closed(),
        "local caller cancellation must not close the domain"
    );
    let deadline = Instant::now() + WAIT;
    runtime.background_domain().control().close(deadline);
    runtime.background_domain().control().close(deadline);
    stop(&wall, &runtime).await;
    assert_eq!(
        phases(&wall, &runtime).await.len(),
        2,
        "repeated close must not retry the terminal"
    );
}

#[tokio::test]
#[serial_test::serial(background_tasks)]
async fn kg_warm_non_benign_provider_error_keeps_completed_terminal() {
    let (runtime, probe) = configured(false, true);
    let _close = CloseOnDrop(runtime.background_domain().control());
    let wall = WallClock::new();
    wall.wait(KgPack::new(runtime.clone()).warm()).await;
    terminal(&wall, &runtime, EventKind::PhaseCompleted).await;
    assert_eq!(
        probe.calls.load(Ordering::SeqCst),
        0,
        "the real provider factory refuses before service entry"
    );
    stop(&wall, &runtime).await;
}
