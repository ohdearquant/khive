//! Registry retirement through the real dispatch, authorization and storage paths.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};

use khive_gate::{classify_operation, Gate, GateDecision, GateError, GateRequest, OperationAccess};
use khive_pack_kg::KgPack;
use khive_pack_tool::{policy::registry_retirement_statements, ToolPack};
use khive_runtime::curation::EntityPatch;
use khive_runtime::pack::PackRuntime;
use khive_runtime::{
    KhiveRuntime, Namespace, NamespaceToken, RuntimeError, VerbRegistry, VerbRegistryBuilder,
};
use khive_storage::{Entity, SqlRow, SqlStatement, SqlValue};
use khive_types::pack::{TOOL_REGISTRY_ENTITY_KIND, TOOL_REGISTRY_TAG};
use serde_json::{json, Value};
use uuid::Uuid;

const REVIEWER: &str = "seat:retirement";
const REQUESTER: &str = "agent:requester";

struct Fixture {
    rt: KhiveRuntime,
    registry: VerbRegistry,
}

impl Fixture {
    fn new() -> Self {
        Self::with_gate(None)
    }

    fn with_gate(gate: Option<Arc<dyn Gate>>) -> Self {
        let rt = KhiveRuntime::memory().unwrap();
        let mut builder = VerbRegistryBuilder::new();
        builder.with_actor_id(Some(REVIEWER.into()));
        if let Some(gate) = gate {
            builder.with_gate(gate);
        }
        builder.register(KgPack::new(rt.clone()));
        builder.register(ToolPack::new(rt.clone()));
        let registry = builder.build().unwrap();
        registry.apply_schema_plans(rt.backend());
        rt.install_edge_rules(registry.all_edge_rules());
        Self { rt, registry }
    }

    fn token(&self, namespace: &str) -> NamespaceToken {
        self.rt
            .authorize(Namespace::parse(namespace).unwrap())
            .unwrap()
    }

    async fn call(&self, verb: &str, args: Value) -> Value {
        self.registry
            .dispatch(verb, args)
            .await
            .unwrap_or_else(|error| panic!("{verb}: {error}"))
    }

    async fn refused(&self, verb: &str, args: Value) -> RuntimeError {
        self.registry.dispatch(verb, args).await.expect_err(verb)
    }

    async fn call_for(&self, token: &NamespaceToken, verb: &str, args: Value) -> Value {
        ToolPack::new(self.rt.clone())
            .dispatch(verb, args, &self.registry, token)
            .await
            .unwrap_or_else(|error| panic!("{verb}: {error}"))
    }

    async fn register(&self, namespace: &str, name: &str) -> Value {
        self.call("tool.register", json!({"namespace":namespace,"name":name,"description":"quartzrecord retirement search witness","source":"mcp:fixture","side_effect":"write","trust":"first_party","schema":{"type":"object"},"capabilities":["quartzrecord capability"]})).await
    }

    async fn policy(&self, namespace: &str, actor: &str, tool: &str, decision: &str) -> String {
        self.call("tool.policy", json!({"namespace":namespace,"actor":actor,"tool":tool,"decision":decision,"note":"retained policy history"})).await["policy"]["id"].as_str().unwrap().to_owned()
    }

    async fn request(&self, namespace: &str, name: &str, actor: &str) -> String {
        self.call("tool.request", json!({"namespace":namespace,"tool":name,"actor":actor,"reason":"retained request reason"})).await["request_id"].as_str().unwrap().to_owned()
    }

    async fn grant(&self, namespace: &str, id: &str) -> Value {
        self.call("tool.grant", json!({"namespace":namespace,"id":id}))
            .await
    }

    async fn write(&self, sql: impl Into<String>, params: Vec<SqlValue>) {
        self.rt
            .sql()
            .writer()
            .await
            .unwrap()
            .execute(SqlStatement::new(sql.into(), params))
            .await
            .unwrap();
    }

    async fn rows(&self, sql: impl Into<String>, params: Vec<SqlValue>) -> Vec<SqlRow> {
        self.rt
            .sql()
            .reader()
            .await
            .unwrap()
            .query_all(SqlStatement::new(sql.into(), params))
            .await
            .unwrap()
    }

    async fn row(&self, table: &str, id: &str) -> SqlRow {
        let mut rows = self
            .rows(
                format!("SELECT * FROM {table} WHERE id = ?1"),
                vec![SqlValue::Text(id.into())],
            )
            .await;
        assert_eq!(rows.len(), 1, "{table}/{id}");
        rows.remove(0)
    }

