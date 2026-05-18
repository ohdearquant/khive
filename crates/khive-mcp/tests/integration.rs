//! Integration tests for the request-only khive-mcp surface (ADR-020 + ADR-027).
//!
//! Validates the single-tool composition: every verb is reached via `request(ops="…")`.

use khive_mcp::server::KhiveMcpServer;
use khive_runtime::{KhiveRuntime, RuntimeConfig};
use rmcp::{
    model::{CallToolRequestParams, CallToolResult, ClientInfo, ErrorCode},
    ClientHandler, ServerHandler, ServiceError, ServiceExt,
};
use serde_json::{json, Value};

fn make_server() -> KhiveMcpServer {
    let config = RuntimeConfig {
        db_path: None,
        default_namespace: "test".to_string(),
        embedding_model: None,
    };
    let runtime = KhiveRuntime::new(config).expect("in-memory runtime");
    KhiveMcpServer::new(runtime)
}

#[derive(Clone, Default)]
struct DummyClient;

impl ClientHandler for DummyClient {
    fn get_info(&self) -> ClientInfo {
        ClientInfo::default()
    }
}

async fn connect(
) -> anyhow::Result<impl std::ops::Deref<Target = rmcp::service::Peer<rmcp::RoleClient>>> {
    let (server_transport, client_transport) = tokio::io::duplex(65536);
    let server = make_server();
    tokio::spawn(async move {
        if let Ok(server_service) = server.serve(server_transport).await {
            let _ = server_service.waiting().await;
        }
    });
    let client = DummyClient.serve(client_transport).await?;
    Ok(client)
}

fn first_text(r: &CallToolResult) -> String {
    r.content
        .first()
        .and_then(|c| c.raw.as_text())
        .map(|t| t.text.clone())
        .unwrap_or_default()
}

async fn call(
    client: &impl std::ops::Deref<Target = rmcp::service::Peer<rmcp::RoleClient>>,
    name: impl Into<String>,
    args: Value,
) -> anyhow::Result<CallToolResult> {
    let params = CallToolRequestParams::new(name.into())
        .with_arguments(args.as_object().expect("args must be JSON object").clone());
    Ok(client.call_tool(params).await?)
}

/// Run a single op via `request` and return the `result` field of the first entry.
/// Panics if the op failed.
async fn ok_one(
    client: &impl std::ops::Deref<Target = rmcp::service::Peer<rmcp::RoleClient>>,
    ops: &str,
) -> anyhow::Result<Value> {
    let result = call(client, "request", json!({ "ops": ops })).await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    let first = body["results"].get(0).cloned().unwrap_or(Value::Null);
    assert_eq!(
        first["ok"],
        json!(true),
        "expected op to succeed, got: {first}"
    );
    Ok(first["result"].clone())
}

// ── server info / surface shape ──────────────────────────────────────────────

#[tokio::test]
async fn server_info_advertises_request_tool_only() {
    let server = make_server();
    let info = server.get_info();
    assert_eq!(info.server_info.name, "khive-mcp");
    let instructions = info.instructions.unwrap_or_default();
    assert!(
        instructions.contains("request-only"),
        "instructions should explain the request-only surface"
    );
    // The kg pack's verbs must appear in the catalog so agents can discover them.
    assert!(
        instructions.contains("create"),
        "kg verb should appear in catalog"
    );
}

#[tokio::test]
async fn list_tools_returns_only_request() -> anyhow::Result<()> {
    let client = connect().await?;
    let listed = client.list_tools(None).await?;
    let names: Vec<&str> = listed.tools.iter().map(|t| t.name.as_ref()).collect();
    assert_eq!(names, vec!["request"], "exactly one tool should be exposed");
    Ok(())
}

// ── kg verbs through the DSL ─────────────────────────────────────────────────

#[tokio::test]
async fn create_entity_via_dsl() -> anyhow::Result<()> {
    let client = connect().await?;
    let result = ok_one(
        &client,
        r#"create(kind="entity", entity_kind="concept", name="LoRA")"#,
    )
    .await?;
    assert_eq!(result["name"], "LoRA");
    Ok(())
}

#[tokio::test]
async fn create_then_list_in_one_batch() -> anyhow::Result<()> {
    let client = connect().await?;
    let result = call(
        &client,
        "request",
        json!({
            "ops": r#"[create(kind="entity", entity_kind="concept", name="A"), create(kind="entity", entity_kind="concept", name="B"), list(kind="entity")]"#
        }),
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    let results = body["results"].as_array().expect("array");
    assert_eq!(results.len(), 3);
    for r in results {
        assert_eq!(r["ok"], json!(true), "op should succeed: {r}");
    }
    // Last op listed entities — handler returns a JSON array directly.
    let entities = results[2]["result"]
        .as_array()
        .expect("entities array (list returns array directly)");
    let names: Vec<&str> = entities.iter().filter_map(|e| e["name"].as_str()).collect();
    assert!(names.contains(&"A"), "entity A missing: {names:?}");
    assert!(names.contains(&"B"), "entity B missing: {names:?}");
    Ok(())
}

#[tokio::test]
async fn invalid_kind_failure_does_not_abort_batch() -> anyhow::Result<()> {
    let client = connect().await?;
    let result = call(
        &client,
        "request",
        json!({
            "ops": r#"[create(kind="entity", entity_kind="not_a_kind", name="X"), create(kind="entity", entity_kind="concept", name="Y")]"#
        }),
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    let results = body["results"].as_array().expect("array");
    assert_eq!(results.len(), 2);
    assert_eq!(
        results[0]["ok"],
        json!(false),
        "first op (invalid kind) should fail"
    );
    assert_eq!(
        results[1]["ok"],
        json!(true),
        "second op should still succeed — batch must not abort"
    );
    assert_eq!(body["summary"]["succeeded"], json!(1));
    assert_eq!(body["summary"]["failed"], json!(1));
    Ok(())
}

#[tokio::test]
async fn malformed_dsl_returns_invalid_params() -> anyhow::Result<()> {
    let client = connect().await?;
    let err = call(&client, "request", json!({ "ops": "create(kind=" }))
        .await
        .expect_err("malformed DSL must error at the protocol level");
    match err.downcast::<ServiceError>() {
        Ok(ServiceError::McpError(e)) => {
            assert_eq!(e.code, ErrorCode::INVALID_PARAMS);
        }
        other => panic!("expected InvalidParams McpError, got: {other:?}"),
    }
    Ok(())
}

#[tokio::test]
async fn unknown_verb_returns_per_op_failure_not_invalid_params() -> anyhow::Result<()> {
    let client = connect().await?;
    let result = call(&client, "request", json!({ "ops": "no_such_verb()" })).await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    let first = &body["results"][0];
    assert_eq!(
        first["ok"],
        json!(false),
        "unknown verb should fail per-op, not abort"
    );
    assert!(
        first["error"]
            .as_str()
            .is_some_and(|s| s.contains("unknown verb")),
        "error should mention unknown verb: {first}"
    );
    Ok(())
}

#[tokio::test]
async fn json_form_request_works_identically() -> anyhow::Result<()> {
    let client = connect().await?;
    let result = call(
        &client,
        "request",
        json!({
            "ops": r#"[{"tool":"create","args":{"kind":"entity","entity_kind":"concept","name":"Z"}}]"#
        }),
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    let first = &body["results"][0];
    assert_eq!(
        first["ok"],
        json!(true),
        "JSON-form should succeed: {first}"
    );
    assert_eq!(first["result"]["name"], "Z");
    Ok(())
}
