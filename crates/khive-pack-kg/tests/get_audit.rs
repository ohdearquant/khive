use std::sync::Arc;

use async_trait::async_trait;
use khive_pack_kg::KgPack;
use khive_runtime::pack::AuditSuccessRowsMode;
use khive_runtime::{
    Gate, GateDecision, GateError, GateRequest, InterceptedDispatchResult, KhiveRuntime, Namespace,
    NamespaceToken, PackRuntime, RuntimeConfig, RuntimeError, VerbRegistry, VerbRegistryBuilder,
};
use khive_storage::{Edge, EdgeRelation, Entity, Event, EventFilter, Note, PageRequest};
use khive_types::{
    EventKind, EventOutcome, HandlerDef, Pack, SubstrateKind, VerbCategory, Visibility,
};
use serde_json::{json, Value};
use uuid::Uuid;

const PRIVATE_NAME: &str = "SubjectAuditPrivateName";
const PRIVATE_BODY: &str = "Subject audit private body";
const PRIVATE_KEY: &str = "subject-audit/private-key";

struct SubjectFixture;

impl Pack for SubjectFixture {
    const NAME: &'static str = "subject_fixture";
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = &[
        HandlerDef {
            name: "subject_fixture.unrelated",
            description: "Return an unrelated identifier",
            visibility: Visibility::Verb,
            category: VerbCategory::Assertive,
            params: &[],
        },
        HandlerDef {
            name: "subject_fixture.declared",
            description: "Return an explicitly declared record subject",
            visibility: Visibility::Verb,
            category: VerbCategory::Assertive,
            params: &[],
        },
    ];
}

#[async_trait]
impl PackRuntime for SubjectFixture {
    khive_runtime::pack_runtime_metadata!();

    fn audit_subject_arg(&self, verb: &str) -> Option<&'static str> {
        match verb {
            "subject_fixture.declared" => Some("subject"),
            // A declaration from a pack that does not own get must never win.
            "get" => Some("not_the_subject"),
            _ => None,
        }
    }

    async fn dispatch(
        &self,
        verb: &str,
        params: Value,
        _registry: &VerbRegistry,
        _token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        let id = if verb == "subject_fixture.declared" {
            &params["subject"]
        } else {
            &params["id"]
        };
        Ok(json!({"id": id, "kind": "concept", "content": PRIVATE_BODY}))
    }
}

#[derive(Debug)]
struct DenyGet;

impl Gate for DenyGet {
    fn check(&self, request: &GateRequest) -> Result<GateDecision, GateError> {
        Ok(if request.verb == "get" {
            GateDecision::deny("read denied")
        } else {
            GateDecision::allow()
        })
    }
}

fn fixture(deny_get: bool) -> (KhiveRuntime, VerbRegistry) {
    let runtime = KhiveRuntime::new_for_test(RuntimeConfig {
        db_path: None,
        packs: vec!["kg".into()],
        ..RuntimeConfig::no_embeddings()
    })
    .unwrap();
    let token = runtime.authorize(Namespace::local()).unwrap();
    let mut builder = VerbRegistryBuilder::new();
    builder.with_event_store(runtime.events(&token).unwrap());
    builder.with_audit_success_rows_mode(AuditSuccessRowsMode::All);
    if deny_get {
        builder.with_gate(Arc::new(DenyGet));
    }
    builder.register(SubjectFixture);
    builder.register(KgPack::new(runtime.clone()));
    (runtime, builder.build().unwrap())
}

async fn dispatch(
    runtime: &KhiveRuntime,
    registry: &VerbRegistry,
    intercepted: bool,
    verb: &str,
    args: Value,
) -> Result<Value, RuntimeError> {
    if !intercepted {
        return registry.dispatch(verb, args).await;
    }
    let handler_args = args.clone();
    registry
        .dispatch_intercepted_with_token_and_disposition(verb, &args, None, |token| async move {
            let result = if verb == "get" {
                KgPack::new(runtime.clone())
                    .dispatch(verb, handler_args, registry, &token)
                    .await?
            } else {
                SubjectFixture
                    .dispatch(verb, handler_args, registry, &token)
                    .await?
            };
            Ok(InterceptedDispatchResult::new(result, ()))
        })
        .await
        .map(|outcome| outcome.result)
        .map_err(|error| error.into_source())
}