    async fn snapshot(&self) -> Value {
        let mut result = serde_json::Map::new();
        for table in [
            "entities",
            "notes",
            "graph_edges",
            "tool_policy",
            "tool_grants",
        ] {
            let rows = self
                .rows(
                    format!("SELECT * FROM {table} ORDER BY namespace, id"),
                    vec![],
                )
                .await;
            result.insert(table.into(), serde_json::to_value(rows).unwrap());
        }
        Value::Object(result)
    }

    async fn entity(&self, namespace: &str, id: Uuid) -> Entity {
        self.rt
            .entities(&self.token(namespace))
            .unwrap()
            .get_entity_including_deleted(id)
            .await
            .unwrap()
            .unwrap()
    }

    async fn fts_rows(&self, id: Uuid) -> Value {
        serde_json::to_value(
            self.rows(
                "SELECT * FROM fts_entities_rowids WHERE subject_id = ?1 ORDER BY namespace",
                vec![SqlValue::Text(id.to_string())],
            )
            .await,
        )
        .unwrap()
    }

    async fn historical_grant(
        &self,
        namespace: &str,
        name: &str,
        status: &str,
        registry: Option<&str>,
        digest: Option<&str>,
    ) -> String {
        let id = Uuid::new_v4().to_string();
        self.write("INSERT INTO tool_grants (id,namespace,actor,tool,scope,reason,status,requested_at,decided_at,decided_by,expires_at,decision_note,registry_id,definition_digest,invalidated_by_registry_id,invalidated_at) VALUES (?1,?2,?3,?4,'once','historical reason',?5,1000000,2000000,'historical reviewer',4102444800000000,'historical decision',?6,?7,'historical invalidation',3000000)", vec![SqlValue::Text(id.clone()),SqlValue::Text(namespace.into()),SqlValue::Text(REQUESTER.into()),SqlValue::Text(name.into()),SqlValue::Text(status.into()),optional_text(registry),optional_text(digest)]).await;
        id
    }
}

fn optional_text(value: Option<&str>) -> SqlValue {
    value.map_or(SqlValue::Null, |value| SqlValue::Text(value.into()))
}

