use khive_pack_kg::KgPack;
use khive_pack_tool::ToolPack;
use khive_runtime::engine_config::{GitWriteEntryConfig, GitWriteSectionConfig};
use khive_runtime::{KhiveRuntime, RuntimeConfig, VerbRegistryBuilder};
use khive_storage::types::{SqlStatement, SqlValue};
use serde_json::{json, Value};

use crate::local_handlers::policy_check_count;
use crate::receipts::{self, Disposition, Receipt};
use crate::GitPack;

#[tokio::test]
async fn arm26_gate_precedes_exactly_one_real_policy_decision() {
    for (gate_allows, policy_decision) in [(false, "allow"), (true, "deny"), (true, "allow")] {
        let directory = tempfile::tempdir().expect("repo directory");
        let repo = std::fs::canonicalize(directory.path()).expect("canonical directory");
        let actor = format!("policy-order:{}", uuid::Uuid::new_v4());
        let config = RuntimeConfig {
            git_write: GitWriteSectionConfig {
                allowed: if gate_allows {
                    vec![GitWriteEntryConfig {
                        repo: repo.display().to_string(),
                        branches: vec!["*".into()],
                    }]
                } else {
                    vec![]
                },
                ..Default::default()
            },
            ..Default::default()
        };
        let rt = KhiveRuntime::new(config).expect("runtime");
        let mut builder = VerbRegistryBuilder::new();
        builder.with_actor_id(Some(actor.clone()));
        builder.register(KgPack::new(rt.clone()));
        builder.register(ToolPack::new(rt.clone()));
        builder.register(GitPack::new(rt.clone()));
        builder.with_runtime_event_store(&rt).expect("audit store");
        let registry = builder.build().expect("registry");
        registry.apply_schema_plans(rt.backend());
        rt.install_edge_rules(registry.all_edge_rules());
        let policy = registry
            .dispatch(
                "tool.policy",
                json!({
                    "actor":actor,"tool":"git.reconcile","decision":policy_decision,
                }),
            )
            .await
            .expect("real policy row");
        let policy_id = policy["policy"]["id"].as_str().expect("stored policy id");

        // An unknown receipt without a prospective ref can be observed without
        // Git I/O, isolating the shared gate/policy seam from process behavior.
        let prior = Receipt::new(
            "local",
            &actor,
            "git.commit",
            repo.to_str().unwrap(),
            json!({}),
            json!({"decision":"allow","source":"git_write.allowed","id":0}),
            Value::Null,
        );
        receipts::insert(&rt, &prior)
            .await
            .expect("unknown prior intent");
        assert_eq!(policy_check_count(&actor), 0);
        let outcome = registry
            .dispatch("git.reconcile", json!({"receipt":prior.id}))
            .await;
        assert_eq!(policy_check_count(&actor), usize::from(gate_allows));
        let id = if gate_allows && policy_decision == "allow" {
            outcome.expect("policy allows read")["receipt_id"]
                .as_str()
                .expect("receipt id")
                .to_string()
        } else {
            let error = outcome.expect_err("refusal").to_string();
            error
                .split("receipt_id=")
                .nth(1)
                .expect("receipt id in refusal")
                .chars()
                .take_while(|c| c.is_ascii_hexdigit() || *c == '-')
                .collect()
        };
        let stored = receipts::load_owned(&rt, "local", &actor, &id)
            .await
            .expect("durable decision");
        if gate_allows {
            assert_eq!(
                stored.gate,
                json!({"decision":"allow","source":"git_write.allowed","id":0})
            );
            assert_eq!(
                stored.policy,
                json!({"decision":policy_decision,"source":"policy","id":policy_id})
            );
        } else {
            assert_eq!(stored.gate["decision"], "deny");
            assert!(stored.policy.is_null());
        }
        assert_eq!(
            policy_check_count(&actor),
            usize::from(gate_allows),
            "receipt reads must not add a policy decision"
        );
    }
}

