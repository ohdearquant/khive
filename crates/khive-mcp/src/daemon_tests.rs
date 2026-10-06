use std::sync::Arc;

use super::test_harness::{
    clear_daemon_env, connect_when_ready, exchange, HarnessDispatch, InProcessDaemonHandle,
    InProcessDaemonLauncher, RecoveryTestGuard,
};
use super::*;
use khive_runtime::daemon::run_daemon;
use serial_test::serial;

use khive_runtime::engine_config::ActorConfig;
use khive_runtime::{
    runtime_config_from_khive_config, GitWriteEntryConfig, GitWriteSectionConfig, KhiveConfig,
    KhiveRuntime, Namespace, RuntimeConfig,
};

#[tokio::test]
async fn lifecycle_probe_uses_own_deadline_when_request_deadline_has_expired() {
    khive_storage::scope_request_read_deadline(std::time::Duration::ZERO, async {
        tokio::task::yield_now().await;
        let now = tokio::time::Instant::now();
        let probe_deadline = now + std::time::Duration::from_millis(500);
        assert_eq!(
            socket_exchange_deadline(true, "", Some(probe_deadline)),
            probe_deadline
        );
        assert!(socket_exchange_deadline(false, "stats()", None) <= now);
    })
    .await;
}

const PRIMARY_MODEL: lattice_embed::EmbeddingModel = lattice_embed::EmbeddingModel::AllMiniLmL6V2;
const EXTRA_MODEL: lattice_embed::EmbeddingModel = lattice_embed::EmbeddingModel::BgeSmallEnV15;
const SECOND_EXTRA_MODEL: lattice_embed::EmbeddingModel =
    lattice_embed::EmbeddingModel::BgeBaseEnV15;

struct FixedModelService {
    dimensions: usize,
}

#[async_trait::async_trait]
impl lattice_embed::EmbeddingService for FixedModelService {
    async fn embed(
        &self,
        texts: &[String],
        _model: lattice_embed::EmbeddingModel,
    ) -> Result<Vec<Vec<f32>>, lattice_embed::EmbedError> {
        Ok(texts.iter().map(|_| vec![0.25; self.dimensions]).collect())
    }

    fn supports_model(&self, _model: lattice_embed::EmbeddingModel) -> bool {
        true
    }

    fn name(&self) -> &'static str {
        "configured-test-embedder"
    }
}

struct FixedModelProvider {
    name: String,
    dimensions: usize,
}

#[async_trait::async_trait]
impl khive_runtime::EmbedderProvider for FixedModelProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn dimensions(&self) -> usize {
        self.dimensions
    }

    async fn build(
        &self,
    ) -> khive_runtime::RuntimeResult<Arc<dyn lattice_embed::EmbeddingService>> {
        Ok(Arc::new(FixedModelService {
            dimensions: self.dimensions,
        }))
    }
}

fn embedding_runtime_config(extras: &[lattice_embed::EmbeddingModel]) -> RuntimeConfig {
    let mut config = memory_runtime_config();
    config.default_namespace = Namespace::parse("test").unwrap();
    config.packs = vec!["kg".to_string(), "memory".to_string()];
    config.embedding_model = Some(PRIMARY_MODEL);
    config.additional_embedding_models = extras.to_vec();
    config
}

fn install_test_embedders(runtime: &KhiveRuntime, config: &RuntimeConfig) {
    let models = config
        .embedding_model
        .into_iter()
        .chain(config.additional_embedding_models.iter().copied());
    for model in models {
        runtime.register_embedder(FixedModelProvider {
            name: model.to_string(),
            dimensions: model.dimensions(),
        });
    }
}

async fn start_embedding_daemon(
    extras: &[lattice_embed::EmbeddingModel],
) -> (
    tempfile::TempDir,
    tokio::task::JoinHandle<()>,
    KhiveRuntime,
    crate::server::KhiveMcpServer,
    std::path::PathBuf,
) {
    clear_daemon_env();
    let config = embedding_runtime_config(extras);
    let runtime = KhiveRuntime::new(config.clone()).expect("embedding runtime");
    install_test_embedders(&runtime, &config);
    let inspect_runtime = runtime.clone();
    let server = crate::server::KhiveMcpServer::new(runtime).expect("embedding server");
    let dir = tempfile::tempdir().expect("daemon tempdir");
    let socket = dir.path().join("khived.sock");
    std::env::set_var("KHIVE_SOCKET", &socket);
    std::env::set_var("KHIVE_PID", dir.path().join("khived.pid"));
    std::env::set_var("KHIVE_LOCK", dir.path().join("khived.recovery.lock"));
    std::env::set_var(
        "KHIVE_RECOVERER_LOCK",
        dir.path().join("khived.recoverer.lock"),
    );
    std::env::remove_var("KHIVE_NO_DAEMON");
    let daemon_server = server.clone();
    let daemon = tokio::spawn(async move {
        let _ = run_daemon(daemon_server).await;
    });
    let ready = connect_when_ready(&socket).await;
    drop(ready);
    (dir, daemon, inspect_runtime, server, socket)
}

fn embedding_request_frame(ops: &str, config_id: String) -> DaemonRequestFrame {
    DaemonRequestFrame {
        plan: false,
        ops: ops.to_owned(),
        presentation: Some("verbose".to_string()),
        presentation_per_op: None,
        namespace: "test".to_string(),
        actor_id: None,
        process_ref: None,
        visible_namespaces: Vec::new(),
        config_id,
        protocol_version: PROTOCOL_VERSION,
        probe_only: false,
        metrics_only: false,
        format: None,
        format_per_op: None,
        from_wire: true,
        request_id: None,
    }
}

/// The namespace a successful daemon `create` wrote into, read from its
/// response, so vector counts look where the write landed.
fn created_namespace(response: &DaemonResponseFrame) -> String {
    assert!(response.ok, "create failed: {:?}", response.error);
    let result: serde_json::Value =
        serde_json::from_str(response.result.as_deref().expect("create response"))
            .expect("JSON response");
    assert_eq!(result["results"][0]["ok"], true, "{result}");
    result["results"][0]["result"]["namespace"]
        .as_str()
        .expect("created note namespace")
        .to_owned()
}

async fn vector_count(runtime: &KhiveRuntime, namespace: &str, model: &str) -> u64 {
    let token = runtime
        .authorize(Namespace::parse(namespace).unwrap())
        .expect("note namespace token");
    runtime
        .vectors_for_model(&token, model)
        .expect("model vector store")
        .count()
        .await
        .expect("vector count")
}

async fn stop_embedding_daemon(daemon: tokio::task::JoinHandle<()>) {
    daemon.abort();
    let _ = daemon.await;
    clear_daemon_env();
}

#[tokio::test]
#[serial]
#[serial_test::serial(config_ledger)]
async fn daemon_omits_vectors_for_undeclared_extra_models() {
    let (_dir, daemon, runtime, server, socket) = start_embedding_daemon(&[EXTRA_MODEL]).await;
    let client_config = embedding_runtime_config(&[]);
    let client_id = crate::server::compute_config_id(&client_config, None);
    let response = exchange(
        &socket,
        &embedding_request_frame(
            "create(kind=\"observation\", content=\"client model scope\")",
            client_id,
        ),
    )
    .await;
    let namespace = created_namespace(&response);
    assert_eq!(
        vector_count(&runtime, &namespace, &PRIMARY_MODEL.to_string()).await,
        1,
        "the client's primary model must receive the note vector"
    );
    assert_eq!(
        vector_count(&runtime, &namespace, &EXTRA_MODEL.to_string()).await,
        0,
        "a daemon-only extra must not receive the note vector"
    );
    assert_eq!(
        server.config_id(),
        crate::server::compute_config_id(&embedding_runtime_config(&[EXTRA_MODEL]), None)
    );
    stop_embedding_daemon(daemon).await;
}

#[tokio::test]
#[serial]
#[serial_test::serial(config_ledger)]
async fn daemon_uses_only_client_declared_extra_models() {
    let (_dir, daemon, runtime, _server, socket) =
        start_embedding_daemon(&[EXTRA_MODEL, SECOND_EXTRA_MODEL]).await;
    let client_config = embedding_runtime_config(&[EXTRA_MODEL]);
    let client_id = crate::server::compute_config_id(&client_config, None);
    let response = exchange(
        &socket,
        &embedding_request_frame(
            "create(kind=\"observation\", content=\"declared extra scope\")",
            client_id,
        ),
    )
    .await;
    let namespace = created_namespace(&response);
    assert_eq!(
        vector_count(&runtime, &namespace, &PRIMARY_MODEL.to_string()).await,
        1
    );
    assert_eq!(
        vector_count(&runtime, &namespace, &EXTRA_MODEL.to_string()).await,
        1
    );
    assert_eq!(
        vector_count(&runtime, &namespace, &SECOND_EXTRA_MODEL.to_string()).await,
        0,
        "the daemon's second extra is outside the client's declaration"
    );

    stop_embedding_daemon(daemon).await;
}

#[tokio::test]
#[serial]
#[serial_test::serial(config_ledger)]
async fn equal_configuration_keeps_the_daemons_full_embedder_set() {
    let (_dir, daemon, runtime, server, socket) =
        start_embedding_daemon(&[EXTRA_MODEL, SECOND_EXTRA_MODEL]).await;
    let equal_id = server.config_id().to_owned();
    let response = exchange(
        &socket,
        &embedding_request_frame(
            "create(kind=\"observation\", content=\"equal configuration scope\")",
            equal_id,
        ),
    )
    .await;
    let namespace = created_namespace(&response);
    assert_eq!(
        vector_count(&runtime, &namespace, &PRIMARY_MODEL.to_string()).await,
        1
    );
    assert_eq!(
        vector_count(&runtime, &namespace, &EXTRA_MODEL.to_string()).await,
        1
    );
    assert_eq!(
        vector_count(&runtime, &namespace, &SECOND_EXTRA_MODEL.to_string()).await,
        1
    );
    stop_embedding_daemon(daemon).await;
}

#[tokio::test]
#[serial]
#[serial_test::serial(config_ledger)]
async fn daemon_treats_an_undeclared_explicit_model_as_unknown() {
    let (_dir, daemon, runtime, _server, socket) = start_embedding_daemon(&[EXTRA_MODEL]).await;
    let client_id = crate::server::compute_config_id(&embedding_runtime_config(&[]), None);
    let response = exchange(
        &socket,
        &embedding_request_frame(
            "memory.remember(content=\"outside the client model set\", memory_type=\"semantic\", embedding_model=\"bge-small-en-v1.5\")",
            client_id.clone(),
        ),
    )
    .await;
    assert!(response.ok, "MCP dispatch failed: {:?}", response.error);
    let result: serde_json::Value =
        serde_json::from_str(response.result.as_deref().expect("remember response"))
            .expect("JSON response");
    let entry = &result["results"][0];
    assert_eq!(entry["ok"], false, "{result}");
    assert!(
        entry
            .to_string()
            .contains("unknown embedding model: bge-small-en-v1.5"),
        "the explicit model must have the in-process unknown-model result: {entry}"
    );
    // A declared write through the same client lands one primary vector, so
    // the counts below read the namespace this client's writes use.
    let anchor = exchange(
        &socket,
        &embedding_request_frame(
            "create(kind=\"observation\", content=\"declared model anchor\")",
            client_id,
        ),
    )
    .await;
    let namespace = created_namespace(&anchor);
    assert_eq!(
        vector_count(&runtime, &namespace, &PRIMARY_MODEL.to_string()).await,
        1
    );
    assert_eq!(
        vector_count(&runtime, &namespace, &EXTRA_MODEL.to_string()).await,
        0
    );
    stop_embedding_daemon(daemon).await;
}

fn memory_runtime_config() -> RuntimeConfig {
    KhiveRuntime::memory()
        .expect("memory runtime")
        .config()
        .clone()
}

#[test]
fn missing_pid_parent_is_treated_as_no_incumbent() {
    let workspace = std::env::current_dir().expect("workspace directory");
    let dir = tempfile::Builder::new()
        .prefix("khive-missing-pid-parent-")
        .tempdir_in(workspace)
        .expect("workspace-local tempdir");
    let pid_file = dir.path().join("not-created").join("khived.pid");

    assert!(!pid_file_directory_is_trusted_if_present(&pid_file)
        .expect("an absent PID parent is not an unsafe PID record"));
}

fn make_test_server() -> crate::server::KhiveMcpServer {
    let mut config = memory_runtime_config();
    config.default_namespace = Namespace::parse("test").unwrap();
    config.packs = vec!["kg".to_string(), "gtd".to_string()];
    let runtime = KhiveRuntime::new(config).expect("in-memory runtime");
    crate::server::KhiveMcpServer::new(runtime).expect("server builds with kg+gtd")
}

/// Server whose pack set includes `brain`, which registers
/// `Visibility::Subhandler` verbs (`brain.state`, …). Used to exercise the
/// wire visibility gate through the daemon round-trip.
fn make_subhandler_test_server() -> crate::server::KhiveMcpServer {
    let mut config = memory_runtime_config();
    config.default_namespace = Namespace::parse("braintest").unwrap();
    config.packs = vec!["kg".to_string(), "brain".to_string()];
    let runtime = KhiveRuntime::new(config).expect("in-memory runtime");
    crate::server::KhiveMcpServer::new(runtime).expect("server builds with kg+brain")
}

/// Server whose pack set includes `comm`, whose `send` verb echoes the
/// dispatching actor directly in its JSON response (`"from": from_actor`,
/// where `from_actor = token.actor().id`). Used to verify per-request
/// actor stamping (ADR-096 Fork 1) without a readback/inbox round-trip.
fn make_comm_test_server(actor_id: Option<&str>) -> crate::server::KhiveMcpServer {
    let mut config = memory_runtime_config();
    config.default_namespace = Namespace::parse("test").unwrap();
    config.packs = vec!["kg".to_string(), "comm".to_string()];
    config.actor_id = actor_id.map(str::to_string);
    let runtime = KhiveRuntime::new(config).expect("in-memory runtime");
    crate::server::KhiveMcpServer::new(runtime).expect("server builds with kg+comm")
}

fn folded_actor_memory_config(actor: &str) -> RuntimeConfig {
    runtime_config_from_khive_config(
        &KhiveConfig {
            actor: ActorConfig {
                id: Some(actor.to_string()),
                ..ActorConfig::default()
            },
            ..KhiveConfig::default()
        },
        memory_runtime_config(),
    )
}

// ── map_response (pure, MCP-specific) ─────────────────────────────────────

const CFG: &str = "packs=[kg];db=:memory:;embed=none;extra=[];backend=main";
const NS: &str = "test";

mod handover {
    use super::*;
    include!("daemon/long_poll_tests.rs");
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    fn request(ops: &str) -> DaemonRequestFrame {
        DaemonRequestFrame {
            plan: false,
            ops: ops.to_owned(),
            presentation: None,
            presentation_per_op: None,
            namespace: NS.to_owned(),
            actor_id: None,
            process_ref: None,
            visible_namespaces: Vec::new(),
            config_id: CFG.to_owned(),
            protocol_version: PROTOCOL_VERSION,
            probe_only: false,
            metrics_only: false,
            format: None,
            format_per_op: None,
            from_wire: true,
            request_id: Some(17),
        }
    }

    fn isolate(dir: &std::path::Path) {
        clear_daemon_env();
        reset_counters();
        std::env::set_var("KHIVE_SOCKET", dir.join("s"));
        std::env::set_var("KHIVE_PID", dir.join("p"));
        std::env::set_var("KHIVE_LOCK", dir.join("l"));
        std::env::set_var("KHIVE_RECOVERER_LOCK", dir.join("r"));
        // Not written here: absent-by-default matches "no marker" for
        // every test in this module unless it writes one itself.
        std::env::set_var("KHIVE_SUPERVISOR_MARKER", dir.join("m"));
    }

    fn never_spawn() -> std::io::Result<std::process::Child> {
        panic!("socket handover must not spawn or invoke local fallback")
    }

    include!("daemon/supervisor_tests.rs");

    #[tokio::test]
    #[serial]
    async fn reconnect_waits_for_live_owner_socket_gap_without_recovery() {
        if crate::test_isolation::rerun_with_private_home() {
            return;
        }

        let _cleanup = RecoveryTestGuard::new();
        for refused in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            isolate(dir.path());
            let sock = socket_path();
            std::fs::write(pid_path(), std::process::id().to_string()).unwrap();
            if refused {
                drop(tokio::net::UnixListener::bind(&sock).unwrap());
            }
            let peer = tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(150)).await;
                if refused {
                    std::fs::remove_file(&sock).unwrap();
                }
                let listener = tokio::net::UnixListener::bind(&sock).unwrap();
                let (mut stream, _) = listener.accept().await.unwrap();
                let frame: DaemonRequestFrame =
                    serde_json::from_slice(&read_frame(&mut stream).await.unwrap()).unwrap();
                assert!(!frame.probe_only, "grace must not enter lifecycle probing");
                write_frame(
                    &mut stream,
                    &serde_json::to_vec(&frame_ok("gap-ok")).unwrap(),
                )
                .await
                .unwrap();
            });
            let result = tokio::time::timeout(
                Duration::from_secs(3),
                forward_or_spawn_with(&request("stats()"), &never_spawn),
            )
            .await
            .unwrap();
            assert_eq!(result.unwrap().unwrap(), "gap-ok");
            peer.await.unwrap();
            assert_eq!(KILL_COUNT.load(Ordering::SeqCst), 0);
        }
    }

    #[tokio::test]
    #[serial]
    async fn read_response_loss_replays_only_with_explicit_policy_and_keeps_identity() {
        if crate::test_isolation::rerun_with_private_home() {
            return;
        }

        let _cleanup = RecoveryTestGuard::new();
        for enabled in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            isolate(dir.path());
            let listener = tokio::net::UnixListener::bind(socket_path()).unwrap();
            let expected = request("comm.thread(id=\"aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa\")");
            let expected_json = serde_json::to_value(&expected).unwrap();
            let calls = Arc::new(AtomicUsize::new(0));
            let peer_calls = calls.clone();
            let (done_tx, mut done_rx) = tokio::sync::oneshot::channel::<()>();
            let peer = tokio::spawn(async move {
                loop {
                    let (mut stream, _) = tokio::select! {
                        _ = &mut done_rx => break,
                        accepted = listener.accept() => accepted.unwrap(),
                    };
                    let bytes = read_frame(&mut stream).await.unwrap();
                    assert_eq!(
                        serde_json::from_slice::<serde_json::Value>(&bytes).unwrap(),
                        expected_json
                    );
                    let attempt = peer_calls.fetch_add(1, Ordering::SeqCst);
                    if attempt > 0 {
                        write_frame(
                            &mut stream,
                            &serde_json::to_vec(&frame_ok("replayed")).unwrap(),
                        )
                        .await
                        .unwrap();
                    }
                }
            });
            let result = tokio::time::timeout(
                Duration::from_secs(5),
                forward_or_spawn_with_policy(&expected, &never_spawn, enabled),
            )
            .await
            .unwrap();
            let _ = done_tx.send(());
            if enabled {
                assert_eq!(result.unwrap().unwrap(), "replayed");
            } else {
                assert!(result.unwrap().is_err());
            }
            peer.await.unwrap();
            assert_eq!(calls.load(Ordering::SeqCst), if enabled { 2 } else { 1 });
            assert_eq!(KILL_COUNT.load(Ordering::SeqCst), 0);
        }
    }

    #[tokio::test]
    #[serial]
    async fn read_replay_attempt_cap_and_identity_drift_stay_terminal() {
        if crate::test_isolation::rerun_with_private_home() {
            return;
        }

        let _cleanup = RecoveryTestGuard::new();
        for drift in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            isolate(dir.path());
            let listener = tokio::net::UnixListener::bind(socket_path()).unwrap();
            let calls = Arc::new(AtomicUsize::new(0));
            let peer_calls = calls.clone();
            let (done_tx, mut done_rx) = tokio::sync::oneshot::channel::<()>();
            let peer = tokio::spawn(async move {
                loop {
                    let (mut stream, _) = tokio::select! {
                        _ = &mut done_rx => break,
                        accepted = listener.accept() => accepted.unwrap(),
                    };
                    read_frame(&mut stream).await.unwrap();
                    let attempt = peer_calls.fetch_add(1, Ordering::SeqCst);
                    if drift && attempt > 0 {
                        let mut response = frame_ok("must-not-use");
                        response.config_mismatch = true;
                        response.served_config_id = Some("replacement-config".to_owned());
                        write_frame(&mut stream, &serde_json::to_vec(&response).unwrap())
                            .await
                            .unwrap();
                    }
                }
            });
            let result = tokio::time::timeout(
                Duration::from_secs(5),
                forward_or_spawn_with_policy(&request("stats()"), &never_spawn, true),
            )
            .await
            .unwrap();
            let _ = done_tx.send(());
            assert!(result.unwrap().is_err());
            peer.await.unwrap();
            assert_eq!(calls.load(Ordering::SeqCst), 2);
            assert_eq!(KILL_COUNT.load(Ordering::SeqCst), 0);
        }
    }

    #[tokio::test]
    #[serial]
    async fn replay_deadline_never_converts_lost_response_to_no_socket() {
        if crate::test_isolation::rerun_with_private_home() {
            return;
        }

        let _cleanup = RecoveryTestGuard::new();
        let dir = tempfile::tempdir().unwrap();
        isolate(dir.path());
        let listener = tokio::net::UnixListener::bind(socket_path()).unwrap();
        let peer = tokio::spawn(serve_crash_on_dispatch(listener));
        let mut budget = ReadReplayBudget::new(true);
        let deadline = tokio::time::Instant::now() + Duration::from_millis(150);
        let outcome =
            try_forward_with_read_replay(&request("stats()"), &mut budget, Some(deadline)).await;
        assert!(matches!(outcome, ForwardOutcome::ResponseLost));
        assert!(tokio::time::Instant::now() < deadline + Duration::from_millis(300));
        peer.await.unwrap();
        assert_eq!(KILL_COUNT.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    #[serial]
    async fn reconnect_deadline_and_cancellation_prevent_fresh_recovery() {
        if crate::test_isolation::rerun_with_private_home() {
            return;
        }

        let _cleanup = RecoveryTestGuard::new();
        for cancel in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            isolate(dir.path());
            std::fs::write(pid_path(), std::process::id().to_string()).unwrap();
            let (tx, rx) = tokio::sync::watch::channel(false);
            // The scope below owns `rx` and drops it the moment the request resolves.
            // That happens at the 60ms reconnect deadline, and on this current-thread
            // runtime the main future can starve the canceller's 20ms sleep past it, so
            // the send can land after the last receiver is gone -- and `watch::Sender::
            // send` reports exactly that as an error. Holding a receiver here keeps the
            // `unwrap` below a check on the send itself rather than a race against the
            // thing under test; the extra receiver is inert on the path being measured.
            let held = tx.subscribe();
            let canceller = tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(20)).await;
                if cancel {
                    tx.send(true).unwrap();
                }
                tx
            });
            let result = khive_storage::scope_request_read_cancellation(
                rx,
                khive_storage::scope_request_read_deadline(
                    Duration::from_millis(60),
                    forward_or_spawn_with(&request("stats()"), &never_spawn),
                ),
            )
            .await;
            let error = result.unwrap().unwrap_err();
            assert_eq!(error.data.unwrap()["reason"], "daemon_reconnect_expired");
            assert_eq!(KILL_COUNT.load(Ordering::SeqCst), 0);
            canceller.await.unwrap();
            drop(held);
        }
    }

    #[tokio::test]
    #[serial]
    async fn malformed_protocol_and_exhausted_deadlines_are_not_replayed() {
        if crate::test_isolation::rerun_with_private_home() {
            return;
        }

        let _cleanup = RecoveryTestGuard::new();
        for kind in ["malformed", "timeout", "protocol"] {
            let dir = tempfile::tempdir().unwrap();
            isolate(dir.path());
            let listener = tokio::net::UnixListener::bind(socket_path()).unwrap();
            let peer = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                read_frame(&mut stream).await.unwrap();
                match kind {
                    "malformed" => write_frame(&mut stream, b"not-json").await.unwrap(),
                    "protocol" => {
                        let mut response = frame_ok("wrong-version");
                        response.daemon_protocol_version = PROTOCOL_VERSION + 1;
                        write_frame(&mut stream, &serde_json::to_vec(&response).unwrap())
                            .await
                            .unwrap();
                    }
                    _ => tokio::time::sleep(Duration::from_millis(750)).await,
                }
                assert!(
                    tokio::time::timeout(Duration::from_millis(150), listener.accept())
                        .await
                        .is_err()
                );
            });
            let mut budget = ReadReplayBudget::new(true);
            let deadline = tokio::time::Instant::now()
                + if kind == "timeout" {
                    Duration::from_millis(500)
                } else {
                    Duration::from_secs(2)
                };
            let outcome =
                try_forward_with_read_replay(&request("stats()"), &mut budget, Some(deadline))
                    .await;
            match kind {
                "protocol" => {
                    assert!(matches!(outcome, ForwardOutcome::ProtocolMismatch { .. }))
                }
                _ => assert!(matches!(outcome, ForwardOutcome::ParseFailure)),
            }
            assert_eq!(budget.remaining, 1);
            peer.await.unwrap();
        }
    }

    #[tokio::test]
    #[ignore = "isolated subprocess fixture"]
    async fn graceful_incumbent_child() {
        assert_eq!(
            std::env::var("KHIVE_HANDOVER_TEST_CHILD").as_deref(),
            Ok("1")
        );
        let _signal =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).unwrap();
        run_daemon(HarnessDispatch::new(NS, "old-config"))
            .await
            .unwrap();
        if let Ok(successor) = std::env::var("KHIVE_HANDOVER_SUCCESSOR_PID") {
            std::fs::write(pid_path(), successor).unwrap();
        }
    }

    #[tokio::test]
    #[serial]
    async fn recovery_releases_boot_lock_for_graceful_exit_and_rechecks_owner() {
        if crate::test_isolation::rerun_with_private_home() {
            return;
        }

        for successor_wins in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let mut cleanup = RecoveryTestGuard::new();
            isolate(dir.path());
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "--exact",
                    "daemon::tests::handover::graceful_incumbent_child",
                    "--ignored",
                    "--nocapture",
                ])
                .env("HOME", dir.path())
                .current_dir(dir.path())
                .env("KHIVE_HANDOVER_TEST_CHILD", "1")
                .env_remove("KHIVE_HANDOVER_SUCCESSOR_PID")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            if successor_wins {
                command.env(
                    "KHIVE_HANDOVER_SUCCESSOR_PID",
                    std::process::id().to_string(),
                );
            }
            let incumbent_pid = cleanup.track_child(command.spawn().unwrap());
            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            while !matches!(
                probe_daemon_identity("old-config", NS, 100).await,
                ProbeOutcome::Alive
            ) {
                assert!(tokio::time::Instant::now() < deadline, "child readiness");
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            FORCE_PID_IS_DAEMON.store(true, Ordering::SeqCst);
            let launches = AtomicUsize::new(0);
            let spawn = || {
                assert!(!process_is_alive(incumbent_pid));
                launches.fetch_add(1, Ordering::SeqCst);
                std::process::Command::new("/bin/sh")
                    .args(["-c", "exit 0"])
                    .spawn()
            };
            // Reap our child while recovery observes its exit: without `ps`
            // access, an exited but unreaped child still looks alive to kill(0).
            // Keep it in the guard so a timeout or panic still cleans it up.
            let reap_incumbent = async {
                let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
                loop {
                    if let Some(status) = cleanup.child_mut().try_wait().unwrap() {
                        break status;
                    }
                    assert!(
                        tokio::time::Instant::now() < deadline,
                        "incumbent did not finish graceful exit"
                    );
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            };
            let (result, incumbent_status) = tokio::join!(
                kill_and_respawn_with_exit_timeout(CFG, NS, &spawn, Duration::from_secs(2)),
                reap_incumbent,
            );
            assert!(incumbent_status.success());
            assert_eq!(
                SIGTERM_COUNT.load(Ordering::SeqCst),
                1,
                "the owned incumbent is the positive control for actual SIGTERM attempts"
            );
            match (successor_wins, result) {
                (false, Ok(RecoveryOutcome::Spawned(mut child))) => {
                    assert!(child.wait().unwrap().success());
                }
                (true, Ok(RecoveryOutcome::Uncertain)) => {}
                (_, other) => panic!("unexpected recovery outcome: {other:?}"),
            }
            assert_eq!(
                launches.load(Ordering::SeqCst),
                usize::from(!successor_wins)
            );
            if successor_wins {
                assert_eq!(
                    std::fs::read_to_string(pid_path()).unwrap(),
                    std::process::id().to_string()
                );
            }
        }
    }
}

fn frame_ok(result: &str) -> DaemonResponseFrame {
    DaemonResponseFrame {
        ok: true,
        result: Some(result.to_string()),
        error: None,
        error_detail: None,
        namespace_mismatch: false,
        config_mismatch: false,
        served_config_id: Some(CFG.to_string()),
        version_mismatch: false,
        daemon_protocol_version: PROTOCOL_VERSION,
        metrics: None,
        request_id: None,
    }
}

fn frame_err(error: Option<&str>) -> DaemonResponseFrame {
    DaemonResponseFrame {
        ok: false,
        result: None,
        error: error.map(str::to_string),
        error_detail: None,
        namespace_mismatch: false,
        config_mismatch: false,
        served_config_id: Some(CFG.to_string()),
        version_mismatch: false,
        daemon_protocol_version: PROTOCOL_VERSION,
        metrics: None,
        request_id: None,
    }
}

// These four tests exercise `map_response` branches that now also increment
// the process-global fallback counters (via `record_fallback`) — `#[serial]`
// + a reset at the top keeps their counter assertions deterministic against
// any other test in this file that touches the same counters.

#[test]
#[serial]
fn map_response_namespace_mismatch_yields_none() {
    reset_fallback_counters();
    let resp = DaemonResponseFrame {
        ok: false,
        result: None,
        error: None,
        error_detail: None,
        namespace_mismatch: true,
        config_mismatch: false,
        served_config_id: Some(CFG.to_string()),
        version_mismatch: false,
        daemon_protocol_version: PROTOCOL_VERSION,
        metrics: None,
        request_id: None,
    };
    assert!(map_response(resp, CFG, NS).is_none());
    assert_eq!(fallback_count(FallbackReason::NamespaceMismatch), 1);
    assert_eq!(fallback_count(FallbackReason::ConfigMismatch), 0);
    assert_eq!(fallback_total(), 1);
}

