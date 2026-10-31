//! The search profile reached through a coordinated search, with its lines captured.

use std::sync::Arc;

use khive_mcp::coordinator::CoordinatorService;
use khive_pack_kg::handlers::ValidatedSearchRequest;
use khive_runtime::{
    BackendId, KhiveRuntime, Namespace, PackRegistry, RuntimeConfig, StorageBackend,
    VerbRegistryBuilder,
};

use super::{search_profile, BackendRegistry, SubstrateCoordinator, SubstrateCoordinatorService};

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn process_scope_config_locks_trace_once_without_store_rows() {
    if crate::test_process::run_in_child() {
        return;
    }

    use khive_runtime::audit_batch::AuditBatchControl;
    use std::sync::Mutex;
    use tracing::field::{Field, Visit};

    #[derive(Clone, Debug, PartialEq)]
    enum ConfigValue {
        Bool(bool),
        U64(u64),
        Other(String),
    }

    #[derive(Default)]
    struct ConfigVisitor {
        name: Option<String>,
        value: Option<ConfigValue>,
    }

    impl Visit for ConfigVisitor {
        fn record_bool(&mut self, field: &Field, value: bool) {
            if field.name() == "value" {
                self.value = Some(ConfigValue::Bool(value));
            }
        }

        fn record_u64(&mut self, field: &Field, value: u64) {
            if field.name() == "value" {
                self.value = Some(ConfigValue::U64(value));
            }
        }

        fn record_str(&mut self, field: &Field, value: &str) {
            if field.name() == "name" {
                self.name = Some(value.to_string());
            } else if field.name() == "value" {
                self.value = Some(ConfigValue::Other(value.to_string()));
            }
        }

        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            if field.name() == "value" {
                self.value = Some(ConfigValue::Other(format!("{value:?}")));
            }
        }
    }

    struct ConfigCapture(Arc<Mutex<Vec<(String, ConfigValue)>>>);

    impl tracing::Subscriber for ConfigCapture {
        fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
            metadata.target() == "khive::config_lock"
        }

        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }

        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}

        fn event(&self, event: &tracing::Event<'_>) {
            if event.metadata().target() != "khive::config_lock" {
                return;
            }
            assert_eq!(*event.metadata().level(), tracing::Level::INFO);
            let mut visitor = ConfigVisitor::default();
            event.record(&mut visitor);
            self.0.lock().unwrap().push((
                visitor.name.expect("config-lock name field"),
                visitor.value.expect("config-lock typed value field"),
            ));
        }

        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
    }

    async fn stored_events(runtime: &KhiveRuntime) -> Vec<khive_storage::Event> {
        let token = runtime.authorize(Namespace::local()).expect("local token");
        let page = runtime
            .events(&token)
            .expect("runtime event store")
            .query_events(
                khive_storage::EventFilter::default(),
                khive_storage::PageRequest {
                    offset: 0,
                    limit: 100,
                },
            )
            .await
            .expect("fixture events");
        assert!(
            page.items.len() < 100,
            "fixture event population exceeds its bound"
        );
        page.items
    }

    std::env::set_var("KHIVE_SEARCH_PROFILE", "1");
    std::env::set_var("KHIVE_COORDINATOR_SEARCH_TIMEOUT_MS", "5000");
    std::env::set_var("KHIVE_REQUEST_READ_TIMEOUT_SECS", "17");
    let captured = Arc::new(Mutex::new(Vec::new()));
    tracing::subscriber::set_global_default(ConfigCapture(Arc::clone(&captured)))
        .expect("isolated process subscriber");
    let bridge_seconds = khive_storage::request_read_timeout_from_env().as_secs();

    let main = memory_runtime();
    let secondary = memory_runtime();
    let mut builder = VerbRegistryBuilder::new();
    builder.with_gate(main.config().gate.clone());
    builder.with_default_namespace(Namespace::LOCAL);
    builder.with_actor_id(main.config().actor_id.clone());
    builder
        .with_runtime_event_store(main.as_ref())
        .expect("runtime audit binding");
    PackRegistry::register_packs(&["kg".into()], (*main).clone(), &mut builder)
        .expect("KG pack registration");
    let registry = builder.build().expect("bound registry");
    let batch = registry
        .audit_batch_handle()
        .expect("actual owning audit batch");

    let mut backends = BackendRegistry::new();
    backends.register(backend_id("main"), Arc::clone(&main));
    backends.register(backend_id("lore"), Arc::clone(&secondary));
    let service = SubstrateCoordinatorService::new(SubstrateCoordinator::new(backends));
    let query = ValidatedSearchRequest::from_value(
        serde_json::json!({"kind": "entity", "query": "ConfigLockProbe", "limit": 10}),
        &registry,
    )
    .expect("valid coordinated query");

    // Do not use search_profile::capture: its forced-profile flag bypasses enabled().
    for _ in 0..2 {
        let _ = service
            .fan_out_search(&query, &Namespace::local(), &[])
            .await;
    }

    // The registry retains the runtime. Keeping server.runtime unset deliberately
    // permits the admitted forwarding branch for this in-memory test registry.
    let server = khive_mcp::server::KhiveMcpServer::from_registry_with_meta(
        registry.clone(),
        Namespace::LOCAL,
        "config-lock-process-fixture",
    );
    for _ in 0..2 {
        khive_mcp::daemon::test_forward_seam::arm();
        let response = server
            .request_for_config_lock_test(
                serde_json::from_value(serde_json::json!({"ops": "stats()"}))
                    .expect("request parameters"),
            )
            .await
            .expect("real bridge request with canned daemon forwarding");
        let response: serde_json::Value = serde_json::from_str(&response).expect("forwarded JSON");
        assert_eq!(response["summary"]["succeeded"], 1);
        assert_eq!(
            khive_mcp::daemon::test_forward_seam::take_captured(),
            Some(Some(vec!["kg".to_string()])),
            "the real forwarding adapter must be reached without local dispatch",
        );
    }
    for runtime in [main.as_ref(), secondary.as_ref()] {
        assert!(
            stored_events(runtime).await.is_empty(),
            "coordinated search and forwarded requests must not dispatch an audit drain"
        );
    }

    registry
        .dispatch("stats", serde_json::json!({}))
        .await
        .expect("ordinary bound dispatch");
    tokio::time::timeout(std::time::Duration::from_secs(60), batch.quiesce())
        .await
        .expect("audit completion watchdog")
        .expect("audit completion");
    let process_names = [
        "KHIVE_SEARCH_PROFILE",
        "KHIVE_COORDINATOR_SEARCH_TIMEOUT_MS",
        "KHIVE_REQUEST_READ_TIMEOUT_SECS",
    ];
    for runtime in [main.as_ref(), secondary.as_ref()] {
        let rows = stored_events(runtime).await;
        assert!(
            !rows
                .iter()
                .any(|event| event.kind == khive_types::EventKind::ConfigLocked
                    && event.payload["key"]
                        .as_str()
                        .is_some_and(|name| process_names.contains(&name))),
            "process-scoped config reached an event store: {rows:?}"
        );
    }
    assert!(
        stored_events(main.as_ref())
            .await
            .iter()
            .any(|event| event.verb == "stats"),
        "the same bound registry must have a completed persisted dispatch witness"
    );

    let mut records = captured.lock().unwrap().clone();
    records.sort_by(|a, b| a.0.cmp(&b.0));
    let mut expected = vec![
        ("KHIVE_SEARCH_PROFILE".to_string(), ConfigValue::Bool(true)),
        (
            "KHIVE_COORDINATOR_SEARCH_TIMEOUT_MS".to_string(),
            ConfigValue::U64(5000),
        ),
        (
            "KHIVE_REQUEST_READ_TIMEOUT_SECS".to_string(),
            ConfigValue::U64(bridge_seconds),
        ),
    ];
    expected.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        records, expected,
        "one typed trace per process name across repeated real flows"
    );
    tokio::time::timeout(
        std::time::Duration::from_secs(60),
        registry.shutdown_audit_batch(),
    )
    .await
    .expect("shutdown watchdog")
    .expect("audit shutdown");
}