#[tokio::test]
async fn reconcile_corrupt_owned_receipt_reports_storage_uncertainty_without_policy_or_rewrite() {
    async fn raw_row(rt: &KhiveRuntime, id: &str) -> Value {
        let mut reader = rt.sql().reader().await.expect("raw row reader");
        let rows = reader
            .query_all(SqlStatement {
                sql: "SELECT * FROM git_receipts WHERE id = ?1".into(),
                params: vec![SqlValue::Text(id.into())],
                label: Some("git_test_corrupt_receipt_snapshot".into()),
            })
            .await
            .expect("raw receipt snapshot");
        assert_eq!(rows.len(), 1);
        serde_json::to_value(rows).expect("serialize exact stored columns")
    }

    fn receipt_id(error: &str) -> String {
        error
            .split("receipt_id=")
            .nth(1)
            .expect("receipt id in error")
            .chars()
            .take_while(|c| c.is_ascii_hexdigit() || *c == '-')
            .collect()
    }

    let directory = tempfile::tempdir().expect("repo directory");
    let repo = std::fs::canonicalize(directory.path()).expect("canonical directory");
    let actor = format!("corrupt-receipt:{}", uuid::Uuid::new_v4());
    let rt = KhiveRuntime::new(RuntimeConfig {
        git_write: GitWriteSectionConfig {
            allowed: vec![GitWriteEntryConfig {
                repo: repo.display().to_string(),
                branches: vec!["*".into()],
            }],
            ..Default::default()
        },
        ..Default::default()
    })
    .expect("runtime");
    let mut builder = VerbRegistryBuilder::new();
    builder.with_actor_id(Some(actor.clone()));
    builder.register(KgPack::new(rt.clone()));
    builder.register(ToolPack::new(rt.clone()));
    builder.register(GitPack::new(rt.clone()));
    builder.with_runtime_event_store(&rt).expect("audit store");
    let registry = builder.build().expect("registry");
    registry.apply_schema_plans(rt.backend());
    rt.install_edge_rules(registry.all_edge_rules());
    let prior = Receipt::new(
        "local",
        &actor,
        "git.commit",
        repo.to_str().unwrap(),
        json!({}),
        json!({"decision":"allow","source":"git_write.allowed","id":0}),
        Value::Null,
    );
    receipts::insert(&rt, &prior)
        .await
        .expect("owned prior intent");
    receipts::load_owned(&rt, "local", &actor, &prior.id)
        .await
        .expect("valid prior before corruption");
    let mut writer = rt.sql().writer().await.expect("corruption fixture writer");
    let changed = writer
        .execute(SqlStatement {
            // Valid JSON with an invalid receipt shape bypasses neither SQL constraints nor decoder.
            sql: "UPDATE git_receipts SET gate = '[]' WHERE id = ?1".into(),
            params: vec![SqlValue::Text(prior.id.clone())],
            label: Some("git_test_corrupt_owned_receipt".into()),
        })
        .await
        .expect("corrupt stored gate shape");
    assert_eq!(changed, 1);
    drop(writer);
    let original = raw_row(&rt, &prior.id).await;
    let error = registry
        .dispatch("git.reconcile", json!({"receipt":prior.id}))
        .await
        .expect_err("corrupt owned receipt cannot reconcile")
        .to_string();
    assert!(error.contains("receipt_storage"), "{error}");
    assert!(
        !error.contains("receipt_not_found"),
        "corruption must not be hidden as absence: {error}"
    );
    let failure = receipts::load_owned(&rt, "local", &actor, &receipt_id(&error))
        .await
        .expect("durable storage uncertainty");
    assert_eq!(failure.reason.as_deref(), Some("receipt_storage"));
    assert_eq!(failure.disposition, Disposition::Unknown);
    assert!(failure.policy.is_null());
    assert_eq!(policy_check_count(&actor), 0);
    assert_eq!(
        raw_row(&rt, &prior.id).await,
        original,
        "reconcile rewrote corrupt source evidence"
    );

    let foreign = Receipt::new(
        "local",
        "foreign-actor",
        "git.commit",
        repo.to_str().unwrap(),
        json!({}),
        json!({"decision":"allow","source":"git_write.allowed","id":0}),
        Value::Null,
    );
    receipts::insert(&rt, &foreign)
        .await
        .expect("foreign prior intent");
    for id in [uuid::Uuid::new_v4().to_string(), foreign.id] {
        let error = registry
            .dispatch("git.reconcile", json!({"receipt":id}))
            .await
            .expect_err("absent or foreign receipt")
            .to_string();
        assert!(error.contains("receipt_not_found"), "{error}");
        let refusal = receipts::load_owned(&rt, "local", &actor, &receipt_id(&error))
            .await
            .expect("durable not-found refusal");
        assert_eq!(refusal.reason.as_deref(), Some("receipt_not_found"));
        assert_eq!(refusal.disposition, Disposition::NotCommitted);
        assert!(refusal.policy.is_null());
    }
    assert_eq!(policy_check_count(&actor), 0);
    assert_eq!(raw_row(&rt, &prior.id).await, original);
}
