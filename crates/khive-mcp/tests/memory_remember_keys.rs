//! ADR-179 keyed-memory contracts over the real MCP request transport.
//!
//! RMCP runs over an in-process duplex stream with a scratch in-memory store
//! and no embedders. This does not exercise daemon forwarding, independent
//! processes, post-commit obligation failure, or Python's native socket client.

#[path = "../../khive-runtime/tests/support/receipt_credentials.rs"]
mod receipt_credentials;

use std::ops::Deref;
use std::time::Duration;

use khive_mcp::server::KhiveMcpServer;
use khive_runtime::{KhiveRuntime, Namespace, RuntimeConfig};
use khive_storage::{SqlStatement, SqlValue};
use rmcp::{
    model::{CallToolRequestParams, ClientInfo},
    ClientHandler, ServiceExt,
};
use serde_json::{json, Value};

const ACTOR: &str = "test:mcp-key-writer";

#[test]
fn receipt_custody_changes_daemon_identity_without_exposing_references() {
    use khive_mcp::server::compute_config_id;
    use khive_runtime::credentials::{
        CredentialConfig, CredentialKind, VisibilityReceiptConfig, VisibilityReceiptKeyConfig,
    };
    use khive_runtime::daemon::{config_ids_compatible, first_config_mismatch_field};

    let absent = RuntimeConfig::no_embeddings();
    let absent_id = compute_config_id(&absent, None);
    assert!(absent_id.contains(";visibility_receipts="));
    let legacy_id = absent_id.split_once(";visibility_receipts=").unwrap().0;
    assert!(!config_ids_compatible(&absent_id, legacy_id));
    assert!(!config_ids_compatible(legacy_id, &absent_id));
    assert_eq!(
        first_config_mismatch_field(&absent_id, Some(legacy_id)),
        "visibility_receipts"
    );
    let mut configured = absent.clone();
    configured.credentials = ["current", "retired"]
        .map(|suffix| CredentialConfig {
            name: format!("private-reference-{suffix}"),
            kind: CredentialKind::SigningKey,
            provider: "env".to_owned(),
            env_var: Some(format!("RECEIPT_CUSTODY_{suffix}")),
            header: None,
        })
        .to_vec();
    configured.visibility_receipts = Some(VisibilityReceiptConfig {
        keys: ["current", "retired"]
            .map(|suffix| VisibilityReceiptKeyConfig {
                id: format!("private-key-id-{suffix}"),
                credential: format!("private-reference-{suffix}"),
                encrypt: suffix == "current",
            })
            .to_vec(),
    });
    let configured_id = compute_config_id(&configured, None);
    assert_ne!(absent_id, configured_id);
    for private_reference in ["private-reference", "private-key-id", "RECEIPT_CUSTODY"] {
        assert!(!configured_id.contains(private_reference));
    }

    let mut reordered = configured.clone();
    reordered.credentials.reverse();
    reordered
        .visibility_receipts
        .as_mut()
        .unwrap()
        .keys
        .reverse();
    assert_eq!(configured_id, compute_config_id(&reordered, None));

    for mutation in 0..6 {
        let mut changed = configured.clone();
        match mutation {
            0 => changed.visibility_receipts = None,
            1 => changed.visibility_receipts.as_mut().unwrap().keys[0]
                .id
                .push('2'),
            2 => {
                let keys = &mut changed.visibility_receipts.as_mut().unwrap().keys;
                keys[0].encrypt = false;
                keys[1].encrypt = true;
            }
            3 => {
                changed.visibility_receipts.as_mut().unwrap().keys[0].credential =
                    changed.credentials[1].name.clone();
            }
            4 => changed.credentials[0].env_var = Some("OTHER_CUSTODY_LOCATION".to_owned()),
            5 => {
                changed.credentials[0].provider = "external-vault".to_owned();
                changed.credentials[0].env_var = None;
            }
            _ => unreachable!(),
        }
        let changed_id = compute_config_id(&changed, None);
        assert_ne!(configured_id, changed_id, "case {mutation}");
        assert!(!config_ids_compatible(&configured_id, &changed_id));
        assert!(!config_ids_compatible(&changed_id, &configured_id));
        assert_eq!(
            first_config_mismatch_field(&configured_id, Some(&changed_id)),
            "visibility_receipts"
        );
    }
}

#[derive(Clone, Default)]
struct MemoryClient;

impl ClientHandler for MemoryClient {
    fn get_info(&self) -> ClientInfo {
        ClientInfo::default()
    }
}