#[test]
#[serial]
fn map_response_config_mismatch_yields_none() {
    reset_fallback_counters();
    let resp = DaemonResponseFrame {
        ok: false,
        result: None,
        error: None,
        error_detail: None,
        namespace_mismatch: false,
        config_mismatch: true,
        served_config_id: Some(CFG.to_string()),
        version_mismatch: false,
        daemon_protocol_version: PROTOCOL_VERSION,
        metrics: None,
        request_id: None,
    };
    assert!(map_response(resp, CFG, NS).is_none());
    assert_eq!(fallback_count(FallbackReason::ConfigMismatch), 1);
    assert_eq!(fallback_count(FallbackReason::NamespaceMismatch), 0);
    assert_eq!(fallback_total(), 1);
}

#[test]
#[serial]
fn map_response_legacy_daemon_missing_echo_yields_none() {
    // A pre-config_id daemon omits served_config_id (→ None). Even on an
    // ok=true result the client MUST fall back to local dispatch.
    reset_fallback_counters();
    let resp = DaemonResponseFrame {
        ok: true,
        result: Some("served-by-broad-registry".to_string()),
        error: None,
        error_detail: None,
        namespace_mismatch: false,
        config_mismatch: false,
        served_config_id: None,
        version_mismatch: false,
        daemon_protocol_version: 0,
        metrics: None,
        request_id: None,
    };
    assert!(map_response(resp, CFG, NS).is_none());
    // The served_config_id-echo path is bucketed under config_mismatch —
    // there is no separate reason for "echo missing/drifted" in the closed
    // 5-value reason set.
    assert_eq!(fallback_count(FallbackReason::ConfigMismatch), 1);
    assert_eq!(fallback_total(), 1);
}

#[test]
#[serial]
fn map_response_echo_drift_yields_none() {
    // A daemon serving under a different config (echo != expected) is not
    // trusted, even without an explicit config_mismatch flag.
    reset_fallback_counters();
    let resp = DaemonResponseFrame {
        ok: true,
        result: Some("served-by-other-config".to_string()),
        error: None,
        error_detail: None,
        namespace_mismatch: false,
        config_mismatch: false,
        served_config_id: Some("packs=[kg,gtd];db=/x;embed=none;extra=[];backend=main".to_string()),
        version_mismatch: false,
        daemon_protocol_version: PROTOCOL_VERSION,
        metrics: None,
        request_id: None,
    };
    assert!(map_response(resp, CFG, NS).is_none());
    assert_eq!(fallback_count(FallbackReason::ConfigMismatch), 1);
    assert_eq!(fallback_total(), 1);
}

#[test]
fn map_response_ok_with_result_yields_some_ok() {
    match map_response(frame_ok("the-result"), CFG, NS) {
        Some(Ok(s)) => assert_eq!(s, "the-result"),
        other => panic!("expected Some(Ok(\"the-result\")), got {other:?}"),
    }
}

#[test]
fn accepted_daemon_timeout_warns_once_and_preserves_result_bytes() {
    let public = serde_json::json!({
        "results": [
            {"ok": true, "tool": "knowledge.search", "result": "| name |\n|---|\n| first |\n"},
            {"ok": true, "tool": "knowledge.suggest", "result": "| name |\n|---|\n| second |\n"}
        ],
        "summary": {"total": 2, "succeeded": 2, "failed": 0}
    });
    let raw = public.to_string();
    let nested = serde_json::json!({
        "results": [{"ok": true, "tool": "stats", "result": {
            "degraded": {"lexical_timeout": true}
        }}]
    })
    .to_string();
    let unmarked = serde_json::json!({
        "results": [{"ok": true, "tool": "knowledge.search", "result": {
            "degraded": {"lexical_timeout": true}
        }}]
    })
    .to_string();
    let logs = capture_sync_events(|| {
        let mut frame = frame_ok(&raw);
        frame.error_detail = Some(serde_json::json!({"lexical_timeout": true}));
        let accepted = map_response(frame, CFG, NS)
            .expect("accepted response")
            .expect("successful response");
        assert_eq!(accepted, raw);
        assert_eq!(
            map_response(frame_ok(&nested), CFG, NS)
                .expect("accepted response")
                .expect("successful response"),
            nested
        );
        assert_eq!(
            map_response(frame_ok(&unmarked), CFG, NS)
                .expect("accepted response")
                .expect("successful response"),
            unmarked
        );
    });
    assert_eq!(logs.matches("lexical read timed out").count(), 1, "{logs}");
    assert!(logs.contains("daemon_response"), "{logs}");
}

#[test]
fn map_response_ok_with_no_result_yields_some_ok_empty() {
    let resp = DaemonResponseFrame {
        ok: true,
        result: None,
        error: None,
        error_detail: None,
        namespace_mismatch: false,
        config_mismatch: false,
        served_config_id: Some(CFG.to_string()),
        version_mismatch: false,
        daemon_protocol_version: PROTOCOL_VERSION,
        metrics: None,
        request_id: None,
    };
    match map_response(resp, CFG, NS) {
        Some(Ok(s)) => assert_eq!(s, ""),
        other => panic!("expected Some(Ok(\"\")), got {other:?}"),
    }
}

#[test]
fn map_response_not_ok_yields_some_err_preserving_message() {
    match map_response(frame_err(Some("boom: bad verb")), CFG, NS) {
        Some(Err(McpError { message, .. })) => {
            assert!(message.contains("boom: bad verb"), "got: {message}");
        }
        other => panic!("expected Some(Err(..)), got {other:?}"),
    }
}

#[test]
fn disposition_map_response_preserves_error_detail_without_dispatch() {
    let detail = serde_json::json!({
        "kind": "obligation",
        "code": "store_failure",
        "message": "audit failed",
        "domain_disposition": "committed",
        "domain_result": { "id": "persisted-row" },
    });
    let mut frame = frame_err(Some("audit failed"));
    frame.error_detail = Some(detail.clone());
    let error = map_response(frame, CFG, NS).unwrap().unwrap_err();
    assert_eq!(error.message, "audit failed");
    assert_eq!(error.data, Some(detail));
}

#[test]
fn disposition_legacy_and_lost_responses_are_unknown() {
    let legacy = map_response(frame_err(Some("legacy failure")), CFG, NS)
        .unwrap()
        .unwrap_err();
    for error in [legacy, ambiguous_forward_error()] {
        let detail = error.data.unwrap();
        assert_eq!(detail["domain_disposition"], "unknown");
        assert!(detail.get("domain_result").is_none());
    }
}

#[test]
fn disposition_version_mismatch_is_unknown_even_with_untrusted_detail() {
    let mut frame = frame_err(Some("protocol mismatch"));
    frame.version_mismatch = true;
    frame.error_detail = Some(serde_json::json!({
        "message": "protocol mismatch",
        "domain_disposition": "committed",
        "domain_result": { "id": "uncertain-peer-result" },
    }));
    let detail = map_response(frame, CFG, NS)
        .unwrap()
        .unwrap_err()
        .data
        .unwrap();
    assert_eq!(detail["domain_disposition"], "unknown");
    assert_eq!(detail["code"], "version_mismatch");
    assert!(detail.get("domain_result").is_none());
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn disposition_mcp_adapter_preserves_request_error_data() {
    use daemon::DaemonDispatch;

    let server = make_test_server();
    let params = RequestParams {
        plan: None,
        ops: "[".to_string(),
        presentation: None,
        presentation_per_op: None,
        save_to: None,
        format: None,
        format_per_op: None,
        request_id: None,
    };
    let expected = server
        .dispatch_request_inner(params, false, None, crate::server::DispatchOrigin::Daemon)
        .await
        .unwrap_err();
    let actual = server
        .dispatch_with_error_detail("[".to_string(), None, None, None, None, false, None)
        .await
        .unwrap_err();
    assert_eq!(actual.message, expected.message.to_string());
    assert_eq!(Some(actual.error_detail), expected.data);
}

#[test]
fn map_response_not_ok_without_message_yields_contextual_err() {
    match map_response(frame_err(None), CFG, NS) {
        Some(Err(McpError { message, .. })) => {
            assert!(!message.is_empty(), "fallback message must not be empty");
            assert!(
                message.contains("daemon returned an error"),
                "fallback must say 'daemon returned an error'; got: {message}"
            );
        }
        other => panic!("expected Some(Err(..)), got {other:?}"),
    }
}

#[test]
fn map_response_version_mismatch_yields_explicit_error() {
    let resp = DaemonResponseFrame {
        ok: false,
        result: None,
        error: Some("daemon protocol mismatch: client=0 daemon=1 — rebuild/update the client binary (make local)".to_string()),
        error_detail: None,
        namespace_mismatch: false,
        config_mismatch: false,
        served_config_id: Some(CFG.to_string()),
        version_mismatch: true,
        daemon_protocol_version: PROTOCOL_VERSION,
        metrics: None,
        request_id: None,
    };
    match map_response(resp, CFG, NS) {
        Some(Err(McpError { message, .. })) => {
            assert!(
                message.contains("protocol mismatch"),
                "version mismatch error must name the mismatch; got: {message}"
            );
            assert!(
                message.contains("make local"),
                "version mismatch error must tell the operator what to do; got: {message}"
            );
        }
        other => panic!("expected Some(Err(..)): got {other:?}"),
    }
}

#[test]
fn map_response_version_mismatch_without_error_field_synthesizes_message() {
    let resp = DaemonResponseFrame {
        ok: false,
        result: None,
        error: None,
        error_detail: None,
        namespace_mismatch: false,
        config_mismatch: false,
        served_config_id: Some(CFG.to_string()),
        version_mismatch: true,
        daemon_protocol_version: 99,
        metrics: None,
        request_id: None,
    };
    match map_response(resp, CFG, NS) {
        Some(Err(McpError { message, .. })) => {
            assert!(
                message.contains("protocol mismatch"),
                "synthesized message must name the mismatch; got: {message}"
            );
            assert!(
                message.contains("99"),
                "synthesized message must include daemon version; got: {message}"
            );
        }
        other => panic!("expected Some(Err(..)): got {other:?}"),
    }
}

// ── daemon_fallback telemetry: counters (ADR-091 F1) ──────────────────────
//
// `no_socket` remains a production fallback. `parse_failure` and
// `protocol_mismatch` are reserved closed-vocabulary counters with no
// production `record_fallback` call site after #644: both are terminal
// once a real request is written. Exercise all three directly here so
// their metrics compatibility remains explicit without implying that a
// post-write ambiguity can still reach local dispatch.

#[test]
#[serial]
fn record_fallback_no_socket_increments_matching_counter_and_total() {
    reset_fallback_counters();
    record_fallback(FallbackReason::NoSocket, CFG, None, NS);
    assert_eq!(fallback_count(FallbackReason::NoSocket), 1);
    assert_eq!(fallback_count(FallbackReason::ParseFailure), 0);
    assert_eq!(fallback_count(FallbackReason::ProtocolMismatch), 0);
    assert_eq!(fallback_count(FallbackReason::ConfigMismatch), 0);
    assert_eq!(fallback_count(FallbackReason::NamespaceMismatch), 0);
    assert_eq!(fallback_total(), 1);
}

#[test]
#[serial]
fn record_fallback_parse_failure_increments_matching_counter_and_total() {
    reset_fallback_counters();
    record_fallback(FallbackReason::ParseFailure, CFG, None, NS);
    assert_eq!(fallback_count(FallbackReason::ParseFailure), 1);
    assert_eq!(fallback_count(FallbackReason::NoSocket), 0);
    assert_eq!(fallback_total(), 1);
}

#[test]
#[serial]
fn record_fallback_protocol_mismatch_increments_matching_counter_and_total() {
    reset_fallback_counters();
    record_fallback(FallbackReason::ProtocolMismatch, CFG, None, NS);
    assert_eq!(fallback_count(FallbackReason::ProtocolMismatch), 1);
    assert_eq!(fallback_count(FallbackReason::ParseFailure), 0);
    assert_eq!(fallback_total(), 1);
}

#[test]
#[serial]
fn record_fallback_config_id_daemon_defaults_to_none_literal_when_absent() {
    // The counter side effect proves the call completes and increments
    // exactly once when `config_id_daemon` is `None` (the common case when
    // no decodable daemon response supplied a served config id).
    reset_fallback_counters();
    record_fallback(FallbackReason::NoSocket, CFG, None, NS);
    assert_eq!(fallback_total(), 1);
}

#[test]
fn first_config_mismatch_field_follows_fingerprint_order() {
    let client = "packs=[kg];db=/private/client.db;embed=none;extra=[];fresh_tail=true;\
                      blob_hydration_bytes=268435456;backend=main;outbound=[];git_write=client-policy";
    let daemon = "packs=[kg,gtd];db=/private/daemon.db;embed=none;extra=[];fresh_tail=true;\
                      blob_hydration_bytes=268435456;backend=main;outbound=[];git_write=daemon-policy";

    assert_eq!(first_config_mismatch_field(client, Some(daemon)), "packs");
}

#[test]
fn first_config_mismatch_field_names_fresh_tail_from_computed_ids() {
    let config = RuntimeConfig::no_embeddings();
    let enabled = crate::server::compute_config_id_with_ann_fresh_tail(&config, None, true);
    let disabled = crate::server::compute_config_id_with_ann_fresh_tail(&config, None, false);

    assert_eq!(
        first_config_mismatch_field(&enabled, Some(&disabled)),
        "fresh_tail"
    );
}

#[test]
fn first_config_mismatch_field_names_blob_hydration_budget_from_computed_ids() {
    let config = RuntimeConfig::no_embeddings();
    let mut changed = config.clone();
    changed.blob_hydration_bytes /= 2;
    let client = crate::server::compute_config_id_with_ann_fresh_tail(&config, None, true);
    let daemon = crate::server::compute_config_id_with_ann_fresh_tail(&changed, None, true);

    assert_eq!(
        first_config_mismatch_field(&client, Some(&daemon)),
        "blob_hydration_bytes"
    );
}

#[test]
fn legacy_config_id_reports_the_new_blob_budget_as_its_first_mismatch() {
    let legacy = "packs=[kg];db=:memory:;embed=none;extra=[];fresh_tail=true;\
                      backend=main;outbound=[];git_write=policy";
    let current = "packs=[kg];db=:memory:;embed=none;extra=[];fresh_tail=true;\
                       blob_hydration_bytes=268435456;backend=main;outbound=[];git_write=policy";

    assert_eq!(
        first_config_mismatch_field(current, Some(legacy)),
        "blob_hydration_bytes"
    );
}

#[test]
fn first_config_mismatch_field_names_later_field_from_computed_ids() {
    let config = RuntimeConfig::no_embeddings();
    let mut changed = config.clone();
    changed.allowed_outbound_namespaces = vec![Namespace::parse("remote").unwrap()];
    let client = crate::server::compute_config_id_with_ann_fresh_tail(&config, None, true);
    let daemon = crate::server::compute_config_id_with_ann_fresh_tail(&changed, None, true);

    assert_eq!(
        first_config_mismatch_field(&client, Some(&daemon)),
        "outbound"
    );
}

#[test]
fn first_config_mismatch_field_names_caller_enrollment_policy() {
    let base = RuntimeConfig::no_embeddings();
    let enrolled = RuntimeConfig {
        gate: Arc::new(khive_runtime::CallerEnrollmentGate::new(
            vec!["lambda:enrolled".to_string()],
            false,
        )),
        ..base.clone()
    };
    let revoked = RuntimeConfig {
        gate: Arc::new(khive_runtime::CallerEnrollmentGate::new(Vec::new(), false)),
        ..base
    };
    let client = crate::server::compute_config_id_with_ann_fresh_tail(&enrolled, None, true);
    let daemon = crate::server::compute_config_id_with_ann_fresh_tail(&revoked, None, true);

    assert_eq!(first_config_mismatch_field(&client, Some(&daemon)), "gate");
}

#[test]
fn first_config_mismatch_field_names_brain_read_policy() {
    let config = RuntimeConfig::no_embeddings();
    let mut changed = config.clone();
    changed.brain.fleet_readers = vec!["lambda:reader".to_string()];
    let client = crate::server::compute_config_id_with_runtime_policies(&config, None, true, false);
    let daemon =
        crate::server::compute_config_id_with_runtime_policies(&changed, None, true, false);

    assert_eq!(first_config_mismatch_field(&client, Some(&daemon)), "brain");
}

#[test]
fn first_config_mismatch_field_names_telemetry_policy_and_legacy_absence() {
    let config = RuntimeConfig::no_embeddings();
    let mut changed = config.clone();
    changed.telemetry.default_carrier = Some(khive_runtime::TelemetryCarrier::Durable);
    let client = crate::server::compute_config_id_with_runtime_policies(&config, None, true, false);
    let daemon =
        crate::server::compute_config_id_with_runtime_policies(&changed, None, true, false);
    assert_eq!(
        first_config_mismatch_field(&client, Some(&daemon)),
        "telemetry"
    );

    let (prefix, rest) = client.split_once(";telemetry=").unwrap();
    let (_, suffix) = rest.split_once(";display_tz=").unwrap();
    let legacy = format!("{prefix};display_tz={suffix}");
    assert_eq!(
        first_config_mismatch_field(&client, Some(&legacy)),
        "telemetry"
    );
}

#[test]
fn first_config_mismatch_field_names_backend_topology_without_values() {
    let base = "packs=[kg];db=:memory:;embed=none;extra=[];fresh_tail=true;\
                    blob_hydration_bytes=268435456;backend=main;outbound=[];git_write=policy";
    let client =
        format!("{base};backends=[main:Sqlite:/private/client.db];pack_backends=[kg=main]");
    let daemon =
        format!("{base};backends=[main:Sqlite:/private/daemon.db];pack_backends=[kg=main]");

    assert_eq!(
        first_config_mismatch_field(&client, Some(&daemon)),
        "backends"
    );
}

#[test]
fn first_config_mismatch_field_recognizes_read_only_runtime_mode() {
    let config = RuntimeConfig::no_embeddings();
    let writable =
        crate::server::compute_config_id_with_runtime_policies(&config, None, true, false);
    let read_only =
        crate::server::compute_config_id_with_runtime_policies(&config, None, true, true);

    assert_eq!(
        first_config_mismatch_field(&read_only, Some(&writable)),
        "backend",
        "storage-mode separation must retain a structured mismatch field"
    );
}

#[test]
#[serial]
fn map_response_config_mismatch_logs_opaque_ids_and_field_without_values() {
    reset_fallback_counters();
    let client =
        "packs=[kg];db=/private/client-topology/main.db;embed=none;extra=[];fresh_tail=true;\
                      blob_hydration_bytes=268435456;backend=main;outbound=[];git_write=same-policy";
    let daemon =
        "packs=[kg];db=/private/daemon-topology/main.db;embed=none;extra=[];fresh_tail=true;\
                      blob_hydration_bytes=268435456;backend=main;outbound=[];git_write=same-policy";
    let response = DaemonResponseFrame {
        ok: false,
        result: None,
        error: None,
        error_detail: None,
        namespace_mismatch: false,
        config_mismatch: true,
        served_config_id: Some(daemon.to_string()),
        version_mismatch: false,
        daemon_protocol_version: PROTOCOL_VERSION,
        metrics: None,
        request_id: None,
    };

    let events = capture_sync_events(|| {
        assert!(map_response(response, client, NS).is_none());
    });

    assert!(events.contains("daemon_fallback"), "{events}");
    assert!(events.contains("config_mismatch_field=\"db\""), "{events}");
    assert!(events.contains(&opaque_config_id(client)), "{events}");
    assert!(events.contains(&opaque_config_id(daemon)), "{events}");
    assert!(!events.contains(client), "{events}");
    assert!(!events.contains(daemon), "{events}");
    assert!(!events.contains("client-topology"), "{events}");
    assert!(!events.contains("daemon-topology"), "{events}");
    assert_eq!(fallback_count(FallbackReason::ConfigMismatch), 1);
    assert_eq!(fallback_total(), 1);
}

#[test]
#[serial]
fn map_response_matching_config_emits_no_fallback_diagnostic() {
    reset_fallback_counters();
    let events = capture_sync_events(|| {
        assert!(matches!(map_response(frame_ok("ok"), CFG, NS), Some(Ok(_))));
    });

    assert!(
        events.is_empty(),
        "matching config logged fallback: {events}"
    );
    assert_eq!(fallback_total(), 0);
}

#[test]
#[serial]
fn fallback_total_sums_all_reason_counters() {
    reset_fallback_counters();
    record_fallback(FallbackReason::ConfigMismatch, CFG, Some("other-cfg"), NS);
    record_fallback(
        FallbackReason::NamespaceMismatch,
        CFG,
        Some(CFG),
        "other-ns",
    );
    record_fallback(FallbackReason::NoSocket, CFG, None, NS);
    record_fallback(FallbackReason::ParseFailure, CFG, None, NS);
    record_fallback(FallbackReason::ProtocolMismatch, CFG, None, NS);

    let sum = fallback_count(FallbackReason::ConfigMismatch)
        + fallback_count(FallbackReason::NamespaceMismatch)
        + fallback_count(FallbackReason::NoSocket)
        + fallback_count(FallbackReason::ParseFailure)
        + fallback_count(FallbackReason::ProtocolMismatch);
    assert_eq!(sum, 5);
    assert_eq!(
        fallback_total(),
        5,
        "total must equal the sum of all reasons"
    );
}

// ── KHIVE_DAEMON_STRICT graduated fail-loud policy (D2) ───────────────────
//
// These tests prove the graduated behavior via `FALLBACK_STRICT_VIOLATIONS`:
// an `Illegitimate` reason bumps it if and only if strict mode is on; every
// other reason/mode combination must never bump it.

fn with_daemon_strict<T>(value: Option<&str>, f: impl FnOnce() -> T) -> T {
    let prev = std::env::var("KHIVE_DAEMON_STRICT").ok();
    match value {
        Some(v) => std::env::set_var("KHIVE_DAEMON_STRICT", v),
        None => std::env::remove_var("KHIVE_DAEMON_STRICT"),
    }
    let result = f();
    match prev {
        Some(v) => std::env::set_var("KHIVE_DAEMON_STRICT", v),
        None => std::env::remove_var("KHIVE_DAEMON_STRICT"),
    }
    result
}

#[test]
#[serial]
fn record_fallback_config_mismatch_strict_off_never_bumps_strict_violations() {
    with_daemon_strict(None, || {
        reset_fallback_counters();
        record_fallback(FallbackReason::ConfigMismatch, CFG, Some("other-cfg"), NS);
        assert_eq!(fallback_count(FallbackReason::ConfigMismatch), 1);
        assert_eq!(
            fallback_strict_violations(),
            0,
            "strict mode is OFF (default local-dev behavior) — the illegitimate \
                 reason must still bump its own counter, but never the strict-violations one"
        );
    });
}

#[test]
#[serial]
fn record_fallback_config_mismatch_strict_on_bumps_strict_violations() {
    with_daemon_strict(Some("1"), || {
        reset_fallback_counters();
        record_fallback(FallbackReason::ConfigMismatch, CFG, Some("other-cfg"), NS);
        assert_eq!(fallback_count(FallbackReason::ConfigMismatch), 1);
        assert_eq!(
            fallback_strict_violations(),
            1,
            "KHIVE_DAEMON_STRICT=1 + an Illegitimate reason (config_mismatch) must \
                 bump the strict-violations counter (D2-R1)"
        );
    });
}

#[test]
#[serial]
fn record_fallback_namespace_mismatch_strict_on_bumps_strict_violations() {
    with_daemon_strict(Some("1"), || {
        reset_fallback_counters();
        record_fallback(
            FallbackReason::NamespaceMismatch,
            CFG,
            Some(CFG),
            "other-ns",
        );
        assert_eq!(
            fallback_strict_violations(),
            1,
            "KHIVE_DAEMON_STRICT=1 + an Illegitimate reason (namespace_mismatch) must \
                 bump the strict-violations counter (D2-R1)"
        );
    });
}

#[test]
#[serial]
fn record_fallback_no_socket_strict_on_never_bumps_strict_violations() {
    with_daemon_strict(Some("1"), || {
        reset_fallback_counters();
        record_fallback(FallbackReason::NoSocket, CFG, None, NS);
        assert_eq!(
            fallback_strict_violations(),
            0,
            "NoSocket is the ADR-049-mandated no-daemon path — it must NEVER be \
                 elevated, even in strict mode (D2-R3)"
        );
    });
}

#[test]
#[serial]
fn record_fallback_protocol_mismatch_strict_on_never_bumps_strict_violations() {
    with_daemon_strict(Some("1"), || {
        reset_fallback_counters();
        record_fallback(FallbackReason::ProtocolMismatch, CFG, None, NS);
        assert_eq!(
            fallback_strict_violations(),
            0,
            "the reserved ProtocolMismatch metric retains its historical \
                 rollout-transient tier and is never a strict violation"
        );
    });
}

#[test]
#[serial]
fn record_fallback_parse_failure_strict_on_never_bumps_strict_violations() {
    with_daemon_strict(Some("1"), || {
        reset_fallback_counters();
        record_fallback(FallbackReason::ParseFailure, CFG, None, NS);
        assert_eq!(
            fallback_strict_violations(),
            0,
            "the reserved ParseFailure metric retains its historical \
                 rollout-transient tier and is never a strict violation"
        );
    });
}

#[test]
fn fallback_reason_severity_matches_the_d2_legitimacy_table() {
    assert_eq!(
        FallbackReason::ConfigMismatch.severity(),
        FallbackSeverity::Illegitimate
    );
    assert_eq!(
        FallbackReason::NamespaceMismatch.severity(),
        FallbackSeverity::Illegitimate
    );
    assert_eq!(
        FallbackReason::ProtocolMismatch.severity(),
        FallbackSeverity::RolloutTransient
    );
    assert_eq!(
        FallbackReason::ParseFailure.severity(),
        FallbackSeverity::RolloutTransient
    );
    assert_eq!(
        FallbackReason::NoSocket.severity(),
        FallbackSeverity::NoDaemon
    );
}

// ── fallback_or_reject: strict mode fails the request (#947) ──────────────
//
// #947: `KHIVE_DAEMON_STRICT=1` must turn a would-be fallback into a
// caller-visible error naming the reason, for EVERY `FallbackReason` —
// not just the `Illegitimate` tier that `record_fallback`'s WARN/ERROR
// log-level graduation (D2-R1) cares about. These tests exercise the
// decision function directly, at the same private-fn level as the
// `record_fallback_*` tests above, so they run in milliseconds instead of
// needing a real unreachable-socket round trip.

#[test]
#[serial]
fn fallback_or_reject_non_strict_returns_none_and_still_counts() {
    with_daemon_strict(None, || {
        reset_fallback_counters();
        let out = fallback_or_reject(FallbackReason::NoSocket, CFG, None, NS);
        assert!(
            out.is_none(),
            "non-strict mode must keep completing locally, unchanged by #947"
        );
        assert_eq!(fallback_count(FallbackReason::NoSocket), 1);
    });
}

#[test]
#[serial]
fn fallback_or_reject_strict_no_socket_errors_naming_the_reason() {
    with_daemon_strict(Some("1"), || {
        reset_fallback_counters();
        match fallback_or_reject(FallbackReason::NoSocket, CFG, None, NS) {
            Some(Err(McpError { message, .. })) => {
                assert!(
                    message.contains("no_socket"),
                    "error must name the fallback reason: {message}"
                );
                assert!(
                    message.contains("KHIVE_DAEMON_STRICT"),
                    "error should point at the mode that caused the rejection: {message}"
                );
            }
            other => panic!("strict mode must reject the request, got {other:?}"),
        }
        // Counters/telemetry are untouched by this change — still exactly
        // what `record_fallback` alone would have produced.
        assert_eq!(fallback_count(FallbackReason::NoSocket), 1);
        assert_eq!(fallback_total(), 1);
    });
}

#[test]
#[serial]
fn fallback_or_reject_strict_config_mismatch_errors_naming_the_reason() {
    with_daemon_strict(Some("1"), || {
        reset_fallback_counters();
        match fallback_or_reject(FallbackReason::ConfigMismatch, CFG, Some("other-cfg"), NS) {
            Some(Err(McpError { message, .. })) => {
                assert!(message.contains("config_mismatch"), "{message}");
            }
            other => panic!("strict mode must reject the request, got {other:?}"),
        }
        // An `Illegitimate`-tier reason still bumps the pre-existing
        // strict-violations counter exactly as it did before #947 — this
        // change only affects the return value, never the telemetry.
        assert_eq!(fallback_strict_violations(), 1);
    });
}

// ── forward_or_spawn fallback (env-mutating → serial) ─────────────────────

#[tokio::test]
#[serial]
async fn forward_or_spawn_returns_none_when_no_daemon_set() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }

    clear_daemon_env();
    reset_fallback_counters();
    let dir = tempfile::tempdir().expect("tempdir");
    let sock = dir.path().join("khived.sock");
    std::env::set_var("KHIVE_SOCKET", &sock);
    std::env::set_var("KHIVE_PID", dir.path().join("khived.pid"));
    std::env::set_var("KHIVE_NO_DAEMON", "1");

    let frame = DaemonRequestFrame {
        plan: false,
        ops: "stats()".to_string(),
        presentation: None,
        presentation_per_op: None,
        namespace: "test".to_string(),
        actor_id: None,
        process_ref: None,
        visible_namespaces: Vec::new(),
        config_id: "test".to_string(),
        protocol_version: PROTOCOL_VERSION,
        probe_only: false,
        metrics_only: false,
        format: None,
        format_per_op: None,
        from_wire: false,
        request_id: None,
    };
    let out = forward_or_spawn(&frame).await;
    assert!(out.is_none());
    assert!(!sock.exists());
    // KHIVE_NO_DAEMON is an explicit operator opt-out, not one of the 5
    // silent-fallback reasons this telemetry tracks — it must NOT bump the
    // fallback counters (that would be noisy for legitimate always-local
    // deployments).
    assert_eq!(fallback_total(), 0);

    clear_daemon_env();
}

// #898: genuine daemon-unreachable fallback (no `KHIVE_NO_DAEMON` opt-out),
// where the respawn THIS call attempted can be positively confirmed dead.
// `spawn_daemon()` really runs here (`SPAWN_COUNT` bumps), spawning this
// same test binary re-invoked with unrecognized `mcp --daemon` args; it
// exits immediately without ever binding the socket — mirroring the
// 2026-07-12 incident's version-skewed binary (`error: Unrecognized
// option: 'daemon'`). This must surface as a loud, caller-visible error in
// BOTH strict and non-strict mode: unlike the ordinary "no daemon
// reachable, cause unknown" fallback, a respawn this process made and can
// prove failed is never eligible for a silent local-dispatch completion.
// Each run pays the ~5s forward deadline plus the boot-quiescence reprobe
// — see `forward_or_spawn_blocks_on_boot_quiescence_before_local_fallback`
// below, which asserts that wait is unaffected by this change.

