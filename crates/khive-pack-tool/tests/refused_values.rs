//! Public tool refusals describe invalid values without returning credentials.

use khive_pack_kg::KgPack;
use khive_pack_tool::ToolPack;
use khive_runtime::{
    runtime_error_value, DomainDisposition, KhiveRuntime, RuntimeError, VerbRegistry,
    VerbRegistryBuilder,
};
use serde_json::{json, Value};

fn fixture() -> VerbRegistry {
    let runtime = KhiveRuntime::memory().expect("private memory runtime");
    assert!(runtime.backend().pool().canonical_path().is_none());
    assert!(runtime.default_embedder_name().is_empty());
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(runtime.clone()));
    builder.register(ToolPack::new(runtime.clone()));
    let registry = builder.build().expect("tool registry");
    registry
        .apply_schema_plans_with_map(&Default::default(), runtime.backend())
        .expect("tool schema");
    runtime.install_edge_rules(registry.all_edge_rules());
    registry
}

fn credential() -> String {
    format!("sk-proj-{}", "A".repeat(80))
}

async fn assert_refusal(
    registry: &VerbRegistry,
    verb: &str,
    params: Value,
    expected: String,
    secret: Option<&str>,
) {
    let error = registry.dispatch(verb, params).await.expect_err("refusal");
    let RuntimeError::InvalidInput(message) = &error else {
        panic!("{verb} changed its refusal variant: {error:?}");
    };
    if let Some(secret) = secret {
        assert!(
            !message.contains(secret),
            "tool refusal exposed the synthetic credential"
        );
        assert!(message.contains("***MASKED***"), "mask absent: {message}");
    }
    assert_eq!(message, &expected, "{verb} diagnostic");
    assert_eq!(
        runtime_error_value(error, DomainDisposition::NotCommitted),
        json!({
            "kind": "runtime_error",
            "message": format!("invalid input: {expected}"),
            "domain_disposition": "not_committed"
        })
    );
}

