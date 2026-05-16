//! Integration tests for khive-mcp.
//!
//! Uses rmcp's duplex transport to test the full JSON-RPC path end-to-end.

use khive_mcp::server::KhiveMcpServer;
use khive_runtime::{KhiveRuntime, RuntimeConfig};
use rmcp::{
    model::{CallToolRequestParams, ClientInfo},
    ClientHandler, ServerHandler, ServiceExt,
};
use serde_json::json;

fn make_server() -> KhiveMcpServer {
    let config = RuntimeConfig {
        db_path: None,
        default_namespace: "test".to_string(),
        embedding_model: None,
    };
    let runtime = KhiveRuntime::new(config).expect("in-memory runtime");
    KhiveMcpServer::new(runtime)
}

// Minimal client handler needed to form a transport pair.
#[derive(Clone, Default)]
struct DummyClient;

impl ClientHandler for DummyClient {
    fn get_info(&self) -> ClientInfo {
        ClientInfo::default()
    }
}

// Spawn a server on a duplex socket and return a connected client.
async fn connect(
) -> anyhow::Result<impl std::ops::Deref<Target = rmcp::service::Peer<rmcp::RoleClient>>> {
    let (server_transport, client_transport) = tokio::io::duplex(65536);
    let server = make_server();
    tokio::spawn(async move {
        let _ = server.serve(server_transport).await?.waiting().await;
        anyhow::Ok(())
    });
    let client = DummyClient.serve(client_transport).await?;
    Ok(client)
}

// Helper: extract text from first content item.
fn first_text(result: &rmcp::model::CallToolResult) -> String {
    result
        .content
        .first()
        .and_then(|c| c.raw.as_text())
        .map(|t| t.text.clone())
        .unwrap_or_default()
}

// Helper: call a tool with JSON arguments.
async fn call(
    client: &impl std::ops::Deref<Target = rmcp::service::Peer<rmcp::RoleClient>>,
    name: impl Into<String>,
    args: serde_json::Value,
) -> anyhow::Result<rmcp::model::CallToolResult> {
    let params = CallToolRequestParams::new(name.into())
        .with_arguments(args.as_object().expect("args must be JSON object").clone());
    Ok(client.call_tool(params).await?)
}

// ---- Tests ----

#[tokio::test]
async fn server_info_contains_name_and_tools_capability() {
    let server = make_server();
    let info = server.get_info();
    assert_eq!(info.server_info.name, "khive-mcp");
    assert!(
        info.capabilities.tools.is_some(),
        "tools capability must be advertised"
    );
}

#[tokio::test]
async fn list_tools_returns_sixteen_tools() -> anyhow::Result<()> {
    let client = connect().await?;
    let result = client.list_tools(None).await?;
    assert_eq!(
        result.tools.len(),
        16,
        "expected 16 tools, got {}",
        result.tools.len()
    );
    Ok(())
}

#[tokio::test]
async fn tool_names_are_correct() -> anyhow::Result<()> {
    let client = connect().await?;
    let result = client.list_tools(None).await?;
    let names: std::collections::HashSet<String> =
        result.tools.iter().map(|t| t.name.to_string()).collect();
    for expected in [
        "entity_create",
        "entity_get",
        "entity_list",
        "entity_delete",
        "entity_update",
        "entity_merge",
        "link",
        "neighbors",
        "traverse",
        "edge_get",
        "edge_list",
        "edge_update",
        "edge_delete",
        "note_create",
        "note_list",
        "query",
    ] {
        assert!(names.contains(expected), "missing tool: {expected}");
    }
    Ok(())
}

#[tokio::test]
async fn entity_create_roundtrip() -> anyhow::Result<()> {
    let client = connect().await?;
    let result = call(
        &client,
        "entity_create",
        json!({
            "kind": "concept",
            "name": "FlashAttention",
            "description": "Memory-efficient exact attention",
            "properties": {"type": "algorithm", "domain": "attention"}
        }),
    )
    .await?;

    let text = first_text(&result);
    let entity: serde_json::Value = serde_json::from_str(&text).expect("result must be JSON");
    assert_eq!(entity["kind"], "concept");
    assert_eq!(entity["name"], "FlashAttention");
    assert!(entity["id"].is_string(), "id must be a string UUID");
    Ok(())
}

#[tokio::test]
async fn entity_get_returns_existing() -> anyhow::Result<()> {
    let client = connect().await?;

    let create = call(
        &client,
        "entity_create",
        json!({"kind": "document", "name": "Attention Is All You Need"}),
    )
    .await?;
    let created: serde_json::Value = serde_json::from_str(&first_text(&create)).unwrap();
    let id = created["id"].as_str().unwrap().to_string();

    let get = call(&client, "entity_get", json!({"id": id})).await?;
    let fetched: serde_json::Value = serde_json::from_str(&first_text(&get)).unwrap();
    assert_eq!(fetched["name"], "Attention Is All You Need");
    Ok(())
}