fn unreachable_daemon_frame(config_id: &str) -> DaemonRequestFrame {
    DaemonRequestFrame {
        plan: false,
        ops: "stats()".to_string(),
        presentation: None,
        presentation_per_op: None,
        namespace: "test".to_string(),
        actor_id: None,
        process_ref: None,
        visible_namespaces: Vec::new(),
        config_id: config_id.to_string(),
        protocol_version: PROTOCOL_VERSION,
        probe_only: false,
        metrics_only: false,
        format: None,
        format_per_op: None,
        from_wire: false,
        request_id: None,
    }
}

#[test]
fn socket_connect_error_classification_distinguishes_absence_from_denial() {
    for error_code in [libc::ENOENT, libc::ECONNREFUSED] {
        assert!(matches!(
            classify_socket_connect_error(std::io::Error::from_raw_os_error(error_code)),
            ForwardOutcome::NoSocket
        ));
    }

    for error_code in [libc::EACCES, libc::EPERM] {
        match classify_socket_connect_error(std::io::Error::from_raw_os_error(error_code)) {
            ForwardOutcome::Unreachable {
                kind,
                os_error_code,
            } => {
                assert_eq!(kind, std::io::ErrorKind::PermissionDenied);
                assert_eq!(os_error_code, Some(error_code));
            }
            _ => panic!("EACCES/EPERM must be unreachable, never safe-to-recover NoSocket"),
        }
    }
}

/// A frame-size refusal happens before the first socket write and cannot
/// be repaired by reconnecting, killing a peer, or spawning a daemon.
/// The unbounded per-op override represents the old caller path: `ops`
/// itself is capped separately before forwarding.
#[tokio::test]
#[serial]
async fn oversized_request_frame_is_terminal_before_recovery() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }

    clear_daemon_env();
    reset_counters();
    reset_fallback_counters();
    let _cleanup = RecoveryTestGuard::new();
    let dir = tempfile::tempdir().expect("tempdir");
    std::env::set_var("KHIVE_SOCKET", dir.path().join("khived.sock"));
    std::env::set_var("KHIVE_PID", dir.path().join("khived.pid"));
    std::env::set_var("KHIVE_LOCK", dir.path().join("khived.recovery.lock"));

    let mut frame = unreachable_daemon_frame("oversized-request-test");
    frame.format_per_op = Some(vec![Some("x".repeat(MAX_FRAME_BYTES))]);
    let bytes = serde_json::to_vec(&frame).expect("serialize frame").len();
    assert!(bytes > MAX_FRAME_BYTES);
    assert!(matches!(
        try_forward_inner(&frame).await,
        ForwardOutcome::RequestTooLarge { bytes: actual } if actual == bytes
    ));

    let spawn_attempts = std::sync::atomic::AtomicUsize::new(0);
    let spawn = || {
        spawn_attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Err(std::io::Error::other("oversized request must not spawn"))
    };
    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        forward_or_spawn_with(&frame, &spawn),
    )
    .await
    .expect("deterministic pre-write refusal must not wait for daemon recovery");
    match outcome {
        Some(Err(error)) => {
            assert!(error.message.contains("request too large"));
            let data = error.data.expect("structured frame cap error");
            assert_eq!(data["code"], "request_frame_size_limit");
            assert_eq!(data["domain_disposition"], "not_committed");
            assert_eq!(data["frame_bytes"].as_u64(), Some(bytes as u64));
            assert_eq!(
                data["max_frame_bytes"].as_u64(),
                Some(MAX_FRAME_BYTES as u64)
            );
        }
        other => panic!("oversized frame must be a terminal request error: {other:?}"),
    }
    assert_eq!(spawn_attempts.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert_eq!(KILL_COUNT.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert_eq!(fallback_count(FallbackReason::NoSocket), 0);
}

#[tokio::test]
#[serial]
async fn permission_denied_socket_fails_without_lifecycle_or_local_fallback() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }

    clear_daemon_env();
    reset_counters();
    reset_fallback_counters();
    let _cleanup = RecoveryTestGuard::new();
    let dir = tempfile::tempdir().expect("tempdir");
    std::env::set_var("KHIVE_SOCKET", dir.path().join("khived.sock"));
    std::env::set_var("KHIVE_PID", dir.path().join("khived.pid"));
    std::env::set_var("KHIVE_LOCK", dir.path().join("khived.recovery.lock"));
    std::env::set_var(
        "KHIVE_RECOVERER_LOCK",
        dir.path().join("khived.recoverer.lock"),
    );
    FORCED_CONNECT_ERROR.store(libc::EPERM, std::sync::atomic::Ordering::SeqCst);

    let config_id = crate::server::compute_config_id(&memory_runtime_config(), None);
    let result = forward_or_spawn(&unreachable_daemon_frame(&config_id)).await;

    assert_eq!(
        KILL_COUNT.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "an unreachable socket must never trigger a kill"
    );
    assert_eq!(
        SPAWN_COUNT.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "an unreachable socket must never trigger a spawn"
    );
    assert_eq!(
        fallback_total(),
        0,
        "an unreachable socket must return an error, not local fallback"
    );
    match result {
        Some(Err(error)) => {
            assert!(
                error.message.contains("cannot reach daemon socket"),
                "the caller-visible error must name the unreachable socket: {}",
                error.message
            );
            let data = error.data.as_ref().expect("unreachable error data");
            assert_eq!(data["reason"], "daemon_unreachable");
            assert_eq!(data["os_error_kind"], "PermissionDenied");
            assert_eq!(data["os_error_code"], libc::EPERM);
        }
        other => {
            panic!("unreachable must be Some(Err), never None/local fallback or success: {other:?}")
        }
    }
}

#[tokio::test]
#[serial]
async fn stale_socket_connection_refused_remains_safe_to_recover() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }

    clear_daemon_env();
    let dir = tempfile::tempdir().expect("tempdir");
    let sock = dir.path().join("khived.sock");
    std::env::set_var("KHIVE_SOCKET", &sock);

    {
        let listener = tokio::net::UnixListener::bind(&sock).expect("bind stale socket");
        drop(listener);
    }
    assert!(sock.exists(), "dropped listener must leave a stale socket");

    let config_id = crate::server::compute_config_id(&memory_runtime_config(), None);
    assert!(matches!(
        try_forward_inner(&unreachable_daemon_frame(&config_id)).await,
        ForwardOutcome::NoSocket
    ));

    clear_daemon_env();
}

struct RespawnDisclosureFixture {
    original_home: Option<std::ffi::OsString>,
    _home: tempfile::TempDir,
    sentinel: &'static str,
}

fn daemon_script_fixture(dir: &tempfile::TempDir, name: &str, body: &str) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;

    let path = dir.path().join(name);
    std::fs::write(&path, body).expect("write daemon executable fixture");
    let mut permissions = std::fs::metadata(&path)
        .expect("read daemon executable fixture metadata")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&path, permissions)
        .expect("make daemon executable fixture executable");
    path
}

/// Exercise the same default lock and stderr producers as daemon recovery.
/// The outer sentinel is also owned by this test, so isolation controls can
/// fail before a writer is reached without ever touching the caller's HOME.
#[test]
#[serial]
fn private_home_child_contains_real_daemon_artifacts() {
    const WITNESS: &str = "KHIVE_MCP_PRIVATE_HOME_WITNESS";
    if std::env::var_os(WITNESS).is_none() {
        let sentinel = tempfile::tempdir().expect("outer sentinel home");
        let marker = sentinel.path().join("sentinel");
        std::fs::write(&marker, b"parent home unchanged").expect("sentinel marker");
        let thread = std::thread::current();
        let test_name = thread.name().expect("named witness test");
        let command = || {
            let mut command =
                std::process::Command::new(std::env::current_exe().expect("witness executable"));
            command
                .args(["--exact", test_name, "--nocapture", "--test-threads=1"])
                .env("HOME", sentinel.path())
                .env("USERPROFILE", sentinel.path())
                .env(WITNESS, "1")
                .env_remove("KHIVE_MCP_PRIVATE_HOME_TEST")
                .env_remove("KHIVE_MCP_PRIVATE_HOME_PATH")
                .env_remove("KHIVE_MCP_PRIVATE_HOME_PARENT_PID")
                .env_remove(crate::test_isolation::PARENT_HOME)
                .current_dir(sentinel.path());
            command
        };
        let output = command().output().expect("run private-home witness");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "MCP_ISOLATION_WITNESS_SUCCEEDS: {stdout}\n{stderr}"
        );
        assert!(
            stdout.contains("running 1 test") && stdout.contains("1 passed; 0 failed"),
            "MCP_ISOLATION_WITNESS_EXACTLY_ONE_TEST: {stdout}\n{stderr}"
        );
        // A marker alone must never authorize running a producer in the
        // outer process/home. The exact-name check precedes every writer.
        let forged = command()
            .env("KHIVE_MCP_PRIVATE_HOME_TEST", "not_this_test")
            .output()
            .expect("run forged-marker witness");
        assert!(
            !forged.status.success()
                && String::from_utf8_lossy(&forged.stderr).contains("MCP_CHILD_EXACT_TEST"),
            "MCP_FORGED_CHILD_MARKER_REJECTED"
        );
        assert_eq!(
            std::fs::read(&marker).expect("read sentinel marker"),
            b"parent home unchanged",
            "MCP_OUTER_SENTINEL_BYTES_UNCHANGED"
        );
        assert_eq!(
            std::fs::read_dir(sentinel.path())
                .expect("read outer home")
                .count(),
            1,
            "MCP_OUTER_SENTINEL_HAS_NO_DAEMON_ARTIFACTS"
        );
        return;
    }

    // Refuse an unisolated process before any filesystem writes.
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }
    let expected_home = std::path::PathBuf::from(
        std::env::var_os("KHIVE_MCP_PRIVATE_HOME_PATH").expect("MCP_ISOLATION_CHILD_REQUIRED"),
    );
    let home = std::path::PathBuf::from(std::env::var_os("HOME").expect("child HOME"));
    let parent_home = std::path::PathBuf::from(
        std::env::var_os(crate::test_isolation::PARENT_HOME).expect("outer sentinel HOME"),
    );
    assert_eq!(home, expected_home, "MCP_ISOLATION_HOME_IS_PRIVATE");
    assert_ne!(
        home, parent_home,
        "MCP_ISOLATION_HOME_DIFFERS_FROM_SENTINEL"
    );
    assert!(
        parent_home.join("sentinel").is_file(),
        "MCP_ISOLATION_OUTER_SENTINEL_EXISTS"
    );
    clear_daemon_env();
    let recovery_path = khive_runtime::daemon::lock_path();
    let recoverer_path = khive_runtime::daemon::recoverer_lock_path();
    assert_eq!(recovery_path, home.join(".khive/khived.recovery.lock"));
    assert_eq!(recoverer_path, home.join(".khive/khived.recoverer.lock"));
    drop(khive_runtime::daemon::acquire_daemon_boot_guard().expect("real recovery lock"));
    drop(
        khive_runtime::daemon::try_acquire_recoverer_lock_until(
            std::time::Instant::now() + std::time::Duration::from_secs(1),
        )
        .expect("real recoverer lock acquisition")
        .expect("uncontended recoverer lock"),
    );
    let fixture = tempfile::tempdir().expect("writer executable fixture");
    let exe = daemon_script_fixture(
        &fixture,
        "write-stderr.sh",
        "#!/bin/sh\nprintf 'MCP_PRIVATE_LOG_WITNESS\\n' >&2\n",
    );
    let status = spawn_daemon_with_exe(&exe)
        .expect("spawn real daemon log writer")
        .wait()
        .expect("reap daemon log writer");
    assert!(status.success(), "MCP_PRIVATE_LOG_WRITER_SUCCEEDS");
    assert!(recovery_path.is_file(), "MCP_PRIVATE_RECOVERY_LOCK_EXISTS");
    assert!(
        recoverer_path.is_file(),
        "MCP_PRIVATE_RECOVERER_LOCK_EXISTS"
    );
    assert_eq!(
        std::fs::read_to_string(home.join(".khive/logs/khived.log")).expect("read real daemon log"),
        "MCP_PRIVATE_LOG_WITNESS\n",
        "MCP_PRIVATE_DAEMON_LOG_CONTAINS_REAL_STDERR"
    );
    assert!(
        !parent_home.join(".khive").exists(),
        "MCP_OUTER_SENTINEL_HAS_NO_DAEMON_ARTIFACTS"
    );
}

/// The config path threaded through `forward_or_spawn_with_config_and_packs` must
/// actually appear on the spawned daemon's command line; a script
/// fixture records its argv so the assertion observes the real child
/// invocation (`crates/kkernel/src/exec.rs`'s spy seam proves the exec
/// side hands the path over; this side proves it reaches `argv`).
#[test]
#[serial]
fn spawn_daemon_with_exe_and_config_appends_config_flag_to_command_line() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let record = dir.path().join("argv.txt");
    let exe = daemon_script_fixture(
        &dir,
        "record-argv.sh",
        &format!("#!/bin/sh\nprintf '%s\\n' \"$*\" > {}\n", record.display()),
    );

    let config_path = dir.path().join("selected.toml");
    let mut child = spawn_daemon_with_exe_and_config(&exe, Some(&config_path), None, None)
        .expect("spawn argv-recording fixture");
    let status = child.wait().expect("wait for argv-recording fixture");
    assert!(status.success(), "fixture must exit 0: {status}");

    let recorded = std::fs::read_to_string(&record).expect("read recorded argv");
    assert_eq!(
        recorded.trim_end(),
        format!(
            "mcp --daemon --lifetime demand --config {}",
            config_path.display()
        ),
        "the explicit config selection must reach the daemon command line"
    );
}

/// The pack list threaded through `forward_or_spawn_with_config_and_packs` must
/// reach the spawned daemon's command line as explicit `--pack` flags —
/// the fix for the auto-spawn dropping a client's resolved `KHIVE_PACKS`
/// (or `--pack`-flag, or config-file `[runtime].packs`) selection when it
/// spawns a fresh daemon (khive-oss#1941).
#[test]
#[serial]
fn spawn_daemon_with_exe_and_config_appends_pack_flags_to_command_line() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let record = dir.path().join("argv.txt");
    let exe = daemon_script_fixture(
        &dir,
        "record-argv.sh",
        &format!("#!/bin/sh\nprintf '%s\\n' \"$*\" > {}\n", record.display()),
    );

    let packs = vec!["kg".to_string(), "gtd".to_string(), "formal".to_string()];
    let mut child = spawn_daemon_with_exe_and_config(&exe, None, None, Some(&packs))
        .expect("spawn argv-recording fixture");
    let status = child.wait().expect("wait for argv-recording fixture");
    assert!(status.success(), "fixture must exit 0: {status}");

    let recorded = std::fs::read_to_string(&record).expect("read recorded argv");
    assert_eq!(
        recorded.trim_end(),
        "mcp --daemon --lifetime demand --pack kg --pack gtd --pack formal",
        "the caller's resolved pack set must reach the daemon command line as --pack flags"
    );
}

/// A `None` pack list (the default, backward-compatible shape used by
/// bare `forward_or_spawn`/`spawn_daemon`) must append no `--pack` flags
/// at all — the spawned daemon falls through to its own env/config
/// resolution exactly as before this fix.
#[test]
#[serial]
fn spawn_daemon_with_exe_and_config_omits_pack_flags_when_none() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let record = dir.path().join("argv.txt");
    let exe = daemon_script_fixture(
        &dir,
        "record-argv.sh",
        &format!("#!/bin/sh\nprintf '%s\\n' \"$*\" > {}\n", record.display()),
    );

    let mut child = spawn_daemon_with_exe_and_config(&exe, None, None, None)
        .expect("spawn argv-recording fixture");
    let status = child.wait().expect("wait for argv-recording fixture");
    assert!(status.success(), "fixture must exit 0: {status}");

    let recorded = std::fs::read_to_string(&record).expect("read recorded argv");
    assert_eq!(
        recorded.trim_end(),
        "mcp --daemon --lifetime demand",
        "no packs supplied must mean no --pack flags on the daemon command line"
    );
}

/// An accepted `--db :memory:` override must follow the spawn: without
/// `--db :memory:` on the daemon's own command line, a fresh daemon
/// would bind the config's declared persistent backend files, the exact
/// inversion of the operator's ephemeral invocation.
#[test]
#[serial]
fn spawn_daemon_with_exe_and_config_appends_memory_db_flag_to_command_line() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let record = dir.path().join("argv.txt");
    let exe = daemon_script_fixture(
        &dir,
        "record-argv.sh",
        &format!("#!/bin/sh\nprintf '%s\\n' \"$*\" > {}\n", record.display()),
    );

    let config_path = dir.path().join("selected.toml");
    let mut child =
        spawn_daemon_with_exe_and_config(&exe, Some(&config_path), Some(":memory:"), None)
            .expect("spawn argv-recording fixture");
    let status = child.wait().expect("wait for argv-recording fixture");
    assert!(status.success(), "fixture must exit 0: {status}");

    let recorded = std::fs::read_to_string(&record).expect("read recorded argv");
    assert_eq!(
        recorded.trim_end(),
        format!(
            "mcp --daemon --lifetime demand --config {} --db :memory:",
            config_path.display()
        ),
        "the ephemeral :memory: override must reach the daemon command line"
    );
}

/// A concrete override handed to the spawn seam must reach the spawned
/// daemon's command line: this is the single-backend case (no
/// `[[backends]]` declared), where the fresh daemon has no
/// config-declared database path and would otherwise bind
/// `$HOME/.khive/khive.db` instead of the operator's override — a
/// `config_id` mismatch against the client's override-anchored frame.
/// The redundant-concrete multi-backend case never reaches this
/// function: the caller (`run_exec_inline_with_forward` in
/// `crates/kkernel/src/exec.rs`) withholds the override there because
/// the frame's fingerprint has already been normalized to the
/// no-override anchor (`normalize_redundant_db_override_with_source`).
#[test]
#[serial]
fn spawn_daemon_with_exe_and_config_forwards_concrete_db_flag_to_command_line() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let record = dir.path().join("argv.txt");
    let exe = daemon_script_fixture(
        &dir,
        "record-argv.sh",
        &format!("#!/bin/sh\nprintf '%s\\n' \"$*\" > {}\n", record.display()),
    );

    let mut child = spawn_daemon_with_exe_and_config(&exe, None, Some("/tmp/main.db"), None)
        .expect("spawn argv-recording fixture");
    let status = child.wait().expect("wait for argv-recording fixture");
    assert!(status.success(), "fixture must exit 0: {status}");

    let recorded = std::fs::read_to_string(&record).expect("read recorded argv");
    assert_eq!(
        recorded.trim_end(),
        "mcp --daemon --lifetime demand --db /tmp/main.db",
        "the concrete override handed to the spawn seam must reach the daemon command line"
    );
}

/// A writer that momentarily still owns the fixture inode must not turn
/// the argv-forwarding family into an ETXTBSY flake. The production spawn
/// seam retries only ExecutableFileBusy, so holding this file open for
/// writing forces the exact failure class reported by coverage CI.
#[test]
#[serial]
fn spawn_daemon_retries_a_transient_executable_file_busy_error() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let exe = daemon_script_fixture(&dir, "temporarily-busy.sh", "#!/bin/sh\nexit 0\n");
    let writer = std::fs::OpenOptions::new()
        .write(true)
        .open(&exe)
        .expect("hold executable fixture open for writing");
    let release = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(15));
        drop(writer);
    });

    let mut child = spawn_daemon_with_exe_and_config(&exe, None, None, None)
        .expect("transient ETXTBSY must be retried");
    release.join().expect("fixture writer release thread");
    let status = child.wait().expect("wait for executable fixture");
    assert!(status.success(), "fixture must exit 0: {status}");
}

/// Blank out `"..."` string-literal contents (escapes and
/// backslash-newline continuations included) so a spawn-seam name
/// mentioned in an error message or doc comment can never read as a
/// call.
fn strip_string_literals_for_census(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_string = false;
    for line in text.lines() {
        let mut chars = line.chars();
        while let Some(c) = chars.next() {
            if in_string {
                if c == '\\' {
                    out.push(' ');
                    if chars.next().is_some() {
                        out.push(' ');
                    }
                    continue;
                }
                if c == '"' {
                    in_string = false;
                    out.push('"');
                } else {
                    out.push(' ');
                }
            } else {
                if c == '"' {
                    in_string = true;
                }
                out.push(c);
            }
        }
        out.push('\n');
    }
    out
}

/// `true` if `text` contains a call to `name` — `name` immediately
/// followed by `(` (optional whitespace between, including newlines —
/// `rustfmt` is free to break a long call onto its own line), a
/// non-identifier character (or start of text) before it, and not
/// inside a string literal. Anchoring both boundaries matters here
/// specifically because `spawn_daemon_with_exe(` is a prefix-shaped
/// substring of `spawn_daemon_with_exe_and_config(` up to the
/// `_and_config` suffix — a plain `contains` check would still tell
/// them apart by luck (the character after `exe` differs), but a
/// boundary check makes that non-collision load-bearing instead of
/// incidental.
fn calls_name_for_census(text: &str, name: &str) -> bool {
    fn is_ident_byte(b: u8) -> bool {
        b.is_ascii_alphanumeric() || b == b'_'
    }
    let text = strip_string_literals_for_census(text);
    let bytes = text.as_bytes();
    let mut search_from = 0usize;
    while let Some(rel) = text[search_from..].find(name) {
        let idx = search_from + rel;
        let before_ok = idx == 0 || !is_ident_byte(bytes[idx - 1]);
        let after = idx + name.len();
        let mut j = after;
        while j < bytes.len() && bytes[j].is_ascii_whitespace() {
            j += 1;
        }
        let after_ok = j < bytes.len() && bytes[j] == b'(';
        let is_definition = text[..idx].ends_with("fn ");
        if before_ok && after_ok && !is_definition {
            return true;
        }
        search_from = idx + 1;
    }
    false
}

/// Regression for a scanner that only tolerated a space/tab between a
/// seam name and its `(` — see the identical fix and rationale for
/// `khive-runtime`'s `calls_name`.
#[test]
fn calls_name_for_census_matches_across_a_newline_before_the_parenthesis() {
    let text = "fn wraps_it() {\n    spawn_daemon_with_exe\n        (exe)\n}";
    assert!(calls_name_for_census(text, "spawn_daemon_with_exe"));
}

/// The name of the function whose signature starts at `sig_line`
/// (already stripped of leading whitespace), if any.
fn fn_name_from_signature_for_census(sig_line: &str) -> Option<&str> {
    let mut rest = sig_line;
    for prefix in ["pub(crate) ", "pub(super) ", "pub "] {
        if let Some(stripped) = rest.strip_prefix(prefix) {
            rest = stripped;
        }
    }
    let rest = rest
        .strip_prefix("async fn ")
        .or_else(|| rest.strip_prefix("fn "))?;
    Some(rest.split(['(', '<', ' ']).next().unwrap_or(rest))
}

/// Recursively collect every `.rs` file under `dir` into `out`.
fn collect_rust_files_for_census(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.is_dir() {
            collect_rust_files_for_census(&path, out);
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

/// `spawn_daemon_with_exe_and_config` and its thin wrapper
/// `spawn_daemon_with_exe` are both module-private — visible only
/// inside the daemon module and its descendants — so unlike the
/// config-ledger seam (`with_event_store`, `pub` and reachable from any
/// crate in the workspace), the real population for this census is
/// bounded to `daemon.rs`, its `daemon_tests.rs` implementation, and the
/// `daemon/` submodule directory, not the whole workspace. This includes
/// the test implementation explicitly and walks the directory so a future
/// submodule under `daemon/` is covered without widening the scan.
fn daemon_module_sources() -> Vec<(std::path::PathBuf, String)> {
    let this_file = std::path::PathBuf::from(file!());
    let daemon_rs = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/daemon.rs");
    let daemon_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/daemon");
    let daemon_tests_rs =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/daemon_tests.rs");
    let mut files = vec![daemon_rs, daemon_tests_rs];
    if daemon_dir.is_dir() {
        collect_rust_files_for_census(&daemon_dir, &mut files);
    }
    let _ = this_file; // `file!()` is relative; CARGO_MANIFEST_DIR-joined paths are the source of truth.
    files
        .into_iter()
        .filter_map(|path| {
            let text = std::fs::read_to_string(&path).ok()?;
            Some((path, text))
        })
        .collect()
}

/// Every direct test caller of the spawn seam — `daemon.rs`,
/// `daemon_tests.rs`, and the `daemon/` submodules, the only places
/// `spawn_daemon_with_exe[_and_config]` are visible from — mutates `SPAWN_COUNT`,
/// so it must share the default serial group with tests that reset and
/// assert that counter.
#[test]
fn direct_spawn_seam_test_callers_are_serialized() {
    let sources = daemon_module_sources();
    assert!(
        !sources.is_empty(),
        "the daemon-module source scan found no .rs files under \
             crates/khive-mcp/src/daemon.rs, src/daemon_tests.rs or src/daemon/; the census's own file walk \
             is broken, not the population it walks"
    );

    let spawn_calls = ["spawn_daemon_with_exe_and_config", "spawn_daemon_with_exe"];
    let mut candidate_count = 0usize;
    let mut offenders = Vec::new();

    for (path, text) in &sources {
        let lines: Vec<&str> = text.lines().collect();
        let test_starts: Vec<usize> = lines
            .iter()
            .enumerate()
            .filter(|(_, line)| {
                let trimmed = line.trim();
                trimmed == "#[test]" || trimmed.starts_with("#[tokio::test")
            })
            .map(|(index, _)| index)
            .collect();

        for (index, start) in test_starts.iter().copied().enumerate() {
            let end = test_starts.get(index + 1).copied().unwrap_or(lines.len());
            let span = &lines[start..end];
            let span_text = span.join("\n");
            let matched = spawn_calls
                .iter()
                .find(|seam| calls_name_for_census(&span_text, seam));
            let Some(matched) = matched else {
                continue;
            };
            candidate_count += 1;

            if !span.iter().any(|line| line.trim() == "#[serial]") {
                let signature_offset = span
                    .iter()
                    .position(|line| fn_name_from_signature_for_census(line.trim_start()).is_some())
                    .expect("test span has a function signature");
                let name = fn_name_from_signature_for_census(span[signature_offset].trim_start())
                    .unwrap_or("<unknown>");
                offenders.push(format!(
                    "{}:{name} (reaches the spawn seam via `{matched}`)",
                    path.display()
                ));
            }
        }
    }

    assert!(
        candidate_count > 0,
        "census found zero direct spawn-seam test candidates under the daemon \
             module ({} source files) — the scan is broken, not the population it \
             should have found (this file's own argv-forwarding tests are known \
             direct callers)",
        sources.len()
    );
    assert!(
        offenders.is_empty(),
        "tests that directly call the SPAWN_COUNT-mutating spawn seam must use \
             #[serial]; offenders: {offenders:?}"
    );
}

/// The helper behind the retry above must actually retry the exact
/// number of times ETXTBSY is injected, then succeed on the next
/// attempt — a fixture that merely holds a file open (as the test above
/// does) never proves an initial ETXTBSY was observed or counts
/// attempts, so it passes even against a no-retry implementation on a
/// filesystem that permits executing a writable-open file.
#[test]
fn spawn_retrying_executable_busy_retries_exact_count_then_succeeds() {
    let attempts = std::sync::atomic::AtomicUsize::new(0);
    let result: std::io::Result<()> =
        spawn_retrying_executable_busy(&EXECUTABLE_BUSY_BACKOFF_MS, || {
            let n = attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if n < 2 {
                Err(std::io::Error::from(std::io::ErrorKind::ExecutableFileBusy))
            } else {
                Ok(())
            }
        });

    assert!(result.is_ok(), "must succeed once ETXTBSY stops recurring");
    assert_eq!(
        attempts.load(std::sync::atomic::Ordering::SeqCst),
        3,
        "must retry exactly until success: 2 injected ETXTBSY failures + 1 success"
    );
}

/// A non-ETXTBSY error must never be retried, even though the backoff
/// budget has room left.
#[test]
fn spawn_retrying_executable_busy_does_not_retry_other_errors() {
    let attempts = std::sync::atomic::AtomicUsize::new(0);
    let result: std::io::Result<()> =
        spawn_retrying_executable_busy(&EXECUTABLE_BUSY_BACKOFF_MS, || {
            attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
        });

    assert!(
        result.is_err(),
        "a non-ETXTBSY error must surface, not be swallowed"
    );
    assert_eq!(
        attempts.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "a non-ETXTBSY error must not retry"
    );
}

#[derive(Clone, Default)]
struct CapturedLog(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for CapturedLog {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .expect("captured log mutex poisoned")
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturedLog {
    type Writer = CapturedLog;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

impl CapturedLog {
    fn contents(&self) -> String {
        String::from_utf8(self.0.lock().expect("captured log mutex poisoned").clone())
            .expect("tracing output is UTF-8")
    }
}

fn capture_sync_events(run: impl FnOnce()) -> String {
    let captured = CapturedLog::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(captured.clone())
        .with_ansi(false)
        .without_time()
        .finish();
    tracing::subscriber::with_default(subscriber, run);
    captured.contents()
}

#[test]
fn captured_log_flush_is_a_noop() {
    use std::io::Write;

    let mut captured = CapturedLog::default();
    captured
        .write_all(b"respawn-event")
        .expect("write captured event");
    captured.flush().expect("flush captured event");
    assert_eq!(captured.contents(), "respawn-event");
}

impl RespawnDisclosureFixture {
    fn new(sentinel: &'static str) -> Self {
        let original_home = std::env::var_os("HOME");
        let home = tempfile::tempdir().expect("isolated HOME tempdir");
        std::env::set_var("HOME", home.path());
        let log_path = daemon_log_path().expect("HOME resolves daemon log path");
        std::fs::create_dir_all(log_path.parent().expect("daemon log has parent"))
            .expect("create daemon log directory");
        std::fs::write(&log_path, format!("{sentinel}\n")).expect("seed daemon log sentinel");
        Self {
            original_home,
            _home: home,
            sentinel,
        }
    }

    fn assert_output_is_sanitized(&self, output_name: &str, output: &str) {
        let executable = std::env::current_exe()
            .expect("resolve current test executable")
            .display()
            .to_string();
        assert!(
            !output.contains(self.sentinel),
            "shared daemon log content must not reach {output_name}: {output}"
        );
        assert!(
            !output.contains(&executable),
            "absolute daemon executable path must not reach {output_name}: {output}"
        );
    }

    fn assert_caller_output_is_sanitized(&self, error: &McpError) {
        let caller_output = serde_json::to_string(error).expect("serialize caller MCP error");
        self.assert_output_is_sanitized("the caller", &caller_output);
        assert!(
            caller_output.contains("respawn_failed"),
            "caller must receive the stable respawn_failed code: {caller_output}"
        );
        assert!(
            caller_output.contains("make local"),
            "caller must receive safe remediation text: {caller_output}"
        );
    }
}

async fn forward_with_exe_and_captured_events(
    frame: &DaemonRequestFrame,
    exe: &std::path::Path,
) -> (Option<Result<String, McpError>>, String) {
    let captured = CapturedLog::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(captured.clone())
        .with_ansi(false)
        .without_time()
        .finish();
    let subscriber_guard = tracing::subscriber::set_default(subscriber);
    let output = forward_or_spawn_with_exe(frame, exe).await;
    drop(subscriber_guard);
    let events = captured.contents();
    (output, events)
}

impl Drop for RespawnDisclosureFixture {
    fn drop(&mut self) {
        match self.original_home.take() {
            Some(home) => std::env::set_var("HOME", home),
            None => std::env::remove_var("HOME"),
        }
    }
}

#[test]
#[serial]
fn respawn_disclosure_fixture_restores_absent_home() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }

    let home = std::env::var_os("HOME").expect("test process has HOME");
    std::env::remove_var("HOME");
    {
        let _fixture =
            RespawnDisclosureFixture::new("KHIVE_RESPAWN_LOG_SENTINEL_ABSENT_HOME_62cc1aeb13");
        assert!(std::env::var_os("HOME").is_some());
    }
    assert!(std::env::var_os("HOME").is_none());
    std::env::set_var("HOME", home);
}

#[tokio::test]
#[serial]
async fn forward_or_spawn_surfaces_loud_error_when_respawn_confirmed_dead_non_strict() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }

    clear_daemon_env();
    reset_fallback_counters();
    let dir = tempfile::tempdir().expect("tempdir");
    std::env::set_var("KHIVE_SOCKET", dir.path().join("khived.sock"));
    std::env::set_var("KHIVE_PID", dir.path().join("khived.pid"));
    std::env::set_var("KHIVE_LOCK", dir.path().join("khived.recovery.lock"));
    std::env::remove_var("KHIVE_NO_DAEMON");
    std::env::remove_var("KHIVE_DAEMON_STRICT");
    let disclosure =
        RespawnDisclosureFixture::new("KHIVE_RESPAWN_LOG_SENTINEL_NON_STRICT_4d9813b72e");
    let exe = daemon_script_fixture(&dir, "exits-before-bind", "#!/bin/sh\nexit 23\n");

    let frame = unreachable_daemon_frame(CFG);
    let (out, events) = forward_with_exe_and_captured_events(&frame, &exe).await;
    disclosure.assert_output_is_sanitized("bridge tracing events", &events);
    assert!(
        events.contains("reason=\"respawn_failed\""),
        "trace must retain the stable reason code: {events}"
    );
    assert!(
        events.contains("failure_category=\"exited_before_bind\""),
        "trace must classify the confirmed failure without raw detail: {events}"
    );

    match out {
        Some(Err(error)) => {
            disclosure.assert_caller_output_is_sanitized(&error);
            let data = error.data.as_ref().expect("respawn error data");
            assert_eq!(data["reason"], "respawn_failed");
            assert!(data.get(STRICT_FALLBACK_MARKER).is_none());
            let message = &error.message;
            assert!(
                message.contains("respawn failed"),
                "must name the respawn failure specifically, not a generic \
                     fallback: {message}"
            );
            assert!(
                message.contains("make local"),
                "must point the operator at the fix: {message}"
            );
        }
        other => panic!(
            "a respawn attempt confirmed dead must surface loudly even in \
                 non-strict mode (#898) instead of completing the request via \
                 silent local dispatch, got {other:?}"
        ),
    }
    // #898's loud respawn-failure path bypasses the ordinary
    // fallback/telemetry machinery entirely — a confirmed respawn failure
    // is never the legitimate ADR-049 no-daemon case that telemetry
    // exists to count.
    assert_eq!(fallback_count(FallbackReason::NoSocket), 0);

    reset_fallback_counters();
    clear_daemon_env();
    std::env::remove_var("KHIVE_LOCK");
}

#[tokio::test]
#[serial]
async fn forward_or_spawn_surfaces_loud_error_when_respawn_confirmed_dead_strict() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }

    clear_daemon_env();
    reset_fallback_counters();
    let dir = tempfile::tempdir().expect("tempdir");
    std::env::set_var("KHIVE_SOCKET", dir.path().join("khived.sock"));
    std::env::set_var("KHIVE_PID", dir.path().join("khived.pid"));
    std::env::set_var("KHIVE_LOCK", dir.path().join("khived.recovery.lock"));
    std::env::remove_var("KHIVE_NO_DAEMON");
    std::env::set_var("KHIVE_DAEMON_STRICT", "1");
    let disclosure = RespawnDisclosureFixture::new("KHIVE_RESPAWN_LOG_SENTINEL_STRICT_7ac60d5391");
    let exe = daemon_script_fixture(&dir, "exits-before-bind", "#!/bin/sh\nexit 23\n");

    let frame = unreachable_daemon_frame(CFG);
    let (out, events) = forward_with_exe_and_captured_events(&frame, &exe).await;
    disclosure.assert_output_is_sanitized("strict-mode bridge tracing events", &events);
    assert!(
        events.contains("reason=\"respawn_failed\""),
        "strict-mode trace must retain the stable reason code: {events}"
    );
    assert!(
        events.contains("failure_category=\"exited_before_bind\""),
        "strict-mode trace must classify the failure without raw detail: {events}"
    );

    match out {
        Some(Err(error)) => {
            disclosure.assert_caller_output_is_sanitized(&error);
            let data = error.data.as_ref().expect("strict respawn error data");
            assert_eq!(data["reason"], "respawn_failed");
            assert_eq!(data[STRICT_FALLBACK_MARKER], true);
            let message = &error.message;
            assert!(
                message.contains("respawn failed"),
                "strict mode must still surface the specific respawn-failure \
                     diagnosis, not the generic no_socket reason: {message}"
            );
        }
        other => panic!(
            "KHIVE_DAEMON_STRICT=1 must reject the request when the daemon is \
                 unreachable, got {other:?}"
        ),
    }
    // Strict mode changes nothing here: #898's loud path is unconditional
    // and never reaches record_fallback/fallback_or_reject at all.
    assert_eq!(fallback_count(FallbackReason::NoSocket), 0);

    reset_fallback_counters();
    clear_daemon_env();
    std::env::remove_var("KHIVE_LOCK");
    std::env::remove_var("KHIVE_DAEMON_STRICT");
}