fn memory_runtime() -> anyhow::Result<KhiveRuntime> {
    Ok(KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        default_namespace: Namespace::local(),
        actor_id: Some(ACTOR.to_owned()),
        visible_namespaces: vec![],
        packs: vec!["kg".to_owned(), "memory".to_owned()],
        ..RuntimeConfig::no_embeddings()
    })?)
}

async fn connect() -> anyhow::Result<impl Deref<Target = rmcp::service::Peer<rmcp::RoleClient>>> {
    connect_runtime(receipt_credentials::with_receipt_credentials(
        memory_runtime()?,
    ))
    .await
}

async fn connect_runtime(
    runtime: KhiveRuntime,
) -> anyhow::Result<impl Deref<Target = rmcp::service::Peer<rmcp::RoleClient>>> {
    static NO_DAEMON: std::sync::Once = std::sync::Once::new();
    NO_DAEMON.call_once(|| std::env::set_var("KHIVE_NO_DAEMON", "1"));
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
    assert_eq!(
        error["details"]["reason"], "idempotency_key_conflict",
        "{receipt}"
    );
    assert_eq!(error["details"]["key"], key, "{receipt}");
    assert_eq!(error["details"]["existing_id"], holder_id, "{receipt}");
    assert_eq!(
        error["domain_disposition"], "not_committed",
        "the keyed conflict is refused before this request writes: {receipt}"
    );
    assert!(error.get("domain_result").is_none(), "{receipt}");
}

