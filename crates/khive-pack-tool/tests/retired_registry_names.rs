//! A retired registry name cannot be revived through the ordinary registration surface.

use khive_pack_kg::KgPack;
use khive_pack_tool::ToolPack;
use khive_runtime::{KhiveRuntime, Namespace, RuntimeError, VerbRegistryBuilder};
use khive_storage::Entity;
use serde_json::json;

#[tokio::test]
async fn retired_registry_name_requires_registering_a_new_name() {
    let runtime = KhiveRuntime::memory().expect("isolated memory runtime");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(runtime.clone()));
    builder.register(ToolPack::new(runtime.clone()));
    let registry = builder.build().expect("tool registry");
    registry.apply_schema_plans(runtime.backend());
    runtime.install_edge_rules(registry.all_edge_rules());

    let token = runtime.authorize(Namespace::local()).unwrap();
    let entities = runtime.entities(&token).unwrap();
    let mut tombstone = Entity::new("local", "project", "retired_tool");
    tombstone.entity_type = Some("tool".into());
    tombstone.tags = vec!["tool-registry".into(), "tool".into()];
    tombstone.properties = Some(json!({"source": "fixture:original", "trust": "first_party"}));
    tombstone.deleted_at = Some(tombstone.updated_at);
    let id = tombstone.id;
    // Public delete refuses registry rows; direct storage represents a historical tombstone.
    entities.upsert_entity(tombstone).await.unwrap();
    let before = entities
        .get_entity_including_deleted(id)
        .await
        .unwrap()
        .expect("seeded tombstone exists");
    assert!(before.deleted_at.is_some());

    let error = registry
        .dispatch("tool.register", json!({"name": "retired_tool"}))
        .await
        .expect_err("a retired name must stay retired");
    let RuntimeError::InvalidInput(message) = error else {
        panic!("expected retired-name refusal, got {error:?}");
    };
    assert_eq!(
        message,
        format!(
            "registry name \"retired_tool\" is soft-deleted at entity {id}; the name stays retired; register a new name"
        )
    );
    assert!(
        !message.contains("restore"),
        "obsolete recovery advice: {message}"
    );
    let after = entities
        .get_entity_including_deleted(id)
        .await
        .unwrap()
        .expect("refused tombstone remains stored");
    assert!(after.deleted_at.is_some());
    assert_eq!(
        serde_json::to_value(&after).unwrap(),
        serde_json::to_value(&before).unwrap()
    );

    let replacement = registry
        .dispatch("tool.register", json!({"name": "replacement_tool"}))
        .await
        .expect("the suggested new-name path succeeds");
    assert_eq!(replacement["created"], true);
    assert_eq!(replacement["tool"]["name"], "replacement_tool");
    assert_ne!(replacement["tool"]["full_id"], id.to_string());
    let described = registry
        .dispatch("tool.describe", json!({"tool": "replacement_tool"}))
        .await
        .expect("the new registry name is usable");
    assert_eq!(described["tool"]["name"], "replacement_tool");
    let listed = registry.dispatch("tool.list", json!({})).await.unwrap();
    assert_eq!(listed["count"], 1);
    assert!(entities
        .get_entity_including_deleted(id)
        .await
        .unwrap()
        .unwrap()
        .deleted_at
        .is_some());
}

use khive_runtime::{NamespaceToken, VerbRegistry};
use khive_storage::SqlStatement;
use serde_json::Value;
use uuid::Uuid;

struct DeleteBoundaryFixture {
    runtime: KhiveRuntime,
    registry: VerbRegistry,
    token: NamespaceToken,
    name: String,
    id: Uuid,
}

