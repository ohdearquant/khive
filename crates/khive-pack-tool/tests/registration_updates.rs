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
            json!({"name": "Echo", "source": "mcp:example"}),
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
                "name": "ECHO", "kind": "skill", "description": "replacement searchable nebula",
                "schema": replacement, "source": "mcp:replacement", "side_effect": "egress",
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
    assert_eq!(after.entity_type.as_deref(), Some("skill"));
    assert_eq!(
        after.description.as_deref(),
        Some("replacement searchable nebula")
    );
    assert_eq!(after.properties.as_ref().unwrap()["schema"], replacement);
    assert_eq!(
        after.properties.as_ref().unwrap()["source"],
        "mcp:replacement"
    );
    assert_eq!(after.properties.as_ref().unwrap()["side_effect"], "egress");
    assert_eq!(after.properties.as_ref().unwrap()["trust"], "marketplace");
    assert_eq!(after.tags, ["tool-registry", "extra", "skill"]);
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
            json!({"name": "shared", "namespace": "a", "source": "mcp:a"}),
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
    let ingest = f.call("tool.ingest", json!({"namespace": "a", "source": "mcp", "server": "ignored", "tools": [{"name": "shared", "description": "not an update", "inputSchema": {"different": true}, "side_effect": "read"}]})).await;
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