// ── daemon socket round-trip (env-mutating → serial) ─────────────────────

#[cfg(unix)]
#[tokio::test]
#[serial]
async fn forward_or_spawn_with_injected_exe_sanitizes_spawn_error_without_local_fallback() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }

    clear_daemon_env();
    reset_fallback_counters();
    let dir = tempfile::tempdir().expect("tempdir");
    std::env::set_var("KHIVE_SOCKET", dir.path().join("khived.sock"));
    std::env::set_var("KHIVE_PID", dir.path().join("khived.pid"));
    std::env::set_var("KHIVE_LOCK", dir.path().join("khived.recovery.lock"));
    std::env::remove_var("KHIVE_NO_DAEMON");
    std::env::remove_var("KHIVE_DAEMON_STRICT");
    let disclosure =
        RespawnDisclosureFixture::new("KHIVE_RESPAWN_LOG_SENTINEL_SPAWN_ERROR_f6a81b23c9");
    let exe = dir.path().join("not-executable");
    std::fs::write(&exe, "not an executable").expect("write non-executable fixture");

    let frame = unreachable_daemon_frame(CFG);
    let (out, events) = forward_with_exe_and_captured_events(&frame, &exe).await;
    disclosure.assert_output_is_sanitized("spawn-error bridge tracing events", &events);
    assert!(
        events.contains("reason=\"respawn_failed\""),
        "trace must retain the stable reason code: {events}"
    );
    assert!(
        events.contains("failure_category=\"spawn_error\""),
        "trace must classify the process-start failure without raw detail: {events}"
    );
    assert!(
        events.contains("os_error_code=Some(13)"),
        "trace diagnostic must be the numeric permission-denied code only: {events}"
    );
    assert!(
        !events.contains("Permission denied") && !events.contains("os error"),
        "trace must not expose the raw OS error text: {events}"
    );

    match out {
        Some(Err(error)) => {
            disclosure.assert_caller_output_is_sanitized(&error);
            let caller_output = serde_json::to_string(&error).expect("serialize caller MCP error");
            assert!(
                !caller_output.contains("Permission denied") && !caller_output.contains("os error"),
                "caller must not receive the raw OS error text: {caller_output}"
            );
        }
        other => panic!(
            "a confirmed process-start failure must return respawn_failed instead of \
                 permitting local dispatch, got {other:?}"
        ),
    }
    assert_eq!(fallback_total(), 0);

    reset_fallback_counters();
    clear_daemon_env();
    std::env::remove_var("KHIVE_LOCK");
}

#[cfg(unix)]
#[tokio::test]
#[serial]
async fn forward_or_spawn_with_injected_exe_falls_back_when_child_stays_alive() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }

    clear_daemon_env();
    reset_fallback_counters();
    let dir = tempfile::tempdir().expect("tempdir");
    std::env::set_var("KHIVE_SOCKET", dir.path().join("khived.sock"));
    std::env::set_var("KHIVE_PID", dir.path().join("khived.pid"));
    std::env::set_var("KHIVE_LOCK", dir.path().join("khived.recovery.lock"));
    std::env::remove_var("KHIVE_NO_DAEMON");
    std::env::remove_var("KHIVE_DAEMON_STRICT");
    let exe = daemon_script_fixture(&dir, "still-running", "#!/bin/sh\nsleep 10\n");

    let out = forward_or_spawn_with_exe(&unreachable_daemon_frame(CFG), &exe).await;

    assert!(
        out.is_none(),
        "a live spawned child with no socket remains eligible for non-strict local fallback: {out:?}"
    );
    assert_eq!(fallback_count(FallbackReason::NoSocket), 1);
    clear_daemon_env();
    std::env::remove_var("KHIVE_LOCK");
}

#[tokio::test]
#[serial]
#[serial_test::serial(config_ledger)]
async fn daemon_round_trip_dispatches_and_enforces_config_id() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }

    clear_daemon_env();
    let dir = tempfile::tempdir().expect("tempdir");
    let sock = dir.path().join("khived.sock");
    let pid = dir.path().join("khived.pid");
    std::env::set_var("KHIVE_SOCKET", &sock);
    std::env::set_var("KHIVE_PID", &pid);
    std::env::remove_var("KHIVE_NO_DAEMON");

    let reference = make_test_server();
    let config_id = reference.config_id().to_string();
    let daemon_server = reference.clone();

    let handle = tokio::spawn(async move {
        let _ = run_daemon(daemon_server).await;
    });

    let _ready = connect_when_ready(&sock).await;
    drop(_ready);

    // (a) valid same-namespace, same-config op
    let req = DaemonRequestFrame {
        plan: false,
        ops: "stats()".to_string(),
        presentation: Some("verbose".to_string()),
        presentation_per_op: None,
        namespace: "test".to_string(),
        actor_id: None,
        process_ref: None,
        visible_namespaces: Vec::new(),
        config_id: config_id.clone(),
        protocol_version: PROTOCOL_VERSION,
        probe_only: false,
        metrics_only: false,
        format: None,
        format_per_op: None,
        from_wire: false,
        request_id: None,
    };
    let resp = exchange(&sock, &req).await;
    assert!(resp.ok, "valid op must succeed; error={:?}", resp.error);
    assert!(!resp.namespace_mismatch);
    assert!(!resp.config_mismatch);
    assert!(!resp.version_mismatch);
    assert_eq!(resp.daemon_protocol_version, PROTOCOL_VERSION);
    assert_eq!(
        resp.served_config_id.as_deref(),
        Some(config_id.as_str()),
        "daemon must echo the config it served under"
    );

    let reference_result = reference
        .dispatch_request_local(RequestParams {
            plan: None,
            ops: "stats()".to_string(),
            presentation: Some("verbose".to_string()),
            presentation_per_op: None,
            save_to: None,
            format: None,
            format_per_op: None,
            request_id: None,
        })
        .await
        .expect("local dispatch of stats() must succeed");
    let mut daemon_result: serde_json::Value = serde_json::from_str(
        resp.result
            .as_deref()
            .expect("daemon dispatch of stats() must return JSON"),
    )
    .expect("daemon stats response must be valid JSON");
    let mut local_result: serde_json::Value =
        serde_json::from_str(&reference_result).expect("local stats response must be valid JSON");
    // Usage counters are request-local. The daemon dispatch may count its
    // own audit event before serializing while the direct reference call
    // observes a different event boundary, so they are not an equivalence
    // oracle for the transport round trip exercised by this test.
    for result in [&mut daemon_result, &mut local_result] {
        if let Some(entries) = result
            .get_mut("results")
            .and_then(serde_json::Value::as_array_mut)
        {
            for entry in entries {
                if let Some(object) = entry.as_object_mut() {
                    object.remove("usage");
                }
            }
        }
    }
    assert_eq!(daemon_result, local_result);
    assert!(reference_result.contains("\"entities\""));

    // (b) ADR-096 Fork 1: a different namespace, same config_id, is no
    // longer rejected — the daemon accepts and serves the request under
    // the frame's OWN namespace ("other") over the same shared warm
    // registry, instead of setting `namespace_mismatch`.
    let other = DaemonRequestFrame {
        plan: false,
        ops: "stats()".to_string(),
        presentation: None,
        presentation_per_op: None,
        namespace: "other".to_string(),
        actor_id: None,
        process_ref: None,
        visible_namespaces: Vec::new(),
        config_id: config_id.clone(),
        protocol_version: PROTOCOL_VERSION,
        probe_only: false,
        metrics_only: false,
        format: None,
        format_per_op: None,
        from_wire: false,
        request_id: None,
    };
    let resp_other = exchange(&sock, &other).await;
    assert!(
        resp_other.ok,
        "a differently-namespaced frame with a matching config_id must be \
             served, not rejected; error={:?}",
        resp_other.error
    );
    assert!(
        !resp_other.namespace_mismatch,
        "ADR-096 Fork 1 removed the namespace_mismatch reject"
    );
    assert!(!resp_other.config_mismatch);
    assert_eq!(
        resp_other.served_config_id.as_deref(),
        Some(config_id.as_str())
    );

    // (c) same namespace but different config (e.g. a `--pack kg` client
    // hitting the broader daemon) → config_mismatch, no dispatch. The
    // config_id reject stays hard under ADR-096 Fork 1 — only the
    // namespace reject was softened.
    let mismatched_config = DaemonRequestFrame {
        plan: false,
        ops: "stats()".to_string(),
        presentation: None,
        presentation_per_op: None,
        namespace: "test".to_string(),
        actor_id: None,
        process_ref: None,
        visible_namespaces: Vec::new(),
        config_id: "packs=[kg];db=:memory:;embed=none;extra=[];backend=main".to_string(),
        protocol_version: PROTOCOL_VERSION,
        probe_only: false,
        metrics_only: false,
        format: None,
        format_per_op: None,
        from_wire: false,
        request_id: None,
    };
    let resp_cfg = exchange(&sock, &mismatched_config).await;
    assert!(
        resp_cfg.config_mismatch,
        "differing config must be rejected"
    );
    assert!(!resp_cfg.namespace_mismatch);
    assert!(!resp_cfg.ok);

    // (d) version mismatch → explicit error, NOT namespace/config mismatch
    let wrong_version = DaemonRequestFrame {
        plan: false,
        ops: "stats()".to_string(),
        presentation: None,
        presentation_per_op: None,
        namespace: "test".to_string(),
        actor_id: None,
        process_ref: None,
        visible_namespaces: Vec::new(),
        config_id: config_id.clone(),
        protocol_version: 0,
        probe_only: false,
        metrics_only: false,
        format: None,
        format_per_op: None,
        from_wire: false,
        request_id: None,
    };
    let resp_ver = exchange(&sock, &wrong_version).await;
    assert!(
        !resp_ver.version_mismatch,
        "a client below the daemon's protocol is refused with version_mismatch=false: \
             the flag is reserved for a client that is ahead, and the deployed bridges \
             re-exec on this shape. The typed code below carries the fact instead."
    );
    assert_eq!(
        resp_ver
            .error_detail
            .as_ref()
            .and_then(|detail| detail.get("code"))
            .and_then(serde_json::Value::as_str),
        Some("version_mismatch"),
        "the refusal must stay typed as a version mismatch; got: {:?}",
        resp_ver.error_detail
    );
    assert!(!resp_ver.ok);
    assert!(
        resp_ver
            .error
            .as_deref()
            .unwrap_or("")
            .contains("protocol mismatch"),
        "version mismatch error must include 'protocol mismatch'; got: {:?}",
        resp_ver.error
    );
    assert!(
        resp_ver
            .error
            .as_deref()
            .unwrap_or("")
            .contains("make local"),
        "version mismatch error must tell operator what to do; got: {:?}",
        resp_ver.error
    );
    assert_eq!(
        resp_ver.daemon_protocol_version, PROTOCOL_VERSION,
        "daemon must echo its own protocol version in the mismatch response"
    );

    handle.abort();
    let _ = handle.await;
    clear_daemon_env();
}

#[tokio::test]
#[serial]
async fn daemon_rejects_client_after_git_write_policy_is_revoked() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }

    clear_daemon_env();
    reset_fallback_counters();
    let dir = tempfile::tempdir().expect("tempdir");
    let sock = dir.path().join("khived.sock");
    let pid = dir.path().join("khived.pid");
    std::env::set_var("KHIVE_SOCKET", &sock);
    std::env::set_var("KHIVE_PID", &pid);
    std::env::remove_var("KHIVE_NO_DAEMON");

    let mut allowed_config = memory_runtime_config();
    allowed_config.default_namespace = Namespace::parse("test").unwrap();
    allowed_config.packs = vec!["kg".to_string()];
    allowed_config.git_write = GitWriteSectionConfig {
        allowed: vec![GitWriteEntryConfig {
            repo: "/srv/repos/alpha".to_string(),
            branches: vec!["feat/*".to_string()],
        }],
        ..Default::default()
    };
    let revoked_config = RuntimeConfig {
        git_write: GitWriteSectionConfig::default(),
        ..allowed_config.clone()
    };

    let daemon_server = crate::server::KhiveMcpServer::new(
        KhiveRuntime::new(allowed_config).expect("allowed-policy runtime"),
    )
    .expect("allowed-policy server");
    let daemon_config_id = daemon_server.config_id().to_string();
    let revoked_config_id = crate::server::compute_config_id(&revoked_config, None);
    assert_ne!(
        daemon_config_id, revoked_config_id,
        "revoking the allowlist must change the daemon identity"
    );

    let handle = tokio::spawn(async move {
        let _ = run_daemon(daemon_server).await;
    });
    let ready = connect_when_ready(&sock).await;
    drop(ready);

    let request = DaemonRequestFrame {
        plan: false,
        ops: "stats()".to_string(),
        presentation: None,
        presentation_per_op: None,
        namespace: "test".to_string(),
        actor_id: None,
        process_ref: None,
        visible_namespaces: Vec::new(),
        config_id: revoked_config_id.clone(),
        protocol_version: PROTOCOL_VERSION,
        probe_only: false,
        metrics_only: false,
        format: None,
        format_per_op: None,
        from_wire: false,
        request_id: None,
    };
    let response = exchange(&sock, &request).await;

    assert!(response.config_mismatch, "revoked policy must be rejected");
    assert!(!response.ok, "the old-policy daemon must not dispatch");
    assert_eq!(
        response.served_config_id.as_deref(),
        Some(daemon_config_id.as_str())
    );
    assert!(
        map_response(response, &revoked_config_id, "test").is_none(),
        "non-strict clients must take the established config-mismatch fallback path"
    );
    assert_eq!(fallback_count(FallbackReason::ConfigMismatch), 1);

    handle.abort();
    let _ = handle.await;
    reset_fallback_counters();
    clear_daemon_env();
}

// ── ADR-096 Fork 1: per-request identity over one warm registry ──────────
//
// Core capability test: requests carrying DIFFERENT frame namespaces and
// actors, dispatched against ONE already-running warm daemon, must all be
// served over the shared backend — and each write must be stamped with its
// OWN frame's actor and process provenance, never the other request's and
// never a daemon-baked default. `comm.send` is the
// vehicle; a chained `comm.thread` reads its persisted properties back.
#[tokio::test]
#[serial]
async fn daemon_serves_per_request_identity_over_one_warm_registry() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }

    clear_daemon_env();
    let dir = tempfile::tempdir().expect("tempdir");
    let sock = dir.path().join("khived.sock");
    let pid = dir.path().join("khived.pid");
    std::env::set_var("KHIVE_SOCKET", &sock);
    std::env::set_var("KHIVE_PID", &pid);
    std::env::remove_var("KHIVE_NO_DAEMON");
    std::env::set_var("KHIVE_PROCESS_REF", "daemon/stale-origin");

    // No baked actor_id on the daemon-side server: every actor must come
    // from the per-request frame, never a construction-time default.
    let reference = make_comm_test_server(None);
    let config_id = reference.config_id().to_string();
    let daemon_server = reference.clone();

    let handle = tokio::spawn(async move {
        let _ = run_daemon(daemon_server).await;
    });
    let _ready = connect_when_ready(&sock).await;
    drop(_ready);

    let alice_frame = DaemonRequestFrame {
        plan: false,
        ops: "comm.send(to=\"bob\", content=\"hello from alice\") | comm.thread(id=$prev.full_id)"
            .to_string(),
        presentation: None,
        presentation_per_op: None,
        namespace: "alpha".to_string(),
        actor_id: Some("alice".to_string()),
        process_ref: Some("worker/alice:17".to_string()),
        visible_namespaces: Vec::new(),
        config_id: config_id.clone(),
        protocol_version: PROTOCOL_VERSION,
        probe_only: false,
        metrics_only: false,
        format: None,
        format_per_op: None,
        from_wire: false,
        request_id: None,
    };
    let bob_frame = DaemonRequestFrame {
        plan: false,
        ops: "comm.send(to=\"alice\", content=\"hello from bob\") | comm.thread(id=$prev.full_id)"
            .to_string(),
        presentation: None,
        presentation_per_op: None,
        namespace: "beta".to_string(),
        actor_id: Some("bob".to_string()),
        process_ref: Some("worker/bob:29".to_string()),
        visible_namespaces: Vec::new(),
        config_id: config_id.clone(),
        protocol_version: PROTOCOL_VERSION,
        probe_only: false,
        metrics_only: false,
        format: None,
        format_per_op: None,
        from_wire: false,
        request_id: None,
    };
    let charlie_frame = DaemonRequestFrame {
        plan: false,
        ops: "comm.send(to=\"alice\", content=\"hello from charlie\") | comm.thread(id=$prev.full_id)"
            .to_string(),
        presentation: None,
        presentation_per_op: None,
        namespace: "gamma".to_string(),
        actor_id: Some("charlie".to_string()),
        process_ref: None,
        visible_namespaces: Vec::new(),
        config_id: config_id.clone(),
        protocol_version: PROTOCOL_VERSION,
        probe_only: false,
        metrics_only: false,
        format: None,
        format_per_op: None,
        from_wire: false,
        request_id: None,
    };

    let resp_alice = exchange(&sock, &alice_frame).await;
    assert!(
        resp_alice.ok,
        "alice's request must be served over the shared warm registry; error={:?}",
        resp_alice.error
    );
    assert!(!resp_alice.namespace_mismatch);
    assert!(!resp_alice.config_mismatch);
    let body_alice: serde_json::Value =
        serde_json::from_str(resp_alice.result.as_deref().expect("alice result body"))
            .expect("decode alice result json");
    assert_eq!(
        body_alice["results"][0]["result"]["from"], "alice",
        "write dispatched under alice's frame must stamp actor=alice, got: {body_alice}"
    );
    assert_eq!(
        body_alice["results"][1]["result"]["messages"][0]["properties"]["sent_by_process"],
        "worker/alice:17",
        "the warm daemon must persist alice's frame provenance, not its own environment: \
             {body_alice}"
    );

    let resp_bob = exchange(&sock, &bob_frame).await;
    assert!(
        resp_bob.ok,
        "bob's request must be served over the SAME shared warm registry; error={:?}",
        resp_bob.error
    );
    assert!(!resp_bob.namespace_mismatch);
    assert!(!resp_bob.config_mismatch);
    let body_bob: serde_json::Value =
        serde_json::from_str(resp_bob.result.as_deref().expect("bob result body"))
            .expect("decode bob result json");
    assert_eq!(
        body_bob["results"][0]["result"]["from"], "bob",
        "write dispatched under bob's frame must stamp actor=bob, NOT cross-\
             contaminated with alice's actor; got: {body_bob}"
    );
    assert_eq!(
        body_bob["results"][1]["result"]["messages"][0]["properties"]["sent_by_process"],
        "worker/bob:29",
        "the warm daemon must persist bob's frame provenance independently: {body_bob}"
    );

    let resp_charlie = exchange(&sock, &charlie_frame).await;
    assert!(
        resp_charlie.ok,
        "charlie's request must be served over the SAME shared warm registry; error={:?}",
        resp_charlie.error
    );
    let body_charlie: serde_json::Value =
        serde_json::from_str(resp_charlie.result.as_deref().expect("charlie result body"))
            .expect("decode charlie result json");
    assert_eq!(body_charlie["results"][0]["result"]["from"], "charlie");
    let charlie_properties = body_charlie["results"][1]["result"]["messages"][0]["properties"]
        .as_object()
        .expect("charlie message properties");
    assert!(
        !charlie_properties.contains_key("sent_by_process"),
        "an explicitly absent frame provenance must stay absent rather than falling back to \
             the daemon environment; got: {body_charlie}"
    );

    handle.abort();
    let _ = handle.await;
    clear_daemon_env();
}

// ADR-096 Fork 1 completion: actor-derived visible namespaces are request
// identity, not daemon engine identity. Two clients with different
// configured actors therefore compute the same config_id, but each daemon
// frame must still read only through its own visible set.
#[tokio::test]
#[serial]
async fn daemon_config_id_ignores_actor_folded_visibility_but_frame_visibility_isolated() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }

    clear_daemon_env();
    let dir = tempfile::tempdir().expect("tempdir");
    let sock = dir.path().join("khived.sock");
    let pid = dir.path().join("khived.pid");
    std::env::set_var("KHIVE_SOCKET", &sock);
    std::env::set_var("KHIVE_PID", &pid);
    std::env::remove_var("KHIVE_NO_DAEMON");

    let actor_a = "lambda:actor-a";
    let actor_b = "lambda:actor-b";
    let cfg_a = folded_actor_memory_config(actor_a);
    let cfg_b = folded_actor_memory_config(actor_b);
    let ns_a = Namespace::parse(actor_a).expect("actor a namespace");
    let ns_b = Namespace::parse(actor_b).expect("actor b namespace");

    assert_eq!(cfg_a.actor_id.as_deref(), Some(actor_a));
    assert_eq!(cfg_b.actor_id.as_deref(), Some(actor_b));
    assert!(
        cfg_a.visible_namespaces.contains(&ns_a),
        "actor.id must fold into client A visible_namespaces"
    );
    assert!(
        cfg_b.visible_namespaces.contains(&ns_b),
        "actor.id must fold into client B visible_namespaces"
    );
    assert_ne!(
        cfg_a.visible_namespaces, cfg_b.visible_namespaces,
        "precondition: clients must carry different folded visible sets"
    );

    let id_a = crate::server::compute_config_id(&cfg_a, None);
    let id_b = crate::server::compute_config_id(&cfg_b, None);
    assert_eq!(
        id_a, id_b,
        "actor-derived visible_namespaces must not affect daemon config_id"
    );

    let daemon_server = {
        let runtime = KhiveRuntime::new(memory_runtime_config()).expect("in-memory runtime");
        crate::server::KhiveMcpServer::new(runtime).expect("server builds with kg")
    };
    assert_eq!(
        daemon_server.config_id(),
        id_a,
        "daemon and both clients must share the same engine-coherence key"
    );
    let handle = tokio::spawn(async move {
        let _ = run_daemon(daemon_server).await;
    });
    let _ready = connect_when_ready(&sock).await;
    drop(_ready);

    let frame = |ops: &str, actor: &str, visible: &[Namespace]| DaemonRequestFrame {
        plan: false,
        ops: ops.to_string(),
        presentation: Some("verbose".to_string()),
        presentation_per_op: None,
        namespace: "local".to_string(),
        actor_id: Some(actor.to_string()),
        process_ref: None,
        visible_namespaces: visible.iter().map(|ns| ns.as_str().to_string()).collect(),
        config_id: id_a.clone(),
        protocol_version: PROTOCOL_VERSION,
        probe_only: false,
        metrics_only: false,
        format: None,
        format_per_op: None,
        from_wire: false,
        request_id: None,
    };

    let seed_a = exchange(
        &sock,
        &frame(
            r#"create(kind="concept", name="ActorAVisibleOnly", namespace="lambda:actor-a")"#,
            actor_a,
            &cfg_a.visible_namespaces,
        ),
    )
    .await;
    assert!(seed_a.ok, "seed A must succeed: {:?}", seed_a.error);
    let seed_b = exchange(
        &sock,
        &frame(
            r#"create(kind="concept", name="ActorBVisibleOnly", namespace="lambda:actor-b")"#,
            actor_b,
            &cfg_b.visible_namespaces,
        ),
    )
    .await;
    assert!(seed_b.ok, "seed B must succeed: {:?}", seed_b.error);

    fn names_from_list_response(resp: &DaemonResponseFrame) -> Vec<String> {
        assert!(resp.ok, "list response must be ok: {:?}", resp.error);
        assert!(
            !resp.config_mismatch,
            "list response must not reject on config_id"
        );
        let body: serde_json::Value =
            serde_json::from_str(resp.result.as_deref().expect("list result body"))
                .expect("decode list result json");
        let first = &body["results"][0];
        assert_eq!(
            first["ok"], true,
            "list op must succeed inside daemon result: {first}"
        );
        let rows = first["result"]["items"]
            .as_array()
            .expect("list result must contain the stable items array");
        rows.iter()
            .filter_map(|row| row.get("name").and_then(|v| v.as_str()).map(str::to_string))
            .collect()
    }

    let list_a = exchange(
        &sock,
        &frame(r#"list(kind="entity")"#, actor_a, &cfg_a.visible_namespaces),
    )
    .await;
    let names_a = names_from_list_response(&list_a);
    assert!(
        names_a.iter().any(|name| name == "ActorAVisibleOnly"),
        "actor A frame must see actor A namespace rows; got {names_a:?}"
    );
    assert!(
        !names_a.iter().any(|name| name == "ActorBVisibleOnly"),
        "actor A frame must not see actor B namespace rows; got {names_a:?}"
    );

    let list_b = exchange(
        &sock,
        &frame(r#"list(kind="entity")"#, actor_b, &cfg_b.visible_namespaces),
    )
    .await;
    let names_b = names_from_list_response(&list_b);
    assert!(
        names_b.iter().any(|name| name == "ActorBVisibleOnly"),
        "actor B frame must see actor B namespace rows; got {names_b:?}"
    );
    assert!(
        !names_b.iter().any(|name| name == "ActorAVisibleOnly"),
        "actor B frame must not see actor A namespace rows; got {names_b:?}"
    );

    handle.abort();
    let _ = handle.await;
    clear_daemon_env();
}

// Back-compat: a caller with NO per-request identity context (pure local
// dispatch, never touching the daemon socket) must keep using the
// server's own construction-baked actor_id, unaffected by the identity-
// override machinery introduced for daemon-forwarded requests.
#[tokio::test]
#[serial]
#[serial_test::serial(config_ledger)]
async fn local_dispatch_without_identity_context_uses_baked_actor() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }

    clear_daemon_env();

    let server = make_comm_test_server(Some("baked-actor"));
    let result = server
        .dispatch_request_local(RequestParams {
            plan: None,
            ops: "comm.send(to=\"someone\", content=\"hello\")".to_string(),
            presentation: None,
            presentation_per_op: None,
            save_to: None,
            format: None,
            format_per_op: None,
            request_id: None,
        })
        .await
        .expect("local dispatch of comm.send must succeed");

    let body: serde_json::Value =
        serde_json::from_str(&result).expect("decode local dispatch result json");
    assert_eq!(
        body["results"][0]["result"]["from"], "baked-actor",
        "local dispatch (no daemon, no identity context) must use the server's \
             own baked actor_id, got: {body}"
    );
}