fn memory_runtime() -> Arc<KhiveRuntime> {
    let backend = Arc::new(StorageBackend::memory().expect("in-memory backend"));
    backend.prepare_core_schema().expect("core schema");
    Arc::new(KhiveRuntime::from_backend(
        backend,
        RuntimeConfig {
            db_path: None,
            events_split: None,
            actor_id: Some("test:coordinator-search-profile".into()),
            packs: vec!["kg".into()],
            ..RuntimeConfig::no_embeddings()
        },
    ))
}

#[path = "../../../khive-runtime/tests/support/receipt_credentials.rs"]
mod config_lock_runtime_receipt_credentials;

#[tokio::test(flavor = "current_thread")]
#[serial_test::serial(config_ledger)]
async fn actual_runtime_config_producers_are_observed_once_per_runtime() {
    if crate::test_process::run_in_child() {
        return;
    }

    use khive_pack_memory::config::RecallConfig;
    use khive_runtime::audit_batch::AuditBatchControl;

    const EXPECTED: [(&str, &str); 5] = [
        ("KHIVE_RECALL_PROFILE", "false"),
        ("ANN_OVERFETCH_MAX_ROUNDS", "3"),
        ("KHIVE_MEMORY_ANN_READY_TIMEOUT_MS", "8000"),
        ("KHIVE_MEMORY_RECALL_DEADLINE_MS", "30000"),
        ("KHIVE_CONTEXT_PROFILE", "false"),
    ];

    fn real_runtime() -> KhiveRuntime {
        let backend = Arc::new(StorageBackend::memory().expect("runtime backend"));
        backend.prepare_core_schema().expect("core schema");
        let runtime = KhiveRuntime::from_backend(
            backend,
            RuntimeConfig {
                db_path: None,
                events_split: None,
                actor_id: Some("test:runtime-config-producers".into()),
                brain_profile: None,
                packs: vec!["kg".into(), "memory".into()],
                ..RuntimeConfig::no_embeddings()
            },
        );
        config_lock_runtime_receipt_credentials::with_receipt_credentials(runtime)
    }

    async fn config_rows(runtime: &KhiveRuntime) -> Vec<khive_storage::Event> {
        let token = runtime.authorize(Namespace::local()).expect("local token");
        let page = runtime
            .events(&token)
            .expect("runtime events")
            .query_events(
                khive_storage::EventFilter {
                    kinds: vec![khive_types::EventKind::ConfigLocked],
                    ..Default::default()
                },
                khive_storage::PageRequest {
                    offset: 0,
                    limit: 100,
                },
            )
            .await
            .expect("config rows");
        assert!(
            page.items.len() < 100,
            "config fixture population exceeds its bound"
        );
        page.items
            .into_iter()
            .filter(|event| {
                event.payload["key"]
                    .as_str()
                    .is_some_and(|key| EXPECTED.iter().any(|(expected, _)| key == *expected))
            })
            .collect()
    }

    fn assert_rows(label: &str, rows: &[khive_storage::Event]) {
        for (key, value) in EXPECTED {
            let matching: Vec<_> = rows
                .iter()
                .filter(|event| event.payload["key"] == key)
                .collect();
            assert_eq!(
                matching.len(),
                1,
                "actual runtime config row count: runtime={label}, key={key}, rows={rows:?}"
            );
            assert_eq!(
                matching[0].payload["value"], value,
                "actual runtime cached value: runtime={label}, key={key}"
            );
            assert_eq!(matching[0].verb, "config.lock");
            assert_eq!(matching[0].namespace, Namespace::LOCAL);
        }
        assert_eq!(rows.len(), EXPECTED.len());
    }

    async fn drive_actual_pack_producers(runtime: &KhiveRuntime, label: &str) {
        let token = runtime.authorize(Namespace::local()).expect("seed token");
        runtime
            .create_note(
                &token,
                "memory",
                None,
                "latency probe note",
                Some(0.7),
                None,
                vec![],
            )
            .await
            .expect("seed actual recall corpus");

        let mut builder = VerbRegistryBuilder::new();
        builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
        builder.register(khive_pack_memory::MemoryPack::new(runtime.clone()));
        builder.with_default_namespace(Namespace::LOCAL);
        builder.with_actor_id(runtime.config().actor_id.clone());
        builder
            .with_runtime_event_store(runtime)
            .expect("runtime audit binding");
        let registry = builder.build().expect("actual KG/memory registry");
        let batch = registry
            .audit_batch_handle()
            .expect("actual owning audit batch");

        // Explicit request config replaces the pack default in effective_config.
        // This proves the real ANN accessors execute, independently of defaults.
        let config = RecallConfig {
            ann_overfetch_max_rounds: None,
            ann_ready_timeout_ms: None,
            ..RecallConfig::default()
        };
        assert!(config.ann_overfetch_max_rounds.is_none());
        assert!(config.ann_ready_timeout_ms.is_none());
        config.validate().expect("valid explicit recall config");
        let config = serde_json::to_value(config).expect("serialize recall config");
        assert_eq!(
            config.get("ann_overfetch_max_rounds"),
            Some(&serde_json::Value::Null)
        );
        assert_eq!(
            config.get("ann_ready_timeout_ms"),
            Some(&serde_json::Value::Null)
        );
        assert!(
            config.get("recall_deadline_ms").is_none(),
            "deadline must use the operator accessor"
        );

        for _ in 0..2 {
            registry
                .dispatch(
                    "memory.recall",
                    serde_json::json!({
                        "namespace": Namespace::LOCAL, "query": "latency probe note", "limit": 10,
                        "config": config.clone(),
                    }),
                )
                .await
                .expect("real recall with all four production accessor fallbacks");
            registry
                .dispatch(
                    "context",
                    serde_json::json!({
                        "namespace": Namespace::LOCAL, "query": "latency probe note",
                    }),
                )
                .await
                .expect("real authorized context with a valid nonblank query");
            registry
                .dispatch("stats", serde_json::json!({}))
                .await
                .expect("drain observations made by context");
            tokio::time::timeout(std::time::Duration::from_secs(60), batch.quiesce())
                .await
                .expect("actual audit completion watchdog")
                .expect("actual audit completion");
            assert_rows(label, &config_rows(runtime).await);
        }
        tokio::time::timeout(
            std::time::Duration::from_secs(60),
            registry.shutdown_audit_batch(),
        )
        .await
        .expect("actual audit shutdown watchdog")
        .expect("actual audit shutdown");
    }

    // The child is cold, and A finishes before B is even constructed.
    std::env::remove_var("KHIVE_RECALL_PROFILE");
    std::env::set_var("ANN_OVERFETCH_MAX_ROUNDS", "3");
    std::env::set_var("KHIVE_MEMORY_ANN_READY_TIMEOUT_MS", "8000");
    std::env::set_var("KHIVE_MEMORY_RECALL_DEADLINE_MS", "30000");
    std::env::remove_var("KHIVE_CONTEXT_PROFILE");
    let a = real_runtime();
    drive_actual_pack_producers(&a, "A").await;
    let a_before_b = config_rows(&a).await;
    let b = real_runtime();
    drive_actual_pack_producers(&b, "B").await;
    assert_rows("A after B", &config_rows(&a).await);
    assert_rows("B", &config_rows(&b).await);
    assert_eq!(
        config_rows(&a).await,
        a_before_b,
        "B must not change A's stored production config rows"
    );
}

