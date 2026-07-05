//! Integration tests for the request-only khive-mcp surface (ADR-016 + ADR-025).
//!
//! Validates the single-tool composition: every verb is reached via `request(ops="…")`.
//!
// FILE SIZE JUSTIFICATION: All test groups share the same in-process server
// helpers (make_server, connect, ok_one, DummyClient, first_text, call).
// Splitting into multiple files would require a `tests/common/` re-export
// module and would scatter the shared setup across files without reducing
// cognitive overhead. The single file makes it easy to verify that every
// section exercises the same server construction path and that helper changes
// propagate to all coverage areas simultaneously.

// Force the knowledge pack to be linked (inventory::submit! requires the crate
// to be linked into the test binary for its PackRegistration to self-register).
use khive_pack_knowledge as _;

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

fn disable_daemon() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| std::env::set_var("KHIVE_NO_DAEMON", "1"));
}

fn make_server() -> KhiveMcpServer {
    disable_daemon();
    let config = RuntimeConfig {
        db_path: None,
        default_namespace: Namespace::parse("test").unwrap(),
        embedding_model: None,
        additional_embedding_models: vec![],
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
    // Ops inside `[...]` are dispatched in parallel (ADR-016 §dispatch).
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
    // (ADR-016 §dispatch).
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

/// UE4-H2: empty batch `ops="[]"` must return an RPC-level `-32602 invalid_params`
/// error, not a `{results: [], summary: {total: 0}}` 200-style response.
#[tokio::test]
async fn empty_batch_returns_invalid_params() -> anyhow::Result<()> {
    let client = connect().await?;
    let err = call(&client, "request", json!({"ops": "[]"})).await.err();
    let svc = err.as_ref().and_then(|e| e.downcast_ref::<ServiceError>());
    assert!(
        matches!(
            svc,
            Some(ServiceError::McpError(e)) if e.code == ErrorCode::INVALID_PARAMS
        ),
        "UE4-H2: empty batch must return INVALID_PARAMS, got {err:?}"
    );
    // Also check JSON-form empty array.
    let err2 = call(&client, "request", json!({"ops": "[]"})).await.err();
    let svc2 = err2.as_ref().and_then(|e| e.downcast_ref::<ServiceError>());
    assert!(
        matches!(
            svc2,
            Some(ServiceError::McpError(e)) if e.code == ErrorCode::INVALID_PARAMS
        ),
        "UE4-H2: empty JSON batch must return INVALID_PARAMS, got {err2:?}"
    );
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
        r#"gtd.assign(title="ship release", status="next", priority="p0")"#,
    )
    .await?;
    let id = assigned["full_id"].as_str().unwrap().to_string();
    assert_eq!(assigned["kind"], "task");
    assert_eq!(assigned["status"], "next");

    let next_list = ok_one(&client, "gtd.next()").await?;
    let arr = next_list.as_array().unwrap();
    assert!(arr.iter().any(|t| t["full_id"] == id));

    let completed = ok_one(
        &client,
        &format!(r#"gtd.complete(id="{id}", result="shipped via request")"#),
    )
    .await?;
    assert_eq!(completed["to"], "done");
    Ok(())
}

#[tokio::test]
async fn transition_lifecycle_rejection_is_per_op_not_protocol_error() -> anyhow::Result<()> {
    let client = connect().await?;
    let assigned = ok_one(&client, r#"gtd.assign(title="lifecycle")"#).await?;
    let id = assigned["full_id"].as_str().unwrap().to_string();

    // inbox → done is allowed; done → inbox is NOT.
    ok_one(
        &client,
        &format!(r#"gtd.transition(id="{id}", status="done")"#),
    )
    .await?;

    let result = call(
        &client,
        "request",
        json!({"ops": format!(r#"gtd.transition(id="{id}", status="inbox")"#)}),
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
        gtd.assign(title="t1", priority="p0"),
        gtd.assign(title="t2", priority="p1"),
        gtd.assign(title="t3", priority="p2")
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
        additional_embedding_models: vec![],
        packs: vec!["kg".to_string()],
        ..RuntimeConfig::default()
    };
    let runtime = KhiveRuntime::new(config).unwrap();
    let server = KhiveMcpServer::new(runtime).expect("server builds with kg");
    let info = server.get_info();
    let instructions = info.instructions.unwrap_or_default();
    assert!(instructions.contains("create"), "kg verb missing");
    assert!(
        !instructions.contains("gtd.assign"),
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
        additional_embedding_models: vec![],
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
        additional_embedding_models: vec![],
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
        json!({"ops": r#"[{"tool":"gtd.assign","args":{"title":"json form","priority":"p1"}}]"#}),
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    assert_eq!(body["summary"]["succeeded"], 1);
    assert_eq!(body["results"][0]["result"]["title"], "json form");
    Ok(())
}

/// RUNTIME-AUD-002 (#433): a present-but-malformed `namespace` value entering
/// through the MCP JSON-form wire boundary must fail closed with a per-op
/// invalid-input error — never be silently coerced to the default namespace and
/// written. The JSON parser preserves the non-string value verbatim (it is not
/// dropped or treated as absent), so `VerbRegistry::dispatch` sees it and
/// rejects it before any gate Allow / storage write. This is the end-to-end
/// ingress counterpart to the runtime-layer `namespace_null_rejected_not_coerced`
/// gate-spy test.
#[tokio::test]
async fn json_form_namespace_non_string_returns_invalid_input() -> anyhow::Result<()> {
    let client = connect().await?;

    // Every non-string JSON type the finding enumerates, embedded in a real
    // JSON-form `create` op exactly as an MCP client would send it.
    let cases: [(&str, &str); 5] = [
        ("null", "null"),
        ("number", "42"),
        ("boolean", "true"),
        ("array", r#"["local"]"#),
        ("object", r#"{"ns":"local"}"#),
    ];

    for (label, ns_json) in cases {
        let ops = format!(
            r#"[{{"tool":"create","args":{{"kind":"entity","entity_kind":"concept","name":"aud002-{label}","namespace":{ns_json}}}}}]"#
        );
        let result = call(&client, "request", json!({ "ops": ops })).await?;
        let body: Value = serde_json::from_str(&first_text(&result))?;

        // Malformed namespace is a per-op validation failure, NOT a protocol
        // error — the JSON parses fine, so the batch is not aborted.
        let first = &body["results"][0];
        assert_eq!(
            first["ok"],
            json!(false),
            "case {label}: op must fail closed, got: {body}"
        );
        let err = first["error"].as_str().unwrap_or_default().to_lowercase();
        assert!(
            err.contains("namespace"),
            "case {label}: error must name the namespace, got: {first}"
        );

        // No local write may have slipped through under a coerced default.
        assert_eq!(
            body["summary"]["succeeded"], 0,
            "case {label}: no op may succeed, got: {body}"
        );
        assert_eq!(
            body["summary"]["failed"], 1,
            "case {label}: the malformed op must be counted as failed, got: {body}"
        );
    }

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
    let blocker = ok_one(&client, r#"gtd.assign(title="write spec")"#).await?;
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

    let blocker = ok_one(&client, r#"gtd.assign(title="write spec")"#).await?;
    let blocker_full = blocker["full_id"].as_str().unwrap().to_string();
    let dependent = ok_one(
        &client,
        &format!(
            r#"gtd.assign(title="implement feature", depends_on=["{}"])"#,
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
    ok_one(&client, r#"gtd.assign(title="GranularTaskA")"#).await?;
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
            got["kind"], "concept",
            "search(kind=\"concept\") returned non-concept: {got}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn search_with_granular_task_kind() -> anyhow::Result<()> {
    let client = connect().await?;
    ok_one(&client, r#"gtd.assign(title="urgent search needle one")"#).await?;
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
            got["kind"], "task",
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
        r#"gtd.assign(title="quasiparticle task entry", description="quasiparticle decoherence backlog")"#,
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
            got["kind"], "concept",
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
        r#"gtd.assign(title="ghyll task entry", description="ghyll mistral foxtrot marker")"#,
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
            got["kind"], "task",
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
            got["kind"], "concept",
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
    disable_daemon();
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
            "ops": r#"gtd.assign(title="chain-prev-id-test", status="next") | gtd.complete(id=$prev.id)"#,
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
        "gtd.assign (op 0) must succeed: {}",
        results[0]
    );
    assert_eq!(
        results[1]["ok"],
        json!(true),
        "gtd.complete (op 1) must succeed — $prev.id was not resolved: {}",
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

/// UE4-H1: Chain bare `$prev` (no dot path) when the prior result is a map
/// must be rejected with a clear substitution error that lists available fields.
///
/// `gtd.assign | gtd.complete(id=$prev.id, result=$prev)` — `$prev.id` resolves fine
/// (scalar), but `result=$prev` resolves to the whole assign result map.
/// The dispatcher must catch the bare map substitution and return a per-op error
/// with `kind=substitution_error` and a message listing the available fields —
/// instead of silently passing the map downstream where the handler emits a
/// confusing "invalid type: map, expected a string".
#[tokio::test]
async fn test_ue4_h1_bare_prev_map_produces_clear_substitution_error() -> anyhow::Result<()> {
    let client = connect().await?;

    let result = call(
        &client,
        "request",
        json!({
            "ops": r#"gtd.assign(title="bare-prev-test") | gtd.complete(id=$prev.id, result=$prev)"#,
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

    // Op 1: result=$prev resolves to the whole assign result map.
    // UE4-H1: the dispatcher must detect this and return a substitution_error
    // rather than passing the map through to the handler.
    assert_eq!(
        results[1]["ok"],
        json!(false),
        "bare $prev -> map must cause op 1 to fail: {}",
        results[1]
    );
    let error = &results[1]["error"];
    let err_msg = error["message"]
        .as_str()
        .unwrap_or_else(|| error.as_str().unwrap_or(""));
    assert!(
        err_msg.contains("dotted path") || err_msg.contains("$prev"),
        "UE4-H1: error must mention dotted path or $prev; got: {err_msg}"
    );
    assert!(
        err_msg.contains("result") || error["kind"].as_str() == Some("substitution_error"),
        "UE4-H1: error must reference the offending arg or be a substitution_error; got: {error}"
    );
    // The error must list at least one available field from the prior result.
    // assign result includes fields like id/full_id/title/kind.
    let mentions_field = err_msg.contains("id")
        || err_msg.contains("title")
        || err_msg.contains("kind")
        || err_msg.contains("full_id");
    assert!(
        mentions_field,
        "UE4-H1: error must list available top-level fields from prior result; got: {err_msg}"
    );

    // Chain is aborted: op 1 fails, no op 2 here (only 2 ops total).
    assert_eq!(body["summary"]["failed"], json!(1));
    Ok(())
}

/// ADR-016 H3 regression: `$prev.nonexistent_field` error must list the
/// available top-level fields from the prior result.
///
/// This test specifically covers the "H3: available fields hint" claim from the
/// PR — that `$prev.bogus` returns an error message containing
/// "Available top-level fields" plus at least one known field name.
/// The existing `test_prev_unresolvable_aborts_chain` only checked that the
/// path name appears in the error; this test asserts the field-hint clause.
#[tokio::test]
async fn test_h3_prev_nonexistent_field_error_lists_available_fields() -> anyhow::Result<()> {
    let client = connect().await?;

    // Create a concept so $prev has known fields (id, full_id, kind, name, …).
    let ops = r#"create(kind="entity", entity_kind="concept", name="H3Test") | get(id=$prev.nonexistent_field)"#;
    let result = call(
        &client,
        "request",
        json!({"ops": ops, "presentation": "verbose"}),
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    let results = body["results"].as_array().expect("results array");

    assert_eq!(results.len(), 2, "expected 2 ops");
    assert_eq!(results[0]["ok"], json!(true), "create must succeed");

    // Op 1 (get) must fail because $prev.nonexistent_field doesn't exist.
    assert_eq!(
        results[1]["ok"],
        json!(false),
        "get with nonexistent field must fail: {}",
        results[1]
    );

    // The error message must contain the "Available top-level fields" hint.
    let err_obj = &results[1]["error"];
    let err_msg = err_obj
        .as_str()
        .unwrap_or_else(|| err_obj["message"].as_str().unwrap_or(""));
    assert!(
        err_msg.contains("Available top-level fields"),
        "H3: error must contain 'Available top-level fields'; got: {err_msg}"
    );
    // The hint must list at least one known field from the create result.
    let mentions_field =
        err_msg.contains("id") || err_msg.contains("kind") || err_msg.contains("full_id");
    assert!(
        mentions_field,
        "H3: available-fields hint must name at least one known field; got: {err_msg}"
    );

    Ok(())
}

// ── help=true schema envelope integration tests ─────────────────────────────
//
// These tests confirm that help=true calls through the MCP surface return
// non-empty params slices with specific known parameters — verifying that
// the HandlerDef.params slices are populated (not left as &[]).

fn make_full_server() -> KhiveMcpServer {
    disable_daemon();
    let config = RuntimeConfig {
        db_path: None,
        default_namespace: Namespace::parse("test").unwrap(),
        embedding_model: None,
        additional_embedding_models: vec![],
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
    let schema = help_schema(&client, "memory.recall").await?;
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
    disable_daemon();
    let config = RuntimeConfig {
        db_path: None,
        default_namespace: Namespace::parse("test").unwrap(),
        embedding_model: None,
        additional_embedding_models: vec![],
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

/// `comm.send(help=true)` must return a non-empty params array with required `to` and `content`.
#[tokio::test]
async fn send_help_returns_required_to_and_content() -> anyhow::Result<()> {
    let client = connect_comm_schedule().await?;
    let result = ok_one(&client, "comm.send(help=true)").await?;

    assert_eq!(result["verb"], "comm.send");
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

/// `comm.inbox(help=true)` must return optional `limit` and `status`.
#[tokio::test]
async fn inbox_help_returns_optional_limit_and_status() -> anyhow::Result<()> {
    let client = connect_comm_schedule().await?;
    let result = ok_one(&client, "comm.inbox(help=true)").await?;

    assert_eq!(result["verb"], "comm.inbox");
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

/// `schedule.schedule(help=true)` must return required `action` and `at`.
#[tokio::test]
async fn schedule_help_returns_required_action_and_at() -> anyhow::Result<()> {
    let client = connect_comm_schedule().await?;
    let result = ok_one(&client, "schedule.schedule(help=true)").await?;

    assert_eq!(result["verb"], "schedule.schedule");
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

/// `schedule.remind(help=true)` must return required `content` and `at`, optional `repeat`.
#[tokio::test]
async fn remind_help_returns_required_content_and_at() -> anyhow::Result<()> {
    let client = connect_comm_schedule().await?;
    let result = ok_one(&client, "schedule.remind(help=true)").await?;

    assert_eq!(result["verb"], "schedule.remind");
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
        additional_embedding_models: vec![],
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
    // issues — the JSON form is equivalent per ADR-016 §§.
    let ops = serde_json::to_string(&json!([{
        "tool": "propose",
        "args": {
            "title": "migration regression test",
            "description": "fix1: run_migrations at startup",
            "changeset": {
                "kind": "add_entity",
                "entity": {
                    "kind": "concept",
                    "name": format!("fix1-{eid}")
                }
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
    disable_daemon();
    let config = RuntimeConfig {
        db_path: None,
        default_namespace: Namespace::parse("braintest").unwrap(),
        embedding_model: None,
        additional_embedding_models: vec![],
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

/// The operator path (`dispatch_request_local`, the path `kkernel exec` uses)
/// must NOT gate subhandler verbs. The visibility gate is wire-only
/// (`from_wire`): agents cannot reach subhandlers via the MCP `request` tool,
/// but operators invoke them directly. This is the invariant that regressed
/// when the gate lived in the shared dispatch — every handler had to be
/// promoted to `Verb` to stay reachable, which is exactly what we are undoing.
#[tokio::test]
async fn subhandler_verbs_are_allowed_on_operator_path() -> anyhow::Result<()> {
    use khive_mcp::tools::request::RequestParams;

    let server = make_brain_server();

    for verb in &["brain.state", "brain.config", "brain.events"] {
        let raw = server
            .dispatch_request_local(RequestParams {
                ops: format!("{verb}()"),
                presentation: None,
                presentation_per_op: None,
                save_to: None,
                format: None,
                format_per_op: None,
            })
            .await
            .expect("operator dispatch must not RPC-fail");
        let body: Value = serde_json::from_str(&raw)?;
        let first = &body["results"][0];
        let err = first["error"].as_str().unwrap_or("");
        // The gate must NOT have fired: its signature message must be absent.
        // The handler may still succeed (ok=true) or fail for its own reasons,
        // but it must have been *reached*, not blocked at the visibility gate.
        assert!(
            !err.contains("internal subhandler") && !err.contains("permission denied"),
            "operator path must NOT gate subhandler {verb:?}: {first}"
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

// ── P-C1: full_id is never shortened in Agent mode ───────────────────────────

/// `get` is AlwaysVerbose (ADR-045 §6) — returns full 36-char UUIDs even
/// in default (Agent) mode.  The response is now flat (P-H2): `{kind, id, ...}`
/// rather than the old wrapped `{kind, data: {...}}` shape.
#[tokio::test]
async fn get_returns_flat_shape_with_full_uuid_in_default_agent_mode() -> anyhow::Result<()> {
    let client = connect().await?;

    // ok_one uses presentation=verbose, so this gives us the full UUID.
    let created = ok_one(
        &client,
        r#"create(kind="entity", entity_kind="concept", name="FlatGetEntity")"#,
    )
    .await?;
    let full_id = created["id"].as_str().unwrap().to_string();
    assert_eq!(full_id.len(), 36, "verbose create must have full UUID");

    // Fetch via `get` WITHOUT specifying presentation — default is Agent mode.
    // `get` is AlwaysVerbose so it returns the full UUID regardless.
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

    // P-H2: `get` now returns a flat object — `kind` is at the top level
    // (the entity_kind, e.g. "concept"), NOT nested as `result.data.kind`.
    // There is no `data` wrapper.
    let entity = &first["result"];
    assert_eq!(
        entity["kind"], "concept",
        "get flat response must have top-level kind=concept (entity_kind); got {entity}"
    );
    assert!(
        entity.get("data").is_none(),
        "get must NOT wrap in {{data: ...}}; got {entity}"
    );
    // `get` is AlwaysVerbose: full 36-char UUID in `id` even in Agent mode.
    let returned_id = entity["id"].as_str().unwrap_or("");
    assert_eq!(
        returned_id.len(),
        36,
        "get (AlwaysVerbose) must return full 36-char UUID in id; got {returned_id:?}"
    );
    assert_eq!(
        returned_id, full_id,
        "returned id must match the created entity's full UUID"
    );
    Ok(())
}

/// ADR-045 §6 C2: `link` is `AlwaysVerbose` — edge IDs needed for follow-up.
///
/// At scale, two edges can share the same 8-char prefix (birthday collision ~65K
/// edges), so shortening the returned edge ID in agent mode violates ADR-045 §6
/// "Edge IDs needed for follow-up." `link` must return full 36-char UUIDs in
/// all modes including agent.
#[tokio::test]
async fn link_is_always_verbose_returns_full_uuids_in_agent_mode() -> anyhow::Result<()> {
    let client = connect().await?;

    // Create two entities via ok_one (verbose) to get full UUIDs for linking.
    let a = ok_one(
        &client,
        r#"create(kind="entity", entity_kind="concept", name="LinkVerboseA")"#,
    )
    .await?;
    let b = ok_one(
        &client,
        r#"create(kind="entity", entity_kind="concept", name="LinkVerboseB")"#,
    )
    .await?;
    let a_id = a["id"].as_str().unwrap().to_string();
    let b_id = b["id"].as_str().unwrap().to_string();
    assert_eq!(a_id.len(), 36);
    assert_eq!(b_id.len(), 36);

    // Call `link` in default Agent mode (no presentation key).
    // AlwaysVerbose policy: source_id/target_id must be full 36-char UUIDs
    // even in agent mode (ADR-045 §6 C2 fix).
    let result = call(
        &client,
        "request",
        json!({
            "ops": format!(
                r#"link(source_id="{a_id}", target_id="{b_id}", relation="extends")"#
            )
            // No `presentation` key — defaults to Agent, but AlwaysVerbose overrides.
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
        "link source_id must be full 36-char UUID in Agent mode (AlwaysVerbose); got {src:?}"
    );
    assert_eq!(
        tgt.len(),
        36,
        "link target_id must be full 36-char UUID in Agent mode (AlwaysVerbose); got {tgt:?}"
    );
    // The edge's own id must also be full UUID in agent mode.
    let edge_id = edge["id"].as_str().unwrap_or("");
    assert_eq!(
        edge_id.len(),
        // Edge IDs are LinkId which may serialize as full UUID; accept 36-char.
        // The AlwaysVerbose policy ensures no shortening occurs.
        36,
        "link edge id must be full UUID in Agent mode (AlwaysVerbose); got {edge_id:?}"
    );

    // Verify: explicit presentation=verbose also returns full 36-char UUIDs.
    let result_verbose = call(
        &client,
        "request",
        json!({
            "ops": format!(
                r#"link(source_id="{a_id}", target_id="{b_id}", relation="variant_of")"#
            ),
            "presentation": "verbose"
        }),
    )
    .await?;
    let body_v: Value = serde_json::from_str(&first_text(&result_verbose))?;
    let first_v = &body_v["results"][0];
    assert_eq!(first_v["ok"], true, "verbose link must succeed: {first_v}");
    let edge_v = &first_v["result"];
    assert_eq!(
        edge_v["source_id"].as_str().unwrap_or("").len(),
        36,
        "link source_id must be 36-char in verbose mode"
    );
    Ok(())
}

// ── #469 regression: bulk/symmetric link write-key conflict preflight ────────

/// #469: a bulk `link(links=[...])` op and a singleton `link` op that target
/// the same natural edge key must both be rejected with per-op conflict
/// errors by MCP preflight (`khive_request::write_keys_for_op_pub`) before
/// either dispatches, instead of racing through storage where SQLite's
/// `ON CONFLICT DO UPDATE` would let the last write silently win.
#[tokio::test]
async fn parallel_link_bulk_conflict_is_rejected_before_storage_race() -> anyhow::Result<()> {
    let client = connect().await?;

    let a = ok_one(
        &client,
        r#"create(kind="entity", entity_kind="concept", name="BulkConflictA")"#,
    )
    .await?;
    let b = ok_one(
        &client,
        r#"create(kind="entity", entity_kind="concept", name="BulkConflictB")"#,
    )
    .await?;
    let a_id = a["id"].as_str().unwrap().to_string();
    let b_id = b["id"].as_str().unwrap().to_string();

    let ops = format!(
        r#"[link(links=[{{"source_id":"{a_id}","target_id":"{b_id}","relation":"extends","weight":0.1}}]), link(source_id="{a_id}", target_id="{b_id}", relation="extends", weight=0.9)]"#
    );
    let result = call(
        &client,
        "request",
        json!({"ops": ops, "presentation": "verbose"}),
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;

    assert_eq!(body["summary"]["total"], json!(2));
    assert_eq!(
        body["summary"]["failed"],
        json!(2),
        "both conflicting link ops must fail preflight: {body}"
    );
    for i in 0..2 {
        let entry = &body["results"][i];
        assert_eq!(entry["ok"], json!(false), "op #{i} must fail: {entry}");
        let err = entry["error"].as_str().unwrap_or("");
        assert!(
            err.contains("conflict"),
            "op #{i} error must mention conflict: {entry}"
        );
    }

    // No edge should have been written — neither op reached storage.
    let get_result = call(
        &client,
        "request",
        json!({
            "ops": format!(r#"list(kind="entity_edge", source_id="{a_id}", target_id="{b_id}")"#),
            "presentation": "verbose"
        }),
    )
    .await;
    if let Ok(get_result) = get_result {
        if let Ok(get_body) = serde_json::from_str::<Value>(&first_text(&get_result)) {
            if get_body["results"][0]["ok"] == json!(true) {
                let items = get_body["results"][0]["result"]["items"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                assert!(
                    items.is_empty(),
                    "no edge should have been written from conflicting preflight-rejected ops: {items:?}"
                );
            }
        }
    }
    Ok(())
}

/// #469: reversed singleton symmetric links (`link(A,B,competes_with)` and
/// `link(B,A,competes_with)`) must canonicalize to the same natural edge key
/// and be rejected as conflicting, matching storage's own endpoint-order
/// canonicalization for symmetric relations.
#[tokio::test]
async fn parallel_reversed_symmetric_link_conflict_is_rejected() -> anyhow::Result<()> {
    let client = connect().await?;

    let a = ok_one(
        &client,
        r#"create(kind="entity", entity_kind="concept", name="SymConflictA")"#,
    )
    .await?;
    let b = ok_one(
        &client,
        r#"create(kind="entity", entity_kind="concept", name="SymConflictB")"#,
    )
    .await?;
    let a_id = a["id"].as_str().unwrap().to_string();
    let b_id = b["id"].as_str().unwrap().to_string();

    let ops = format!(
        r#"[link(source_id="{a_id}", target_id="{b_id}", relation="competes_with"), link(source_id="{b_id}", target_id="{a_id}", relation="competes_with")]"#
    );
    let result = call(
        &client,
        "request",
        json!({"ops": ops, "presentation": "verbose"}),
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;

    assert_eq!(body["summary"]["total"], json!(2));
    assert_eq!(
        body["summary"]["failed"],
        json!(2),
        "reversed symmetric link ops must both fail preflight: {body}"
    );
    for i in 0..2 {
        let entry = &body["results"][i];
        assert_eq!(entry["ok"], json!(false), "op #{i} must fail: {entry}");
        let err = entry["error"].as_str().unwrap_or("");
        assert!(
            err.contains("conflict"),
            "op #{i} error must mention conflict: {entry}"
        );
    }
    Ok(())
}

// ── ADR-046 regression: get(id=proposal_id) returns ProposalCreated payload ──

/// ADR-046:299 — get(id=<proposal_id>) must return the full ProposalCreated
/// event payload: description, changeset, reviewers, parent_id.
/// Before the fix, get returned only projection columns and omitted those fields.
#[tokio::test]
async fn get_proposal_id_returns_proposal_created_payload() -> anyhow::Result<()> {
    let client = connect().await?;

    // Create a parent proposal so we can set parent_id on the amendment proposal.
    // BUG-6 fix: parent_id must reference an existing proposal in proposals_open,
    // not an arbitrary entity UUID.
    let parent_ops = serde_json::to_string(&json!([{
        "tool": "propose",
        "args": {
            "title": "parent proposal",
            "description": "base proposal that the amendment will reference",
            "changeset": {
                "kind": "add_entity",
                "entity": { "kind": "concept", "name": "ParentProposalEntity" }
            }
        }
    }]))
    .unwrap();
    let parent_result = call(
        &client,
        "request",
        json!({"ops": parent_ops, "presentation": "verbose"}),
    )
    .await?;
    let parent_body: Value = serde_json::from_str(&first_text(&parent_result))?;
    let parent_first = &parent_body["results"][0];
    assert_eq!(
        parent_first["ok"], true,
        "parent propose must succeed; got: {parent_first}"
    );
    let parent_id = parent_first["result"]["id"]
        .as_str()
        .expect("parent id")
        .to_string();
    assert_eq!(parent_id.len(), 36, "parent id must be full UUID");

    // Propose with all optional fields populated: description, reviewers, parent_id,
    // and a changeset that carries a named entity.
    let ops = serde_json::to_string(&json!([{
        "tool": "propose",
        "args": {
            "title": "get-payload regression",
            "description": "ADR-046:299 regression — description must survive get()",
            "changeset": {
                "kind": "add_entity",
                "entity": {
                    "kind": "concept",
                    "name": "PayloadRegressionEntity"
                }
            },
            "reviewers": ["alice", "bob"],
            "parent_id": parent_id
        }
    }]))
    .unwrap();
    let result = call(
        &client,
        "request",
        json!({"ops": ops, "presentation": "verbose"}),
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    let first = &body["results"][0];
    assert_eq!(first["ok"], true, "propose must succeed; got: {first}");
    let proposal_id = first["result"]["id"]
        .as_str()
        .expect("propose must return id")
        .to_string();
    assert_eq!(proposal_id.len(), 36, "id from propose must be full UUID");
    assert!(
        first["result"].get("proposal_id").is_none(),
        "propose result must NOT contain old proposal_id key; got: {}",
        first["result"]
    );

    // Now get(id=<proposal_id>) — must return the ProposalCreated event payload.
    let get_result = ok_one(&client, &format!(r#"get(id="{proposal_id}")"#)).await?;

    // ADR-046:299: the four previously-missing fields must be present.
    assert_eq!(
        get_result["description"].as_str().unwrap_or(""),
        "ADR-046:299 regression — description must survive get()",
        "get(id=proposal_id) must return description from ProposalCreated payload"
    );
    let reviewers = get_result["reviewers"]
        .as_array()
        .expect("get(id=proposal_id) must return reviewers array");
    assert_eq!(
        reviewers.len(),
        2,
        "get(id=proposal_id) must return all reviewers; got: {reviewers:?}"
    );
    assert!(
        reviewers.iter().any(|r| r.as_str() == Some("alice")),
        "reviewers must include alice; got: {reviewers:?}"
    );
    assert!(
        reviewers.iter().any(|r| r.as_str() == Some("bob")),
        "reviewers must include bob; got: {reviewers:?}"
    );
    let changeset = &get_result["changeset"];
    assert!(
        !changeset.is_null(),
        "get(id=proposal_id) must return changeset; got null"
    );
    assert_eq!(
        changeset["kind"].as_str().unwrap_or(""),
        "add_entity",
        "changeset kind must be add_entity; got: {changeset}"
    );
    // parent_id is stored as Id128 (numeric); check it round-trips to a non-null value.
    assert!(
        !get_result["parent_id"].is_null(),
        "get(id=proposal_id) must return parent_id when set; got: {get_result}"
    );
    assert!(
        get_result.get("proposal_id").is_none(),
        "get(id=proposal_uuid) must NOT return old proposal_id key; got: {get_result}"
    );

    Ok(())
}

// ── ADR-046 regression: list(kind=proposal) unfiltered returns all rows ───────

/// ADR-046:277-279 — list(kind=proposal) without a status filter must return
/// ALL rows including applied/withdrawn (audit trail).
/// Before the fix, no-status defaulted to status IN ('open','changes_requested'),
/// hiding audit rows.
#[tokio::test]
async fn list_proposals_without_status_returns_all_rows() -> anyhow::Result<()> {
    let client = connect().await?;

    // Create two proposals.
    let ops = serde_json::to_string(&json!([
        {
            "tool": "propose",
            "args": {
                "title": "audit-row-A",
                "description": "first proposal",
                "changeset": {
                    "kind": "add_entity",
                    "entity": {"kind": "concept", "name": "AuditEntityA"}
                },
                "reviewers": []
            }
        },
        {
            "tool": "propose",
            "args": {
                "title": "audit-row-B",
                "description": "second proposal",
                "changeset": {
                    "kind": "add_entity",
                    "entity": {"kind": "concept", "name": "AuditEntityB"}
                },
                "reviewers": []
            }
        }
    ]))
    .unwrap();
    let result = call(
        &client,
        "request",
        json!({"ops": ops, "presentation": "verbose"}),
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    assert_eq!(body["results"][0]["ok"], true, "first propose must succeed");
    assert_eq!(
        body["results"][1]["ok"], true,
        "second propose must succeed"
    );
    let pid_a = body["results"][0]["result"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        body["results"][0]["result"].get("proposal_id").is_none(),
        "propose result must NOT contain old proposal_id key; got: {}",
        body["results"][0]["result"]
    );

    // Withdraw proposal A so it moves to a terminal status.
    let ops_withdraw = serde_json::to_string(&json!([{
        "tool": "withdraw",
        "args": {
            "id": pid_a,
            "rationale": "test withdrawal for audit list"
        }
    }]))
    .unwrap();
    let wr = call(
        &client,
        "request",
        json!({"ops": ops_withdraw, "presentation": "verbose"}),
    )
    .await?;
    let wr_body: Value = serde_json::from_str(&first_text(&wr))?;
    assert_eq!(
        wr_body["results"][0]["ok"], true,
        "withdraw must succeed; got: {}",
        wr_body["results"][0]
    );
    assert!(
        wr_body["results"][0]["result"].get("proposal_id").is_none(),
        "withdraw result must NOT contain old proposal_id key; got: {}",
        wr_body["results"][0]["result"]
    );

    // list(kind=proposal) without status — must return BOTH rows (open + withdrawn).
    // The list result is a bare JSON array (same shape as other list verbs).
    let list_result = ok_one(&client, r#"list(kind="proposal")"#).await?;
    let items = list_result
        .as_array()
        .expect("list(kind=proposal) must return a JSON array");
    assert!(
        items.len() >= 2,
        "list(kind=proposal) without status must include all rows (audit trail); \
         got {} items — withdrawn proposal must not be hidden",
        items.len()
    );
    // All list rows must expose `id`, not the old `proposal_id` key.
    for item in items.iter() {
        assert!(
            item.get("proposal_id").is_none(),
            "list(kind=proposal) row must NOT contain proposal_id key; got: {item}"
        );
        assert!(
            item.get("id").is_some(),
            "list(kind=proposal) row must contain id key; got: {item}"
        );
    }

    // list(kind=proposal, status=open) — must return only the open one.
    let list_open = ok_one(&client, r#"list(kind="proposal", status="open")"#).await?;
    let open_items = list_open
        .as_array()
        .expect("list(kind=proposal, status=open) must return a JSON array");
    assert!(
        open_items
            .iter()
            .all(|i| i["status"].as_str() == Some("open")),
        "list(kind=proposal, status=open) must return only open proposals; got: {open_items:?}"
    );

    Ok(())
}

// ── Actor / namespace precedence matrix (ADR-007 amendment) ──────────────────
//
// These tests exercise the 4-tier resolution order without a live server, using
// the same config-loading primitives that main.rs calls.  Each test covers one
// isolated conflict tier to lock in the precedence-matrix regression cases.

/// Tier 4 (hard default): no CLI actor, no env, no config file → "local".
#[test]
fn actor_precedence_default_local_with_no_config() {
    use khive_runtime::{Namespace, RuntimeConfig};

    let config = RuntimeConfig::default();
    assert_eq!(
        config.default_namespace,
        Namespace::parse("local").unwrap(),
        "RuntimeConfig::default() must produce namespace 'local' (tier-4 hard default)"
    );
}

/// Tier 3 (config file): no CLI, config has actor.id. Under ADR-007 Rev 4 Rule 0
/// the config `[actor] id` must NOT route the storage `default_namespace` — writes
/// stay pinned to `local`. Note: a non-`'local'` `actor.id` IS folded into the
/// default READ visible-set (ADR-007 Rev 4 Rule 3b), but that does not change
/// `default_namespace`. This test asserts only the write-routing invariant.
#[test]
fn actor_precedence_config_actor_id_does_not_route_namespace() {
    use khive_runtime::{runtime_config_from_khive_config, KhiveConfig, Namespace, RuntimeConfig};
    use std::io::Write;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    writeln!(
        std::fs::File::create(&path).unwrap(),
        "[actor]\nid = \"lambda:from-config\"\n"
    )
    .unwrap();

    let khive_cfg = KhiveConfig::load(Some(&path))
        .expect("load should succeed")
        .expect("file found");
    assert_eq!(khive_cfg.actor.id.as_deref(), Some("lambda:from-config"));

    // No CLI actor → base stays at "local" default; config actor.id must not move it.
    let base = RuntimeConfig::default();
    let resolved = runtime_config_from_khive_config(&khive_cfg, base);
    assert_eq!(
        resolved.default_namespace,
        Namespace::parse("local").unwrap(),
        "config actor.id must NOT become default_namespace (ADR-007 Rev 4 Rule 0); \
         writes stay pinned to local (actor.id does contribute to READ visible-set per Rule 3b, \
         but that does not affect default_namespace)"
    );
}

/// Tier 2 (--namespace / KHIVE_NAMESPACE with explicit value "local"): explicit
/// --namespace local must win over a conflicting config actor.
///
/// This is the regression case for [High] finding 1: previously the value
/// comparison `args.namespace != "local"` treated `--namespace local` as
/// identical to the absent default, letting config override it.  Now that
/// `namespace` is `Option<String>`, `Some("local")` is correctly explicit.
#[test]
fn actor_precedence_explicit_namespace_local_wins_over_config() {
    use khive_runtime::{runtime_config_from_khive_config, KhiveConfig, Namespace, RuntimeConfig};
    use std::io::Write;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    writeln!(
        std::fs::File::create(&path).unwrap(),
        "[actor]\nid = \"lambda:from-config\"\n"
    )
    .unwrap();

    let khive_cfg = KhiveConfig::load(Some(&path))
        .expect("load should succeed")
        .expect("file found");

    // Simulate: --namespace local supplied → cli_namespace_explicit = true.
    // Caller nullifies config actor before calling runtime_config_from_khive_config.
    let mut effective_cfg = khive_cfg;
    effective_cfg.actor.id = None; // CLI wins — suppress config actor.

    let base = RuntimeConfig {
        default_namespace: Namespace::parse("local").unwrap(), // explicit CLI value
        additional_embedding_models: vec![],
        ..RuntimeConfig::default()
    };
    let resolved = runtime_config_from_khive_config(&effective_cfg, base);
    assert_eq!(
        resolved.default_namespace,
        Namespace::parse("local").unwrap(),
        "--namespace local (explicit) must win over config actor.id"
    );
}

/// Tier 1 (--actor / KHIVE_ACTOR): explicit --actor value wins over config actor.
#[test]
fn actor_precedence_cli_actor_wins_over_config() {
    use khive_runtime::{runtime_config_from_khive_config, KhiveConfig, Namespace, RuntimeConfig};
    use std::io::Write;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    writeln!(
        std::fs::File::create(&path).unwrap(),
        "[actor]\nid = \"lambda:from-config\"\n"
    )
    .unwrap();

    let khive_cfg = KhiveConfig::load(Some(&path))
        .expect("load should succeed")
        .expect("file found");

    // Simulate: --actor lambda:cli-actor supplied → cli_namespace_explicit = true.
    let mut effective_cfg = khive_cfg;
    effective_cfg.actor.id = None; // CLI wins — suppress config actor.

    let base = RuntimeConfig {
        default_namespace: Namespace::parse("lambda:cli-actor").unwrap(),
        additional_embedding_models: vec![],
        ..RuntimeConfig::default()
    };
    let resolved = runtime_config_from_khive_config(&effective_cfg, base);
    assert_eq!(
        resolved.default_namespace,
        Namespace::parse("lambda:cli-actor").unwrap(),
        "--actor lambda:cli-actor must win over config actor.id"
    );
}

/// Invalid config actor.id must be caught at load time (not silently downgraded).
///
/// This is the regression case for [High] finding 2: previously an invalid
/// actor.id logged a warning and fell back to the base namespace.  Now it is a
/// hard startup error via ConfigError::InvalidActorId.
#[test]
fn actor_invalid_config_id_fails_at_load() {
    use khive_runtime::{ConfigError, KhiveConfig};
    use std::io::Write;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    writeln!(
        std::fs::File::create(&path).unwrap(),
        "[actor]\nid = \"bad namespace\"\n"
    )
    .unwrap();

    let err = KhiveConfig::load(Some(&path)).expect_err("invalid actor.id must fail at load");
    assert!(
        matches!(err, ConfigError::InvalidActorId { .. }),
        "expected ConfigError::InvalidActorId, got {err:?}"
    );
}

/// Empty-string actor.id must be caught at load time.
#[test]
fn actor_empty_string_id_fails_at_load() {
    use khive_runtime::{ConfigError, KhiveConfig};
    use std::io::Write;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    writeln!(
        std::fs::File::create(&path).unwrap(),
        "[actor]\nid = \"\"\n"
    )
    .unwrap();

    let err = KhiveConfig::load(Some(&path)).expect_err("empty actor.id must fail at load");
    assert!(
        matches!(err, ConfigError::InvalidActorId { .. }),
        "expected ConfigError::InvalidActorId for empty string, got {err:?}"
    );
}

// ---------------------------------------------------------------------------
// CLI / env precedence: real Args parsing via clap try_parse_from
//
// These tests exercise the actual clap parser + resolve_cli_namespace so that
// a regression such as `args.namespace != "local"` (the original High finding)
// would cause failures here, not just in the manually-constructed tests above.
// ---------------------------------------------------------------------------

/// RAII guard that unsets the named env vars on construction.
/// Prevents leakage from a prior serial test that may not have cleaned up.
struct ClearEnvGuard {
    vars: Vec<&'static str>,
}

impl ClearEnvGuard {
    fn new(vars: &[&'static str]) -> Self {
        for &v in vars {
            std::env::remove_var(v);
        }
        Self {
            vars: vars.to_vec(),
        }
    }
}

impl Drop for ClearEnvGuard {
    fn drop(&mut self) {
        for &v in &self.vars {
            std::env::remove_var(v);
        }
    }
}

/// Tier 1a: --actor flag → explicit=true, namespace = supplied value.
#[test]
#[serial_test::serial]
fn cli_args_actor_flag_is_explicit() {
    use clap::Parser;
    use khive_mcp::args::{resolve_cli_namespace, Args};
    use khive_runtime::Namespace;

    let _guard = ClearEnvGuard::new(&["KHIVE_ACTOR", "KHIVE_NAMESPACE"]);
    let args = Args::try_parse_from(["khive-mcp", "--actor", "lambda:cli-actor"]).unwrap();
    let (explicit, ns) = resolve_cli_namespace(&args).unwrap();
    assert!(explicit, "--actor must mark namespace as explicit");
    assert_eq!(ns, Namespace::parse("lambda:cli-actor").unwrap());
}

/// Tier 1b: --actor local → explicit=true (regression guard: must NOT be treated as absent).
#[test]
#[serial_test::serial]
fn cli_args_actor_local_is_explicit() {
    use clap::Parser;
    use khive_mcp::args::{resolve_cli_namespace, Args};
    use khive_runtime::Namespace;

    let _guard = ClearEnvGuard::new(&["KHIVE_ACTOR", "KHIVE_NAMESPACE"]);
    let args = Args::try_parse_from(["khive-mcp", "--actor", "local"]).unwrap();
    let (explicit, ns) = resolve_cli_namespace(&args).unwrap();
    assert!(
        explicit,
        "--actor local must be explicit, not treated as absent default"
    );
    assert_eq!(ns, Namespace::parse("local").unwrap());
}

/// Tier 2a: --namespace flag → explicit=true.
#[test]
#[serial_test::serial]
fn cli_args_namespace_flag_is_explicit() {
    use clap::Parser;
    use khive_mcp::args::{resolve_cli_namespace, Args};
    use khive_runtime::Namespace;

    let _guard = ClearEnvGuard::new(&["KHIVE_ACTOR", "KHIVE_NAMESPACE"]);
    let args = Args::try_parse_from(["khive-mcp", "--namespace", "lambda:ns-flag"]).unwrap();
    let (explicit, ns) = resolve_cli_namespace(&args).unwrap();
    assert!(explicit, "--namespace must mark namespace as explicit");
    assert_eq!(ns, Namespace::parse("lambda:ns-flag").unwrap());
}

/// Tier 2b: --namespace local → explicit=true (the original regression case).
#[test]
#[serial_test::serial]
fn cli_args_namespace_local_is_explicit() {
    use clap::Parser;
    use khive_mcp::args::{resolve_cli_namespace, Args};
    use khive_runtime::Namespace;

    let _guard = ClearEnvGuard::new(&["KHIVE_ACTOR", "KHIVE_NAMESPACE"]);
    let args = Args::try_parse_from(["khive-mcp", "--namespace", "local"]).unwrap();
    let (explicit, ns) = resolve_cli_namespace(&args).unwrap();
    assert!(
        explicit,
        "--namespace local must be explicit (regression: was previously treated as absent)"
    );
    assert_eq!(ns, Namespace::parse("local").unwrap());
}

/// Tier 1 wins over Tier 2: --actor beats --namespace when both supplied.
#[test]
#[serial_test::serial]
fn cli_args_actor_wins_over_namespace_when_both_supplied() {
    use clap::Parser;
    use khive_mcp::args::{resolve_cli_namespace, Args};
    use khive_runtime::Namespace;

    let _guard = ClearEnvGuard::new(&["KHIVE_ACTOR", "KHIVE_NAMESPACE"]);
    let args = Args::try_parse_from([
        "khive-mcp",
        "--actor",
        "lambda:actor-wins",
        "--namespace",
        "lambda:ns-loses",
    ])
    .unwrap();
    let (explicit, ns) = resolve_cli_namespace(&args).unwrap();
    assert!(explicit);
    assert_eq!(
        ns,
        Namespace::parse("lambda:actor-wins").unwrap(),
        "--actor must win over --namespace when both are supplied"
    );
}

/// Tier 4 (hard default): no CLI flags → explicit=false, namespace = "local".
#[test]
#[serial_test::serial]
fn cli_args_no_flags_gives_local_default() {
    use clap::Parser;
    use khive_mcp::args::{resolve_cli_namespace, Args};
    use khive_runtime::Namespace;

    let _guard = ClearEnvGuard::new(&["KHIVE_ACTOR", "KHIVE_NAMESPACE"]);
    let args = Args::try_parse_from(["khive-mcp"]).unwrap();
    let (explicit, ns) = resolve_cli_namespace(&args).unwrap();
    assert!(!explicit, "no flags must not be treated as explicit");
    assert_eq!(
        ns,
        Namespace::parse("local").unwrap(),
        "default namespace must be 'local' when no CLI flags are supplied"
    );
}

/// KHIVE_NAMESPACE env var → explicit=true (env var has same effect as flag).
///
/// Uses `clap`'s env-source support. `ClearEnvGuard` unsets both
/// `KHIVE_NAMESPACE` and `KHIVE_ACTOR` on construction AND drop, so the env is
/// clean for the parse and restored to clean state after, even on panic.
/// `#[serial]` prevents races with other env-mutating tests.
#[test]
#[serial_test::serial]
fn cli_args_khive_namespace_env_is_explicit() {
    use clap::Parser;
    use khive_mcp::args::{resolve_cli_namespace, Args};
    use khive_runtime::Namespace;

    let _guard = ClearEnvGuard::new(&["KHIVE_NAMESPACE", "KHIVE_ACTOR"]);

    std::env::set_var("KHIVE_NAMESPACE", "lambda:from-env");
    let args = Args::try_parse_from(["khive-mcp"]).unwrap();
    std::env::remove_var("KHIVE_NAMESPACE");

    let (explicit, ns) = resolve_cli_namespace(&args).unwrap();
    assert!(
        explicit,
        "KHIVE_NAMESPACE env must mark namespace as explicit"
    );
    assert_eq!(ns, Namespace::parse("lambda:from-env").unwrap());
}

/// KHIVE_ACTOR env var → explicit=true, wins over KHIVE_NAMESPACE.
/// `ClearEnvGuard` keeps env state isolated; `#[serial]` prevents races.
#[test]
#[serial_test::serial]
fn cli_args_khive_actor_env_is_explicit_and_wins() {
    use clap::Parser;
    use khive_mcp::args::{resolve_cli_namespace, Args};
    use khive_runtime::Namespace;

    let _guard = ClearEnvGuard::new(&["KHIVE_NAMESPACE", "KHIVE_ACTOR"]);

    std::env::set_var("KHIVE_ACTOR", "lambda:actor-env");
    std::env::set_var("KHIVE_NAMESPACE", "lambda:ns-env");
    let args = Args::try_parse_from(["khive-mcp"]).unwrap();
    std::env::remove_var("KHIVE_ACTOR");
    std::env::remove_var("KHIVE_NAMESPACE");

    let (explicit, ns) = resolve_cli_namespace(&args).unwrap();
    assert!(explicit);
    assert_eq!(
        ns,
        Namespace::parse("lambda:actor-env").unwrap(),
        "KHIVE_ACTOR env must win over KHIVE_NAMESPACE"
    );
}

// ── ue-errors C1: unknown-kwarg rejection ────────────────────────────────────

/// `update(id=<uuid>, nonexistent_field="x")` must return `ok: false`, not
/// silently succeed (ue-errors C1).  The caller must be able to trust that
/// `ok: true` means the intended update was actually applied.
#[tokio::test]
async fn update_rejects_unknown_kwarg() -> anyhow::Result<()> {
    let client = connect().await?;

    // Create an entity to update.
    let entity = ok_one(
        &client,
        r#"create(kind="entity", entity_kind="concept", name="UpdateUnknownKwargTest")"#,
    )
    .await?;
    let id = entity["id"].as_str().unwrap();

    // Attempt update with an unknown kwarg.
    let result = call(
        &client,
        "request",
        json!({ "ops": format!(r#"update(id="{id}", nonexistent_field="x")"#) }),
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    let first = &body["results"][0];
    assert_eq!(
        first["ok"],
        json!(false),
        "update with unknown kwarg must fail; got: {first}"
    );
    let err = first["error"].as_str().unwrap_or("");
    assert!(
        err.contains("nonexistent_field") || err.contains("unknown field"),
        "error must mention the unknown field; got: {err}"
    );
    Ok(())
}

/// `remember(content="x", garbage_arg="y")` must return `ok: false` (ue-errors C1).
#[tokio::test]
async fn remember_rejects_unknown_kwarg() -> anyhow::Result<()> {
    let client = connect_full().await?;

    let result = call(
        &client,
        "request",
        json!({ "ops": r#"memory.remember(content="test memory", garbage_arg="xyz")"# }),
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    let first = &body["results"][0];
    assert_eq!(
        first["ok"],
        json!(false),
        "remember with unknown kwarg must fail; got: {first}"
    );
    let err = first["error"].as_str().unwrap_or("");
    assert!(
        err.contains("garbage_arg") || err.contains("unknown field"),
        "error must mention the unknown field; got: {err}"
    );
    Ok(())
}

/// Known `remember` aliases (`salience`, `decay`, `source`) must still work
/// after deny_unknown_fields is applied (ue-errors C1 regression guard).
#[tokio::test]
async fn remember_aliases_still_accepted() -> anyhow::Result<()> {
    let client = connect_full().await?;

    let result = call(
        &client,
        "request",
        json!({ "ops": r#"memory.remember(content="alias test", salience=0.8, decay=0.005)"# }),
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    let first = &body["results"][0];
    assert_eq!(
        first["ok"],
        json!(true),
        "remember with aliases salience/decay must succeed; got: {first}"
    );
    Ok(())
}

// ── ADR-045 §5 handler invariant: ISO-8601 timestamps at MCP boundary ────────

/// Entity `create` must return ISO-8601 timestamps (not raw microsecond i64s).
///
/// This is the Blocker C1 regression guard: the note create path was missing
/// normalize_entity_timestamps, causing `created_at`/`updated_at` to arrive
/// as integer microseconds. Fixed by wrapping the note create response with
/// normalize_entity_timestamps before remap_note_status.
#[tokio::test]
async fn entity_create_returns_iso8601_timestamps() -> anyhow::Result<()> {
    let client = connect().await?;

    let result = ok_one(
        &client,
        r#"create(kind="entity", entity_kind="concept", name="TimestampTest-Entity")"#,
    )
    .await?;

    let created_at = result["created_at"].as_str().unwrap_or("");
    let updated_at = result["updated_at"].as_str().unwrap_or("");
    assert!(
        !created_at.is_empty(),
        "entity create created_at must be a string, got: {:?}",
        result["created_at"]
    );
    // ISO-8601 strings start with 4-digit year
    assert!(
        created_at.starts_with("20"),
        "entity create created_at must be ISO-8601, got: {created_at:?}"
    );
    assert!(
        updated_at.starts_with("20"),
        "entity create updated_at must be ISO-8601, got: {updated_at:?}"
    );
    Ok(())
}

/// Note `create` must return ISO-8601 timestamps (Blocker C1 fix: note path was missing
/// normalize_entity_timestamps before the MCP response).
#[tokio::test]
async fn note_create_returns_iso8601_timestamps() -> anyhow::Result<()> {
    let client = connect().await?;

    let result = ok_one(
        &client,
        r#"create(kind="note", content="timestamp test note")"#,
    )
    .await?;

    let created_at = result["created_at"].as_str().unwrap_or("");
    let updated_at = result["updated_at"].as_str().unwrap_or("");
    assert!(
        created_at.starts_with("20"),
        "note create created_at must be ISO-8601, got: {created_at:?}"
    );
    assert!(
        updated_at.starts_with("20"),
        "note create updated_at must be ISO-8601, got: {updated_at:?}"
    );
    Ok(())
}

/// Entity `get` (AlwaysVerbose) must return ISO-8601 timestamps.
#[tokio::test]
async fn entity_get_returns_iso8601_timestamps() -> anyhow::Result<()> {
    let client = connect().await?;

    let created = ok_one(
        &client,
        r#"create(kind="entity", entity_kind="concept", name="TimestampGet-Entity")"#,
    )
    .await?;
    let id = created["id"].as_str().unwrap();

    let result = call(
        &client,
        "request",
        json!({"ops": format!(r#"get(id="{id}")"#)}),
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    let first = &body["results"][0];
    assert_eq!(first["ok"], true, "get must succeed: {first}");
    let entity = &first["result"];
    let created_at = entity["created_at"].as_str().unwrap_or("");
    assert!(
        created_at.starts_with("20"),
        "entity get created_at must be ISO-8601, got: {created_at:?}"
    );
    Ok(())
}

/// Entity `list` must return ISO-8601 timestamps across all items.
#[tokio::test]
async fn entity_list_returns_iso8601_timestamps() -> anyhow::Result<()> {
    let client = connect().await?;

    // Ensure at least one entity exists.
    ok_one(
        &client,
        r#"create(kind="entity", entity_kind="concept", name="TimestampList-Entity")"#,
    )
    .await?;

    let result = ok_one(&client, r#"list(kind="entity", limit=3)"#).await?;
    let items = result
        .as_array()
        .expect("list(kind=entity) returns array of entities");
    assert!(!items.is_empty(), "list must return at least one entity");

    for item in items {
        let created_at = item["created_at"].as_str().unwrap_or("");
        assert!(
            created_at.starts_with("20"),
            "entity list created_at must be ISO-8601, got: {created_at:?} in {item}"
        );
    }
    Ok(())
}

/// Entity `update` must return ISO-8601 timestamps (the update response goes
/// through normalize_entity_timestamps before the presentation layer).
#[tokio::test]
async fn entity_update_returns_iso8601_timestamps() -> anyhow::Result<()> {
    let client = connect().await?;

    let created = ok_one(
        &client,
        r#"create(kind="entity", entity_kind="concept", name="TimestampUpdate-Entity")"#,
    )
    .await?;
    let id = created["id"].as_str().unwrap();

    let result = ok_one(
        &client,
        &format!(r#"update(id="{id}", description="updated")"#),
    )
    .await?;

    let updated_at = result["updated_at"].as_str().unwrap_or("");
    assert!(
        updated_at.starts_with("20"),
        "entity update updated_at must be ISO-8601, got: {updated_at:?}"
    );
    Ok(())
}

// ── ue-errors C1 extension: unknown-kwarg rejection on additional verbs ───────

/// `recall(query="x", typo_kwarg="y")` must return `ok: false` (ue-errors C1).
#[tokio::test]
async fn recall_rejects_unknown_kwarg() -> anyhow::Result<()> {
    let client = connect_full().await?;

    let result = call(
        &client,
        "request",
        json!({ "ops": r#"memory.recall(query="test", typo_kwarg="oops")"# }),
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    let first = &body["results"][0];
    assert_eq!(
        first["ok"],
        json!(false),
        "recall with unknown kwarg must fail; got: {first}"
    );
    let err = first["error"].as_str().unwrap_or("");
    assert!(
        err.contains("typo_kwarg") || err.contains("unknown field"),
        "error must mention the unknown field; got: {err}"
    );
    Ok(())
}

/// `list(kind="entity", typo_kwarg="y")` must return `ok: false` (ue-errors C1).
#[tokio::test]
async fn list_rejects_unknown_kwarg() -> anyhow::Result<()> {
    let client = connect().await?;

    let result = call(
        &client,
        "request",
        json!({ "ops": r#"list(kind="entity", typo_kwarg="oops")"# }),
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    let first = &body["results"][0];
    assert_eq!(
        first["ok"],
        json!(false),
        "list with unknown kwarg must fail; got: {first}"
    );
    let err = first["error"].as_str().unwrap_or("");
    assert!(
        err.contains("typo_kwarg") || err.contains("unknown field"),
        "error must mention the unknown field; got: {err}"
    );
    Ok(())
}

// ── Round 3: MCP-wide ISO-8601 timestamps (Blocker fix) ──────────────────────

/// `remember` must return ISO-8601 `created_at` (not a raw microsecond i64).
#[tokio::test]
async fn remember_returns_iso8601_timestamp() -> anyhow::Result<()> {
    let client = connect_full().await?;

    let result = ok_one(
        &client,
        r#"memory.remember(content="r3 timestamp test", salience=0.7)"#,
    )
    .await?;

    let created_at = result["created_at"].as_str().unwrap_or("");
    assert!(
        created_at.starts_with("20"),
        "remember created_at must be ISO-8601 string, got: {:?}",
        result["created_at"]
    );
    Ok(())
}

/// `recall` must return ISO-8601 `created_at` on each hit (not raw i64).
#[tokio::test]
async fn recall_returns_iso8601_timestamps() -> anyhow::Result<()> {
    let client = connect_full().await?;

    // Seed a memory first.
    ok_one(
        &client,
        r#"memory.remember(content="r3 recall timestamp seed")"#,
    )
    .await?;

    let result = ok_one(
        &client,
        r#"memory.recall(query="r3 recall timestamp seed", limit=1)"#,
    )
    .await?;

    let hits = result.as_array().expect("recall returns array");
    assert!(!hits.is_empty(), "recall must return at least one hit");
    let created_at = hits[0]["created_at"].as_str().unwrap_or("");
    assert!(
        created_at.starts_with("20"),
        "recall hit created_at must be ISO-8601 string, got: {:?}",
        hits[0]["created_at"]
    );
    Ok(())
}

fn make_comm_server_only() -> KhiveMcpServer {
    disable_daemon();
    let config = RuntimeConfig {
        db_path: None,
        default_namespace: Namespace::parse("commtest").unwrap(),
        embedding_model: None,
        additional_embedding_models: vec![],
        packs: vec!["kg".to_string(), "comm".to_string()],
        ..RuntimeConfig::default()
    };
    let runtime = KhiveRuntime::new(config).expect("kg+comm runtime");
    KhiveMcpServer::new(runtime).expect("server builds with kg+comm")
}

async fn connect_comm_only(
) -> anyhow::Result<impl std::ops::Deref<Target = rmcp::service::Peer<rmcp::RoleClient>>> {
    let (server_transport, client_transport) = tokio::io::duplex(65536);
    let server = make_comm_server_only();
    tokio::spawn(async move {
        if let Ok(svc) = server.serve(server_transport).await {
            let _ = svc.waiting().await;
        }
    });
    let client = DummyClient.serve(client_transport).await?;
    Ok(client)
}

/// `inbox` returns message notes; `created_at` and `updated_at` must be ISO-8601
/// strings (not raw microsecond i64s) — validates note_to_message_json fix.
#[tokio::test]
async fn send_returns_iso8601_timestamps() -> anyhow::Result<()> {
    let client = connect_comm_only().await?;

    // Self-send produces one outbound note visible to inbox.
    ok_one(
        &client,
        r#"comm.send(to="commtest", content="r3 timestamp test message")"#,
    )
    .await?;

    // inbox lists inbound; self-send is the same note — use the listing path
    // that calls note_to_message_json which we fixed.
    let result = call(
        &client,
        "request",
        json!({"ops": r#"list(kind="note", limit=1)"#, "presentation": "verbose"}),
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    let first = &body["results"][0];
    assert_eq!(
        first["ok"],
        json!(true),
        "list(kind=note) must succeed: {first}"
    );
    let items = first["result"].as_array().expect("list returns array");
    assert!(!items.is_empty(), "must have at least one message note");
    let created_at = items[0]["created_at"].as_str().unwrap_or("");
    assert!(
        created_at.starts_with("20"),
        "message note created_at must be ISO-8601 string, got: {:?}",
        items[0]["created_at"]
    );
    Ok(())
}

fn make_schedule_server_only() -> KhiveMcpServer {
    disable_daemon();
    let config = RuntimeConfig {
        db_path: None,
        default_namespace: Namespace::parse("schedtest").unwrap(),
        embedding_model: None,
        additional_embedding_models: vec![],
        packs: vec!["kg".to_string(), "schedule".to_string()],
        ..RuntimeConfig::default()
    };
    let runtime = KhiveRuntime::new(config).expect("kg+schedule runtime");
    KhiveMcpServer::new(runtime).expect("server builds with kg+schedule")
}

async fn connect_schedule_only(
) -> anyhow::Result<impl std::ops::Deref<Target = rmcp::service::Peer<rmcp::RoleClient>>> {
    let (server_transport, client_transport) = tokio::io::duplex(65536);
    let server = make_schedule_server_only();
    tokio::spawn(async move {
        if let Ok(svc) = server.serve(server_transport).await {
            let _ = svc.waiting().await;
        }
    });
    let client = DummyClient.serve(client_transport).await?;
    Ok(client)
}

/// `remind` creates a scheduled_event note; `agenda` returns ISO-8601 timestamps.
#[tokio::test]
async fn agenda_returns_iso8601_timestamps() -> anyhow::Result<()> {
    let client = connect_schedule_only().await?;

    ok_one(
        &client,
        r#"schedule.remind(content="r3 agenda ts test", at="2099-01-01T00:00:00Z")"#,
    )
    .await?;

    let result = ok_one(&client, r#"schedule.agenda()"#).await?;
    // agenda returns { events: [...], count: N }
    let items = result["events"]
        .as_array()
        .expect("agenda returns events array");
    assert!(!items.is_empty(), "agenda must return at least one event");
    let created_at = items[0]["created_at"].as_str().unwrap_or("");
    assert!(
        created_at.starts_with("20"),
        "agenda event created_at must be ISO-8601 string, got: {:?}",
        items[0]["created_at"]
    );
    Ok(())
}

async fn connect_brain_only(
) -> anyhow::Result<impl std::ops::Deref<Target = rmcp::service::Peer<rmcp::RoleClient>>> {
    let (server_transport, client_transport) = tokio::io::duplex(65536);
    let config = RuntimeConfig {
        db_path: None,
        default_namespace: Namespace::parse("braintest2").unwrap(),
        embedding_model: None,
        additional_embedding_models: vec![],
        packs: vec!["kg".to_string(), "brain".to_string()],
        ..RuntimeConfig::default()
    };
    let runtime = KhiveRuntime::new(config).expect("kg+brain runtime");
    let server = KhiveMcpServer::new(runtime).expect("server builds");
    tokio::spawn(async move {
        if let Ok(svc) = server.serve(server_transport).await {
            let _ = svc.waiting().await;
        }
    });
    let client = DummyClient.serve(client_transport).await?;
    Ok(client)
}

/// `brain.profiles` must return ISO-8601 `created_at` on profile records.
#[tokio::test]
async fn brain_profiles_returns_iso8601_timestamps() -> anyhow::Result<()> {
    let client = connect_brain_only().await?;

    let result = ok_one(&client, r#"brain.profiles()"#).await?;
    let profiles = result["profiles"]
        .as_array()
        .expect("brain.profiles returns profiles array");
    assert!(
        !profiles.is_empty(),
        "brain.profiles must return at least one profile"
    );
    let created_at = profiles[0]["created_at"].as_str().unwrap_or("");
    assert!(
        created_at.starts_with("20"),
        "brain.profiles created_at must be ISO-8601 string, got: {:?}",
        profiles[0]["created_at"]
    );
    Ok(())
}

/// `propose` + `list(kind="proposal")` must return ISO-8601 timestamps on proposal rows.
#[tokio::test]
async fn proposal_list_returns_iso8601_timestamps() -> anyhow::Result<()> {
    let client = connect().await?;

    ok_one(
        &client,
        r#"propose(title="r3 ts test proposal", description="r3 timestamp regression test", changeset={"kind": "add_entity", "entity": {"kind": "concept", "name": "R3TsEntity"}})"#,
    )
    .await?;

    let result = ok_one(&client, r#"list(kind="proposal")"#).await?;
    let proposals = result
        .as_array()
        .expect("list(kind=proposal) returns array");
    assert!(!proposals.is_empty(), "must have at least one proposal");
    let created_at = proposals[0]["created_at"].as_str().unwrap_or("");
    assert!(
        created_at.starts_with("20"),
        "proposal list created_at must be ISO-8601 string, got: {:?}",
        proposals[0]["created_at"]
    );
    Ok(())
}

// ── Round 3: cross-pack deny_unknown_fields (High fix) ───────────────────────

/// `create(kind="concept", unknownkw="x")` must return `ok: false`.
#[tokio::test]
async fn create_rejects_unknown_kwarg() -> anyhow::Result<()> {
    let client = connect().await?;

    let result = call(
        &client,
        "request",
        json!({ "ops": r#"create(kind="concept", name="X", unknownkw="oops")"# }),
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    let first = &body["results"][0];
    assert_eq!(
        first["ok"],
        json!(false),
        "create with unknown kwarg must fail; got: {first}"
    );
    let err = first["error"].as_str().unwrap_or("");
    assert!(
        err.contains("unknownkw") || err.contains("unknown field"),
        "error must mention the unknown field; got: {err}"
    );
    Ok(())
}

/// `assign(title="T", unknownkw="x")` (GTD) must return `ok: false`.
#[tokio::test]
async fn assign_rejects_unknown_kwarg() -> anyhow::Result<()> {
    let client = connect_full().await?;

    let result = call(
        &client,
        "request",
        json!({ "ops": r#"gtd.assign(title="GTD unknown kwarg test", unknownkw="oops")"# }),
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    let first = &body["results"][0];
    assert_eq!(
        first["ok"],
        json!(false),
        "assign with unknown kwarg must fail; got: {first}"
    );
    let err = first["error"].as_str().unwrap_or("");
    assert!(
        err.contains("unknownkw") || err.contains("unknown field"),
        "error must mention the unknown field; got: {err}"
    );
    Ok(())
}

/// `send(to="x", content="y", unknownkw="z")` (comm) must return `ok: false`.
#[tokio::test]
async fn send_rejects_unknown_kwarg() -> anyhow::Result<()> {
    let client = connect_comm_only().await?;

    let result = call(
        &client,
        "request",
        json!({ "ops": r#"comm.send(to="alice", content="test", unknownkw="oops")"# }),
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    let first = &body["results"][0];
    assert_eq!(
        first["ok"],
        json!(false),
        "send with unknown kwarg must fail; got: {first}"
    );
    let err = first["error"].as_str().unwrap_or("");
    assert!(
        err.contains("unknownkw") || err.contains("unknown field"),
        "error must mention the unknown field; got: {err}"
    );
    Ok(())
}

/// `agenda(unknownkw="x")` (schedule) must return `ok: false`.
#[tokio::test]
async fn agenda_rejects_unknown_kwarg() -> anyhow::Result<()> {
    let client = connect_schedule_only().await?;

    let result = call(
        &client,
        "request",
        json!({ "ops": r#"schedule.agenda(unknownkw="oops")"# }),
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    let first = &body["results"][0];
    assert_eq!(
        first["ok"],
        json!(false),
        "agenda with unknown kwarg must fail; got: {first}"
    );
    let err = first["error"].as_str().unwrap_or("");
    assert!(
        err.contains("unknownkw") || err.contains("unknown field"),
        "error must mention the unknown field; got: {err}"
    );
    Ok(())
}

/// `brain.profile(id="balanced-recall-v1", unknownkw="x")` must return `ok: false`.
#[tokio::test]
async fn brain_profile_rejects_unknown_kwarg() -> anyhow::Result<()> {
    let client = connect_brain_only().await?;

    let result = call(
        &client,
        "request",
        json!({ "ops": r#"brain.profile(id="balanced-recall-v1", unknownkw="oops")"# }),
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    let first = &body["results"][0];
    assert_eq!(
        first["ok"],
        json!(false),
        "brain.profile with unknown kwarg must fail; got: {first}"
    );
    let err = first["error"].as_str().unwrap_or("");
    assert!(
        err.contains("unknownkw") || err.contains("unknown field"),
        "error must mention the unknown field; got: {err}"
    );
    Ok(())
}

fn make_knowledge_server() -> KhiveMcpServer {
    disable_daemon();
    let config = RuntimeConfig {
        db_path: None,
        default_namespace: Namespace::parse("knowtest").unwrap(),
        embedding_model: None,
        additional_embedding_models: vec![],
        packs: vec!["kg".to_string(), "knowledge".to_string()],
        ..RuntimeConfig::default()
    };
    let runtime = KhiveRuntime::new(config).expect("kg+knowledge runtime");
    KhiveMcpServer::new(runtime).expect("server builds with kg+knowledge")
}

async fn connect_knowledge(
) -> anyhow::Result<impl std::ops::Deref<Target = rmcp::service::Peer<rmcp::RoleClient>>> {
    let (server_transport, client_transport) = tokio::io::duplex(65536);
    let server = make_knowledge_server();
    tokio::spawn(async move {
        if let Ok(svc) = server.serve(server_transport).await {
            let _ = svc.waiting().await;
        }
    });
    let client = DummyClient.serve(client_transport).await?;
    Ok(client)
}

/// `topic(unknownkw="x")` (knowledge) must return `ok: false`.
#[tokio::test]
async fn topic_rejects_unknown_kwarg() -> anyhow::Result<()> {
    let client = connect_knowledge().await?;

    let result = call(
        &client,
        "request",
        json!({ "ops": r#"knowledge.topic(unknownkw="oops")"# }),
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    let first = &body["results"][0];
    assert_eq!(
        first["ok"],
        json!(false),
        "topic with unknown kwarg must fail; got: {first}"
    );
    let err = first["error"].as_str().unwrap_or("");
    assert!(
        err.contains("unknownkw") || err.contains("unknown field"),
        "error must mention the unknown field; got: {err}"
    );
    Ok(())
}

// ── #545: brain.feedback default Agent response preserves full target_id ──────

/// `brain.feedback` in default Agent mode must return `target_id` as the full
/// 36-char UUID, not the 8-char Agent-mode prefix (#545).
#[tokio::test]
async fn brain_feedback_default_agent_response_preserves_full_target_id() -> anyhow::Result<()> {
    let client = connect_brain_only().await?;

    let created = ok_one(
        &client,
        r#"create(kind="entity", entity_kind="concept", name="BrainFeedbackTarget")"#,
    )
    .await?;
    let target_id = created["id"]
        .as_str()
        .expect("created entity id")
        .to_string();
    assert_eq!(
        target_id.len(),
        36,
        "entity id from verbose ok_one must be 36-char"
    );

    // Use plain `call` (not `ok_one`) so Agent mode is not forced to verbose.
    let result = call(
        &client,
        "request",
        json!({"ops": format!(r#"brain.feedback(target_id="{target_id}", signal="useful")"#)}),
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    let first = &body["results"][0];
    assert_eq!(
        first["ok"],
        json!(true),
        "brain.feedback must succeed: {first}"
    );

    let returned = first["result"]["target_id"].as_str().unwrap_or("");
    assert_eq!(
        returned.len(),
        36,
        "brain.feedback Agent response target_id must be full 36-char UUID, got: {returned:?}"
    );
    assert_eq!(returned, target_id, "returned target_id must match input");
    Ok(())
}

// ── #81: exec JSON output valid when content contains backslash escapes ──────

/// Regression for #81: note content containing backslash escape sequences
/// (`\n`, `\\`, `\t`, `\"`) must produce valid, parseable JSON from
/// `dispatch_request_local` — the path exercised by `kkernel exec`.
///
/// Previously, a stale daemon binary (pre-c5ffc54) had a code path that
/// interpolated result strings into JSON without going through serde, causing
/// `Invalid \escape` parse errors at the caller. This test locks the correct
/// behavior: create → output parses clean; update → output parses clean;
/// content round-trips byte-identical.
#[tokio::test]
async fn exec_output_valid_json_with_backslash_escape_content() -> anyhow::Result<()> {
    use khive_mcp::tools::request::RequestParams;

    let server = {
        std::env::set_var("KHIVE_NO_DAEMON", "1");
        let config = khive_runtime::RuntimeConfig {
            db_path: None,
            default_namespace: khive_runtime::Namespace::parse("test").unwrap(),
            embedding_model: None,
            additional_embedding_models: vec![],
            packs: vec!["kg".to_string()],
            ..khive_runtime::RuntimeConfig::default()
        };
        let runtime = khive_runtime::KhiveRuntime::new(config).expect("in-memory runtime");
        khive_mcp::server::KhiveMcpServer::new(runtime).expect("server builds")
    };

    // Content with every common backslash-escape type: newline, tab, backslash,
    // embedded quote. The DSL arg is a JSON-quoted string so the parser sees
    // the escape sequences and serde_json decodes them to the real characters.
    let content_with_escapes = "line1\nline2\t\\tabbed\\ \"quoted\"";
    let create_ops = format!(
        r#"create(kind="observation", content="{}")"#,
        // Escape for the DSL/JSON string: newline → \n, tab → \t, backslash → \\, quote → \"
        content_with_escapes
            .replace('\\', r"\\")
            .replace('"', r#"\""#)
            .replace('\n', r"\n")
            .replace('\t', r"\t")
    );

    // ── Step 1: create — output must be valid JSON ────────────────────────────
    let create_out = server
        .dispatch_request_local(RequestParams {
            ops: create_ops,
            presentation: Some("verbose".to_string()),
            presentation_per_op: None,
            save_to: None,
            format: None,
            format_per_op: None,
        })
        .await
        .expect("dispatch must succeed");

    let create_body: Value = serde_json::from_str(&create_out)
        .expect("#81 regression: create output must be valid JSON");
    let first = &create_body["results"][0];
    assert_eq!(first["ok"], json!(true), "create must succeed: {first}");
    let note_id = first["result"]["id"].as_str().expect("id present");

    // ── Step 2: get — content round-trips byte-identical ─────────────────────
    let get_out = server
        .dispatch_request_local(RequestParams {
            ops: format!(r#"get(id="{note_id}")"#),
            presentation: Some("verbose".to_string()),
            presentation_per_op: None,
            save_to: None,
            format: None,
            format_per_op: None,
        })
        .await
        .expect("get dispatch must succeed");

    let get_body: Value =
        serde_json::from_str(&get_out).expect("#81 regression: get output must be valid JSON");
    let get_first = &get_body["results"][0];
    assert_eq!(
        get_first["ok"],
        json!(true),
        "get must succeed: {get_first}"
    );
    let got_content = get_first["result"]["content"]
        .as_str()
        .expect("content field present");
    assert_eq!(
        got_content, content_with_escapes,
        "#81 regression: content with backslash escapes must round-trip byte-identical"
    );

    // ── Step 3: update with new backslash content — output valid JSON ─────────
    let updated_content = "updated\\npath\\t\"value\"";
    let update_ops = format!(
        r#"update(id="{note_id}", content="{}")"#,
        updated_content
            .replace('\\', r"\\")
            .replace('"', r#"\""#)
            .replace('\n', r"\n")
            .replace('\t', r"\t")
    );
    let update_out = server
        .dispatch_request_local(RequestParams {
            ops: update_ops,
            presentation: Some("verbose".to_string()),
            presentation_per_op: None,
            save_to: None,
            format: None,
            format_per_op: None,
        })
        .await
        .expect("update dispatch must succeed");

    let update_body: Value = serde_json::from_str(&update_out)
        .expect("#81 regression: update output must be valid JSON");
    let update_first = &update_body["results"][0];
    assert_eq!(
        update_first["ok"],
        json!(true),
        "update must succeed: {update_first}"
    );

    Ok(())
}

// ── #546: schedule.agenda Agent response preserves properties.trigger_at ──────

/// Schedule agenda in default Agent mode must not compact `trigger_at` inside
/// `properties` — the full ISO-8601 string must round-trip verbatim (#546).
#[tokio::test]
async fn schedule_agenda_agent_preserves_properties_trigger_at_verbatim() -> anyhow::Result<()> {
    let client = connect_schedule_only().await?;
    let trigger_at = "2099-01-01T00:00:00Z";

    ok_one(
        &client,
        &format!(r#"schedule.remind(content="agent trigger_at fidelity", at="{trigger_at}")"#),
    )
    .await?;

    // Default Agent mode (no `presentation` key).
    let result = call(&client, "request", json!({"ops": "schedule.agenda()"})).await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    let first = &body["results"][0];
    assert_eq!(
        first["ok"],
        json!(true),
        "schedule.agenda must succeed: {first}"
    );

    let events = first["result"]["events"].as_array().expect("events array");
    assert!(!events.is_empty(), "agenda must have at least one event");
    let actual = events[0]["properties"]["trigger_at"].as_str().unwrap_or("");
    assert_eq!(
        actual, trigger_at,
        "trigger_at inside properties must be preserved verbatim in Agent mode"
    );
    assert_ne!(
        actual, "2099-01-01T00:00",
        "trigger_at must not be truncated to minute granularity"
    );
    Ok(())
}

// ── PR #121: proposal_id → id wire-key — DSL chain tests ─────────────────────
//
// These tests prove that `$prev.id` substitution works end-to-end through the
// request envelope for the proposal lifecycle.  Direct handler dispatch in
// crates/khive-pack-kg/tests/ does NOT prove this path — it bypasses the MCP
// request envelope, presentation mode selection, and $prev resolver.
//
// Each test uses the `|` DSL chain operator so the MCP server exercises the
// full substitution path defined in ADR-016.

/// Chain: propose(...) | review(id=$prev.id, decision="reject").
///
/// Asserts that $prev.id from the propose result is correctly substituted into
/// the review call, and that the resulting proposal status is "rejected".
/// Also asserts the old `proposal_id` wire key is absent from both results.
#[tokio::test]
async fn test_propose_pipe_review_reject_chain() -> anyhow::Result<()> {
    let client = connect().await?;

    // DSL chain: propose then immediately review with $prev.id.
    // The changeset uses DSL object syntax (JSON keys: "key":"value").
    let ops = r#"propose(title="ChainReviewRejectTest", description="pr121 chain test", changeset={"kind":"add_note","note":{"kind":"observation","content":"chain-review-reject"}}) | review(id=$prev.id, decision="reject")"#;
    let result = call(
        &client,
        "request",
        json!({"ops": ops, "presentation": "verbose"}),
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    let results = body["results"].as_array().expect("results array");

    assert_eq!(results.len(), 2, "expected 2 ops in chain");
    assert_eq!(
        results[0]["ok"],
        json!(true),
        "propose (op 0) must succeed: {}",
        results[0]
    );
    assert_eq!(
        results[1]["ok"],
        json!(true),
        "review(id=$prev.id) (op 1) must succeed — $prev.id from propose not resolved: {}",
        results[1]
    );
    assert_eq!(
        body["summary"]["succeeded"],
        json!(2),
        "both ops must succeed"
    );
    assert_eq!(body["summary"]["aborted"], json!(0));

    // The review result must report status "rejected".
    let review_result = &results[1]["result"];
    assert_eq!(
        review_result["status"].as_str().unwrap_or(""),
        "rejected",
        "review(decision=reject) must yield status rejected; got: {review_result}"
    );

    // Clean-break: neither result must expose the old wire key.
    assert!(
        results[0]["result"].get("proposal_id").is_none(),
        "propose result must NOT contain proposal_id; got: {}",
        results[0]["result"]
    );
    assert!(
        results[1]["result"].get("proposal_id").is_none(),
        "review result must NOT contain proposal_id; got: {}",
        results[1]["result"]
    );

    Ok(())
}

/// Chain: propose(...) | withdraw(id=$prev.id).
///
/// Asserts that $prev.id from the propose result is correctly substituted into
/// the withdraw call, and that the resulting proposal status is "withdrawn".
/// Also asserts the old `proposal_id` wire key is absent from both results.
#[tokio::test]
async fn test_propose_pipe_withdraw_chain() -> anyhow::Result<()> {
    let client = connect().await?;

    // DSL chain: propose then immediately withdraw with $prev.id.
    let ops = r#"propose(title="ChainWithdrawTest", description="pr121 chain test", changeset={"kind":"add_note","note":{"kind":"observation","content":"chain-withdraw"}}) | withdraw(id=$prev.id)"#;
    let result = call(
        &client,
        "request",
        json!({"ops": ops, "presentation": "verbose"}),
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    let results = body["results"].as_array().expect("results array");

    assert_eq!(results.len(), 2, "expected 2 ops in chain");
    assert_eq!(
        results[0]["ok"],
        json!(true),
        "propose (op 0) must succeed: {}",
        results[0]
    );
    assert_eq!(
        results[1]["ok"],
        json!(true),
        "withdraw(id=$prev.id) (op 1) must succeed — $prev.id from propose not resolved: {}",
        results[1]
    );
    assert_eq!(
        body["summary"]["succeeded"],
        json!(2),
        "both ops must succeed"
    );
    assert_eq!(body["summary"]["aborted"], json!(0));

    // The withdraw result must report status "withdrawn".
    let withdraw_result = &results[1]["result"];
    assert_eq!(
        withdraw_result["status"].as_str().unwrap_or(""),
        "withdrawn",
        "withdraw must yield status withdrawn; got: {withdraw_result}"
    );

    // Clean-break: neither result must expose the old wire key.
    assert!(
        results[0]["result"].get("proposal_id").is_none(),
        "propose result must NOT contain proposal_id; got: {}",
        results[0]["result"]
    );
    assert!(
        results[1]["result"].get("proposal_id").is_none(),
        "withdraw result must NOT contain proposal_id; got: {}",
        results[1]["result"]
    );

    Ok(())
}

// =============================================================================
// Finding 1: compute_config_id must include visible_namespaces so two configs
// with different visible sets produce distinct fingerprints.
// =============================================================================

/// Two RuntimeConfigs that are identical except for their `visible_namespaces`
/// must produce different `compute_config_id` fingerprints.
///
/// `visible_namespaces` widens the default multi-record read scope to
/// `['local'] ∪ visible_namespaces` on the OSS dispatch path (ADR-007 Rev 4
/// Rule 3b). The fingerprint includes the visible set so configs with different
/// read scopes are treated as distinct daemon configurations.
#[test]
fn compute_config_id_differs_when_visible_namespaces_differ() {
    use khive_mcp::server::compute_config_id;
    use khive_runtime::{Namespace, RuntimeConfig};

    let base = RuntimeConfig {
        db_path: None,
        default_namespace: Namespace::parse("vis-a").unwrap(),
        embedding_model: None,
        additional_embedding_models: vec![],
        packs: vec!["kg".to_string()],
        visible_namespaces: vec![],
        ..RuntimeConfig::default()
    };
    let with_visible = RuntimeConfig {
        visible_namespaces: vec![Namespace::parse("vis-b").unwrap()],
        ..base.clone()
    };

    let id_no_vis = compute_config_id(&base, None);
    let id_with_vis = compute_config_id(&with_visible, None);

    assert_ne!(
        id_no_vis, id_with_vis,
        "compute_config_id must differ when visible_namespaces differs; \
         same id would allow wrong-visibility daemon reuse"
    );
    assert!(
        id_with_vis.contains("vis-b"),
        "visible namespace 'vis-b' must appear in config_id string; got: {id_with_vis}"
    );
}

/// Order of entries in visible_namespaces must not change the fingerprint
/// (the fingerprint sorts + deduplicates before hashing).
#[test]
fn compute_config_id_is_stable_under_visible_namespace_reorder() {
    use khive_mcp::server::compute_config_id;
    use khive_runtime::{Namespace, RuntimeConfig};

    let cfg_ab = RuntimeConfig {
        db_path: None,
        default_namespace: Namespace::parse("vis-a").unwrap(),
        embedding_model: None,
        additional_embedding_models: vec![],
        packs: vec!["kg".to_string()],
        visible_namespaces: vec![
            Namespace::parse("vis-b").unwrap(),
            Namespace::parse("vis-c").unwrap(),
        ],
        ..RuntimeConfig::default()
    };
    let cfg_ba = RuntimeConfig {
        visible_namespaces: vec![
            Namespace::parse("vis-c").unwrap(),
            Namespace::parse("vis-b").unwrap(),
        ],
        ..cfg_ab.clone()
    };

    assert_eq!(
        compute_config_id(&cfg_ab, None),
        compute_config_id(&cfg_ba, None),
        "compute_config_id must be stable under reordering of visible_namespaces"
    );
}

// =============================================================================
// Fix 1: compute_config_id must include allowed_outbound_namespaces so a
// daemon started with a permissive outbound allowlist cannot be reused for a
// client whose config has an empty allowlist.
// =============================================================================

/// Two RuntimeConfigs that are identical except for their
/// `allowed_outbound_namespaces` must produce different `compute_config_id`
/// fingerprints.
///
/// Without this, a daemon started with `allowed_outbound_namespaces =
/// ["lambda:khive"]` could be reused for a client whose local config has an
/// empty allowlist — granting cross-namespace writes that the client should
/// fail closed on.
#[test]
fn compute_config_id_differs_when_allowed_outbound_namespaces_differ() {
    use khive_mcp::server::compute_config_id;
    use khive_runtime::{Namespace, RuntimeConfig};

    let base = RuntimeConfig {
        db_path: None,
        default_namespace: Namespace::parse("out-a").unwrap(),
        embedding_model: None,
        additional_embedding_models: vec![],
        packs: vec!["kg".to_string()],
        allowed_outbound_namespaces: vec![],
        ..RuntimeConfig::default()
    };
    let with_outbound = RuntimeConfig {
        allowed_outbound_namespaces: vec![Namespace::parse("lambda:khive").unwrap()],
        ..base.clone()
    };

    let id_empty = compute_config_id(&base, None);
    let id_with_outbound = compute_config_id(&with_outbound, None);

    assert_ne!(
        id_empty, id_with_outbound,
        "compute_config_id must differ when allowed_outbound_namespaces differs; \
         same id would allow wrong-allowlist daemon reuse"
    );
    assert!(
        id_with_outbound.contains("lambda:khive"),
        "allowed outbound namespace 'lambda:khive' must appear in config_id string; got: {id_with_outbound}"
    );
}

/// Order of entries in allowed_outbound_namespaces must not change the
/// fingerprint (the fingerprint sorts + deduplicates before hashing).
#[test]
fn compute_config_id_is_stable_under_allowed_outbound_namespace_reorder() {
    use khive_mcp::server::compute_config_id;
    use khive_runtime::{Namespace, RuntimeConfig};

    let cfg_ab = RuntimeConfig {
        db_path: None,
        default_namespace: Namespace::parse("out-a").unwrap(),
        embedding_model: None,
        additional_embedding_models: vec![],
        packs: vec!["kg".to_string()],
        allowed_outbound_namespaces: vec![
            Namespace::parse("lambda:khive").unwrap(),
            Namespace::parse("lambda:leo").unwrap(),
        ],
        ..RuntimeConfig::default()
    };
    let cfg_ba = RuntimeConfig {
        allowed_outbound_namespaces: vec![
            Namespace::parse("lambda:leo").unwrap(),
            Namespace::parse("lambda:khive").unwrap(),
        ],
        ..cfg_ab.clone()
    };
    let cfg_dup = RuntimeConfig {
        allowed_outbound_namespaces: vec![
            Namespace::parse("lambda:khive").unwrap(),
            Namespace::parse("lambda:leo").unwrap(),
            Namespace::parse("lambda:khive").unwrap(), // duplicate
        ],
        ..cfg_ab.clone()
    };

    assert_eq!(
        compute_config_id(&cfg_ab, None),
        compute_config_id(&cfg_ba, None),
        "compute_config_id must be stable under reordering of allowed_outbound_namespaces"
    );
    assert_eq!(
        compute_config_id(&cfg_ab, None),
        compute_config_id(&cfg_dup, None),
        "compute_config_id must be stable under duplication of allowed_outbound_namespaces"
    );
}

// =============================================================================
// ADR-007 Rev 2: dispatch honors the resolved namespace — an explicit `namespace=`
// request param when supplied, else `default_namespace` (`local` for OSS). Actor
// identity never silently routes storage (Rule 0, enforced at the config layer:
// see runtime.rs `..._actor_id_does_not_override_default_namespace`).
// =============================================================================

/// Proves the dispatch-side namespace contract at the real MCP server boundary.
///
/// All ops go through `dispatch_request_local`, so a regression at the
/// `VerbRegistry::dispatch` mint site (`pack.rs`) is caught here:
///
///   (a) with `default_namespace = local`, a plain `create` lands in `"local"`.
///
///   (b) an explicit `create(namespace="lambda:leo")` lands in `"lambda:leo"` —
///       the caller deliberately targeting a namespace (Rule 1).  This is exactly
///       what #159's unconditional `Namespace::local()` hard-pin wrongly collapsed
///       to `"local"`.
///
///   (c) a default `list` (local) sees the local entity but NOT the lambda:leo one,
///       and `list(namespace="lambda:leo")` sees the lambda:leo entity but NOT the
///       local one — multi-record reads filter by the supplied namespace.
///
/// Regression sensitivity: if dispatch reverts to pinning `Namespace::local()`,
/// assertion (b) fails (the entity namespace would be `"local"`) and the scoped
/// list in (c) would be empty.
#[tokio::test]
async fn dispatch_honors_explicit_namespace_else_local_adr007() {
    use khive_mcp::tools::request::RequestParams;
    use khive_runtime::{KhiveRuntime, Namespace, RuntimeConfig};

    disable_daemon();

    // OSS default: default_namespace = local; actor identity is attribution only.
    let cfg = RuntimeConfig {
        db_path: None,
        default_namespace: Namespace::local(),
        embedding_model: None,
        additional_embedding_models: vec![],
        packs: vec!["kg".to_string()],
        ..RuntimeConfig::default()
    };

    let rt = KhiveRuntime::new(cfg).expect("in-memory runtime");
    let server =
        KhiveMcpServer::with_packs(rt.clone(), &["kg".to_string()]).expect("server builds");

    // Dispatch one `create`/`list` op and return the parsed `results[0]` body.
    async fn dispatch_op(server: &KhiveMcpServer, ops: &str) -> Value {
        let out = server
            .dispatch_request_local(RequestParams {
                ops: ops.to_string(),
                presentation: Some("verbose".to_string()),
                presentation_per_op: None,
                save_to: None,
                format: None,
                format_per_op: None,
            })
            .await
            .expect("dispatch must not error at the MCP level");
        let body: Value = serde_json::from_str(&out).expect("response must be valid JSON");
        body["results"][0].clone()
    }

    fn list_ids(result: &Value) -> Vec<String> {
        match result["result"].as_array() {
            Some(arr) => arr
                .iter()
                .filter_map(|e| e.get("id").and_then(|v| v.as_str()).map(str::to_string))
                .collect(),
            None => match result["result"]["items"].as_array() {
                Some(arr) => arr
                    .iter()
                    .filter_map(|e| e.get("id").and_then(|v| v.as_str()).map(str::to_string))
                    .collect(),
                None => {
                    panic!("list result must be a JSON array or object with items; got: {result}")
                }
            },
        }
    }

    // ── (a) DEFAULT CREATE: lands in "local" ─────────────────────────────────
    let default_res = dispatch_op(&server, r#"create(kind="concept", name="DefaultProbe")"#).await;
    assert_eq!(
        default_res["ok"],
        json!(true),
        "default create must succeed; got: {default_res}"
    );
    assert_eq!(
        default_res["result"]["namespace"].as_str().unwrap_or(""),
        "local",
        "a create with no explicit namespace must land in 'local'; got: {default_res}"
    );
    let default_id = default_res["result"]["id"]
        .as_str()
        .expect("create result must carry 'id'")
        .to_string();

    // ── (b) EXPLICIT CREATE: namespace="lambda:leo" is honored ───────────────
    let named_res = dispatch_op(
        &server,
        r#"create(kind="concept", name="NamedProbe", namespace="lambda:leo")"#,
    )
    .await;
    assert_eq!(
        named_res["ok"],
        json!(true),
        "explicit-namespace create must succeed; got: {named_res}"
    );
    assert_eq!(
        named_res["result"]["namespace"].as_str().unwrap_or(""),
        "lambda:leo",
        "create(namespace=\"lambda:leo\") must land in 'lambda:leo', not be collapsed to \
         'local' (ADR-007 Rev 2 Rule 1 — explicit namespace is honored); got: {named_res}"
    );
    let named_id = named_res["result"]["id"]
        .as_str()
        .expect("create result must carry 'id'")
        .to_string();

    // ── (c) LIST scoping: default(local) vs explicit(lambda:leo) ─────────────
    let local_ids = list_ids(&dispatch_op(&server, r#"list(kind="entity")"#).await);
    assert!(
        local_ids.contains(&default_id),
        "default list must see the local entity; got: {local_ids:?}"
    );
    assert!(
        !local_ids.contains(&named_id),
        "default(local) list must NOT see the lambda:leo entity; got: {local_ids:?}"
    );

    let leo_ids =
        list_ids(&dispatch_op(&server, r#"list(kind="entity", namespace="lambda:leo")"#).await);
    assert!(
        leo_ids.contains(&named_id),
        "list(namespace=lambda:leo) must see the lambda:leo entity; got: {leo_ids:?}"
    );
    assert!(
        !leo_ids.contains(&default_id),
        "list(namespace=lambda:leo) must NOT see the local entity; got: {leo_ids:?}"
    );
}

// ── ADR-078 server-level format tests (Medium 4 — server seam coverage) ──────
//
// These tests drive `dispatch_request_local` directly so they pin the PUBLIC
// server behaviour that combines `format`, `format_per_op`,
// `presentation_per_op`, ok entries, and error entries.

fn make_format_server() -> KhiveMcpServer {
    std::env::set_var("KHIVE_NO_DAEMON", "1");
    let config = khive_runtime::RuntimeConfig {
        db_path: None,
        default_namespace: khive_runtime::Namespace::parse("test").unwrap(),
        embedding_model: None,
        additional_embedding_models: vec![],
        packs: vec!["kg".to_string(), "gtd".to_string()],
        ..khive_runtime::RuntimeConfig::default()
    };
    let rt = khive_runtime::KhiveRuntime::new(config).expect("in-memory runtime");
    KhiveMcpServer::new(rt).expect("server builds")
}

/// (fmt-1) Mixed ok/error batch under `format=auto`: error entries must always
/// be compact JSON; ok entries must be formatted with the requested format.
///
/// ADR-078 §8.2: error envelopes are never passed through auto/table renderers.
/// ADR-078 §8.4: ok results are rendered per-op.
#[tokio::test]
async fn format_auto_mixed_ok_error_batch_error_stays_compact() {
    use khive_mcp::tools::request::RequestParams;

    let server = make_format_server();

    // Batch: op0 succeeds (stats()), op1 fails (bad verb).
    let params = RequestParams {
        ops: r#"[stats(), no_such_verb()]"#.to_string(),
        presentation: None,
        presentation_per_op: None,
        save_to: None,
        format: Some("auto".to_string()),
        format_per_op: None,
    };

    let raw = server
        .dispatch_request_local(params)
        .await
        .expect("dispatch must not itself fail");

    // The outer envelope must be valid JSON regardless of format.
    let body: serde_json::Value =
        serde_json::from_str(&raw).expect("envelope must be valid JSON under format=auto");

    let results = body["results"]
        .as_array()
        .expect("results array must be present");
    assert_eq!(results.len(), 2, "batch must produce 2 result entries");

    // op0 succeeded — result should have been formatted (rendered as string).
    assert_eq!(
        results[0]["ok"],
        serde_json::json!(true),
        "op0 (stats) must succeed: {}",
        results[0]
    );

    // op1 failed — error envelope must be compact JSON (never reformatted).
    assert_eq!(
        results[1]["ok"],
        serde_json::json!(false),
        "op1 (no_such_verb) must fail: {}",
        results[1]
    );
    // The error entry must contain a string error field, not a rendered table.
    assert!(
        results[1]["error"].is_string(),
        "error field must be a plain string, not reformatted: {}",
        results[1]
    );
    // Summary must always be present and valid.
    assert_eq!(body["summary"]["total"], serde_json::json!(2));
}

/// (fmt-2) `format_per_op` overrides: op0 gets json, op1 gets auto.
///
/// Pins ADR-078 §8.4: a single `format` applies uniformly; `format_per_op`
/// overrides per position.
#[tokio::test]
async fn format_per_op_override_selects_format_per_position() {
    use khive_mcp::tools::request::RequestParams;

    let server = make_format_server();

    // Create two entities, then list — use gtd.assign so results are non-trivial.
    // First build a state: two assign ops (both json), then one stats op (auto).
    // Simpler: two parallel stats() calls — one forced json, one forced auto.
    let params = RequestParams {
        ops: r#"[stats(), stats()]"#.to_string(),
        presentation: None,
        presentation_per_op: None,
        save_to: None,
        // Batch default is auto, but op0 overrides to json.
        format: Some("auto".to_string()),
        format_per_op: Some(vec![
            Some("json".to_string()), // op0 → json (compact, parseable)
            None,                     // op1 → inherits batch "auto"
        ]),
    };

    let raw = server
        .dispatch_request_local(params)
        .await
        .expect("dispatch must succeed");

    let body: serde_json::Value = serde_json::from_str(&raw).expect("valid JSON envelope");
    let results = body["results"].as_array().expect("results array");
    assert_eq!(results.len(), 2);

    // op0 was forced json → the result stays as the original JSON Value (not
    // wrapped as a string).  When format=json, render_result passes the entry
    // through unchanged (the entry.result remains a JSON object, not a string).
    assert_eq!(results[0]["ok"], serde_json::json!(true));
    let op0_result = &results[0]["result"];
    assert!(
        op0_result.is_object(),
        "json-format op result must remain a JSON object (not reformatted to string): {op0_result}"
    );
    // stats() returns entity/edge/note counts — these fields must be present.
    assert!(
        op0_result.get("entities").is_some() || op0_result.get("total").is_some(),
        "op0 json result must contain stats fields: {op0_result}"
    );

    // op1 inherited auto → result is a rendered string (not a raw json object).
    assert_eq!(results[1]["ok"], serde_json::json!(true));
    assert!(
        results[1]["result"].is_string(),
        "auto-format op result must be a rendered string: {}",
        results[1]
    );

    // Summary envelope must be compact JSON regardless of format.
    assert_eq!(body["summary"]["total"], serde_json::json!(2));
    assert_eq!(body["summary"]["succeeded"], serde_json::json!(2));
}

/// (fmt-3) `presentation_per_op=verbose` pins High 1: a verbose op under
/// `format=auto` must preserve `full_id`, `namespace="local"`, and duplicate
/// `properties` keys — the redundancy-drop pre-pass must be skipped.
///
/// This is the regression pin for the bug fixed in this round: the batch
/// `presentation` was passed to `render_format` instead of the per-op
/// effective presentation, so `full_id`/`namespace`/duplicate-props could
/// be stripped even when that specific op was verbose.
#[tokio::test]
async fn presentation_per_op_verbose_preserves_full_id_namespace_and_props() {
    use khive_mcp::tools::request::RequestParams;

    let server = make_format_server();

    // Create a GTD task so we have a record with duplicated properties
    // (assignee/priority/status echoed in both top-level and `properties`).
    let create_params = RequestParams {
        ops: r#"gtd.assign(title="verbose-pin-task", priority="p1", assignee="lambda:test")"#
            .to_string(),
        presentation: Some("verbose".to_string()),
        presentation_per_op: None,
        save_to: None,
        format: None,
        format_per_op: None,
    };
    let create_raw = server
        .dispatch_request_local(create_params)
        .await
        .expect("task creation must succeed");
    let create_body: serde_json::Value = serde_json::from_str(&create_raw).unwrap();
    let task_id = create_body["results"][0]["result"]["id"]
        .as_str()
        .expect("task id must be present");

    // Now fetch as a 2-op batch: op0 = agent (will be redundancy-dropped),
    // op1 = verbose (must survive the redundancy-drop pre-pass).
    //
    // We use gtd.tasks (which returns records with assignee/priority/status in both
    // top-level AND properties), then get the specific task in verbose mode.
    let batch_params = RequestParams {
        ops: format!(r#"[gtd.tasks(limit=10), get(id="{task_id}")]"#),
        // Batch default: agent (will apply redundancy drop).
        presentation: Some("agent".to_string()),
        // Op1 overrides to verbose — must skip redundancy drop for that op.
        presentation_per_op: Some(vec![
            None,                        // op0 → inherits agent
            Some("verbose".to_string()), // op1 → verbose
        ]),
        save_to: None,
        format: Some("auto".to_string()),
        format_per_op: None,
    };

    let raw = server
        .dispatch_request_local(batch_params)
        .await
        .expect("batch dispatch must succeed");

    let body: serde_json::Value = serde_json::from_str(&raw).expect("valid JSON envelope");
    let results = body["results"].as_array().expect("results array");
    assert_eq!(results.len(), 2, "batch must return 2 results");

    // op0 was agent + auto → redundancy drop applied (namespace="local" elided).
    // The result is a rendered string; just verify it succeeded.
    assert_eq!(
        results[0]["ok"],
        serde_json::json!(true),
        "op0 (gtd.tasks) must succeed: {}",
        results[0]
    );

    // op1 was verbose + auto → redundancy drop MUST be skipped.
    assert_eq!(
        results[1]["ok"],
        serde_json::json!(true),
        "op1 (get task) must succeed: {}",
        results[1]
    );

    // The rendered result for op1 is a string (auto-formatted).
    // Under verbose mode the redundancy-drop pre-pass is skipped.
    //
    // Key differences vs. agent+auto:
    //   - In agent mode `present_response` shortens `id` to 8 chars and adds a
    //     separate `full_id` field; redundancy drop then removes that `full_id`.
    //   - In verbose mode `present_response` keeps the full 36-char UUID in `id`
    //     directly — there is NO separate `full_id` field to strip.
    //
    // What we can assert positively:
    //   - `namespace` appears (in agent+auto it is elided when "local" per §7.3).
    //   - `properties` keys that duplicate top-level fields (assignee, priority,
    //     status) survive (in agent+auto they are deduped away per §7.2).
    let op1_rendered = results[1]["result"]
        .as_str()
        .expect("op1 result must be a rendered string");

    // namespace: elided when "local" in auto+agent mode (§7.3); kept in verbose.
    assert!(
        op1_rendered.contains("namespace"),
        "verbose op: namespace must survive the redundancy-drop pre-pass; rendered: {op1_rendered}"
    );

    // properties: duplicate keys (assignee, priority, status) must survive in
    // verbose mode (§7.2 dedup is skipped).  In agent+auto these are removed.
    assert!(
        op1_rendered.contains("assignee"),
        "verbose op: properties.assignee must survive the redundancy-drop pre-pass; rendered: {op1_rendered}"
    );
    assert!(
        op1_rendered.contains("priority"),
        "verbose op: properties.priority must survive the redundancy-drop pre-pass; rendered: {op1_rendered}"
    );
}

/// (fmt-4) AlwaysVerbose verbs must skip the redundancy-drop pre-pass under
/// `format=auto` even with the DEFAULT Agent presentation and NO per-op override.
///
/// Pins the round-2 finding: `render_result` recomputed the format-time
/// presentation only from `presentation_per_op` → batch default, blind to the
/// `VerbPresentationPolicy::AlwaysVerbose` that `run_parsed` applies. So a
/// policy-verbose verb (`get`) under `format=auto` with the default Agent mode
/// was redundancy-dropped, stripping `namespace="local"` and duplicate
/// `properties` keys it is declared AlwaysVerbose to preserve. The fix folds the
/// AlwaysVerbose policy into the format-seam presentation. This is the *implicit
/// policy* sibling of `presentation_per_op_verbose_preserves_*` (explicit override).
#[tokio::test]
async fn format_auto_always_verbose_verb_skips_redundancy_drop_without_override() {
    use khive_mcp::tools::request::RequestParams;

    let server = make_format_server();

    // Create a GTD task: assignee/priority/status are echoed in both top-level
    // and `properties`, and the record carries namespace="local".
    let create_params = RequestParams {
        ops: r#"gtd.assign(title="always-verbose-pin", priority="p1", assignee="lambda:test")"#
            .to_string(),
        presentation: Some("verbose".to_string()),
        presentation_per_op: None,
        save_to: None,
        format: None,
        format_per_op: None,
    };
    let create_raw = server
        .dispatch_request_local(create_params)
        .await
        .expect("task creation must succeed");
    let create_body: serde_json::Value = serde_json::from_str(&create_raw).unwrap();
    let task_id = create_body["results"][0]["result"]["id"]
        .as_str()
        .expect("task id must be present");

    // get() is AlwaysVerbose (ADR-045 §6). Dispatch it under format=auto with the
    // DEFAULT Agent presentation and NO presentation_per_op override. The
    // AlwaysVerbose policy must force Verbose at the format seam, so the
    // redundancy-drop pre-pass is skipped and namespace/properties survive.
    let get_params = RequestParams {
        ops: format!(r#"get(id="{task_id}")"#),
        presentation: None,        // → default Agent
        presentation_per_op: None, // → no per-op override
        save_to: None,
        format: Some("auto".to_string()),
        format_per_op: None,
    };
    let raw = server
        .dispatch_request_local(get_params)
        .await
        .expect("get dispatch must succeed");

    let body: serde_json::Value = serde_json::from_str(&raw).expect("valid JSON envelope");
    let results = body["results"].as_array().expect("results array");
    assert_eq!(results.len(), 1, "single get must produce 1 result");
    assert_eq!(
        results[0]["ok"],
        serde_json::json!(true),
        "get must succeed: {}",
        results[0]
    );

    let rendered = results[0]["result"]
        .as_str()
        .expect("get result must be a rendered string under format=auto");

    // namespace="local" is elided under agent+auto (§7.3) but MUST survive for an
    // AlwaysVerbose verb — this is the regression the round-2 fix closes.
    assert!(
        rendered.contains("namespace"),
        "AlwaysVerbose get: namespace must survive redundancy-drop under format=auto + \
         default agent (no override); rendered: {rendered}"
    );
    // Duplicate properties keys (assignee/priority) are deduped under agent+auto
    // but MUST survive for an AlwaysVerbose verb.
    assert!(
        rendered.contains("assignee"),
        "AlwaysVerbose get: properties.assignee must survive redundancy-drop; rendered: {rendered}"
    );
    assert!(
        rendered.contains("priority"),
        "AlwaysVerbose get: properties.priority must survive redundancy-drop; rendered: {rendered}"
    );
}
