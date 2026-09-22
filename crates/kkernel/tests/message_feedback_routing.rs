//! Search-returned KG handles remain usable across configured pack backends.

use std::sync::Arc;

use khive_mcp::server::KhiveMcpServer;
use khive_mcp::tools::request::RequestParams;
use khive_runtime::{
    BackendConfig, BackendId, BackendKind, KhiveConfig, KhiveRuntime, Namespace, PackConfig,
    RuntimeConfig, RuntimeError, VerbRegistry,
};
use khive_types::SubstrateKind;
use kkernel::coordinator::{BackendRegistry, SubstrateCoordinator, SubstrateCoordinatorService};
use serde_json::{json, Value};
use uuid::Uuid;

struct Fixture {
    _dir: tempfile::TempDir,
    server: KhiveMcpServer,
    registry: VerbRegistry,
    main: KhiveRuntime,
    comm: Arc<KhiveRuntime>,
}

async fn fixture() -> Fixture {
    fixture_with_gate(Arc::new(khive_runtime::AllowAllGate)).await
}

async fn fixture_with_gate(gate: khive_runtime::GateRef) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let main_path = dir.path().join("main.db");
    let config = KhiveConfig {
        backends: ["main", "comm"]
            .into_iter()
            .map(|name| BackendConfig {
                name: name.into(),
                kind: BackendKind::Sqlite,
                path: Some(dir.path().join(format!("{name}.db"))),
                cache_mb: None,
                journal_mode: None,
                served_kinds: None,
                read_only: false,
            })
            .collect(),
        packs: std::collections::HashMap::from([(
            "comm".into(),
            PackConfig {
                backend: "comm".into(),
                no_embed: true,
            },
        )]),
        ..KhiveConfig::default()
    };
    let main_db = main_path.to_str().unwrap().to_owned();
    let boot = khive_mcp::serve::build_registry_for_multi_backend(
        RuntimeConfig {
            db_path: Some(main_path),
            actor_id: Some("routing-reader".into()),
            gate,
            packs: vec!["kg".into(), "comm".into(), "brain".into()],
            ..RuntimeConfig::no_embeddings()
        },
        &config,
        Some(main_db.as_str()),
    )
    .await
    .unwrap();
    let registry = boot.registry.clone();
    let main = boot.default_runtime.clone();
    let comm = Arc::clone(&boot.per_pack_runtimes["comm"]);
    let mut backends = BackendRegistry::new();
    backends.register(BackendId::main(), Arc::new(main.clone()));
    backends.register(BackendId::parse("comm").unwrap(), Arc::clone(&comm));
    let coordinator = Arc::new(SubstrateCoordinatorService::new(SubstrateCoordinator::new(
        backends,
    )));
    let server = khive_mcp::serve::build_server_from_multi_backend_registry(
        boot,
        &config,
        Some(coordinator),
    );
    Fixture {
        _dir: dir,
        server,
        registry,
        main,
        comm,
    }
}

async fn call(server: &KhiveMcpServer, verb: &str, args: Value) -> Value {
    let raw = server
        .dispatch_request_local(RequestParams {
            ops: json!([{ "tool": verb, "args": args }]).to_string(),
            presentation: Some("verbose".into()),
            ..RequestParams::default()
        })
        .await
        .unwrap();
    let response: Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(response["results"][0]["ok"], true, "{response}");
    response["results"][0]["result"].clone()
}

