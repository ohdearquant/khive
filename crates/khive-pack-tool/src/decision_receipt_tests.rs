use super::*;
use std::collections::HashMap;
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
async fn same_microsecond_rows_list_newest_id_first() {
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
}

/// A credential-shaped value built at run time from parts.
fn credential_shaped() -> String {
    format!("{}{}", ["gh", "p_"].concat(), "A".repeat(36))
}

#[tokio::test]
async fn credential_shaped_check_input_leaves_no_receipt_carrying_it() {
    let secret = credential_shaped();
    for field in ["actor", "tool"] {
        let fixture = Fixture::new(false);
        let mut params = json!({"tool": "unregistered", "actor": "agent:receipt"});
        params[field] = json!(secret);

        let error = fixture
            .registry
            .dispatch("tool.check", params)
            .await
            .expect_err("credential-shaped input must be refused");
        assert!(
            matches!(error, RuntimeError::SecretDetected(_)),
            "{field}: {error:?}"
        );
        assert!(
            !error.to_string().contains(&secret),
            "{field}: the refusal must not echo the value"
        );

        let events = fixture
            .runtime
            .list_events(
                &fixture.token,
                EventFilter::default(),
                PageRequest {
                    limit: 100,
                    offset: 0,
                },
            )
            .await
            .expect("list all events");
        for event in &events.items {
            assert!(
                !serde_json::to_string(event)
                    .expect("event serializes")
                    .contains(&secret),
                "{field}: an event row stores the credential-shaped value"
            );
        }
        assert!(fixture.decisions(0).await.is_empty(), "{field}");
    }
}

/// Caller side of the catch-up procedure in the ADR: re-read a window behind
/// the newest row seen, and deduplicate by event id instead of by id order.
struct DecisionPoller {
    window: i64,
    newest: i64,
    seen: HashMap<Uuid, i64>,
}

impl DecisionPoller {
    fn new(window: i64) -> Self {
        Self {
            window,
            newest: 0,
            seen: HashMap::new(),
        }
    }

    async fn poll(&mut self, fixture: &Fixture) -> Vec<Uuid> {
        let since = self.newest.saturating_sub(self.window + 1).max(0);
        let mut fresh = Vec::new();
        for row in fixture.decisions(since).await {
            let at = khive_runtime::rfc3339_to_utc_micros(row["created_at"].as_str().unwrap())
                .expect("created_at parses");
            let id = Uuid::parse_str(row["id"].as_str().unwrap()).expect("event id");
            if self.seen.insert(id, at).is_none() {
                fresh.push((at, id));
            }
            self.newest = self.newest.max(at);
        }
        let floor = self.newest - self.window;
        self.seen.retain(|_, at| *at >= floor);
        fresh.sort();
        fresh.into_iter().map(|(_, id)| id).collect()
    }
}

async fn append_receipt(fixture: &Fixture, id: Uuid, created_at: i64) {
    let mut event = khive_storage::Event::new(
        "local",
        "tool.check",
        EventKind::ToolCheckDecided,
        SubstrateKind::Event,
        actor_label(&fixture.token),
    )
    .with_payload(json!({"actor":"agent:receipt","tool":"late"}));
    event.id = id;
    event.created_at = created_at;
    fixture
        .runtime
        .events(&fixture.token)
        .expect("event store")
        .append_event(event)
        .await
        .expect("receipt append");
}

#[tokio::test]
async fn late_arrival_sharing_the_cursor_microsecond_is_returned() {
    let fixture = Fixture::new(false);
    let timestamp = now_micros();
    let first = Uuid::from_u128(2);
    let mut poller = DecisionPoller::new(1_000_000);

    append_receipt(&fixture, first, timestamp).await;
    assert_eq!(poller.poll(&fixture).await, vec![first]);

    // Appended after the cursor was saved: same microsecond, lower id.
    let late = Uuid::from_u128(1);
    append_receipt(&fixture, late, timestamp).await;
    assert_eq!(poller.poll(&fixture).await, vec![late]);
    assert!(poller.poll(&fixture).await.is_empty());
}

#[tokio::test]
async fn late_arrival_older_than_the_cursor_is_returned_inside_the_window_only() {
    let fixture = Fixture::new(false);
    let window = 1_000_000;
    let timestamp = now_micros();
    let mut poller = DecisionPoller::new(window);

    append_receipt(&fixture, Uuid::from_u128(5), timestamp).await;
    poller.poll(&fixture).await;

    // Stamped before the cursor row but committed after it was read.
    let inside = Uuid::from_u128(3);
    let outside = Uuid::from_u128(4);
    append_receipt(&fixture, inside, timestamp - window / 2).await;
    append_receipt(&fixture, outside, timestamp - window * 2).await;
    assert_eq!(poller.poll(&fixture).await, vec![inside]);
}

#[tokio::test]
async fn poller_keeps_only_the_ids_inside_the_window() {
    let fixture = Fixture::new(false);
    let window = 1_000_000;
    let timestamp = now_micros();
    let mut poller = DecisionPoller::new(window);

    append_receipt(&fixture, Uuid::from_u128(7), timestamp - window * 10).await;
    append_receipt(&fixture, Uuid::from_u128(8), timestamp).await;
    assert_eq!(poller.poll(&fixture).await.len(), 2);
    assert_eq!(poller.seen.len(), 1);
    assert!(poller.seen.contains_key(&Uuid::from_u128(8)));
}
