//! Domain-write restrictions at the real dispatch and broad-token boundaries.
//! These tests deliberately permit normal audit/recall telemetry persistence.

use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::time::Duration;

use async_trait::async_trait;
use khive_runtime::{
    mount_config::MountEffect, mounted_verb::MountedVerb, CallerEnrollmentGate, KhiveRuntime,
    Namespace, NamespaceToken, PackRuntime, RequestIdentity, RuntimeConfig, RuntimeError,
    VerbRegistry, VerbRegistryBuilder, VerifiedActor,
};
use khive_storage::{types::PageRequest, EventFilter};
use khive_types::{EventKind, EventOutcome, HandlerDef};
use serde_json::{json, Value};

const DUTY: &str = "test:duty";
const WRITER: &str = "test:writer";

fn runtime(actor: &str) -> KhiveRuntime {
    KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        actor_id: Some(actor.into()),
        default_namespace: Namespace::local(),
        visible_namespaces: vec![],
        brain_profile: None,
        packs: ["kg", "gtd", "comm", "schedule", "memory", "brain"]
            .into_iter()
            .map(str::to_owned)
            .collect(),
        gate: Arc::new(CallerEnrollmentGate::with_write_denials(
            vec![DUTY.into(), WRITER.into()],
            false,
            vec!["*:duty".into()],
        )),
        ..RuntimeConfig::no_embeddings()
    })
    .expect("isolated in-memory runtime")
}

fn builder(runtime: &KhiveRuntime) -> VerbRegistryBuilder {
    let mut builder = VerbRegistryBuilder::new();
    builder.with_actor_id(runtime.config().actor_id.clone());
    builder.with_gate(runtime.config().gate.clone());
    builder.with_runtime_event_store(runtime).unwrap();
    builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
    builder.register(khive_pack_gtd::GtdPack::new(runtime.clone()));
    builder.register(khive_pack_comm::CommPack::new(runtime.clone()));
    builder.register(khive_pack_schedule::SchedulePack::new(runtime.clone()));
    builder.register(khive_pack_memory::MemoryPack::new(runtime.clone()));
    builder.register(khive_pack_brain::BrainPack::new(runtime.clone()));
    builder
}

fn build_registry(runtime: &KhiveRuntime, builder: VerbRegistryBuilder) -> VerbRegistry {
    let registry = builder.build().expect("policy registry builds");
    // Match the MCP single-backend boot path: registration alone does not
    // install pack-owned schema, including the indexes required by comm.probe.
    registry
        .apply_schema_plans_with_map(&Default::default(), runtime.backend())
        .expect("all registered pack schema plans apply");
    registry
}

fn identity(actor: &str) -> RequestIdentity {
    RequestIdentity {
        namespace: "local".into(),
        actor_id: Some(actor.into()),
        process_ref: Some("unrestricted-writer-label".into()),
        ..Default::default()
    }
}

