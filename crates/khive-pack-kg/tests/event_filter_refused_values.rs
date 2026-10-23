//! Event filter refusals preserve ordinary diagnostics and mask detected credentials.
use khive_pack_kg::KgPack;
use khive_runtime::secret_gate::{mask_for_redaction_surface, RedactionSurface};
use khive_runtime::{
    runtime_error_value, DomainDisposition, KhiveRuntime, RuntimeConfig, RuntimeError,
    VerbRegistry, VerbRegistryBuilder,
};
use khive_storage::{Event, EventFilter, PageRequest};
use khive_types::{EventKind, EventOutcome, SubstrateKind};
use serde_json::{json, Value};
use uuid::Uuid;

struct Fixture {
    runtime: KhiveRuntime,
    registry: VerbRegistry,
    wanted: Uuid,
    target: Uuid,
    session: Uuid,
}

impl Fixture {
    async fn new() -> Self {
        let runtime = KhiveRuntime::new(RuntimeConfig {
            db_path: None,
            events_split: None,
            packs: vec!["kg".into()],
            brain_profile: None,
            ..RuntimeConfig::no_embeddings()
        })
        .unwrap();
        assert!(!runtime.backend().is_file_backed());
        assert!(runtime.registered_embedding_model_names().is_empty());
        let mut builder = VerbRegistryBuilder::new();
        builder.register_trusted(KgPack::new(runtime.clone()));
        let registry = builder.build().unwrap();
        let target = Uuid::new_v4();
        let session = Uuid::new_v4();
        let wanted = Event::new(
            "local",
            "fixture.search",
            EventKind::SearchExecuted,
            SubstrateKind::Note,
            "fixture:actor",
        )
        .with_outcome(EventOutcome::Denied)
        .with_target(target)
        .with_session_id(session)
        .with_payload(json!({"candidates":[target], "selected":[target], "result_kind":"note"}));
        let mut hidden = wanted.clone();
        hidden.id = Uuid::new_v4();
        hidden.namespace = "hidden".into();
        let other = Event::new(
            "local",
            "fixture.other",
            EventKind::Audit,
            SubstrateKind::Entity,
            "fixture:actor",
        )
        .with_target(Uuid::new_v4())
        .with_session_id(Uuid::new_v4());
        runtime
            .backend()
            .events_for_namespace("local")
            .unwrap()
            .append_events(vec![wanted.clone(), other])
            .await
            .unwrap();
        runtime
            .backend()
            .events_for_namespace("hidden")
            .unwrap()
            .append_event(hidden)
            .await
            .unwrap();
        Self {
            runtime,
            registry,
            wanted: wanted.id,
            target,
            session,
        }
    }

    async fn snapshot(&self) -> Vec<Event> {
        let mut rows = Vec::new();
        for namespace in ["local", "hidden"] {
            rows.extend(
                self.runtime
                    .backend()
                    .events_for_namespace(namespace)
                    .unwrap()
                    .query_events(
                        EventFilter::default(),
                        PageRequest {
                            limit: 10,
                            offset: 0,
                        },
                    )
                    .await
                    .unwrap()
                    .items,
            );
        }
        assert_eq!(
            rows.len(),
            3,
            "the fixture must contain real visible and hidden events"
        );
        rows
    }

    async fn assert_one(&self, verb: &str, args: Value) {
        let result = self.registry.dispatch(verb, args).await.unwrap();
        if verb == "count" {
            assert_eq!(result, json!({"fixture.search":1}));
        } else {
            let items = result["items"].as_array().unwrap();
            assert_eq!(items.len(), 1, "{result}");
            assert_eq!(items[0]["id"], self.wanted.to_string());
            assert_eq!(items[0]["namespace"], "local");
        }
    }
}

fn args(verb: &str) -> Value {
    if verb == "count" {
        json!({"kind":"event", "group_by":"verb"})
    } else {
        json!({"kind":"event"})
    }
}

fn operands() -> [String; 2] {
    ["bogus".into(), format!("sk-proj-{}", "A".repeat(80))]
}

fn enum_diagnostic(field: &str, raw: &str) -> String {
    match field {
        "outcome" => format!("unknown outcome {raw:?}; valid: success | denied | error"),
        "substrate" => format!("unknown substrate {raw:?}; valid: note | entity | event"),
        "event_kind" | "event_kinds" => format!(
            "unknown event_kind {raw:?}: {}",
            raw.parse::<EventKind>().unwrap_err()
        ),
        _ => panic!("unexpected fixture field"),
    }
}

