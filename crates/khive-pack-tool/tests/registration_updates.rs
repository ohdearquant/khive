//! Registration corrections and equal replays through the public registry.

use khive_pack_kg::KgPack;
use khive_pack_tool::{RegistryPin, ToolPack};
use khive_runtime::{KhiveRuntime, Namespace, VerbRegistry, VerbRegistryBuilder};
use khive_storage::Entity;
use serde_json::{json, Value};
use uuid::Uuid;

struct Fixture {
    runtime: KhiveRuntime,
    registry: VerbRegistry,
}

impl Fixture {
    fn new() -> Self {
        let runtime = KhiveRuntime::memory().unwrap();
        assert!(runtime.backend().pool().canonical_path().is_none());
        assert!(runtime.default_embedder_name().is_empty());
        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(runtime.clone()));
        builder.register(ToolPack::new(runtime.clone()));
        let registry = builder.build().unwrap();
        registry
            .apply_schema_plans_with_map(&Default::default(), runtime.backend())
            .unwrap();
        runtime.install_edge_rules(registry.all_edge_rules());
        Self { runtime, registry }
    }

    async fn call(&self, verb: &str, params: Value) -> Value {
        self.registry.dispatch(verb, params).await.unwrap()
    }

    async fn row(&self, id: &Value) -> Entity {
        let token = self.runtime.authorize(Namespace::local()).unwrap();
        self.runtime
            .get_entity(&token, id.as_str().unwrap().parse().unwrap())
            .await
            .unwrap()
    }

    async fn describe(&self, name: &str) -> Value {
        self.call("tool.describe", json!({"tool": name})).await["tool"].clone()
    }
}

fn assert_outcome(response: &Value, created: bool, updated: bool, unchanged: bool) {
    assert_eq!(response["created"], created, "{response}");
    assert_eq!(response["updated"], updated, "{response}");
    assert_eq!(response["unchanged"], unchanged, "{response}");
}

#[tokio::test]
async fn register_updates_supplied_fields_and_preserves_identity() {
    let f = Fixture::new();
    let first = f
        .call(
            "tool.register",
            json!({"name": "Echo", "source": "mcp:example", "trust": "first_party"}),
        )
        .await;
    assert_outcome(&first, true, false, false);
    let id = &first["tool"]["full_id"];
    let original = f.row(id).await;
    let schema = json!({"type": "object", "properties": {"old": {"type": "string"}}});
    let corrected = f
        .call("tool.register", json!({"name": "echo", "schema": schema}))
        .await;
    assert_outcome(&corrected, false, true, false);
    assert_eq!(corrected["tool"]["full_id"], *id);
    assert_eq!(f.describe("echo").await["schema"], schema);

    let token = f.runtime.authorize(Namespace::local()).unwrap();
    let mut with_extension = f.row(id).await;
    with_extension.properties.as_mut().unwrap()["extension"] = json!({"retained": true});
    f.runtime
        .entities(&token)
        .unwrap()
        .upsert_entity(with_extension)
        .await
        .unwrap();
    let before = f.row(id).await;
    let replacement = json!({"type": "object", "properties": {"new": {"type": "integer"}}});
    let updated = f
        .call(
            "tool.register",
            json!({
                "name": "ECHO", "kind": "tool", "description": "replacement searchable nebula",
                "schema": replacement, "source": "mcp:example", "side_effect": "egress",
                "trust": "marketplace", "tags": ["extra", "extra"],
            }),
        )
        .await;
    assert_outcome(&updated, false, true, false);
    let after = f.row(id).await;
    assert_eq!(after.id, original.id);
    assert_eq!(after.name, original.name);
    assert_eq!(after.namespace, original.namespace);
    assert_eq!(after.created_at, original.created_at);
    assert_eq!(
        after.properties.as_ref().unwrap()["registered_at"],
        original.properties.as_ref().unwrap()["registered_at"]
    );
    assert_eq!(
        after.properties.as_ref().unwrap()["extension"],
        json!({"retained": true})
    );
    assert_eq!(after.entity_type.as_deref(), Some("tool"));
    assert_eq!(
        after.description.as_deref(),
        Some("replacement searchable nebula")
    );
    assert_eq!(after.properties.as_ref().unwrap()["schema"], replacement);
    assert_eq!(after.properties.as_ref().unwrap()["source"], "mcp:example");
    assert_eq!(after.properties.as_ref().unwrap()["side_effect"], "egress");
    assert_eq!(after.properties.as_ref().unwrap()["trust"], "marketplace");
    assert_eq!(after.tags, ["tool-registry", "tool", "extra"]);
    assert_eq!(after.version, before.version + 1);
    assert!(after.updated_at > before.updated_at);
    assert_ne!(
        RegistryPin::from_entity(&before).unwrap(),
        RegistryPin::from_entity(&after).unwrap()
    );
    let found = f
        .call(
            "tool.suggest",
            json!({"query": "replacement searchable nebula"}),
        )
        .await;
    assert!(
        found["results"]
            .as_array()
            .unwrap()
            .iter()
            .any(|hit| hit["full_id"] == *id),
        "{found}"
    );

    let emptied = f
        .call("tool.register", json!({"name": "echo", "schema": {}}))
        .await;
    assert_outcome(&emptied, false, true, false);
    assert_eq!(f.describe("echo").await["schema"], json!({}));
}