// ── daemon-forward wire-origin gate (security regression) ────────────────
//
// The agent-facing MCP `request` tool sets `from_wire=true` on its daemon
// frame; the daemon must HONOR that bit so the subhandler visibility gate
// fires after the socket round-trip, not only on the local-fallback path.
// The local-fallback tests (in tests/integration.rs) run with the daemon
// disabled and would stay green even if the MCP frame were flipped to
// `from_wire=false` — which would open an agent-reachable subhandler bypass
// on every daemon-backed deployment. This pins the round-trip seam.
#[tokio::test]
#[serial]
async fn daemon_round_trip_honors_from_wire_for_subhandlers() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }

    clear_daemon_env();
    let dir = tempfile::tempdir().expect("tempdir");
    let sock = dir.path().join("khived.sock");
    let pid = dir.path().join("khived.pid");
    std::env::set_var("KHIVE_SOCKET", &sock);
    std::env::set_var("KHIVE_PID", &pid);
    std::env::remove_var("KHIVE_NO_DAEMON");

    let reference = make_subhandler_test_server();
    let config_id = reference.config_id().to_string();
    let daemon_server = reference.clone();
    let handle = tokio::spawn(async move {
        let _ = run_daemon(daemon_server).await;
    });
    let _ready = connect_when_ready(&sock).await;
    drop(_ready);

    let frame = |from_wire: bool| DaemonRequestFrame {
        plan: false,
        ops: "brain.state()".to_string(),
        presentation: None,
        presentation_per_op: None,
        namespace: "braintest".to_string(),
        actor_id: None,
        process_ref: None,
        visible_namespaces: Vec::new(),
        config_id: config_id.clone(),
        protocol_version: PROTOCOL_VERSION,
        probe_only: false,
        metrics_only: false,
        format: None,
        format_per_op: None,
        from_wire,
        request_id: None,
    };

    // (a) from_wire=true → daemon applies the wire visibility gate:
    // `brain.state` is a Subhandler and must be blocked after the round-trip.
    let resp_wire = exchange(&sock, &frame(true)).await;
    assert!(
        resp_wire.ok,
        "dispatch itself must succeed (the op carries the gate error); error={:?}",
        resp_wire.error
    );
    let body_wire: serde_json::Value =
        serde_json::from_str(resp_wire.result.as_deref().expect("wire result body"))
            .expect("decode wire result json");
    let first_wire = &body_wire["results"][0];
    assert_eq!(
        first_wire["ok"], false,
        "from_wire=true subhandler must be blocked through the daemon: {first_wire}"
    );
    let err_wire = first_wire["error"]["message"].as_str().unwrap_or("");
    assert_eq!(first_wire["error"]["domain_disposition"], "not_committed");
    assert!(
        err_wire.contains("permission denied") || err_wire.contains("subhandler"),
        "daemon-forward wire path must surface the subhandler gate error; got: {err_wire}"
    );

    // (b) from_wire=false (operator frame, e.g. `kkernel exec`) → the same
    // Subhandler verb must be REACHED, not gated.
    let resp_op = exchange(&sock, &frame(false)).await;
    let body_op: serde_json::Value =
        serde_json::from_str(resp_op.result.as_deref().expect("operator result body"))
            .expect("decode operator result json");
    let first_op = &body_op["results"][0];
    let err_op = first_op["error"]["message"].as_str().unwrap_or("");
    assert!(
        !err_op.contains("permission denied") && !err_op.contains("subhandler"),
        "operator frame must NOT gate the subhandler through the daemon: {first_op}"
    );

    handle.abort();
    let _ = handle.await;
    clear_daemon_env();
}

// The agent-facing `request` tool must stamp `from_wire=true` on its daemon
// forward-frame. Without this, a daemon-backed MCP request would dispatch
// with `from_wire=false` and silently reopen the agent subhandler bypass
// (review #369 Medium). Pinned at the frame-builder seam so the daemon
// round-trip test above (which proves the daemon HONORS the bit) is paired
// with proof that the tool SETS it.
#[test]
fn wire_request_frame_sets_from_wire_true() {
    let server = make_subhandler_test_server();
    let params = RequestParams {
        ops: "brain.state()".to_string(),
        ..Default::default()
    };
    let frame = server.wire_daemon_frame(&params);
    assert!(
        frame.from_wire,
        "request tool must set from_wire=true on the daemon forward-frame"
    );
    assert_eq!(frame.ops, "brain.state()");
    assert_eq!(frame.namespace, "braintest");
}

// ── new-client + old-daemon regression (fix for #98 BLOCKER) ─────────────
//
// Simulates a pre-versioning daemon that:
//   • Ignores the unknown `protocol_version` request field (it deserializes
//     as missing and is silently dropped).
//   • Returns a decodable response with a matching `served_config_id` but
//     WITHOUT `daemon_protocol_version` or `version_mismatch` (they default
//     to `0` / `false` on the client).
//
// Before the fix, `map_response` accepted this response because `version_mismatch`
// was false and `served_config_id` matched — the stale daemon was trusted.
//
// After the fix, `try_forward_inner` detects `daemon_protocol_version == 0 != 1`
// and returns `ForwardOutcome::ProtocolMismatch`, which `forward_or_spawn`
// rejects without retry, local dispatch, kill, or respawn.
//
// The test verifies at the `forward_or_spawn` level (not just `map_response`)
// that:
//   1. The stale response is NOT accepted.
//   2. No second connection or recovery attempt is made.
//   3. A clear "protocol mismatch" error is returned.

/// Minimal "old daemon" frame: has `served_config_id` but omits the
/// protocol version fields (they default to `false`/`0` on the client side).
fn old_daemon_response(config_id: &str) -> DaemonResponseFrame {
    DaemonResponseFrame {
        ok: true,
        result: Some("stale-result".to_string()),
        error: None,
        error_detail: None,
        namespace_mismatch: false,
        config_mismatch: false,
        served_config_id: Some(config_id.to_string()),
        // Pre-versioning daemon would never set these:
        version_mismatch: false,
        daemon_protocol_version: 0,
        metrics: None,
        request_id: None,
    }
}

/// Serve exactly one connection with `response`, then stop accepting.
async fn serve_one_response(listener: tokio::net::UnixListener, response: DaemonResponseFrame) {
    if let Ok((mut stream, _)) = listener.accept().await {
        // Read the inbound request frame (and discard it — old daemon ignores
        // unknown fields, which is the scenario we're simulating).
        if read_frame(&mut stream).await.is_ok() {
            if let Ok(payload) = serde_json::to_vec(&response) {
                let _ = write_frame(&mut stream, &payload).await;
            }
        }
    }
    // Listener drops here; subsequent connection attempts see "connection refused".
}

#[tokio::test]
#[serial]
async fn forward_or_spawn_rejects_old_daemon_and_returns_protocol_mismatch_error() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }

    clear_daemon_env();
    let dir = tempfile::tempdir().expect("tempdir");
    let sock = dir.path().join("khived.sock");
    let pid_file = dir.path().join("khived.pid");
    let lock_file = dir.path().join("khived.recovery.lock");

    std::env::set_var("KHIVE_SOCKET", &sock);
    std::env::set_var("KHIVE_PID", &pid_file);
    std::env::set_var("KHIVE_LOCK", &lock_file);
    std::env::remove_var("KHIVE_NO_DAEMON");

    let config_id = "packs=[kg];db=:memory:;embed=none;extra=[];backend=main";

    // Bind the fake old-daemon socket BEFORE starting the client.
    let listener = tokio::net::UnixListener::bind(&sock).expect("bind fake old-daemon socket");
    // A PID file makes the fake daemon's rendezvous realistic. The terminal
    // mismatch path must leave it untouched.
    std::fs::write(&pid_file, std::process::id().to_string()).expect("write pid file");

    let old_resp = old_daemon_response(config_id);
    // Serve exactly one exchange, then let the listener drop (no second connection
    // will be served — any second connection would therefore expose an
    // incorrect retry as `NoSocket` rather than hiding it in the fixture.
    let fake_handle = tokio::spawn(serve_one_response(listener, old_resp));

    let frame = DaemonRequestFrame {
        plan: false,
        ops: "stats()".to_string(),
        presentation: None,
        presentation_per_op: None,
        namespace: "test".to_string(),
        actor_id: None,
        process_ref: None,
        visible_namespaces: Vec::new(),
        config_id: config_id.to_string(),
        protocol_version: PROTOCOL_VERSION,
        probe_only: false,
        metrics_only: false,
        format: None,
        format_per_op: None,
        from_wire: false,
        request_id: None,
    };

    let result = forward_or_spawn(&frame).await;

    // The fake socket served exactly one old-protocol response.  The fake handle
    // should have completed by now; join it to catch any panics.
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), fake_handle).await;

    // Must NOT accept the stale daemon result.
    match result {
        Some(Err(McpError { message, .. })) => {
            assert!(
                message.contains("protocol mismatch"),
                "error must name 'protocol mismatch'; got: {message}"
            );
            assert!(
                message.contains("make local") || message.contains("rebuild"),
                "error must tell the operator what to do; got: {message}"
            );
        }
        Some(Ok(v)) => {
            panic!("forward_or_spawn must NOT accept old-daemon response; got Ok({v:?})")
        }
        None => panic!(
            "forward_or_spawn must return Some(Err(..)) for protocol mismatch, \
                 not None (which would cause silent fallback to local dispatch)"
        ),
    }

    clear_daemon_env();
    std::env::remove_var("KHIVE_LOCK");
}

// ── bridge behind the daemon: the self-heal must arm ─────────────────────
//
// A binary swap that carries a protocol bump respawns the daemon under the
// new number while every running bridge keeps the old one. The daemon
// refuses each request with `version_mismatch=true` and its higher number.
// Before the fix `try_forward_inner` classified that as `Response` and
// `map_response` returned the hard error on every request for the rest of
// the bridge's life; the #714 re-exec, built for exactly this scenario, was
// armed only for the daemon-behind direction. Now both directions classify
// as `ProtocolMismatch`, the error names the direction, and the re-exec is
// armed so the next flush re-execs the on-disk binary the daemon came from.

fn newer_daemon_response(config_id: &str) -> DaemonResponseFrame {
    DaemonResponseFrame {
        ok: false,
        result: None,
        error: Some(format!(
            "daemon protocol mismatch: client={} daemon={} — \
                 rebuild/update the client binary (make local)",
            PROTOCOL_VERSION,
            PROTOCOL_VERSION + 1
        )),
        error_detail: None,
        namespace_mismatch: false,
        config_mismatch: false,
        served_config_id: Some(config_id.to_string()),
        version_mismatch: true,
        daemon_protocol_version: PROTOCOL_VERSION + 1,
        metrics: None,
        request_id: None,
    }
}

#[tokio::test]
#[serial]
async fn forward_or_spawn_behind_a_newer_daemon_returns_the_error_and_arms_reexec() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }

    clear_daemon_env();
    clear_pending_self_heal();
    let dir = tempfile::tempdir().expect("tempdir");
    let sock = dir.path().join("khived.sock");
    let pid_file = dir.path().join("khived.pid");
    let lock_file = dir.path().join("khived.recovery.lock");

    std::env::set_var("KHIVE_SOCKET", &sock);
    std::env::set_var("KHIVE_PID", &pid_file);
    std::env::set_var("KHIVE_LOCK", &lock_file);
    std::env::remove_var("KHIVE_NO_DAEMON");

    let config_id = "packs=[kg];db=:memory:;embed=none;extra=[];backend=main";
    let listener = tokio::net::UnixListener::bind(&sock).expect("bind fake newer-daemon socket");
    std::fs::write(&pid_file, std::process::id().to_string()).expect("write pid file");
    let fake_handle = tokio::spawn(serve_one_response(
        listener,
        newer_daemon_response(config_id),
    ));

    let frame = DaemonRequestFrame {
        plan: false,
        ops: "stats()".to_string(),
        presentation: None,
        presentation_per_op: None,
        namespace: "test".to_string(),
        actor_id: None,
        process_ref: None,
        visible_namespaces: Vec::new(),
        config_id: config_id.to_string(),
        protocol_version: PROTOCOL_VERSION,
        probe_only: false,
        metrics_only: false,
        format: None,
        format_per_op: None,
        from_wire: false,
        request_id: None,
    };

    let result = forward_or_spawn(&frame).await;
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), fake_handle).await;

    match result {
        Some(Err(McpError { message, .. })) => {
            assert!(
                message.contains("protocol mismatch"),
                "error must name 'protocol mismatch'; got: {message}"
            );
            assert!(
                message.contains("re-execs"),
                "error must say this bridge re-execs itself, not send the operator to \
                     rebuild a daemon that is already current; got: {message}"
            );
        }
        Some(Ok(v)) => {
            panic!("forward_or_spawn must NOT accept a newer daemon's refusal; got Ok({v:?})")
        }
        None => panic!(
            "forward_or_spawn must return Some(Err(..)) for protocol mismatch, \
                 not None (which would cause silent fallback to local dispatch)"
        ),
    }

    let armed = PENDING_SELF_HEAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        .map(|pending| (pending.action, pending.executable.clone()));
    assert_eq!(
        armed,
        Some((MismatchRecovery::ReexecScheduled, None)),
        "a bridge behind the daemon must arm the in-place re-exec"
    );
    // The pid file belongs to the live daemon; the terminal path leaves it alone.
    assert!(
        pid_file.exists(),
        "the newer daemon's pid file must survive"
    );

    clear_pending_self_heal();
    clear_daemon_env();
    std::env::remove_var("KHIVE_LOCK");
}

// ── daemon crash mid-dispatch regression (#91) ────────────────────────────
//
// When the daemon crashes (or panics) during dispatch it closes the
// connection without writing a response frame. Before the fix, `try_forward_inner`
// returned `ForwardOutcome::NoSocket` on the `read_frame` error, causing
// `forward_or_spawn` to return `None` — a silent fallback to local dispatch.
//
// The fix promotes the `read_frame` error to `ForwardOutcome::ParseFailure`.
// Since the real frame was fully written, `forward_or_spawn` turns that
// classification into a terminal ambiguity error and performs no recovery.
//
// This test binds a fake socket that reads the request but immediately drops
// the connection without writing a response (simulating a crash during
// dispatch). This focused test validates the exact `ForwardOutcome`
// discriminant; the parallel test validates the terminal call-site behavior
// and zero lifecycle actions.

/// Serve one connection: read the request frame, then drop the stream
/// without writing any response (simulating a daemon crash mid-dispatch).
async fn serve_crash_on_dispatch(listener: tokio::net::UnixListener) {
    if let Ok((mut stream, _)) = listener.accept().await {
        // Read the inbound request (and discard it — the "crash" happens here).
        let _ = read_frame(&mut stream).await;
        // Drop stream without writing a response — connection resets.
    }
    // Listener drops; subsequent connection attempts see "connection refused".
}

#[tokio::test]
#[serial]
#[serial_test::serial(config_ledger)]
async fn try_forward_inner_returns_response_lost_when_daemon_closes_without_response() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }

    clear_daemon_env();
    let dir = tempfile::tempdir().expect("tempdir");
    let sock = dir.path().join("khived.sock");
    let pid_file = dir.path().join("khived.pid");

    std::env::set_var("KHIVE_SOCKET", &sock);
    std::env::set_var("KHIVE_PID", &pid_file);
    std::env::remove_var("KHIVE_NO_DAEMON");

    let config_id = "packs=[kg];db=:memory:;embed=none;extra=[];backend=main";

    let listener = tokio::net::UnixListener::bind(&sock).expect("bind fake crash-daemon socket");
    // A PID file makes the fake daemon's rendezvous realistic; this focused
    // classifier test must not touch it.
    std::fs::write(&pid_file, std::process::id().to_string()).expect("write pid file");

    // Serve one connection that crashes without replying.
    let fake_handle = tokio::spawn(serve_crash_on_dispatch(listener));

    let frame = DaemonRequestFrame {
        plan: false,
        ops: "stats()".to_string(),
        presentation: None,
        presentation_per_op: None,
        namespace: "test".to_string(),
        actor_id: None,
        process_ref: None,
        visible_namespaces: Vec::new(),
        config_id: config_id.to_string(),
        protocol_version: PROTOCOL_VERSION,
        probe_only: false,
        metrics_only: false,
        format: None,
        format_per_op: None,
        from_wire: false,
        request_id: None,
    };

    // Call try_forward_inner directly to assert the discriminant.
    let outcome = try_forward_inner(&frame).await;

    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), fake_handle).await;

    assert!(
        matches!(outcome, ForwardOutcome::ResponseLost),
        "daemon crash (connection closed without response) must yield \
             ResponseLost, not NoSocket — got a different variant"
    );

    clear_daemon_env();
}

// ── unbounded-write regression (khive-oss#2337) ────────────────────────────
//
// Before the fix, `try_forward_inner` only bounded the response *read*;
// the connect and write phases had no deadline at all. A same-UID peer
// that accepts the connection and never reads from it can fill the
// kernel socket send buffer and block the write forever — nothing above
// this function could stop it, so cancellation-shielding in `server.rs`
// would eventually detach the task, but the task, its `UnixStream`, and
// the request payload would live on indefinitely as long as that peer
// stays connected.
//
// The fix bounds connect, write, and read by one absolute deadline. On a
// write timeout the stream is dropped (so a peer that later resumes
// reading observes end-of-stream mid-frame) and the outcome is
// `NoSocket` — nothing was fully delivered, so it is safe for
// `forward_or_spawn_with_exe`'s recovery path to kill/respawn and retry,
// unlike the post-write `ParseFailure` ambiguity case above.

/// Accept one connection and never read from it, simulating a same-UID
/// peer that squats on the socket path without servicing requests. The
/// accepted stream is handed back over `hold` so the test can observe it
/// after the writer's own timeout fires.
async fn serve_accept_and_never_read(
    listener: tokio::net::UnixListener,
    hold: tokio::sync::oneshot::Sender<tokio::net::UnixStream>,
) {
    if let Ok((stream, _)) = listener.accept().await {
        let _ = hold.send(stream);
    }
}

#[tokio::test]
#[serial]
#[serial_test::serial(config_ledger)]
async fn try_forward_inner_write_timeout_drops_stream_and_returns_no_socket() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }

    clear_daemon_env();
    let dir = tempfile::tempdir().expect("tempdir");
    let sock = dir.path().join("khived.sock");
    let pid_file = dir.path().join("khived.pid");

    std::env::set_var("KHIVE_SOCKET", &sock);
    std::env::set_var("KHIVE_PID", &pid_file);
    std::env::remove_var("KHIVE_NO_DAEMON");
    // A short ceiling keeps this test fast. `try_forward_inner` has no
    // inherited request-read context here (called directly, not via the
    // `server.rs` spawn site), so it falls back to reading this env var
    // fresh on every call via `request_read_timeout_from_env()` — real
    // (unpaused) time, since a genuinely blocked socket write is real
    // I/O, not a timer, and would not release control for a paused
    // clock's auto-advance to fire.
    let request_timeout = std::time::Duration::from_secs(2);
    std::env::set_var(
        "KHIVE_REQUEST_READ_TIMEOUT_SECS",
        request_timeout.as_secs().to_string(),
    );
    // A fixed second covers runner scheduling without scaling down with the
    // timeout. The strict ordinary cap still rejects an extra 1s return
    // delay; coverage keeps its wider cap, below the watchdog.
    let elapsed_limit = if std::env::var_os("LLVM_PROFILE_FILE").is_some() {
        request_timeout * 3
    } else {
        request_timeout + std::time::Duration::from_secs(1)
    };
    let watchdog = request_timeout * 5;

    let config_id = "packs=[kg];db=:memory:;embed=none;extra=[];backend=main";

    let listener = tokio::net::UnixListener::bind(&sock).expect("bind fake silent-peer socket");
    std::fs::write(&pid_file, std::process::id().to_string()).expect("write pid file");

    let (held_tx, held_rx) = tokio::sync::oneshot::channel();
    let fake_handle = tokio::spawn(serve_accept_and_never_read(listener, held_tx));

    // Comfortably inside MAX_FRAME_BYTES but large enough to exceed any
    // realistic Unix-domain-socket kernel send buffer, so the write
    // blocks once that buffer fills (the peer above never reads).
    let big_ops = format!("stats(padding=\"{}\")", "x".repeat(7 * 1024 * 1024));
    assert!(
        big_ops.len() < khive_runtime::daemon::MAX_FRAME_BYTES,
        "test payload must stay under MAX_FRAME_BYTES so write_frame does not reject it \
             outright before the timeout can be observed"
    );
    let frame = DaemonRequestFrame {
        plan: false,
        ops: big_ops,
        presentation: None,
        presentation_per_op: None,
        namespace: "test".to_string(),
        actor_id: None,
        process_ref: None,
        visible_namespaces: Vec::new(),
        config_id: config_id.to_string(),
        protocol_version: PROTOCOL_VERSION,
        probe_only: false,
        metrics_only: false,
        format: None,
        format_per_op: None,
        from_wire: false,
        request_id: None,
    };

    let started = std::time::Instant::now();
    // (a) The watchdog stops an unbounded write; the tighter elapsed
    // assertion below separately detects a delayed timeout return.
    let outcome = tokio::time::timeout(watchdog, try_forward_inner(&frame))
        .await
        .expect(
            "try_forward_inner must return within the ceiling plus margin, not hang on an \
             unbounded write to a silent peer",
        );
    let elapsed = started.elapsed();

    assert!(
        matches!(outcome, ForwardOutcome::NoSocket),
        "a write that never completes delivered nothing, so it must be treated like the \
             pre-write no-socket case (safe to retry/recover) — got a different variant: \
             {outcome:?}",
    );
    assert!(
        elapsed < elapsed_limit,
        "write timeout exceeded its configured caller timeout plus scheduling allowance; \
             timeout {request_timeout:?}, limit {elapsed_limit:?}, elapsed {elapsed:?}"
    );

    // (b) the peer's held stream must observe end-of-stream once
    // `try_forward_inner` drops its end on timeout — proving the cleanup
    // actually closed the socket rather than leaking it. Pre-fix this
    // read hangs forever (the writer's task, and its stream, never went
    // away), which is the red-before signal for this assertion.
    let mut held_stream = held_rx.await.expect("peer accepted the connection");
    // The kernel receive buffer may already hold bytes that made it
    // through before the write blocked, so draining to end-of-stream
    // takes a read loop, not a single read: the peer must consume
    // whatever was already queued before it can observe the writer's
    // end actually closing.
    let drain_result = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        use tokio::io::AsyncReadExt;
        let mut buf = [0u8; 4096];
        loop {
            match held_stream.read(&mut buf).await {
                Ok(0) => return Ok(()),
                Ok(_) => continue,
                Err(e) => return Err(e),
            }
        }
    })
    .await
    .expect(
        "peer-side read must not hang once try_forward_inner's write timeout has fired \
             and dropped its end of the connection",
    );
    // A connection-reset error also proves the writer's end was dropped;
    // `Ok(())` (clean end-of-stream) is the expected outcome here.
    let _ = drain_result;

    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), fake_handle).await;
    clear_daemon_env();
}

/// (d) The existing read-timeout arm — the peer reads the frame in full
/// but never answers — must still yield `ParseFailure` (terminal
/// ambiguity: the write completed, so the request may already be
/// executing on the daemon side). This is the control that proves the
/// write-phase fix above did not change the read-phase's own timeout
/// classification.
#[tokio::test]
#[serial]
#[serial_test::serial(config_ledger)]
async fn try_forward_inner_read_timeout_after_full_write_returns_parse_failure() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }

    clear_daemon_env();
    let dir = tempfile::tempdir().expect("tempdir");
    let sock = dir.path().join("khived.sock");
    let pid_file = dir.path().join("khived.pid");

    std::env::set_var("KHIVE_SOCKET", &sock);
    std::env::set_var("KHIVE_PID", &pid_file);
    std::env::remove_var("KHIVE_NO_DAEMON");
    let request_timeout = std::time::Duration::from_secs(2);
    std::env::set_var(
        "KHIVE_REQUEST_READ_TIMEOUT_SECS",
        request_timeout.as_secs().to_string(),
    );
    // Coverage uses wider slack; the ordinary cap allows a fixed second
    // for scheduling but still rejects an extra 1s return delay.
    let elapsed_limit = if std::env::var_os("LLVM_PROFILE_FILE").is_some() {
        request_timeout * 3
    } else {
        request_timeout + std::time::Duration::from_secs(1)
    };
    let watchdog = request_timeout * 5;

    let config_id = "packs=[kg];db=:memory:;embed=none;extra=[];backend=main";

    async fn serve_read_then_never_answer(listener: tokio::net::UnixListener) {
        if let Ok((mut stream, _)) = listener.accept().await {
            let _ = read_frame(&mut stream).await;
            std::future::pending::<()>().await;
        }
    }

    let listener =
        tokio::net::UnixListener::bind(&sock).expect("bind fake read-then-silent socket");
    std::fs::write(&pid_file, std::process::id().to_string()).expect("write pid file");
    let fake_handle = tokio::spawn(serve_read_then_never_answer(listener));

    let frame = DaemonRequestFrame {
        plan: false,
        ops: "stats()".to_string(),
        presentation: None,
        presentation_per_op: None,
        namespace: "test".to_string(),
        actor_id: None,
        process_ref: None,
        visible_namespaces: Vec::new(),
        config_id: config_id.to_string(),
        protocol_version: PROTOCOL_VERSION,
        probe_only: false,
        metrics_only: false,
        format: None,
        format_per_op: None,
        from_wire: false,
        request_id: None,
    };

    let started = std::time::Instant::now();
    let outcome = tokio::time::timeout(watchdog, try_forward_inner(&frame))
        .await
        .expect("try_forward_inner must return within the ceiling plus margin");
    let elapsed = started.elapsed();

    assert!(
        matches!(outcome, ForwardOutcome::ParseFailure),
        "a read timeout after a completed write must stay ParseFailure (terminal \
             ambiguity) — got a different variant: {outcome:?}"
    );
    assert!(
        elapsed < elapsed_limit,
        "read timeout exceeded its configured caller timeout plus scheduling allowance; \
             timeout {request_timeout:?}, limit {elapsed_limit:?}, elapsed {elapsed:?}"
    );

    fake_handle.abort();
    clear_daemon_env();
}

/// (c) Control: a peer that reads the frame and answers normally well
/// within the ceiling still yields the ordinary `Response` outcome. The
/// end-to-end round trip in `daemon_round_trip_dispatches_and_enforces_config_id`
/// above already exercises `write_frame`/`read_frame` against a real
/// in-process daemon; this test instead drives `try_forward_inner`
/// itself, so the single-deadline rewrite above is checked directly at
/// the same call site the write/read-timeout tests use.
#[tokio::test]
#[serial]
#[serial_test::serial(config_ledger)]
async fn try_forward_inner_normal_response_within_ceiling_still_succeeds() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }

    clear_daemon_env();
    let dir = tempfile::tempdir().expect("tempdir");
    let sock = dir.path().join("khived.sock");
    let pid_file = dir.path().join("khived.pid");

    std::env::set_var("KHIVE_SOCKET", &sock);
    std::env::set_var("KHIVE_PID", &pid_file);
    std::env::remove_var("KHIVE_NO_DAEMON");

    let config_id = "packs=[kg];db=:memory:;embed=none;extra=[];backend=main";

    async fn serve_one_ok_response(listener: tokio::net::UnixListener, config_id: String) {
        if let Ok((mut stream, _)) = listener.accept().await {
            let req = read_frame(&mut stream).await.expect("read request frame");
            let req: DaemonRequestFrame =
                serde_json::from_slice(&req).expect("decode request frame");
            assert_eq!(req.config_id, config_id);
            let resp = DaemonResponseFrame {
                ok: true,
                result: Some(serde_json::json!({"results": [], "summary": {}}).to_string()),
                error: None,
                error_detail: None,
                namespace_mismatch: false,
                config_mismatch: false,
                served_config_id: Some(req.config_id.clone()),
                version_mismatch: false,
                daemon_protocol_version: PROTOCOL_VERSION,
                metrics: None,
                request_id: req.request_id,
            };
            let payload = serde_json::to_vec(&resp).expect("serialize response frame");
            write_frame(&mut stream, &payload)
                .await
                .expect("write response frame");
        }
    }

    let listener = tokio::net::UnixListener::bind(&sock).expect("bind fake well-behaved socket");
    std::fs::write(&pid_file, std::process::id().to_string()).expect("write pid file");
    let fake_handle = tokio::spawn(serve_one_ok_response(listener, config_id.to_string()));

    let frame = DaemonRequestFrame {
        plan: false,
        ops: "stats()".to_string(),
        presentation: None,
        presentation_per_op: None,
        namespace: "test".to_string(),
        actor_id: None,
        process_ref: None,
        visible_namespaces: Vec::new(),
        config_id: config_id.to_string(),
        protocol_version: PROTOCOL_VERSION,
        probe_only: false,
        metrics_only: false,
        format: None,
        format_per_op: None,
        from_wire: false,
        request_id: Some(555),
    };

    let outcome =
        tokio::time::timeout(std::time::Duration::from_secs(5), try_forward_inner(&frame))
            .await
            .expect("try_forward_inner must not time out against a well-behaved peer");

    match outcome {
        ForwardOutcome::Response(resp) => {
            assert!(resp.ok, "well-behaved response must round-trip as ok=true");
            assert_eq!(resp.request_id, Some(555));
        }
        other => panic!("expected ForwardOutcome::Response, got {other:?}"),
    }

    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), fake_handle).await;
    clear_daemon_env();
}

