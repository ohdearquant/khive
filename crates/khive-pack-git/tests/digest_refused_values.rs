use khive_pack_git::GitPack;
use khive_pack_kg::KgPack;
use khive_runtime::{
    runtime_error_value, DomainDisposition, KhiveRuntime, RuntimeError, VerbRegistry,
    VerbRegistryBuilder,
};
use serde_json::{json, Value};

const SOURCE: &str = "https://example.invalid/refused-include/repo";

fn fixture() -> VerbRegistry {
    let runtime = KhiveRuntime::memory().expect("private memory runtime");
    assert!(runtime.backend().pool().canonical_path().is_none());
    assert!(runtime.default_embedder_name().is_empty());
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(runtime.clone()));
    builder.register(GitPack::new(runtime.clone()));
    let registry = builder.build().expect("git registry");
    registry
        .apply_schema_plans_with_map(&Default::default(), runtime.backend())
        .expect("git schema");
    runtime.install_edge_rules(registry.all_edge_rules());
    registry
}

fn credential() -> String {
    format!("sk-proj-{}", "A".repeat(80))
}

async fn graph_counts(registry: &VerbRegistry) -> [u64; 3] {
    let stats = registry.dispatch("stats", json!({})).await.unwrap();
    ["entities", "notes", "edges"].map(|key| stats[key].as_u64().unwrap())
}

async fn assert_refusal(
    registry: &VerbRegistry,
    params: Value,
    expected: String,
    secret: Option<&str>,
) {
    let result = registry.dispatch("git.digest", params).await;
    assert_eq!(
        graph_counts(registry).await,
        [0, 0, 0],
        "refusal changed graph rows"
    );
    let error = result.expect_err("include refusal");
    let RuntimeError::InvalidInput(message) = &error else {
        panic!("git.digest changed its refusal variant: {error:?}");
    };
    if let Some(secret) = secret {
        assert!(
            !message.contains(secret),
            "digest refusal exposed the synthetic credential"
        );
        assert!(message.contains("***MASKED***"), "mask absent: {message}");
    }
    assert_eq!(message, &expected, "digest diagnostic");
    assert_eq!(
        runtime_error_value(error, DomainDisposition::NotCommitted),
        json!({
            "kind": "runtime_error",
            "message": format!("invalid input: {expected}"),
            "domain_disposition": "not_committed"
        })
    );
}

#[tokio::test]
async fn include_array_type_refusals_show_masked_values_without_writes() {
    let registry = fixture();
    let secret = credential();
    for (value, shown, masked) in [
        (json!(42), "42".to_string(), false),
        (json!(false), "false".to_string(), false),
        (json!("commits"), json!("commits").to_string(), false),
        (
            json!({"ordinary": 7}),
            json!({"ordinary": 7}).to_string(),
            false,
        ),
        (json!(secret), json!("***MASKED***").to_string(), true),
        (
            json!({"nested": [secret]}),
            json!({"nested": ["***MASKED***"]}).to_string(),
            true,
        ),
    ] {
        assert_refusal(
            &registry,
            json!({"source": SOURCE, "include": value, "project": false}),
            format!("include must be an array of strings; got {shown}"),
            masked.then_some(secret.as_str()),
        )
        .await;
    }
}

#[tokio::test]
async fn include_entry_type_refusals_show_masked_values_without_writes() {
    let registry = fixture();
    let secret = credential();
    for (entry, shown, masked) in [
        (Value::Null, "null".to_string(), false),
        (json!(42), "42".to_string(), false),
        (json!(["ordinary"]), json!(["ordinary"]).to_string(), false),
        (json!([secret]), json!(["***MASKED***"]).to_string(), true),
        (
            json!({"nested": [secret]}),
            json!({"nested": ["***MASKED***"]}).to_string(),
            true,
        ),
    ] {
        assert_refusal(
            &registry,
            json!({"source": SOURCE, "include": ["commits", entry], "project": false}),
            format!("include entries must be strings; got {shown}"),
            masked.then_some(secret.as_str()),
        )
        .await;
    }
}

#[tokio::test]
async fn include_kind_refusals_mask_credentials_and_keep_ordinary_diagnostics() {
    let registry = fixture();
    let secret = credential();
    for (value, shown, masked) in [
        ("branches", "branches", false),
        ("COMMITS", "COMMITS", false),
        ("", "", false),
        (secret.as_str(), "***MASKED***", true),
    ] {
        assert_refusal(
            &registry,
            json!({"source": SOURCE, "include": ["issues", value], "project": false}),
            format!("unknown include kind {shown:?}; valid: commits | issues | pull_requests"),
            masked.then_some(secret.as_str()),
        )
        .await;
    }
}

#[tokio::test]
async fn digest_keeps_include_precedence_and_accepts_empty_explicit_project() {
    let registry = fixture();
    for include in [
        json!([]),
        json!(["commits"]),
        json!(["issues"]),
        json!(["pull_requests"]),
        json!(["commits", "issues", "pull_requests", "commits"]),
        Value::Null,
    ] {
        // Project validation follows a valid include and still precedes any clone or API read.
        assert_refusal(
            &registry,
            json!({"source": SOURCE, "include": include, "project": false}),
            "project must be a string when provided".to_string(),
            None,
        )
        .await;
    }
    for (params, expected) in [
        (
            json!({"source": "", "include": false}),
            "source must not be empty",
        ),
        (
            json!({"source": SOURCE, "max_items": false, "include": false}),
            "max_items must be an integer, got Bool(false)",
        ),
    ] {
        assert_refusal(&registry, params, expected.to_string(), None).await;
    }
    let created = registry
        .dispatch(
            "create",
            json!({
                "kind": "project", "name": "Explicit include control", "skip_dedup_check": true
            }),
        )
        .await
        .unwrap();
    let project = created["id"].as_str().expect("created project id");
    assert_eq!(graph_counts(&registry).await, [1, 0, 0]);
    let report = registry
        .dispatch(
            "git.digest",
            json!({
                "source": SOURCE, "project": project, "include": []
            }),
        )
        .await
        .expect("empty include needs no repository or remote API");
    assert_eq!(report["project_id"], project);
    assert_eq!(report["project_created"], false);
    assert_eq!(report["done"], true);
    assert_eq!(graph_counts(&registry).await, [1, 0, 0]);
}
