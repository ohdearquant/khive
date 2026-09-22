//! Missing-subject taxonomy through actual pack handlers and the MCP envelope.

use std::sync::Arc;

use khive_mcp::{server::KhiveMcpServer, tools::request::RequestParams};
use khive_runtime::{KhiveRuntime, RuntimeConfig};
use khive_storage::BlobStore;
use serde_json::{json, Value};

async fn request(server: &KhiveMcpServer, tool: &str, args: Value) -> Value {
    let response = server
        .dispatch_request_local(RequestParams {
            plan: None,
            ops: json!({"tool": tool, "args": args}).to_string(),
            presentation: Some("verbose".into()),
            presentation_per_op: None,
            save_to: None,
            format: None,
            format_per_op: None,
            request_id: None,
        })
        .await
        .expect("MCP request dispatch");
    serde_json::from_str::<Value>(&response).unwrap()["results"][0].clone()
}

// Must-FAIL control: remove the runtime's typed NotFound projection arm.
// All three actual read paths then emit runtime_error at the MCP boundary.
#[tokio::test]
async fn absent_blob_entity_and_tree_share_not_found_envelope() {
    let dir = tempfile::tempdir().unwrap();
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: Some(dir.path().join("records.db")),
        ..RuntimeConfig::no_embeddings()
    })
    .unwrap();
    let store =
        Arc::new(khive_db::stores::blob::FsBlobStore::new(dir.path().join("blobs"), 0).unwrap());
    let invalid_tree = store.put(b"not a tree manifest".to_vec()).await.unwrap();
    runtime.install_blob_store(store).unwrap();
    let packs = ["kg", "blob", "tool", "exec"].map(str::to_string);
    let server = KhiveMcpServer::with_packs(runtime, &packs).unwrap();
    let missing_ref = "0".repeat(64);
    for (tool, args) in [
        ("blob.get", json!({"content_ref": missing_ref})),
        ("get", json!({"id": "00000000-0000-4000-8000-000000000000"})),
        ("exec.tree_get", json!({"tree": missing_ref})),
    ] {
        let entry = request(&server, tool, args).await;
        assert_eq!(entry["ok"], false, "{tool}: {entry}");
        let error = &entry["error"];
        assert_eq!(error["kind"], "not_found", "{tool}: {entry}");
        assert_eq!(error.get("code"), Some(&Value::Null), "{entry}");
        assert_eq!(error.get("details"), Some(&Value::Null), "{entry}");
        assert!(error["message"].is_string(), "{entry}");
    }

    // A present but invalid tree and a malformed reference are runtime failures,
    // not absent subjects. Classification must follow variants, never prose.
    for (tool, args) in [
        ("exec.tree_get", json!({"tree": invalid_tree.to_string()})),
        ("blob.get", json!({"content_ref": "malformed"})),
    ] {
        let entry = request(&server, tool, args).await;
        assert_eq!(entry["ok"], false, "{entry}");
        assert_eq!(entry["error"]["kind"], "runtime_error", "{entry}");
    }
}