fn request(params: serde_json::Value) -> ValidatedSearchRequest {
    let runtime = memory_runtime();
    let mut builder = VerbRegistryBuilder::new();
    builder.with_gate(runtime.config().gate.clone());
    builder.with_default_namespace(Namespace::LOCAL);
    builder.with_actor_id(runtime.config().actor_id.clone());
    PackRegistry::register_packs(&["kg".into()], (*runtime).clone(), &mut builder)
        .expect("KG pack registration");
    ValidatedSearchRequest::from_value(params, &builder.build().expect("registry"))
        .expect("valid search request")
}

fn backend_id(id: &str) -> BackendId {
    BackendId::parse(id).expect("backend id")
}

async fn seeded_runtime() -> Arc<KhiveRuntime> {
    let runtime = memory_runtime();
    let token = runtime.authorize(Namespace::local()).expect("authorize");
    runtime
        .create_entity_with_embedding_report(
            &token,
            "concept",
            None,
            "ProfileProbe",
            Some("probe entity for the search profile"),
            None,
            vec![],
        )
        .await
        .expect("seed entity");
    runtime
}

async fn profiled_search(
    registry: BackendRegistry,
    failing: Option<&str>,
) -> Vec<serde_json::Value> {
    let mut coordinator = SubstrateCoordinator::new(registry);
    if let Some(backend) = failing {
        coordinator = coordinator.with_failing_backend(backend);
    }
    let service = SubstrateCoordinatorService::new(coordinator);
    let request =
        request(serde_json::json!({"kind": "entity", "query": "ProfileProbe", "limit": 10}));
    let (_result, lines) =
        search_profile::capture(service.fan_out_search(&request, &Namespace::local(), &[])).await;
    lines
        .iter()
        .map(|line| serde_json::from_str(line).expect("each line is one JSON object"))
        .collect()
}

