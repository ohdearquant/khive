//! Integration tests for the request-only khive-mcp surface (ADR-020 + ADR-025).
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
        packs: vec!["kg".to_string(), "gtd".to_string()],
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

/// Helper: run a single op via `request` and return the parsed `result` field
/// of the first entry. Panics if the op failed.
async fn ok_one(
    client: &impl std::ops::Deref<Target = rmcp::service::Peer<rmcp::RoleClient>>,
    ops: &str,
) -> anyhow::Result<Value> {
    let result = call(client, "request", json!({"ops": ops})).await?;
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
    // Pack verbs must appear in the catalog so agents can discover what's loaded.
    assert!(instructions.contains("assign"), "gtd verb should appear");
    assert!(instructions.contains("create"), "kg verb should appear");
}

#[tokio::test]
async fn list_tools_returns_only_request() -> anyhow::Result<()> {
    let client = connect().await?;
    let result = client.list_tools(None).await?;
    let names: Vec<&str> = result.tools.iter().map(|t| t.name.as_ref()).collect();
    assert_eq!(names, vec!["request"], "surface should be a single tool");
    Ok(())
}

// ── KG verbs round-tripped through the DSL ──────────────────────────────────

#[tokio::test]
async fn create_entity_via_dsl() -> anyhow::Result<()> {
    let client = connect().await?;
    let result = ok_one(
        &client,
        r#"create(kind="entity", entity_kind="concept", name="LoRA")"#,
    )
    .await?;
    assert_eq!(result["kind"], "concept");
    assert_eq!(result["name"], "LoRA");
    Ok(())
}