// ── argv_is_khive_daemon unit tests ───────────────────────────────────────

#[test]
fn argv_daemon_true_bare() {
    // Exact daemon argv as spawned by spawn_daemon().
    assert!(argv_is_khive_daemon("kkernel mcp --daemon"));
}

#[test]
fn argv_daemon_true_absolute_path() {
    // Absolute path to kkernel binary — basename must match.
    assert!(argv_is_khive_daemon(
        "/Users/x/.cargo/bin/kkernel mcp --daemon"
    ));
}

#[test]
fn argv_daemon_false_editor_with_kkernel_in_filename() {
    // Editor opened on a file whose name contains kkernel — not a daemon.
    assert!(!argv_is_khive_daemon("vim kkernel-notes.md"));
}

#[test]
fn argv_daemon_false_less_with_kkernel_path() {
    // less paging a kkernel source file — argv[0] is "less", not "kkernel".
    assert!(!argv_is_khive_daemon(
        "less /Users/x/projects/kkernel/daemon.rs"
    ));
}

#[test]
fn argv_daemon_false_kkernel_no_daemon_flag() {
    // kkernel exec subcommand — has kkernel basename but no --daemon token.
    assert!(!argv_is_khive_daemon("kkernel exec 'something'"));
}

#[test]
fn argv_daemon_false_wrapper_argv0_not_kkernel() {
    // A wrapper script passes kkernel mcp --daemon as args but its own
    // argv[0] is "some-wrapper" — basename check must reject it.
    assert!(!argv_is_khive_daemon("some-wrapper kkernel mcp --daemon"));
}

#[test]
fn argv_daemon_false_empty_string() {
    assert!(!argv_is_khive_daemon(""));
}

#[test]
fn argv_daemon_true_with_surrounding_and_inner_whitespace() {
    assert!(argv_is_khive_daemon(
        "  /Users/x/.cargo/bin/kkernel   mcp    --daemon  "
    ));
}

#[test]
fn argv_daemon_true_kkernel_bench_basename() {
    // bench binary copy is named "kkernel-bench"; pid management must treat it
    // as a valid daemon so stale bench daemons are SIGTERM'd on respawn.
    assert!(argv_is_khive_daemon("kkernel-bench mcp --daemon"));
    assert!(argv_is_khive_daemon(
        "/Users/x/.cargo/bin/kkernel-bench mcp --daemon"
    ));
}

#[tokio::test]
#[serial]
async fn recovery_requires_incumbent_exit_before_spawning() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }

    let mut cleanup = RecoveryTestGuard::new();
    clear_daemon_env();
    reset_counters();
    let dir = tempfile::tempdir().expect("tempdir");
    let pid_file = dir.path().join("khived.pid");
    let ready_file = dir.path().join("incumbent.ready");

    std::env::set_var("KHIVE_SOCKET", dir.path().join("khived.sock"));
    std::env::set_var("KHIVE_PID", &pid_file);
    std::env::set_var("KHIVE_LOCK", dir.path().join("khived.recovery.lock"));
    std::env::set_var(
        "KHIVE_RECOVERER_LOCK",
        dir.path().join("khived.recoverer.lock"),
    );
    std::env::remove_var("KHIVE_NO_DAEMON");

    let incumbent = std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg("trap '' TERM; : > \"$1\"; while :; do sleep 1; done")
        .arg("stubborn-incumbent")
        .arg(&ready_file)
        .spawn()
        .expect("spawn signal-resistant incumbent");
    let incumbent_pid = cleanup.track_child(incumbent);
    let ready_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    while !ready_file.exists() {
        assert!(
            tokio::time::Instant::now() < ready_deadline,
            "incumbent did not install its signal handler"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    std::fs::write(&pid_file, incumbent_pid.to_string()).expect("write incumbent pid file");
    FORCE_PID_IS_DAEMON.store(true, std::sync::atomic::Ordering::SeqCst);

    let spawn_calls = std::sync::atomic::AtomicUsize::new(0);
    let spawn = || {
        spawn_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        std::process::Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
    };
    let outcome =
        kill_and_respawn_with_exit_timeout(CFG, NS, &spawn, std::time::Duration::from_millis(100))
            .await;
    let refused_pid = match outcome {
        Err(RecoveryError::IncumbentStillAlive { pid }) => Some(pid),
        Ok(RecoveryOutcome::Spawned(mut child)) => {
            let _ = child.wait();
            None
        }
        Ok(RecoveryOutcome::Skipped | RecoveryOutcome::Uncertain)
        | Err(
            RecoveryError::Spawn(_)
            | RecoveryError::RequestExpired
            | RecoveryError::PidFileDirectoryUntrusted(_),
        ) => None,
    };
    let live_pid_file_preserved = pid_file.exists();
    cleanup.kill_and_reap_child();

    assert_eq!(refused_pid, Some(incumbent_pid));
    assert_eq!(
        spawn_calls.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "a replacement must not spawn while the signalled incumbent PID is still alive"
    );
    assert!(
        live_pid_file_preserved,
        "the live incumbent's PID file must remain in place after refusal"
    );
    let refusal = incumbent_still_alive_error(incumbent_pid);
    assert!(refusal.message.contains(&format!("PID {incumbent_pid}")));
    let data = refusal.data.expect("live-incumbent refusal data");
    assert_eq!(data["reason"], "incumbent_still_alive");
    assert_eq!(data["pid"], incumbent_pid);

    let recovered =
        kill_and_respawn_with_exit_timeout(CFG, NS, &spawn, std::time::Duration::from_millis(100))
            .await;
    let spawned = match recovered {
        Ok(RecoveryOutcome::Spawned(mut child)) => {
            let _ = child.wait();
            true
        }
        _ => false,
    };

    FORCE_PID_IS_DAEMON.store(false, std::sync::atomic::Ordering::SeqCst);
    clear_daemon_env();
    assert!(spawned, "a confirmed-dead incumbent must still be replaced");
    assert_eq!(
        spawn_calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "confirmed-dead recovery must spawn exactly one replacement"
    );
    assert!(
        !pid_file.exists(),
        "the confirmed-dead incumbent PID file must be removed before spawning"
    );
}

#[tokio::test]
#[serial]
async fn recovery_replaces_stale_pid_without_waiting_on_live_foreign_process() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }

    let mut cleanup = RecoveryTestGuard::new();
    clear_daemon_env();
    reset_counters();
    let dir = tempfile::tempdir().expect("tempdir");
    let pid_file = dir.path().join("khived.pid");

    std::env::set_var("KHIVE_SOCKET", dir.path().join("khived.sock"));
    std::env::set_var("KHIVE_PID", &pid_file);
    std::env::set_var("KHIVE_LOCK", dir.path().join("khived.recovery.lock"));
    std::env::set_var(
        "KHIVE_RECOVERER_LOCK",
        dir.path().join("khived.recoverer.lock"),
    );
    std::env::remove_var("KHIVE_NO_DAEMON");

    let foreign = std::process::Command::new("/bin/sleep")
        .arg("30")
        .spawn()
        .expect("spawn live foreign process");
    let foreign_pid = cleanup.track_child(foreign);
    std::fs::write(&pid_file, foreign_pid.to_string()).expect("write foreign pid file");
    // Process inspection may be restricted in test environments, so force
    // the classification while retaining a live child to prove it is not killed.
    FORCE_PID_IS_FOREIGN.store(true, std::sync::atomic::Ordering::SeqCst);

    let spawn_calls = std::sync::atomic::AtomicUsize::new(0);
    let spawn = || {
        spawn_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        std::process::Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
    };

    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        kill_and_respawn(CFG, NS, &spawn),
    )
    .await
    .expect("recovery stalled on a PID positively identified as foreign")
    .expect("foreign-PID recovery failed");
    match outcome {
        RecoveryOutcome::Spawned(mut child) => {
            child.wait().expect("reap replacement fixture");
        }
        RecoveryOutcome::Skipped | RecoveryOutcome::Uncertain => {
            panic!("foreign-PID recovery did not spawn a replacement")
        }
    }

    assert_eq!(
        spawn_calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "foreign-PID recovery must spawn exactly one replacement"
    );
    assert!(
        !pid_file.exists(),
        "the stale foreign PID file must be removed before spawning"
    );
    assert!(
        cleanup
            .child_mut()
            .try_wait()
            .expect("query foreign child state")
            .is_none(),
        "recovery must not signal the live foreign process"
    );
}

// ── concurrent recovery — second client skips kill+spawn when daemon alive ──
//
// Exercises the recheck-under-lock (double-checked locking) in
// kill_and_respawn.  Scenario:
//
//   1. A real daemon is running (via run_daemon).
//   2. A recovering client calls kill_and_respawn directly — simulating a
//      client that observed pre-write `NoSocket`, but a concurrent first
//      recoverer has ALREADY
//      spawned a healthy daemon before this client reached the lock.
//   3. Under the lock, kill_and_respawn sends a probe_only frame and finds a
//      responsive, identity-matching daemon → returns RecoveryOutcome::Skipped.
//   4. KILL_COUNT must be 0 and SPAWN_COUNT must be 0.
//   5. The fresh daemon's PID file and socket must survive intact.
//
// FORCE_PID_IS_DAEMON=true makes every live PID SIGTERM-eligible so that
// if the bounded-probe is removed (reverted) kill_stale_daemon_inner would
// attempt SIGTERM against the real daemon PID, KILL_COUNT would be 1, and
// the assertion below would catch the regression.
//
// Fail-if-reverted assertion: the `assert_eq!(KILL_COUNT, 0)` below catches
// a reverted probe because without it, kill_stale_daemon_inner is called
// unconditionally and KILL_COUNT increments to 1.

#[tokio::test]
#[serial]
async fn concurrent_recovery_second_client_skips_kill_when_daemon_alive() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }

    clear_daemon_env();
    reset_counters();
    let dir = tempfile::tempdir().expect("tempdir");
    let sock = dir.path().join("khived.sock");
    let pid_file = dir.path().join("khived.pid");
    let lock_file = dir.path().join("khived.recovery.lock");

    std::env::set_var("KHIVE_SOCKET", &sock);
    std::env::set_var("KHIVE_PID", &pid_file);
    std::env::set_var("KHIVE_LOCK", &lock_file);
    std::env::remove_var("KHIVE_NO_DAEMON");

    // Run a real daemon so probe_daemon_identity in kill_and_respawn finds a
    // live, responsive, identity-matching daemon under the lock.
    let server = make_test_server();
    let config_id = server.config_id().to_string();
    let daemon_server = server.clone();
    let handle = tokio::spawn(async move {
        let _ = run_daemon(daemon_server).await;
    });

    // Wait for the daemon to bind the socket and write its PID file.
    let _ready = connect_when_ready(&sock).await;
    drop(_ready);

    // Record the daemon PID as written by run_daemon.
    let daemon_pid_str =
        std::fs::read_to_string(&pid_file).expect("daemon must have written a pid file");
    let daemon_pid: u32 = daemon_pid_str
        .trim()
        .parse()
        .expect("daemon pid file must contain a u32");

    // Arm the SIGTERM-eligible hook: classify_pid_identity() will now identify
    // the live daemon PID as khive. Without the bounded-probe, a reverted
    // kill_and_respawn would send SIGTERM to that PID and unlink the socket —
    // KILL_COUNT catches both paths.
    FORCE_PID_IS_DAEMON.store(true, std::sync::atomic::Ordering::SeqCst);
    reset_counters();

    // Call kill_and_respawn directly — simulates a second recovering client
    // whose turn arrives after the first recoverer already replaced the stale
    // daemon.  The bounded probe confirms the live daemon; Skipped is returned
    // without killing.
    let outcome = kill_and_respawn(&config_id, "test", &spawn_daemon).await;

    assert!(
        matches!(outcome, Ok(RecoveryOutcome::Skipped)),
        "kill_and_respawn must return Ok(RecoveryOutcome::Skipped) when a live \
             matching daemon exists under the lock"
    );
    assert_eq!(
        KILL_COUNT.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "KILL_COUNT must be 0: the probe found the daemon alive so \
             kill_stale_daemon_inner must NOT be called \
             (this assertion fails if the probe-under-lock is removed)"
    );
    assert_eq!(
        SPAWN_COUNT.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "SPAWN_COUNT must be 0: no respawn needed when the daemon is alive \
             (this assertion fails if the probe-under-lock is removed)"
    );

    // The daemon's PID file and socket must be intact.
    assert!(
        pid_file.exists(),
        "PID file must survive: kill_and_respawn must NOT unlink it"
    );
    assert!(
        sock.exists(),
        "socket must survive: kill_and_respawn must NOT unlink it"
    );
    let surviving_pid: u32 = std::fs::read_to_string(&pid_file)
        .expect("pid file readable")
        .trim()
        .parse()
        .expect("pid file is a u32");
    assert_eq!(
        surviving_pid, daemon_pid,
        "PID in file must be the original daemon PID — no new daemon was spawned"
    );

    handle.abort();
    let _ = handle.await;
    reset_counters();
    clear_daemon_env();
    std::env::remove_var("KHIVE_LOCK");
}

// ── oversized daemon response does not trigger kill/respawn ───────────────
//
// When the daemon's serialized response exceeds MAX_FRAME_BYTES the server
// sends a small explicit error frame (ok=false, "response too large") instead
// of closing the connection without a response.  The client receives this as
// ForwardOutcome::Response (decodable frame) → map_response → Some(Err(..));
// NOT the less-actionable ambiguous-forward ParseFailure error.
//
// This test drives the REAL handle_conn server path via run_daemon with a
// BigDispatch that returns a string larger than MAX_FRAME_BYTES.  The client
// calls forward_or_spawn and must receive Some(Err(..)) containing "too large".
// The daemon's PID file and socket must survive the call.
//
// Fail-if-reverted: if the handle_conn oversized gate (the `if payload.len()
// > MAX_FRAME_BYTES` branch that sends the small error frame) is removed,
// handle_conn falls through to write_frame with the oversized payload, which
// write_frame REJECTS (its own guard returns Err), causing the connection to
// close without a response → the client sees the generic terminal
// ambiguous-forward error instead of the precise "response too large"
// refusal asserted below.

/// A minimal DaemonDispatch that returns a payload larger than MAX_FRAME_BYTES
/// so handle_conn's oversized guard fires and emits the "response too large"
/// error frame.
#[derive(Clone)]
struct BigDispatch {
    namespace: String,
    config_id: String,
    calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait]
impl daemon::DaemonDispatch for BigDispatch {
    fn plan(&self, ops: &str) -> String {
        khive_request::plan_request(ops, &Default::default()).to_string()
    }

    async fn dispatch(
        &self,
        _ops: String,
        _presentation: Option<String>,
        _presentation_per_op: Option<Vec<Option<String>>>,
        _format: Option<String>,
        _format_per_op: Option<Vec<Option<String>>>,
        _from_wire: bool,
        _identity: Option<khive_runtime::RequestIdentity>,
    ) -> Result<String, String> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(serde_json::json!({"results": [
            {"ok": true, "tool": "create", "result": {
                "payload": "X".repeat(khive_runtime::daemon::MAX_FRAME_BYTES + 1),
            }},
            {"ok": false, "tool": "create", "error": {
                "kind": "internal", "message": "handler failed",
                "domain_disposition": "unknown",
            }},
        ]})
        .to_string())
    }

    async fn warm_all(&self) {}

    fn namespace(&self) -> &str {
        &self.namespace
    }

    fn config_id(&self) -> &str {
        &self.config_id
    }
}

#[tokio::test]
#[serial]
async fn oversized_daemon_response_sends_error_frame_not_kills_daemon() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }

    clear_daemon_env();
    reset_counters();
    let dir = tempfile::tempdir().expect("tempdir");
    let sock = dir.path().join("khived.sock");
    let pid_file = dir.path().join("khived.pid");
    let lock_file = dir.path().join("khived.recovery.lock");

    std::env::set_var("KHIVE_SOCKET", &sock);
    std::env::set_var("KHIVE_PID", &pid_file);
    std::env::set_var("KHIVE_LOCK", &lock_file);
    std::env::remove_var("KHIVE_NO_DAEMON");

    let config_id = "test-oversized-config";
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let dispatcher = BigDispatch {
        namespace: "test".to_string(),
        config_id: config_id.to_string(),
        calls: std::sync::Arc::clone(&calls),
    };

    // Run the real daemon server (with handle_conn's oversized gate live).
    let handle = tokio::spawn(async move {
        let _ = run_daemon(dispatcher).await;
    });

    // Wait for the daemon to bind and write its PID.
    let _ready = connect_when_ready(&sock).await;
    drop(_ready);

    let daemon_pid: u32 = std::fs::read_to_string(&pid_file)
        .expect("daemon must have written a pid file")
        .trim()
        .parse()
        .expect("daemon pid must be a u32");

    let frame = DaemonRequestFrame {
        plan: false,
        ops: "stats()".to_string(),
        presentation: None,
        presentation_per_op: None,
        namespace: "test".to_string(),
        actor_id: None,
        process_ref: None,
        visible_namespaces: Vec::new(),
        config_id: config_id.to_string(),
        protocol_version: PROTOCOL_VERSION,
        probe_only: false,
        metrics_only: false,
        format: None,
        format_per_op: None,
        from_wire: false,
        request_id: None,
    };

    let result = forward_or_spawn(&frame).await;

    // Primary assertion: the daemon's PID file and socket must be intact
    // (no kill/respawn triggered).
    assert!(
        pid_file.exists(),
        "PID file must survive: oversized response is NOT a daemon crash"
    );
    assert!(
        sock.exists(),
        "socket must survive: oversized response is NOT a daemon crash"
    );
    let surviving_pid: u32 = std::fs::read_to_string(&pid_file)
        .expect("pid file readable")
        .trim()
        .parse()
        .expect("pid file is a u32");
    assert_eq!(
        surviving_pid, daemon_pid,
        "daemon PID must not change — no kill+respawn occurred"
    );
    assert_eq!(
        KILL_COUNT.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "KILL_COUNT must be 0 — oversized response must NOT trigger \
             kill_stale_daemon_inner (fails if handle_conn oversized gate is removed)"
    );
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);

    // The result must be Some(Err(..)) containing "too large" — the explicit
    // error frame the server sends when the real response is oversized.
    match result {
        Some(Err(e)) => {
            assert!(
                e.message.contains("too large"),
                "error must describe the oversized response; got: {}",
                e.message
            );
            let detail = e.data.expect("frame-cap disposition");
            assert_eq!(detail["domain_disposition"], "unknown");
            assert_eq!(detail["code"], "response_frame_size_limit");
            assert!(detail.get("domain_result").is_none());
        }
        Some(Ok(_)) => panic!("oversized response must not produce Ok result"),
        None => panic!(
            "oversized response must produce Some(Err(..)) from the explicit \
                 error frame, not None (None would mean map_response fell back to \
                 local dispatch, hiding the server-side error)"
        ),
    }

    handle.abort();
    let _ = handle.await;
    reset_counters();
    clear_daemon_env();
    std::env::remove_var("KHIVE_LOCK");
}

// ── probe-only recovery primitive never dispatches a real request ─────────
//
// Scenario:
//   1. Recovery is invoked directly after an independently established
//      pre-write liveness failure (`NoSocket` is the only production caller).
//   2. Under the lock, probe_daemon_identity sends a probe_only frame to a
//      REAL CountingDispatch daemon (already running).  The daemon's
//      handle_conn returns an identity frame without calling dispatch() —
//      DAEMON_DISPATCH stays 0.
//   3. kill_and_respawn returns RecoveryOutcome::Skipped (live daemon found).
//   4. The call site forwards the REAL request once via try_forward_inner.
//      CountingDispatch.dispatch() is called → DAEMON_DISPATCH == 1.
//
// Fail-if-reverted: if kill_and_respawn is reverted to use the real frame as
// the probe (try_forward_inner(frame) under the lock), CountingDispatch.dispatch()
// is called for the probe (DAEMON_DISPATCH == 1), and again at the call site
// (DAEMON_DISPATCH == 2).  The assert_eq!(DAEMON_DISPATCH, 1) then fails.
//
// This intentionally exercises the recovery primitive directly. A
// post-write ParseFailure cannot reach it after #644; the parallel terminal
// test below guards that call-site boundary separately.

/// A minimal DaemonDispatch that increments DAEMON_DISPATCH on every real
/// (non-probe) dispatch.  probe_only frames never reach dispatch() — they
/// are short-circuited by handle_conn before calling the dispatcher.
#[derive(Clone)]
struct CountingDispatch {
    namespace: String,
    config_id: String,
}

#[async_trait]
impl daemon::DaemonDispatch for CountingDispatch {
    fn plan(&self, ops: &str) -> String {
        khive_request::plan_request(ops, &Default::default()).to_string()
    }

    async fn dispatch(
        &self,
        _ops: String,
        _presentation: Option<String>,
        _presentation_per_op: Option<Vec<Option<String>>>,
        _format: Option<String>,
        _format_per_op: Option<Vec<Option<String>>>,
        _from_wire: bool,
        _identity: Option<khive_runtime::RequestIdentity>,
    ) -> Result<String, String> {
        DAEMON_DISPATCH.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok("{\"ok\":true,\"counted\":true}".to_string())
    }

    async fn warm_all(&self) {}

    fn namespace(&self) -> &str {
        &self.namespace
    }

    fn config_id(&self) -> &str {
        &self.config_id
    }
}

#[tokio::test]
#[serial]
#[serial_test::serial(config_ledger)]
async fn recovery_path_dispatches_real_request_exactly_once() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }

    clear_daemon_env();
    reset_counters();
    let dir = tempfile::tempdir().expect("tempdir");
    let real_sock = dir.path().join("khived.sock");
    let stale_sock = dir.path().join("stale.sock");
    let pid_file = dir.path().join("khived.pid");
    let lock_file = dir.path().join("khived.recovery.lock");

    // Start a real CountingDispatch daemon on `real_sock`.
    let config_id = "packs=[kg];db=:memory:;embed=none;extra=[];backend=main";
    let counting_dispatcher = CountingDispatch {
        namespace: "test".to_string(),
        config_id: config_id.to_string(),
    };
    // Temporarily point KHIVE_SOCKET at real_sock to let run_daemon bind there.
    std::env::set_var("KHIVE_SOCKET", &real_sock);
    std::env::set_var("KHIVE_PID", &pid_file);
    std::env::set_var("KHIVE_LOCK", &lock_file);
    std::env::remove_var("KHIVE_NO_DAEMON");

    let counting_handle = tokio::spawn(async move {
        let _ = run_daemon(counting_dispatcher).await;
    });
    let _ready = connect_when_ready(&real_sock).await;
    drop(_ready);

    // Bind the stale fake socket on `stale_sock` BEFORE redirecting the client
    // env so the client sees it first.  The fake stale socket reads one frame
    // then drops without responding — simulating a crashed old daemon.
    let stale_listener = tokio::net::UnixListener::bind(&stale_sock).expect("bind stale socket");
    let stale_handle = tokio::spawn(serve_crash_on_dispatch(stale_listener));

    // The stale fixture documents the historical failure shape, while the
    // actual recovery assertion below starts from the live daemon path. A
    // real post-write ParseFailure no longer enters recovery.
    let stale_pid_file = dir.path().join("stale.pid");
    std::fs::write(&stale_pid_file, std::process::id().to_string()).expect("write stale pid");
    std::env::set_var("KHIVE_SOCKET", &stale_sock);
    std::env::set_var("KHIVE_PID", &stale_pid_file);

    // After kill_and_respawn's probe, the probe needs to reach the REAL daemon.
    // We achieve this by having the probe use `real_sock` (via KHIVE_SOCKET).
    // Redirect KHIVE_SOCKET back to the real daemon socket AFTER the stale
    // socket has been read (but we need the probe to already know where to look).
    //
    // Simpler approach: use a single socket for both — the stale response is
    // from a `serve_crash_on_dispatch` (closes after one read), the NEXT
    // connection attempt (the probe) goes to the same path, but the stale
    // listener is now gone.  Instead we use real_sock for the probe by
    // redirecting KHIVE_SOCKET before the probe fires.
    //
    // To get the "Skipped" (live daemon under lock) scenario without a real
    // spawn: call kill_and_respawn directly with KHIVE_SOCKET pointing at the
    // live CountingDispatch daemon, and separately assert DAEMON_DISPATCH from
    // a direct try_forward_inner call.

    // Point back at the real daemon for the probe and the real forward.
    std::env::set_var("KHIVE_SOCKET", &real_sock);
    std::env::set_var("KHIVE_PID", &pid_file);

    // Simulate the exactly-once scenario:
    //   (a) kill_and_respawn sees a live daemon → returns Skipped (0 dispatches)
    //   (b) call site forwards the real request once → 1 dispatch
    let recovery = kill_and_respawn(config_id, "test", &spawn_daemon).await;
    assert!(
        matches!(recovery, Ok(RecoveryOutcome::Skipped)),
        "probe must find the live CountingDispatch daemon and return Skipped"
    );
    // DAEMON_DISPATCH must still be 0: the probe_only frame does not reach dispatch().
    assert_eq!(
        DAEMON_DISPATCH.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "probe_only frame must NOT increment DAEMON_DISPATCH \
             (fails if the real request is used as the probe)"
    );

    // Now forward the real request exactly once — the call site's single forward.
    let real_frame = DaemonRequestFrame {
        plan: false,
        ops: "stats()".to_string(),
        presentation: None,
        presentation_per_op: None,
        namespace: "test".to_string(),
        actor_id: None,
        process_ref: None,
        visible_namespaces: Vec::new(),
        config_id: config_id.to_string(),
        protocol_version: PROTOCOL_VERSION,
        probe_only: false,
        metrics_only: false,
        format: None,
        format_per_op: None,
        from_wire: false,
        request_id: None,
    };
    let fwd = try_forward_inner(&real_frame).await;
    assert!(
        matches!(fwd, ForwardOutcome::Response(_)),
        "real forward after Skipped must succeed; got non-Response outcome"
    );

    // DAEMON_DISPATCH must now be exactly 1.
    assert_eq!(
        DAEMON_DISPATCH.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "real request must be dispatched EXACTLY ONCE across the recovery path \
             (assert fails with count==2 if the real frame is used as the probe \
              AND re-forwarded at the call site — the double-dispatch bug)"
    );

    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), stale_handle).await;
    counting_handle.abort();
    let _ = counting_handle.await;
    reset_counters();
    clear_daemon_env();
    std::env::remove_var("KHIVE_LOCK");
}

/// Serve every connection with a valid probe-ack response identity-matching
/// `config_id`, forever (until the listener is dropped/aborted). Simulates
/// an already-bound, healthy daemon that answers `probe_only` frames
/// immediately.
async fn serve_probe_ack_forever(listener: tokio::net::UnixListener, config_id: String) {
    loop {
        let Ok((mut stream, _)) = listener.accept().await else {
            return;
        };
        if read_frame(&mut stream).await.is_err() {
            continue;
        }
        let response = DaemonResponseFrame {
            ok: true,
            result: None,
            error: None,
            error_detail: None,
            namespace_mismatch: false,
            config_mismatch: false,
            served_config_id: Some(config_id.clone()),
            version_mismatch: false,
            daemon_protocol_version: PROTOCOL_VERSION,
            metrics: None,
            request_id: None,
        };
        if let Ok(payload) = serde_json::to_vec(&response) {
            let _ = write_frame(&mut stream, &payload).await;
        }
    }
}

// ── #758: confirm_genuinely_dead must not trust a bare Dead reading while
// a peer's boot is in flight ───────────────────────────────────────────
//
// Regression for the daemon-recovery double-spawn window: `spawn_daemon()`
// is fire-and-forget, so a concurrent recoverer's identity probe can
// observe `Dead` in the gap between a peer's `cmd.spawn()` returning and
// that child reaching its own `acquire_daemon_boot_guard()` call. This
// test drives that gap directly: a background OS thread holds the real
// boot/recovery lock for `GUARD_HOLD` (simulating "a peer's child is
// mid cold-boot, holding the same lock this process would need to
// classify it"), while a fake, already-bound, identity-matching listener
// answers probe_only frames the instant it is asked (simulating "the
// peer's child has already bound its socket and would answer this
// instant, if only the classifier would wait for the lock instead of
// trusting an immediate Dead reading").
//
// Fail-if-reverted: without the fix, `confirm_genuinely_dead` would not
// exist and `kill_and_respawn` would trust the bare `Dead` result
// immediately — this test exercises the new function directly, so
// reverting it is a compile error. The regression oracle is the `timeout`
// window below: it asserts `confirm_genuinely_dead` does NOT resolve
// while the peer explicitly still holds the lock, then asserts it DOES
// resolve (as `Alive`) once the peer explicitly releases it — real
// two-way synchronization via channels, not a fixed sleep + elapsed-time
// assertion (#838: the previous version held the guard
// via `std::thread::sleep` and asserted `elapsed >= GUARD_HOLD`, which is
// timing-dependent under load).
#[tokio::test]
#[serial]
async fn confirm_genuinely_dead_waits_for_peer_to_release_boot_guard() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }

    clear_daemon_env();
    reset_counters();
    let dir = tempfile::tempdir().expect("tempdir");
    let sock = dir.path().join("khived.sock");
    let pid_file = dir.path().join("khived.pid");
    let lock_file = dir.path().join("khived.recovery.lock");

    std::env::set_var("KHIVE_SOCKET", &sock);
    std::env::set_var("KHIVE_PID", &pid_file);
    std::env::set_var("KHIVE_LOCK", &lock_file);
    std::env::remove_var("KHIVE_NO_DAEMON");

    let server = make_test_server();
    let config_id = server.config_id().to_string();

    // A fake daemon is already reachable from T=0 — proving the wait
    // below is caused by the contended lock, not by the daemon being
    // slow to bind.
    let listener = tokio::net::UnixListener::bind(&sock).expect("bind fake daemon socket");
    std::fs::write(&pid_file, std::process::id().to_string()).expect("write fake pid file");
    let serve_handle = tokio::spawn(serve_probe_ack_forever(listener, config_id.clone()));

    // Real two-way synchronization: the boot-holder thread signals once it
    // has genuinely acquired the lock (so the test never proceeds before
    // contention is real), then blocks on an explicit release channel
    // instead of a fixed sleep — the test controls exactly when the lock
    // becomes available, with no timing guess involved.
    let (acquired_tx, acquired_rx) = std::sync::mpsc::channel::<()>();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let boot_thread = std::thread::spawn(move || {
        let guard = khive_runtime::daemon::acquire_daemon_boot_guard()
            .expect("test boot holder must acquire the recovery lock");
        acquired_tx.send(()).expect("signal lock acquired");
        let _ = release_rx.recv();
        drop(guard);
    });
    acquired_rx
        .recv()
        .expect("boot-holder thread must signal after acquiring the lock");

    let confirm_fut = confirm_genuinely_dead(&config_id, "test");
    tokio::pin!(confirm_fut);

    // Bounded assertion window (NOT the release mechanism — the lock is
    // released explicitly below via `release_tx`): proves
    // confirm_genuinely_dead does not resolve while the peer still holds
    // the lock.
    let too_early =
        tokio::time::timeout(std::time::Duration::from_millis(150), &mut confirm_fut).await;
    assert!(
        too_early.is_err(),
        "confirm_genuinely_dead must not resolve while a peer holds the \
             boot/recovery lock"
    );

    release_tx
        .send(())
        .expect("boot-holder thread still awaiting release");
    boot_thread
        .join()
        .expect("boot-holder thread must not panic");

    let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), confirm_fut)
        .await
        .expect("confirm_genuinely_dead must resolve promptly once the peer releases the lock");

    assert!(
        matches!(outcome, ProbeOutcome::Alive),
        "confirm_genuinely_dead must observe the already-reachable daemon \
             once the contended lock clears, not conclude Dead early"
    );

    serve_handle.abort();
    let _ = serve_handle.await;
    reset_counters();
    clear_daemon_env();
    std::env::remove_var("KHIVE_LOCK");
}