fn assert_refusal(error: RuntimeError, raw: &str, original_diagnostic: String) {
    let RuntimeError::InvalidInput(message) = &error else {
        panic!("wrong refusal kind: {error:?}");
    };
    let expected = if raw == "bogus" {
        original_diagnostic
    } else {
        assert!(
            !message.contains(raw) && !message.contains(&raw.to_ascii_lowercase()),
            "credential escaped from event filter: {message}"
        );
        mask_for_redaction_surface(RedactionSurface::GateProbe, &original_diagnostic).into_owned()
    };
    assert_eq!(message, &expected);
    assert_eq!(
        runtime_error_value(error, DomainDisposition::NotCommitted),
        json!({"kind":"runtime_error", "message":format!("invalid input: {expected}"),
            "domain_disposition":"not_committed"})
    );
}

async fn enum_case(field: &str, accepted: &str) {
    let fixture = Fixture::new().await;
    let before = fixture.snapshot().await;
    let fields: &[&str] = if field == "event_kind" {
        &["event_kind", "event_kinds"]
    } else {
        &[field]
    };
    for verb in ["list", "count"] {
        for field in fields {
            for raw in operands() {
                let mut input = args(verb);
                input[*field] = if *field == "event_kinds" {
                    json!(["audit", raw])
                } else {
                    json!(raw)
                };
                let error = fixture.registry.dispatch(verb, input).await.unwrap_err();
                assert_refusal(error, &raw, enum_diagnostic(field, &raw));
            }
            let mut input = args(verb);
            input[*field] = if *field == "event_kinds" {
                json!([accepted])
            } else {
                json!(accepted)
            };
            fixture.assert_one(verb, input).await;
        }
        // The two public callers deliberately validate these filters in different orders.
        let input = json!({"kind":"event", "group_by":"verb", "substrate":"bogus", "outcome":"bogus", "event_kind":"bogus"});
        let mut input = input;
        if verb == "list" {
            input.as_object_mut().unwrap().remove("group_by");
        }
        let first = if verb == "list" {
            "substrate"
        } else {
            "event_kind"
        };
        assert_refusal(
            fixture.registry.dispatch(verb, input).await.unwrap_err(),
            "bogus",
            enum_diagnostic(first, "bogus"),
        );
    }
    assert_eq!(
        fixture.snapshot().await,
        before,
        "read refusals must not change stored events"
    );
}

#[tokio::test]
async fn outcome_refusals_mask_both_public_event_readers() {
    enum_case("outcome", " DeNiEd ").await;
}

#[tokio::test]
async fn substrate_refusals_mask_both_public_event_readers() {
    enum_case("substrate", " NoTe ").await;
}

#[tokio::test]
async fn kind_refusals_mask_both_spellings_and_nested_diagnostic() {
    enum_case("event_kind", " SeArCh_ExEcUtEd ").await;
}

async fn id_case(field: &str) {
    let fixture = Fixture::new().await;
    let before = fixture.snapshot().await;
    for raw in operands() {
        let mut input = args("list");
        input[field] = if matches!(field, "observed" | "selected") {
            json!([raw])
        } else {
            json!(raw)
        };
        let original = format!(
            "list: {field} must contain a full UUID because a short-prefix resolution can miss \
             or be ambiguous, while an event filter must identify the exact stable record; \
             got {raw:?}: {}",
            Uuid::parse_str(&raw).unwrap_err()
        );
        assert_refusal(
            fixture.registry.dispatch("list", input).await.unwrap_err(),
            &raw,
            original,
        );
    }
    let id = if field == "session_id" {
        fixture.session
    } else {
        fixture.target
    };
    let mut input = args("list");
    input[field] = if matches!(field, "observed" | "selected") {
        json!([id])
    } else {
        json!(id)
    };
    fixture.assert_one("list", input).await;
    assert_eq!(
        fixture.snapshot().await,
        before,
        "read refusals must not change stored events"
    );
}

#[tokio::test]
async fn target_refusal_masks_exact_event_filter_operand() {
    id_case("target_id").await;
}
#[tokio::test]
async fn session_refusal_masks_exact_event_filter_operand() {
    id_case("session_id").await;
}
#[tokio::test]
async fn observed_refusal_masks_exact_event_filter_operand() {
    id_case("observed").await;
}
#[tokio::test]
async fn selected_refusal_masks_exact_event_filter_operand() {
    id_case("selected").await;
}