async fn audits(runtime: &KhiveRuntime, verb: &str) -> Vec<Event> {
    let token = runtime.authorize(Namespace::local()).unwrap();
    runtime
        .events(&token)
        .unwrap()
        .query_events(
            EventFilter {
                kinds: vec![EventKind::Audit],
                ..Default::default()
            },
            PageRequest {
                offset: 0,
                limit: 100,
            },
        )
        .await
        .unwrap()
        .items
        .into_iter()
        .filter(|event| event.verb == verb)
        .collect()
}

fn assert_no_record_content(event: &Event) {
    let serialized = serde_json::to_string(event).unwrap();
    for private in [PRIVATE_NAME, PRIVATE_BODY, PRIVATE_KEY, "forged_kind"] {
        assert!(!serialized.contains(private), "{serialized}");
    }
}

fn assert_subject(event: &Event, id: Uuid, kind: &str) {
    assert_eq!(event.outcome, EventOutcome::Success);
    assert_eq!(event.target_id, Some(id));
    assert_eq!(event.payload["record_kind"], kind);
    assert_eq!(event.kind, EventKind::Audit);
    assert_eq!(event.substrate, SubstrateKind::Event);
    assert_no_record_content(event);
}

async fn seed(runtime: &KhiveRuntime) -> (Entity, Note, Uuid, Uuid) {
    let token = runtime.authorize(Namespace::local()).unwrap();
    let entity = Entity::new("local", "concept", PRIVATE_NAME).with_description(PRIVATE_BODY);
    runtime
        .entities(&token)
        .unwrap()
        .upsert_entity(entity.clone())
        .await
        .unwrap();
    let mut note = Note::new("local", "observation", PRIVATE_BODY);
    note.key = Some(PRIVATE_KEY.into());
    runtime
        .notes(&token)
        .unwrap()
        .upsert_note(note.clone())
        .await
        .unwrap();
    let edge_id = Uuid::new_v4();
    let now = chrono::Utc::now();
    runtime
        .graph(&token)
        .unwrap()
        .upsert_edges(vec![Edge {
            id: edge_id.into(),
            namespace: "local".into(),
            source_id: entity.id,
            target_id: note.id,
            relation: EdgeRelation::Supports,
            weight: 1.0,
            created_at: now,
            updated_at: now,
            deleted_at: None,
            metadata: None,
            target_backend: None,
        }])
        .await
        .unwrap();
    let event = Event::new(
        "local",
        "fixture.seed",
        EventKind::Audit,
        SubstrateKind::Event,
        "fixture",
    )
    .with_payload(json!({"body": PRIVATE_BODY}));
    let event_id = event.id;
    runtime
        .events(&token)
        .unwrap()
        .append_event(event)
        .await
        .unwrap();
    (entity, note, edge_id, event_id)
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn get_audits_record_resolved_subjects_through_both_dispatch_paths() {
    for intercepted in [false, true] {
        for lookup in ["entity", "note", "edge", "event", "prefix", "name", "key"] {
            let (runtime, registry) = fixture(false);
            let (entity, note, edge_id, event_id) = seed(&runtime).await;
            let (args, id, kind) = match lookup {
                "entity" => (
                    json!({"id": entity.id.to_string().to_uppercase()}),
                    entity.id,
                    "concept",
                ),
                "note" => (json!({"id": note.id}), note.id, "observation"),
                "edge" => (json!({"id": edge_id}), edge_id, "edge"),
                "event" => (json!({"id": event_id}), event_id, "event"),
                "prefix" => (
                    json!({"id": &entity.id.simple().to_string()[..16]}),
                    entity.id,
                    "concept",
                ),
                "name" => (json!({"id": PRIVATE_NAME}), entity.id, "concept"),
                "key" => (
                    json!({"key": PRIVATE_KEY, "kind": "note"}),
                    note.id,
                    "observation",
                ),
                _ => unreachable!(),
            };
            let result = dispatch(&runtime, &registry, intercepted, "get", args)
                .await
                .unwrap();
            assert_eq!(
                result["id"],
                id.to_string(),
                "{lookup}, intercepted={intercepted}"
            );
            assert_eq!(result["kind"], kind);
            let events = audits(&runtime, "get").await;
            assert_eq!(events.len(), 1, "{lookup}, intercepted={intercepted}");
            assert_subject(&events[0], id, kind);
        }
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn redirected_get_audits_name_the_returned_record_not_the_requested_tombstone() {
    for intercepted in [false, true] {
        let (runtime, registry) = fixture(false);
        let token = runtime.authorize(Namespace::local()).unwrap();
        let kept = Entity::new("local", "concept", PRIVATE_NAME);
        let mut old = Entity::new("local", "concept", "Absorbed audit fixture");
        old.deleted_at = Some(old.updated_at);
        old.merged_into = Some(kept.id);
        old.merge_event_id = Some(Uuid::new_v4());
        runtime
            .entities(&token)
            .unwrap()
            .upsert_entities(vec![kept.clone(), old.clone()])
            .await
            .unwrap();

        let result = dispatch(
            &runtime,
            &registry,
            intercepted,
            "get",
            json!({"id": old.id}),
        )
        .await
        .unwrap();
        assert_eq!(result["id"], kept.id.to_string());
        assert_eq!(result["redirected_from"], json!([old.id]));
        let events = audits(&runtime, "get").await;
        assert_eq!(
            events.len(),
            2,
            "effective-target gate check and completed read"
        );
        let completed: Vec<_> = events
            .iter()
            .filter(|event| event.payload.get("record_kind").is_some())
            .collect();
        assert_eq!(completed.len(), 1);
        assert_subject(completed[0], kept.id, "concept");
        assert_ne!(completed[0].target_id, Some(old.id));
        assert!(events.iter().all(|event| event.target_id == Some(kept.id)));
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn unrelated_ids_and_results_require_an_owning_pack_subject_declaration() {
    for intercepted in [false, true] {
        for declared in [false, true] {
            let (runtime, registry) = fixture(false);
            let id = Uuid::new_v4();
            let (verb, args) = if declared {
                ("subject_fixture.declared", json!({"subject": id}))
            } else {
                ("subject_fixture.unrelated", json!({"id": id}))
            };
            let result = dispatch(&runtime, &registry, intercepted, verb, args)
                .await
                .unwrap();
            assert_eq!(result["id"], id.to_string());
            assert_eq!(result["kind"], "concept");
            let events = audits(&runtime, verb).await;
            assert_eq!(events.len(), 1);
            if declared {
                assert_subject(&events[0], id, "concept");
            } else {
                assert_eq!(events[0].target_id, None);
                assert!(events[0].payload.get("record_kind").is_none());
                assert_no_record_content(&events[0]);
            }
        }
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn unsuccessful_get_audits_keep_only_a_declared_literal_uuid() {
    for intercepted in [false, true] {
        for denied in [false, true] {
            let id = Uuid::new_v4();
            for (args, target) in [
                (json!({"id": id, "kind": "forged_kind"}), Some(id)),
                (json!({"id": PRIVATE_NAME}), None),
                (json!({"key": PRIVATE_KEY}), None),
            ] {
                let (runtime, registry) = fixture(denied);
                assert!(dispatch(&runtime, &registry, intercepted, "get", args)
                    .await
                    .is_err());
                let events = audits(&runtime, "get").await;
                assert_eq!(events.len(), 1);
                assert_eq!(
                    events[0].outcome,
                    if denied {
                        EventOutcome::Denied
                    } else {
                        EventOutcome::Error
                    }
                );
                assert_eq!(events[0].target_id, target);
                assert!(events[0].payload.get("record_kind").is_none());
                assert_no_record_content(&events[0]);
            }
        }
    }
}