async fn assert_no_writes(registry: &VerbRegistry) {
    for verb in ["tool.list", "tool.policies", "tool.requests"] {
        let response = registry.dispatch(verb, json!({})).await.unwrap();
        assert_eq!(response["count"], 0, "refusal wrote through {verb}");
    }
    let entities = registry
        .dispatch("list", json!({"kind": "entity"}))
        .await
        .unwrap();
    assert!(entities["items"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn wrong_string_types_mask_nested_credentials_and_keep_valid_strings() {
    let registry = fixture();
    let secret = credential();
    for (value, shown, masked) in [
        (json!(42), json!(42).to_string(), false),
        (json!(false), json!(false).to_string(), false),
        (
            json!(["ordinary", 17]),
            json!(["ordinary", 17]).to_string(),
            false,
        ),
        (
            json!({"nested": [secret, 17]}),
            r#"{"nested":["***MASKED***]}"#.to_string(),
            true,
        ),
        (json!([secret]), json!(["***MASKED***"]).to_string(), true),
    ] {
        for (verb, params) in [
            (
                "tool.register",
                json!({"name": "refused_tool", "description": value}),
            ),
            (
                "tool.ingest",
                json!({"source": "mcp", "server": "fixture", "tools": [
                    {"name": "refused_tool", "description": value}
                ]}),
            ),
        ] {
            assert_refusal(
                &registry,
                verb,
                params,
                format!("description must be a string; got {shown}"),
                masked.then_some(secret.as_str()),
            )
            .await;
        }
    }
    // Every remaining public caller of opt_str/req_str reaches the same refusal
    // before lookup, retrieval, grant mutation, or policy mutation.
    for (value, shown, masked) in [
        (json!(42), json!(42).to_string(), false),
        (
            json!({"nested": [secret]}),
            json!({"nested": ["***MASKED***"]}).to_string(),
            true,
        ),
    ] {
        for (verb, field) in [
            ("tool.suggest", "query"),
            ("tool.describe", "tool"),
            ("tool.check", "tool"),
            ("tool.request", "tool"),
            ("tool.grant", "id"),
            ("tool.deny", "id"),
            ("tool.revoke", "id"),
            ("tool.list", "kind"),
            ("tool.requests", "status"),
            ("tool.policy", "actor"),
            ("tool.policy_delete", "actor"),
            ("tool.policies", "actor"),
        ] {
            let mut params = json!({});
            params[field] = value.clone();
            assert_refusal(
                &registry,
                verb,
                params,
                format!("{field} must be a string; got {shown}"),
                masked.then_some(secret.as_str()),
            )
            .await;
        }
    }
    assert_no_writes(&registry).await;
    for value in [json!(""), json!(" \t ")] {
        assert_refusal(
            &registry,
            "tool.register",
            json!({"name": "refused_tool", "description": value}),
            "description must be a non-empty string when provided".into(),
            None,
        )
        .await;
    }
    let created = registry
        .dispatch(
            "tool.register",
            json!({"name": "  valid_tool  ", "description": "  ordinary text  "}),
        )
        .await
        .expect("valid strings still trim and register");
    assert_eq!(created["created"], true);
    assert_eq!(created["tool"]["name"], "valid_tool");
    assert_eq!(created["tool"]["description"], "ordinary text");
    let ingested = registry
        .dispatch(
            "tool.ingest",
            json!({"source": "mcp", "server": "fixture", "tools": [
                {"name": "ingested_tool", "description": null}
            ]}),
        )
        .await
        .expect("null optional string remains omitted");
    assert_eq!(ingested["registered"], 1);
    let described = registry
        .dispatch("tool.describe", json!({"tool": "ingested_tool"}))
        .await
        .unwrap();
    assert_eq!(described["tool"]["description"], Value::Null);
}

#[tokio::test]
async fn enum_refusals_mask_credentials_and_preserve_sets_and_valid_policy() {
    let registry = fixture();
    let secret = credential();
    for (raw, shown, masked) in [("bogus", "bogus", false), (&secret, "***MASKED***", true)] {
        for (field, allowed) in [
            ("kind", "tool, skill, plugin, verb"),
            ("side_effect", "read, write, egress, irreversible"),
            ("trust", "first_party, marketplace, external"),
        ] {
            let mut params = json!({"name": "refused_tool"});
            params[field] = json!(raw);
            assert_refusal(
                &registry,
                "tool.register",
                params,
                format!("{field} must be one of {allowed}; got {shown:?}"),
                masked.then_some(secret.as_str()),
            )
            .await;
        }
        for (verb, params, field, allowed) in [
            (
                "tool.ingest",
                json!({"source": "mcp", "server": "fixture", "tools": [
                    {"name": "refused_tool", "side_effect": raw}
                ]}),
                "side_effect",
                "read, write, egress, irreversible",
            ),
            (
                "tool.ingest",
                json!({"source": "mcp", "server": "fixture", "trust": raw,
                    "tools": [{"name": "refused_tool"}]}),
                "trust",
                "first_party, marketplace, external",
            ),
            (
                "tool.list",
                json!({"kind": raw}),
                "kind",
                "tool, skill, plugin, verb",
            ),
            (
                "tool.suggest",
                json!({"query": "fixture", "kind": raw}),
                "kind",
                "tool, skill, plugin, verb",
            ),
            (
                "tool.requests",
                json!({"status": raw}),
                "status",
                "requested, granted, denied, revoked",
            ),
            (
                "tool.policy",
                json!({"actor": "actor:fixture", "tool": "valid_tool", "decision": raw}),
                "decision",
                "allow, deny, ask",
            ),
        ] {
            assert_refusal(
                &registry,
                verb,
                params,
                format!("{field} must be one of {allowed}; got {shown:?}"),
                masked.then_some(secret.as_str()),
            )
            .await;
        }
    }
    assert_no_writes(&registry).await;
    for kind in ["tool", "skill", "plugin", "verb"] {
        let created = registry
            .dispatch(
                "tool.register",
                json!({"name": format!("valid_{kind}"), "kind": kind,
                "side_effect": "read", "trust": "first_party"}),
            )
            .await
            .expect("accepted registry kind");
        assert_eq!(created["created"], true);
        assert_eq!(created["tool"]["kind"], kind);
        let listed = registry
            .dispatch("tool.list", json!({"kind": kind}))
            .await
            .unwrap();
        assert_eq!(listed["count"], 1);
        assert_eq!(listed["tools"][0]["full_id"], created["tool"]["full_id"]);
    }
    for decision in ["allow", "deny", "ask"] {
        let response = registry
            .dispatch("tool.policy", json!({"actor": format!("actor:{decision}"), "tool": "valid_tool", "decision": decision}))
            .await
            .expect("accepted policy decision");
        assert_eq!(response["policy"]["decision"], decision);
    }
    let policies = registry.dispatch("tool.policies", json!({})).await.unwrap();
    assert_eq!(policies["count"], 3);
}

#[tokio::test]
async fn string_list_refusals_show_masked_values_and_keep_valid_lists() {
    let registry = fixture();
    let secret = credential();
    for (value, shown, item, masked) in [
        (json!([42]), json!(42), true, false),
        (json!([""]), json!(""), true, false),
        (json!([" \t "]), json!(" \t "), true, false),
        (json!("ordinary"), json!("ordinary"), false, false),
        (
            json!([{"nested": [secret]}]),
            json!({"nested": ["***MASKED***"]}),
            true,
            true,
        ),
        (json!(secret), json!("***MASKED***"), false, true),
        (
            json!({"nested": [secret]}),
            json!({"nested": ["***MASKED***"]}),
            false,
            true,
        ),
    ] {
        for field in ["tags", "capabilities"] {
            let mut params = json!({"name": "refused_tool"});
            params[field] = value.clone();
            let required = if item {
                "an array of non-empty strings"
            } else {
                "an array of strings"
            };
            assert_refusal(
                &registry,
                "tool.register",
                params,
                format!("{field} must be {required}; got {shown}"),
                masked.then_some(secret.as_str()),
            )
            .await;
            if field == "capabilities" {
                assert_refusal(
                    &registry,
                    "tool.ingest",
                    json!({"source": "mcp", "server": "fixture", "tools": [
                        {"name": "refused_tool", "capabilities": value}
                    ]}),
                    format!("{field} must be {required}; got {shown}"),
                    masked.then_some(secret.as_str()),
                )
                .await;
            }
        }
    }
    assert_no_writes(&registry).await;
    let created = registry
        .dispatch(
            "tool.register",
            json!({"name": "listed_tool", "tags": ["  alpha  ", "beta"],
                "capabilities": ["  read files  "]}),
        )
        .await
        .expect("valid lists still trim and register");
    assert_eq!(created["created"], true);
    let tags = created["tool"]["tags"].as_array().unwrap();
    assert!(tags.contains(&json!("alpha")) && tags.contains(&json!("beta")));
    assert!(!tags.contains(&json!("  alpha  ")));
    assert_eq!(created["capabilities"][0]["name"], "read files");
    let described = registry
        .dispatch("tool.describe", json!({"tool": "listed_tool"}))
        .await
        .expect("accepted capability link persists");
    assert_eq!(described["tool"]["capabilities"][0]["name"], "read files");
    for (name, value) in [("null_lists", Value::Null), ("empty_lists", json!([]))] {
        let response = registry
            .dispatch(
                "tool.register",
                json!({"name": name, "tags": value, "capabilities": value}),
            )
            .await
            .expect("null and empty lists remain accepted");
        assert_eq!(response["created"], true);
        assert!(response["capabilities"].as_array().unwrap().is_empty());
    }
}