#[tokio::test]
async fn entity_get_missing_returns_error() -> anyhow::Result<()> {
    let client = connect().await?;
    let result = client
        .call_tool(
            CallToolRequestParams::new("entity_get").with_arguments(
                json!({"id": "00000000-0000-0000-0000-000000000000"})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await;
    // MCP protocol may return the error as an Err or as a failed CallToolResult
    match result {
        Err(_) => {}
        Ok(r) => {
            // Error may be encoded as isError=true in content
            assert!(
                r.is_error.unwrap_or(false),
                "missing entity should be flagged as error"
            );
        }
    }
    Ok(())
}

#[tokio::test]
async fn entity_list_returns_all_created() -> anyhow::Result<()> {
    let client = connect().await?;
    for name in ["LoRA", "QLoRA", "DoRA"] {
        call(
            &client,
            "entity_create",
            json!({"kind": "concept", "name": name}),
        )
        .await?;
    }

    let result = call(&client, "entity_list", json!({"kind": "concept"})).await?;
    let entities: Vec<serde_json::Value> = serde_json::from_str(&first_text(&result)).unwrap();
    assert_eq!(entities.len(), 3);
    Ok(())
}

#[tokio::test]
async fn entity_delete_succeeds() -> anyhow::Result<()> {
    let client = connect().await?;

    let create = call(
        &client,
        "entity_create",
        json!({"kind": "concept", "name": "ToDelete"}),
    )
    .await?;
    let created: serde_json::Value = serde_json::from_str(&first_text(&create)).unwrap();
    let id = created["id"].as_str().unwrap().to_string();

    let del = call(&client, "entity_delete", json!({"id": id})).await?;
    let result: serde_json::Value = serde_json::from_str(&first_text(&del)).unwrap();
    assert_eq!(result["deleted"], true);
    Ok(())
}

#[tokio::test]
async fn link_creates_edge() -> anyhow::Result<()> {
    let client = connect().await?;

    let a = call(
        &client,
        "entity_create",
        json!({"kind": "concept", "name": "A"}),
    )
    .await?;
    let b = call(
        &client,
        "entity_create",
        json!({"kind": "concept", "name": "B"}),
    )
    .await?;

    let a_id = serde_json::from_str::<serde_json::Value>(&first_text(&a)).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let b_id = serde_json::from_str::<serde_json::Value>(&first_text(&b)).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();

    let link = call(
        &client,
        "link",
        json!({"source_id": a_id, "target_id": b_id, "relation": "extends", "weight": 0.9}),
    )
    .await?;
    let edge: serde_json::Value = serde_json::from_str(&first_text(&link)).unwrap();
    assert_eq!(edge["relation"], "extends");
    assert!((edge["weight"].as_f64().unwrap() - 0.9).abs() < 0.001);
    Ok(())
}

#[tokio::test]
async fn neighbors_returns_linked_nodes() -> anyhow::Result<()> {
    let client = connect().await?;

    let a = call(
        &client,
        "entity_create",
        json!({"kind": "concept", "name": "Root"}),
    )
    .await?;
    let b = call(
        &client,
        "entity_create",
        json!({"kind": "concept", "name": "Child"}),
    )
    .await?;

    let a_id = serde_json::from_str::<serde_json::Value>(&first_text(&a)).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let b_id = serde_json::from_str::<serde_json::Value>(&first_text(&b)).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();

    call(
        &client,
        "link",
        json!({"source_id": a_id, "target_id": b_id, "relation": "contains"}),
    )
    .await?;

    let nbr = call(
        &client,
        "neighbors",
        json!({"node_id": a_id, "direction": "out"}),
    )
    .await?;
    let hits: Vec<serde_json::Value> = serde_json::from_str(&first_text(&nbr)).unwrap();
    assert_eq!(hits.len(), 1, "should have exactly one neighbor");
    assert_eq!(hits[0]["node_id"].as_str().unwrap(), b_id);
    Ok(())
}

#[tokio::test]
async fn traverse_reaches_multi_hop_nodes() -> anyhow::Result<()> {
    let client = connect().await?;

    let a = call(
        &client,
        "entity_create",
        json!({"kind": "concept", "name": "A"}),
    )
    .await?;
    let b = call(
        &client,
        "entity_create",
        json!({"kind": "concept", "name": "B"}),
    )
    .await?;
    let c = call(
        &client,
        "entity_create",
        json!({"kind": "concept", "name": "C"}),
    )
    .await?;

    let a_id = serde_json::from_str::<serde_json::Value>(&first_text(&a)).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let b_id = serde_json::from_str::<serde_json::Value>(&first_text(&b)).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let c_id = serde_json::from_str::<serde_json::Value>(&first_text(&c)).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();

    call(
        &client,
        "link",
        json!({"source_id": a_id, "target_id": b_id, "relation": "extends"}),
    )
    .await?;
    call(
        &client,
        "link",
        json!({"source_id": b_id, "target_id": c_id, "relation": "extends"}),
    )
    .await?;

    let trav = call(
        &client,
        "traverse",
        json!({"roots": [a_id], "max_depth": 2, "include_roots": false}),
    )
    .await?;
    let paths: Vec<serde_json::Value> = serde_json::from_str(&first_text(&trav)).unwrap();
    let node_ids: Vec<String> = paths
        .iter()
        .flat_map(|p| p["nodes"].as_array().unwrap())
        .map(|n| n["node_id"].as_str().unwrap().to_string())
        .collect();
    assert!(node_ids.contains(&b_id), "B must be reachable");
    assert!(node_ids.contains(&c_id), "C must be reachable at depth 2");
    Ok(())
}

#[tokio::test]
async fn note_create_and_list_roundtrip() -> anyhow::Result<()> {
    let client = connect().await?;

    call(
        &client,
        "note_create",
        json!({"kind": "decision", "content": "Use FlashAttention-2 for attention", "salience": 0.9}),
    )
    .await?;

    let notes = call(&client, "note_list", json!({"kind": "decision"})).await?;
    let items: Vec<serde_json::Value> = serde_json::from_str(&first_text(&notes)).unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["content"], "Use FlashAttention-2 for attention");
    Ok(())
}

#[tokio::test]
async fn note_create_with_canonical_kind() -> anyhow::Result<()> {
    let client = connect().await?;
    let result = call(
        &client,
        "note_create",
        json!({"kind": "observation", "content": "FlashAttention is fast"}),
    )
    .await?;
    assert!(
        !result.is_error.unwrap_or(false),
        "canonical kind 'observation' should succeed"
    );
    let note: serde_json::Value = serde_json::from_str(&first_text(&result)).unwrap();
    assert_eq!(note["kind"], "observation");
    Ok(())
}

#[tokio::test]
async fn note_create_with_alias_obs() -> anyhow::Result<()> {
    let client = connect().await?;
    let result = call(
        &client,
        "note_create",
        json!({"kind": "obs", "content": "GQA reduces KV cache memory"}),
    )
    .await?;
    assert!(
        !result.is_error.unwrap_or(false),
        "alias 'obs' should map to observation"
    );
    let note: serde_json::Value = serde_json::from_str(&first_text(&result)).unwrap();
    assert_eq!(note["kind"], "observation");
    Ok(())
}

#[tokio::test]
async fn note_create_unknown_kind_returns_error() -> anyhow::Result<()> {
    let client = connect().await?;
    let result = client
        .call_tool(
            CallToolRequestParams::new("note_create").with_arguments(
                json!({"kind": "garbage", "content": "should fail"})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await;
    match result {
        Err(_) => {}
        Ok(r) => {
            assert!(
                r.is_error.unwrap_or(false),
                "unknown kind 'garbage' should produce an error response"
            );
            let text = first_text(&r);
            assert!(
                text.contains("observation") || text.contains("invalid"),
                "error message should mention valid kinds, got: {text}"
            );
        }
    }
    Ok(())
}

#[tokio::test]
async fn note_list_empty_returns_empty_array() -> anyhow::Result<()> {
    let client = connect().await?;
    let result = call(&client, "note_list", json!({})).await?;
    let items: Vec<serde_json::Value> = serde_json::from_str(&first_text(&result)).unwrap();
    assert!(items.is_empty(), "fresh namespace should have no notes");
    Ok(())
}

#[tokio::test]
async fn invalid_uuid_returns_error() -> anyhow::Result<()> {
    let client = connect().await?;
    let result = client
        .call_tool(
            CallToolRequestParams::new("entity_get")
                .with_arguments(json!({"id": "not-a-uuid"}).as_object().unwrap().clone()),
        )
        .await;
    match result {
        Err(_) => {}
        Ok(r) => {
            assert!(
                r.is_error.unwrap_or(false),
                "invalid UUID should produce error response"
            );
        }
    }
    Ok(())
}

#[tokio::test]
async fn entity_list_limit_is_respected() -> anyhow::Result<()> {
    let client = connect().await?;
    for i in 0..10 {
        call(
            &client,
            "entity_create",
            json!({"kind": "concept", "name": format!("Entity{i}")}),
        )
        .await?;
    }
    let result = call(&client, "entity_list", json!({"limit": 3})).await?;
    let items: Vec<serde_json::Value> = serde_json::from_str(&first_text(&result)).unwrap();
    assert!(
        items.len() <= 3,
        "result should respect limit, got {}",
        items.len()
    );
    Ok(())
}

// =============================================================================
// ADR-014 curation tools
// =============================================================================

#[tokio::test]
async fn entity_update_tool_patches_name() -> anyhow::Result<()> {
    let client = connect().await?;

    let create = call(
        &client,
        "entity_create",
        json!({"kind": "concept", "name": "OldName", "description": "keep this"}),
    )
    .await?;
    let created: serde_json::Value = serde_json::from_str(&first_text(&create)).unwrap();
    let id = created["id"].as_str().unwrap().to_string();

    // Only patch name — description must be unchanged.
    let update = call(
        &client,
        "entity_update",
        json!({"id": id, "name": "NewName"}),
    )
    .await?;
    let updated: serde_json::Value = serde_json::from_str(&first_text(&update)).unwrap();
    assert_eq!(updated["name"], "NewName");
    assert_eq!(updated["description"], "keep this");
    Ok(())
}

#[tokio::test]
async fn entity_merge_tool_returns_summary() -> anyhow::Result<()> {
    let client = connect().await?;

    let a = call(
        &client,
        "entity_create",
        json!({"kind": "concept", "name": "MergeInto"}),
    )
    .await?;
    let b = call(
        &client,
        "entity_create",
        json!({"kind": "concept", "name": "MergeFrom"}),
    )
    .await?;
    let a_id = serde_json::from_str::<serde_json::Value>(&first_text(&a)).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let b_id = serde_json::from_str::<serde_json::Value>(&first_text(&b)).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();

    let merge = call(
        &client,
        "entity_merge",
        json!({"into_id": a_id, "from_id": b_id, "strategy": "prefer_into"}),
    )
    .await?;
    let summary: serde_json::Value = serde_json::from_str(&first_text(&merge)).unwrap();
    assert_eq!(summary["kept_id"].as_str().unwrap(), a_id);
    assert_eq!(summary["removed_id"].as_str().unwrap(), b_id);
    assert!(summary["edges_rewired"].is_number());
    assert!(summary["properties_merged"].is_number());
    assert!(summary["tags_unioned"].is_number());
    Ok(())
}

#[tokio::test]
async fn edge_list_tool_returns_edges() -> anyhow::Result<()> {
    let client = connect().await?;

    let a = call(
        &client,
        "entity_create",
        json!({"kind": "concept", "name": "EdgeSrc"}),
    )
    .await?;
    let b = call(
        &client,
        "entity_create",
        json!({"kind": "concept", "name": "EdgeTgt"}),
    )
    .await?;
    let a_id = serde_json::from_str::<serde_json::Value>(&first_text(&a)).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let b_id = serde_json::from_str::<serde_json::Value>(&first_text(&b)).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();

    call(
        &client,
        "link",
        json!({"source_id": a_id, "target_id": b_id, "relation": "extends"}),
    )
    .await?;

    let list = call(&client, "edge_list", json!({"source_id": a_id})).await?;
    let edges: Vec<serde_json::Value> = serde_json::from_str(&first_text(&list)).unwrap();
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0]["relation"], "extends");
    Ok(())
}

#[tokio::test]
async fn edge_update_tool_validates_relation() -> anyhow::Result<()> {
    let client = connect().await?;

    let a = call(
        &client,
        "entity_create",
        json!({"kind": "concept", "name": "A"}),
    )
    .await?;
    let b = call(
        &client,
        "entity_create",
        json!({"kind": "concept", "name": "B"}),
    )
    .await?;
    let a_id = serde_json::from_str::<serde_json::Value>(&first_text(&a)).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let b_id = serde_json::from_str::<serde_json::Value>(&first_text(&b)).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();

    let edge = call(
        &client,
        "link",
        json!({"source_id": a_id, "target_id": b_id, "relation": "extends"}),
    )
    .await?;
    let edge_data: serde_json::Value = serde_json::from_str(&first_text(&edge)).unwrap();
    let edge_id = edge_data["id"].as_str().unwrap().to_string();

    // Valid relation update succeeds.
    let ok = call(
        &client,
        "edge_update",
        json!({"id": edge_id, "relation": "depends_on"}),
    )
    .await?;
    let updated: serde_json::Value = serde_json::from_str(&first_text(&ok)).unwrap();
    assert_eq!(updated["relation"], "depends_on");

    // Invalid relation returns an error.
    let bad = client
        .call_tool(
            CallToolRequestParams::new("edge_update").with_arguments(
                json!({"id": edge_id, "relation": "related_to"})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await;
    match bad {
        Err(_) => {}
        Ok(r) => {
            assert!(
                r.is_error.unwrap_or(false),
                "unknown relation should produce an error"
            );
        }
    }
    Ok(())
}