#[tokio::test]
async fn create_then_list_in_one_batch() -> anyhow::Result<()> {
    let client = connect().await?;
    let result = call(
        &client,
        "request",
        json!({"ops": r#"[create(kind="entity", entity_kind="concept", name="A"), create(kind="entity", entity_kind="concept", name="B"), list(kind="entity")]"#}),
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    assert_eq!(body["summary"]["total"], 3);
    assert_eq!(body["summary"]["succeeded"], 3);
    // Last result is the list — must include both newly created entities.
    let listed = body["results"][2]["result"].as_array().unwrap();
    let names: Vec<&str> = listed.iter().filter_map(|e| e["name"].as_str()).collect();
    assert!(names.contains(&"A"));
    assert!(names.contains(&"B"));
    Ok(())
}

#[tokio::test]
async fn invalid_kind_failure_does_not_abort_batch() -> anyhow::Result<()> {
    let client = connect().await?;
    let result = call(
        &client,
        "request",
        json!({"ops": r#"[create(kind="entity", entity_kind="concept", name="ok"), create(kind="entity", entity_kind="bogus", name="bad")]"#}),
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    assert_eq!(body["summary"]["total"], 2);
    assert_eq!(body["summary"]["succeeded"], 1);
    assert_eq!(body["summary"]["failed"], 1);
    assert_eq!(body["results"][0]["ok"], true);
    assert_eq!(body["results"][1]["ok"], false);
    assert!(body["results"][1]["error"]
        .as_str()
        .unwrap()
        .contains("bogus"));
    Ok(())
}

#[tokio::test]
async fn malformed_dsl_returns_invalid_params() -> anyhow::Result<()> {
    let client = connect().await?;
    let err = call(&client, "request", json!({"ops": "create("}))
        .await
        .err();
    let svc = err.as_ref().and_then(|e| e.downcast_ref::<ServiceError>());
    assert!(
        matches!(
            svc,
            Some(ServiceError::McpError(e)) if e.code == ErrorCode::INVALID_PARAMS
        ),
        "expected invalid_params for malformed DSL, got {err:?}"
    );
    Ok(())
}

// ── GTD verbs round-tripped through the DSL ─────────────────────────────────

#[tokio::test]
async fn assign_then_next_then_complete() -> anyhow::Result<()> {
    let client = connect().await?;

    let assigned = ok_one(
        &client,
        r#"assign(title="ship release", status="next", priority="p0")"#,
    )
    .await?;
    let id = assigned["full_id"].as_str().unwrap().to_string();
    assert_eq!(assigned["kind"], "task");
    assert_eq!(assigned["status"], "next");

    let next_list = ok_one(&client, "next()").await?;
    let arr = next_list.as_array().unwrap();
    assert!(arr.iter().any(|t| t["full_id"] == id));

    let completed = ok_one(
        &client,
        &format!(r#"complete(id="{id}", result="shipped via request")"#),
    )
    .await?;
    assert_eq!(completed["to"], "done");
    Ok(())
}

#[tokio::test]
async fn transition_lifecycle_rejection_is_per_op_not_protocol_error() -> anyhow::Result<()> {
    let client = connect().await?;
    let assigned = ok_one(&client, r#"assign(title="lifecycle")"#).await?;
    let id = assigned["full_id"].as_str().unwrap().to_string();

    // inbox → done is allowed; done → inbox is NOT.
    ok_one(&client, &format!(r#"transition(id="{id}", status="done")"#)).await?;

    let result = call(
        &client,
        "request",
        json!({"ops": format!(r#"transition(id="{id}", status="inbox")"#)}),
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    let first = &body["results"][0];
    assert_eq!(first["ok"], false);
    assert!(first["error"]
        .as_str()
        .unwrap()
        .contains("cannot transition"));
    Ok(())
}

#[tokio::test]
async fn parallel_assign_batch_creates_n_tasks() -> anyhow::Result<()> {
    let client = connect().await?;
    let ops = r#"[
        assign(title="t1", priority="p0"),
        assign(title="t2", priority="p1"),
        assign(title="t3", priority="p2")
    ]"#;
    let result = call(&client, "request", json!({"ops": ops})).await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    assert_eq!(body["summary"]["succeeded"], 3);
    Ok(())
}

#[tokio::test]
async fn unknown_verb_returns_per_op_failure_not_invalid_params() -> anyhow::Result<()> {
    let client = connect().await?;
    let result = call(&client, "request", json!({"ops": "retire()"})).await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    let first = &body["results"][0];
    assert_eq!(first["ok"], false);
    assert!(first["error"].as_str().unwrap().contains("unknown verb"));
    Ok(())
}

#[tokio::test]
async fn pack_only_kg_omits_gtd_verbs_from_catalog() {
    let config = RuntimeConfig {
        db_path: None,
        default_namespace: "test".to_string(),
        embedding_model: None,
        packs: vec!["kg".to_string()],
    };
    let runtime = KhiveRuntime::new(config).unwrap();
    let server = KhiveMcpServer::new(runtime);
    let info = server.get_info();
    let instructions = info.instructions.unwrap_or_default();
    assert!(instructions.contains("create"), "kg verb missing");
    assert!(
        !instructions.contains("\n  assign "),
        "gtd verb should not be in catalog when only kg is loaded"
    );
}

#[tokio::test]
async fn pack_only_gtd_omits_kg_verbs_from_catalog() {
    let config = RuntimeConfig {
        db_path: None,
        default_namespace: "test".to_string(),
        embedding_model: None,
        packs: vec!["gtd".to_string()],
    };
    let runtime = KhiveRuntime::new(config).unwrap();
    let server = KhiveMcpServer::new(runtime);
    let info = server.get_info();
    let instructions = info.instructions.unwrap_or_default();
    assert!(instructions.contains("assign"), "gtd verb missing");
    assert!(
        !instructions.contains("\n  create "),
        "kg verb should not be in catalog when only gtd is loaded"
    );
}

#[tokio::test]
async fn json_form_request_works_identically() -> anyhow::Result<()> {
    let client = connect().await?;
    let result = call(
        &client,
        "request",
        json!({"ops": r#"[{"tool":"assign","args":{"title":"json form","priority":"p1"}}]"#}),
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    assert_eq!(body["summary"]["succeeded"], 1);
    assert_eq!(body["results"][0]["result"]["title"], "json form");
    Ok(())
}

// ── Kind hooks (ADR-030) — shared CRUD reaches gtd-owned `task` via TaskHook ──

#[tokio::test]
async fn kg_create_with_note_kind_task_invokes_gtd_hook_defaults() -> anyhow::Result<()> {
    let client = connect().await?;
    // Drive the kg `create` verb with note_kind="task" — the kg handler
    // consults the registry, finds gtd's TaskHook, and the hook fills GTD
    // defaults (status=inbox) before the storage write.
    let created = ok_one(
        &client,
        r#"create(kind="note", note_kind="task", title="ship release", priority="p0")"#,
    )
    .await?;

    // Response is the kg note envelope, NOT the gtd task envelope.
    assert_eq!(created["kind"], "task", "note stored with kind=task");
    assert_eq!(created["name"], "ship release", "title folded into name");
    assert_eq!(
        created["properties"]["status"], "inbox",
        "TaskHook applies default status"
    );
    assert_eq!(
        created["properties"]["priority"], "p0",
        "user-supplied priority preserved in properties"
    );
    Ok(())
}

#[tokio::test]
async fn kg_create_with_note_kind_task_resolves_depends_on_into_properties() -> anyhow::Result<()> {
    let client = connect().await?;

    // Stand up a target entity that the task will depend on. `depends_on`
    // edges from a note source are rejected by ADR-002 validation, but the
    // task hook *does* resolve the dependency UUIDs and stash them in
    // properties — which is the source of truth per ADR-026.
    let target = ok_one(
        &client,
        r#"create(kind="entity", entity_kind="concept", name="DependencyTarget")"#,
    )
    .await?;
    let target_id = target["id"].as_str().unwrap().to_string();

    let task = ok_one(
        &client,
        &format!(
            r#"create(kind="note", note_kind="task", title="depends on something", depends_on=["{}"])"#,
            target_id
        ),
    )
    .await?;

    // Hook resolved the entity short/full id into a canonical UUID string
    // and placed it in `properties.depends_on` — same shape gtd's `assign`
    // produces.
    let deps = task["properties"]["depends_on"].as_array().unwrap();
    assert_eq!(deps.len(), 1, "exactly one resolved dependency");
    let resolved = deps[0].as_str().unwrap();
    assert!(
        resolved.contains('-'),
        "depends_on stored as full UUID string, got: {resolved}"
    );
    Ok(())
}

#[tokio::test]
async fn kg_create_unknown_note_kind_lists_merged_pack_vocabulary() -> anyhow::Result<()> {
    let client = connect().await?;
    let result = call(
        &client,
        "request",
        json!({"ops": r#"create(kind="note", note_kind="bogus", content="x")"#}),
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    let first = &body["results"][0];
    assert_eq!(first["ok"], false);
    let err = first["error"].as_str().unwrap();
    assert!(err.contains("bogus"), "error names the bad kind: {err}");
    // The merged vocabulary list must include "task" (gtd) alongside kg kinds.
    assert!(
        err.contains("task"),
        "error must list gtd-registered 'task' kind: {err}"
    );
    assert!(
        err.contains("observation"),
        "error must list kg's 'observation' kind: {err}"
    );
    Ok(())
}
