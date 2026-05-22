//! Integration tests for the request-only khive-mcp surface (ADR-020 + ADR-025).
//!
//! Validates the single-tool composition: every verb is reached via `request(ops="…")`.

use async_trait::async_trait;
use khive_mcp::server::KhiveMcpServer;
use khive_runtime::{
    KhiveRuntime, PackRuntime, RuntimeConfig, RuntimeError, VerbRegistry, VerbRegistryBuilder,
};
use khive_types::{Details, ErrorCode as KhiveErrorCode, ErrorDomain, KhiveError, Pack, VerbDef};
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
        ..RuntimeConfig::default()
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
        ..RuntimeConfig::default()
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
async fn pack_gtd_auto_loads_kg_via_transitive_requires() {
    // GTD declares requires(&["kg"]) — requesting only "gtd" must auto-load "kg"
    // so that kg verbs (e.g. "create") are present alongside gtd verbs (e.g. "assign").
    let config = RuntimeConfig {
        db_path: None,
        default_namespace: "test".to_string(),
        embedding_model: None,
        packs: vec!["gtd".to_string()],
        ..RuntimeConfig::default()
    };
    let runtime = KhiveRuntime::new(config).unwrap();
    let server = KhiveMcpServer::new(runtime);
    let info = server.get_info();
    let instructions = info.instructions.unwrap_or_default();
    assert!(instructions.contains("assign"), "gtd verb must be present");
    assert!(
        instructions.contains("create"),
        "kg verb must be auto-loaded via gtd's transitive requires"
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
    const VERBS: &'static [VerbDef] = &[VerbDef {
        name: "always_fail",
        description: "always returns a KhiveError::unavailable with code + details",
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

    fn verbs(&self) -> &'static [VerbDef] {
        ErrorInjectPack::VERBS
    }

    async fn dispatch(
        &self,
        _verb: &str,
        _params: serde_json::Value,
        _registry: &VerbRegistry,
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