/// Before the routing repair, search succeeds but the first get fails on the
/// main backend. Returning fewer search hits is not an acceptable repair.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn issue2992_search_message_full_and_prefix_ids_support_get_and_feedback() {
    let f = fixture().await;
    let sent = call(
        &f.server,
        "comm.send",
        json!({"to": "routing-recipient", "subject": "routeoracle", "content": "routeoracle cross backend message"}),
    )
    .await;
    let full = sent["full_id"].as_str().unwrap();
    let id = Uuid::parse_str(full).unwrap();
    let main_token = f.main.authorize(Namespace::local()).unwrap();
    let comm_token = f.comm.authorize(Namespace::local()).unwrap();
    assert!(f
        .main
        .notes(&main_token)
        .unwrap()
        .get_note(id)
        .await
        .unwrap()
        .is_none());
    let stored = f
        .comm
        .notes(&comm_token)
        .unwrap()
        .get_note(id)
        .await
        .unwrap()
        .unwrap();
    let results = call(
        &f.server,
        "search",
        json!({"kind": "note", "query": "routeoracle", "limit": 10}),
    )
    .await;
    let hit = results
        .as_array()
        .unwrap()
        .iter()
        .find(|hit| hit.get("full_id").unwrap_or(&hit["id"]) == full)
        .unwrap_or_else(|| panic!("search must retain the remote message: {results}"));
    assert_eq!(hit["note_kind"], "message");
    for target in [full, &full[..8]] {
        let fetched = call(&f.server, "get", json!({"id": target})).await;
        assert_eq!(fetched["id"], full);
        assert_eq!(fetched["content"], stored.content);
        // Omit full_id in the prefix arm so it exercises actual prefix routing.
        let selected = if target == full {
            hit.clone()
        } else {
            json!({"id": target})
        };
        let feedback = call(
            &f.server,
            "brain.auto_feedback",
            json!({"query": "routeoracle", "results": [selected], "signal": "useful", "target_id": target}),
        )
        .await;
        assert_eq!(feedback["emitted"], true);
        assert_eq!(feedback["target_id"], full);
        let event_id = Uuid::parse_str(feedback["event_id"].as_str().unwrap()).unwrap();
        let event = f
            .main
            .events(&main_token)
            .unwrap()
            .get_event(event_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event.target_id, Some(id));
        assert_eq!(event.substrate, SubstrateKind::Note);
        assert!(
            f.comm
                .events(&comm_token)
                .unwrap()
                .get_event(event_id)
                .await
                .unwrap()
                .is_none(),
            "feedback must remain owned by the brain backend"
        );
    }
    assert_eq!(
        f.comm
            .notes(&comm_token)
            .unwrap()
            .get_note(id)
            .await
            .unwrap()
            .unwrap(),
        stored,
        "feedback is not a message mutation"
    );
}

