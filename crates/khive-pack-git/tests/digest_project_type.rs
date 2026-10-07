use khive_pack_git::GitPack;
use khive_pack_kg::KgPack;
use khive_runtime::{KhiveRuntime, RuntimeConfig, RuntimeError, VerbRegistry, VerbRegistryBuilder};
use serde_json::json;
use uuid::Uuid;

const SOURCE: &str = "https://example.invalid/strict-project/repo";

fn registry() -> VerbRegistry {
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        embedding_model: None,
        additional_embedding_models: vec![],
        wal_ceiling_env_raw: None,
        actor_id: None,
        brain_profile: None,
        ..RuntimeConfig::default()
    })
    .unwrap();
    assert!(!runtime.backend().is_file_backed());
    assert!(runtime.backend().data_dir().is_none());
    assert!(runtime.backend().ann_root().is_none());
    assert!(runtime.registered_embedding_model_names().is_empty());
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(runtime.clone()));
    builder.register(GitPack::new(runtime.clone()));
    builder.with_runtime_event_store(&runtime).unwrap();
    let registry = builder.build().unwrap();
    runtime.install_edge_rules(registry.all_edge_rules());
    registry.apply_schema_plans(runtime.backend());
    registry
}

async fn graph_counts(registry: &VerbRegistry) -> [u64; 3] {
    let stats = registry.dispatch("stats", json!({})).await.unwrap();
    ["entities", "notes", "edges"].map(|key| stats[key].as_u64().unwrap())
}

#[tokio::test]
async fn digest_rejects_non_string_project_without_graph_writes() {
    let registry = registry();
    assert_eq!(graph_counts(&registry).await, [0, 0, 0]);
    for project in [
        json!(false),
        json!(true),
        json!(7),
        json!(1.5),
        json!([]),
        json!(["project"]),
        json!({}),
        json!({"id": "project"}),
    ] {
        let result = registry
            .dispatch(
                "git.digest",
                json!({
                    "source": SOURCE, "project": project, "include": [],
                }),
            )
            .await;
        // Check graph state even if the old handler returned success after creating an anchor.
        // Refused dispatches may still append audit events, which are not graph rows.
        assert_eq!(
            graph_counts(&registry).await,
            [0, 0, 0],
            "project={project}, result={result:?}"
        );
        assert!(
            matches!(result, Err(RuntimeError::InvalidInput(ref message))
            if message == "project must be a string when provided"),
            "project={project}, result={result:?}"
        );
    }
}

#[tokio::test]
async fn digest_preserves_explicit_strings_and_missing_or_null_auto_anchor() {
    let registry = registry();
    let created = registry
        .dispatch(
            "create",
            json!({
                "kind": "project", "name": "Explicit digest project", "skip_dedup_check": true,
            }),
        )
        .await
        .unwrap();
    let explicit_id = Uuid::parse_str(created["id"].as_str().unwrap()).unwrap();
    // The only entity is this project, so its eight-hex prefix is unambiguous.
    for project in [
        explicit_id.to_string(),
        explicit_id.to_string().to_uppercase(),
        explicit_id.simple().to_string()[..8].to_owned(),
    ] {
        let report = registry
            .dispatch(
                "git.digest",
                json!({
                    "source": SOURCE, "project": project, "include": [],
                }),
            )
            .await
            .unwrap();
        assert_eq!(
            report["project_id"],
            json!(explicit_id.to_string()),
            "{report}"
        );
        assert_eq!(report["project_created"], json!(false), "{report}");
        assert_eq!(graph_counts(&registry).await, [1, 0, 0]);
    }
    let omitted = registry
        .dispatch(
            "git.digest",
            json!({
                "source": SOURCE, "include": [],
            }),
        )
        .await
        .unwrap();
    let auto_id = Uuid::parse_str(omitted["project_id"].as_str().unwrap()).unwrap();
    assert_ne!(auto_id, explicit_id);
    assert_eq!(omitted["project_created"], json!(true), "{omitted}");
    assert_eq!(graph_counts(&registry).await, [2, 0, 0]);
    let null = registry
        .dispatch(
            "git.digest",
            json!({
                "source": SOURCE, "project": null, "include": [],
            }),
        )
        .await
        .unwrap();
    assert_eq!(null["project_id"], json!(auto_id.to_string()), "{null}");
    assert_eq!(null["project_created"], json!(false), "{null}");
    assert_eq!(graph_counts(&registry).await, [2, 0, 0]);
}

#[tokio::test]
async fn digest_keeps_source_budget_and_include_refusals_before_project_validation() {
    let registry = registry();
    for (params, expected) in [
        (
            json!({"source": "", "project": false, "include": []}),
            "source must not be empty",
        ),
        (
            json!({"source": "http://example.invalid/repo", "project": false, "include": []}),
            "plain http:// URLs are rejected -- use https://",
        ),
        (
            json!({"source": SOURCE, "project": false, "max_items": false, "include": []}),
            "max_items must be an integer",
        ),
        (
            json!({"source": SOURCE, "project": false, "include": false}),
            "include must be an array of strings",
        ),
    ] {
        let result = registry.dispatch("git.digest", params).await;
        assert_eq!(graph_counts(&registry).await, [0, 0, 0]);
        assert!(
            matches!(result, Err(RuntimeError::InvalidInput(ref message))
            if message.contains(expected)),
            "expected {expected:?}, result={result:?}"
        );
    }
}
