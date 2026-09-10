//! Policy authority follows the registered ToolPack's backend, including when
//! the GitPack's database still contains contradictory policy rows. Replacing
//! the registry policy lookup with a direct lookup on the git runtime must
//! break both the deny and allow cases below.

use std::path::Path;

use khive_pack_kg::KgPack;
use khive_pack_tool::ToolPack;
use khive_runtime::engine_config::{GitWriteEntryConfig, GitWriteSectionConfig};
use khive_runtime::{
    KhiveRuntime, RequestIdentity, RuntimeConfig, VerbRegistry, VerbRegistryBuilder,
};
use khive_storage::types::SqlStatement;
use khive_types::Pack;
use serde_json::{json, Value};

use crate::receipts::{self, Disposition, Receipt};
use crate::GitPack;

const NAMESPACE: &str = "backend-policy-tenant";
const BAKED_ACTOR: &str = "backend-policy-daemon";

async fn apply_pack_schema<P: Pack>(runtime: &KhiveRuntime) {
    if let Some(plan) = P::SCHEMA_PLAN {
        let mut writer = runtime.sql().writer().await.expect("schema writer");
        for statement in plan.statements {
            writer
                .execute(SqlStatement {
                    sql: (*statement).to_string(),
                    params: vec![],
                    label: Some(format!("{}_backend_policy_test_schema", plan.pack)),
                })
                .await
                .expect("pack-owned schema on its assigned backend");
        }
    }
}

fn file_runtime(path: &Path, git_write: GitWriteSectionConfig) -> KhiveRuntime {
    KhiveRuntime::new(RuntimeConfig {
        db_path: Some(path.to_path_buf()),
        git_write,
        ..Default::default()
    })
    .expect("independent file-backed runtime")
}

fn registry(
    core_runtime: &KhiveRuntime,
    tool_runtime: &KhiveRuntime,
    git_runtime: Option<&KhiveRuntime>,
) -> VerbRegistry {
    let mut builder = VerbRegistryBuilder::new();
    // Neither of these defaults is the request's resolved caller or explicit
    // namespace. A nested bare dispatch must not accidentally satisfy the test.
    builder.with_actor_id(Some(BAKED_ACTOR.into()));
    builder.with_default_namespace("backend-policy-daemon-default");
    builder.register(KgPack::new(core_runtime.clone()));
    builder.register(ToolPack::new(tool_runtime.clone()));
    if let Some(runtime) = git_runtime {
        builder.register(GitPack::new(runtime.clone()));
    }
    builder
        .with_runtime_event_store(core_runtime)
        .expect("independent core audit store");
    let registry = builder.build().expect("registered pack runtimes");
    core_runtime.install_edge_rules(registry.all_edge_rules());
    registry
}

async fn seed_policy(
    registry: &VerbRegistry,
    identity: &RequestIdentity,
    actor: &str,
    decision: &str,
) -> String {
    let response = registry
        .dispatch_with_identity(
            "tool.policy",
            json!({
                "namespace": NAMESPACE,
                "actor": actor,
                "tool": "git.reconcile",
                "decision": decision,
            }),
            Some(identity.clone()),
        )
        .await
        .expect("seed through the real ToolPack policy handler");
    response["policy"]["id"]
        .as_str()
        .expect("stored policy id")
        .to_string()
}

async fn assert_tool_decision(
    registry: &VerbRegistry,
    identity: &RequestIdentity,
    actor: &str,
    expected_decision: &str,
    expected_id: &str,
) {
    // Deliberately omit actor: the ToolPack must resolve the request identity.
    let decision = registry
        .dispatch_with_identity(
            "tool.check",
            json!({"namespace": NAMESPACE, "tool": "git.reconcile"}),
            Some(identity.clone()),
        )
        .await
        .expect("real ToolPack decision");
    assert_eq!(decision["actor"], actor);
    assert_eq!(decision["decision"], expected_decision);
    assert_eq!(decision["source"], "policy");
    assert_eq!(decision["policy_id"], expected_id);
}

