//! ADR-179 keyed-memory contracts over the real MCP request transport.
//!
//! RMCP runs over an in-process duplex stream with a scratch in-memory store
//! and no embedders. This does not exercise daemon forwarding, independent
//! processes, post-commit obligation failure, or Python's native socket client.

use std::ops::Deref;
use std::time::Duration;

use khive_mcp::server::KhiveMcpServer;
use khive_runtime::{KhiveRuntime, Namespace, RuntimeConfig};
use rmcp::{
    model::{CallToolRequestParams, ClientInfo},
    ClientHandler, ServiceExt,
};
use serde_json::{json, Value};

const ACTOR: &str = "test:mcp-key-writer";

#[derive(Clone, Default)]
struct MemoryClient;

impl ClientHandler for MemoryClient {
    fn get_info(&self) -> ClientInfo {
        ClientInfo::default()
    }
}

async fn connect() -> anyhow::Result<impl Deref<Target = rmcp::service::Peer<rmcp::RoleClient>>> {
    static NO_DAEMON: std::sync::Once = std::sync::Once::new();
    NO_DAEMON.call_once(|| std::env::set_var("KHIVE_NO_DAEMON", "1"));
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        default_namespace: Namespace::local(),
        actor_id: Some(ACTOR.to_owned()),
        visible_namespaces: vec![],
        packs: vec!["kg".to_owned(), "memory".to_owned()],
        ..RuntimeConfig::no_embeddings()
    })?;
    let server = KhiveMcpServer::new(runtime)?;
    let (server_transport, client_transport) = tokio::io::duplex(65536);
    tokio::spawn(async move {
        if let Ok(service) = server.serve(server_transport).await {
            let _ = service.waiting().await;
        }
    });
    Ok(MemoryClient.serve(client_transport).await?)
}

async fn request(
    client: &impl Deref<Target = rmcp::service::Peer<rmcp::RoleClient>>,
    tool: &str,
    args: Value,
) -> anyhow::Result<Value> {
    let params = CallToolRequestParams::new("request").with_arguments(
        json!({
            "ops": json!([{"tool": tool, "args": args}]).to_string(),
            "presentation": "verbose",
            "format": "json",
        })
        .as_object()
        .unwrap()
        .clone(),
    );
    let result = tokio::time::timeout(Duration::from_secs(15), client.call_tool(params)).await??;
    let text = result
        .content
        .first()
        .and_then(|content| content.raw.as_text())
        .expect("MCP request must return a JSON text envelope");
    let body: Value = serde_json::from_str(&text.text)?;
    let results = body["results"].as_array().expect("per-operation results");
    assert_eq!(results.len(), 1, "exactly one operation submitted: {body}");
    assert_eq!(
        results[0]["tool"], tool,
        "receipt identifies operation: {body}"
    );
    Ok(results[0].clone())
}

async fn ok(
    client: &impl Deref<Target = rmcp::service::Peer<rmcp::RoleClient>>,
    tool: &str,
    args: Value,
) -> anyhow::Result<Value> {
    let receipt = request(client, tool, args).await?;
    assert_eq!(receipt["ok"], true, "operation must succeed: {receipt}");
    Ok(receipt["result"].clone())
}

fn remember(key: Option<&str>, namespace: Option<&str>, source_id: Option<&str>) -> Value {
    let mut args = json!({
        "content": "MCP keyed-memory contract sentinel",
        "memory_type": "episodic",
        "salience": 0.4,
        "decay_factor": 0.0,
    });
    for (name, value) in [
        ("key", key),
        ("namespace", namespace),
        ("source_id", source_id),
    ] {
        if let Some(value) = value {
            args[name] = json!(value);
        }
    }
    args
}

fn id(result: &Value) -> &str {
    let id = result["id"].as_str().expect("successful result UUID");
    assert_eq!(uuid::Uuid::parse_str(id).unwrap().to_string(), id);
    id
}

