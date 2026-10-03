//! Receipt evidence through the public exec verbs and durable SQL storage.

use khive_pack_blob::BlobPack;
use khive_pack_exec::ExecPack;
use khive_pack_kg::KgPack;
use khive_pack_tool::ToolPack;
use khive_runtime::engine_config::ExecSectionConfig;
use khive_runtime::{
    runtime_error_value, DomainDisposition, KhiveRuntime, RuntimeConfig, RuntimeError,
    VerbRegistry, VerbRegistryBuilder,
};
use khive_storage::{
    BlobStore, ContentRef, SqlStatement, SqlValue, StorageCapability, StorageError, StorageResult,
};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[derive(Debug)]
struct ProfileFaultBlobStore {
    inner: Arc<dyn BlobStore>,
    fail_next_put: Arc<AtomicBool>,
}

#[async_trait::async_trait]
impl BlobStore for ProfileFaultBlobStore {
    async fn put(&self, bytes: Vec<u8>) -> StorageResult<ContentRef> {
        if self.fail_next_put.swap(false, Ordering::SeqCst) {
            return Err(StorageError::InvalidInput {
                capability: StorageCapability::Blob,
                operation: "put".into(),
                message: "injected cleanup receipt profile write failure".into(),
            });
        }
        self.inner.put(bytes).await
    }

    async fn get_bounded_verified(
        &self,
        content_ref: &ContentRef,
        max_bytes: u64,
    ) -> StorageResult<Vec<u8>> {
        self.inner
            .get_bounded_verified(content_ref, max_bytes)
            .await
    }

    async fn exists(&self, content_ref: &ContentRef) -> StorageResult<bool> {
        self.inner.exists(content_ref).await
    }

    async fn size(&self, content_ref: &ContentRef) -> StorageResult<Option<u64>> {
        self.inner.size(content_ref).await
    }

    async fn delete(&self, content_ref: &ContentRef) -> StorageResult<bool> {
        self.inner.delete(content_ref).await
    }
}

struct Fixture {
    registry: VerbRegistry,
    runtime: KhiveRuntime,
    namespace: String,
    _dir: tempfile::TempDir,
}

impl Fixture {
    fn new(cap: u64, fail_next_put: Option<Arc<AtomicBool>>) -> Self {
        let dir = tempfile::tempdir().expect("isolated receipt fixture");
        let config = RuntimeConfig {
            db_path: Some(dir.path().join("receipts.db")),
            exec: ExecSectionConfig {
                root: Some(dir.path().join("exec-root").to_string_lossy().into_owned()),
                read_roots: vec!["/bin".into(), "/usr/bin".into()],
                max_output_bytes: Some(cap),
                timeout_default_s: Some(5.0),
                timeout_max_s: Some(10.0),
                ..Default::default()
            },
            ..RuntimeConfig::no_embeddings()
        };
        let namespace = config.default_namespace.as_str().to_string();
        let runtime = KhiveRuntime::new(config).expect("file-backed runtime");
        let inner: Arc<dyn BlobStore> = Arc::new(
            khive_db::stores::blob::FsBlobStore::new(dir.path().join("blobs"), 0)
                .expect("real blob store"),
        );
        let store: Arc<dyn BlobStore> = match fail_next_put {
            Some(fail_next_put) => Arc::new(ProfileFaultBlobStore {
                inner,
                fail_next_put,
            }),
            None => inner,
        };
        runtime
            .install_blob_store(store)
            .expect("blob store installed");
        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(runtime.clone()));
        builder.register(BlobPack::new(runtime.clone()));
        builder.register(ToolPack::new(runtime.clone()));
        builder.register(ExecPack::new(runtime.clone()));
        let registry = builder.build().expect("real verb registry");
        registry.apply_schema_plans(runtime.backend());
        runtime.install_edge_rules(registry.all_edge_rules());
        Self {
            registry,
            runtime,
            namespace,
            _dir: dir,
        }
    }

    async fn call(&self, verb: &str, params: Value) -> Value {
        self.registry
            .dispatch(verb, params)
            .await
            .unwrap_or_else(|error| panic!("{verb} failed: {error}"))
    }

    async fn empty_tree(&self) -> String {
        self.call("exec.tree", json!({"entries": []})).await["tree"]
            .as_str()
            .expect("stored empty tree")
            .to_string()
    }

    #[cfg(target_os = "macos")]
    async fn register_shell(&self) {
        self.call(
            "tool.register",
            json!({
                "name": "cleanup-sh", "kind": "tool", "description": "shell",
                "source": "exec:/bin/sh", "side_effect": "write", "trust": "first_party"
            }),
        )
        .await;
        self.call(
            "tool.policy",
            json!({"actor": "*", "tool": "cleanup-sh", "decision": "allow"}),
        )
        .await;
    }

    #[cfg(target_os = "macos")]
    async fn blob_bytes(&self, content_ref: &Value) -> Vec<u8> {
        use base64::Engine;
        let blob = self
            .call("blob.get", json!({"content_ref": content_ref}))
            .await;
        base64::engine::general_purpose::STANDARD
            .decode(blob["bytes"].as_str().expect("blob payload"))
            .expect("base64 blob bytes")
    }

    async fn assert_durable(&self, receipt: &Value) {
        let stored = self
            .call("exec.receipt", json!({"id": receipt["id"]}))
            .await;
        assert_eq!(&stored, receipt, "lookup preserves the whole receipt");
        let page = self
            .call("exec.runs", json!({"actor": receipt["actor"]}))
            .await;
        let listed = page["runs"]
            .as_array()
            .expect("run list")
            .iter()
            .find(|item| item["id"] == receipt["id"])
            .expect("durable run is listed");
        assert_eq!(listed, receipt, "listing preserves the whole receipt");
    }
}

