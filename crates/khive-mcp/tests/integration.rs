//! Integration tests for the request-only khive-mcp surface (ADR-020 + ADR-025).
//!
//! Validates the single-tool composition: every verb is reached via `request(ops="…")`.

use async_trait::async_trait;
use khive_mcp::server::KhiveMcpServer;
use khive_runtime::{
    KhiveRuntime, Namespace, NamespaceToken, PackRuntime, RuntimeConfig, RuntimeError,
    VerbRegistry, VerbRegistryBuilder,
};
use khive_types::{
    Details, ErrorCode as KhiveErrorCode, ErrorDomain, HandlerDef, KhiveError, Pack, VerbCategory,
    Visibility,
};
use rmcp::{
    model::{CallToolRequestParams, CallToolResult, ClientInfo, ErrorCode},
    ClientHandler, ServerHandler, ServiceError, ServiceExt,
};
use serde_json::{json, Value};

fn make_server() -> KhiveMcpServer {
    let config = RuntimeConfig {
        db_path: None,
        default_namespace: Namespace::parse("test").unwrap(),
        embedding_model: None,
        packs: vec!["kg".to_string(), "gtd".to_string()],
        ..RuntimeConfig::default()
    };
    let runtime = KhiveRuntime::new(config).expect("in-memory runtime");
    KhiveMcpServer::new(runtime).expect("server builds with kg+gtd")
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
/// of the first entry. Uses `presentation: "verbose"` so tests receive full
/// canonical UUIDs and timestamps (not Agent-mode short forms). Panics if the
/// op failed.
async fn ok_one(
    client: &impl std::ops::Deref<Target = rmcp::service::Peer<rmcp::RoleClient>>,
    ops: &str,
) -> anyhow::Result<Value> {
    let result = call(
        client,
        "request",
        json!({"ops": ops, "presentation": "verbose"}),
    )
    .await?;
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

#[tokio::test]
async fn request_tool_description_contains_dynamic_verb_catalog() -> anyhow::Result<()> {
    let client = connect().await?;
    let listed = client.list_tools(None).await?;
    let request = listed
        .tools
        .iter()
        .find(|t| t.name == "request")
        .expect("request tool must be present");
    let desc = request.description.as_deref().unwrap_or("");

    // The dynamic catalog must reach `tools/list` consumers (ADR-027). Each
    // verb the kg pack registers should appear by name in the description.
    for verb in [
        "create",
        "get",
        "list",
        "update",
        "delete",
        "merge",
        "search",
        "link",
        "neighbors",
        "traverse",
        "query",
    ] {
        assert!(
            desc.contains(verb),
            "request description missing verb {verb:?}: {desc}"
        );
    }
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
async fn parallel_batch_of_independent_creates_all_succeed() -> anyhow::Result<()> {
    // Ops inside `[...]` are dispatched in parallel (ADR-020 §dispatch).
    // This test exercises that contract with independent ops only —
    // dependent ops (e.g. create-then-list) must split across two `request`
    // calls because the list won't see the creates inside the same batch.
    let client = connect().await?;
    let result = call(
        &client,
        "request",
        json!({
            "ops": r#"[create(kind="entity", entity_kind="concept", name="A"), create(kind="entity", entity_kind="concept", name="B"), create(kind="entity", entity_kind="concept", name="C")]"#
        }),
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    let results = body["results"].as_array().expect("array");
    assert_eq!(results.len(), 3);
    for r in results {
        assert_eq!(r["ok"], json!(true), "op should succeed: {r}");
    }
    assert_eq!(body["summary"]["succeeded"], json!(3));
    assert_eq!(body["summary"]["failed"], json!(0));
    Ok(())
}

#[tokio::test]
async fn create_then_list_across_separate_request_calls() -> anyhow::Result<()> {
    // Create-then-read requires two `request` calls because operations inside
    // a single batch run in parallel and have no ordering guarantee
    // (ADR-020 §dispatch).
    let client = connect().await?;
    call(
        &client,
        "request",
        json!({
            "ops": r#"[create(kind="entity", entity_kind="concept", name="A"), create(kind="entity", entity_kind="concept", name="B")]"#
        }),
    )
    .await?;

    let listed = ok_one(&client, r#"list(kind="entity")"#).await?;
    let entities = listed
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
    // Per P15 (PR #418), terminal states (done/cancelled) reject ALL outgoing
    // transitions with "task X is in terminal state Y; no further transitions allowed".
    assert!(
        first["error"].as_str().unwrap().contains("terminal state"),
        "expected terminal-state rejection, got: {}",
        first["error"]
    );
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
        default_namespace: Namespace::parse("test").unwrap(),
        embedding_model: None,
        packs: vec!["kg".to_string()],
        ..RuntimeConfig::default()
    };
    let runtime = KhiveRuntime::new(config).unwrap();
    let server = KhiveMcpServer::new(runtime).expect("server builds with kg");
    let info = server.get_info();
    let instructions = info.instructions.unwrap_or_default();
    assert!(instructions.contains("create"), "kg verb missing");
    assert!(
        !instructions.contains("\n  assign "),
        "gtd verb should not be in catalog when only kg is loaded"
    );
}

#[tokio::test]
async fn pack_gtd_without_kg_fails_at_boot() {
    // ADR-027: gtd declares requires=["kg"]; omitting "kg" from the pack list
    // must fail at boot with a clear error — not silently auto-add kg.
    let config = RuntimeConfig {
        db_path: None,
        default_namespace: Namespace::parse("test").unwrap(),
        embedding_model: None,
        packs: vec!["gtd".to_string()],
        ..RuntimeConfig::default()
    };
    let runtime = KhiveRuntime::new(config).unwrap();
    match KhiveMcpServer::new(runtime) {
        Ok(_) => panic!("gtd without kg must fail: missing dependency is a boot error (ADR-027)"),
        Err(e) => {
            let msg = e.to_string();
            assert!(
                msg.contains("kg") || msg.contains("unknown pack"),
                "error must name the missing dependency: {msg}"
            );
        }
    }
}

#[tokio::test]
async fn pack_gtd_with_kg_explicit_works() {
    // When both kg and gtd are listed, gtd's requires=["kg"] is satisfied.
    let config = RuntimeConfig {
        db_path: None,
        default_namespace: Namespace::parse("test").unwrap(),
        embedding_model: None,
        packs: vec!["kg".to_string(), "gtd".to_string()],
        ..RuntimeConfig::default()
    };
    let runtime = KhiveRuntime::new(config).unwrap();
    let server = KhiveMcpServer::new(runtime).expect("kg+gtd builds");
    let info = server.get_info();
    let instructions = info.instructions.unwrap_or_default();
    assert!(instructions.contains("assign"), "gtd verb must be present");
    assert!(instructions.contains("create"), "kg verb must be present");
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
async fn kg_create_note_kind_task_resolves_depends_on_against_task_target() -> anyhow::Result<()> {
    let client = connect().await?;

    // Stand up a task that the new task will depend on. The GTD ADR-031 edge
    // rule allows depends_on between two task notes, so this is the only
    // shape the kg-create-with-task-kind path will accept.
    let blocker = ok_one(&client, r#"assign(title="write spec")"#).await?;
    let blocker_full = blocker["full_id"].as_str().unwrap().to_string();

    let task = ok_one(
        &client,
        &format!(
            r#"create(kind="note", note_kind="task", title="depends on something", depends_on=["{}"])"#,
            blocker_full
        ),
    )
    .await?;

    // Hook resolved the short/full id into a canonical UUID string and
    // placed it in `properties.depends_on` — same shape gtd's `assign`
    // produces.
    let deps = task["properties"]["depends_on"].as_array().unwrap();
    assert_eq!(deps.len(), 1, "exactly one resolved dependency");
    let resolved = deps[0].as_str().unwrap();
    assert!(
        resolved.contains('-'),
        "depends_on stored as full UUID string, got: {resolved}"
    );
    assert_eq!(resolved, &blocker_full, "depends_on resolves to blocker");
    Ok(())
}

#[tokio::test]
async fn kg_create_note_kind_task_rejects_non_task_depends_on_before_write() -> anyhow::Result<()> {
    let client = connect().await?;

    // Stand up an entity target. The GTD ADR-031 edge rule is task→task only,
    // so the kg-create path must reject this BEFORE the task is persisted —
    // otherwise we'd leave a task with `properties.depends_on` pointing at a
    // non-task (ADR-030 forbids reporting failure after a successful write).
    let entity = ok_one(
        &client,
        r#"create(kind="entity", entity_kind="concept", name="DependencyTarget")"#,
    )
    .await?;
    // Entity create returns the storage-layer struct keyed on `id` (full UUID),
    // not the GTD task envelope shape.
    let entity_full = entity["id"].as_str().unwrap().to_string();

    let result = call(
        &client,
        "request",
        json!({"ops": format!(
            r#"create(kind="note", note_kind="task", title="depends on entity", depends_on=["{}"])"#,
            entity_full
        )}),
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    let first = &body["results"][0];
    assert_eq!(first["ok"], false, "expected rejection: {first}");
    let err = first["error"].as_str().unwrap();
    assert!(
        err.contains("must be a task note"),
        "error must point to the GTD edge rule: {err}"
    );

    // And there should be no task with the supplied title — write was prevented.
    let listed = ok_one(&client, r#"list(kind="note", note_kind="task")"#).await?;
    let notes = listed.as_array().expect("note list");
    let titles: Vec<&str> = notes.iter().filter_map(|n| n["name"].as_str()).collect();
    assert!(
        !titles.contains(&"depends on entity"),
        "task must not be persisted when depends_on validation fails: {titles:?}"
    );
    Ok(())
}

#[tokio::test]
async fn gtd_assign_creates_depends_on_edge_between_two_tasks() -> anyhow::Result<()> {
    let client = connect().await?;

    let blocker = ok_one(&client, r#"assign(title="write spec")"#).await?;
    let blocker_full = blocker["full_id"].as_str().unwrap().to_string();
    let dependent = ok_one(
        &client,
        &format!(
            r#"assign(title="implement feature", depends_on=["{}"])"#,
            blocker_full
        ),
    )
    .await?;
    let dep_full = dependent["full_id"].as_str().unwrap().to_string();

    // ADR-031: the GTD pack's EDGE_RULES adds task→task `depends_on`.
    // `neighbors(node_id=dependent, direction="out", relations=["depends_on"])`
    // should surface the blocker — proving the edge landed.
    let neighbors = ok_one(
        &client,
        &format!(
            r#"neighbors(node_id="{}", direction="out", relations=["depends_on"])"#,
            dep_full
        ),
    )
    .await?;

    let hits = neighbors.as_array().expect("neighbors returns array");
    // #148: response uses canonical `id` (legacy `node_id` accepted as alias on input only).
    let targets: Vec<&str> = hits.iter().filter_map(|h| h["id"].as_str()).collect();
    assert!(
        targets.iter().any(|t| *t == blocker_full),
        "task→task depends_on edge missing — got targets {targets:?}"
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

// ── Granular `kind=<specific>` discriminator (no entity_kind / note_kind) ────

#[tokio::test]
async fn create_with_granular_entity_kind() -> anyhow::Result<()> {
    let client = connect().await?;
    let result = ok_one(
        &client,
        r#"create(kind="concept", name="GraphAttention", description="self-attention over graph neighborhoods")"#,
    )
    .await?;
    assert_eq!(result["kind"], "concept", "stored under concept kind");
    assert_eq!(result["name"], "GraphAttention");
    Ok(())
}

#[tokio::test]
async fn create_with_granular_note_kind() -> anyhow::Result<()> {
    let client = connect().await?;
    let result = ok_one(
        &client,
        r#"create(kind="observation", content="qwen3.5 retains long-context recall up to 64k")"#,
    )
    .await?;
    assert_eq!(
        result["kind"], "observation",
        "stored under observation kind"
    );
    Ok(())
}

#[tokio::test]
async fn create_granular_kind_conflicts_with_legacy_subfield() -> anyhow::Result<()> {
    let client = connect().await?;
    let result = call(
        &client,
        "request",
        json!({"ops": r#"create(kind="concept", entity_kind="document", name="Conflict")"#}),
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    let first = &body["results"][0];
    assert_eq!(first["ok"], false, "expected contradiction error: {first}");
    let err = first["error"].as_str().unwrap();
    assert!(
        err.contains("contradicts"),
        "error should explain the contradiction: {err}"
    );
    Ok(())
}

#[tokio::test]
async fn list_with_granular_entity_kind_filters_results() -> anyhow::Result<()> {
    let client = connect().await?;
    ok_one(&client, r#"create(kind="concept", name="GranularListA")"#).await?;
    ok_one(&client, r#"create(kind="document", name="GranularListB")"#).await?;

    let listed = ok_one(&client, r#"list(kind="concept")"#).await?;
    let arr = listed.as_array().expect("array");
    let names: Vec<&str> = arr.iter().filter_map(|n| n["name"].as_str()).collect();
    assert!(
        names.contains(&"GranularListA"),
        "concept missing: {names:?}"
    );
    assert!(
        !names.contains(&"GranularListB"),
        "document leaked into concept filter: {names:?}"
    );
    Ok(())
}

#[tokio::test]
async fn list_with_granular_task_kind_lists_only_tasks() -> anyhow::Result<()> {
    let client = connect().await?;
    ok_one(&client, r#"assign(title="GranularTaskA")"#).await?;
    ok_one(
        &client,
        r#"create(kind="observation", content="not a task")"#,
    )
    .await?;

    let listed = ok_one(&client, r#"list(kind="task")"#).await?;
    let arr = listed.as_array().expect("array");
    let titles: Vec<&str> = arr.iter().filter_map(|n| n["name"].as_str()).collect();
    assert!(
        titles.contains(&"GranularTaskA"),
        "task missing: {titles:?}"
    );
    assert!(
        !titles.iter().any(|t| t.contains("not a task")),
        "observation leaked into task list: {titles:?}"
    );
    Ok(())
}

#[tokio::test]
async fn search_with_granular_entity_kind() -> anyhow::Result<()> {
    let client = connect().await?;
    ok_one(
        &client,
        r#"create(kind="concept", name="HybridSearchConcept", description="needle for search")"#,
    )
    .await?;
    ok_one(
        &client,
        r#"create(kind="document", name="HybridSearchDocument", description="needle for search")"#,
    )
    .await?;

    let hits = ok_one(
        &client,
        r#"search(kind="concept", query="HybridSearch needle", limit=10)"#,
    )
    .await?;
    let arr = hits.as_array().expect("array");
    assert!(!arr.is_empty(), "expected at least one hit");
    // Verify the hit kind: fetch each via get and assert kind=concept.
    for hit in arr {
        let id = hit["id"].as_str().unwrap().to_string();
        let got = ok_one(&client, &format!(r#"get(id="{}")"#, id)).await?;
        assert_eq!(
            got["data"]["kind"], "concept",
            "search(kind=\"concept\") returned non-concept: {got}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn search_with_granular_task_kind() -> anyhow::Result<()> {
    let client = connect().await?;
    ok_one(&client, r#"assign(title="urgent search needle one")"#).await?;
    ok_one(
        &client,
        r#"create(kind="observation", content="urgent search needle two")"#,
    )
    .await?;

    let hits = ok_one(
        &client,
        r#"search(kind="task", query="urgent search needle", limit=10)"#,
    )
    .await?;
    let arr = hits.as_array().expect("array");
    assert!(!arr.is_empty(), "expected task hits");
    for hit in arr {
        let id = hit["id"].as_str().unwrap().to_string();
        let got = ok_one(&client, &format!(r#"get(id="{}")"#, id)).await?;
        assert_eq!(
            got["data"]["kind"], "task",
            "search(kind=\"task\") returned non-task: {got}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn search_substrate_wide_note_kind_still_works() -> anyhow::Result<()> {
    let client = connect().await?;
    ok_one(
        &client,
        r#"assign(title="quasiparticle task entry", description="quasiparticle decoherence backlog")"#,
    )
    .await?;
    ok_one(
        &client,
        r#"create(kind="observation", content="quasiparticle decoherence drives loss in transmons")"#,
    )
    .await?;

    // Backwards-compat: kind="note" still ranges over every note kind.
    let hits = ok_one(
        &client,
        r#"search(kind="note", query="quasiparticle decoherence", limit=10)"#,
    )
    .await?;
    let arr = hits.as_array().expect("array");
    assert!(
        arr.len() >= 2,
        "kind=note should range over task AND observation; got {arr:?}"
    );
    Ok(())
}

#[tokio::test]
async fn search_unknown_kind_lists_all_valid_options() -> anyhow::Result<()> {
    let client = connect().await?;
    let result = call(
        &client,
        "request",
        json!({"ops": r#"search(kind="bogus", query="anything")"#}),
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    let first = &body["results"][0];
    assert_eq!(first["ok"], false);
    let err = first["error"].as_str().unwrap();
    assert!(err.contains("bogus"), "error names the bad kind: {err}");
    // The merged list must include substrate-level + pack-registered kinds.
    for expected in ["entity", "note", "edge", "concept", "task"] {
        assert!(
            err.contains(expected),
            "error must list {expected:?}: {err}"
        );
    }
    Ok(())
}

// ── Sub-filter contract: substrate `kind` + legacy `entity_kind`/`note_kind` ──

#[tokio::test]
async fn search_substrate_kind_entity_with_legacy_entity_kind_sub_filter() -> anyhow::Result<()> {
    // ADR-023 §`kind` parameter: substrate `kind="entity"` must honor the
    // legacy `entity_kind` sub-filter and behave identically to granular form.
    let client = connect().await?;
    ok_one(
        &client,
        r#"create(kind="concept", name="SubFilterEntityConcept", description="zaphod beeblebrox marker")"#,
    )
    .await?;
    ok_one(
        &client,
        r#"create(kind="document", name="SubFilterEntityDoc", description="zaphod beeblebrox marker")"#,
    )
    .await?;

    let hits = ok_one(
        &client,
        r#"search(kind="entity", entity_kind="concept", query="zaphod beeblebrox", limit=10)"#,
    )
    .await?;
    let arr = hits.as_array().expect("array");
    assert!(!arr.is_empty(), "expected concept hits, got: {arr:?}");
    for hit in arr {
        let id = hit["id"].as_str().unwrap().to_string();
        let got = ok_one(&client, &format!(r#"get(id="{}")"#, id)).await?;
        assert_eq!(
            got["data"]["kind"], "concept",
            "search(kind=\"entity\", entity_kind=\"concept\") returned non-concept: {got}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn search_substrate_kind_note_with_legacy_note_kind_sub_filter() -> anyhow::Result<()> {
    // ADR-023 §`kind` parameter: substrate `kind="note"` must honor the
    // legacy `note_kind` sub-filter and behave identically to granular form.
    let client = connect().await?;
    ok_one(
        &client,
        r#"assign(title="ghyll task entry", description="ghyll mistral foxtrot marker")"#,
    )
    .await?;
    ok_one(
        &client,
        r#"create(kind="observation", content="ghyll mistral foxtrot marker observation")"#,
    )
    .await?;

    let hits = ok_one(
        &client,
        r#"search(kind="note", note_kind="task", query="ghyll mistral foxtrot", limit=10)"#,
    )
    .await?;
    let arr = hits.as_array().expect("array");
    assert!(!arr.is_empty(), "expected task hits, got: {arr:?}");
    for hit in arr {
        let id = hit["id"].as_str().unwrap().to_string();
        let got = ok_one(&client, &format!(r#"get(id="{}")"#, id)).await?;
        assert_eq!(
            got["data"]["kind"], "task",
            "search(kind=\"note\", note_kind=\"task\") returned non-task: {got}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn search_granular_kind_contradicting_legacy_subfield_is_rejected() -> anyhow::Result<()> {
    // ADR-023 §`kind` parameter contradiction rule: granular `kind="concept"`
    // with `entity_kind="document"` must be rejected, not silently coerced.
    let client = connect().await?;
    let result = call(
        &client,
        "request",
        json!({"ops": r#"search(kind="concept", entity_kind="document", query="anything", limit=5)"#}),
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    let first = &body["results"][0];
    assert_eq!(first["ok"], false, "expected contradiction error: {first}");
    let err = first["error"].as_str().unwrap();
    assert!(
        err.contains("contradicts"),
        "error should explain the contradiction: {err}"
    );
    Ok(())
}

#[tokio::test]
async fn search_kind_filter_surfaces_right_kind_when_wrong_kind_outranks() -> anyhow::Result<()> {
    // Regression: previously the kind filter applied AFTER truncating fused
    // candidates to `limit`, so right-kind hits ranked below `limit` got
    // dropped. The fix defers truncation until after the alive+kind filter.
    //
    // Setup: 5 documents matching the query (likely to dominate the top of
    // the fused list) + 1 concept matching the same query. With limit=2,
    // pre-fix would return 0 hits when the top-2 fused are all documents;
    // post-fix the kind filter retains the lone concept from the wider
    // candidate pool (limit * 4 = 8).
    let client = connect().await?;
    for i in 0..5 {
        ok_one(
            &client,
            &format!(
                r#"create(kind="document", name="WrongKindDoc{i}", description="orthogonal wavelet quibble marker")"#
            ),
        )
        .await?;
    }
    ok_one(
        &client,
        r#"create(kind="concept", name="RightKindConcept", description="orthogonal wavelet quibble marker")"#,
    )
    .await?;

    let hits = ok_one(
        &client,
        r#"search(kind="concept", query="orthogonal wavelet quibble", limit=2)"#,
    )
    .await?;
    let arr = hits.as_array().expect("array");
    assert!(
        !arr.is_empty(),
        "right-kind hit must surface even when wrong-kind hits outrank it; got: {arr:?}"
    );
    for hit in arr {
        let id = hit["id"].as_str().unwrap().to_string();
        let got = ok_one(&client, &format!(r#"get(id="{}")"#, id)).await?;
        assert_eq!(
            got["data"]["kind"], "concept",
            "search(kind=\"concept\") must only return concepts: {got}"
        );
    }
    Ok(())
}

// ── Structured KhiveError preservation through the MCP boundary ──────────────

/// A minimal mock pack whose single verb always returns a `RuntimeError::Khive`
/// with code + details + retry_hint set. Used to verify that the MCP per-op
/// serializer emits a structured JSON error object (not a flat string).
struct ErrorInjectPack;

impl khive_types::Pack for ErrorInjectPack {
    const NAME: &'static str = "error-inject";
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = &[HandlerDef {
        name: "always_fail",
        description: "always returns a KhiveError::unavailable with code + details",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[],
    }];
}

#[async_trait]
impl PackRuntime for ErrorInjectPack {
    fn name(&self) -> &str {
        "error-inject"
    }

    fn note_kinds(&self) -> &'static [&'static str] {
        &[]
    }

    fn entity_kinds(&self) -> &'static [&'static str] {
        &[]
    }

    fn handlers(&self) -> &'static [HandlerDef] {
        ErrorInjectPack::HANDLERS
    }

    async fn dispatch(
        &self,
        _verb: &str,
        _params: serde_json::Value,
        _registry: &VerbRegistry,
        _token: &NamespaceToken,
    ) -> Result<serde_json::Value, RuntimeError> {
        let err = KhiveError::unavailable("downstream service offline")
            .with_code(KhiveErrorCode::new(ErrorDomain::Runtime, 10))
            .with_details(Details::new([
                ("service", "embed"),
                ("region", "us-east-1"),
            ]));
        Err(RuntimeError::Khive(err))
    }
}

/// Build a server backed only by the `ErrorInjectPack` (no DB, no embedding).
fn make_error_inject_server() -> KhiveMcpServer {
    let mut builder = VerbRegistryBuilder::new();
    builder.register(ErrorInjectPack);
    let registry = builder.build().expect("error-inject registry builds");
    KhiveMcpServer::from_registry(registry)
}

async fn connect_error_inject(
) -> anyhow::Result<impl std::ops::Deref<Target = rmcp::service::Peer<rmcp::RoleClient>>> {
    let (server_transport, client_transport) = tokio::io::duplex(65536);
    let server = make_error_inject_server();
    tokio::spawn(async move {
        if let Ok(svc) = server.serve(server_transport).await {
            let _ = svc.waiting().await;
        }
    });
    let client = DummyClient.serve(client_transport).await?;
    Ok(client)
}

/// `RuntimeError::Khive` must survive the MCP per-op boundary as a structured
/// JSON object — not collapsed to a flat string via `Display`.
///
/// Verifies:
/// - `error` is a JSON object (not a string)
/// - `error.kind` is present (snake_case string)
/// - `error.message` is present
/// - `error.code` is present as a wire string (e.g. "runtime:10")
/// - `error.details` is a non-null JSON object
/// - Non-Khive errors still produce a flat string (backward-compat check via
///   the existing `unknown_verb_returns_per_op_failure_not_invalid_params` test)
#[tokio::test]
async fn runtime_khive_error_serializes_as_structured_object() -> anyhow::Result<()> {
    let client = connect_error_inject().await?;
    let result = call(
        &client,
        "request",
        serde_json::json!({"ops": "always_fail()"}),
    )
    .await?;
    let body: serde_json::Value = serde_json::from_str(&first_text(&result))?;
    let first = &body["results"][0];

    // The op failed.
    assert_eq!(first["ok"], false, "expected op failure: {first}");

    // `error` must be an object, not a string.
    let error = &first["error"];
    assert!(
        error.is_object(),
        "error must be a JSON object (not a string); got: {error}"
    );

    // Required fields must be present.
    assert!(
        error["kind"].is_string(),
        "error.kind must be a string; got: {error}"
    );
    assert!(
        error["message"].is_string(),
        "error.message must be a string; got: {error}"
    );
    assert!(
        error["code"].is_string(),
        "error.code must be a wire string (e.g. 'runtime:10'); got: {error}"
    );
    assert!(
        error["details"].is_object(),
        "error.details must be a JSON object; got: {error}"
    );

    // Spot-check values.
    assert_eq!(
        error["kind"].as_str().unwrap(),
        "unavailable",
        "KhiveError::unavailable should map to kind='unavailable'"
    );
    assert_eq!(
        error["code"].as_str().unwrap(),
        "runtime:10",
        "ErrorCode(Runtime, 10) should serialize as 'runtime:10'"
    );
    assert_eq!(
        error["details"]["service"].as_str().unwrap(),
        "embed",
        "details key 'service' should be preserved"
    );

    Ok(())
}

// ── engine_config integration ─────────────────────────────────────────────────

/// Write a fake config.toml with 3 engines, build a KhiveRuntime from it, and
/// confirm that `registered_embedding_model_names()` returns all 3 model names.
///
/// This test verifies the full pipeline:
///   KhiveConfig::load  →  runtime_config_from_khive_config  →  KhiveRuntime::new
///   →  registered_embedding_model_names
#[test]
fn engine_config_three_engines_all_registered() {
    use khive_runtime::{
        runtime_config_from_khive_config, KhiveConfig, KhiveRuntime, RuntimeConfig,
    };
    use std::io::Write;

    // Write a config.toml with 3 engines.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    writeln!(
        std::fs::File::create(&path).unwrap(),
        r#"
[[engines]]
name = "primary"
model = "all-minilm-l6-v2"
default = true

[[engines]]
name = "para"
model = "paraphrase-multilingual-minilm-l12-v2"

[[engines]]
name = "bge-small"
model = "bge-small-en-v1.5"
"#
    )
    .unwrap();

    let khive_cfg = KhiveConfig::load(Some(&path))
        .expect("load should succeed")
        .expect("file should be found");
    assert_eq!(khive_cfg.engines.len(), 3);

    // Build RuntimeConfig from the KhiveConfig.
    let base = RuntimeConfig {
        db_path: None,
        embedding_model: None,
        additional_embedding_models: vec![],
        ..RuntimeConfig::default()
    };
    let config = runtime_config_from_khive_config(&khive_cfg, base);
    assert!(
        config.embedding_model.is_some(),
        "default engine should set embedding_model"
    );
    assert_eq!(
        config.additional_embedding_models.len(),
        2,
        "two non-default engines should appear in additional_embedding_models"
    );

    // Create runtime and verify all 3 are registered.
    let rt = KhiveRuntime::new(config).expect("runtime should build");
    let mut names = rt.registered_embedding_model_names();
    names.sort();

    // The canonical to_string() forms of the models.
    let expected_substring_check = [
        "all-minilm-l6-v2",
        "bge-small-en-v1.5",
        "paraphrase-multilingual-minilm-l12-v2",
    ];
    assert_eq!(
        names.len(),
        3,
        "all 3 engines should be registered; got {names:?}"
    );
    for expected in &expected_substring_check {
        assert!(
            names.iter().any(|n| n.contains(expected)),
            "expected a registered model containing {expected:?}; registered: {names:?}"
        );
    }
}

// ── Chain $prev dispatch tests (ADR-016) ─────────────────────────────────────
//
// These tests verify that $prev / $prev.dotted.path references in chain ops are
// resolved against the prior op's canonical result BEFORE dispatch — not passed
// through as literal strings.  The four cases mirror the UE4 DSL critical finding.

/// Chain: assign a task then complete it using $prev.id.
///
/// The canonical result of `assign` contains an `id` field (short UUID).
/// `$prev.id` must resolve to that value so `complete` receives a valid ID.
#[tokio::test]
async fn test_prev_dot_id_resolves() -> anyhow::Result<()> {
    let client = connect().await?;

    let result = call(
        &client,
        "request",
        json!({
            "ops": r#"assign(title="chain-prev-id-test", status="next") | complete(id=$prev.id)"#,
            "presentation": "verbose"
        }),
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    let results = body["results"].as_array().expect("results array");

    assert_eq!(results.len(), 2, "expected 2 ops in chain result");
    assert_eq!(
        results[0]["ok"],
        json!(true),
        "assign (op 0) must succeed: {}",
        results[0]
    );
    assert_eq!(
        results[1]["ok"],
        json!(true),
        "complete (op 1) must succeed — $prev.id was not resolved: {}",
        results[1]
    );
    assert_eq!(body["summary"]["succeeded"], json!(2));
    assert_eq!(body["summary"]["failed"], json!(0));
    assert_eq!(body["summary"]["aborted"], json!(0));

    // The completed task must have status "done".
    let complete_result = &results[1]["result"];
    assert_eq!(
        complete_result["to"].as_str().unwrap_or(""),
        "done",
        "completed task must have to=done: {complete_result}"
    );
    Ok(())
}

/// Chain: create a concept entity, then link it to a pre-created target using
/// $prev.id (op 0 result), then fetch the link using $prev.id (op 1 result).
///
/// This verifies that $prev.field correctly walks single-level dotted paths in
/// a 3-op chain, and that $prev always refers to the IMMEDIATELY preceding op.
#[tokio::test]
async fn test_prev_dotted_path_resolves() -> anyhow::Result<()> {
    let client = connect().await?;

    // Create a target entity first (outside the chain — we need its id).
    // Entity create results expose "id" (short 8-char form); full UUID is not
    // separately aliased for entities (unlike task notes which use "full_id").
    let target = ok_one(
        &client,
        r#"create(kind="entity", entity_kind="concept", name="PrevDottedTarget")"#,
    )
    .await?;
    let target_id = target["id"]
        .as_str()
        .expect("id field on entity result")
        .to_string();

    // Chain: create source | link (uses $prev.id from create) | get (uses $prev.id from link)
    let ops = format!(
        r#"create(kind="entity", entity_kind="concept", name="PrevDottedSource") | link(source_id=$prev.id, target_id="{target_id}", relation="extends") | get(id=$prev.id)"#
    );
    let result = call(
        &client,
        "request",
        json!({"ops": ops, "presentation": "verbose"}),
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    let results = body["results"].as_array().expect("results array");

    assert_eq!(results.len(), 3, "expected 3 ops");
    assert_eq!(
        results[0]["ok"],
        json!(true),
        "create failed: {}",
        results[0]
    );
    assert_eq!(
        results[1]["ok"],
        json!(true),
        "link failed — $prev.id (create result) not resolved: {}",
        results[1]
    );
    assert_eq!(
        results[2]["ok"],
        json!(true),
        "get failed — $prev.id (link result) not resolved: {}",
        results[2]
    );
    assert_eq!(body["summary"]["succeeded"], json!(3));
    assert_eq!(body["summary"]["aborted"], json!(0));

    // The link result should have source_id matching the created entity.
    let source_id = results[0]["result"]["id"]
        .as_str()
        .unwrap_or_else(|| results[0]["result"]["full_id"].as_str().unwrap_or(""));
    let link_source = results[1]["result"]["source_id"].as_str().unwrap_or("");
    assert!(
        link_source.starts_with(source_id) || source_id.starts_with(link_source),
        "link.source_id {link_source:?} should match created entity {source_id:?}"
    );
    Ok(())
}

/// Chain abort: second op references a non-existent $prev field.
///
/// The failing op must have ok=false with an error message referencing the
/// unavailable path.  All subsequent ops must be marked aborted (ok=false,
/// aborted=true).  Summary: succeeded=1, failed=1, aborted=1.
#[tokio::test]
async fn test_prev_unresolvable_aborts_chain() -> anyhow::Result<()> {
    let client = connect().await?;

    let ops = r#"create(kind="entity", entity_kind="concept", name="AbortSource") | get(id=$prev.bogus_field_xyz) | create(kind="entity", entity_kind="concept", name="AbortSink")"#;
    let result = call(
        &client,
        "request",
        json!({"ops": ops, "presentation": "verbose"}),
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    let results = body["results"].as_array().expect("results array");

    assert_eq!(results.len(), 3, "expected 3 ops in chain result");

    // Op 0: create must succeed.
    assert_eq!(
        results[0]["ok"],
        json!(true),
        "create (op 0) must succeed: {}",
        results[0]
    );

    // Op 1: get with unresolvable $prev path must fail (not be silently ok).
    assert_eq!(
        results[1]["ok"],
        json!(false),
        "get with bogus $prev path (op 1) must fail: {}",
        results[1]
    );
    // The error message must reference the path that could not be resolved.
    let err_obj = &results[1]["error"];
    let err_str = err_obj
        .as_str()
        .unwrap_or_else(|| err_obj["message"].as_str().unwrap_or(""));
    assert!(
        err_str.contains("bogus_field_xyz") || err_str.contains("not found"),
        "error must mention the unresolvable path; got: {err_str}"
    );
    // The failing op itself must NOT be marked aborted.
    assert_ne!(
        results[1]["aborted"],
        json!(true),
        "the failing op (op 1) must not be marked aborted: {}",
        results[1]
    );

    // Op 2: must be aborted because op 1 failed.
    assert_eq!(
        results[2]["ok"],
        json!(false),
        "aborted op (op 2) must have ok=false: {}",
        results[2]
    );
    assert_eq!(
        results[2]["aborted"],
        json!(true),
        "aborted op (op 2) must have aborted=true: {}",
        results[2]
    );

    assert_eq!(body["summary"]["total"], json!(3));
    assert_eq!(body["summary"]["succeeded"], json!(1));
    assert_eq!(body["summary"]["failed"], json!(1));
    assert_eq!(body["summary"]["aborted"], json!(1));
    Ok(())
}

/// Chain: $prev bare (no dot path) substitutes the entire prior op's result object.
///
/// We use `assign | complete(id=$prev.id, result=$prev)` — the `result` arg
/// receives the entire assign result JSON object.  The substitution itself must
/// succeed (no "unresolved $prev" error).  Even if `complete` rejects the object
/// value for `result`, the failure must not be a substitution error.
#[tokio::test]
async fn test_prev_bare_resolves_full_result() -> anyhow::Result<()> {
    let client = connect().await?;

    let result = call(
        &client,
        "request",
        json!({
            "ops": r#"assign(title="bare-prev-test") | complete(id=$prev.id, result=$prev)"#,
            "presentation": "verbose"
        }),
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    let results = body["results"].as_array().expect("results array");

    assert_eq!(results.len(), 2, "expected 2 ops");
    assert_eq!(
        results[0]["ok"],
        json!(true),
        "assign must succeed: {}",
        results[0]
    );

    // Op 1 uses $prev.id (resolves to ID string) and $prev (resolves to the
    // whole assign result object).  Whether or not complete succeeds, the
    // failure must NOT be a substitution error mentioning "$prev".
    let op1_err = {
        let e = &results[1]["error"];
        e.as_str()
            .unwrap_or_else(|| e["message"].as_str().unwrap_or(""))
            .to_string()
    };
    assert!(
        !op1_err.contains("$prev"),
        "op 1 error must not mention '$prev' — bare substitution must have succeeded; got: {op1_err}"
    );
    Ok(())
}

// ── help=true schema envelope integration tests ─────────────────────────────
//
// These tests confirm that help=true calls through the MCP surface return
// non-empty params slices with specific known parameters — verifying that
// the HandlerDef.params slices are populated (not left as &[]).

fn make_full_server() -> KhiveMcpServer {
    let config = RuntimeConfig {
        db_path: None,
        default_namespace: Namespace::parse("test").unwrap(),
        embedding_model: None,
        packs: vec![
            "kg".to_string(),
            "gtd".to_string(),
            "memory".to_string(),
            "brain".to_string(),
        ],
        ..RuntimeConfig::default()
    };
    let runtime = KhiveRuntime::new(config).expect("in-memory runtime with all packs");
    KhiveMcpServer::new(runtime).expect("server builds with kg+gtd+memory+brain")
}

async fn connect_full(
) -> anyhow::Result<impl std::ops::Deref<Target = rmcp::service::Peer<rmcp::RoleClient>>> {
    let (server_transport, client_transport) = tokio::io::duplex(65536);
    let server = make_full_server();
    tokio::spawn(async move {
        if let Ok(server_service) = server.serve(server_transport).await {
            let _ = server_service.waiting().await;
        }
    });
    let client = DummyClient.serve(client_transport).await?;
    Ok(client)
}

/// Helper: call `verb(help=true)` through the MCP surface and return the
/// parsed result. Asserts the op succeeded and returns the schema envelope.
async fn help_schema(
    client: &impl std::ops::Deref<Target = rmcp::service::Peer<rmcp::RoleClient>>,
    verb: &str,
) -> anyhow::Result<Value> {
    let ops = format!("{verb}(help=true)");
    let result = call(client, "request", json!({"ops": &ops})).await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    let first = body["results"].get(0).cloned().unwrap_or(Value::Null);
    assert_eq!(
        first["ok"],
        json!(true),
        "{verb}(help=true) must succeed, got: {first}"
    );
    Ok(first["result"].clone())
}

#[tokio::test]
async fn help_recall_params_non_empty_with_query_param() -> anyhow::Result<()> {
    let client = connect_full().await?;
    let schema = help_schema(&client, "recall").await?;
    let params = schema["params"]
        .as_array()
        .expect("params must be an array");
    assert!(
        !params.is_empty(),
        "recall help=true must return non-empty params; got empty slice"
    );
    let has_query = params.iter().any(|p| p["name"] == json!("query"));
    assert!(
        has_query,
        "recall params must include 'query'; got: {params:?}"
    );
    Ok(())
}

#[tokio::test]
async fn help_brain_feedback_params_non_empty_with_target_and_signal() -> anyhow::Result<()> {
    let client = connect_full().await?;
    let schema = help_schema(&client, "brain.feedback").await?;
    let params = schema["params"]
        .as_array()
        .expect("params must be an array");
    assert!(
        !params.is_empty(),
        "brain.feedback help=true must return non-empty params"
    );
    let has_target_id = params.iter().any(|p| p["name"] == json!("target_id"));
    assert!(
        has_target_id,
        "brain.feedback params must include 'target_id'; got: {params:?}"
    );
    let has_signal = params.iter().any(|p| p["name"] == json!("signal"));
    assert!(
        has_signal,
        "brain.feedback params must include 'signal'; got: {params:?}"
    );
    Ok(())
}

#[tokio::test]
async fn help_propose_params_non_empty_with_title_description_changeset() -> anyhow::Result<()> {
    let client = connect_full().await?;
    let schema = help_schema(&client, "propose").await?;
    let params = schema["params"]
        .as_array()
        .expect("params must be an array");
    assert!(
        !params.is_empty(),
        "propose help=true must return non-empty params"
    );
    let has_title = params.iter().any(|p| p["name"] == json!("title"));
    assert!(
        has_title,
        "propose params must include 'title'; got: {params:?}"
    );
    let has_description = params.iter().any(|p| p["name"] == json!("description"));
    assert!(
        has_description,
        "propose params must include 'description'; got: {params:?}"
    );
    let has_changeset = params.iter().any(|p| p["name"] == json!("changeset"));
    assert!(
        has_changeset,
        "propose params must include 'changeset'; got: {params:?}"
    );
    Ok(())
}

// ── help=true schema envelopes for comm + schedule verbs (issue #287) ─────────

fn make_comm_schedule_server() -> KhiveMcpServer {
    let config = RuntimeConfig {
        db_path: None,
        default_namespace: Namespace::parse("test").unwrap(),
        embedding_model: None,
        packs: vec!["kg".to_string(), "comm".to_string(), "schedule".to_string()],
        ..RuntimeConfig::default()
    };
    let runtime = KhiveRuntime::new(config).expect("in-memory runtime with comm+schedule");
    KhiveMcpServer::new(runtime).expect("server builds with kg+comm+schedule")
}

async fn connect_comm_schedule(
) -> anyhow::Result<impl std::ops::Deref<Target = rmcp::service::Peer<rmcp::RoleClient>>> {
    let (server_transport, client_transport) = tokio::io::duplex(65536);
    let server = make_comm_schedule_server();
    tokio::spawn(async move {
        if let Ok(svc) = server.serve(server_transport).await {
            let _ = svc.waiting().await;
        }
    });
    let client = DummyClient.serve(client_transport).await?;
    Ok(client)
}

/// `send(help=true)` must return a non-empty params array with required `to` and `content`.
#[tokio::test]
async fn send_help_returns_required_to_and_content() -> anyhow::Result<()> {
    let client = connect_comm_schedule().await?;
    let result = ok_one(&client, "send(help=true)").await?;

    assert_eq!(result["verb"], "send");
    assert_eq!(result["pack"], "comm");

    let params = result["params"]
        .as_array()
        .expect("params must be an array");
    assert!(!params.is_empty(), "send help must have non-empty params");

    let to = params
        .iter()
        .find(|p| p["name"] == "to")
        .expect("send help must include 'to'");
    assert_eq!(to["required"], serde_json::json!(true));

    let content = params
        .iter()
        .find(|p| p["name"] == "content")
        .expect("send help must include 'content'");
    assert_eq!(content["required"], serde_json::json!(true));

    Ok(())
}

/// `inbox(help=true)` must return optional `limit` and `status`.
#[tokio::test]
async fn inbox_help_returns_optional_limit_and_status() -> anyhow::Result<()> {
    let client = connect_comm_schedule().await?;
    let result = ok_one(&client, "inbox(help=true)").await?;

    assert_eq!(result["verb"], "inbox");
    assert_eq!(result["pack"], "comm");

    let params = result["params"]
        .as_array()
        .expect("params must be an array");
    assert!(!params.is_empty(), "inbox help must have non-empty params");

    let limit = params
        .iter()
        .find(|p| p["name"] == "limit")
        .expect("inbox help must include 'limit'");
    assert_eq!(limit["required"], serde_json::json!(false));

    let status = params
        .iter()
        .find(|p| p["name"] == "status")
        .expect("inbox help must include 'status'");
    assert_eq!(status["required"], serde_json::json!(false));

    Ok(())
}

/// `schedule(help=true)` must return required `action` and `at`.
#[tokio::test]
async fn schedule_help_returns_required_action_and_at() -> anyhow::Result<()> {
    let client = connect_comm_schedule().await?;
    let result = ok_one(&client, "schedule(help=true)").await?;

    assert_eq!(result["verb"], "schedule");
    assert_eq!(result["pack"], "schedule");

    let params = result["params"]
        .as_array()
        .expect("params must be an array");
    assert!(
        !params.is_empty(),
        "schedule help must have non-empty params"
    );

    let action = params
        .iter()
        .find(|p| p["name"] == "action")
        .expect("schedule help must include 'action'");
    assert_eq!(action["required"], serde_json::json!(true));

    let at = params
        .iter()
        .find(|p| p["name"] == "at")
        .expect("schedule help must include 'at'");
    assert_eq!(at["required"], serde_json::json!(true));

    Ok(())
}

/// `remind(help=true)` must return required `content` and `at`, optional `repeat`.
#[tokio::test]
async fn remind_help_returns_required_content_and_at() -> anyhow::Result<()> {
    let client = connect_comm_schedule().await?;
    let result = ok_one(&client, "remind(help=true)").await?;

    assert_eq!(result["verb"], "remind");
    assert_eq!(result["pack"], "schedule");

    let params = result["params"]
        .as_array()
        .expect("params must be an array");
    assert!(!params.is_empty(), "remind help must have non-empty params");

    let content = params
        .iter()
        .find(|p| p["name"] == "content")
        .expect("remind help must include 'content'");
    assert_eq!(content["required"], serde_json::json!(true));

    let at = params
        .iter()
        .find(|p| p["name"] == "at")
        .expect("remind help must include 'at'");
    assert_eq!(at["required"], serde_json::json!(true));

    let repeat = params
        .iter()
        .find(|p| p["name"] == "repeat")
        .expect("remind help must include 'repeat'");
    assert_eq!(repeat["required"], serde_json::json!(false));

    Ok(())
}

// ── Fix 1: run_migrations() at MCP startup ──────────────────────────────────

/// V15 (`proposals_open`) and V16/V17 (vec `embedding_model` column) are
/// applied by `KhiveRuntime::new` before any pack handler runs.  Without the
/// fix, `propose(...)` fails with "no such table: proposals_open" on a fresh
/// file-backed database.
///
/// This test creates a fresh tempfile-backed runtime (the path is not
/// pre-migrated), creates a `propose` op, and asserts it succeeds — proving
/// the migration ran at construction time.
#[tokio::test]
async fn startup_migrations_applied_to_fresh_file_backed_db() -> anyhow::Result<()> {
    let db_file = tempfile::NamedTempFile::new()?;
    let config = RuntimeConfig {
        db_path: Some(db_file.path().to_path_buf()),
        default_namespace: Namespace::parse("fix1test").unwrap(),
        embedding_model: None,
        packs: vec!["kg".to_string()],
        ..RuntimeConfig::default()
    };
    let runtime = KhiveRuntime::new(config).expect("fresh file-backed runtime");
    let server = KhiveMcpServer::new(runtime).expect("server builds");

    let (server_transport, client_transport) = tokio::io::duplex(65536);
    tokio::spawn(async move {
        if let Ok(svc) = server.serve(server_transport).await {
            let _ = svc.waiting().await;
        }
    });
    let client = DummyClient.serve(client_transport).await?;

    // First create an entity to propose a change against.
    let entity = ok_one(
        &client,
        r#"create(kind="entity", entity_kind="concept", name="MigrationTarget")"#,
    )
    .await?;
    // Entity create in verbose mode returns `id` (full UUID), not `full_id`.
    let eid = entity["id"].as_str().unwrap().to_string();

    // `propose` writes to proposals_open (V15). Before the fix this would
    // crash with "no such table: proposals_open" on a fresh DB.
    //
    // Use the JSON batch form to pass the nested changeset without DSL quoting
    // issues — the JSON form is equivalent per ADR-020 §§.
    let ops = serde_json::to_string(&json!([{
        "tool": "propose",
        "args": {
            "title": "migration regression test",
            "description": "fix1: run_migrations at startup",
            "changeset": {
                "kind": "add_entity",
                "entity": format!(r#"{{"kind":"concept","name":"fix1-{eid}"}}"#)
            }
        }
    }]))
    .unwrap();
    let result = call(
        &client,
        "request",
        json!({
            "ops": ops,
            "presentation": "verbose"
        }),
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    let first = &body["results"][0];
    assert_eq!(
        first["ok"], true,
        "propose must succeed on a freshly-migrated DB; got: {first}"
    );
    Ok(())
}

// ── Fix 2: Visibility::Subhandler gate ──────────────────────────────────────

/// `brain.state`, `brain.config`, `brain.events`, and `brain.emit` are
/// tagged `Visibility::Subhandler` in the brain pack.  The MCP request
/// surface must reject them with a per-op `{ok: false}` rather than routing
/// to the handler.  `help=true` introspection must still work (short-circuit
/// before the gate).
fn make_brain_server() -> KhiveMcpServer {
    let config = RuntimeConfig {
        db_path: None,
        default_namespace: Namespace::parse("braintest").unwrap(),
        embedding_model: None,
        packs: vec!["kg".to_string(), "brain".to_string()],
        ..RuntimeConfig::default()
    };
    let runtime = KhiveRuntime::new(config).expect("kg+brain runtime");
    KhiveMcpServer::new(runtime).expect("server builds with kg+brain")
}

#[tokio::test]
async fn subhandler_verbs_are_blocked_at_mcp_boundary() -> anyhow::Result<()> {
    let (server_transport, client_transport) = tokio::io::duplex(65536);
    let server = make_brain_server();
    tokio::spawn(async move {
        if let Ok(svc) = server.serve(server_transport).await {
            let _ = svc.waiting().await;
        }
    });
    let client = DummyClient.serve(client_transport).await?;

    // All four Subhandler verbs must be rejected.
    for verb in &["brain.state", "brain.config", "brain.events", "brain.emit"] {
        let result = call(&client, "request", json!({"ops": format!("{verb}()")})).await?;
        let body: Value = serde_json::from_str(&first_text(&result))?;
        let first = &body["results"][0];
        assert_eq!(
            first["ok"], false,
            "Subhandler verb {verb:?} must be blocked: got {first}"
        );
        let err = first["error"].as_str().unwrap_or("");
        assert!(
            err.contains("permission denied") || err.contains("subhandler"),
            "error for {verb:?} must mention permission/subhandler: {err}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn subhandler_verb_help_introspection_still_works() -> anyhow::Result<()> {
    let (server_transport, client_transport) = tokio::io::duplex(65536);
    let server = make_brain_server();
    tokio::spawn(async move {
        if let Ok(svc) = server.serve(server_transport).await {
            let _ = svc.waiting().await;
        }
    });
    let client = DummyClient.serve(client_transport).await?;

    // `help=true` is short-circuited before the visibility gate — must succeed.
    let result = ok_one(&client, r#"brain.state(help=true)"#).await?;
    // Help response includes the verb name or param list.
    let text = serde_json::to_string(&result).unwrap_or_default();
    assert!(
        text.contains("brain.state") || text.contains("params") || text.contains("help"),
        "help response for Subhandler verb must return introspection data: {text}"
    );
    Ok(())
}

// ── Fix 3: AlwaysVerbose verbs return full UUIDs in default (Agent) mode ────

/// `get` and `link` are declared AlwaysVerbose (ADR-045 §6).  Without the
/// fix, they return 8-char short UUIDs in default (Agent) mode — callers
/// cannot chain them into subsequent operations that require full UUIDs.
///
/// With the fix, both verbs return full 36-char UUIDs even when the request
/// uses the default presentation (Agent mode).
#[tokio::test]
async fn get_returns_full_uuid_in_default_agent_mode() -> anyhow::Result<()> {
    let client = connect().await?;

    // Create in verbose mode so we have the full UUID.
    // Entity create returns `id` (full UUID) in verbose mode.
    let created = ok_one(
        &client,
        r#"create(kind="entity", entity_kind="concept", name="AlwaysVerboseEntity")"#,
    )
    .await?;
    let full_id = created["id"].as_str().unwrap().to_string();
    assert_eq!(full_id.len(), 36, "created entity must have full UUID");

    // Fetch via `get` WITHOUT specifying presentation — default is Agent mode.
    // AlwaysVerbose must override and return the full UUID in the `id` field.
    let result = call(
        &client,
        "request",
        json!({"ops": format!(r#"get(id="{full_id}")"#)}),
        // Deliberately no `presentation` key — defaults to Agent.
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    let first = &body["results"][0];
    assert_eq!(first["ok"], true, "get must succeed: {first}");

    // `get` wraps the entity in {"kind": "entity", "data": <entity>}.
    // The entity's UUID lives at result["data"]["id"].
    let entity = &first["result"];
    let returned_id = entity["data"]["id"].as_str().unwrap_or("");
    assert_eq!(
        returned_id.len(),
        36,
        "get in default (Agent) mode must return full 36-char UUID in data.id; got {returned_id:?}"
    );
    assert_eq!(
        returned_id, full_id,
        "returned id must match the created entity's full UUID"
    );
    Ok(())
}

#[tokio::test]
async fn link_returns_full_uuids_in_default_agent_mode() -> anyhow::Result<()> {
    let client = connect().await?;

    // Create two entities in verbose mode.
    let a = ok_one(
        &client,
        r#"create(kind="entity", entity_kind="concept", name="NodeA")"#,
    )
    .await?;
    let b = ok_one(
        &client,
        r#"create(kind="entity", entity_kind="concept", name="NodeB")"#,
    )
    .await?;
    // Entity create returns `id` (full UUID) in verbose mode.
    let a_id = a["id"].as_str().unwrap().to_string();
    let b_id = b["id"].as_str().unwrap().to_string();

    // Call `link` WITHOUT presentation=verbose — default is Agent mode.
    // AlwaysVerbose must override and return full UUIDs in source_id / target_id.
    let result = call(
        &client,
        "request",
        json!({
            "ops": format!(
                r#"link(source_id="{a_id}", target_id="{b_id}", relation="extends")"#
            )
            // No `presentation` key — defaults to Agent.
        }),
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    let first = &body["results"][0];
    assert_eq!(first["ok"], true, "link must succeed: {first}");

    let edge = &first["result"];
    let src = edge["source_id"].as_str().unwrap_or("");
    let tgt = edge["target_id"].as_str().unwrap_or("");
    assert_eq!(
        src.len(),
        36,
        "link source_id must be full 36-char UUID in default mode; got {src:?}"
    );
    assert_eq!(
        tgt.len(),
        36,
        "link target_id must be full 36-char UUID in default mode; got {tgt:?}"
    );
    Ok(())
}
