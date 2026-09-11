use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};

use khive_pack_git::GitPack;
use khive_runtime::{
    Gate, GateDecision, GateError, GateRequest, KhiveRuntime, RequestIdentity, VerbRegistry,
    VerbRegistryBuilder,
};
use khive_storage::types::{SqlStatement, SqlValue};
use serde_json::{json, Value};

#[derive(Debug, Default)]
struct InspectGate {
    deny_get: AtomicBool,
    deny_get_namespace: Mutex<Option<String>>,
    seen: Mutex<Vec<GateRequest>>,
}

impl Gate for InspectGate {
    fn check(&self, request: &GateRequest) -> Result<GateDecision, GateError> {
        self.seen.lock().unwrap().push(request.clone());
        Ok(
            if request.verb == "get"
                && (self.deny_get.load(Ordering::SeqCst)
                    || self.deny_get_namespace.lock().unwrap().as_deref()
                        == Some(request.namespace.as_str()))
            {
                GateDecision::deny("project read denied")
            } else {
                GateDecision::allow()
            },
        )
    }
}

async fn fixture() -> (KhiveRuntime, VerbRegistry, Arc<InspectGate>, String) {
    fixture_in_namespace("local").await
}

async fn fixture_in_namespace(
    namespace: &str,
) -> (KhiveRuntime, VerbRegistry, Arc<InspectGate>, String) {
    let runtime = KhiveRuntime::memory().unwrap();
    let gate = Arc::new(InspectGate::default());
    let mut builder = VerbRegistryBuilder::new();
    builder.with_default_namespace(namespace);
    builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
    builder.register(GitPack::new(runtime.clone()));
    builder.with_gate(gate.clone());
    builder.with_runtime_event_store(&runtime).unwrap();
    let registry = builder.build().unwrap();
    runtime.install_edge_rules(registry.all_edge_rules());
    registry.apply_schema_plans(runtime.backend());
    let project = registry
        .dispatch(
            "create",
            json!({"kind": "project", "name": "cursor fixture"}),
        )
        .await
        .unwrap();
    (
        runtime,
        registry,
        gate,
        project["id"].as_str().unwrap().to_owned(),
    )
}

async fn store_pair(
    runtime: &KhiveRuntime,
    project: &str,
    kind: &str,
    cursor: SqlValue,
    progress: &str,
) {
    runtime.sql().writer().await.unwrap().execute(SqlStatement {
        sql: "INSERT INTO git_mirror_cursor(project_id, kind, cursor_value, updated_at) VALUES (?1, ?2, ?3, 42), (?1, ?4, ?5, 43) ON CONFLICT(project_id,kind) DO UPDATE SET cursor_value=excluded.cursor_value, updated_at=excluded.updated_at".into(),
        params: vec![SqlValue::Text(project.into()), SqlValue::Text(kind.into()), cursor,
            SqlValue::Text(format!("{kind}_checkpoint")), SqlValue::Text(progress.into())],
        label: None,
    }).await.unwrap();
}

