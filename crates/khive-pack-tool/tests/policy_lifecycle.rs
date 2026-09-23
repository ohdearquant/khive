use khive_pack_kg::KgPack;
use khive_pack_tool::ToolPack;
use khive_runtime::{KhiveRuntime, VerbRegistry, VerbRegistryBuilder};
use khive_storage::{SqlStatement, SqlValue};
use khive_types::Pack;
use serde_json::{json, Value};

struct Fixture {
    rt: KhiveRuntime,
    registry: VerbRegistry,
}

impl Fixture {
    fn new(apply_schema: bool) -> Self {
        let rt = KhiveRuntime::memory().unwrap();
        let mut builder = VerbRegistryBuilder::new();
        builder.with_actor_id(Some("policy-author".into()));
        builder.register(KgPack::new(rt.clone()));
        builder.register(ToolPack::new(rt.clone()));
        let registry = builder.build().unwrap();
        if apply_schema {
            registry
                .apply_schema_plans_with_map(&Default::default(), rt.backend())
                .unwrap();
        }
        Self { rt, registry }
    }

    fn upgrade(&self) -> Result<(), khive_runtime::PackSchemaCollisionError> {
        self.registry
            .apply_schema_plans_with_map(&Default::default(), self.rt.backend())
    }

    async fn call(&self, verb: &str, args: Value) -> Value {
        self.registry.dispatch(verb, args).await.unwrap()
    }