// ── #838: an earlier LockContended round must not be
// erased by a later Dead round ──────────────────────────────────────────
//
// `confirm_genuinely_dead` only trusts `Dead` once EVERY round agrees the
// daemon is absent. Before this fix, the aggregation tracked only the
// LAST round's outcome: a LockContended round followed by a later Dead
// round overwrote the earlier contention and the whole call returned
// `Dead`, which `kill_and_respawn` trusts enough to kill+spawn — even
// though quiescence was never actually established across every round.
//
// This test drives that exact sequence directly: it holds the real
// boot/recovery lock until round 1 reports `LockContended`, then releases
// it so the remaining rounds observe the (genuinely absent) daemon as `Dead`.
//
// Fail-if-reverted: with the old last-round-wins aggregation, this
// LockContended-then-Dead sequence resolves to `ProbeOutcome::Dead`, and
// the assertion below fails.
#[tokio::test]
#[serial]
async fn confirm_genuinely_dead_is_sticky_uncertain_after_earlier_contention() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }

    clear_daemon_env();
    reset_counters();
    let dir = tempfile::tempdir().expect("tempdir");
    let sock = dir.path().join("khived.sock");
    let pid_file = dir.path().join("khived.pid");
    let lock_file = dir.path().join("khived.recovery.lock");

    std::env::set_var("KHIVE_SOCKET", &sock);
    std::env::set_var("KHIVE_PID", &pid_file);
    std::env::set_var("KHIVE_LOCK", &lock_file);
    std::env::remove_var("KHIVE_NO_DAEMON");

    // Genuinely no daemon at all: no socket, no pid file. Once the lock
    // is free, every round's identity probe observes Dead.
    let config_id = "packs=[kg];db=:memory:;embed=none;extra=[];backend=main".to_string();

    // Hold the real boot/recovery lock from before `confirm_genuinely_dead`
    // starts until round 1 has actually reported its result.
    let mut guard = Some(
        khive_runtime::daemon::acquire_daemon_boot_guard()
            .expect("test lock holder must acquire the recovery lock"),
    );
    let mut observed_rounds = Vec::new();
    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        confirm_genuinely_dead_with_round_observer(&config_id, "test", |round, outcome| {
            observed_rounds.push((
                round,
                matches!(outcome, ProbeOutcome::LockContended),
                matches!(outcome, ProbeOutcome::Dead),
            ));
            if round == 0 {
                drop(guard.take());
            }
        }),
    )
    .await
    .expect("confirm_genuinely_dead must resolve once the lock is released");

    assert_eq!(observed_rounds.len(), DEAD_CONFIRM_ROUNDS as usize);
    assert_eq!(observed_rounds[0], (0, true, false));
    assert!(
        observed_rounds
            .iter()
            .skip(1)
            .all(|(_, contended, dead)| !contended && *dead),
        "every round after release must observe the absent daemon"
    );
    assert!(
        matches!(outcome, ProbeOutcome::LockContended),
        "an earlier LockContended round must make the whole call \
             LockContended (sticky), never overwritten by a later round's \
             Dead reading; got {outcome:?}"
    );
    assert_eq!(
        KILL_COUNT.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "confirm_genuinely_dead must never kill on its own"
    );
    assert_eq!(
        SPAWN_COUNT.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "confirm_genuinely_dead must never spawn on its own"
    );

    reset_counters();
    clear_daemon_env();
    std::env::remove_var("KHIVE_LOCK");
}

// ── #539/#544: real parallel recovery and terminal post-write failure ───

/// Eight clients independently observe `NoSocket`, rendezvous before the
/// recoverer lock, and launch the real in-process daemon server. The oracle
/// is one live, responsive owner after quiescence — not an exact spawn-call
/// count, because a launch can legitimately lose the server-side ownership
/// fence before it binds.
///
/// Deliberately NOT `launched_count() == 1`: racing launches are tolerated
/// by design. The recoverer lock is best-effort serialization (it shrinks
/// the raced-launch window; it is not the correctness mechanism), and the
/// server-side boot fence alone guarantees convergence — so removing the
/// lock would degrade this test to more raced launches without violating
/// its oracle, and that is the intended contract, not a coverage gap.
#[tokio::test(flavor = "multi_thread", worker_threads = 12)]
#[serial]
async fn parallel_no_socket_recovery_converges_to_one_usable_daemon() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }

    const CLIENTS: usize = 8;

    let _cleanup = RecoveryTestGuard::new();
    clear_daemon_env();
    reset_counters();
    *RECOVERY_RACE_BARRIER
        .lock()
        .expect("barrier mutex poisoned") =
        Some(std::sync::Arc::new(tokio::sync::Barrier::new(CLIENTS)));

    let dir = tempfile::tempdir().expect("tempdir");
    let sock = dir.path().join("khived.sock");
    let pid_file = dir.path().join("khived.pid");
    std::env::set_var("KHIVE_SOCKET", &sock);
    std::env::set_var("KHIVE_PID", &pid_file);
    std::env::set_var("KHIVE_LOCK", dir.path().join("khived.recovery.lock"));
    std::env::set_var(
        "KHIVE_RECOVERER_LOCK",
        dir.path().join("khived.recoverer.lock"),
    );
    std::env::remove_var("KHIVE_NO_DAEMON");

    let config_id = "packs=[kg];db=:memory:;embed=none;extra=[];backend=main";
    let dispatcher = HarnessDispatch::new("test", config_id);
    let launcher = InProcessDaemonLauncher::new(dispatcher.clone());
    let mut recoverers = tokio::task::JoinSet::new();
    for _ in 0..CLIENTS {
        let launcher = launcher.clone();
        recoverers.spawn(async move {
            kill_and_respawn_with_launcher(config_id, "test", &launcher).await
        });
    }

    let (spawned, skipped, uncertain) =
        tokio::time::timeout(std::time::Duration::from_secs(30), async move {
            let mut spawned: Vec<InProcessDaemonHandle> = Vec::new();
            let mut skipped = 0usize;
            let mut uncertain = 0usize;
            while let Some(result) = recoverers.join_next().await {
                match result.expect("recoverer task must not panic") {
                    Ok(RecoveryOutcome::Spawned(handle)) => spawned.push(handle),
                    Ok(RecoveryOutcome::Skipped) => skipped += 1,
                    Ok(RecoveryOutcome::Uncertain) => uncertain += 1,
                    Err(error) => panic!("parallel NoSocket recovery failed: {error:?}"),
                }
            }
            (spawned, skipped, uncertain)
        })
        .await
        .expect("parallel recoverers must quiesce within 30s");
    assert!(
        !spawned.is_empty(),
        "at least one recoverer must launch a daemon"
    );
    assert_eq!(
        spawned.len() + skipped + uncertain,
        CLIENTS,
        "every parallel recoverer must reach a classified outcome"
    );
    // Uncertain is a deadline-bound degraded outcome per
    // docs/api/daemon-lifecycle.md. The healthy parallel path resolves
    // losers as Skipped well inside the 16s deadline, so any Uncertain
    // here is a regression signal, not noise.
    assert_eq!(uncertain, 0, "healthy parallel recovery must not time out");
    assert_eq!(
        launcher.launched_count(),
        spawned.len(),
        "every launch attempt must return its owned test handle"
    );

    drop(connect_when_ready(&sock).await);
    let frame = DaemonRequestFrame {
        plan: false,
        ops: "stats()".to_string(),
        presentation: None,
        presentation_per_op: None,
        namespace: "test".to_string(),
        actor_id: None,
        process_ref: None,
        visible_namespaces: Vec::new(),
        config_id: config_id.to_string(),
        protocol_version: PROTOCOL_VERSION,
        probe_only: false,
        metrics_only: false,
        format: None,
        format_per_op: None,
        from_wire: false,
        request_id: None,
    };
    let response = exchange(&sock, &frame).await;
    assert!(
        response.ok,
        "the surviving daemon must answer stats(): {response:?}"
    );
    assert_eq!(
        dispatcher.dispatch_count(),
        1,
        "probe traffic must not dispatch; the one stats exchange must dispatch once"
    );

    launcher.wait_for_running_count(1).await;
    let handle_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    let active_handles = loop {
        let active = spawned
            .iter()
            .filter(|handle| !handle.is_finished())
            .count();
        if active == 1 {
            break active;
        }
        assert!(
            tokio::time::Instant::now() < handle_deadline,
            "launched daemon handles did not converge to one active task; active={active}"
        );
        tokio::task::yield_now().await;
    };
    assert_eq!(
        active_handles,
        1,
        "server-side ownership must converge to one live daemon handle; launched={} running={}",
        launcher.launched_count(),
        launcher.running_count()
    );
    assert!(
        sock.exists(),
        "the surviving daemon must own one socket path"
    );
    assert_eq!(
        std::fs::read_to_string(&pid_file)
            .expect("surviving pid file")
            .trim(),
        std::process::id().to_string(),
        "the surviving in-process daemon must own the sole PID rendezvous"
    );

    for handle in spawned {
        handle.stop().await;
    }
    launcher.wait_for_running_count(0).await;
    let _ = std::fs::remove_file(&sock);
    let _ = std::fs::remove_file(&pid_file);
    assert!(
        !sock.exists(),
        "test teardown must remove the daemon socket"
    );
    assert!(
        !pid_file.exists(),
        "test teardown must remove the daemon pid file"
    );
}

/// A post-write response loss is ambiguous and therefore terminal. All
/// clients return the exactly-once refusal, with no kill, spawn, retry, or
/// follow-up socket connection.
#[tokio::test(flavor = "multi_thread", worker_threads = 12)]
#[serial]
async fn parallel_parse_failure_is_terminal_and_never_recovers() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }

    const CLIENTS: usize = 8;

    let _cleanup = RecoveryTestGuard::new();
    clear_daemon_env();
    reset_counters();
    let dir = tempfile::tempdir().expect("tempdir");
    let sock = dir.path().join("khived.sock");
    std::env::set_var("KHIVE_SOCKET", &sock);
    std::env::set_var("KHIVE_PID", dir.path().join("khived.pid"));
    std::env::set_var("KHIVE_LOCK", dir.path().join("khived.recovery.lock"));
    std::env::set_var(
        "KHIVE_RECOVERER_LOCK",
        dir.path().join("khived.recoverer.lock"),
    );
    std::env::remove_var("KHIVE_NO_DAEMON");

    let listener = tokio::net::UnixListener::bind(&sock).expect("bind crash fixture socket");
    let release = std::sync::Arc::new(tokio::sync::Barrier::new(CLIENTS));
    let (done_tx, mut done_rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        let mut accepted = 0usize;
        let mut connections = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                _ = &mut done_rx => break,
                incoming = listener.accept() => {
                    let (mut stream, _) = incoming.expect("accept client frame");
                    accepted += 1;
                    if accepted > CLIENTS {
                        // A retry is itself the regression oracle. Do not put
                        // it into the first-wave barrier or teardown would
                        // deadlock waiting for another full cohort.
                        drop(stream);
                        continue;
                    }
                    let release = std::sync::Arc::clone(&release);
                    connections.spawn(async move {
                        read_frame(&mut stream).await.expect("read complete client frame");
                        release.wait().await;
                        // Drop without a response: the request may have dispatched,
                        // so the client must classify this as ParseFailure.
                    });
                }
            }
        }
        // Teardown race: done_tx fires the instant the last client has its
        // terminal error, but a regression-produced follow-up connect may
        // already sit queued in the listener backlog. Dropping the listener
        // immediately would let it escape uncounted. Quiesce-drain instead:
        // every first-wave connection is already accepted (done only fires
        // after all CLIENTS completed), so anything still arriving here is
        // a retry, and counting it makes `accepted == CLIENTS` below fail
        // the test rather than pass silently.
        while let Ok(incoming) =
            tokio::time::timeout(std::time::Duration::from_millis(250), listener.accept()).await
        {
            let (stream, _) = incoming.expect("accept client frame");
            accepted += 1;
            drop(stream);
        }
        while let Some(connection) = connections.join_next().await {
            connection.expect("crash fixture connection task must not panic");
        }
        accepted
    });

    let frame = std::sync::Arc::new(DaemonRequestFrame {
        plan: false,
        ops: "stats()".to_string(),
        presentation: None,
        presentation_per_op: None,
        namespace: "test".to_string(),
        actor_id: None,
        process_ref: None,
        visible_namespaces: Vec::new(),
        config_id: CFG.to_string(),
        protocol_version: PROTOCOL_VERSION,
        probe_only: false,
        metrics_only: false,
        format: None,
        format_per_op: None,
        from_wire: false,
        request_id: None,
    });
    let mut clients = tokio::task::JoinSet::new();
    for _ in 0..CLIENTS {
        let frame = std::sync::Arc::clone(&frame);
        clients.spawn(async move { forward_or_spawn(&frame).await });
    }

    let terminal_errors = tokio::time::timeout(std::time::Duration::from_secs(10), async move {
        let mut terminal_errors = 0usize;
        while let Some(result) = clients.join_next().await {
            match result.expect("client task must not panic") {
                Some(Err(error)) => {
                    assert!(
                        error
                            .message
                            .contains("response lost after request was sent"),
                        "ParseFailure must return the stable ambiguous-forward refusal: {error:?}"
                    );
                    terminal_errors += 1;
                }
                other => {
                    panic!("ParseFailure must be a terminal hard error, got {other:?}")
                }
            }
        }
        terminal_errors
    })
    .await
    .expect("parallel ParseFailure clients must terminate within 10s");
    done_tx.send(()).expect("crash fixture still running");
    let accepted = server.await.expect("crash fixture must not panic");

    assert_eq!(terminal_errors, CLIENTS);
    assert_eq!(
        accepted, CLIENTS,
        "terminal ParseFailure must not make any follow-up connection"
    );
    assert_eq!(
        KILL_COUNT.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "post-write ambiguity must never trigger daemon lifecycle actions"
    );
    assert_eq!(
        SPAWN_COUNT.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "post-write ambiguity must never spawn a replacement"
    );
}

// ── probe classifier is fail-CLOSED for same-protocol pre-probe daemons ──
//
// Regression test for the version-skew gap: a daemon built BEFORE probe_only
// was introduced but carrying PROTOCOL_VERSION (same numeric version, older
// binary) deserialises the probe frame via serde default and falls through to
// dispatch on the empty `ops` string.  It returns ok=false (parse error on
// empty ops) WITH matching identity fields (namespace / config / protocol all
// match).  Before this fix, probe_daemon_identity classified
// ANY response with matching identity as Alive, leaving the stale daemon in
// place.
//
// After the fix, the classifier requires the probe-ack sentinel shape:
//   resp.ok && resp.result.is_none() && resp.error.is_none()
// An ok=false response fails this predicate → Dead → kill+spawn.
//
// Fail-if-reverted: removing the `is_probe_ack` check from the classifier
// causes the ok=false identity-matching response to be classified Alive →
// kill_and_respawn returns Skipped → KILL_COUNT stays 0 → assertion fails.

/// Build a response that matches all identity fields but has ok=false (the
/// shape a pre-probe daemon produces when dispatching the empty-ops probe).
fn pre_probe_daemon_response(config_id: &str) -> DaemonResponseFrame {
    DaemonResponseFrame {
        ok: false,
        result: None,
        error: Some("parse error: empty ops string".to_string()),
        error_detail: None,
        namespace_mismatch: false,
        config_mismatch: false,
        served_config_id: Some(config_id.to_string()),
        version_mismatch: false,
        daemon_protocol_version: PROTOCOL_VERSION,
        metrics: None,
        request_id: None,
    }
}

#[tokio::test]
#[serial]
async fn probe_classifier_dead_when_same_protocol_daemon_lacks_probe_support() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }

    clear_daemon_env();
    reset_counters();
    let dir = tempfile::tempdir().expect("tempdir");
    let sock = dir.path().join("khived.sock");
    let pid_file = dir.path().join("khived.pid");
    let lock_file = dir.path().join("khived.recovery.lock");

    std::env::set_var("KHIVE_SOCKET", &sock);
    std::env::set_var("KHIVE_PID", &pid_file);
    std::env::set_var("KHIVE_LOCK", &lock_file);
    std::env::remove_var("KHIVE_NO_DAEMON");

    let config_id = "packs=[kg];db=:memory:;embed=none;extra=[];backend=main";

    // Bind a fake socket that serves one pre-probe response, then stops
    // accepting (simulates a same-protocol daemon without probe_only support).
    let listener = tokio::net::UnixListener::bind(&sock).expect("bind fake pre-probe socket");
    std::fs::write(&pid_file, std::process::id().to_string()).expect("write pid file");
    let pre_probe_resp = pre_probe_daemon_response(config_id);
    let fake_handle = tokio::spawn(serve_one_response(listener, pre_probe_resp));

    // Arm FORCE_PID_IS_DAEMON so kill_stale_daemon_inner would attempt SIGTERM
    // IF it were called.  This makes KILL_COUNT the reliable regression signal:
    // if the classifier incorrectly returns Alive (Skipped), kill is not called
    // and KILL_COUNT stays 0.
    FORCE_PID_IS_DAEMON.store(true, std::sync::atomic::Ordering::SeqCst);
    reset_counters();

    let outcome = kill_and_respawn(config_id, "test", &spawn_daemon).await;

    // The fake socket served exactly one response; join it before asserting.
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), fake_handle).await;

    // The ok=false response must NOT be classified as Alive.  kill_and_respawn
    // must attempt kill+spawn (Spawned outcome — spawn itself fails because there
    // is no real kkernel binary in test, but KILL_COUNT is checked BEFORE spawn).
    assert!(
        matches!(outcome, Ok(RecoveryOutcome::Spawned(_)) | Err(_)),
        "pre-probe same-protocol daemon must NOT be classified Alive; \
             expected Spawned or spawn-error, got Skipped"
    );
    assert_eq!(
        KILL_COUNT.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "KILL_COUNT must be 1 — the pre-probe response must classify as Dead \
             so kill_stale_daemon_inner is called \
             (this fails if is_probe_ack check is removed and ok=false response \
              is incorrectly classified as Alive)"
    );

    reset_counters();
    clear_daemon_env();
    std::env::remove_var("KHIVE_LOCK");
}

// ── dispatch error propagates to client as non-empty message (#91) ─────────
//
// Regression for #91: when the daemon's dispatcher returns Err(msg), the
// client-side `map_response` must surface that message through
// `forward_or_spawn` as `Some(Err(McpError { message, .. }))` with a
// non-empty `message`.  Before the #91 fix, some failure paths swallowed the
// message and the client saw only "daemon returned an error without a message".
//
// This test uses a real run_daemon + FailDispatch (always returns Err("forced
// dispatch error: <detail>")) and drives the full forward_or_spawn path so we
// exercise both the daemon's response serialization AND the client's
// map_response deserialization in one round trip.

#[derive(Clone)]
struct FailDispatch {
    namespace: String,
    config_id: String,
}

#[async_trait]
impl daemon::DaemonDispatch for FailDispatch {
    fn plan(&self, ops: &str) -> String {
        khive_request::plan_request(ops, &Default::default()).to_string()
    }

    async fn dispatch(
        &self,
        _ops: String,
        _presentation: Option<String>,
        _presentation_per_op: Option<Vec<Option<String>>>,
        _format: Option<String>,
        _format_per_op: Option<Vec<Option<String>>>,
        _from_wire: bool,
        _identity: Option<khive_runtime::RequestIdentity>,
    ) -> Result<String, String> {
        Err("forced dispatch error: verb returned an error for testing".to_string())
    }

    async fn warm_all(&self) {}

    fn namespace(&self) -> &str {
        &self.namespace
    }

    fn config_id(&self) -> &str {
        &self.config_id
    }
}

#[tokio::test]
#[serial]
async fn dispatch_error_propagates_as_non_empty_client_message() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }

    clear_daemon_env();
    let dir = tempfile::tempdir().expect("tempdir");
    let sock = dir.path().join("khived.sock");
    let pid_file = dir.path().join("khived.pid");
    let lock_file = dir.path().join("khived.recovery.lock");

    std::env::set_var("KHIVE_SOCKET", &sock);
    std::env::set_var("KHIVE_PID", &pid_file);
    std::env::set_var("KHIVE_LOCK", &lock_file);
    std::env::remove_var("KHIVE_NO_DAEMON");

    let config_id = "packs=[kg];db=:memory:;embed=none;extra=[];backend=main";
    let dispatcher = FailDispatch {
        namespace: "test".to_string(),
        config_id: config_id.to_string(),
    };

    let handle = tokio::spawn(async move {
        let _ = run_daemon(dispatcher).await;
    });

    let _ready = connect_when_ready(&sock).await;
    drop(_ready);

    let frame = DaemonRequestFrame {
        plan: false,
        ops: "stats()".to_string(),
        presentation: None,
        presentation_per_op: None,
        namespace: "test".to_string(),
        actor_id: None,
        process_ref: None,
        visible_namespaces: Vec::new(),
        config_id: config_id.to_string(),
        protocol_version: PROTOCOL_VERSION,
        probe_only: false,
        metrics_only: false,
        format: None,
        format_per_op: None,
        from_wire: false,
        request_id: None,
    };

    let result = forward_or_spawn(&frame).await;

    match result {
        Some(Err(McpError { message, .. })) => {
            assert!(
                !message.is_empty(),
                "error message forwarded to client must not be empty"
            );
            assert!(
                message.contains("forced dispatch error"),
                "client must receive the daemon's error message verbatim; got: {message}"
            );
        }
        Some(Ok(v)) => panic!("FailDispatch always errs; got Ok({v:?})"),
        None => panic!(
            "forward_or_spawn returned None (local fallback) instead of \
                 propagating the daemon's error — the error message was swallowed"
        ),
    }

    handle.abort();
    let _ = handle.await;
    clear_daemon_env();
    std::env::remove_var("KHIVE_LOCK");
}

// ── explicit version_mismatch is terminal after the real write (#156) ─────
//
// Regression for #156: when a NEWER client connects to an OLD warm daemon,
// the old daemon responds with `version_mismatch=true` and its own (lower)
// `daemon_protocol_version`. Before the fix, this went to the generic
// `Response` arm → `map_response` → hard MCP error without a stable
// stale-daemon classification.
//
// After the fix, `try_forward_inner` detects `version_mismatch=true` &&
// `daemon_protocol_version < PROTOCOL_VERSION` and returns
// `ForwardOutcome::ProtocolMismatch`, routing it through the terminal
// no-retry/no-lifecycle path exactly like the implicit old-daemon case.
//
// This test serves one connection returning the protocol-v3 shape that a
// still-warm pre-process_ref daemon reports to a v4 client
// (version_mismatch=true, daemon_protocol_version=3) and asserts that
// try_forward_inner classifies it as ProtocolMismatch (not Response).

fn explicit_version_mismatch_response(config_id: &str) -> DaemonResponseFrame {
    DaemonResponseFrame {
        ok: false,
        result: None,
        error: Some(format!(
            "daemon protocol mismatch: client={} daemon=3 — \
                 rebuild/update the client binary (make local)",
            PROTOCOL_VERSION
        )),
        error_detail: None,
        namespace_mismatch: false,
        config_mismatch: false,
        served_config_id: Some(config_id.to_string()),
        version_mismatch: true,
        daemon_protocol_version: 3,
        metrics: None,
        request_id: None,
    }
}

#[tokio::test]
#[serial]
async fn current_client_rejects_warm_v3_daemon_before_accepting_result() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }

    clear_daemon_env();
    const {
        assert!(
            PROTOCOL_VERSION >= 4,
            "process_ref requires protocol v4 or later"
        )
    };
    let dir = tempfile::tempdir().expect("tempdir");
    let sock = dir.path().join("khived.sock");
    let pid_file = dir.path().join("khived.pid");

    std::env::set_var("KHIVE_SOCKET", &sock);
    std::env::set_var("KHIVE_PID", &pid_file);
    std::env::remove_var("KHIVE_NO_DAEMON");

    let config_id = "packs=[kg];db=:memory:;embed=none;extra=[];backend=main";

    let listener = tokio::net::UnixListener::bind(&sock).expect("bind explicit-mismatch socket");
    std::fs::write(&pid_file, std::process::id().to_string()).expect("write pid file");

    let mismatch_resp = explicit_version_mismatch_response(config_id);
    let fake_handle = tokio::spawn(serve_one_response(listener, mismatch_resp));

    let frame = DaemonRequestFrame {
        plan: false,
        ops: "stats()".to_string(),
        presentation: None,
        presentation_per_op: None,
        namespace: "test".to_string(),
        actor_id: None,
        process_ref: None,
        visible_namespaces: Vec::new(),
        config_id: config_id.to_string(),
        protocol_version: PROTOCOL_VERSION,
        probe_only: false,
        metrics_only: false,
        format: None,
        format_per_op: None,
        from_wire: false,
        request_id: None,
    };

    let outcome = try_forward_inner(&frame).await;

    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), fake_handle).await;

    assert!(
        matches!(outcome, ForwardOutcome::ProtocolMismatch { .. }),
        "explicit version_mismatch=true with daemon_protocol_version < PROTOCOL_VERSION \
             must classify as ProtocolMismatch (terminal no-retry path), not Response \
             (which would lose the stable stale-daemon classification)"
    );

    clear_daemon_env();
}

// ── explicit version_mismatch from NEWER daemon is NOT routed to recovery ──
//
// Complementary to the test above: when a stale CLIENT talks to a NEWER
// daemon, the daemon responds with `version_mismatch=true` and a
// `daemon_protocol_version > PROTOCOL_VERSION`. Kill+respawn cannot fix this
// (it would just spawn the same newer daemon again); the client must receive
// a hard error telling the operator to upgrade the client binary.
//
// This test asserts try_forward_inner returns ForwardOutcome::Response
// (not ProtocolMismatch) so map_response produces the hard error.

#[tokio::test]
#[serial]
async fn try_forward_inner_behind_a_newer_daemon_yields_protocol_mismatch() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }

    clear_daemon_env();
    let dir = tempfile::tempdir().expect("tempdir");
    let sock = dir.path().join("khived.sock");
    let pid_file = dir.path().join("khived.pid");

    std::env::set_var("KHIVE_SOCKET", &sock);
    std::env::set_var("KHIVE_PID", &pid_file);
    std::env::remove_var("KHIVE_NO_DAEMON");

    let config_id = "packs=[kg];db=:memory:;embed=none;extra=[];backend=main";

    let listener = tokio::net::UnixListener::bind(&sock).expect("bind newer-daemon socket");
    std::fs::write(&pid_file, std::process::id().to_string()).expect("write pid file");

    let mismatch_resp = newer_daemon_response(config_id);
    let fake_handle = tokio::spawn(serve_one_response(listener, mismatch_resp));

    let frame = DaemonRequestFrame {
        plan: false,
        ops: "stats()".to_string(),
        presentation: None,
        presentation_per_op: None,
        namespace: "test".to_string(),
        actor_id: None,
        process_ref: None,
        visible_namespaces: Vec::new(),
        config_id: config_id.to_string(),
        protocol_version: PROTOCOL_VERSION,
        probe_only: false,
        metrics_only: false,
        format: None,
        format_per_op: None,
        from_wire: false,
        request_id: None,
    };

    let outcome = try_forward_inner(&frame).await;

    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), fake_handle).await;

    assert!(
        matches!(
            outcome,
            ForwardOutcome::ProtocolMismatch {
                daemon_protocol_version
            } if daemon_protocol_version == PROTOCOL_VERSION + 1
        ),
        "a daemon ahead of this bridge yields ProtocolMismatch carrying the daemon's \
             version, so the bridge answers the caller and then re-execs the current \
             binary; the version_mismatch flag on the frame does not decide this"
    );

    clear_daemon_env();
}

// ── #644: ambiguous post-write outcome never retries or falls back ───────
//
// Before the #644 fix, a `ParseFailure` on the first `try_forward_inner`
// attempt (the real frame was already fully written) triggered
// `kill_and_respawn` followed by resending the SAME real frame to whatever
// daemon answered next. If the original (stale) daemon had actually
// dispatched the mutation before the connection dropped, that retry would
// execute it a second time on the freshly-spawned daemon.
//
// This test proves the fixed contract: once the real frame is confirmed
// written, `forward_or_spawn` returns a hard error and never opens another
// connection — not even to a daemon that is fully ready and willing to
// serve the exact same request.