fn assert_current_cleanup(receipt: &Value) {
    assert_eq!(
        receipt.get("process_cleanup"),
        Some(&json!({
            "scope": "initial_group",
            "observation": "not_attempted",
            "seen_alive": false,
            "certification": "unverified",
            "detail": "Detached descendant termination is not certified on this backend."
        })),
        "current receipt producers describe their cleanup scope without certification: {receipt}"
    );
    assert_eq!(receipt.get("tree_quiescence"), Some(&json!("unverified")));
}

#[tokio::test]
async fn process_cleanup_refusal_is_durable_and_projected() {
    let fixture = Fixture::new(17, None);
    let tree = fixture.empty_tree().await;
    let error = fixture
        .registry
        .dispatch(
            "exec.run",
            json!({"tree": tree, "tool": "unregistered-cleanup-tool", "actor": "local",
                "session_id": "cleanup-refusal"}),
        )
        .await
        .expect_err("unregistered tool is refused with a durable receipt");
    let id = match &error {
        RuntimeError::RefusedWithReceipt(refusal) => refusal.receipt_id.clone(),
        other => panic!("expected receipt-bearing refusal, got {other}"),
    };
    let projected = runtime_error_value(error, DomainDisposition::Unknown);
    let receipt = fixture.call("exec.receipt", json!({"id": id})).await;
    assert_eq!(receipt["denied"], true);
    assert_eq!(receipt["success"], false);
    assert_eq!(receipt["seq"], 1);
    assert!(receipt["started_at"].is_null());
    assert_current_cleanup(&receipt);
    assert_eq!(projected["receipt_id"], receipt["id"]);
    assert_eq!(
        projected.get("process_cleanup"),
        receipt.get("process_cleanup")
    );
    assert_eq!(
        projected.get("tree_quiescence"),
        receipt.get("tree_quiescence")
    );
    fixture.assert_durable(&receipt).await;
    let page = fixture.call("exec.runs", json!({"actor": "local"})).await;
    assert_eq!(page["count"], 1);
}