fn stages(lines: &[serde_json::Value]) -> Vec<&str> {
    lines
        .iter()
        .map(|line| line["s"].as_str().expect("stage name"))
        .collect()
}

// The child isolates the process-wide profiling switch and captures real stderr
// emitted by runtime spans. Both coordinator backends use `memory_runtime()`.
#[tokio::test(flavor = "current_thread")]
async fn runtime_and_coordinator_emit_correlated_search_lines() {
    const CHILD: &str = "KHIVE_6213_CORRELATION_CHILD";
    const COORDINATOR_PREFIX: &str = "KHIVE_6213_COORDINATOR:";

    if std::env::var_os(CHILD).is_some() {
        let mut registry = BackendRegistry::new();
        registry.register(backend_id("main"), memory_runtime());
        registry.register(backend_id("lore"), seeded_runtime().await);
        let service = SubstrateCoordinatorService::new(SubstrateCoordinator::new(registry));
        let query = request(serde_json::json!({
            "kind": "entity", "query": "ProfileProbe", "limit": 10
        }));
        let (result, lines) =
            search_profile::capture(service.fan_out_search(&query, &Namespace::local(), &[])).await;
        result.expect("in-memory coordinated search must succeed");
        for line in lines {
            eprintln!("{COORDINATOR_PREFIX}{line}");
        }
        return;
    }

    let mut child = std::process::Command::new(std::env::current_exe().expect("test executable"))
        .arg("runtime_and_coordinator_emit_correlated_search_lines")
        .arg("--nocapture")
        .env(CHILD, "1")
        .env("KHIVE_SEARCH_PROFILE", "1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("isolated profiling child");
    // Drain both pipes while the child runs so a full stderr pipe cannot block
    // completion. Both reader threads are joined after the child is reaped.
    let mut stdout_pipe = child.stdout.take().expect("child stdout");
    let mut stderr_pipe = child.stderr.take().expect("child stderr");
    let stdout_reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut stdout_pipe, &mut bytes).expect("read child stdout");
        bytes
    });
    let stderr_reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut stderr_pipe, &mut bytes).expect("read child stderr");
        bytes
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    let mut timed_out = false;
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll profiling child") {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            timed_out = true;
            let _ = child.kill();
            break child.wait().expect("reap timed-out child");
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    };
    let stdout_bytes = stdout_reader.join().expect("stdout reader joined");
    let stderr_bytes = stderr_reader.join().expect("stderr reader joined");
    let stdout = String::from_utf8_lossy(&stdout_bytes);
    let stderr = String::from_utf8_lossy(&stderr_bytes);
    assert!(
        !timed_out,
        "profiling child timed out: stdout={stdout} stderr={stderr}"
    );
    assert!(
        status.success(),
        "child failed: stdout={stdout} stderr={stderr}"
    );
    assert!(
        stdout.lines().any(|line| line.trim() == "running 1 test"),
        "expected exactly one child test: {stdout}"
    );

    let coordinator: Vec<serde_json::Value> = stderr
        .lines()
        .filter_map(|line| line.strip_prefix(COORDINATOR_PREFIX))
        .map(|line| serde_json::from_str(line).expect("coordinator JSON"))
        .collect();
    let call = coordinator
        .iter()
        .find(|line| line["s"] == "search")
        .and_then(|line| line["c"].as_u64())
        .filter(|id| *id > 0)
        .expect("coordinator search line with call id");
    assert!(
        coordinator.iter().any(|line| {
            line["s"] == "backend"
                && line["b"] == "lore"
                && line["o"] == "ok"
                && line["n"].as_u64().is_some_and(|n| n > 0)
        }),
        "seeded backend must return a hit: {coordinator:?}"
    );

    let runtime: Vec<serde_json::Value> = stderr
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|line| line["layer"] == "runtime")
        .collect();
    assert!(
        !runtime.is_empty(),
        "runtime must emit real stage lines: {stderr}"
    );
    assert!(
        runtime
            .iter()
            .any(|line| line["s"] == "text" && line["outcome"] == "ok"),
        "successful runtime text-stage witness missing: {runtime:?}"
    );
    assert!(
        runtime.iter().all(|line| line["c"].as_u64() == Some(call)),
        "runtime and coordinator call IDs differ: {runtime:?} vs {call}"
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn a_two_backend_search_writes_a_line_per_backend_and_per_stage_under_one_call() {
    let mut registry = BackendRegistry::new();
    registry.register(backend_id("main"), memory_runtime());
    registry.register(backend_id("lore"), seeded_runtime().await);
    let lines = profiled_search(registry, Some("main")).await;

    assert_eq!(
        stages(&lines),
        ["backend", "backend", "merge", "fanout", "finalize", "hydrate", "search"],
        "{lines:?}"
    );
    let call = lines[0]["c"].as_u64().expect("call id");
    assert!(call > 0);
    assert!(lines.iter().all(|line| line["c"] == call), "{lines:?}");
    assert!(lines.iter().all(|line| line["us"].is_u64()), "{lines:?}");

    let backend = |name: &str| {
        lines
            .iter()
            .find(|line| line["s"] == "backend" && line["b"] == name)
            .unwrap_or_else(|| panic!("no backend line for {name}: {lines:?}"))
    };
    assert_eq!(backend("main")["o"], "error");
    assert_eq!(backend("main")["n"], 0);
    assert_eq!(backend("lore")["o"], "ok");
    assert!(
        backend("lore")["n"].as_u64().is_some_and(|n| n >= 1),
        "{lines:?}"
    );
    assert_eq!(lines[3]["backends"], 2);
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn a_scoped_note_search_is_one_profiled_call() {
    // the path the search verb takes: `fan_out_search_scoped` with the caller's token
    let lore = memory_runtime();
    let token = lore.authorize(Namespace::local()).expect("authorize");
    lore.create_note(
        &token,
        "observation",
        Some("ProfileNote"),
        "probe note for the search profile",
        None,
        None,
        vec![],
    )
    .await
    .expect("seed note");
    let mut registry = BackendRegistry::new();
    registry.register(backend_id("lore"), Arc::clone(&lore));
    registry.register(backend_id("archive"), memory_runtime());
    let service = SubstrateCoordinatorService::new(SubstrateCoordinator::new(registry));
    let request = request(serde_json::json!({"kind": "note", "query": "ProfileNote", "limit": 10}));
    let (result, lines) = search_profile::capture(service.fan_out_search_scoped(
        &request,
        &token,
        &serde_json::json!({}),
        &[],
    ))
    .await;
    result.expect("scoped note search");
    let lines: Vec<serde_json::Value> = lines
        .iter()
        .map(|line| serde_json::from_str(line).expect("each line is one JSON object"))
        .collect();

    assert_eq!(
        stages(&lines),
        ["backend", "backend", "merge", "fanout", "finalize", "hydrate", "search"],
        "{lines:?}"
    );
    let call = lines[0]["c"].as_u64().expect("call id");
    assert!(
        call > 0 && lines.iter().all(|line| line["c"] == call),
        "{lines:?}"
    );
    let lore_line = lines
        .iter()
        .find(|line| line["b"] == "lore")
        .expect("lore backend line");
    assert_eq!(lore_line["o"], "ok");
    assert!(lore_line["n"].as_u64().is_some_and(|n| n >= 1), "{lines:?}");
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn a_single_backend_search_names_its_backend_with_the_fanout_time() {
    let mut registry = BackendRegistry::new();
    registry.register(backend_id("lore"), seeded_runtime().await);
    let lines = profiled_search(registry, None).await;

    assert_eq!(
        stages(&lines),
        ["backend", "fanout", "finalize", "hydrate", "search"],
        "{lines:?}"
    );
    assert_eq!(lines[0]["b"], "lore");
    assert_eq!(lines[0]["o"], "ok");
    assert!(lines[0]["n"].as_u64().is_some_and(|n| n >= 1), "{lines:?}");
    assert_eq!(lines[0]["us"], lines[1]["us"], "{lines:?}");
    assert_eq!(lines[1]["backends"], 1);
    assert_eq!(lines[1]["n"], lines[0]["n"], "{lines:?}");
}

// Paused clock: the hung backend's deadline fires on the first idle advance.
#[tokio::test(start_paused = true)]
#[serial_test::serial(config_ledger)]
async fn a_single_hung_backend_is_named_with_its_timeout() {
    let mut registry = BackendRegistry::new();
    registry.register(backend_id("hung"), memory_runtime());
    let service = SubstrateCoordinatorService::new(
        SubstrateCoordinator::new(registry).with_hanging_backend("hung"),
    );
    let request =
        request(serde_json::json!({"kind": "entity", "query": "ProfileProbe", "limit": 10}));
    let (_result, lines) =
        search_profile::capture(service.fan_out_search(&request, &Namespace::local(), &[])).await;
    let lines: Vec<serde_json::Value> = lines
        .iter()
        .map(|line| serde_json::from_str(line).expect("each line is one JSON object"))
        .collect();

    assert_eq!(
        stages(&lines),
        [
            "backend_deadline",
            "backend",
            "fanout",
            "finalize",
            "hydrate",
            "search"
        ],
        "{lines:?}"
    );
    assert_eq!(lines[0]["b"], "hung");
    assert_eq!(lines[0]["observation"], "timed_out");
    assert_eq!(lines[0]["c"], lines[1]["c"], "{lines:?}");
    assert_eq!(lines[1]["b"], "hung");
    assert_eq!(lines[1]["o"], "timeout");
    assert_eq!(lines[1]["n"], 0);
}
