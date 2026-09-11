//! ADR-180 Amendments 1 and 2 through actual grant and decision consumers.

use khive_pack_kg::KgPack;
use khive_pack_tool::{RegistryPin, ToolPack};
use khive_runtime::{KhiveRuntime, Namespace, VerbRegistry, VerbRegistryBuilder};
use khive_storage::{Entity, SqlStatement, SqlValue};
use serde_json::{json, Value};

struct Fixture {
    rt: KhiveRuntime,
    registry: VerbRegistry,
}

impl Fixture {
    fn new() -> Self {
        let rt = KhiveRuntime::memory().expect("memory runtime");
        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(rt.clone()));
        builder.register(ToolPack::new(rt.clone()));
        let registry = builder.build().unwrap();
        registry.apply_schema_plans(rt.backend());
        rt.install_edge_rules(registry.all_edge_rules());
        Self { rt, registry }
    }

    async fn call(&self, verb: &str, args: Value) -> Value {
        self.registry
            .dispatch(verb, args)
            .await
            .unwrap_or_else(|error| panic!("{verb}: {error}"))
    }

    async fn register(&self, name: &str) -> Value {
        self.call("tool.register", json!({"name":name, "source":"mcp:fixture", "side_effect":"write", "trust":"first_party", "schema":{"type":"object", "properties":{"z":{"type":"integer"}, "a":{"type":"string"}}}})).await["tool"].clone()
    }

    async fn request(&self, name: &str) -> String {
        self.call(
            "tool.request",
            json!({"tool":name, "actor":"agent:requester"}),
        )
        .await["request_id"]
            .as_str()
            .expect("request row")
            .to_string()
    }

    async fn grant(&self, id: &str) -> Value {
        self.call("tool.grant", json!({"id":id})).await["grant"].clone()
    }

    async fn check(&self, name: &str) -> Value {
        self.call(
            "tool.check",
            json!({"tool":name, "actor":"agent:requester"}),
        )
        .await
    }

    async fn row(&self, id: &str) -> Value {
        self.call("tool.requests", json!({})).await["requests"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["id"] == id)
            .expect("grant row")
            .clone()
    }

    async fn entity(&self, id: &Value) -> Entity {
        let token = self.rt.authorize(Namespace::local()).unwrap();
        self.rt
            .get_entity(&token, id.as_str().unwrap().parse().unwrap())
            .await
            .unwrap()
    }

    async fn write(&self, sql: &str, params: Vec<SqlValue>) {
        self.rt
            .sql()
            .writer()
            .await
            .unwrap()
            .execute(SqlStatement {
                sql: sql.into(),
                params,
                label: None,
            })
            .await
            .unwrap();
    }

    async fn properties(&self, tool: &Value, properties: Value) {
        self.write(
            "UPDATE entities SET properties = ?1 WHERE id = ?2",
            vec![
                SqlValue::Json(properties),
                SqlValue::Text(tool["full_id"].as_str().unwrap().into()),
            ],
        )
        .await;
    }

    async fn retire(&self, tool: &Value) {
        // The owning-pack fixture bypasses the generic KG mutation refusal.
        self.write(
            "UPDATE entities SET deleted_at = ?1 WHERE id = ?2",
            vec![
                SqlValue::Integer(chrono::Utc::now().timestamp_micros()),
                SqlValue::Text(tool["full_id"].as_str().unwrap().into()),
            ],
        )
        .await;
    }
}

#[tokio::test]
async fn grant_binds_current_definition_only_when_the_decision_is_made() {
    let f = Fixture::new();
    let tool = f.register("pinned").await;
    let id = f.request("pinned").await;
    let requested = f.row(&id).await;
    assert!(requested["registry_id"].is_null());
    assert!(requested["definition_digest"].is_null());
    let mut properties = f.entity(&tool["full_id"]).await.properties.unwrap();
    properties["trust"] = json!("marketplace");
    f.properties(&tool, properties).await;
    let grant = f.grant(&id).await;
    let pin = RegistryPin::from_entity(&f.entity(&tool["full_id"]).await).unwrap();
    assert_eq!(grant["registry_id"], tool["full_id"]);
    assert_eq!(grant["definition_digest"], pin.definition_digest());
    assert_eq!(
        grant["definition_digest"],
        blake3::hash(pin.canonical_bytes()).to_hex().to_string()
    );
    assert!(grant.get("canonical_bytes").is_none());
    let check = f.check("pinned").await;
    assert_eq!(check["decision"], "allow");
    assert_eq!(check["source"], "grant");
    assert_eq!(check["grant_id"], id);
}