async fn seed(runtime: &KhiveRuntime, raw_id: &str, namespace: &str) -> khive_storage::Note {
    let token = runtime
        .authorize(Namespace::parse(namespace).unwrap())
        .unwrap();
    let mut note = khive_storage::Note::new(namespace, "observation", "shared handle");
    note.id = Uuid::parse_str(raw_id).unwrap();
    runtime
        .notes(&token)
        .unwrap()
        .upsert_note(note.clone())
        .await
        .unwrap();
    note
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn issue2992_registry_prefix_collision_is_global_and_feedback_has_no_effect() {
    let f = fixture().await;
    let first = seed(&f.main, "facade12-0000-4000-8000-000000000001", "local").await;
    let second = seed(&f.comm, "facade12-0000-4000-8000-000000000002", "elsewhere").await;
    for (verb, args) in [
        ("get", json!({"id": "facade12"})),
        (
            "brain.feedback",
            json!({"target_id": "facade12", "signal": "useful"}),
        ),
        (
            "brain.auto_feedback",
            json!({"query": "shared", "results": [{"id": "facade12"}], "target_id": "facade12", "signal": "useful"}),
        ),
    ] {
        let error = f.registry.dispatch(verb, args).await.unwrap_err();
        assert!(
            matches!(error, RuntimeError::AmbiguousPrefix { matches, .. } if matches == vec![first.id, second.id])
        );
    }
    assert_eq!(
        feedback_count(&f.main).await,
        0,
        "ambiguous targets must not emit feedback"
    );
    // Full-ID reads retain ADR-007 namespace independence, and direct registry
    // callers use the same resolver as the MCP request path.
    let got = f
        .registry
        .dispatch("get", json!({"id": second.id, "namespace": "another"}))
        .await
        .unwrap();
    assert_eq!(got["id"], second.id.to_string());
    let credited = f
        .registry
        .dispatch(
            "brain.feedback",
            json!({"target_id": second.id, "signal": "useful", "namespace": "another"}),
        )
        .await
        .unwrap();
    assert_eq!(credited["emitted"], true);
    let token = f
        .main
        .authorize(Namespace::parse("another").unwrap())
        .unwrap();
    let event = f
        .main
        .events(&token)
        .unwrap()
        .get_event(Uuid::parse_str(credited["event_id"].as_str().unwrap()).unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(event.namespace, "another");
    assert_eq!(event.actor, "actor:routing-reader");
    assert_eq!(event.substrate, SubstrateKind::Note);
    // This repair only routes reads. Generic mutation remains on the KG pack's
    // configured backend and cannot accidentally mutate the remote note.
    for (verb, args) in [
        (
            "update",
            json!({"id": second.id, "content": "must not move writes"}),
        ),
        ("delete", json!({"id": second.id})),
    ] {
        assert!(
            f.registry.dispatch(verb, args).await.is_err(),
            "{verb} must retain its home-backend contract"
        );
    }
    assert_eq!(
        f.comm
            .notes(&token)
            .unwrap()
            .get_note(second.id)
            .await
            .unwrap()
            .unwrap(),
        second
    );
    assert_eq!(
        f.main
            .notes(&token)
            .unwrap()
            .get_note(first.id)
            .await
            .unwrap()
            .unwrap(),
        first
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn issue2992_public_reads_preserve_backend_failure_and_reject_non_kg_targets() {
    let f = fixture().await;
    let main = seed(&f.main, "badfab12-0000-4000-8000-000000000001", "local").await;
    let token = f.main.authorize(Namespace::local()).unwrap();
    let event = khive_storage::Event::new(
        "local",
        "probe",
        khive_types::EventKind::PhaseStarted,
        SubstrateKind::Note,
        "actor:fixture",
    );
    f.comm
        .events(&token)
        .unwrap()
        .append_event(event.clone())
        .await
        .unwrap();
    let error = f
        .registry
        .dispatch(
            "brain.feedback",
            json!({"target_id": event.id, "signal": "useful"}),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(error, RuntimeError::NotFound(_)),
        "non-KG targets must remain ineligible: {error}"
    );
    let before = feedback_count(&f.main).await;
    // A schema fault injected through the writer is invisible to pooled
    // readers holding an earlier snapshot, so the backend read failure is
    // forced with an already-expired read deadline instead.
    for (verb, args) in [
        ("get", json!({"id": main.id})),
        ("get", json!({"id": "badfab12"})),
        (
            "brain.feedback",
            json!({"target_id": main.id, "signal": "useful"}),
        ),
        (
            "brain.auto_feedback",
            json!({"query": "shared", "results": [{"id": "badfab12"}], "target_id": "badfab12", "signal": "useful"}),
        ),
    ] {
        let error = khive_storage::scope_request_read_deadline(
            std::time::Duration::ZERO,
            f.registry.dispatch(verb, args),
        )
        .await
        .unwrap_err();
        if verb == "get" {
            assert!(
                matches!(error, RuntimeError::Storage(_) | RuntimeError::Sqlite(_)),
                "must preserve backend failure: {error}"
            );
        } else {
            // Feedback may fail on an earlier read of its own state; what it
            // must never do is report the unreadable target as missing.
            assert!(
                !matches!(
                    error,
                    RuntimeError::NotFound(_) | RuntimeError::InvalidInput(_)
                ),
                "backend failure must not read as an absent target: {error}"
            );
        }
    }
    assert_eq!(
        feedback_count(&f.main).await,
        before,
        "failed lookup must not emit feedback"
    );
    assert_eq!(
        f.main
            .notes(&token)
            .unwrap()
            .get_note(main.id)
            .await
            .unwrap()
            .unwrap(),
        main
    );
}

async fn feedback_count(runtime: &KhiveRuntime) -> u64 {
    let token = runtime.authorize(Namespace::local()).unwrap();
    runtime
        .events(&token)
        .unwrap()
        .count_events(khive_storage::EventFilter {
            kinds: vec![khive_types::EventKind::FeedbackExplicit],
            ..khive_storage::EventFilter::default()
        })
        .await
        .unwrap()
}

#[derive(Debug, Default)]
struct ReadGate {
    refuse: std::sync::atomic::AtomicBool,
}

impl khive_runtime::Gate for ReadGate {
    fn check(
        &self,
        request: &khive_runtime::GateRequest,
    ) -> Result<khive_runtime::GateDecision, khive_runtime::GateError> {
        if self.refuse.load(std::sync::atomic::Ordering::SeqCst)
            && matches!(
                request.verb.as_str(),
                "get" | "brain.feedback" | "brain.auto_feedback"
            )
        {
            Ok(khive_runtime::GateDecision::deny("read denied"))
        } else {
            Ok(khive_runtime::GateDecision::allow())
        }
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn issue2992_gate_refuses_before_cross_backend_lookup() {
    let gate = Arc::new(ReadGate::default());
    let f = fixture_with_gate(gate.clone()).await;
    let sql = f.comm.sql();
    let mut writer = sql.writer().await.unwrap();
    writer
        .execute_script("DROP TABLE notes".into())
        .await
        .unwrap();
    drop(writer);
    gate.refuse.store(true, std::sync::atomic::Ordering::SeqCst);
    for (verb, args) in [
        ("get", json!({"id": "00000000-0000-4000-8000-000000000001"})),
        (
            "brain.feedback",
            json!({"target_id": "00000000", "signal": "useful"}),
        ),
        (
            "brain.auto_feedback",
            json!({"query": "shared", "results": [{"id": "00000000"}], "target_id": "00000000", "signal": "useful"}),
        ),
    ] {
        let error = f.registry.dispatch(verb, args).await.unwrap_err();
        assert!(
            matches!(error, RuntimeError::PermissionDenied { .. }),
            "Gate must run before the broken backend is queried: {error}"
        );
    }
    assert_eq!(feedback_count(&f.main).await, 0);
}