fn assert_replay(receipt: &Value, holder_id: &str) {
    assert_eq!(receipt["ok"], true, "replay must succeed: {receipt}");
    assert_eq!(receipt["result"]["replayed"], true, "{receipt}");
    assert_eq!(
        id(&receipt["result"]),
        holder_id,
        "replay must return holder"
    );
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
    assert_replay(
        &request(&client, "memory.remember", args.clone()).await?,
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
    assert_replay(
        &request(&client, "memory.remember", empty).await?,
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
    assert_replay(&explicit_actor, id(&implicit));
    let namespace = "keys:mcp-pin";
    let pinned = ok(
        &client,
        "memory.remember",
        remember(Some(key), Some(namespace), None),
    )
    .await?;
    assert_ne!(id(&implicit), id(&pinned));
    assert_replay(
        &request(
            &client,
            "memory.remember",
            remember(Some(key), Some(namespace), None),
        )
        .await?,
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
        assert_replay(
            &request(&client, "memory.remember", args).await?,
            id(&first),
        );
    }
    assert_eq!(memories(&client, namespace).await?.len(), 2);
    assert_eq!(annotations(&client, namespace, id(&source)).await?.len(), 2);
    Ok(())
}

/// Snapshot the actual domain rows, including any dynamically created vector
/// tables, rather than treating equal row counts as proof of a no-write replay.
async fn visibility_domain_rows(runtime: &KhiveRuntime) -> anyhow::Result<Value> {
    let mut reader = runtime.sql().reader().await?;
    let tables = reader
        .query_all(SqlStatement {
            sql: "SELECT name FROM sqlite_master WHERE type = 'table' AND \
                  (name IN ('notes', 'graph_edges', 'ann_write_log', \
                   'memory_visibility_receipts', 'memory_visibility_fences', \
                   'memory_visibility_epochs', 'vector_provenance') \
                   OR name GLOB 'vec_*') ORDER BY name"
                .into(),
            params: vec![],
            label: Some("receipt-replay-domain-tables".into()),
        })
        .await?;
    let mut snapshot = serde_json::Map::new();
    for table in tables {
        let name = table.text("name")?;
        let rows = reader
            .query_all(SqlStatement {
                sql: format!("SELECT * FROM \"{}\"", name.replace('"', "\"\"")),
                params: vec![],
                label: Some("receipt-replay-domain-rows".into()),
            })
            .await?;
        let mut canonical = rows
            .iter()
            .map(serde_json::to_string)
            .collect::<Result<Vec<_>, _>>()?;
        canonical.sort();
        snapshot.insert(name.to_owned(), json!(canonical));
    }
    for required in [
        "notes",
        "graph_edges",
        "ann_write_log",
        "memory_visibility_receipts",
        "memory_visibility_fences",
        "memory_visibility_epochs",
    ] {
        assert!(
            snapshot.contains_key(required),
            "missing domain table {required}"
        );
    }
    Ok(Value::Object(snapshot))
}

#[tokio::test]
async fn classified_receipt_replays_reconcile_through_the_request_envelope() -> anyhow::Result<()> {
    for (epoch, expected_reason, retryable) in [
        ("legacy", "legacy_receipt_absent", false),
        ("unknown", "receipt_epoch_unknown", false),
        ("missing", "receipt_epoch_unknown", false),
        ("modern", "receipt_temporarily_unavailable", true),
    ] {
        let runtime = receipt_credentials::with_receipt_credentials(memory_runtime()?);
        let client = connect_runtime(runtime.clone()).await?;
        let source = ok(
            &client,
            "create",
            json!({"kind": "concept", "name": "receipt replay source"}),
        )
        .await?;
        let args = remember(Some("receipt-replay"), None, Some(id(&source)));
        let first = ok(&client, "memory.remember", args.clone()).await?;
        assert!(
            first["visibility_token"].is_string(),
            "opaque receipt: {first}"
        );
        let memory_id = id(&first).to_owned();

        // Seed the post-upgrade provenance states. The DB migration tests prove
        // how real historical stores reach these states; this test proves their
        // public transport disposition and exact no-write replay behavior.
        let mut writer = runtime.sql().writer().await?;
        if matches!(epoch, "legacy" | "modern") {
            writer
                .execute(SqlStatement {
                    sql: "DELETE FROM memory_visibility_receipts WHERE note_id = ?1".into(),
                    params: vec![SqlValue::Text(memory_id.clone())],
                    label: Some("receipt-replay-remove-header".into()),
                })
                .await?;
        }
        let statement = if epoch == "missing" {
            SqlStatement {
                sql: "DELETE FROM memory_visibility_epochs WHERE note_id = ?1".into(),
                params: vec![SqlValue::Text(memory_id.clone())],
                label: Some("receipt-replay-remove-provenance".into()),
            }
        } else {
            SqlStatement {
                sql: "UPDATE memory_visibility_epochs SET epoch = ?1 WHERE note_id = ?2".into(),
                params: vec![
                    SqlValue::Text(epoch.into()),
                    SqlValue::Text(memory_id.clone()),
                ],
                label: Some("receipt-replay-seed-provenance".into()),
            }
        };
        writer.execute(statement).await?;
        drop(writer);
        let before = visibility_domain_rows(&runtime).await?;
        assert_eq!(before["notes"].as_array().unwrap().len(), 1);
        assert_eq!(before["graph_edges"].as_array().unwrap().len(), 1);

        let mut previous = None;
        for _ in 0..2 {
            let receipt = request(&client, "memory.remember", args.clone()).await?;
            assert_eq!(receipt["ok"], false, "{receipt}");
            assert!(receipt.get("result").is_none(), "{receipt}");
            let error = &receipt["error"];
            assert_eq!(error["details"]["reason"], expected_reason, "{receipt}");
            assert_eq!(error["details"]["memory_id"], memory_id, "{receipt}");
            assert_eq!(
                error["retryable"], retryable,
                "boolean retryability: {receipt}"
            );
            assert_eq!(error["domain_disposition"], "not_committed", "{receipt}");
            for hidden in [
                "visibility_token",
                "fences",
                "ann_write_log_seq",
                "issued_at",
                "unknown_by_namespace",
            ] {
                assert!(
                    error.get(hidden).is_none(),
                    "private receipt field {hidden}: {receipt}"
                );
                assert!(
                    error["details"].get(hidden).is_none(),
                    "private detail {hidden}: {receipt}"
                );
            }
            if let Some(previous) = previous.as_ref() {
                assert_eq!(
                    error, previous,
                    "repeated replay must reconcile identically"
                );
            }
            previous = Some(error.clone());
            assert_eq!(
                visibility_domain_rows(&runtime).await?,
                before,
                "{epoch}: exact replay changed domain rows"
            );
        }
    }
    Ok(())
}

#[tokio::test]
async fn absent_receipt_key_refuses_before_a_memory_write() -> anyhow::Result<()> {
    let runtime = memory_runtime()?;
    let client = connect_runtime(runtime.clone()).await?;
    let before = visibility_domain_rows(&runtime).await?;
    let receipt = request(
        &client,
        "memory.remember",
        remember(Some("missing-key"), None, None),
    )
    .await?;
    assert_eq!(receipt["ok"], false, "{receipt}");
    assert_eq!(
        receipt["error"]["details"]["reason"], "visibility_key_unavailable",
        "{receipt}"
    );
    assert_eq!(receipt["error"]["retryable"], true, "{receipt}");
    assert!(receipt.get("result").is_none());
    assert_eq!(visibility_domain_rows(&runtime).await?, before);
    Ok(())
}