#[tokio::test]
async fn historical_exec_receipts_remain_uncertified_without_rewriting() {
    let fixture = Fixture::new(17, None);
    let historical = json!({
        "id": "historical-cleanup-receipt", "actor": "local", "tool": "historical-tool",
        "session_id": null, "seq": null, "success": true, "timed_out": false,
        "exit_code": 0, "sandbox": {"backend": "seatbelt"},
        "stdout_produced_bytes": 3, "stdout_retained_bytes": 3, "stdout_capture": "complete"
    });
    let raw = historical.to_string();
    let sql = fixture.runtime.sql();
    let mut writer = sql.writer().await.expect("public SQL writer");
    let inserted = writer
        .execute(SqlStatement {
            sql: "INSERT INTO exec_runs \
                  (id, namespace, actor, tool, session_id, seq, receipt, created_at) \
                  VALUES (?1, ?2, ?3, ?4, NULL, NULL, ?5, ?6)"
                .into(),
            params: vec![
                SqlValue::Text("historical-cleanup-receipt".into()),
                SqlValue::Text(fixture.namespace.clone()),
                SqlValue::Text("local".into()),
                SqlValue::Text("historical-tool".into()),
                SqlValue::Text(raw.clone()),
                SqlValue::Integer(1),
            ],
            label: Some("historical_exec_receipt_fixture".into()),
        })
        .await
        .expect("insert a historical row without new receipt fields");
    assert_eq!(inserted, 1);
    drop(writer);
    let stored = fixture
        .call("exec.receipt", json!({"id": historical["id"]}))
        .await;
    assert_eq!(stored, historical);
    assert!(stored.get("process_cleanup").is_none());
    assert!(stored.get("tree_quiescence").is_none());
    fixture.assert_durable(&historical).await;
    let mut reader = sql.reader().await.expect("public SQL reader");
    let raw_after = reader
        .query_scalar(SqlStatement {
            sql: "SELECT receipt FROM exec_runs WHERE id = ?1 AND namespace = ?2".into(),
            params: vec![
                SqlValue::Text("historical-cleanup-receipt".into()),
                SqlValue::Text(fixture.namespace.clone()),
            ],
            label: Some("historical_exec_receipt_bytes".into()),
        })
        .await
        .expect("read the stored JSON after lookup and listing");
    match raw_after {
        Some(SqlValue::Text(actual)) => {
            assert_eq!(actual, raw, "reads do not rewrite old receipts")
        }
        other => panic!("expected unchanged TEXT receipt, got {other:?}"),
    }
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn process_cleanup_success_keeps_artifacts_and_matches_storage() {
    for cap in [0, 128] {
        let fixture = Fixture::new(cap, None);
        fixture.register_shell().await;
        let tree = fixture.empty_tree().await;
        let output = fixture
            .call(
                "exec.run",
                json!({"tree": tree, "tool": "cleanup-sh", "actor": "local",
                    "session_id": "cleanup-success", "args": ["-c", "printf out; printf saved > output.txt"]}),
            )
            .await;
        let receipt = &output["receipt"];
        assert_eq!(receipt["exit_code"], 0, "{receipt}");
        assert_eq!(receipt["success"], true, "{receipt}");
        assert_eq!(receipt["tree_capture"], "complete");
        assert!(receipt["tree_out"].is_string());
        assert_eq!(receipt["changed"].as_array().expect("changes").len(), 1);
        assert_eq!(receipt["changed"][0]["path"], "output.txt");
        assert_eq!(output["changed"], receipt["changed"]);
        assert_eq!(receipt["stdout_produced_bytes"], 3);
        assert_eq!(receipt["stdout_retained_bytes"], cap.min(3));
        assert_eq!(
            receipt["stdout_capture"],
            if cap == 0 { "incomplete" } else { "complete" }
        );
        assert_current_cleanup(receipt);
        fixture.assert_durable(receipt).await;
        let tree_out = fixture
            .call("exec.tree_get", json!({"tree": receipt["tree_out"]}))
            .await;
        let file = tree_out["entries"]
            .as_array()
            .expect("captured tree entries")
            .iter()
            .find(|entry| entry["path"] == "output.txt")
            .expect("published file");
        assert_eq!(fixture.blob_bytes(&file["ref"]).await, b"saved".to_vec());
        assert_eq!(
            fixture.blob_bytes(&receipt["stdout_ref"]).await,
            if cap == 0 {
                Vec::new()
            } else {
                b"out".to_vec()
            }
        );
    }
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn process_cleanup_nonzero_exit_is_durable() {
    let fixture = Fixture::new(128, None);
    fixture.register_shell().await;
    let tree = fixture.empty_tree().await;
    let output = fixture
        .call(
            "exec.run",
            json!({"tree": tree, "tool": "cleanup-sh", "actor": "local",
                "args": ["-c", "printf failed > output.txt; exit 7"]}),
        )
        .await;
    let receipt = &output["receipt"];
    assert_eq!(receipt["exit_code"], 7, "{receipt}");
    assert_eq!(receipt["success"], false, "{receipt}");
    assert!(receipt["tree_out"].is_string());
    assert_eq!(receipt["changed"].as_array().expect("changes").len(), 1);
    assert_current_cleanup(receipt);
    fixture.assert_durable(receipt).await;
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn process_cleanup_profile_storage_failure_is_durable() {
    let fail_next_put = Arc::new(AtomicBool::new(false));
    let fixture = Fixture::new(128, Some(Arc::clone(&fail_next_put)));
    fixture.register_shell().await;
    let tree = fixture.empty_tree().await;
    fail_next_put.store(true, Ordering::SeqCst);
    let output = fixture
        .call(
            "exec.run",
            json!({"tree": tree, "tool": "cleanup-sh", "actor": "local",
                "args": ["-c", "printf must-not-launch"]}),
        )
        .await;
    let receipt = &output["receipt"];
    assert_eq!(receipt["success"], false, "{receipt}");
    assert_eq!(receipt["denied"], false);
    assert!(receipt["reason"]
        .as_str()
        .expect("failure reason")
        .contains("injected cleanup receipt profile write failure"));
    assert!(receipt["started_at"].is_null());
    assert!(receipt["finished_at"].is_string());
    assert!(receipt["tree_out"].is_null());
    assert!(receipt["changed"].as_array().expect("changes").is_empty());
    assert_current_cleanup(receipt);
    fixture.assert_durable(receipt).await;
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn process_cleanup_timeout_is_durable() {
    let fixture = Fixture::new(128, None);
    fixture.register_shell().await;
    let tree = fixture.empty_tree().await;
    let output = fixture
        .call(
            "exec.run",
            json!({"tree": tree, "tool": "cleanup-sh", "actor": "local", "timeout_s": 0.2,
                "args": ["-c", "printf timeout > output.txt; exec /bin/sleep 5"]}),
        )
        .await;
    let receipt = &output["receipt"];
    assert_eq!(receipt["timed_out"], true, "{receipt}");
    assert_eq!(receipt["success"], false, "{receipt}");
    assert!(receipt["exit_code"].is_null());
    assert!(receipt["started_at"].is_string());
    assert_current_cleanup(receipt);
    fixture.assert_durable(receipt).await;
}