#[tokio::test]
async fn description_is_outside_the_pin_but_each_policy_input_invalidates_it() {
    let f = Fixture::new();
    let tool = f.register("pinned").await;
    let id = f.request("pinned").await;
    f.grant(&id).await;
    let policy = f
        .call(
            "tool.policy",
            json!({"actor":"agent:requester", "tool":"pinned", "decision":"deny"}),
        )
        .await;
    f.write(
        "UPDATE entities SET description = 'owning-pack correction' WHERE id = ?1",
        vec![SqlValue::Text(tool["full_id"].as_str().unwrap().into())],
    )
    .await;
    assert_eq!(f.check("pinned").await["source"], "grant");
    let original = f.entity(&tool["full_id"]).await.properties.unwrap();
    for (field, value) in [
        ("source", json!("mcp:replacement")),
        ("side_effect", json!("egress")),
        ("trust", json!("external")),
        ("schema", json!({"type":"object", "required":["different"]})),
    ] {
        let mut changed = original.clone();
        changed[field] = value;
        f.properties(&tool, changed).await;
        let check = f.check("pinned").await;
        assert_eq!(check["source"], "policy", "{field}: {check}");
        assert_eq!(check["decision"], "deny");
        assert_eq!(check["policy_id"], policy["policy"]["id"]);
        assert_eq!(f.row(&id).await["status"], "granted");
        f.properties(&tool, original.clone()).await;
        assert_eq!(f.check("pinned").await["source"], "grant");
    }
}

#[tokio::test]
async fn unregistered_grant_ends_at_first_registration_even_after_removal() {
    for (grant_name, registered_name) in [
        ("future", "future"),
        ("future", "FUTURE"),
        ("future*", "future-child"),
        ("*", "anything"),
        ("a\0b*", "a\0bcd"),
        ("A\0B", "a\0b"),
    ] {
        let f = Fixture::new();
        let id = f.request(grant_name).await;
        let original = f.grant(&id).await;
        assert!(original["registry_id"].is_null());
        assert!(original["definition_digest"].is_null());
        assert_eq!(f.check(grant_name).await["source"], "grant");
        let tool = f.register(registered_name).await;
        let invalidated = f.row(&id).await;
        assert_eq!(invalidated["status"], "granted");
        assert_eq!(invalidated["invalidated_by_registry_id"], tool["full_id"]);
        assert!(invalidated["invalidated_at"].is_string());
        assert!(invalidated["registry_id"].is_null());
        assert!(invalidated["definition_digest"].is_null());
        assert_eq!(f.check(registered_name).await["source"], "default");
        f.retire(&tool).await;
        let after = f.check(grant_name).await;
        assert_eq!(after["source"], "default", "{grant_name}: {after}");
        assert_eq!(after["decision"], "ask");
        assert_eq!(
            f.row(&id).await["invalidated_by_registry_id"],
            tool["full_id"]
        );
    }
}

#[tokio::test]
async fn a_new_registered_approval_binds_the_replacement_and_supersedes_invalidation() {
    let f = Fixture::new();
    let old_id = f.request("future").await;
    f.grant(&old_id).await;
    let original = f.register("future").await;
    assert_eq!(f.check("future").await["decision"], "ask");
    let new_id = f.request("future").await;
    let new_grant = f.grant(&new_id).await;
    assert_eq!(new_grant["registry_id"], original["full_id"]);
    assert_eq!(f.check("future").await["grant_id"], new_id);
    f.retire(&original).await;
    f.call("tool.deny", json!({"id":old_id})).await;
    let absent_reapproval = f.grant(&old_id).await;
    assert!(absent_reapproval["registry_id"].is_null());
    assert!(absent_reapproval["definition_digest"].is_null());
    assert_eq!(
        absent_reapproval["invalidated_by_registry_id"],
        original["full_id"]
    );
    assert!(absent_reapproval["invalidated_at"].is_string());
    assert_eq!(f.check("future").await["source"], "default");
    let replacement = f.register("future").await;
    assert_ne!(original["full_id"], replacement["full_id"]);
    assert_eq!(f.check("future").await["source"], "default");
    f.call("tool.deny", json!({"id":old_id})).await;
    assert_eq!(
        f.row(&old_id).await["invalidated_by_registry_id"],
        original["full_id"]
    );
    let reapproved = f.grant(&old_id).await;
    assert_eq!(reapproved["registry_id"], replacement["full_id"]);
    assert!(reapproved["invalidated_by_registry_id"].is_null());
    assert!(reapproved["invalidated_at"].is_null());
    assert_eq!(f.check("future").await["grant_id"], old_id);
}