    async fn error(&self, verb: &str, args: Value) -> String {
        self.registry
            .dispatch(verb, args)
            .await
            .unwrap_err()
            .to_string()
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

    async fn scalar(&self, sql: &str) -> i64 {
        match self
            .rt
            .sql()
            .reader()
            .await
            .unwrap()
            .query_scalar(SqlStatement {
                sql: sql.into(),
                params: vec![],
                label: None,
            })
            .await
            .unwrap()
        {
            Some(SqlValue::Integer(value)) => value,
            other => panic!("expected integer scalar, got {other:?}"),
        }
    }

    async fn policy(&self, actor: &str, tool: &str, decision: &str) -> Value {
        self.call(
            "tool.policy",
            json!({"actor":actor, "tool":tool, "decision":decision}),
        )
        .await["policy"]
            .clone()
    }

    async fn check(&self, actor: &str, tool: &str) -> Value {
        self.call("tool.check", json!({"actor":actor, "tool":tool}))
            .await
    }
}

#[tokio::test]
async fn deny_correction_requires_the_current_row_id_and_retains_history() {
    let f = Fixture::new(true);
    let deny = f
        .call(
            "tool.policy",
            json!({"actor":"svc:a", "tool":"t.x", "decision":"deny", "note":"initial restriction"}),
        )
        .await["policy"]
        .clone();
    let id = deny["id"].as_str().unwrap();
    for replaces in [None, Some("00000000-0000-0000-0000-000000000099")] {
        let mut args = json!({"actor":"svc:a", "tool":"t.x", "decision":"allow"});
        if let Some(replaces) = replaces {
            args["replaces"] = json!(replaces);
        }
        let error = f.error("tool.policy", args).await;
        assert!(error.contains(id), "{error}");
        let check = f.check("svc:a", "t.x").await;
        assert_eq!(check["decision"], "deny");
        assert_eq!(check["policy_id"], id);
        assert_eq!(
            f.call("tool.policies", json!({})).await["policies"],
            json!([deny])
        );
    }
    let allow = f.call("tool.policy", json!({"actor":"svc:a", "tool":"t.x", "decision":"allow", "note":"restriction lifted", "replaces":id})).await["policy"].clone();
    assert_eq!(allow["id"], deny["id"]);
    assert_eq!(allow["created_at"], deny["created_at"]);
    assert_ne!(allow["updated_at"], deny["updated_at"]);
    assert_eq!(allow["history"].as_array().unwrap().len(), 1);
    assert_eq!(allow["history"][0]["decision"], "deny");
    assert_eq!(allow["history"][0]["note"], "initial restriction");
    assert_eq!(allow["history"][0]["author"], "policy-author");
    assert!(allow["history"][0]["timestamp"].is_i64());
    let check = f.check("svc:a", "t.x").await;
    assert_eq!(check["decision"], "allow");
    assert_eq!(check["policy_id"], id);
    let error = f
        .error(
            "tool.policy",
            json!({"actor":"svc:a", "tool":"t.x", "decision":"ask", "replaces":"stale-id"}),
        )
        .await;
    assert!(error.contains(id), "{error}");
    assert_eq!(
        f.call("tool.policies", json!({})).await["policies"],
        json!([allow])
    );
}

#[tokio::test]
async fn identical_policy_refuses_but_note_only_correction_preserves_the_deny() {
    let f = Fixture::new(true);
    let deny = f.policy("svc:a", "t.x", "deny").await;
    let error = f
        .error(
            "tool.policy",
            json!({"actor":"svc:a", "tool":"t.x", "decision":"deny"}),
        )
        .await;
    assert!(error.contains("policy_unchanged"), "{error}");
    assert!(error.contains(deny["id"].as_str().unwrap()), "{error}");
    assert_eq!(
        f.call("tool.policies", json!({})).await["policies"],
        json!([deny])
    );
    let corrected = f
        .call(
            "tool.policy",
            json!({"actor":"svc:a", "tool":"t.x", "decision":"deny", "note":"corrected rationale"}),
        )
        .await["policy"]
        .clone();
    assert_eq!(corrected["id"], deny["id"]);
    assert_ne!(corrected["updated_at"], deny["updated_at"]);
    assert_eq!(corrected["history"][0]["decision"], "deny");
    assert!(corrected["history"][0]["note"].is_null());
    assert_eq!(f.check("svc:a", "t.x").await["decision"], "deny");
}

#[tokio::test]
async fn policy_delete_matches_exact_labels_and_retired_rows_remain_readable() {
    let f = Fixture::new(true);
    let fallback = f.policy("*", "t.x", "ask").await;
    let prefix = f.policy("svc:*", "t.x", "deny").await;
    let exact = f.policy("svc:a", "t.x", "allow").await;
    let error = f
        .error(
            "tool.policy_delete",
            json!({"actor":"svc:*", "tool":"t.x", "namespace":"elsewhere"}),
        )
        .await;
    assert!(error.contains("no live tool policy"), "{error}");
    assert_eq!(f.check("svc:b", "t.x").await["policy_id"], prefix["id"]);
    let deleted = f
        .call("tool.policy_delete", json!({"actor":"svc:*", "tool":"t.x"}))
        .await["policy"]
        .clone();
    assert_eq!(deleted["id"], prefix["id"]);
    assert!(deleted["deleted_at"].is_string());
    assert_eq!(deleted["deleted_by"], "policy-author");
    assert_eq!(f.check("svc:a", "t.x").await["policy_id"], exact["id"]);
    assert_eq!(f.check("svc:b", "t.x").await["policy_id"], fallback["id"]);
    let error = f
        .error("tool.policy_delete", json!({"actor":"svc:*", "tool":"t.x"}))
        .await;
    assert!(error.contains("no live tool policy"), "{error}");
    let error = f
        .error("tool.policy_delete", json!({"actor":"svc:*", "tool":"t.*"}))
        .await;
    assert!(error.contains("no live tool policy"), "{error}");
    let listed = f.call("tool.policies", json!({})).await;
    assert!(listed["policies"].as_array().unwrap().contains(&deleted));
    let recreated = f.policy("svc:*", "t.x", "ask").await;
    assert_ne!(recreated["id"], prefix["id"]);
    assert_eq!(f.check("svc:b", "t.x").await["policy_id"], recreated["id"]);
    let error = f
        .error(
            "tool.policy",
            json!({"actor":"svc:*", "tool":"t.x", "decision":"allow", "replaces":prefix["id"]}),
        )
        .await;
    assert!(error.contains(recreated["id"].as_str().unwrap()), "{error}");
}

#[tokio::test]
async fn policy_history_does_not_participate_in_decisions() {
    let f = Fixture::new(true);
    let deny = f.policy("svc:a", "t.x", "deny").await;
    f.write(
        "UPDATE tool_policy SET history = 'invalid history' WHERE actor = 'svc:a'",
        vec![],
    )
    .await;
    let check = f.check("svc:a", "t.x").await;
    assert_eq!(check["decision"], "deny");
    assert_eq!(check["policy_id"], deny["id"]);
}

#[tokio::test]
async fn equal_specificity_patterns_still_prefer_deny_over_later_allow() {
    let f = Fixture::new(true);
    let deny = f.policy("svc:*", "t.x", "deny").await;
    f.policy("svc:a", "t.*", "allow").await;
    let check = f.check("svc:a", "t.x").await;
    assert_eq!(check["decision"], "deny");
    assert_eq!(check["policy_id"], deny["id"]);
}

#[tokio::test]
async fn concurrent_policy_corrections_keep_one_live_row_and_both_values() {
    let f = Fixture::new(true);
    let (first, second) = tokio::join!(
        f.policy("svc:a", "t.x", "allow"),
        f.policy("svc:a", "t.x", "ask"),
    );
    assert_eq!(first["id"], second["id"]);
    let listed = f.call("tool.policies", json!({})).await;
    assert_eq!(listed["count"], 1);
    let live = &listed["policies"][0];
    assert_eq!(live["history"].as_array().unwrap().len(), 1);
    assert_ne!(live["history"][0]["decision"], live["decision"]);
    assert_eq!(
        f.scalar("SELECT COUNT(*) FROM tool_policy WHERE deleted_at IS NULL")
            .await,
        1
    );
}

#[tokio::test]
async fn legacy_consolidation_preserves_the_deciding_id_and_marks_upgrade_history() {
    let f = Fixture::new(false);
    // Nullable additions alone leave the legacy append-only decisions intact.
    let plan = <ToolPack as Pack>::SCHEMA_PLAN.unwrap();
    f.rt.backend()
        .apply_pack_ddl_statements(&plan.statements[..4])
        .unwrap();
    for (id, actor, decision, created_at) in [
        ("deny-first", "denied", "deny", 1),
        ("allow-later", "denied", "allow", 2),
        ("allow-first", "allowed", "allow", 1),
        ("allow-second", "allowed", "allow", 2),
    ] {
        f.write("INSERT INTO tool_policy (id, namespace, actor, tool, decision, note, created_at, created_by) VALUES (?1, 'local', ?2, 't.x', ?3, ?1, ?4, 'legacy-author')", vec![SqlValue::Text(id.into()), SqlValue::Text(actor.into()), SqlValue::Text(decision.into()), SqlValue::Integer(created_at)]).await;
    }
    let before_deny = f.check("denied", "t.x").await;
    let before_allow = f.check("allowed", "t.x").await;
    assert_eq!(before_deny["decision"], "deny");
    assert_eq!(before_deny["policy_id"], "deny-first");
    assert_eq!(before_allow["decision"], "allow");
    assert_eq!(before_allow["policy_id"], "allow-first");
    f.upgrade().unwrap();
    for (actor, before) in [("denied", before_deny), ("allowed", before_allow)] {
        let after = f.check(actor, "t.x").await;
        assert_eq!(after["decision"], before["decision"]);
        assert_eq!(after["policy_id"], before["policy_id"]);
    }
    let listed = f.call("tool.policies", json!({})).await;
    let policies = listed["policies"].as_array().unwrap();
    assert_eq!(policies.len(), 4);
    for (live_id, retired_id) in [
        ("deny-first", "allow-later"),
        ("allow-first", "allow-second"),
    ] {
        let live = policies.iter().find(|row| row["id"] == live_id).unwrap();
        assert!(live["deleted_at"].is_null());
        assert_eq!(live["history"].as_array().unwrap().len(), 1);
        assert_eq!(live["history"][0]["id"], retired_id);
        assert_eq!(live["history"][0]["reason"], "legacy_consolidation");
        assert_eq!(live["history"][0]["author"], "legacy-author");
        assert_eq!(live["history"][0]["timestamp"], 2);
        assert!(live["history"][0]["consolidated_at"].is_i64());
        let retired = policies.iter().find(|row| row["id"] == retired_id).unwrap();
        assert!(retired["deleted_at"].is_string());
    }
    f.upgrade().unwrap();
    assert_eq!(f.call("tool.policies", json!({})).await, listed);
    assert_eq!(
        f.scalar("SELECT COUNT(*) FROM tool_policy WHERE deleted_at IS NULL")
            .await,
        2
    );
}

const LEGACY_POLICY: &str = "CREATE TABLE tool_policy (
    id TEXT PRIMARY KEY,
    namespace TEXT NOT NULL,
    actor TEXT NOT NULL,
    tool TEXT NOT NULL,
    decision TEXT NOT NULL,
    note TEXT,
    created_at INTEGER NOT NULL,
    created_by TEXT
);";