#[tokio::test]
async fn register_equal_and_omitted_fields_preserve_the_stored_row() {
    let f = Fixture::new();
    let definition = json!({
        "name": "unchanged", "kind": "plugin", "description": "a stable plugin",
        "source": "mcp:stable", "side_effect": "egress", "trust": "first_party",
        "schema": {"type": "object", "properties": {"a": {"type": "integer"}, "b": {"type": "string"}}},
        "tags": ["retained"], "capabilities": ["stable capability"]
    });
    let first = f.call("tool.register", definition.clone()).await;
    let before = f.row(&first["tool"]["full_id"]).await;
    for params in [
        json!({"name": "UNCHANGED"}),
        json!({"name": "unchanged", "kind": null, "description": null, "source": null,
            "schema": null, "side_effect": null, "trust": null, "tags": null, "capabilities": null}),
        definition,
        json!({"name": "unchanged", "schema": {"properties": {"b": {"type": "string"}, "a": {"type": "integer"}}, "type": "object"}, "tags": [], "capabilities": []}),
    ] {
        let response = f.call("tool.register", params).await;
        assert_outcome(&response, false, false, true);
        assert_eq!(
            serde_json::to_value(f.row(&first["tool"]["full_id"]).await).unwrap(),
            serde_json::to_value(&before).unwrap()
        );
    }
    assert_eq!(
        f.describe("unchanged").await["capabilities"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let tags = f
        .call(
            "tool.register",
            json!({"name": "unchanged", "tags": ["added", "retained"]}),
        )
        .await;
    assert_outcome(&tags, false, true, false);
    assert_eq!(
        tags["tool"]["tags"],
        json!(["tool-registry", "plugin", "retained", "added"])
    );
    let after_tags = f.row(&first["tool"]["full_id"]).await;
    assert_eq!(
        RegistryPin::from_entity(&after_tags).unwrap(),
        RegistryPin::from_entity(&before).unwrap()
    );
    let linked = f
        .call(
            "tool.register",
            json!({"name": "unchanged", "capabilities": ["second capability"]}),
        )
        .await;
    assert_outcome(&linked, false, false, true);
    assert_eq!(
        f.describe("unchanged").await["capabilities"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        serde_json::to_value(f.row(&first["tool"]["full_id"]).await).unwrap(),
        serde_json::to_value(after_tags).unwrap()
    );
}

#[tokio::test]
async fn register_updates_only_its_own_named_registration_and_leaves_ingest_unchanged() {
    let f = Fixture::new();
    let a = f
        .call(
            "tool.register",
            json!({"name": "shared", "namespace": "a", "source": "mcp:a", "side_effect": "read"}),
        )
        .await;
    let b = f.call("tool.register", json!({"name": "SHARED", "namespace": "b", "source": "mcp:b", "capabilities": ["b capability"]})).await;
    let foreign = f.row(&b["tool"]["full_id"]).await;
    let foreign_description = f
        .call(
            "tool.describe",
            json!({"tool": b["tool"]["full_id"], "namespace": "b"}),
        )
        .await["tool"]
        .clone();
    assert_eq!(
        foreign_description["capabilities"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        foreign_description["capabilities"][0]["name"],
        "b capability"
    );
    let changed = f
        .call(
            "tool.register",
            json!({"name": "shared", "namespace": "a", "schema": {"type": "object"}}),
        )
        .await;
    assert_outcome(&changed, false, true, false);
    assert_eq!(changed["tool"]["full_id"], a["tool"]["full_id"]);
    assert_ne!(changed["tool"]["full_id"], b["tool"]["full_id"]);
    assert_eq!(
        serde_json::to_value(f.row(&b["tool"]["full_id"]).await).unwrap(),
        serde_json::to_value(foreign).unwrap()
    );
    assert_eq!(
        f.call(
            "tool.describe",
            json!({"tool": b["tool"]["full_id"], "namespace": "b"})
        )
        .await["tool"]["capabilities"],
        foreign_description["capabilities"]
    );
    let before = f.row(&a["tool"]["full_id"]).await;
    let ingest = f.call("tool.ingest", json!({"namespace": "a", "source": "mcp", "server": "a", "tools": [{"name": "shared", "description": "not an update", "inputSchema": {"different": true}, "side_effect": "read"}]})).await;
    assert_eq!(ingest["registered"], 0);
    assert_eq!(ingest["existing"], 1);
    assert_eq!(
        serde_json::to_value(f.row(&a["tool"]["full_id"]).await).unwrap(),
        serde_json::to_value(before).unwrap()
    );
}

#[tokio::test]
async fn refused_registration_updates_preserve_rows_and_capabilities() {
    let f = Fixture::new();
    let first = f
        .call(
            "tool.register",
            json!({"name": "protected", "source": "mcp:original", "capabilities": ["retained"]}),
        )
        .await;
    let before = f.row(&first["tool"]["full_id"]).await;
    let described = f.describe("protected").await;
    for patch in [
        json!({"kind": "unknown"}),
        json!({"side_effect": "unknown"}),
        json!({"trust": "unknown"}),
        json!({"schema": []}),
        json!({"description": 42}),
        json!({"schema": {"credential": format!("sk-proj-{}", "A".repeat(80))}}),
    ] {
        let mut params = patch;
        params["name"] = json!("protected");
        params["capabilities"] = json!(["must not link"]);
        f.registry
            .dispatch("tool.register", params)
            .await
            .expect_err("invalid update refuses");
        assert_eq!(
            serde_json::to_value(f.row(&first["tool"]["full_id"]).await).unwrap(),
            serde_json::to_value(&before).unwrap()
        );
        assert_eq!(
            f.describe("protected").await["capabilities"],
            described["capabilities"]
        );
    }
    let token = f.runtime.authorize(Namespace::local()).unwrap();
    assert_eq!(
        f.runtime
            .count_entities_tagged(&token, Some("concept"), Some("tool-capability"))
            .await
            .unwrap(),
        1
    );
    assert_eq!(f.call("tool.requests", json!({})).await["count"], 0);
}

#[tokio::test]
async fn existing_legacy_registration_keeps_its_minted_id_when_updated() {
    let f = Fixture::new();
    let token = f.runtime.authorize(Namespace::local()).unwrap();
    let legacy = Entity::new("local", "project", "Legacy")
        .with_entity_type(Some("tool"))
        .with_tags(vec!["tool-registry".into(), "tool".into()])
        .with_properties(json!({"source": "mcp:legacy", "registered_at": "legacy timestamp"}));
    let id: Uuid = legacy.id;
    f.runtime
        .entities(&token)
        .unwrap()
        .upsert_entity(legacy)
        .await
        .unwrap();
    let response = f
        .call("tool.register", json!({"name": "LEGACY", "schema": {}}))
        .await;
    assert_outcome(&response, false, true, false);
    assert_eq!(response["tool"]["full_id"], id.to_string());
    assert_eq!(response["tool"]["name"], "Legacy");
    assert_eq!(
        f.describe("legacy").await["properties"]["registered_at"],
        "legacy timestamp"
    );
    assert_eq!(f.call("tool.list", json!({})).await["count"], 1);
}

fn registration_identity(actor: &str) -> khive_runtime::RequestIdentity {
    khive_runtime::RequestIdentity {
        namespace: "local".into(),
        actor_id: Some(actor.into()),
        visible_namespaces: Vec::new(),
        owned_namespaces: Vec::new(),
        process_ref: None,
        request_id: None,
    }
}

impl Fixture {
    async fn register_as(&self, actor: &str, params: Value) -> Value {
        self.registry
            .dispatch_with_identity("tool.register", params, Some(registration_identity(actor)))
            .await
            .unwrap()
    }

    async fn registration_events(&self, id: Uuid) -> Vec<khive_storage::Event> {
        let token = self.runtime.authorize(Namespace::local()).unwrap();
        let store = self.runtime.events(&token).unwrap();
        let filter = khive_storage::EventFilter {
            target_id: Some(id),
            kinds: vec![khive_types::EventKind::ToolRegistrationChanged],
            ..Default::default()
        };
        let page = store
            .query_events(
                filter.clone(),
                khive_storage::PageRequest {
                    limit: 100,
                    offset: 0,
                },
            )
            .await
            .unwrap();
        assert_eq!(
            store.count_events(filter).await.unwrap(),
            page.items.len() as u64
        );
        let mut events = page.items;
        events.sort_by_key(|event| event.id);
        events
    }

    async fn registration_state(&self) -> Value {
        let mut reader = self.runtime.sql().reader().await.unwrap();
        let mut state = serde_json::Map::new();
        for table in [
            "entities",
            "graph_edges",
            "events",
            "tool_policy",
            "tool_grants",
        ] {
            let rows = reader
                .query_all(khive_storage::SqlStatement::new(
                    format!("SELECT * FROM {table} ORDER BY id"),
                    Vec::new(),
                ))
                .await
                .unwrap();
            state.insert(table.into(), serde_json::to_value(rows).unwrap());
        }
        Value::Object(state)
    }
}

#[tokio::test]
async fn register_digest_input_change_appends_one_registration_changed_event() {
    let f = Fixture::new();
    let first = f
        .register_as(
            "registrar-a",
            json!({
                "name": "eventful", "source": "mcp:original", "side_effect": "read",
                "trust": "first_party", "schema": {"type": "object"}
            }),
        )
        .await;
    let id = &first["tool"]["full_id"];
    let original = f.row(id).await;
    assert!(f.registration_events(original.id).await.is_empty());
    let token = f.runtime.authorize(Namespace::local()).unwrap();
    let created = f
        .runtime
        .events(&token)
        .unwrap()
        .query_events(
            khive_storage::EventFilter {
                target_id: Some(original.id),
                kinds: vec![khive_types::EventKind::EntityCreated],
                ..Default::default()
            },
            khive_storage::PageRequest {
                limit: 10,
                offset: 0,
            },
        )
        .await
        .unwrap();
    assert_eq!(created.items.len(), 1);
    assert_eq!(created.items[0].actor, "actor:registrar-a");

    for fields in [
        json!({"side_effect": "egress"}),
        json!({"trust": "marketplace"}),
        json!({"schema": {"type": "object", "properties": {"query": {"type": "string"}}}}),
        json!({"side_effect": "irreversible", "trust": "external", "schema": {}}),
    ] {
        let before = f.row(id).await;
        let prior = f.registration_events(original.id).await;
        let mut params = fields;
        params["name"] = json!("eventful");
        let response = f.register_as("registrar-b", params).await;
        assert_outcome(&response, false, true, false);
        let after = f.row(id).await;
        assert_eq!(after.id, original.id);
        assert_eq!(after.created_at, original.created_at);
        let events = f.registration_events(original.id).await;
        assert_eq!(
            events.len(),
            prior.len() + 1,
            "one event per corrected registration"
        );
        let added: Vec<_> = events
            .iter()
            .filter(|event| !prior.iter().any(|old| old.id == event.id))
            .collect();
        assert_eq!(added.len(), 1);
        let event = added[0];
        let before_pin = RegistryPin::from_entity(&before).unwrap();
        let after_pin = RegistryPin::from_entity(&after).unwrap();
        assert_ne!(
            before_pin.definition_digest(),
            after_pin.definition_digest()
        );
        assert_eq!(event.namespace, "local");
        assert_eq!(event.actor, "actor:registrar-b");
        assert_ne!(event.actor, created.items[0].actor);
        assert_eq!(event.verb, "tool.register");
        assert_eq!(event.substrate, khive_types::SubstrateKind::Event);
        assert_eq!(event.outcome, khive_types::EventOutcome::Success);
        assert_eq!(event.target_id, Some(original.id));
        assert_eq!(
            event.payload,
            json!({
                "actor": event.actor, "id": original.id.to_string(),
                "digest_before": before_pin.definition_digest(),
                "digest_after": after_pin.definition_digest(),
            })
        );
    }

    let events = f.registration_events(original.id).await;
    for fields in [
        json!({"description": "metadata-only searchable description"}),
        json!({"kind": "tool", "tags": ["metadata-tag"]}),
        json!({"capabilities": ["new-capability"]}),
        json!({"source": "mcp:original", "side_effect": "irreversible", "trust": "external", "schema": {}}),
        json!({"source": null, "side_effect": null, "trust": null, "schema": null}),
        json!({}),
    ] {
        let mut params = fields;
        params["name"] = json!("eventful");
        f.register_as("registrar-b", params).await;
        assert_eq!(f.registration_events(original.id).await, events);
    }
    let final_row = f.row(id).await;
    assert_eq!(
        final_row.description.as_deref(),
        Some("metadata-only searchable description")
    );
    assert!(final_row.tags.iter().any(|tag| tag == "metadata-tag"));
    assert_eq!(
        f.describe("eventful").await["capabilities"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let listed = f
        .call(
            "list",
            json!({
                "kind": "event", "event_kinds": ["tool_registration_changed"],
                "verb": "tool.register", "limit": 100
            }),
        )
        .await;
    assert_eq!(listed["has_more"], false);
    let mut listed_ids: Vec<_> = listed["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["id"].as_str().unwrap().parse::<Uuid>().unwrap())
        .collect();
    listed_ids.sort();
    assert_eq!(
        listed_ids,
        events.iter().map(|event| event.id).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn registration_change_rejects_credential_actor_before_writing() {
    let f = Fixture::new();
    let first = f
        .register_as(
            "safe",
            json!({"name": "credential-actor", "source": "mcp:original"}),
        )
        .await;
    let id = &first["tool"]["full_id"];
    let original = f.row(id).await;
    let before = f.registration_state().await;
    let secret = format!("{}{}", ["gh", "p_"].concat(), "A".repeat(36));
    assert!(khive_runtime::secret_gate::check_at(
        &format!("actor:{secret}"),
        "tool.register",
        "actor"
    )
    .is_err());
    let error = f.registry.dispatch_with_identity(
        "tool.register", json!({"name": "credential-actor", "schema": {"marker": "accepted"}, "capabilities": ["must-not-link"]}),
        Some(registration_identity(&secret)),
    ).await.expect_err("credential-bearing actor must refuse before correction");
    assert!(
        matches!(error, khive_runtime::RuntimeError::SecretDetected(_)),
        "{error}"
    );
    assert!(!error.to_string().contains(&secret));
    assert_eq!(f.registration_state().await, before);
    assert_eq!(
        serde_json::to_value(f.row(id).await).unwrap(),
        serde_json::to_value(&original).unwrap()
    );
    f.register_as(
        "safe",
        json!({"name": "credential-actor", "schema": {"marker": "accepted"}}),
    )
    .await;
    assert_eq!(f.registration_events(original.id).await.len(), 1);
}

#[tokio::test]
async fn registration_change_event_failure_rolls_back_and_retry_appends_once() {
    let f = Fixture::new();
    let first = f
        .register_as(
            "registrar",
            json!({
                "name": "rollback", "source": "mcp:original", "capabilities": ["retained"]
            }),
        )
        .await;
    let id = &first["tool"]["full_id"];
    let original = f.row(id).await;
    f.call(
        "tool.policy",
        json!({"actor": "agent:requester", "tool": "rollback", "decision": "deny"}),
    )
    .await;
    f.call(
        "tool.request",
        json!({"actor": "agent:requester", "tool": "rollback"}),
    )
    .await;
    let before = f.registration_state().await;
    for table in [
        "entities",
        "graph_edges",
        "events",
        "tool_policy",
        "tool_grants",
    ] {
        assert!(
            !before[table].as_array().unwrap().is_empty(),
            "populated {table}"
        );
    }
    let trigger = format!(
        "CREATE TRIGGER fail_registration_change BEFORE INSERT ON events
         WHEN NEW.kind = 'tool_registration_changed' AND NEW.target_id = '{}'
         BEGIN
           SELECT CASE WHEN (SELECT json_extract(properties, '$.schema.marker') FROM entities WHERE id = '{}') = 'replacement'
             THEN RAISE(ABORT, 'registration_event_after_replacement')
             ELSE RAISE(ABORT, 'registration_event_before_replacement') END;
         END;", original.id, original.id
    );
    f.runtime
        .backend()
        .pool_arc()
        .writer()
        .unwrap()
        .autocommit_sqlite(|connection| connection.execute_batch(&trigger))
        .unwrap();
    let refused = f
        .registry
        .dispatch_with_identity(
            "tool.register",
            json!({
                "name": "rollback", "schema": {"marker": "replacement"}, "capabilities": ["must-not-link"]
            }),
            Some(registration_identity("registrar")),
        )
        .await;
    f.runtime
        .backend()
        .pool_arc()
        .writer()
        .unwrap()
        .autocommit_sqlite(|connection| {
            connection.execute_batch("DROP TRIGGER fail_registration_change")
        })
        .unwrap();
    let error = refused.expect_err("required event failure must refuse the replacement");
    assert!(
        error
            .to_string()
            .contains("registration_event_after_replacement"),
        "{error}"
    );
    assert_eq!(f.registration_state().await, before);
    assert_eq!(
        serde_json::to_value(f.row(id).await).unwrap(),
        serde_json::to_value(&original).unwrap()
    );
    assert!(f.registration_events(original.id).await.is_empty());
    let retry = f
        .register_as(
            "registrar",
            json!({"name": "rollback", "schema": {"marker": "replacement"}}),
        )
        .await;
    assert_outcome(&retry, false, true, false);
    assert_eq!(
        f.row(id).await.properties.unwrap()["schema"],
        json!({"marker": "replacement"})
    );
    assert_eq!(f.registration_events(original.id).await.len(), 1);
}

#[tokio::test]
async fn required_registration_event_uses_token_attribution_and_skips_noop() {
    let f = Fixture::new();
    let first = f
        .call(
            "tool.register",
            json!({"name": "trusted-boundary", "source": "mcp:old"}),
        )
        .await;
    let id = &first["tool"]["full_id"];
    let original = f.row(id).await;
    let token = f.runtime.authorize(Namespace::local()).unwrap();
    let actor = format!("{}:{}", token.actor().kind, token.actor().id);
    let mut changed = original.clone();
    changed.properties.as_mut().unwrap()["source"] = json!("mcp:new");
    let payload = json!({
        "actor": actor, "id": original.id.to_string(),
        "digest_before": RegistryPin::from_entity(&original).unwrap().definition_digest(),
        "digest_after": RegistryPin::from_entity(&changed).unwrap().definition_digest(),
    });
    let event = khive_storage::Event::new(
        "foreign",
        "tool.register",
        khive_types::EventKind::ToolRegistrationChanged,
        khive_types::SubstrateKind::Event,
        "forged:actor",
    )
    .with_target(original.id)
    .with_payload(payload.clone());
    let updated = f
        .runtime
        .update_entity_if_unchanged_with_required_event(
            &token,
            &original,
            khive_runtime::EntityPatch {
                properties: Some(json!({"source": "mcp:new"})),
                ..Default::default()
            },
            &[],
            event.clone(),
        )
        .await
        .unwrap();
    let events = f.registration_events(original.id).await;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].namespace, "local");
    assert_eq!(events[0].actor, actor);
    assert_eq!(events[0].payload, payload);
    let stale_event = khive_storage::Event::new(
        "foreign",
        "tool.register",
        khive_types::EventKind::ToolRegistrationChanged,
        khive_types::SubstrateKind::Event,
        "forged:actor",
    )
    .with_target(original.id)
    .with_payload(payload);
    f.runtime
        .update_entity_if_unchanged_with_required_event(
            &token,
            &original,
            khive_runtime::EntityPatch {
                properties: Some(json!({"source": "mcp:stale"})),
                ..Default::default()
            },
            &[],
            stale_event,
        )
        .await
        .expect_err("stale replacement cannot append its event");
    assert_eq!(f.registration_events(original.id).await, events);
    assert_eq!(f.row(id).await.properties, updated.properties);
    assert_eq!(
        serde_json::to_value(f.row(id).await).unwrap(),
        serde_json::to_value(&updated).unwrap()
    );
    let unchanged = f
        .runtime
        .update_entity_if_unchanged_with_required_event(
            &token,
            &updated,
            khive_runtime::EntityPatch::default(),
            &[],
            event,
        )
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(&unchanged).unwrap(),
        serde_json::to_value(&updated).unwrap()
    );
    assert_eq!(f.registration_events(original.id).await, events);
}

impl Fixture {
    async fn populate_registration_authorization(&self, name: &str) {
        let default = self
            .call(
                "tool.check",
                json!({"actor": "agent:default", "tool": name}),
            )
            .await;
        assert_eq!(default["source"], "default");
        self.call(
            "tool.policy",
            json!({"actor": "agent:policy", "tool": name, "decision": "allow"}),
        )
        .await;
        let policy = self
            .call("tool.check", json!({"actor": "agent:policy", "tool": name}))
            .await;
        assert_eq!(policy["source"], "policy");
        assert_eq!(policy["decision"], "allow");
        self.call(
            "tool.policy",
            json!({"actor": "agent:requester", "tool": name, "decision": "deny"}),
        )
        .await;
        let request = self
            .call(
                "tool.request",
                json!({"actor": "agent:requester", "tool": name}),
            )
            .await;
        self.call("tool.grant", json!({"id": request["request_id"]}))
            .await;
        let check = self
            .call(
                "tool.check",
                json!({"actor": "agent:requester", "tool": name}),
            )
            .await;
        assert_eq!(check["source"], "grant");
        for table in [
            "entities",
            "graph_edges",
            "events",
            "tool_policy",
            "tool_grants",
        ] {
            assert!(
                !self.registration_state().await[table]
                    .as_array()
                    .unwrap()
                    .is_empty(),
                "populated {table}"
            );
        }
    }
}

#[tokio::test]
async fn register_correction_refuses_source_or_kind_change_on_existing_rows() {
    let program = json!({"version": 1, "fixed_args": ["--version"], "extra_args": "none"});
    let digest = "a".repeat(64);
    for (source, declarations) in [
        ("exec:/fixture/original", json!({})),
        ("mcp:original", json!({})),
        ("exec:/fixture/original", json!({"network": "outbound"})),
        (
            "exec:/fixture/original",
            json!({"executable_digest": digest}),
        ),
        ("exec:/fixture/original", json!({"program": program})),
        (
            "exec:/fixture/original",
            json!({"network": "outbound", "executable_digest": digest}),
        ),
        (
            "exec:/fixture/original",
            json!({"program": program, "executable_digest": digest}),
        ),
    ] {
        let f = Fixture::new();
        let first = f
            .register_as(
                "original-registrar",
                json!({
                    "name": "frozen", "source": source, "kind": "tool",
                    "side_effect": "read", "trust": "first_party", "capabilities": ["retained"]
                }),
            )
            .await;
        let id = &first["tool"]["full_id"];
        let token = f.runtime.authorize(Namespace::local()).unwrap();
        let mut seeded = f.row(id).await;
        for (key, value) in declarations.as_object().unwrap() {
            seeded.properties.as_mut().unwrap()[key] = value.clone();
        }
        f.runtime
            .entities(&token)
            .unwrap()
            .upsert_entity(seeded)
            .await
            .unwrap();
        let stored = f.row(id).await;
        for (key, value) in declarations.as_object().unwrap() {
            assert_eq!(&stored.properties.as_ref().unwrap()[key], value);
        }
        let default = f
            .call(
                "tool.check",
                json!({"actor": "agent:default", "tool": "frozen"}),
            )
            .await;
        assert_eq!(default["decision"], "allow");
        assert_eq!(default["source"], "default");

        // No policy or grant is needed for an established name to require protection.
        let unreferenced = f.registration_state().await;
        assert!(unreferenced["tool_policy"].as_array().unwrap().is_empty());
        assert!(unreferenced["tool_grants"].as_array().unwrap().is_empty());
        let error = f.registry.dispatch_with_identity(
            "tool.register", json!({"name": "frozen", "source": "exec:/fixture/other", "capabilities": ["must-not-link"]}),
            Some(registration_identity("different-registrar")),
        ).await.expect_err("source changes refuse without policy or grant references");
        assert!(
            matches!(error, khive_runtime::RuntimeError::InvalidInput(_)),
            "{error}"
        );
        assert!(error.to_string().contains("source"), "{error}");
        assert!(error.to_string().contains("new name"), "{error}");
        assert_eq!(f.registration_state().await, unreferenced);

        f.populate_registration_authorization("frozen").await;
        let before_replays = f.registration_state().await;
        for fields in [
            json!({"source": source, "kind": "tool"}),
            json!({"source": null, "kind": null}),
            json!({}),
        ] {
            let mut params = fields;
            params["name"] = json!("frozen");
            let equal = f.register_as("different-registrar", params).await;
            assert_outcome(&equal, false, false, true);
            assert_eq!(f.registration_state().await, before_replays);
        }
        let metadata = f
            .register_as(
                "different-registrar",
                json!({
                    "name": "frozen", "description": "metadata remains correctable"
                }),
            )
            .await;
        assert_outcome(&metadata, false, true, false);
        let original = serde_json::to_value(f.row(id).await).unwrap();
        let capabilities = f.describe("frozen").await["capabilities"].clone();
        assert_eq!(capabilities.as_array().unwrap().len(), 1);
        assert!(f.registration_events(stored.id).await.is_empty());
        for actor in ["original-registrar", "different-registrar"] {
            for (fields, changed) in [
                (json!({"source": "mcp:replacement"}), "source"),
                (json!({"source": "exec:/fixture/other"}), "source"),
                (json!({"kind": "skill"}), "kind"),
                (
                    json!({"source": "exec:/fixture/other", "kind": "plugin"}),
                    "source and kind",
                ),
            ] {
                // A preceding tool.check may append its own decision receipt.
                let before = f.registration_state().await;
                let mut params = fields;
                params["name"] = json!("frozen");
                params["description"] = json!("must not replace metadata");
                params["schema"] = json!({"must_not_replace": true});
                params["tags"] = json!(["must-not-add"]);
                params["capabilities"] = json!(["must-not-link"]);
                let error = f
                    .registry
                    .dispatch_with_identity(
                        "tool.register",
                        params,
                        Some(registration_identity(actor)),
                    )
                    .await
                    .expect_err("all existing rows keep their source and kind");
                let khive_runtime::RuntimeError::InvalidInput(reason) = error else {
                    panic!("expected InvalidInput, got {error:?}");
                };
                assert_eq!(reason, format!(
                    "cannot change {changed} on an existing registration; register a different source or kind under a new name"
                ));
                assert_eq!(serde_json::to_value(f.row(id).await).unwrap(), original);
                assert_eq!(f.registration_state().await, before);
                assert_eq!(f.describe("frozen").await["capabilities"], capabilities);
                assert!(f.registration_events(stored.id).await.is_empty());
                let check = f
                    .call(
                        "tool.check",
                        json!({"actor": "agent:requester", "tool": "frozen"}),
                    )
                    .await;
                assert_eq!(check["source"], "grant");
                assert_eq!(check["decision"], "allow");
            }
        }
    }
}

#[tokio::test]
async fn register_correction_refuses_loosening() {
    // Literal contract order keeps the oracle independent of the production arrays.
    for (field, order) in [
        (
            "side_effect",
            &["read", "write", "egress", "irreversible"][..],
        ),
        ("trust", &["first_party", "marketplace", "external"][..]),
    ] {
        for (stored_index, stored) in order.iter().enumerate().skip(1) {
            for passed in &order[..stored_index] {
                let f = Fixture::new();
                let mut definition = json!({
                    "name": "monotonic", "source": "mcp:original",
                    "side_effect": "read", "trust": "first_party", "capabilities": ["retained"]
                });
                definition[field] = json!(stored);
                let first = f.register_as("original-registrar", definition).await;
                let id = &first["tool"]["full_id"];
                let original = f.row(id).await;
                f.populate_registration_authorization("monotonic").await;
                let before = f.registration_state().await;
                assert!(f.registration_events(original.id).await.is_empty());
                let mut params = json!({"name": "monotonic", "capabilities": ["must-not-link"]});
                params[field] = json!(passed);
                let error = f
                    .registry
                    .dispatch_with_identity(
                        "tool.register",
                        params,
                        Some(registration_identity("different-registrar")),
                    )
                    .await
                    .expect_err("another actor cannot loosen the stored registration");
                let khive_runtime::RuntimeError::InvalidInput(reason) = error else {
                    panic!("expected InvalidInput, got {error:?}");
                };
                assert_eq!(
                    reason,
                    format!("cannot loosen {field}: stored {stored:?}, passed {passed:?}")
                );
                assert_eq!(
                    serde_json::to_value(f.row(id).await).unwrap(),
                    serde_json::to_value(&original).unwrap()
                );
                assert_eq!(f.registration_state().await, before);
                assert!(f.registration_events(original.id).await.is_empty());
            }
        }

        let f = Fixture::new();
        let first = f.register_as("original-registrar", json!({
            "name": "tightening", "source": "mcp:original", "side_effect": "read", "trust": "first_party"
        })).await;
        let id = &first["tool"]["full_id"];
        let before = f.row(id).await;
        let mut params = json!({"name": "tightening"});
        params[field] = json!(order.last().unwrap());
        let response = f.register_as("different-registrar", params).await;
        assert_outcome(&response, false, true, false);
        let after = f.row(id).await;
        assert_eq!(
            after.properties.as_ref().unwrap()[field],
            json!(order.last().unwrap())
        );
        let events = f.registration_events(before.id).await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].actor, "actor:different-registrar");
        assert_eq!(
            events[0].payload,
            json!({
                "actor": "actor:different-registrar", "id": before.id.to_string(),
                "digest_before": RegistryPin::from_entity(&before).unwrap().definition_digest(),
                "digest_after": RegistryPin::from_entity(&after).unwrap().definition_digest(),
            })
        );
    }
}

#[tokio::test]
async fn register_correction_applies_conservative_unknown_policy_floors() {
    // Legacy values are deliberately seeded through the trusted store, not public registration.
    let legacy_states = [
        ("absent", None),
        ("null", Some(Value::Null)),
        ("non-string", Some(json!(true))),
        ("non-string", Some(json!(17))),
        ("non-string", Some(json!(["legacy-value-do-not-echo"]))),
        (
            "non-string",
            Some(json!({"legacy": "legacy-value-do-not-echo"})),
        ),
        ("unrecognized", Some(json!("legacy-value-do-not-echo"))),
    ];
    for (field, refused, admitted, remedy) in [
        (
            "side_effect",
            &["read"][..],
            &["write", "egress", "irreversible"][..],
            "use write, egress or irreversible",
        ),
        (
            "trust",
            &["first_party", "marketplace"][..],
            &["external"][..],
            "use external",
        ),
    ] {
        for (classification, legacy) in &legacy_states {
            let f = Fixture::new();
            let first = f
                .register_as(
                    "original-registrar",
                    json!({
                        "name": "legacy-policy", "source": "mcp:original", "kind": "tool",
                        "side_effect": "read", "trust": "first_party", "capabilities": ["retained"]
                    }),
                )
                .await;
            let id = &first["tool"]["full_id"];
            let token = f.runtime.authorize(Namespace::local()).unwrap();
            let mut seeded = f.row(id).await;
            let properties = seeded.properties.as_mut().unwrap().as_object_mut().unwrap();
            if let Some(value) = legacy {
                properties.insert(field.into(), value.clone());
            } else {
                properties.remove(field);
            }
            f.runtime
                .entities(&token)
                .unwrap()
                .upsert_entity(seeded)
                .await
                .unwrap();
            let stored = f.row(id).await;
            assert_eq!(
                stored.properties.as_ref().unwrap().get(field),
                legacy.as_ref()
            );
            f.populate_registration_authorization("legacy-policy").await;
            let before_preservation = f.registration_state().await;
            let mut explicit_null = json!({});
            explicit_null[field] = Value::Null;
            for mut params in [json!({}), explicit_null] {
                params["name"] = json!("legacy-policy");
                let replay = f.register_as("different-registrar", params).await;
                assert_outcome(&replay, false, false, true);
                assert_eq!(f.registration_state().await, before_preservation);
            }
            let metadata = f.register_as("different-registrar", json!({
                "name": "legacy-policy", "description": "unrelated metadata remains correctable"
            })).await;
            assert_outcome(&metadata, false, true, false);
            let after_metadata = f.row(id).await;
            assert_eq!(after_metadata.properties, stored.properties);
            assert_eq!(
                after_metadata.description.as_deref(),
                Some("unrelated metadata remains correctable")
            );
            assert!(f.registration_events(stored.id).await.is_empty());
            assert_eq!(
                f.describe("legacy-policy").await["properties"],
                stored.properties.as_ref().unwrap().clone()
            );

            let before_refusal = f.registration_state().await;
            let row_before_refusal = serde_json::to_value(&after_metadata).unwrap();
            let capabilities = f.describe("legacy-policy").await["capabilities"].clone();
            assert_eq!(capabilities.as_array().unwrap().len(), 1);
            for passed in refused {
                let mut params = json!({
                    "name": "legacy-policy", "description": "must-not-change", "schema": {"must_not_change": true},
                    "tags": ["must-not-add"], "capabilities": ["must-not-link"]
                });
                params[field] = json!(passed);
                let error = f
                    .registry
                    .dispatch_with_identity(
                        "tool.register",
                        params,
                        Some(registration_identity("different-registrar")),
                    )
                    .await
                    .expect_err("unknown stored policy values have conservative floors");
                let khive_runtime::RuntimeError::InvalidInput(reason) = error else {
                    panic!("expected InvalidInput, got {error:?}");
                };
                assert_eq!(
                    reason,
                    format!(
                    "cannot set {field} to {passed:?}: stored {field} is {classification}; {remedy}"
                )
                );
                assert!(!reason.contains("legacy-value-do-not-echo"), "{reason}");
                assert_eq!(
                    serde_json::to_value(f.row(id).await).unwrap(),
                    row_before_refusal
                );
                assert_eq!(f.registration_state().await, before_refusal);
                assert_eq!(
                    f.describe("legacy-policy").await["capabilities"],
                    capabilities
                );
                assert!(f.registration_events(stored.id).await.is_empty());
            }

            for passed in admitted {
                // Each admitted value starts from the same legacy class, not a prior tightening.
                let mut seeded = f.row(id).await;
                let properties = seeded.properties.as_mut().unwrap().as_object_mut().unwrap();
                if let Some(value) = legacy {
                    properties.insert(field.into(), value.clone());
                } else {
                    properties.remove(field);
                }
                f.runtime
                    .entities(&token)
                    .unwrap()
                    .upsert_entity(seeded)
                    .await
                    .unwrap();
                let before = f.row(id).await;
                assert_eq!(
                    before.properties.as_ref().unwrap().get(field),
                    legacy.as_ref()
                );
                let prior_events = f.registration_events(stored.id).await;
                let prior_state = f.registration_state().await;
                let check = f
                    .call(
                        "tool.check",
                        json!({"actor": "agent:requester", "tool": "legacy-policy"}),
                    )
                    .await;
                assert_eq!(check["source"], "grant");
                let mut params = json!({"name": "legacy-policy"});
                params[field] = json!(passed);
                let response = f.register_as("different-registrar", params).await;
                assert_outcome(&response, false, true, false);
                let after = f.row(id).await;
                let mut expected = before.clone();
                expected.properties.as_mut().unwrap()[field] = json!(passed);
                expected.version += 1;
                expected.updated_at = after.updated_at;
                assert!(after.updated_at > before.updated_at);
                assert_eq!(
                    serde_json::to_value(&after).unwrap(),
                    serde_json::to_value(expected).unwrap()
                );
                let described = f.describe("legacy-policy").await;
                assert_eq!(described[field], json!(passed));
                assert_eq!(
                    described["properties"],
                    after.properties.as_ref().unwrap().clone()
                );
                assert_eq!(described["capabilities"], capabilities);
                let after_state = f.registration_state().await;
                for table in ["graph_edges", "tool_policy", "tool_grants"] {
                    assert_eq!(after_state[table], prior_state[table], "{table}");
                }
                let events = f.registration_events(stored.id).await;
                assert_eq!(events.len(), prior_events.len() + 1);
                for prior in &prior_events {
                    assert!(events.contains(prior));
                }
                let added: Vec<_> = events
                    .iter()
                    .filter(|event| !prior_events.iter().any(|prior| prior.id == event.id))
                    .collect();
                assert_eq!(added.len(), 1);
                let event = added[0];
                let before_pin = RegistryPin::from_entity(&before).unwrap();
                let after_pin = RegistryPin::from_entity(&after).unwrap();
                assert_ne!(
                    before_pin.definition_digest(),
                    after_pin.definition_digest()
                );
                assert_eq!(event.kind, khive_types::EventKind::ToolRegistrationChanged);
                assert_eq!(event.namespace, "local");
                assert_eq!(event.verb, "tool.register");
                assert_eq!(event.actor, "actor:different-registrar");
                assert_eq!(event.substrate, khive_types::SubstrateKind::Event);
                assert_eq!(event.outcome, khive_types::EventOutcome::Success);
                assert_eq!(event.target_id, Some(stored.id));
                assert_eq!(
                    event.payload,
                    json!({
                        "actor": event.actor, "id": stored.id.to_string(),
                        "digest_before": before_pin.definition_digest(), "digest_after": after_pin.definition_digest(),
                    })
                );
                let check = f
                    .call(
                        "tool.check",
                        json!({"actor": "agent:requester", "tool": "legacy-policy"}),
                    )
                    .await;
                assert_eq!(check["source"], "policy");
                assert_eq!(check["decision"], "deny");
            }
        }
    }
}