#[tokio::test]
#[serial]
#[serial_test::serial(config_ledger)]
async fn ambiguous_write_never_retries_against_freshly_spawned_daemon() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }

    clear_daemon_env();
    let dir = tempfile::tempdir().expect("tempdir");
    let sock = dir.path().join("khived.sock");
    let pid_file = dir.path().join("khived.pid");
    let lock_file = dir.path().join("khived.recovery.lock");

    let config_id = "packs=[kg];db=:memory:;embed=none;extra=[];backend=main";

    // Stale daemon: accepts one connection, reads the request, then drops
    // without responding — forces the first try_forward_inner to see
    // ParseFailure (frame written, response lost).
    let stale_listener = tokio::net::UnixListener::bind(&sock).expect("bind stale socket");
    let stale_handle = tokio::spawn(serve_crash_on_dispatch(stale_listener));

    std::fs::write(&pid_file, std::process::id().to_string()).expect("write pid file");

    std::env::set_var("KHIVE_SOCKET", &sock);
    std::env::set_var("KHIVE_PID", &pid_file);
    std::env::set_var("KHIVE_LOCK", &lock_file);
    std::env::remove_var("KHIVE_NO_DAEMON");

    // A fully ready "freshly respawned" daemon binds shortly after the
    // stale one drops. If the fix regresses and forward_or_spawn retries
    // the real frame, this listener would happily answer it — that's
    // exactly why connect_count must stay 0.
    let connect_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let connect_count_srv = connect_count.clone();
    let resp = frame_ok("stats-result");
    let fresh_sock = sock.clone();
    let fresh_handle = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let listener =
            tokio::net::UnixListener::bind(&fresh_sock).expect("bind fresh daemon socket");
        if let Ok((mut stream, _)) = listener.accept().await {
            connect_count_srv.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if read_frame(&mut stream).await.is_ok() {
                if let Ok(payload) = serde_json::to_vec(&resp) {
                    let _ = write_frame(&mut stream, &payload).await;
                }
            }
        }
    });

    let frame = DaemonRequestFrame {
        plan: false,
        ops: "stats()".to_string(),
        presentation: None,
        presentation_per_op: None,
        namespace: "test".to_string(),
        actor_id: None,
        process_ref: None,
        visible_namespaces: Vec::new(),
        config_id: config_id.to_string(),
        protocol_version: PROTOCOL_VERSION,
        probe_only: false,
        metrics_only: false,
        format: None,
        format_per_op: None,
        from_wire: false,
        request_id: None,
    };

    let result = forward_or_spawn(&frame).await;

    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), stale_handle).await;
    // Give the never-contacted fresh listener a moment to prove it stays idle,
    // then drop it so its task doesn't hang the test process.
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    fresh_handle.abort();
    let _ = fresh_handle.await;

    match result {
        Some(Err(McpError { message, .. })) => {
            assert!(
                message.contains("not retrying") && message.contains("duplicate execution"),
                "ambiguous post-write outcome must return the #644 hard-error \
                     message; got: {message}"
            );
        }
        other => {
            panic!("expected Some(Err(..)) for an ambiguous post-write outcome, got {other:?}")
        }
    }

    assert_eq!(
        connect_count.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "forward_or_spawn must NOT contact any daemon (stale or freshly \
             spawned) again once the real frame has been fully written — \
             retrying risks a duplicate dispatch (#644)"
    );

    clear_daemon_env();
    std::env::remove_var("KHIVE_LOCK");
}

// ── #644: exactly-once dispatch through the full forward_or_spawn path ───
//
// End-to-end version of `recovery_path_dispatches_real_request_exactly_once`
// (which drives `kill_and_respawn` + `try_forward_inner` directly to avoid
// a two-socket setup problem). This test drives the full public entry point:
// a single fake daemon counts every real (non-probe) dispatch, answers
// `probe_only` frames with a valid identity ack, and closes the connection
// without responding to the real frame — simulating a crash after dispatch.
//
// Fail-if-reverted: if `forward_or_spawn` ever resends the real frame after
// a confirmed write (the #644 bug), this fake daemon would dispatch it
// again and DAEMON_DISPATCH would read 2, not 1.

#[tokio::test]
#[serial]
async fn forward_or_spawn_dispatches_real_frame_exactly_once_end_to_end() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }

    clear_daemon_env();
    reset_counters();
    let dir = tempfile::tempdir().expect("tempdir");
    let sock = dir.path().join("khived.sock");
    let pid_file = dir.path().join("khived.pid");
    let lock_file = dir.path().join("khived.recovery.lock");

    std::env::set_var("KHIVE_SOCKET", &sock);
    std::env::set_var("KHIVE_PID", &pid_file);
    std::env::set_var("KHIVE_LOCK", &lock_file);
    std::env::remove_var("KHIVE_NO_DAEMON");

    let config_id = "packs=[kg];db=:memory:;embed=none;extra=[];backend=main";

    // `forward_or_spawn`'s first attempt writes the real (non-probe) frame
    // directly — no probe precedes it — so this fake daemon only needs to
    // simulate "read the real frame, dispatch it, then crash before
    // responding" for the very first connection.
    let listener = tokio::net::UnixListener::bind(&sock).expect("bind fake daemon socket");
    std::fs::write(&pid_file, std::process::id().to_string()).expect("write pid file");
    let dispatch_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let dispatch_count_srv = dispatch_count.clone();
    let fake_handle = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            if read_frame(&mut stream).await.is_err() {
                continue;
            }
            // Count the dispatch, then drop the connection without
            // responding — simulating a crash after the mutation already ran.
            dispatch_count_srv.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            drop(stream);
        }
    });

    let frame = DaemonRequestFrame {
        plan: false,
        ops: "stats()".to_string(),
        presentation: None,
        presentation_per_op: None,
        namespace: "test".to_string(),
        actor_id: None,
        process_ref: None,
        visible_namespaces: Vec::new(),
        config_id: config_id.to_string(),
        protocol_version: PROTOCOL_VERSION,
        probe_only: false,
        metrics_only: false,
        format: None,
        format_per_op: None,
        from_wire: false,
        request_id: None,
    };

    let result = forward_or_spawn(&frame).await;

    match result {
        Some(Err(_)) => {}
        other => panic!(
            "expected Some(Err(..)) — not None (silent local fallback) — for a \
                 dispatch-then-crash response, got {other:?}"
        ),
    }
    assert_eq!(
        dispatch_count.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the real request must be dispatched EXACTLY ONCE; a value of 2 means \
             forward_or_spawn resent the frame after the write already completed"
    );

    fake_handle.abort();
    let _ = fake_handle.await;
    reset_counters();
    clear_daemon_env();
    std::env::remove_var("KHIVE_LOCK");
}

// ── #644 boundary: a successful daemon round trip dispatches exactly once ──
//
// The crash-after-dispatch test above proves the `Err` boundary (a lost
// response must never trigger a resend). This test proves the opposite
// boundary on the SAME counter: when the daemon successfully answers, the
// real frame must still have been dispatched exactly once — not retried
// after a successful response, and not dispatched a second time by any
// fallback path once `forward_or_spawn` already returns `Some(Ok(_))`.
//
// Fail-if-reverted: if a future edit ever re-sent the real frame after a
// successful response (e.g. a stray retry-on-timeout wrapped around the
// whole attempt), this fake daemon would observe DISPATCH_COUNT == 2.
#[tokio::test]
#[serial]
async fn forward_or_spawn_dispatches_real_frame_exactly_once_on_success() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }

    clear_daemon_env();
    reset_counters();
    let dir = tempfile::tempdir().expect("tempdir");
    let sock = dir.path().join("khived.sock");
    let pid_file = dir.path().join("khived.pid");
    let lock_file = dir.path().join("khived.recovery.lock");

    std::env::set_var("KHIVE_SOCKET", &sock);
    std::env::set_var("KHIVE_PID", &pid_file);
    std::env::set_var("KHIVE_LOCK", &lock_file);
    std::env::remove_var("KHIVE_NO_DAEMON");

    let config_id = "packs=[kg];db=:memory:;embed=none;extra=[];backend=main";

    let listener = tokio::net::UnixListener::bind(&sock).expect("bind fake daemon socket");
    std::fs::write(&pid_file, std::process::id().to_string()).expect("write pid file");
    let dispatch_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let dispatch_count_srv = dispatch_count.clone();
    let cfg_for_srv = config_id.to_string();
    let fake_handle = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            if read_frame(&mut stream).await.is_err() {
                continue;
            }
            dispatch_count_srv.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let resp = DaemonResponseFrame {
                ok: true,
                result: Some("daemon-handled-stats".to_string()),
                error: None,
                error_detail: None,
                namespace_mismatch: false,
                config_mismatch: false,
                served_config_id: Some(cfg_for_srv.clone()),
                version_mismatch: false,
                daemon_protocol_version: PROTOCOL_VERSION,
                metrics: None,
                request_id: None,
            };
            let payload = serde_json::to_vec(&resp).expect("serialize response frame");
            let _ = write_frame(&mut stream, &payload).await;
        }
    });

    let frame = DaemonRequestFrame {
        plan: false,
        ops: "stats()".to_string(),
        presentation: None,
        presentation_per_op: None,
        namespace: "test".to_string(),
        actor_id: None,
        process_ref: None,
        visible_namespaces: Vec::new(),
        config_id: config_id.to_string(),
        protocol_version: PROTOCOL_VERSION,
        probe_only: false,
        metrics_only: false,
        format: None,
        format_per_op: None,
        from_wire: false,
        request_id: None,
    };

    let result = forward_or_spawn(&frame).await;

    match result {
        Some(Ok(ref body)) => {
            assert_eq!(
                body, "daemon-handled-stats",
                "request() must surface the daemon's response verbatim"
            );
        }
        other => {
            panic!("expected Some(Ok(_)) for a successful daemon round trip, got {other:?}")
        }
    }
    assert_eq!(
        dispatch_count.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "a successful daemon round trip must dispatch the real request EXACTLY \
             ONCE; a value other than 1 means forward_or_spawn retried or double-sent \
             the frame around a successful response"
    );

    fake_handle.abort();
    let _ = fake_handle.await;
    reset_counters();
    clear_daemon_env();
    std::env::remove_var("KHIVE_LOCK");
}

// ── #667: readiness-timeout fallback must wait for boot quiescence ────────
//
// Before this fix, `forward_or_spawn`'s post-respawn readiness loop treated
// a bare deadline elapse as "no daemon" and returned `None` (silent local
// fallback) unconditionally — even if a concurrent process was still
// holding the cold-boot guard (ADR-D3) running migrations / pack schema
// plans (FTS DDL included). A local writer/searcher racing in at exactly
// that moment could observe or create a partially-initialized
// `notes`/`fts_notes` schema (#667).
//
// Nothing ever binds the socket in this test (no `NoSocket` outcome here
// ever writes a real frame, so #644's at-most-once invariant is untouched
// by this scenario) — `kill_and_respawn`'s own probe therefore sees
// `NoSocket` too, classifies the (nonexistent) daemon as `Dead`, and calls
// the real `spawn_daemon()` (which forks this test binary with `mcp
// --daemon`; the child fails to parse those as libtest args and exits
// almost immediately without ever binding anything — the same tolerated
// pattern already used by
// `try_forward_inner_returns_parse_failure_when_daemon_closes_without_response`
// in this file).
//
// `SPAWN_COUNT` (already incremented for real inside `spawn_daemon`) is
// used purely as a **synchronization signal**: it tells the background
// "child boot" thread below that `kill_and_respawn`'s own (much shorter)
// use of the recovery lock is in its final moments, so the boot thread's
// blocking `acquire_daemon_boot_guard()` call queues immediately behind
// it via the real `flock`, rather than racing it — the two never expect
// to hold the lock at the same time, and the ordering between them is
// never guessed at with a fixed sleep.
//
// Once the boot thread holds the guard (for `GUARD_HOLD`, deliberately
// longer than `forward_or_spawn`'s fixed 5s readiness deadline), the fix
// must block inside `wait_for_boot_quiescence_then_reprobe` until that
// guard is released before it is allowed to decide "genuinely no daemon".
// The elapsed-time assertion below is the fail-if-reverted oracle for
// #667: reverting that fence makes `forward_or_spawn` return right at the
// 5s readiness deadline (measured from well before the boot thread even
// starts holding the guard), strictly before `GUARD_HOLD` has elapsed.
//
// #898: by the time that wait finally ends, THIS call's own spawned child
// (the test binary, rejected `mcp --daemon` and exited within
// milliseconds) has long since exited — so the final outcome is now the
// loud, specific respawn-failure error rather than a silent `None`. That
// change is orthogonal to what this test actually guards: the fence must
// still be waited out in full regardless of which terminal outcome
// follows it, which the elapsed-time assertion below continues to prove.
#[tokio::test]
#[serial]
async fn forward_or_spawn_blocks_on_boot_quiescence_before_local_fallback() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }

    clear_daemon_env();
    reset_counters();
    let dir = tempfile::tempdir().expect("tempdir");
    let sock = dir.path().join("khived.sock");
    let pid_file = dir.path().join("khived.pid");
    let lock_file = dir.path().join("khived.recovery.lock");

    std::env::set_var("KHIVE_SOCKET", &sock);
    std::env::set_var("KHIVE_PID", &pid_file);
    std::env::set_var("KHIVE_LOCK", &lock_file);
    std::env::remove_var("KHIVE_NO_DAEMON");

    let config_id = "packs=[kg];db=:memory:;embed=none;extra=[];backend=main";

    const GUARD_HOLD: std::time::Duration = std::time::Duration::from_secs(6);
    let boot_thread = std::thread::spawn(|| {
        let wait_deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while SPAWN_COUNT.load(std::sync::atomic::Ordering::SeqCst) == 0 {
            assert!(
                std::time::Instant::now() < wait_deadline,
                "kill_and_respawn never reached spawn_daemon() within 5s; \
                     the boot-holder thread has nothing to queue behind"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        // `kill_and_respawn` is at most a few milliseconds from dropping
        // its own lock guard at this point (spawn_daemon() is its last
        // action before returning) — this blocks on the real `flock`
        // until that happens, then holds it for GUARD_HOLD.
        let guard = khive_runtime::daemon::acquire_daemon_boot_guard()
            .expect("test boot holder must acquire the recovery lock");
        std::thread::sleep(GUARD_HOLD);
        drop(guard);
    });

    let frame = DaemonRequestFrame {
        plan: false,
        ops: "stats()".to_string(),
        presentation: None,
        presentation_per_op: None,
        namespace: "test".to_string(),
        actor_id: None,
        process_ref: None,
        visible_namespaces: Vec::new(),
        config_id: config_id.to_string(),
        protocol_version: PROTOCOL_VERSION,
        probe_only: false,
        metrics_only: false,
        format: None,
        format_per_op: None,
        from_wire: false,
        request_id: None,
    };

    let started = std::time::Instant::now();
    let result = forward_or_spawn(&frame).await;
    let elapsed = started.elapsed();

    boot_thread
        .join()
        .expect("boot-holder thread must not panic");

    match &result {
        Some(Err(McpError { message, .. })) => {
            assert!(
                message.contains("respawn failed"),
                "#898: this call's own spawned child is confirmed dead by \
                     now, so the outcome must be the specific loud respawn- \
                     failure error, not a generic message: {message}"
            );
        }
        other => panic!(
            "after boot quiescence, a respawn attempt confirmed dead must \
                 surface loudly (#898) rather than falling back silently, got \
                 {other:?}"
        ),
    }
    assert!(
        elapsed >= GUARD_HOLD,
        "forward_or_spawn must block until the cold-boot guard is released \
             before deciding to fall back locally — returned after {elapsed:?}, \
             faster than the {GUARD_HOLD:?} the boot guard was held, meaning it \
             (or a reverted version of this fix) would local-dispatch while \
             cold-boot schema init could still be in progress (#667)"
    );

    reset_counters();
    clear_daemon_env();
    std::env::remove_var("KHIVE_LOCK");
}

// ── daemon stderr log-file helpers (no process spawned) ───────────────────

#[test]
fn daemon_log_path_from_home_none_when_home_unset() {
    assert!(daemon_log_path_from_home(None).is_none());
}

#[test]
fn daemon_log_path_from_home_joins_dot_khive_logs() {
    let home = std::ffi::OsStr::new("/home/example");
    let path = daemon_log_path_from_home(Some(home)).expect("home present");
    assert_eq!(
        path,
        std::path::PathBuf::from("/home/example/.khive/logs/khived.log")
    );
}

#[test]
fn daemon_log_should_rotate_under_cap_is_false() {
    assert!(!daemon_log_should_rotate(100, 1000));
}

#[test]
fn daemon_log_should_rotate_at_cap_is_true() {
    assert!(daemon_log_should_rotate(1000, 1000));
}

#[test]
fn daemon_log_should_rotate_over_cap_is_true() {
    assert!(daemon_log_should_rotate(1001, 1000));
}

#[test]
fn prepare_daemon_log_file_creates_dir_and_file_on_first_use() {
    let dir = tempfile::tempdir().expect("tempdir");
    let log_path = dir.path().join(".khive").join("logs").join("khived.log");

    let file = prepare_daemon_log_file_with_cap(&log_path, DAEMON_LOG_MAX_BYTES);

    assert!(file.is_some(), "must create dir + file on first use");
    assert!(log_path.exists());
}

#[test]
fn prepare_daemon_log_file_leaves_existing_when_under_cap() {
    let dir = tempfile::tempdir().expect("tempdir");
    let log_dir = dir.path().join("logs");
    std::fs::create_dir_all(&log_dir).expect("create log dir");
    let log_path = log_dir.join("khived.log");
    std::fs::write(&log_path, b"existing content\n").expect("seed existing log");

    let file = prepare_daemon_log_file_with_cap(&log_path, 1_000_000);

    assert!(file.is_some());
    assert!(
        !log_dir.join("khived.log.1").exists(),
        "under-cap log must not be rotated"
    );
    let content = std::fs::read_to_string(&log_path).expect("read log");
    assert_eq!(
        content, "existing content\n",
        "append-open must preserve existing content"
    );
}

#[test]
fn prepare_daemon_log_file_rotates_when_over_cap() {
    let dir = tempfile::tempdir().expect("tempdir");
    let log_dir = dir.path().join("logs");
    std::fs::create_dir_all(&log_dir).expect("create log dir");
    let log_path = log_dir.join("khived.log");
    std::fs::write(&log_path, vec![7u8; 20]).expect("seed oversized log");

    let file = prepare_daemon_log_file_with_cap(&log_path, 10);

    assert!(file.is_some());
    let backup = log_dir.join("khived.log.1");
    assert!(backup.exists(), "oversized log must rotate to .log.1");
    assert_eq!(
        std::fs::metadata(&backup).expect("backup metadata").len(),
        20,
        "backup must retain the original oversized content"
    );
    assert_eq!(
        std::fs::metadata(&log_path).expect("log metadata").len(),
        0,
        "post-rotation log must start fresh"
    );
}

#[test]
fn prepare_daemon_log_file_rotation_replaces_prior_backup() {
    let dir = tempfile::tempdir().expect("tempdir");
    let log_dir = dir.path().join("logs");
    std::fs::create_dir_all(&log_dir).expect("create log dir");
    let log_path = log_dir.join("khived.log");
    let backup = log_dir.join("khived.log.1");
    std::fs::write(&backup, b"stale backup").expect("seed stale backup");
    std::fs::write(&log_path, vec![9u8; 20]).expect("seed oversized log");

    let file = prepare_daemon_log_file_with_cap(&log_path, 10);

    assert!(file.is_some());
    let backup_content = std::fs::read(&backup).expect("read backup");
    assert_eq!(
        backup_content,
        vec![9u8; 20],
        "rotation must replace the prior .log.1, not merge with it"
    );
}

#[test]
fn prepare_daemon_log_file_returns_none_when_dir_creation_fails() {
    let dir = tempfile::tempdir().expect("tempdir");
    // Put a regular FILE where the log directory needs to be, so
    // create_dir_all cannot create a directory at that path.
    let blocker = dir.path().join("logs");
    std::fs::write(&blocker, b"not a directory").expect("seed blocker file");
    let log_path = blocker.join("khived.log");

    assert!(prepare_daemon_log_file_with_cap(&log_path, DAEMON_LOG_MAX_BYTES).is_none());
}

// ── #645: remove_daemon_paths_if_still_stale ownership recheck ───────────
//
// These mirror the existing `shutdown_cleanup_skips_when_*` tests in
// `khive-runtime/src/daemon.rs` (owner-checked cleanup on the daemon's own
// shutdown path) but exercise the CLIENT's stale-daemon cleanup instead:
// between observing a PID as stale and reaching the unlink, a concurrent
// starter that could not rely on the recovery lock alone (e.g. it failed
// to acquire it) may have already claimed the rendezvous. Deterministic
// state fabrication (not a sleep-based race) proves each skip condition.

#[test]
#[serial]
fn remove_daemon_paths_if_still_stale_removes_when_pid_unchanged() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }

    let _cleanup = RecoveryTestGuard::new();
    clear_daemon_env();
    let dir = tempfile::tempdir().expect("tempdir");
    let pid_file = dir.path().join("khived.pid");
    let sock = dir.path().join("khived.sock");
    std::env::set_var("KHIVE_SOCKET", &sock);
    std::fs::write(&pid_file, "4242").expect("write pid file");
    drop(std::os::unix::net::UnixListener::bind(&sock).expect("bind stale socket"));
    assert_eq!(
        std::os::unix::net::UnixStream::connect(&sock)
            .expect_err("closed listener must refuse connections")
            .kind(),
        std::io::ErrorKind::ConnectionRefused,
    );

    assert!(remove_daemon_paths_if_still_stale(
        &pid_file,
        &PidFileSnapshot::read(&pid_file),
    ));

    assert!(!pid_file.exists(), "unchanged pid file must be removed");
    assert!(!sock.exists(), "stale socket must be removed");
    clear_daemon_env();
}

#[test]
#[serial]
fn remove_daemon_paths_if_still_stale_skips_when_socket_probe_is_uncertain() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }

    let _cleanup = RecoveryTestGuard::new();
    clear_daemon_env();
    let dir = tempfile::tempdir().expect("tempdir");
    let pid_file = dir.path().join("khived.pid");
    let blocker = dir.path().join("not-a-directory");
    let sock = blocker.join("khived.sock");
    std::env::set_var("KHIVE_SOCKET", &sock);
    std::fs::write(&pid_file, "4242").expect("write pid file");
    std::fs::write(&blocker, "not a socket directory").expect("write path blocker");
    assert_eq!(
        std::os::unix::net::UnixStream::connect(&sock)
            .expect_err("a non-directory parent makes the probe uncertain")
            .kind(),
        std::io::ErrorKind::NotADirectory,
    );

    assert!(!remove_daemon_paths_if_still_stale(
        &pid_file,
        &PidFileSnapshot::read(&pid_file),
    ));

    assert_eq!(std::fs::read(&pid_file).unwrap(), b"4242");
    assert_eq!(std::fs::read(&blocker).unwrap(), b"not a socket directory");
}

#[test]
#[serial]
fn remove_daemon_paths_if_still_stale_skips_when_pid_file_changed() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }

    clear_daemon_env();
    let dir = tempfile::tempdir().expect("tempdir");
    let pid_file = dir.path().join("khived.pid");
    let sock = dir.path().join("khived.sock");
    std::env::set_var("KHIVE_SOCKET", &sock);
    // A replacement daemon already wrote its own (different) pid here.
    std::fs::write(&pid_file, "5555").expect("write replacement pid file");
    std::fs::write(&sock, "replacement socket placeholder").expect("write sock placeholder");

    assert!(!remove_daemon_paths_if_still_stale(
        &pid_file,
        &PidFileSnapshot::Present(b"4242".to_vec()),
    ));

    assert!(
        pid_file.exists(),
        "replacement daemon's pid file must survive when it no longer \
             matches the expected (pre-SIGTERM) pid"
    );
    assert!(
        sock.exists(),
        "replacement daemon's socket must survive alongside its pid file"
    );
    clear_daemon_env();
}

#[test]
#[serial]
fn remove_daemon_paths_if_still_stale_skips_when_socket_has_a_live_listener() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }

    clear_daemon_env();
    let dir = tempfile::tempdir().expect("tempdir");
    let pid_file = dir.path().join("khived.pid");
    let sock = dir.path().join("khived.sock");
    std::env::set_var("KHIVE_SOCKET", &sock);
    std::fs::write(&pid_file, "4242").expect("write pid file matching expected_pid");
    // A replacement daemon already bound the socket path — even though the
    // pid file on disk has not been overwritten yet (e.g. its bind landed
    // just before its own pid-write).
    let _listener = std::os::unix::net::UnixListener::bind(&sock).expect("bind live socket");

    assert!(!remove_daemon_paths_if_still_stale(
        &pid_file,
        &PidFileSnapshot::read(&pid_file),
    ));

    assert!(
        sock.exists(),
        "a socket with a live listener must never be unlinked"
    );
    assert!(
        pid_file.exists(),
        "pid file must be left alone alongside the live socket"
    );
    clear_daemon_env();
}

#[test]
#[serial]
fn remove_daemon_paths_if_still_stale_compares_malformed_pid_bytes() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }

    let _cleanup = RecoveryTestGuard::new();
    clear_daemon_env();
    let dir = tempfile::tempdir().expect("tempdir");
    let pid_file = dir.path().join("khived.pid");
    let sock = dir.path().join("khived.sock");
    std::env::set_var("KHIVE_SOCKET", &sock);

    for contents in [
        b"".as_slice(),
        b"partial-pid".as_slice(),
        b"\xff".as_slice(),
    ] {
        for changed in [false, true] {
            std::fs::write(&pid_file, contents).expect("write incomplete pid file");
            let expected_snapshot = PidFileSnapshot::read(&pid_file);
            assert_eq!(expected_snapshot.pid(), None);
            drop(std::os::unix::net::UnixListener::bind(&sock).expect("bind stale socket"));
            let mut current_contents = contents.to_vec();
            if changed {
                current_contents.push(b'\n');
                std::fs::write(&pid_file, &current_contents).expect("change pid file bytes");
            }

            assert_eq!(
                remove_daemon_paths_if_still_stale(&pid_file, &expected_snapshot),
                !changed,
            );
            if changed {
                assert_eq!(std::fs::read(&pid_file).unwrap(), current_contents);
                assert!(sock.exists(), "changed owner's socket must survive");
                std::fs::remove_file(&pid_file).unwrap();
                std::fs::remove_file(&sock).unwrap();
            } else {
                assert!(
                    !pid_file.exists(),
                    "unchanged incomplete pid must be removed"
                );
                assert!(!sock.exists(), "unchanged stale socket must be removed");
            }
        }
    }
}

// ── bridge self-heal on ProtocolMismatch (#714) ───────────────────────────

#[test]
fn resumed_generation_from_args_absent_is_none() {
    let argv = vec![
        "kkernel".to_string(),
        "mcp".to_string(),
        "--daemon".to_string(),
    ];
    assert_eq!(resumed_generation_from_args(argv.into_iter()), None);
}

#[test]
fn resumed_generation_from_args_present_parses_value() {
    let argv = vec![
        "kkernel".to_string(),
        "mcp".to_string(),
        "--resumed-generation=1".to_string(),
    ];
    assert_eq!(resumed_generation_from_args(argv.into_iter()), Some(1));
}

#[test]
fn resumed_generation_from_args_malformed_value_is_none() {
    let argv = vec![
        "kkernel".to_string(),
        "--resumed-generation=notanumber".to_string(),
    ];
    assert_eq!(resumed_generation_from_args(argv.into_iter()), None);
}

#[test]
fn resumed_generation_from_args_takes_the_last_occurrence() {
    // Defensive: `reexec_in_place` filters any pre-existing marker before
    // appending its own, so duplicates should never occur in practice —
    // but if one ever did, the last one (the freshest exec's own marker)
    // must win, not the first.
    let argv = vec![
        "kkernel".to_string(),
        "--resumed-generation=1".to_string(),
        "--resumed-generation=2".to_string(),
    ];
    assert_eq!(resumed_generation_from_args(argv.into_iter()), Some(2));
}

// Cold-start non-regression (issue #714, self-heal test plan item 4): a
// real `cargo test` process's own argv never carries the marker, so the
// production entry point must resolve to `None` — the same guarantee
// `KhiveMcpServer::serve_stdio` relies on to keep using the normal
// `.serve()` handshake for every ordinary session.
#[test]
fn resumed_generation_is_none_for_a_normal_test_process() {
    assert_eq!(resumed_generation(), None);
}

// Loop-breaker guard rail (issue #714, self-heal test plan item 2): a
// resumed generation that observes ProtocolMismatch again must take the
// fallback, never a second exec. Pure decision function — no live
// process needed.
#[test]
fn decide_mismatch_recovery_first_generation_schedules_reexec() {
    assert_eq!(
        decide_mismatch_recovery(None),
        MismatchRecovery::ReexecScheduled
    );
}

#[test]
fn decide_mismatch_recovery_resumed_generation_drains_and_exits() {
    assert_eq!(
        decide_mismatch_recovery(Some(1)),
        MismatchRecovery::DrainAndExit
    );
}

// Ordering regression (issue #714, self-heal test plan item 1): arming a
// self-heal action must never fire it — only `fire_pending_self_heal`
// (the stdio transport's post-flush hook, exercised for real in
// `crates/kkernel/tests/mcp_bridge_reexec_protocol_mismatch.rs`) may take
// the armed action, and only after a flush has actually completed. This
// is the exact bug the original toy-server evidence reproduced (an
// in-flight response lost because `execv()` ran before the response
// bytes were flushed) — these tests pin the "arm never fires" half of
// that contract; the "fire only after a real flush" half is what the
// live integration test proves.

#[test]
#[serial]
fn schedule_reexec_on_mismatch_arms_without_firing() {
    reset_self_heal_counters();
    schedule_reexec_on_mismatch();
    assert_eq!(
        REEXEC_INVOKED_COUNT.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "arming must never exec synchronously or eagerly"
    );
    fire_pending_self_heal();
    assert_eq!(
        REEXEC_INVOKED_COUNT.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the armed action must fire exactly once it is taken"
    );
}

#[test]
#[serial]
fn schedule_drain_and_exit_arms_without_firing() {
    reset_self_heal_counters();
    schedule_drain_and_exit();
    assert_eq!(
        DRAIN_EXIT_INVOKED_COUNT.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "arming must never exit synchronously or eagerly"
    );
    fire_pending_self_heal();
    assert_eq!(
        DRAIN_EXIT_INVOKED_COUNT.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the armed action must fire exactly once it is taken"
    );
}

#[test]
#[serial]
fn fire_pending_self_heal_is_a_no_op_when_nothing_is_armed() {
    reset_self_heal_counters();
    fire_pending_self_heal();
    assert_eq!(
        REEXEC_INVOKED_COUNT.load(std::sync::atomic::Ordering::SeqCst),
        0
    );
    assert_eq!(
        DRAIN_EXIT_INVOKED_COUNT.load(std::sync::atomic::Ordering::SeqCst),
        0
    );
}

#[test]
#[serial]
fn fire_pending_self_heal_takes_the_armed_action_exactly_once() {
    reset_self_heal_counters();
    schedule_reexec_on_mismatch();
    fire_pending_self_heal();
    // A second flush completing after the action was already taken must
    // not re-fire it — `PENDING_SELF_HEAL` is `take()`n, not just read.
    fire_pending_self_heal();
    assert_eq!(
        REEXEC_INVOKED_COUNT.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "must fire exactly once even if fire_pending_self_heal is called again"
    );
}
