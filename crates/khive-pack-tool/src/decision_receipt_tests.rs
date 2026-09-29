use super::*;
use std::sync::Arc;

use khive_gate::{Gate, GateDecision, GateError, GateRequest};
use khive_pack_kg::KgPack;
use khive_runtime::{Namespace, VerbRegistryBuilder};
use khive_storage::types::PageRequest;
use khive_storage::EventFilter;
use khive_types::{EventKind, SubstrateKind};

#[derive(Debug)]
struct DenyCheckGate;

impl Gate for DenyCheckGate {
    fn check(&self, request: &GateRequest) -> Result<GateDecision, GateError> {
        if request.verb == "tool.check" {
            Ok(GateDecision::deny("test gate denial"))
        } else {
            Ok(GateDecision::allow())
        }
    }
}

struct Fixture {
    runtime: KhiveRuntime,
    registry: VerbRegistry,
    token: NamespaceToken,
}

impl Fixture {
    fn new(deny_check: bool) -> Self {
        let runtime = KhiveRuntime::memory().expect("memory runtime");
        let token = runtime
            .authorize(Namespace::parse("local").expect("test namespace"))
            .expect("namespace token");
        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(runtime.clone()));
        builder.register(crate::ToolPack::new(runtime.clone()));
        builder.with_event_store(runtime.events(&token).expect("event store"));
        if deny_check {
            builder.with_gate(Arc::new(DenyCheckGate));
        }
        let registry = builder.build().expect("registry builds");
        registry.apply_schema_plans(runtime.backend());
        runtime.install_edge_rules(registry.all_edge_rules());
        Self {
            runtime,
            registry,
            token,
        }
    }

    async fn decisions(&self, since: i64) -> Vec<Value> {
        let result = self
            .registry
            .dispatch(
                "list",
                json!({
                    "kind": "event",
                    "event_kinds": ["tool_check_decided"],
                    "verb": "tool.check",
                    "actor": format!("{}:{}", self.token.actor().kind, self.token.actor().id),
                    "since": since,
                    "limit": 20,
                }),
            )
            .await
            .expect("list decision events");
        result["items"].as_array().expect("list items").clone()
    }

    async fn set_policy(&self, tool: &str, decision: &str, replaces: Option<&str>) -> String {
        let mut params = json!({
            "actor": "agent:receipt",
            "tool": tool,
            "decision": decision,
        });
        if let Some(id) = replaces {
            params["replaces"] = json!(id);
        }
        let result = self
            .registry
            .dispatch("tool.policy", params)
            .await
            .expect("policy write");
        result["policy"]["id"]
            .as_str()
            .expect("policy id")
            .to_string()
    }
}

#[tokio::test]
async fn gate_refusal_has_no_decision_receipt_after_cursor() {
    let fixture = Fixture::new(true);
    let cursor = now_micros().saturating_sub(1);
    let error = fixture
        .registry
        .dispatch(
            "tool.check",
            json!({"tool": "unregistered", "actor": "agent:receipt"}),
        )
        .await
        .expect_err("gate must refuse before the handler");
    assert!(matches!(error, RuntimeError::PermissionDenied { .. }));
    assert!(fixture.decisions(cursor).await.is_empty());

    let audit = fixture
        .runtime
        .list_events(
            &fixture.token,
            EventFilter {
                kinds: vec![EventKind::Audit],
                verbs: vec!["tool.check".into()],
                ..EventFilter::default()
            },
            PageRequest {
                limit: 10,
                offset: 0,
            },
        )
        .await
        .expect("gate audit query");
    assert_eq!(audit.items.len(), 1);
}

#[tokio::test]
async fn allowed_gate_yields_one_typed_receipt_with_call_data() {
    let fixture = Fixture::new(false);
    let cursor = now_micros().saturating_sub(1);
    let result = fixture
        .registry
        .dispatch(
            "tool.check",
            json!({"tool": "unregistered", "actor": "agent:receipt"}),
        )
        .await
        .expect("tool.check");
    assert_eq!(result["decision"], "ask");
    let rows = fixture.decisions(cursor).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["kind"], "tool_check_decided");
    assert_eq!(rows[0]["namespace"], "local");
    let stored_actor = format!(
        "{}:{}",
        fixture.token.actor().kind,
        fixture.token.actor().id
    );
    assert_eq!(rows[0]["actor"].as_str(), Some(stored_actor.as_str()));
    assert_eq!(rows[0]["payload"]["actor"], "agent:receipt");
    assert_eq!(rows[0]["payload"]["tool"], "unregistered");
    assert_eq!(rows[0]["payload"]["registered"], false);
    assert_eq!(rows[0]["payload"]["decision"], "ask");
    assert_eq!(rows[0]["payload"]["source"], "default");
    assert_eq!(rows[0]["payload"]["id"], Value::Null);
    assert_eq!(rows[0]["payload"]["scope"], Value::Null);
    assert_eq!(rows[0]["payload"]["caller_verb"], "tool.check");
}

