//! One real registry-backed daemon recall route at 16 MiB, then 2 MiB.
//! The seeded hit is lexical; deterministic query embedding is exercised, but
//! this fixture has no populated-vector or ANN coverage. Lower-stack control
//! calibration remains owner-native. Watchdog expiry is never semantic credit.

#![cfg(unix)]

use crate::receipt_credentials;
use anyhow::{anyhow, ensure, Context, Result};
use async_trait::async_trait;
use khive_mcp::server::KhiveMcpServer;
use khive_pack_kg::KgPack;
use khive_pack_memory::MemoryPack;
use khive_runtime::daemon::{
    read_frame, run_daemon, write_frame, DaemonDispatch, DaemonDispatchError, DaemonRequestFrame,
    DaemonResponseFrame, PROTOCOL_VERSION,
};
use khive_runtime::{
    curation::note_fts_document, EmbedderProvider, KhiveRuntime, Namespace, RequestIdentity,
    RuntimeConfig, RuntimeError, VerbRegistryBuilder,
};
use khive_storage::Note;
use lattice_embed::{EmbedError, EmbeddingModel, EmbeddingService};
use serde_json::{json, Value};
use std::{
    path::Path,
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc, Arc,
    },
    time::Duration,
};
use tokio::{
    net::UnixStream,
    time::{sleep, timeout},
};

const BASELINE_STACK: usize = 16 * 1024 * 1024;
const TARGET_STACK: usize = 2 * 1024 * 1024;
const STARTUP_LIMIT: Duration = Duration::from_secs(60);
const REQUEST_LIMIT: Duration = Duration::from_secs(30);
const CLEANUP_LIMIT: Duration = Duration::from_secs(15);
const WALL_LIMIT: Duration = Duration::from_secs(180);
const ACTOR: &str = "test:daemon-recall-stack";
const CONFIG_ID: &str = "daemon-recall-stack-fixture";
const CONTENT: &str = "Deliberate stack guard memory marker for deterministic recall";
const RECALL_OPS: &str =
    "memory.recall(query=\"Deliberate stack guard memory marker for deterministic recall\", limit=10)";