fn denied(error: RuntimeError) -> Option<uuid::Uuid> {
    let RuntimeError::PermissionDenied {
        reason, receipt, ..
    } = error
    else {
        panic!("expected policy denial, got {error:?}");
    };
    assert!(reason.contains("deny_writes_for"), "{reason}");
    receipt.audit_event_id
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn policy_reads_work_and_domain_writes_deny_with_audit_and_writer_control() {
    let runtime = runtime(WRITER);
    let token = runtime.authorize(Namespace::local()).unwrap();
    let registry = build_registry(&runtime, builder(&runtime));
    let seed = registry
        .dispatch(
            "create",
            json!({
                "kind": "observation", "content": "domain policy sentinel"
            }),
        )
        .await
        .unwrap();
    let seed_id = seed["id"].as_str().expect("created note id");
    let seed_uuid = uuid::Uuid::parse_str(seed_id).unwrap();
    let notes = runtime.notes(&token).unwrap();
    let before = notes.get_note(seed_uuid).await.unwrap().unwrap();
    let note_count = notes.count_notes("local", None).await.unwrap();

    for (verb, args) in [
        ("get", json!({"id": seed_id})),
        ("list", json!({"kind": "note"})),
        ("stats", json!({})),
        ("verbs", json!({})),
        ("whoami", json!({})),
        ("comm.inbox", json!({})),
        ("comm.unread", json!({})),
        ("comm.health", json!({})),
        ("comm.probe", json!({"actor": DUTY})),
        ("gtd.next", json!({})),
        ("gtd.tasks", json!({})),
    ] {
        registry
            .dispatch_with_identity(verb, args, Some(identity(DUTY)))
            .await
            .unwrap_or_else(|err| panic!("approved read {verb}: {err}"));
    }

    let mut denial_ids = vec![];
    for (verb, args) in [
        (
            "create",
            json!({"kind":"observation", "content":"must not exist"}),
        ),
        ("update", json!({"id":seed_id, "content":"must not change"})),
        ("delete", json!({"id":seed_id})),
        (
            "link",
            json!({"source_id":seed_id, "target_id":seed_id, "relation":"relates_to"}),
        ),
        ("merge", json!({"source_id":seed_id, "target_id":seed_id})),
        ("gtd.transition", json!({"id":seed_id, "status":"done"})),
        ("gtd.complete", json!({"id":seed_id})),
        ("schedule.cancel", json!({"id":seed_id})),
        ("comm.read", json!({"id":seed_id})),
        ("comm.mark_read", json!({"ids":[seed_id]})),
        (
            "comm.send",
            json!({"to":WRITER, "subject":"blocked", "content":"blocked"}),
        ),
        ("telemetry.emit", json!({"content":"blocked"})),
        ("brain.emit", json!({})),
        ("brain.feedback", json!({})),
    ] {
        let id = denied(
            registry
                .dispatch_with_identity(verb, args, Some(identity(DUTY)))
                .await
                .unwrap_err(),
        )
        .expect("denied dispatch persisted an audit");
        denial_ids.push((verb, id));
    }
    assert_eq!(notes.count_notes("local", None).await.unwrap(), note_count);
    assert_eq!(notes.get_note(seed_uuid).await.unwrap().unwrap(), before);
    let events = runtime.events(&token).unwrap();
    for (verb, id) in denial_ids {
        let event = events.get_event(id).await.unwrap().unwrap();
        assert_eq!(event.kind, EventKind::Audit);
        assert_eq!(event.outcome, EventOutcome::Denied);
        assert_eq!(event.verb, verb);
        assert_eq!(event.actor, format!("actor:{DUTY}"));
    }
    registry
        .dispatch(
            "update",
            json!({"id":seed_id, "content":"writer changed it"}),
        )
        .await
        .unwrap();
    assert_eq!(
        notes.get_note(seed_uuid).await.unwrap().unwrap().content,
        "writer changed it"
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn broad_token_minting_denies_but_concrete_read_dispatch_succeeds() {
    let runtime = runtime(DUTY);
    denied(runtime.authorize(Namespace::local()).unwrap_err());
    denied(
        runtime
            .authorize_with_visibility(Namespace::local(), vec![Namespace::parse("other").unwrap()])
            .unwrap_err(),
    );
    let registry = build_registry(&runtime, builder(&runtime));
    registry
        .dispatch("list", json!({"kind":"note"}))
        .await
        .unwrap();
    denied(
        registry
            .authorize_namespace(Namespace::local())
            .unwrap_err(),
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn effective_identity_controls_canonical_verified_and_intercepted_dispatch() {
    let runtime = runtime(WRITER);
    let registry = build_registry(&runtime, builder(&runtime));
    let create = json!({"kind":"observation", "content":"identity sentinel",
        "actor":WRITER, "process_ref":WRITER, "namespace":"other"});
    denied(
        registry
            .dispatch_with_identity("create", create.clone(), Some(identity(DUTY)))
            .await
            .unwrap_err(),
    );
    denied(
        registry
            .dispatch_as("create", create.clone(), VerifiedActor::new(DUTY).unwrap())
            .await
            .unwrap_err(),
    );
    let who = registry
        .dispatch_with_identity("whoami", json!({}), Some(identity(DUTY)))
        .await
        .unwrap();
    assert!(who.to_string().contains(DUTY), "{who}");
    assert!(!who.to_string().contains(WRITER), "{who}");

    let calls = AtomicUsize::new(0);
    let duty = identity(DUTY);
    denied(
        registry
            .dispatch_intercepted_with_identity("create", &create, Some(&duty), |_| async {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(json!({}))
            })
            .await
            .unwrap_err(),
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    registry
        .dispatch_intercepted_with_identity("list", &json!({}), Some(&duty), |_| async {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(json!({"read":true}))
        })
        .await
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let error = registry
        .dispatch_with_identity("list", json!({}), Some(identity("not-enrolled")))
        .await
        .unwrap_err();
    assert!(
        matches!(error, RuntimeError::PermissionDenied { reason, .. } if reason == "actor is not enrolled")
    );
    // An argument label cannot restrict the baked writer either. Shared create
    // rejects unknown user keys, so use the intercepted seam to isolate matching.
    registry
        .dispatch_intercepted_with_identity("create", &json!({"actor":DUTY}), None, |_| async {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(json!({}))
        })
        .await
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    registry
        .dispatch(
            "create",
            json!({"kind":"observation", "content":"writer control"}),
        )
        .await
        .unwrap();
}

struct MountedRead(Arc<AtomicUsize>);

#[async_trait]
impl PackRuntime for MountedRead {
    fn name(&self) -> &str {
        "mounted"
    }
    fn note_kinds(&self) -> &'static [&'static str] {
        &[]
    }
    fn entity_kinds(&self) -> &'static [&'static str] {
        &[]
    }
    fn handlers(&self) -> &'static [HandlerDef] {
        &[]
    }
    fn mounted_namespace(&self) -> Option<&str> {
        Some("mounted")
    }
    fn mounted_catalog_snapshot(&self) -> Vec<MountedVerb> {
        vec![MountedVerb {
            name: "read".into(),
            description: None,
            input_schema: json!({"type":"object"}),
            output_schema: None,
            effect: MountEffect::Read,
            digest: "fixture".into(),
            generation: 1,
        }]
    }
    async fn mounted_catalog(&self) -> Result<Vec<MountedVerb>, RuntimeError> {
        Ok(self.mounted_catalog_snapshot())
    }
    async fn dispatch(
        &self,
        _verb: &str,
        _params: Value,
        _registry: &VerbRegistry,
        _token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(json!({"read":true}))
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn unknown_and_mounted_reads_deny_before_handler_but_help_stays_introspection() {
    let runtime = runtime(WRITER);
    let calls = Arc::new(AtomicUsize::new(0));
    let mut builder = builder(&runtime);
    builder
        .register_mounted(Box::new(MountedRead(calls.clone())))
        .unwrap();
    let registry = build_registry(&runtime, builder);
    for verb in ["unloaded.read", "mounted.read"] {
        denied(
            registry
                .dispatch_with_identity(verb, json!({}), Some(identity(DUTY)))
                .await
                .unwrap_err(),
        );
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(matches!(
        registry
            .dispatch("unloaded.read", json!({}))
            .await
            .unwrap_err(),
        RuntimeError::UnknownVerb(_)
    ));
    registry.dispatch("mounted.read", json!({})).await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    // Existing help=true short-circuit describes a mutator without invoking it.
    registry
        .dispatch_with_identity("create", json!({"help":true}), Some(identity(DUTY)))
        .await
        .unwrap();
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn recall_returns_hits_while_nested_serve_ledger_dispatch_is_denied() {
    let runtime = runtime(WRITER);
    let token = runtime.authorize(Namespace::local()).unwrap();
    runtime
        .create_note(
            &token,
            "memory",
            None,
            "read policy recall sentinel",
            Some(0.7),
            None,
            vec![],
        )
        .await
        .unwrap();
    let registry = build_registry(&runtime, builder(&runtime));
    let result = registry
        .dispatch_with_identity(
            "memory.recall",
            json!({"query":"read policy recall sentinel", "limit":10}),
            Some(identity(DUTY)),
        )
        .await
        .unwrap();
    assert!(!result
        .as_array()
        .expect("nonempty recall result array")
        .is_empty());
    let events = runtime.events(&token).unwrap();
    let mut recall = vec![];
    for _ in 0..100 {
        recall = events
            .query_events(
                EventFilter {
                    kinds: vec![EventKind::RecallExecuted],
                    ..Default::default()
                },
                PageRequest {
                    limit: 50,
                    offset: 0,
                },
            )
            .await
            .unwrap()
            .items;
        if !recall.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        recall.len(),
        1,
        "read telemetry is permitted incidental persistence"
    );
    assert_eq!(recall[0].actor, format!("actor:{DUTY}"));
    let ledger_audits = events
        .query_events(
            EventFilter {
                kinds: vec![EventKind::Audit],
                verbs: vec!["brain.record_serve".into()],
                ..Default::default()
            },
            PageRequest {
                limit: 50,
                offset: 0,
            },
        )
        .await
        .unwrap()
        .items;
    assert_eq!(ledger_audits.len(), 1);
    assert_eq!(ledger_audits[0].actor, format!("actor:{DUTY}"));
    assert_eq!(ledger_audits[0].outcome, EventOutcome::Denied);
    let mut reader = runtime.sql().reader().await.unwrap();
    let row = reader
        .query_row(khive_storage::types::SqlStatement {
            sql: "SELECT COUNT(*) AS count FROM brain_serve_ledger".into(),
            params: vec![],
            label: None,
        })
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        row.get("count"),
        Some(khive_storage::types::SqlValue::Integer(0))
    ));
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn invalid_programmatic_policy_never_executes_either_dispatch_seam() {
    let valid = runtime(WRITER);
    let mut config = valid.config().clone();
    config.gate = Arc::new(CallerEnrollmentGate::with_write_denials(
        vec![WRITER.into()],
        false,
        vec![String::new()],
    ));
    let runtime = KhiveRuntime::new(config).unwrap();
    assert!(runtime.authorize(Namespace::local()).is_err());
    assert!(runtime
        .authorize_with_visibility(Namespace::local(), vec![])
        .is_err());
    let registry = build_registry(&runtime, builder(&runtime));
    assert!(matches!(
        registry
            .dispatch("list", json!({"kind":"note"}))
            .await
            .unwrap_err(),
        RuntimeError::GateUnavailable { .. }
    ));
    let calls = AtomicUsize::new(0);
    let error = registry
        .dispatch_intercepted_with_identity("list", &json!({}), None, |_| async {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(json!({}))
        })
        .await
        .unwrap_err();
    assert!(matches!(error, RuntimeError::GateUnavailable { .. }));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}