#[tokio::test]
async fn names_differing_after_nul_are_distinct_registry_and_invalidation_targets() {
    let f = Fixture::new();
    let id = f.request("a\0b").await;
    f.grant(&id).await;
    let other = f.register("a\0c").await;
    let still_absent = f.check("a\0b").await;
    assert_eq!(still_absent["registered"], false);
    assert_eq!(still_absent["source"], "grant");
    assert!(f.row(&id).await["invalidated_by_registry_id"].is_null());
    let intended = f.register("a\0b").await;
    assert_ne!(intended["full_id"], other["full_id"]);
    assert_eq!(
        f.row(&id).await["invalidated_by_registry_id"],
        intended["full_id"]
    );
    f.retire(&intended).await;
    assert_eq!(f.check("a\0b").await["source"], "default");
}

#[tokio::test]
async fn wildcard_grants_record_registrations_that_precede_the_approval() {
    for register_before_request in [true, false] {
        for (pattern, name) in [("family.*", "family.live"), ("a\0b*", "a\0bcd")] {
            let f = Fixture::new();
            let (tool, id) = if register_before_request {
                let tool = f.register(name).await;
                (tool, f.request(pattern).await)
            } else {
                let id = f.request(pattern).await;
                (f.register(name).await, id)
            };
            let granted = f.grant(&id).await;
            assert_eq!(granted["status"], "granted");
            assert!(granted["registry_id"].is_null());
            assert!(granted["definition_digest"].is_null());
            assert_eq!(granted["invalidated_by_registry_id"], tool["full_id"]);
            assert!(granted["invalidated_at"].is_string());
            let unrelated = f.request("unrelated.*").await;
            let granted = f.grant(&unrelated).await;
            assert!(granted["invalidated_by_registry_id"].is_null());
            assert_eq!(f.check("unrelated.future").await["source"], "grant");
            f.retire(&tool).await;
            assert_eq!(f.check(name).await["source"], "default");
        }
    }
}

#[tokio::test]
async fn pin_only_mutations_and_partial_pins_fall_through_without_changing_status() {
    let f = Fixture::new();
    let tool = f.register("pinned").await;
    let other = f.register("other").await;
    let id = f.request("pinned").await;
    let grant = f.grant(&id).await;
    for (registry_id, digest, invalidated_by, invalidated_at) in [
        (
            other["full_id"].clone(),
            grant["definition_digest"].clone(),
            Value::Null,
            Value::Null,
        ),
        (
            Value::Null,
            grant["definition_digest"].clone(),
            Value::Null,
            Value::Null,
        ),
        (
            tool["full_id"].clone(),
            Value::Null,
            Value::Null,
            Value::Null,
        ),
        (Value::Null, Value::Null, Value::Null, Value::Null),
        (
            tool["full_id"].clone(),
            json!("different"),
            Value::Null,
            Value::Null,
        ),
        (
            tool["full_id"].clone(),
            grant["definition_digest"].clone(),
            other["full_id"].clone(),
            Value::Null,
        ),
        (
            tool["full_id"].clone(),
            grant["definition_digest"].clone(),
            Value::Null,
            json!(1),
        ),
    ] {
        let text = |value: &Value| {
            value
                .as_str()
                .map_or(SqlValue::Null, |value| SqlValue::Text(value.into()))
        };
        f.write("UPDATE tool_grants SET registry_id=?1, definition_digest=?2, invalidated_by_registry_id=?3, invalidated_at=?4 WHERE id=?5", vec![text(&registry_id), text(&digest), text(&invalidated_by), invalidated_at.as_i64().map_or(SqlValue::Null, SqlValue::Integer), SqlValue::Text(id.clone())]).await;
        let check = f.check("pinned").await;
        assert_eq!(check["source"], "default", "{check}");
        assert_eq!(check["decision"], "ask");
        assert_eq!(f.row(&id).await["status"], "granted");
        f.write("UPDATE tool_grants SET registry_id=?1, definition_digest=?2, invalidated_by_registry_id=NULL, invalidated_at=NULL WHERE id=?3", vec![text(&tool["full_id"]), text(&grant["definition_digest"]), SqlValue::Text(id.clone())]).await;
        assert_eq!(f.check("pinned").await["source"], "grant");
    }
}

#[tokio::test]
async fn bootstrap_marks_legacy_null_pins_without_binding_them() {
    let f = Fixture::new();
    let tool = f.register("legacy").await;
    let id = f.request("legacy").await;
    f.grant(&id).await;
    f.write(
        "UPDATE tool_grants SET registry_id=NULL, definition_digest=NULL WHERE id=?1",
        vec![SqlValue::Text(id.clone())],
    )
    .await;
    assert_eq!(f.check("legacy").await["source"], "default");
    f.registry.apply_schema_plans(f.rt.backend());
    let row = f.row(&id).await;
    assert!(row["registry_id"].is_null());
    assert!(row["definition_digest"].is_null());
    assert_eq!(row["invalidated_by_registry_id"], tool["full_id"]);
    assert_eq!(row["status"], "granted");
    f.retire(&tool).await;
    assert_eq!(f.check("legacy").await["source"], "default");
}