/// Independent of the exercised Tokio workers, including their timer driver.
/// Only constructed after the isolation helper has entered the controlled child.
struct WallWatchdog {
    cancel: Option<mpsc::Sender<()>>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl WallWatchdog {
    fn start() -> Self {
        let (cancel, receive) = mpsc::channel();
        let worker = std::thread::Builder::new()
            .name("daemon-recall-wall-watchdog".to_owned())
            .stack_size(2 * 1024 * 1024)
            .spawn(move || {
                if matches!(
                    receive.recv_timeout(WALL_LIMIT),
                    Err(mpsc::RecvTimeoutError::Timeout)
                ) {
                    eprintln!("DAEMON_RECALL_STACK_WALL_WATCHDOG_TIMEOUT");
                    // A starved runtime cannot drive an async timeout or cleanup.
                    // Abort only this isolated child; the timeout is not a kill.
                    std::process::abort();
                }
            })
            .expect("start independent child wall watchdog");
        Self {
            cancel: Some(cancel),
            worker: Some(worker),
        }
    }
}

impl Drop for WallWatchdog {
    fn drop(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            let _ = cancel.send(());
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

struct DeterministicEmbeddings {
    dims: usize,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl EmbeddingService for DeterministicEmbeddings {
    async fn embed(
        &self,
        texts: &[String],
        _model: EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        self.calls.fetch_add(texts.len(), Ordering::SeqCst);
        Ok(texts.iter().map(|_| vec![1.0; self.dims]).collect())
    }
    fn supports_model(&self, _model: EmbeddingModel) -> bool {
        true
    }
    fn name(&self) -> &'static str {
        "daemon-recall-stack-fixture"
    }
}

struct DeterministicProvider {
    model_name: String,
    dims: usize,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl EmbedderProvider for DeterministicProvider {
    fn name(&self) -> &str {
        &self.model_name
    }
    fn dimensions(&self) -> usize {
        self.dims
    }
    async fn build(&self) -> Result<Arc<dyn EmbeddingService>, RuntimeError> {
        Ok(Arc::new(DeterministicEmbeddings {
            dims: self.dims,
            calls: Arc::clone(&self.calls),
        }))
    }
}

/// The real MCP dispatcher handles recall; this wrapper only marks its entry.
#[derive(Clone)]
struct MarkedDispatch {
    inner: KhiveMcpServer,
}

#[async_trait]
impl DaemonDispatch for MarkedDispatch {
    fn plan(&self, ops: &str) -> String {
        self.inner.plan(ops)
    }
    async fn dispatch(
        &self,
        ops: String,
        presentation: Option<String>,
        presentation_per_op: Option<Vec<Option<String>>>,
        format: Option<String>,
        format_per_op: Option<Vec<Option<String>>>,
        from_wire: bool,
        identity: Option<RequestIdentity>,
    ) -> Result<String, String> {
        if ops == RECALL_OPS {
            eprintln!("DAEMON_RECALL_STACK_RECALL_ENTERED");
        }
        self.inner
            .dispatch(
                ops,
                presentation,
                presentation_per_op,
                format,
                format_per_op,
                from_wire,
                identity,
            )
            .await
    }
    async fn dispatch_with_error_detail(
        &self,
        ops: String,
        presentation: Option<String>,
        presentation_per_op: Option<Vec<Option<String>>>,
        format: Option<String>,
        format_per_op: Option<Vec<Option<String>>>,
        from_wire: bool,
        identity: Option<RequestIdentity>,
    ) -> Result<String, DaemonDispatchError> {
        if ops == RECALL_OPS {
            eprintln!("DAEMON_RECALL_STACK_RECALL_ENTERED");
        }
        self.inner
            .dispatch_with_error_detail(
                ops,
                presentation,
                presentation_per_op,
                format,
                format_per_op,
                from_wire,
                identity,
            )
            .await
    }
    async fn warm_all(&self) {
        self.inner.warm_all().await;
    }
    fn namespace(&self) -> &str {
        self.inner.namespace()
    }
    fn config_id(&self) -> &str {
        self.inner.config_id()
    }
}

fn frame(ops: &str, request_id: u64) -> DaemonRequestFrame {
    DaemonRequestFrame {
        ops: ops.to_owned(),
        namespace: "local".to_owned(),
        actor_id: Some(ACTOR.to_owned()),
        config_id: CONFIG_ID.to_owned(),
        protocol_version: PROTOCOL_VERSION,
        presentation: Some("verbose".to_owned()),
        format: Some("json".to_owned()),
        from_wire: true,
        request_id: Some(request_id),
        ..Default::default()
    }
}

async fn exchange(socket: &Path, frame: &DaemonRequestFrame) -> Result<DaemonResponseFrame> {
    timeout(REQUEST_LIMIT, async {
        let mut stream = UnixStream::connect(socket)
            .await
            .context("connect owned daemon")?;
        write_frame(&mut stream, &serde_json::to_vec(frame)?)
            .await
            .context("write daemon frame")?;
        let raw = read_frame(&mut stream).await.context("read daemon frame")?;
        let response: DaemonResponseFrame = serde_json::from_slice(&raw)?;
        ensure!(response.ok, "daemon refusal: {response:?}");
        ensure!(
            !response.namespace_mismatch && !response.config_mismatch && !response.version_mismatch,
            "identity mismatch: {response:?}"
        );
        ensure!(
            response.served_config_id.as_deref() == Some(CONFIG_ID)
                && response.daemon_protocol_version == PROTOCOL_VERSION
                && response.request_id == frame.request_id,
            "response does not match the owned request: {response:?}"
        );
        Ok(response)
    })
    .await
    .context("daemon exchange timed out")?
}

async fn probe(socket: &Path, request_id: u64) -> Result<()> {
    let mut request = frame("", request_id);
    request.probe_only = true;
    let response = exchange(socket, &request).await?;
    ensure!(
        response.result.is_none() && response.metrics.is_none(),
        "probe did not take the identity branch: {response:?}"
    );
    Ok(())
}

async fn stop_daemon(daemon: &mut tokio::task::JoinHandle<Result<()>>) -> Result<()> {
    if !daemon.is_finished() {
        // SAFETY: this is the sole test in its isolated child, with a signal
        // listener installed before launch and retained through cleanup.
        let rc = unsafe { libc::kill(libc::getpid(), libc::SIGTERM) };
        ensure!(
            rc == 0,
            "signal isolated daemon: {}",
            std::io::Error::last_os_error()
        );
    }
    match timeout(CLEANUP_LIMIT, &mut *daemon).await {
        Ok(joined) => joined.context("daemon task panicked")?,
        Err(_) => {
            daemon.abort();
            let settled = timeout(CLEANUP_LIMIT, daemon).await;
            Err(anyhow!(
                "daemon cleanup timed out; abort settlement={settled:?}"
            ))
        }
    }
}

async fn exercise() -> Result<()> {
    let root = tempfile::Builder::new()
        .prefix("recall-stack-")
        .tempdir_in("/tmp")
        .context("short private daemon directory")?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700))?;
    let socket = root.path().join("s");
    std::env::set_var("KHIVE_SOCKET", &socket);
    std::env::set_var("KHIVE_PID", root.path().join("p"));
    std::env::set_var("KHIVE_LOCK", root.path().join("l"));
    std::env::set_var("KHIVE_RECOVERER_LOCK", root.path().join("r"));
    std::env::set_var("KHIVE_EVENTS_SPLIT", "0");
    std::env::set_var("KHIVE_DRAIN_TIMEOUT_SECS", "2");

    let model = EmbeddingModel::AllMiniLmL6V2;
    let embed_calls = Arc::new(AtomicUsize::new(0));
    let runtime = receipt_credentials::with_receipt_credentials(
        KhiveRuntime::new(RuntimeConfig {
            db_path: None,
            actor_id: Some(ACTOR.to_owned()),
            default_namespace: Namespace::local(),
            brain_profile: None,
            events_split: None,
            mounts: vec![],
            credentials: vec![],
            embedding_model: Some(model),
            additional_embedding_models: vec![],
            packs: vec!["kg".into(), "memory".into()],
            ..RuntimeConfig::no_embeddings()
        })
        .context("in-memory recall runtime")?,
    );
    ensure!(
        runtime.backend_data_dir().is_none() && runtime.backend_ann_root().is_none(),
        "fixture selected a persistent backend"
    );
    runtime
        .try_register_embedder(DeterministicProvider {
            model_name: model.to_string(),
            dims: model.dimensions(),
            calls: Arc::clone(&embed_calls),
        })
        .context("register deterministic query embedder")?;
    let token = runtime.authorize(Namespace::local())?;
    let mut note = Note::new("local", "memory", CONTENT);
    note.salience = Some(0.5);
    note.decay_factor = Some(0.005);
    note.properties = Some(json!({"memory_type": "semantic"}));
    let wanted_id = note.id.to_string();
    runtime.notes(&token)?.upsert_note(note.clone()).await?;
    runtime
        .text_for_notes(&token)?
        .upsert_document(note_fts_document(&note))
        .await?;
    ensure!(
        runtime.notes(&token)?.get_note(note.id).await?.is_some(),
        "seed did not commit"
    );

    let mut builder = VerbRegistryBuilder::new();
    builder.with_actor_id(Some(ACTOR.to_owned()));
    builder.with_default_namespace("local");
    builder.register(KgPack::new(runtime.clone()));
    builder.register(MemoryPack::new(runtime.clone()));
    let registry = builder.build().context("real memory registry")?;
    registry
        .apply_schema_plans_with_map(&Default::default(), runtime.backend())
        .context("memory pack schemas")?;
    let dispatcher = MarkedDispatch {
        inner: KhiveMcpServer::from_registry_with_meta(registry, "local", CONFIG_ID),
    };
    let _signal_guard = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("install isolated child signal guard")?;
    let mut daemon = tokio::spawn(run_daemon(dispatcher));
    let body: Result<()> = async {
        timeout(STARTUP_LIMIT, async {
            loop {
                ensure!(!daemon.is_finished(), "daemon exited before readiness");
                if probe(&socket, 1).await.is_ok() {
                    break;
                }
                sleep(Duration::from_millis(20)).await;
            }
            Ok::<_, anyhow::Error>(())
        })
        .await
        .context("daemon readiness timed out")??;
        eprintln!("DAEMON_RECALL_STACK_SETUP_READY");
        let embeddings_before_recall = embed_calls.load(Ordering::SeqCst);
        let response = exchange(&socket, &frame(RECALL_OPS, 2)).await?;
        let envelope: Value =
            serde_json::from_str(response.result.as_deref().context("recall has no result")?)?;
        ensure!(
            envelope["status"] == "success"
                && envelope["results"]
                    .as_array()
                    .is_some_and(|rows| rows.len() == 1)
                && envelope["results"][0]["ok"] == true,
            "recall request did not succeed: {envelope}"
        );
        let result = &envelope["results"][0]["result"];
        let hits = result
            .as_array()
            .or_else(|| result["results"].as_array())
            .context("recall result has no hit array")?;
        ensure!(
            hits.iter()
                .any(|hit| hit["full_id"].as_str() == Some(wanted_id.as_str())),
            "recall missed seeded UUID {wanted_id}: {envelope}"
        );
        ensure!(
            embed_calls.load(Ordering::SeqCst) > embeddings_before_recall,
            "successful recall did not call the deterministic query embedder"
        );
        ensure!(!daemon.is_finished(), "daemon exited after recall");
        probe(&socket, 3)
            .await
            .context("post-recall liveness probe")?;
        eprintln!("DAEMON_RECALL_STACK_RECALL_SUCCEEDED");
        Ok(())
    }
    .await;
    let cleanup = stop_daemon(&mut daemon).await;
    match (body, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(body), Ok(())) => Err(body),
        (Ok(()), Err(cleanup)) => Err(cleanup),
        (Err(body), Err(cleanup)) => Err(anyhow!("recall: {body:#}; cleanup: {cleanup:#}")),
    }
}

#[test]
fn daemon_memory_recall_completes_on_two_mib_worker_stack() {
    if crate::test_process::run_in_child_with_stack_size(BASELINE_STACK) {
        assert!(crate::test_process::run_in_child_with_stack_size(
            TARGET_STACK
        ));
        return;
    }
    let stack = std::env::var("RUST_MIN_STACK")
        .expect("isolated arm must declare RUST_MIN_STACK")
        .parse::<usize>()
        .expect("numeric isolated arm stack");
    assert!(
        stack == BASELINE_STACK || stack == TARGET_STACK,
        "unexpected isolated arm stack {stack}"
    );
    eprintln!("DAEMON_RECALL_STACK_ARM bytes={stack}");
    let watchdog = WallWatchdog::start();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_stack_size(stack)
        .enable_all()
        .build()
        .expect("isolated arm Tokio runtime");
    let result = runtime.block_on(exercise());
    runtime.shutdown_timeout(CLEANUP_LIMIT);
    drop(watchdog);
    if let Err(error) = result {
        panic!("daemon recall stack arm bytes={stack}: {error:#}");
    }
}