async fn authoritative_tool_backend_case(stale_decision: &str, current_decision: &str) {
    let directory = tempfile::tempdir().expect("fixture directory");
    let repo = directory.path().join("repo");
    std::fs::create_dir(&repo).expect("repository gate directory");
    let repo = std::fs::canonicalize(repo).expect("canonical repository gate directory");
    let actor = format!("backend-policy-caller:{}", uuid::Uuid::new_v4());
    let identity = RequestIdentity {
        namespace: "backend-policy-request-default".into(),
        actor_id: Some(actor.clone()),
        visible_namespaces: vec!["backend-policy-unrelated".into()],
        ..Default::default()
    };
    let core_runtime = file_runtime(&directory.path().join("core.db"), Default::default());
    let git_runtime = file_runtime(
        &directory.path().join("git.db"),
        GitWriteSectionConfig {
            allowed: vec![GitWriteEntryConfig {
                repo: repo.display().to_string(),
                branches: vec!["*".into()],
            }],
            ..Default::default()
        },
    );
    let tool_runtime = file_runtime(&directory.path().join("tool.db"), Default::default());
    apply_pack_schema::<KgPack>(&core_runtime).await;
    apply_pack_schema::<GitPack>(&git_runtime).await;
    apply_pack_schema::<ToolPack>(&tool_runtime).await;
    // Simulate a tool backend migration that left old tables behind. Use the
    // pack's actual schema and handler, not a hand-written policy row or DDL.
    apply_pack_schema::<ToolPack>(&git_runtime).await;
    let stale_registry = registry(&core_runtime, &git_runtime, None);
    let registry = registry(&core_runtime, &tool_runtime, Some(&git_runtime));
    let stale_id = seed_policy(&stale_registry, &identity, &actor, stale_decision).await;
    let current_id = seed_policy(&registry, &identity, &actor, current_decision).await;
    assert_ne!(stale_id, current_id);
    assert_tool_decision(
        &stale_registry,
        &identity,
        &actor,
        stale_decision,
        &stale_id,
    )
    .await;
    assert_tool_decision(&registry, &identity, &actor, current_decision, &current_id).await;

    // No prospective ref/SHA means reconcile observes this unknown intent
    // without spawning Git. The ordinary directory above needs no git init.
    let prior = Receipt::new(
        NAMESPACE,
        &actor,
        "git.commit",
        repo.to_str().expect("fixture path UTF-8"),
        json!({}),
        json!({"decision": "allow", "source": "git_write.allowed", "id": 0}),
        Value::Null,
    );
    receipts::insert(&git_runtime, &prior)
        .await
        .expect("intent belongs to the GitPack backend");
    let result = registry
        .dispatch_with_identity(
            "git.reconcile",
            json!({"namespace": NAMESPACE, "receipt": prior.id}),
            Some(identity.clone()),
        )
        .await;

    let page = receipts::list_owned(&git_runtime, NAMESPACE, &actor, None, None, 10, 0)
        .await
        .expect("caller-owned receipts in the explicit namespace");
    assert_eq!(
        page.receipts.len(),
        2,
        "one prior intent and one reconcile receipt"
    );
    assert!(page.next_offset.is_none());
    let stored = page
        .receipts
        .iter()
        .find(|receipt| receipt.verb == "git.reconcile")
        .expect("durable root git operation");
    assert_eq!(stored.namespace, NAMESPACE);
    assert_eq!(stored.actor, actor);
    assert_eq!(
        stored.gate,
        json!({"decision": "allow", "source": "git_write.allowed", "id": 0})
    );
    assert_eq!(
        stored.policy,
        json!({"decision": current_decision, "source": "policy", "id": current_id})
    );
    assert_ne!(stored.policy["id"], stale_id);
    if current_decision == "allow" {
        let response = result.expect("authoritative allow overrides stale deny");
        assert_eq!(response["receipt_id"], stored.id);
        assert_eq!(response["receipt"]["id"], prior.id);
        assert_eq!(response["receipt"]["disposition"], "unknown");
        assert_eq!(stored.disposition, Disposition::Committed);
        assert!(stored.reason.is_none());
    } else {
        let error = result
            .expect_err("authoritative deny overrides stale allow")
            .to_string();
        assert!(error.contains("policy_denied"), "{error}");
        assert!(
            error.contains(&format!("receipt_id={}", stored.id)),
            "{error}"
        );
        assert_eq!(stored.disposition, Disposition::NotCommitted);
        assert_eq!(stored.reason.as_deref(), Some("policy_denied"));
    }
    let prior_after = receipts::load_owned(&git_runtime, NAMESPACE, &actor, &prior.id)
        .await
        .expect("original intent remains owned by the resolved caller");
    assert_eq!(prior_after.disposition, Disposition::Unknown);
    assert!(prior_after.result.is_null());
    assert_tool_decision(
        &stale_registry,
        &identity,
        &actor,
        stale_decision,
        &stale_id,
    )
    .await;
    assert_tool_decision(&registry, &identity, &actor, current_decision, &current_id).await;
    assert!(
        receipts::load_owned(&git_runtime, "local", &actor, &stored.id)
            .await
            .is_err()
    );
    assert!(
        receipts::load_owned(&git_runtime, NAMESPACE, BAKED_ACTOR, &stored.id)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn authoritative_tool_backend_deny_overrides_stale_git_backend_allow() {
    authoritative_tool_backend_case("allow", "deny").await;
}

#[tokio::test]
async fn authoritative_tool_backend_allow_overrides_stale_git_backend_deny() {
    authoritative_tool_backend_case("deny", "allow").await;
}