fn assert_conflict(receipt: &Value, key: &str, holder_id: &str) {
    assert_eq!(receipt["ok"], false, "replay must refuse: {receipt}");
    assert!(
        receipt.get("result").is_none(),
        "no second success receipt: {receipt}"
    );
    let error = &receipt["error"];
    assert_eq!(error["kind"], "conflict", "{receipt}");
    assert_eq!(error["details"]["reason"], "key_conflict", "{receipt}");
    assert_eq!(error["details"]["key"], key, "{receipt}");
    assert_eq!(error["details"]["existing_id"], holder_id, "{receipt}");
    assert_eq!(error["domain_disposition"], "not_committed", "{receipt}");
    assert!(error.get("domain_result").is_none(), "{receipt}");
}

async fn memories(
    client: &impl Deref<Target = rmcp::service::Peer<rmcp::RoleClient>>,
    namespace: &str,
) -> anyhow::Result<Vec<Value>> {
    let page = ok(
        client,
        "list",
        json!({"kind": "memory", "namespace": namespace, "limit": 100}),
    )
    .await?;
    Ok(page["items"].as_array().expect("memory list items").clone())
}

async fn annotations(
    client: &impl Deref<Target = rmcp::service::Peer<rmcp::RoleClient>>,
    namespace: &str,
    source_id: &str,
) -> anyhow::Result<Vec<Value>> {
    let page = ok(
        client,
        "list",
        json!({
            "kind": "edge", "namespace": namespace,
            "target_id": source_id, "relations": ["annotates"], "limit": 100,
        }),
    )
    .await?;
    Ok(page["items"]
        .as_array()
        .expect("annotation list items")
        .clone())
}

#[tokio::test]
async fn memory_keys_mcp_replay_preserves_holder_and_single_source_edge() -> anyhow::Result<()> {
    let client = connect().await?;
    let namespace = "keys:mcp-replay";
    let source = ok(
        &client,
        "create",
        json!({
            "kind": "observation", "content": "MCP memory source", "namespace": namespace,
        }),
    )
    .await?;
    let args = remember(Some("operation-a"), Some(namespace), Some(id(&source)));
    let first = ok(&client, "memory.remember", args.clone()).await?;
    assert_eq!(first["kind"], "memory");
    assert_eq!(first["memory_type"], "episodic");
    assert_eq!(first["salience"], 0.4);
    assert_eq!(first["decay_factor"], 0.0);
    assert!(first["created_at"].is_string());
    let edge_id = first["edge_id"].as_str().expect("source edge receipt");
    assert_eq!(uuid::Uuid::parse_str(edge_id)?.to_string(), edge_id);

    let before = ok(
        &client,
        "get",
        json!({"id": id(&first), "namespace": namespace}),
    )
    .await?;
    assert_conflict(
        &request(&client, "memory.remember", args.clone()).await?,
        "operation-a",
        id(&first),
    );
    let mut changed = args;
    changed["content"] = json!("replay must not overwrite the holder");
    changed["salience"] = json!(0.9);
    assert_conflict(
        &request(&client, "memory.remember", changed).await?,
        "operation-a",
        id(&first),
    );
    assert_eq!(
        ok(
            &client,
            "get",
            json!({"id": id(&first), "namespace": namespace})
        )
        .await?,
        before
    );
    let notes = memories(&client, namespace).await?;
    assert_eq!(notes.len(), 1);
    assert_eq!(id(&notes[0]), id(&first));
    let edges = annotations(&client, namespace, id(&source)).await?;
    assert_eq!(edges.len(), 1, "replay must not duplicate annotation edges");
    assert_eq!(id(&edges[0]), edge_id);

    let other = ok(
        &client,
        "memory.remember",
        remember(Some("operation-b"), Some(namespace), Some(id(&source))),
    )
    .await?;
    assert_ne!(id(&first), id(&other));
    assert_eq!(memories(&client, namespace).await?.len(), 2);
    assert_eq!(annotations(&client, namespace, id(&source)).await?.len(), 2);
    Ok(())
}