impl DeleteBoundaryFixture {
    async fn new(name: &str, legacy: bool) -> Self {
        let runtime = KhiveRuntime::memory().expect("isolated memory runtime");
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
        let token = runtime.authorize(Namespace::local()).unwrap();
        let legacy_id = if legacy {
            let row = Entity::new("local", "project", name)
                .with_entity_type(Some("tool"))
                .with_tags(vec!["tool-registry".into(), "tool".into()])
                .with_properties(json!({
                    "source": "mcp:original", "side_effect": "write", "trust": "first_party"
                }));
            let id = row.id;
            // Minted v4 IDs cannot collide with the registration claim's v5 ID.
            assert_eq!(id.get_version_num(), 4);
            runtime
                .entities(&token)
                .unwrap()
                .upsert_entity(row)
                .await
                .unwrap();
            Some(id)
        } else {
            None
        };
        let registered = registry
            .dispatch(
                "tool.register",
                json!({
                    "name": name, "source": "mcp:original", "side_effect": "write",
                    "trust": "first_party", "capabilities": ["retained capability"]
                }),
            )
            .await
            .expect("registration and capability fixture");
        let id: Uuid = registered["tool"]["full_id"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(registered["created"], !legacy);
        if let Some(legacy_id) = legacy_id {
            assert_eq!(id, legacy_id);
        } else {
            assert_eq!(id.get_version_num(), 5);
        }
        let requested = registry
            .dispatch(
                "tool.request",
                json!({"actor": "agent:requester", "tool": name}),
            )
            .await
            .unwrap();
        let granted = registry
            .dispatch("tool.grant", json!({"id": requested["request_id"]}))
            .await
            .unwrap();
        assert_eq!(granted["grant"]["status"], "granted");
        assert_eq!(granted["grant"]["registry_id"], id.to_string());
        assert!(granted["grant"]["definition_digest"].is_string());
        let policy = registry
            .dispatch(
                "tool.policy",
                json!({
                    "actor": "agent:requester", "tool": name, "decision": "allow"
                }),
            )
            .await
            .unwrap();
        assert!(policy["policy"]["id"].is_string());
        let check = registry
            .dispatch(
                "tool.check",
                json!({"actor": "agent:requester", "tool": name}),
            )
            .await
            .unwrap();
        assert_eq!(check["source"], "grant");
        assert_eq!(check["decision"], "allow");
        let described = registry
            .dispatch("tool.describe", json!({"tool": name}))
            .await
            .unwrap();
        let capabilities = described["tool"]["capabilities"].as_array().unwrap();
        assert_eq!(capabilities.len(), 1);
        assert_eq!(capabilities[0]["name"], "retained capability");
        let capability_id: Uuid = capabilities[0]["id"].as_str().unwrap().parse().unwrap();
        assert!(runtime.get_entity(&token, capability_id).await.is_ok());
        Self {
            runtime,
            registry,
            token,
            name: name.into(),
            id,
        }
    }

    async fn row(&self) -> Entity {
        self.runtime
            .entities(&self.token)
            .unwrap()
            .get_entity_including_deleted(self.id)
            .await
            .unwrap()
            .expect("registry row remains stored")
    }

    async fn seed_historical_tombstone(&self) {
        let mut row = self.row().await;
        assert!(row.deleted_at.is_none());
        row.deleted_at = Some(row.updated_at);
        // Privileged storage represents a historical tombstone; no public revival is implied.
        self.runtime
            .entities(&self.token)
            .unwrap()
            .upsert_entity(row)
            .await
            .unwrap();
        assert!(self.row().await.deleted_at.is_some());
    }

    async fn state(&self) -> Value {
        let mut reader = self.runtime.sql().reader().await.unwrap();
        let mut result = serde_json::Map::new();
        for table in [
            "entities",
            "graph_edges",
            "events",
            "tool_policy",
            "tool_grants",
        ] {
            let rows = reader
                .query_all(SqlStatement::new(
                    format!("SELECT * FROM {table} ORDER BY id"),
                    Vec::new(),
                ))
                .await
                .unwrap();
            assert!(!rows.is_empty(), "populated {table}");
            result.insert(table.into(), serde_json::to_value(rows).unwrap());
        }
        Value::Object(result)
    }

    async fn assert_replacement_refused(&self, retired: bool, before: &Value) {
        let error = self
            .registry
            .dispatch(
                "tool.register",
                json!({
                    "name": self.name.to_ascii_uppercase(), "source": "mcp:replacement",
                    "description": "must not replace the row", "capabilities": ["must not link"]
                }),
            )
            .await
            .expect_err("same-name source replacement must refuse");
        let RuntimeError::InvalidInput(reason) = error else {
            panic!("expected registration refusal, got {error:?}");
        };
        assert!(reason.contains("new name"), "{reason}");
        if retired {
            assert!(reason.contains("soft-deleted"), "{reason}");
            assert!(reason.contains(&self.id.to_string()), "{reason}");
        } else {
            assert!(reason.contains("source"), "{reason}");
        }
        assert_eq!(&self.state().await, before);
        let row = self.row().await;
        assert_eq!(row.properties.as_ref().unwrap()["source"], "mcp:original");
        assert_eq!(row.deleted_at.is_some(), retired);
    }
}

#[derive(Clone, Copy, Debug)]
enum DeleteRoute {
    Ordinary,
    AtomicPrepare,
}

async fn assert_public_delete_boundary(route: DeleteRoute) {
    for retired in [false, true] {
        let fixture = DeleteBoundaryFixture::new("reserved tool", false).await;
        if retired {
            fixture.seed_historical_tombstone().await;
        }
        let before = fixture.state().await;
        let args = json!({"id": fixture.id.to_string(), "hard": true});
        let refused = match route {
            DeleteRoute::Ordinary => fixture.registry.dispatch("delete", args).await.map(|_| ()),
            // This is the actual atomic delete preparer used by the CLI, not an MCP transport test.
            DeleteRoute::AtomicPrepare => khive_runtime::atomic_prepare::prepare_delete(
                &fixture.runtime,
                &fixture.token,
                &args,
                None,
            )
            .await
            .map(|_| ()),
        };
        let error = refused.expect_err("public hard deletion must refuse a registry row");
        let RuntimeError::InvalidInput(reason) = error else {
            panic!("expected registry deletion refusal for {route:?}, got {error:?}");
        };
        assert!(reason.contains("delete"), "{reason}");
        assert!(reason.contains("tool-registry"), "{reason}");
        assert_eq!(
            fixture.state().await,
            before,
            "{route:?}, retired={retired}"
        );
        fixture.assert_replacement_refused(retired, &before).await;

        // The same route still permits hard deletion of an ordinary entity.
        let plain = Entity::new("local", "concept", "ordinary control");
        let plain_id = plain.id;
        fixture
            .runtime
            .entities(&fixture.token)
            .unwrap()
            .upsert_entity(plain)
            .await
            .unwrap();
        let args = json!({"id": plain_id.to_string(), "hard": true});
        match route {
            DeleteRoute::Ordinary => {
                let deleted = fixture.registry.dispatch("delete", args).await.unwrap();
                assert_eq!(deleted["deleted"], true);
            }
            DeleteRoute::AtomicPrepare => {
                let plan = khive_runtime::atomic_prepare::prepare_delete(
                    &fixture.runtime,
                    &fixture.token,
                    &args,
                    None,
                )
                .await
                .expect("ordinary entity is admitted by the atomic preparer");
                let outcome =
                    khive_runtime::run_atomic_unit(fixture.runtime.sql().as_ref(), vec![plan])
                        .await
                        .unwrap();
                let khive_runtime::AtomicRunOutcome::Committed { post_commit } = outcome else {
                    panic!("ordinary hard deletion must commit");
                };
                khive_runtime::atomic_prepare::apply_post_commit_effects(
                    &fixture.runtime,
                    &fixture.token,
                    post_commit,
                )
                .await
                .unwrap();
            }
        }
        assert!(fixture
            .runtime
            .entities(&fixture.token)
            .unwrap()
            .get_entity_including_deleted(plain_id)
            .await
            .unwrap()
            .is_none());
    }
}

#[tokio::test]
async fn public_hard_delete_preserves_registry_rows_and_name_reservations() {
    assert_public_delete_boundary(DeleteRoute::Ordinary).await;
}

#[tokio::test]
async fn atomic_hard_delete_prepare_preserves_registry_rows_and_name_reservations() {
    assert_public_delete_boundary(DeleteRoute::AtomicPrepare).await;
}

#[tokio::test]
async fn legacy_registry_tombstone_refuses_source_replacement_with_authorization_state() {
    let fixture = DeleteBoundaryFixture::new("legacy reserved tool", true).await;
    fixture.seed_historical_tombstone().await;
    let before = fixture.state().await;
    fixture.assert_replacement_refused(true, &before).await;

    let replacement = fixture
        .registry
        .dispatch(
            "tool.register",
            json!({
                "name": "different replacement name", "source": "mcp:replacement"
            }),
        )
        .await
        .expect("the new-name remedy remains usable");
    assert_eq!(replacement["created"], true);
    assert_ne!(replacement["tool"]["full_id"], fixture.id.to_string());
    assert_eq!(
        fixture.row().await.properties.as_ref().unwrap()["source"],
        "mcp:original"
    );
    assert!(fixture.row().await.deleted_at.is_some());
}