#[tokio::test]
async fn legacy_policy_schema_upgrade_is_atomic_and_adds_the_live_uniqueness_guard() {
    let f = Fixture::new(false);
    f.rt.backend()
        .apply_pack_ddl_statements(&[LEGACY_POLICY])
        .unwrap();
    for (id, decision, timestamp) in [("old-deny", "deny", 1), ("new-allow", "allow", 2)] {
        f.write("INSERT INTO tool_policy (id, namespace, actor, tool, decision, created_at, created_by) VALUES (?1, 'local', 'svc:a', 't.x', ?2, ?3, 'legacy-author')", vec![SqlValue::Text(id.into()), SqlValue::Text(decision.into()), SqlValue::Integer(timestamp)]).await;
    }
    f.rt.backend().apply_pack_ddl_statements(&["CREATE TRIGGER refuse_policy_upgrade BEFORE UPDATE ON tool_policy BEGIN SELECT RAISE(ABORT, 'policy upgrade refused'); END;"]).unwrap();
    let error = f.upgrade().unwrap_err().to_string();
    assert!(error.contains("policy upgrade refused"), "{error}");
    assert_eq!(
        f.scalar("SELECT COUNT(*) FROM pragma_table_info('tool_policy') WHERE name = 'history'")
            .await,
        0
    );
    assert_eq!(f.scalar("SELECT COUNT(*) FROM tool_policy").await, 2);
    f.rt.backend()
        .apply_pack_ddl_statements(&["DROP TRIGGER refuse_policy_upgrade"])
        .unwrap();
    f.upgrade().unwrap();
    assert_eq!(f.check("svc:a", "t.x").await["policy_id"], "old-deny");
    let duplicate = f.rt.sql().writer().await.unwrap().execute(SqlStatement {
        sql: "INSERT INTO tool_policy (id, namespace, actor, tool, decision, created_at) VALUES ('duplicate', 'local', 'svc:a', 't.x', 'allow', 3)".into(),
        params: vec![], label: None,
    }).await.unwrap_err();
    assert!(duplicate.to_string().contains("UNIQUE"), "{duplicate}");
}