#[tokio::test]
async fn re_registration_keeps_the_approved_object_and_definition() {
    let f = Fixture::new();
    let first = f.register("same").await;
    let id = f.request("same").await;
    let grant = f.grant(&id).await;
    let again = f.call("tool.register", json!({"name":"same", "source":"mcp:changed", "side_effect":"read", "trust":"external", "schema":{"type":"object", "required":["changed"]}, "capabilities":["new capability"]})).await;
    assert_eq!(again["created"], false);
    assert_eq!(again["tool"]["full_id"], first["full_id"]);
    let described = f.call("tool.describe", json!({"tool":"same"})).await;
    assert!(described["tool"]["capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .any(|capability| capability["name"] == "new capability"));
    assert_eq!(f.check("same").await["source"], "grant");
    assert_eq!(
        f.row(&id).await["definition_digest"],
        grant["definition_digest"]
    );
}

#[tokio::test]
async fn grant_resolves_a_registration_beyond_the_old_registry_list_cap() {
    let f = Fixture::new();
    let original = f.register("oldest-tool").await;
    let created_at = f.entity(&original["full_id"]).await.created_at;
    f.write(
        "WITH RECURSIVE newer(n) AS (VALUES(1) UNION ALL SELECT n+1 FROM newer WHERE n<5001) \
         INSERT INTO entities (id, namespace, kind, entity_type, name, properties, tags, created_at, updated_at) \
         SELECT printf('00000000-0000-4000-8000-%012d', n), 'local', 'project', 'tool', \
                'newer-tool-' || n, '{\"side_effect\":\"write\"}', '[\"tool-registry\",\"tool\"]', ?1+n, ?1+n \
         FROM newer",
        vec![SqlValue::Integer(created_at)],
    ).await;
    let id = f.request("oldest-tool").await;
    let granted = f.grant(&id).await;
    assert_eq!(granted["registry_id"], original["full_id"]);
    let again = f.call("tool.register", json!({"name":"oldest-tool"})).await;
    assert_eq!(again["created"], false);
    assert_eq!(again["tool"]["full_id"], original["full_id"]);
    assert_eq!(f.check("oldest-tool").await["source"], "grant");
}

#[tokio::test]
async fn migrated_and_direct_bootstrap_grant_schemas_and_triggers_agree() {
    let f = Fixture::new();
    let direct = khive_db::StorageBackend::memory().unwrap();
    direct.apply_pack_ddl_statements(&[
        "CREATE TABLE entities (id TEXT PRIMARY KEY, namespace TEXT, kind TEXT, tags TEXT, name TEXT, created_at INTEGER, deleted_at INTEGER)",
    ]).unwrap();
    for _ in 0..2 {
        direct
            .apply_pack_ddl_statements(&khive_pack_tool::vocab::TOOL_SCHEMA_PLAN_STMTS)
            .unwrap();
    }
    for sql in [
        "SELECT cid, name, type, \"notnull\", dflt_value, pk FROM pragma_table_info('tool_grants') ORDER BY cid",
        "SELECT sql FROM sqlite_master WHERE type='trigger' AND name='tool_grants_invalidate_on_registry_insert'",
    ] {
        let mut migrated_reader = f.rt.sql().reader().await.unwrap();
        let mut direct_reader = direct.sql().reader().await.unwrap();
        let statement = || SqlStatement { sql:sql.into(), params:vec![], label:None };
        let migrated = migrated_reader.query_all(statement()).await.unwrap();
        let bootstrapped = direct_reader.query_all(statement()).await.unwrap();
        assert!(!migrated.is_empty(), "{sql}");
        assert_eq!(serde_json::to_value(migrated).unwrap(), serde_json::to_value(bootstrapped).unwrap(), "{sql}");
    }
}

#[test]
fn schema_key_order_preserves_digest_but_changed_values_do_not() {
    let mut entity = Entity::new("local", "project", "canonical").with_properties(serde_json::from_str(r#"{"schema":{"z":{"type":"integer"},"a":[{"y":2,"x":1}]},"trust":"first_party","source":"mcp:fixture","side_effect":"write"}"#).unwrap());
    let original = RegistryPin::from_entity(&entity).unwrap();
    entity.properties = Some(serde_json::from_str(r#"{"side_effect":"write","source":"mcp:fixture","trust":"first_party","schema":{"a":[{"x":1,"y":2}],"z":{"type":"integer"}}}"#).unwrap());
    assert_eq!(RegistryPin::from_entity(&entity).unwrap(), original);
    entity.properties.as_mut().unwrap()["schema"]["a"][0]["x"] = json!(9);
    assert_ne!(
        RegistryPin::from_entity(&entity)
            .unwrap()
            .definition_digest(),
        original.definition_digest()
    );
}