#[tokio::test]
async fn opposite_policy_sequences_are_distinguishable_from_rows() {
    let fixture = Fixture::new(false);
    let cursor = now_micros().saturating_sub(1);
    let first = fixture
        .registry
        .dispatch(
            "tool.policy",
            json!({"actor": "agent:receipt", "tool": "deny-first", "decision": "deny"}),
        )
        .await
        .expect("deny policy");
    fixture
        .registry
        .dispatch(
            "tool.check",
            json!({"tool": "deny-first", "actor": "agent:receipt"}),
        )
        .await
        .expect("first check");
    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    fixture
        .set_policy("deny-first", "allow", first["policy"]["id"].as_str())
        .await;
    fixture
        .registry
        .dispatch(
            "tool.check",
            json!({"tool": "deny-first", "actor": "agent:receipt"}),
        )
        .await
        .expect("second check");

    let allow_id = fixture.set_policy("allow-first", "allow", None).await;
    fixture
        .registry
        .dispatch(
            "tool.check",
            json!({"tool": "allow-first", "actor": "agent:receipt"}),
        )
        .await
        .expect("third check");
    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    let rows = fixture.decisions(cursor).await;
    assert_eq!(rows.len(), 3);
    fixture
        .set_policy("allow-first", "deny", Some(&allow_id))
        .await;
    fixture
        .registry
        .dispatch(
            "tool.check",
            json!({"tool": "allow-first", "actor": "agent:receipt"}),
        )
        .await
        .expect("fourth check");

    let rows = fixture.decisions(cursor).await;
    assert_eq!(rows.len(), 4);
    let sequence = |tool: &str| {
        rows.iter()
            .rev()
            .filter(|row| row["payload"]["tool"] == tool)
            .map(|row| row["payload"]["decision"].as_str().unwrap().to_string())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        sequence("deny-first"),
        vec!["deny".to_string(), "allow".to_string()]
    );
    assert_eq!(
        sequence("allow-first"),
        vec!["allow".to_string(), "deny".to_string()]
    );
}

#[tokio::test]
async fn same_microsecond_cursor_replays_tie_then_filters_by_id() {
    let fixture = Fixture::new(false);
    let timestamp = now_micros();
    let events = fixture.runtime.events(&fixture.token).expect("event store");
    for id in [Uuid::from_u128(1), Uuid::from_u128(2)] {
        let mut event = khive_storage::Event::new(
            "local",
            "tool.check",
            EventKind::ToolCheckDecided,
            SubstrateKind::Event,
            actor_label(&fixture.token),
        )
        .with_payload(json!({"actor":"agent:receipt","tool":"tie"}));
        event.id = id;
        event.created_at = timestamp;
        events.append_event(event).await.expect("tie event append");
    }
    let page = fixture
        .runtime
        .list_events(
            &fixture.token,
            EventFilter {
                kinds: vec![EventKind::ToolCheckDecided],
                after: Some(timestamp.saturating_sub(1).max(0)),
                ..EventFilter::default()
            },
            PageRequest {
                limit: 10,
                offset: 0,
            },
        )
        .await
        .expect("tie page");
    assert_eq!(page.items.len(), 2);
    assert_eq!(page.items[0].id, Uuid::from_u128(2));
    assert_eq!(page.items[1].id, Uuid::from_u128(1));
    let after_cursor = page
        .items
        .into_iter()
        .filter(|row| (row.created_at, row.id) > (timestamp, Uuid::from_u128(1)))
        .collect::<Vec<_>>();
    assert_eq!(after_cursor.len(), 1);
    assert_eq!(after_cursor[0].id, Uuid::from_u128(2));
}