async fn inspect(registry: &VerbRegistry, project: &str, source_kind: &str) -> Value {
    registry
        .dispatch(
            "git.ingest_cursor",
            json!({"project": project, "source_kind": source_kind}),
        )
        .await
        .unwrap()
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn absent_cursor_read_does_not_initialize_rows() {
    let (runtime, registry, _, project) = fixture().await;
    let expected =
        json!({"project_id": project, "source_kind": "issues", "cursor": null, "checkpoint": null});
    assert_eq!(inspect(&registry, &project, "issues").await, expected);
    assert_eq!(inspect(&registry, &project, "issues").await, expected);
    let count = runtime
        .sql()
        .reader()
        .await
        .unwrap()
        .query_scalar(SqlStatement {
            sql: "SELECT COUNT(*) FROM git_mirror_cursor".into(),
            params: vec![],
            label: None,
        })
        .await
        .unwrap();
    assert!(matches!(count, Some(SqlValue::Integer(0))));
    assert_eq!(
        registry.presentation_policy_for("git.ingest_cursor"),
        khive_types::VerbPresentationPolicy::AlwaysVerbose
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn all_source_kinds_preserve_exact_cursor_checkpoint_and_update_times() {
    let (runtime, registry, _, project) = fixture().await;
    for (source_kind, stored_kind, value) in [
        ("commits", "commits", "a".repeat(40)),
        ("issues", "issues", "2026-09-01T12:01:02+00:00".into()),
        ("pull_requests", "prs", "2026-09-02T12:01:02Z".into()),
    ] {
        let progress = if source_kind == "commits" {
            json!({"version":1,"namespace":"source-attribution","base_cursor":null,
                "snapshot_head":"b".repeat(40),"last_completed_sha":value})
            .to_string()
        } else {
            json!({"version":1,"namespace":"source-attribution","floor":value,
                "at_floor":{"7":"00000000-0000-0000-0000-000000000007"},"undated":{}})
            .to_string()
        };
        store_pair(
            &runtime,
            &project,
            stored_kind,
            SqlValue::Text(value.clone()),
            &progress,
        )
        .await;
        let result = inspect(&registry, &project, source_kind).await;
        assert_eq!(result["project_id"], project);
        assert_eq!(result["source_kind"], source_kind);
        assert_eq!(
            result["cursor"],
            json!({"value":value, "value_bytes":value.len(), "updated_at":42, "truncated":false})
        );
        assert_eq!(
            result["checkpoint"],
            json!({"value":progress, "value_bytes":progress.len(), "updated_at":43, "truncated":false})
        );
        assert_eq!(inspect(&registry, &project, source_kind).await, result);
    }
    let other = registry
        .dispatch("create", json!({"kind":"project","name":"other anchor"}))
        .await
        .unwrap();
    assert!(inspect(&registry, other["id"].as_str().unwrap(), "issues").await["cursor"].is_null());
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn inspection_preserves_null_invalid_and_oversized_checkpoint_states() {
    let (runtime, registry, _, project) = fixture().await;
    store_pair(
        &runtime,
        &project,
        "issues",
        SqlValue::Null,
        "invalid checkpoint JSON 文",
    )
    .await;
    let result = inspect(&registry, &project, "issues").await;
    assert_eq!(
        result["cursor"],
        json!({"value":null,"value_bytes":null,"updated_at":42,"truncated":false})
    );
    assert_eq!(result["checkpoint"]["value"], "invalid checkpoint JSON 文");
    let boundary = "x".repeat(256 * 1024);
    store_pair(&runtime, &project, "issues", SqlValue::Null, &boundary).await;
    let result = inspect(&registry, &project, "issues").await;
    assert_eq!(result["checkpoint"]["value"], boundary);
    assert_eq!(result["checkpoint"]["truncated"], false);
    let oversized = "文".repeat(100_000);
    store_pair(
        &runtime,
        &project,
        "issues",
        SqlValue::Text("invalid timestamp".into()),
        &oversized,
    )
    .await;
    let result = inspect(&registry, &project, "issues").await;
    assert_eq!(result["cursor"]["value"], "invalid timestamp");
    assert_eq!(
        result["checkpoint"],
        json!({"value":null,"value_bytes":300_000,"updated_at":43,"truncated":true})
    );
    assert!(result.to_string().len() < 1024);
    assert_eq!(inspect(&registry, &project, "issues").await, result);
    runtime
        .sql()
        .writer()
        .await
        .unwrap()
        .execute(SqlStatement {
            sql: "DELETE FROM git_mirror_cursor WHERE project_id=?1 AND kind='issues_checkpoint'"
                .into(),
            params: vec![SqlValue::Text(project.clone())],
            label: None,
        })
        .await
        .unwrap();
    let legacy = inspect(&registry, &project, "issues").await;
    assert_eq!(legacy["cursor"], result["cursor"]);
    assert!(legacy["checkpoint"].is_null());
    runtime
        .sql()
        .writer()
        .await
        .unwrap()
        .execute(SqlStatement {
            sql: "UPDATE git_mirror_cursor SET cursor_value=?2 WHERE project_id=?1".into(),
            params: vec![
                SqlValue::Text(project.clone()),
                SqlValue::Blob(vec![0; 262_145]),
            ],
            label: None,
        })
        .await
        .unwrap();
    let error = registry
        .dispatch(
            "git.ingest_cursor",
            json!({"project":project,"source_kind":"issues"}),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("invalid column types"));
    runtime
        .sql()
        .writer()
        .await
        .unwrap()
        .execute(SqlStatement {
            sql:
                "UPDATE git_mirror_cursor SET cursor_value=CAST(X'80' AS TEXT) WHERE project_id=?1"
                    .into(),
            params: vec![SqlValue::Text(project.clone())],
            label: None,
        })
        .await
        .unwrap();
    let error = registry
        .dispatch(
            "git.ingest_cursor",
            json!({"project":project,"source_kind":"issues"}),
        )
        .await
        .unwrap_err();
    assert!(error
        .to_string()
        .contains("invalid column types or text encoding"));
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn project_get_gate_receives_request_identity_and_denial_is_preserved() {
    let (runtime, registry, gate, project) = fixture().await;
    store_pair(
        &runtime,
        &project,
        "issues",
        SqlValue::Text("2026-09-01T00:00:00Z".into()),
        "{}",
    )
    .await;
    gate.seen.lock().unwrap().clear();
    let params = json!({"project":project,"source_kind":"issues","namespace":"read-context"});
    let identity = RequestIdentity {
        namespace: "identity-default".into(),
        actor_id: Some("cursor-reader".into()),
        ..Default::default()
    };
    let response = registry
        .dispatch_with_identity("git.ingest_cursor", params.clone(), Some(identity.clone()))
        .await
        .unwrap();
    assert_eq!(response["cursor"]["updated_at"], 42);
    {
        let requests = gate.seen.lock().unwrap();
        for verb in ["git.ingest_cursor", "get"] {
            let request = requests.iter().find(|r| r.verb == verb).unwrap();
            assert_eq!(request.actor.id, "cursor-reader");
            assert_eq!(request.namespace.as_str(), "read-context");
        }
    }
    gate.deny_get.store(true, Ordering::SeqCst);
    let error = registry
        .dispatch_with_identity("git.ingest_cursor", params, Some(identity))
        .await
        .unwrap_err();
    assert!(
        matches!(error, khive_runtime::RuntimeError::PermissionDenied { .. }),
        "{error:?}"
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn implicit_gate_namespace_denial_matches_direct_get() {
    for use_request_identity in [false, true] {
        let default = if use_request_identity {
            "local"
        } else {
            "restricted"
        };
        let (_, registry, gate, project) = fixture_in_namespace(default).await;
        *gate.deny_get_namespace.lock().unwrap() = Some("restricted".into());
        let identity = use_request_identity.then(|| RequestIdentity {
            namespace: "restricted".into(),
            actor_id: Some("cursor-reader".into()),
            ..Default::default()
        });
        for (verb, params) in [
            ("get", json!({"id":project})),
            (
                "git.ingest_cursor",
                json!({"project":project,"source_kind":"issues"}),
            ),
        ] {
            gate.seen.lock().unwrap().clear();
            let error = registry
                .dispatch_with_identity(verb, params, identity.clone())
                .await
                .unwrap_err();
            assert!(
                matches!(error, khive_runtime::RuntimeError::PermissionDenied { .. }),
                "{error:?}"
            );
            let requests = gate.seen.lock().unwrap();
            assert!(requests.iter().any(|r| r.verb == "get"));
            assert!(requests
                .iter()
                .all(|r| r.namespace.as_str() == "restricted"));
        }
        gate.seen.lock().unwrap().clear();
        let result = registry
            .dispatch_with_identity(
                "git.ingest_cursor",
                json!({"project":project,"source_kind":"issues","namespace":"allowed"}),
                identity,
            )
            .await
            .unwrap();
        assert!(result["cursor"].is_null());
        let requests = gate.seen.lock().unwrap();
        assert!(requests.iter().any(|r| r.verb == "get"));
        assert!(requests.iter().all(|r| r.namespace.as_str() == "allowed"));
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn invalid_selector_wrong_kind_missing_and_deleted_projects_refuse() {
    let (_, registry, _, project) = fixture().await;
    for params in [
        json!({"project":&project[..8],"source_kind":"issues"}),
        json!({"project":project,"source_kind":"prs"}),
        json!({"project":project,"source_kind":false}),
        json!({"project":project}),
        json!({"project":project,"source_kind":"issues","reset":true}),
        json!({"project":"00000000-0000-0000-0000-000000000007","source_kind":"issues"}),
    ] {
        assert!(registry
            .dispatch("git.ingest_cursor", params)
            .await
            .is_err());
    }
    let entity = registry
        .dispatch("create", json!({"kind":"concept","name":"not a project"}))
        .await
        .unwrap();
    assert!(registry
        .dispatch(
            "git.ingest_cursor",
            json!({"project":entity["id"],"source_kind":"issues"})
        )
        .await
        .is_err());
    registry
        .dispatch("delete", json!({"id":project}))
        .await
        .unwrap();
    assert!(registry
        .dispatch(
            "git.ingest_cursor",
            json!({"project":project,"source_kind":"issues"})
        )
        .await
        .is_err());
}
