//! The search profile reached through a coordinated search, with its lines captured.

use std::sync::Arc;

use khive_mcp::coordinator::CoordinatorService;
use khive_pack_kg::handlers::ValidatedSearchRequest;
use khive_runtime::{
    BackendId, KhiveRuntime, Namespace, PackRegistry, RuntimeConfig, StorageBackend,
    VerbRegistryBuilder,
};

use super::{search_profile, BackendRegistry, SubstrateCoordinator, SubstrateCoordinatorService};

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