#[tokio::test]
async fn memory_keys_mcp_empty_key_is_present_and_unkeyed_calls_remain_distinct(
) -> anyhow::Result<()> {
    let client = connect().await?;
    let namespace = "keys:mcp-empty";
    let empty = remember(Some(""), Some(namespace), None);
    let first = ok(&client, "memory.remember", empty.clone()).await?;
    assert_conflict(
        &request(&client, "memory.remember", empty).await?,
        "",
        id(&first),
    );
    let unkeyed = remember(None, Some(namespace), None);
    let one = ok(&client, "memory.remember", unkeyed.clone()).await?;
    let two = ok(&client, "memory.remember", unkeyed).await?;
    assert_ne!(id(&one), id(&two));
    assert_ne!(id(&first), id(&one));
    assert_ne!(id(&first), id(&two));
    assert!(one.get("edge_id").is_none());
    assert_eq!(memories(&client, namespace).await?.len(), 3);
    Ok(())
}

#[tokio::test]
async fn memory_keys_mcp_namespace_pin_is_exact_and_independent_of_actor_default(
) -> anyhow::Result<()> {
    let client = connect().await?;
    let key = "same-operation";
    let implicit = ok(&client, "memory.remember", remember(Some(key), None, None)).await?;
    let explicit_actor = request(
        &client,
        "memory.remember",
        remember(Some(key), Some(ACTOR), None),
    )
    .await?;
    assert_conflict(&explicit_actor, key, id(&implicit));
    let namespace = "keys:mcp-pin";
    let pinned = ok(
        &client,
        "memory.remember",
        remember(Some(key), Some(namespace), None),
    )
    .await?;
    assert_ne!(id(&implicit), id(&pinned));
    assert_conflict(
        &request(
            &client,
            "memory.remember",
            remember(Some(key), Some(namespace), None),
        )
        .await?,
        key,
        id(&pinned),
    );
    assert_eq!(memories(&client, ACTOR).await?.len(), 1);
    assert_eq!(memories(&client, namespace).await?.len(), 1);
    assert!(memories(&client, "local").await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn memory_keys_mcp_validation_counts_utf8_bytes_before_note_or_edge_writes(
) -> anyhow::Result<()> {
    let client = connect().await?;
    let namespace = "keys:mcp-validation";
    let source = ok(
        &client,
        "create",
        json!({
            "kind": "observation", "content": "validation source", "namespace": namespace,
        }),
    )
    .await?;
    for key in [
        "x".repeat(513),
        "contains\0nul".to_owned(),
        format!("{}x", "\u{00e9}".repeat(256)),
    ] {
        let invalid = request(
            &client,
            "memory.remember",
            remember(Some(&key), Some(namespace), Some(id(&source))),
        )
        .await?;
        assert_eq!(invalid["ok"], false, "invalid key must refuse: {invalid}");
        let error = invalid["error"].to_string().to_lowercase();
        assert!(
            error.contains("key") && error.contains("invalid"),
            "key validation error required: {invalid}"
        );
        assert!(memories(&client, namespace).await?.is_empty());
        assert!(annotations(&client, namespace, id(&source))
            .await?
            .is_empty());
    }
    for key in ["x".repeat(512), "\u{00e9}".repeat(256)] {
        let args = remember(Some(&key), Some(namespace), Some(id(&source)));
        let first = ok(&client, "memory.remember", args.clone()).await?;
        assert_conflict(
            &request(&client, "memory.remember", args).await?,
            &key,
            id(&first),
        );
    }
    assert_eq!(memories(&client, namespace).await?.len(), 2);
    assert_eq!(annotations(&client, namespace, id(&source)).await?.len(), 2);
    Ok(())
}