fn id(registration: &Value) -> Uuid {
    registration["tool"]["full_id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap()
}

fn unchanged_columns(before: &SqlRow, after: &SqlRow, changed: &[&str]) {
    let retained = |row: &SqlRow| {
        serde_json::to_value(
            row.columns
                .iter()
                .filter(|column| !changed.contains(&column.name.as_str()))
                .collect::<Vec<_>>(),
        )
        .unwrap()
    };
    assert_eq!(retained(before), retained(after));
}

fn unchanged_row(before: &SqlRow, after: &SqlRow) {
    assert_eq!(
        serde_json::to_value(before).unwrap(),
        serde_json::to_value(after).unwrap()
    );
}

fn assert_retired(receipt: &Value, target: Uuid, policies: u64, grants: u64) {
    assert_eq!(receipt["id"], target.to_string());
    assert_eq!(receipt["full_id"], target.to_string());
    assert_eq!(receipt["retired"], true);
    assert_eq!(receipt["name_retired"], true);
    assert_eq!(receipt["policies_retired"], policies);
    assert_eq!(receipt["grants_revoked"], grants);
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn unregister_accepts_the_registered_kind_and_tag() {
    let f = Fixture::new();
    let target = id(&f.register("local", "registry-identity").await);
    let registered = f.entity("local", target).await;
    assert_eq!(registered.kind, TOOL_REGISTRY_ENTITY_KIND.name());
    assert!(registered.tags.iter().any(|tag| tag == TOOL_REGISTRY_TAG));

    let receipt = f.call("tool.unregister", json!({"tool":target})).await;
    assert_retired(&receipt, target, 0, 0);
    assert!(f.entity("local", target).await.deleted_at.is_some());
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn unregister_retires_registry_authorization_and_discovery() {
    let f = Fixture::new();
    let registration = f.register("local", "retiring-tool").await;
    let target = id(&registration);
    let survivor = id(&f.register("local", "surviving-tool").await);
    let policy = f
        .policy("local", "agent:policy", "retiring-tool", "ask")
        .await;
    f.policy("local", "agent:policy", "retiring-tool", "deny")
        .await;
    let request = f.request("local", "retiring-tool", REQUESTER).await;
    f.grant("local", &request).await;
    assert_eq!(
        f.call(
            "tool.check",
            json!({"tool":"retiring-tool","actor":REQUESTER})
        )
        .await["source"],
        "grant"
    );
    let suggested = f
        .call("tool.suggest", json!({"query":"quartzrecord","limit":20}))
        .await;
    assert!(
        suggested["results"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["full_id"] == target.to_string()),
        "{suggested}"
    );
    let old = f.entity("local", target).await;
    let old_policy = f.row("tool_policy", &policy).await;
    assert_ne!(old_policy.text("history").unwrap(), "[]");
    let old_grant = f.row("tool_grants", &request).await;
    let edges = f
        .rows("SELECT * FROM graph_edges ORDER BY namespace, id", vec![])
        .await;
    assert!(!edges.is_empty());
    assert!(!f.fts_rows(target).await.as_array().unwrap().is_empty());
    let before = f.snapshot().await;
    assert!(f
        .refused("delete", json!({"id":target}))
        .await
        .to_string()
        .contains("tool-registry"));
    assert_eq!(f.snapshot().await, before);
    let receipt = f.call("tool.unregister", json!({"tool":target})).await;
    assert_retired(&receipt, target, 1, 1);
    assert_eq!(receipt["post_commit_degradations"], json!([]));
    let retired = f.entity("local", target).await;
    assert!(retired.deleted_at.is_some());
    assert_eq!(retired.version, old.version + 1);
    let mut restored_fields = retired.clone();
    restored_fields.deleted_at = old.deleted_at;
    restored_fields.updated_at = old.updated_at;
    restored_fields.version = old.version;
    assert_eq!(restored_fields.properties, old.properties);
    assert_eq!(
        serde_json::to_value(restored_fields).unwrap(),
        serde_json::to_value(&old).unwrap()
    );
    let policy_after = f.row("tool_policy", &policy).await;
    unchanged_columns(&old_policy, &policy_after, &["deleted_at", "deleted_by"]);
    assert!(policy_after.opt_i64("deleted_at").unwrap().is_some());
    assert_eq!(policy_after.text("deleted_by").unwrap(), REVIEWER);
    let grant_after = f.row("tool_grants", &request).await;
    unchanged_columns(
        &old_grant,
        &grant_after,
        &["status", "decided_at", "decided_by", "decision_note"],
    );
    assert_eq!(grant_after.text("status").unwrap(), "revoked");
    assert_eq!(grant_after.text("decided_by").unwrap(), REVIEWER);
    assert_eq!(
        serde_json::to_value(
            f.rows("SELECT * FROM graph_edges ORDER BY namespace, id", vec![])
                .await
        )
        .unwrap(),
        serde_json::to_value(edges).unwrap()
    );
    for capability in registration["capabilities"].as_array().unwrap() {
        let capability_id = capability["id"].as_str().unwrap().parse().unwrap();
        assert!(f.entity("local", capability_id).await.deleted_at.is_none());
    }
    assert_eq!(f.fts_rows(target).await, json!([]));
    assert!(!f.fts_rows(survivor).await.as_array().unwrap().is_empty());
    let listed = f.call("tool.list", json!({})).await;
    assert_eq!(listed["tools"].as_array().unwrap().len(), 1);
    assert_eq!(listed["tools"][0]["full_id"], survivor.to_string());
    f.refused("tool.describe", json!({"tool":target})).await;
    let suggested = f
        .call("tool.suggest", json!({"query":"quartzrecord","limit":20}))
        .await;
    assert!(suggested["results"]
        .as_array()
        .unwrap()
        .iter()
        .any(|row| row["full_id"] == survivor.to_string()));
    assert!(!suggested["results"]
        .as_array()
        .unwrap()
        .iter()
        .any(|row| row["full_id"] == target.to_string()));
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn unregister_preserves_shared_rules_and_other_registry_ids() {
    for name in ["ExactName", "nul\0name", "literal*", "nul\0name*"] {
        let f = Fixture::new();
        let target = id(&f.register("local", name).await);
        let survivor = id(&f.register("local", "survivor").await);
        let mut exact = Vec::new();
        for actor in ["agent:one", "agent:two"] {
            let policy = f.policy("local", actor, name, "ask").await;
            exact.push((policy.clone(), f.row("tool_policy", &policy).await));
        }
        let mut preserved = Vec::new();
        for (index, pattern) in [
            "*".to_owned(),
            format!("{name}*"),
            format!("{name}*interior"),
            name.to_uppercase(),
            format!("{name}\0different"),
        ]
        .iter()
        .enumerate()
        {
            let policy = f
                .policy("local", &format!("agent:control-{index}"), pattern, "allow")
                .await;
            preserved.push((policy.clone(), f.row("tool_policy", &policy).await));
        }
        let foreign = f.policy("foreign", "agent:foreign", name, "deny").await;
        preserved.push((foreign.clone(), f.row("tool_policy", &foreign).await));
        let other = f
            .historical_grant(
                "local",
                name,
                "granted",
                Some(&survivor.to_string()),
                Some("other digest"),
            )
            .await;
        let other_before = f.row("tool_grants", &other).await;
        let unpinned = f
            .historical_grant("local", name, "requested", None, None)
            .await;
        let unpinned_before = f.row("tool_grants", &unpinned).await;
        let pinned = f
            .historical_grant(
                "local",
                name,
                "denied",
                Some(&target.to_string()),
                Some("target digest"),
            )
            .await;
        let has_star = name.contains('*');
        let receipt = f.call("tool.unregister", json!({"tool":target})).await;
        assert_retired(
            &receipt,
            target,
            if has_star { 0 } else { 2 },
            if has_star { 1 } else { 2 },
        );
        for (policy, before) in exact {
            let after = f.row("tool_policy", &policy).await;
            if has_star {
                unchanged_row(&before, &after);
            } else {
                assert!(after.opt_i64("deleted_at").unwrap().is_some());
                unchanged_columns(&before, &after, &["deleted_at", "deleted_by"]);
            }
        }
        for (policy, before) in preserved {
            unchanged_row(&before, &f.row("tool_policy", &policy).await);
        }
        unchanged_row(&other_before, &f.row("tool_grants", &other).await);
        if has_star {
            unchanged_row(&unpinned_before, &f.row("tool_grants", &unpinned).await);
        } else {
            assert_eq!(
                f.row("tool_grants", &unpinned)
                    .await
                    .text("status")
                    .unwrap(),
                "revoked"
            );
        }
        assert_eq!(
            f.row("tool_grants", &pinned).await.text("status").unwrap(),
            "revoked"
        );
        assert_eq!(
            f.call(
                "tool.check",
                json!({"tool":"survivor","actor":"agent:control-0"})
            )
            .await["decision"],
            "allow"
        );
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn unregister_preserves_foreign_pins_but_makes_them_inactive() {
    let f = Fixture::new();
    let owner = f.token("owner");
    let viewer =
        f.rt.authorize_with_visibility(
            Namespace::parse("viewer").unwrap(),
            vec![Namespace::parse("owner").unwrap()],
        )
        .unwrap();
    let independent = f.token("independent");
    let target = id(&f.register("owner", "shared-tool").await);
    let other = id(&f.register("independent", "shared-tool").await);
    let args = json!({"tool":"shared-tool","actor":REQUESTER});
    let request = f.call_for(&viewer, "tool.request", args.clone()).await["request_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let grant = f
        .call_for(&viewer, "tool.grant", json!({"id":request}))
        .await;
    assert_eq!(grant["grant"]["registry_id"], target.to_string());
    let other_request = f.call_for(&independent, "tool.request", args.clone()).await["request_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let other_grant = f
        .call_for(&independent, "tool.grant", json!({"id":other_request}))
        .await;
    assert_eq!(other_grant["grant"]["registry_id"], other.to_string());
    assert_eq!(
        f.call_for(&viewer, "tool.check", args.clone()).await["grant_id"],
        request
    );
    assert_eq!(
        f.call_for(&independent, "tool.check", args.clone()).await["grant_id"],
        other_request
    );
    let pending = f
        .historical_grant("viewer", "shared-tool", "requested", None, None)
        .await;
    let before = [
        f.row("tool_grants", &request).await,
        f.row("tool_grants", &other_request).await,
        f.row("tool_grants", &pending).await,
    ];
    let other_entity = f.entity("independent", other).await;
    let receipt = f
        .call_for(&owner, "tool.unregister", json!({"tool":target}))
        .await;
    assert_retired(&receipt, target, 0, 0);
    for (row, grant_id) in before.iter().zip([&request, &other_request, &pending]) {
        unchanged_row(row, &f.row("tool_grants", grant_id).await);
    }
    let after = f.call_for(&viewer, "tool.check", args.clone()).await;
    assert_eq!(after["registered"], false);
    assert_ne!(after["source"], "grant");
    assert_eq!(after["decision"], "ask");
    let other_check = f.call_for(&independent, "tool.check", args).await;
    assert_eq!(other_check["registered"], true);
    assert_eq!(other_check["grant_id"], other_request);
    assert_eq!(other_check["source"], "grant");
    let stored = f.entity("independent", other).await;
    assert_eq!(stored.properties, other_entity.properties);
    assert_eq!(
        serde_json::to_value(stored).unwrap(),
        serde_json::to_value(&other_entity).unwrap()
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn unregister_ends_existing_unpinned_requests_without_revival() {
    let f = Fixture::new();
    let target = id(&f.register("local", "request-tool").await);
    let other = id(&f.register("local", "other-tool").await);
    let requested = f.request("local", "request-tool", "agent:pending").await;
    let denied = f.request("local", "request-tool", "agent:denied").await;
    f.call("tool.deny", json!({"id":denied,"note":"retained denial"}))
        .await;
    let legacy = f
        .historical_grant("local", "request-tool", "granted", None, None)
        .await;
    let pinned = f
        .historical_grant(
            "local",
            "different recorded label",
            "requested",
            Some(&target.to_string()),
            Some("captured digest"),
        )
        .await;
    let ended = [requested, denied, legacy, pinned];
    let mut ended_before = Vec::new();
    for grant in &ended {
        ended_before.push(f.row("tool_grants", grant).await);
    }
    let mut preserved = Vec::new();
    for (name, status, registry, digest) in [
        ("request-tool", "revoked", None, None),
        ("*", "granted", None, None),
        ("request-*", "requested", None, None),
        ("request*tool", "denied", None, None),
        ("Request-tool", "requested", None, None),
        ("request-tool", "requested", None, Some("half pin")),
        ("request-tool", "granted", Some(other.to_string()), None),
        (
            "request-tool",
            "denied",
            Some(other.to_string()),
            Some("other digest"),
        ),
    ] {
        let grant = f
            .historical_grant("local", name, status, registry.as_deref(), digest)
            .await;
        preserved.push((grant.clone(), f.row("tool_grants", &grant).await));
    }
    let receipt = f
        .call("tool.unregister", json!({"tool":"request-tool"}))
        .await;
    assert_retired(&receipt, target, 0, ended.len() as u64);
    for (grant, before) in ended.iter().zip(&ended_before) {
        let after = f.row("tool_grants", grant).await;
        assert_eq!(after.text("status").unwrap(), "revoked");
        unchanged_columns(
            before,
            &after,
            &["status", "decided_at", "decided_by", "decision_note"],
        );
        assert_eq!(
            after.text("decision_note").unwrap(),
            "Registry registration retired"
        );
        let snapshot = f.snapshot().await;
        f.refused("tool.grant", json!({"id":grant})).await;
        assert_eq!(
            f.snapshot().await,
            snapshot,
            "a retired request cannot revive"
        );
    }
    for (grant, before) in preserved {
        unchanged_row(&before, &f.row("tool_grants", &grant).await);
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn unregister_is_own_namespace_only_and_idempotent() {
    let f = Fixture::new();
    let own = id(&f.register("local", "shared-name").await);
    let foreign = id(&f.register("visible", "shared-name").await);
    let hidden = id(&f.register("hidden", "hidden-tool").await);
    let foreign_only = id(&f.register("visible", "foreign-only").await);
    let caller =
        f.rt.authorize_with_visibility(
            Namespace::local(),
            vec![Namespace::parse("visible").unwrap()],
        )
        .unwrap();
    let ordinary = f
        .call(
            "create",
            json!({"kind":"concept","name":"ordinary","skip_dedup_check":true,"verbose":true}),
        )
        .await;
    let capability =
        f.call("tool.describe", json!({"tool":own})).await["tool"]["capabilities"][0]["id"].clone();
    let ordinary_id: Uuid = ordinary["id"].as_str().unwrap().parse().unwrap();
    let capability_id: Uuid = capability.as_str().unwrap().parse().unwrap();
    assert_eq!(f.entity("local", ordinary_id).await.kind, "concept");
    assert_eq!(f.entity("local", capability_id).await.kind, "concept");
    let before = f.snapshot().await;
    for reference in [
        json!(foreign),
        json!(hidden),
        json!(foreign_only),
        json!("foreign-only"),
        json!(ordinary_id),
        json!(capability_id),
        json!("does-not-exist"),
    ] {
        let result = ToolPack::new(f.rt.clone())
            .dispatch(
                "tool.unregister",
                json!({"tool":reference}),
                &f.registry,
                &caller,
            )
            .await;
        assert!(
            result.is_err(),
            "foreign or non-registry reference unexpectedly retired: {reference}"
        );
        assert_eq!(f.snapshot().await, before);
    }
    let receipt = f
        .call_for(&caller, "tool.unregister", json!({"tool":"shared-name"}))
        .await;
    assert_retired(&receipt, own, 0, 0);
    assert!(f.entity("visible", foreign).await.deleted_at.is_none());
    assert!(f.entity("hidden", hidden).await.deleted_at.is_none());
    let later_policy = f.policy("local", "agent:later", "shared-name", "ask").await;
    let later_request = f.request("local", "shared-name", "agent:later").await;
    let settled = f.snapshot().await;
    for reference in [json!(own), json!("shared-name")] {
        let retry = f.call("tool.unregister", json!({"tool":reference})).await;
        assert_eq!(
            retry,
            json!({"id":own,"full_id":own,"retired":false,"name_retired":true,"policies_retired":0,"grants_revoked":0,"post_commit_degradations":[]})
        );
        assert_eq!(f.snapshot().await, settled);
    }
    assert!(f
        .row("tool_policy", &later_policy)
        .await
        .opt_i64("deleted_at")
        .unwrap()
        .is_none());
    assert_eq!(
        f.row("tool_grants", &later_request)
            .await
            .text("status")
            .unwrap(),
        "requested"
    );
    assert!(f
        .refused("tool.register", json!({"name":"shared-name"}))
        .await
        .to_string()
        .contains("retired"));
    assert!(f
        .refused(
            "tool.ingest",
            json!({"source":"mcp","server":"fixture","tools":[{"name":"shared-name"}]})
        )
        .await
        .to_string()
        .contains("retired"));
    assert_eq!(f.snapshot().await, settled);
    // An explicit selected namespace may retire its own row, but not its namesake.
    assert_retired(
        &f.call(
            "tool.unregister",
            json!({"namespace":"visible","tool":foreign}),
        )
        .await,
        foreign,
        0,
        0,
    );
    let prefix_target = id(&f.register("local", "prefix-target").await);
    let prefix = prefix_target.simple().to_string()[..24].to_owned();
    assert_retired(
        &f.call("tool.unregister", json!({"tool":prefix})).await,
        prefix_target,
        0,
        0,
    );
    assert!(f.entity("local", prefix_target).await.deleted_at.is_some());
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn unregister_rolls_back_each_companion_failure() {
    for table in ["tool_policy", "tool_grants"] {
        let f = Fixture::new();
        let target = id(&f.register("local", "atomic-tool").await);
        f.policy("local", "agent:policy", "atomic-tool", "deny")
            .await;
        let request = f.request("local", "atomic-tool", REQUESTER).await;
        f.grant("local", &request).await;
        f.call(
            "create",
            json!({"kind":"observation","content":"unrelated retained observation"}),
        )
        .await;
        let before = f.snapshot().await;
        // This fault only fires if the preceding entity CAS is visible in the
        // same transaction; its error must roll back that CAS and both companions.
        f.write(format!("CREATE TRIGGER fail_retirement BEFORE UPDATE ON {table} WHEN EXISTS (SELECT 1 FROM entities WHERE id = '{target}' AND deleted_at IS NOT NULL) BEGIN SELECT RAISE(ABORT, 'retirement companion fault'); END"), vec![]).await;
        let error = f.refused("tool.unregister", json!({"tool":target})).await;
        assert!(
            error.to_string().contains("retirement companion fault"),
            "{error}"
        );
        assert_eq!(
            f.snapshot().await,
            before,
            "{table} failure must roll back the entire retirement"
        );
        assert_eq!(
            f.call(
                "tool.check",
                json!({"tool":"atomic-tool","actor":REQUESTER})
            )
            .await["source"],
            "grant"
        );
        f.write("DROP TRIGGER fail_retirement", vec![]).await;
        let retry = f.call("tool.unregister", json!({"tool":target})).await;
        assert_retired(&retry, target, 1, 1);
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn unregister_rejects_stale_registry_snapshot() {
    let f = Fixture::new();
    let target = id(&f.register("local", "changed-tool").await);
    f.policy("local", "agent:policy", "changed-tool", "deny")
        .await;
    let request = f.request("local", "changed-tool", REQUESTER).await;
    f.grant("local", &request).await;
    f.call(
        "create",
        json!({"kind":"observation","content":"unrelated retained observation"}),
    )
    .await;
    let token = f.token("local");
    let captured = f.entity("local", target).await;
    let (current, _) =
        f.rt.update_entity_with_index_report(
            &token,
            target,
            EntityPatch {
                description: Some(Some("a later registry description".into())),
                ..Default::default()
            },
            Some(captured.version),
        )
        .await
        .unwrap();
    assert!(current.version > captured.version);
    assert_ne!(current.description, captured.description);
    let before = f.snapshot().await;
    let retired_at = chrono::Utc::now()
        .timestamp_micros()
        .max(current.updated_at + 1);
    let companions =
        registry_retirement_statements("local", target, &captured.name, retired_at, REVIEWER);
    assert!(f
        .rt
        .retire_tool_registry_entity_with_post_commit_report(
            &token, &captured, retired_at, companions
        )
        .await
        .is_err());
    assert_eq!(
        f.snapshot().await,
        before,
        "stale registry CAS must not retire either authorization table"
    );
    let stored = f.entity("local", target).await;
    assert_eq!(stored.properties, current.properties);
    assert_eq!(
        serde_json::to_value(stored).unwrap(),
        serde_json::to_value(&current).unwrap()
    );
    assert_retired(
        &f.call("tool.unregister", json!({"tool":target})).await,
        target,
        1,
        1,
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn unregister_reports_committed_cleanup_failure() {
    let f = Fixture::new();
    let target = id(&f.register("local", "cleanup-tool").await);
    let survivor = id(&f.register("local", "cleanup-survivor").await);
    f.policy("local", "agent:policy", "cleanup-tool", "deny")
        .await;
    let request = f.request("local", "cleanup-tool", REQUESTER).await;
    f.grant("local", &request).await;
    let old_index = f.fts_rows(target).await;
    assert_eq!(old_index.as_array().unwrap().len(), 1);
    f.write(format!("CREATE TRIGGER fail_retirement_cleanup BEFORE DELETE ON fts_entities_rowids WHEN OLD.subject_id = '{target}' BEGIN SELECT RAISE(ABORT, 'retirement cleanup fault'); END"), vec![]).await;
    let receipt = f.call("tool.unregister", json!({"tool":target})).await;
    assert_retired(&receipt, target, 1, 1);
    let degradations = receipt["post_commit_degradations"].as_array().unwrap();
    assert_eq!(degradations.len(), 1, "{receipt}");
    assert_eq!(degradations[0]["stage"], "fts_cleanup");
    assert!(degradations[0]["error"]
        .as_str()
        .unwrap()
        .contains("retirement cleanup fault"));
    assert!(f.entity("local", target).await.deleted_at.is_some());
    assert_eq!(
        f.row("tool_grants", &request).await.text("status").unwrap(),
        "revoked"
    );
    assert_eq!(f.fts_rows(target).await, old_index);
    let listed = f.call("tool.list", json!({})).await;
    assert_eq!(listed["tools"].as_array().unwrap().len(), 1);
    assert_eq!(listed["tools"][0]["full_id"], survivor.to_string());
    let suggested = f
        .call("tool.suggest", json!({"query":"quartzrecord","limit":20}))
        .await;
    assert!(suggested["results"]
        .as_array()
        .unwrap()
        .iter()
        .any(|row| row["full_id"] == survivor.to_string()));
    assert!(!suggested["results"]
        .as_array()
        .unwrap()
        .iter()
        .any(|row| row["full_id"] == target.to_string()));
    f.write("DROP TRIGGER fail_retirement_cleanup", vec![])
        .await;
    let before_retry = f.snapshot().await;
    let retry = f.call("tool.unregister", json!({"tool":target})).await;
    assert_eq!(retry["retired"], false);
    assert_eq!(retry["post_commit_degradations"], json!([]));
    assert_eq!(f.snapshot().await, before_retry);
    assert_eq!(
        f.fts_rows(target).await,
        old_index,
        "tombstone retry does not repeat cleanup"
    );
}

#[derive(Debug)]
struct RetirementGate {
    deny: AtomicBool,
    seen: Mutex<Vec<GateRequest>>,
}

impl Gate for RetirementGate {
    fn check(&self, request: &GateRequest) -> Result<GateDecision, GateError> {
        if request.verb == "tool.unregister" {
            self.seen.lock().unwrap().push(request.clone());
            if self.deny.load(Ordering::SeqCst) {
                return Ok(GateDecision::deny("registry retirement denied"));
            }
        }
        Ok(GateDecision::allow())
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn unregister_catalog_and_write_gate_are_complete() {
    let gate = Arc::new(RetirementGate {
        deny: AtomicBool::new(false),
        seen: Mutex::new(Vec::new()),
    });
    let f = Fixture::with_gate(Some(gate.clone()));
    let target = id(&f.register("local", "gated-tool").await);
    f.policy("local", "agent:policy", "gated-tool", "deny")
        .await;
    let request = f.request("local", "gated-tool", REQUESTER).await;
    f.grant("local", &request).await;
    assert_eq!(
        classify_operation("tool.unregister"),
        Some(OperationAccess::Write)
    );
    let catalog = f.call("verbs", json!({"pack":"tool"})).await;
    let entries = catalog["verbs"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|entry| entry["verb"] == "tool.unregister")
        .collect::<Vec<_>>();
    assert_eq!(entries.len(), 1, "{catalog}");
    let handler = f
        .registry
        .all_verbs()
        .into_iter()
        .find(|handler| handler.name == "tool.unregister")
        .unwrap();
    assert!(
        handler.description.contains("retir"),
        "{}",
        handler.description
    );
    let before = f.snapshot().await;
    for args in [
        json!({}),
        json!({"tool":null}),
        json!({"tool":42}),
        json!({"tool":""}),
        json!({"tool":target,"extra":true}),
        json!({"tool":target,"namespace":42}),
        json!([]),
    ] {
        f.refused("tool.unregister", args).await;
        assert_eq!(f.snapshot().await, before);
    }
    gate.deny.store(true, Ordering::SeqCst);
    let args = json!({"tool":target});
    assert!(matches!(
        f.refused("tool.unregister", args.clone()).await,
        RuntimeError::PermissionDenied { .. }
    ));
    assert_eq!(f.snapshot().await, before);
    assert!(gate
        .seen
        .lock()
        .unwrap()
        .iter()
        .any(|request| request.verb == "tool.unregister" && request.args == args));
    gate.deny.store(false, Ordering::SeqCst);
    assert_retired(&f.call("tool.unregister", args).await, target, 1, 1);
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn unregister_refuses_carried_reserved_properties() {
    for (key, owner) in [
        (
            khive_runtime::secret_gate::RESERVED_SECRET_GATE_KEY,
            "runtime-owned",
        ),
        (
            khive_runtime::secret_gate::RESERVED_WEB_RECEIPT_KEY,
            "web-pack-owned",
        ),
    ] {
        let f = Fixture::new();
        let target = id(&f.register("local", "reserved-retirement").await);
        let survivor = id(&f.register("local", "reserved-survivor").await);
        f.call(
            "create",
            json!({"kind":"observation","content":"retirement refusal history"}),
        )
        .await;
        let policy = f
            .policy("local", "agent:policy", "reserved-retirement", "ask")
            .await;
        f.policy("local", "agent:policy", "reserved-retirement", "deny")
            .await;
        let request = f.request("local", "reserved-retirement", REQUESTER).await;
        f.grant("local", &request).await;
        assert_ne!(
            f.row("tool_policy", &policy).await.text("history").unwrap(),
            "[]"
        );
        assert_eq!(
            f.row("tool_grants", &request).await.text("status").unwrap(),
            "granted"
        );

        let mut legacy = f.entity("local", target).await;
        legacy
            .properties
            .as_mut()
            .unwrap()
            .as_object_mut()
            .unwrap()
            .insert(key.into(), json!({"legacy":true}));
        f.write(
            "UPDATE entities SET properties = ?1, version = version + 1 WHERE id = ?2",
            vec![
                SqlValue::Text(serde_json::to_string(&legacy.properties).unwrap()),
                SqlValue::Text(target.to_string()),
            ],
        )
        .await;
        legacy.version += 1;
        let stored = f.entity("local", target).await;
        assert!(stored.deleted_at.is_none());
        assert_eq!(
            stored.properties.as_ref().unwrap()[key],
            json!({"legacy":true})
        );
        assert_eq!(stored.properties, legacy.properties);
        assert_eq!(
            serde_json::to_value(&stored).unwrap(),
            serde_json::to_value(&legacy).unwrap()
        );
        let before = f.snapshot().await;
        for table in [
            "entities",
            "notes",
            "graph_edges",
            "tool_policy",
            "tool_grants",
        ] {
            assert!(!before[table].as_array().unwrap().is_empty(), "{table}");
        }
        let fts_before = [f.fts_rows(target).await, f.fts_rows(survivor).await];
        assert!(fts_before
            .iter()
            .all(|rows| !rows.as_array().unwrap().is_empty()));
        let events_before = serde_json::to_value(
            f.rows("SELECT * FROM events ORDER BY namespace, id", vec![])
                .await,
        )
        .unwrap();
        assert!(!events_before.as_array().unwrap().is_empty());
        let error = f.refused("tool.unregister", json!({"tool":target})).await;
        let expected = format!(
            "property key `{key}` is {owner} and cannot be created, replaced, merged, or removed by callers"
        );
        assert!(
            matches!(&error, RuntimeError::InvalidInput(message) if message == &expected),
            "{error:?}"
        );
        let after = f.entity("local", target).await;
        assert!(after.deleted_at.is_none());
        assert_eq!(after.properties, stored.properties);
        assert_eq!(
            serde_json::to_value(after).unwrap(),
            serde_json::to_value(stored).unwrap()
        );
        assert_eq!(f.snapshot().await, before);
        assert_eq!(
            [f.fts_rows(target).await, f.fts_rows(survivor).await],
            fts_before
        );
        assert_eq!(
            serde_json::to_value(
                f.rows("SELECT * FROM events ORDER BY namespace, id", vec![])
                    .await,
            )
            .unwrap(),
            events_before
        );
    }
}
