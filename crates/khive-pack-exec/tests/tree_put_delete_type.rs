use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use khive_pack_blob::BlobPack;
use khive_pack_exec::ExecPack;
use khive_pack_kg::KgPack;
use khive_pack_tool::ToolPack;
use khive_runtime::engine_config::ExecSectionConfig;
use khive_runtime::{KhiveRuntime, RuntimeConfig, RuntimeError, VerbRegistry, VerbRegistryBuilder};
use khive_storage::{BlobStore, ContentRef, StorageResult};
use serde_json::{json, Value};

#[derive(Debug)]
struct CountingBlobStore {
    inner: khive_db::stores::blob::FsBlobStore,
    puts: AtomicUsize,
}

#[async_trait::async_trait]
impl BlobStore for CountingBlobStore {
    async fn put(&self, bytes: Vec<u8>) -> StorageResult<ContentRef> {
        self.puts.fetch_add(1, Ordering::SeqCst);
        self.inner.put(bytes).await
    }

    async fn get_bounded_verified(
        &self,
        reference: &ContentRef,
        max_bytes: u64,
    ) -> StorageResult<Vec<u8>> {
        self.inner.get_bounded_verified(reference, max_bytes).await
    }

    async fn exists(&self, reference: &ContentRef) -> StorageResult<bool> {
        self.inner.exists(reference).await
    }

    async fn size(&self, reference: &ContentRef) -> StorageResult<Option<u64>> {
        self.inner.size(reference).await
    }

    async fn delete(&self, reference: &ContentRef) -> StorageResult<bool> {
        self.inner.delete(reference).await
    }
}

struct Fixture {
    registry: VerbRegistry,
    store: Arc<CountingBlobStore>,
    blobs: PathBuf,
    _dir: tempfile::TempDir,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().expect("private fixture directory");
    let mut config = RuntimeConfig::no_embeddings();
    config.db_path = None;
    config.wal_ceiling_bytes = 0;
    config.wal_ceiling_configured_bytes = 0;
    config.wal_ceiling_source = khive_runtime::WalCeilingSource::Default;
    config.wal_ceiling_env_raw = None;
    config.disk_guard_environment = Default::default();
    config.disk_guard_config = None;
    config.volume_lock_dir = None;
    config.credentials.clear();
    config.visibility_receipts = None;
    config.mounts.clear();
    config.events_split = None;
    config.actor_id = Some("fixture-tree-delete-type".into());
    config.default_namespace = khive_runtime::Namespace::local();
    config.visible_namespaces.clear();
    config.allowed_outbound_namespaces.clear();
    config.brain_profile = None;
    config.brain = Default::default();
    config.packs = vec!["kg".into(), "blob".into(), "tool".into(), "exec".into()];
    config.exec = ExecSectionConfig {
        root: Some(dir.path().join("exec").to_string_lossy().into_owned()),
        ..ExecSectionConfig::default()
    };
    assert!(config.db_path.is_none());
    assert!(config.embedding_model.is_none());
    assert!(config.additional_embedding_models.is_empty());
    let runtime = KhiveRuntime::new(config).expect("memory runtime");
    assert!(!runtime.backend().is_file_backed());
    assert!(runtime.backend_data_dir().is_none());
    assert!(runtime.backend_ann_root().is_none());
    let blobs = dir.path().join("blobs");
    let store = Arc::new(CountingBlobStore {
        inner: khive_db::stores::blob::FsBlobStore::new(blobs.clone(), 0).unwrap(),
        puts: AtomicUsize::new(0),
    });
    runtime.install_blob_store(store.clone()).unwrap();
    let mut builder = VerbRegistryBuilder::new();
    builder.with_actor_id(Some("fixture-tree-delete-type".into()));
    builder.register(KgPack::new(runtime.clone()));
    builder.register(BlobPack::new(runtime.clone()));
    builder.register(ToolPack::new(runtime.clone()));
    builder.register(ExecPack::new(runtime.clone()));
    let registry = builder.build().expect("real exec registry");
    registry.apply_schema_plans(runtime.backend());
    runtime.install_edge_rules(registry.all_edge_rules());
    Fixture {
        registry,
        store,
        blobs,
        _dir: dir,
    }
}

fn object_count(path: &Path) -> usize {
    std::fs::read_dir(path)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                object_count(&entry.path())
            } else {
                1
            }
        })
        .sum()
}

async fn entries(f: &Fixture, tree: &str) -> Value {
    f.registry
        .dispatch("exec.tree_get", json!({"tree": tree}))
        .await
        .unwrap()["entries"]
        .clone()
}

async fn body(f: &Fixture, entries: &Value, path: &str) -> Vec<u8> {
    let entry = entries
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["path"] == path)
        .unwrap();
    let reference = ContentRef::from_hex(entry["ref"].as_str().unwrap()).unwrap();
    f.store
        .get_bounded_verified(&reference, 1024)
        .await
        .unwrap()
}

#[tokio::test]
async fn tree_put_rejects_non_boolean_delete_without_publishing_any_sibling() {
    let f = fixture();
    let original = f.store.put(b"original a".to_vec()).await.unwrap();
    let keep = f.store.put(b"original keep".to_vec()).await.unwrap();
    let replacement = f.store.put(b"existing replacement".to_vec()).await.unwrap();
    let base = f
        .registry
        .dispatch(
            "exec.tree",
            json!({"entries": [
                {"path": "a", "ref": original.as_str(), "mode": 755},
                {"path": "keep", "ref": keep.as_str(), "mode": 644},
            ]}),
        )
        .await
        .unwrap()["tree"]
        .as_str()
        .unwrap()
        .to_owned();
    let before_entries = entries(&f, &base).await;
    let before_puts = f.store.puts.load(Ordering::SeqCst);
    let before_objects = object_count(&f.blobs);
    assert!(before_puts > 0 && before_objects > 0);

    for (index, malformed) in [
        json!("true"),
        json!("false"),
        json!(0),
        json!(1),
        json!(1.5),
        json!([]),
        json!([true]),
        json!({}),
        json!({"enabled": true}),
    ]
    .into_iter()
    .enumerate()
    {
        for action in ["content", "ref"] {
            let value = if action == "content" {
                format!("bad edit content {index}")
            } else {
                replacement.to_string()
            };
            let mut bad = json!({"path": "a", "delete": malformed});
            bad[action] = json!(value);
            let result = f.registry.dispatch("exec.tree_put", json!({"tree": base, "edits": [
                {"path": format!("pending-{index}-{action}"), "content": format!("new pending body {index} {action}")},
                bad,
            ]})).await;
            let error = match result {
                Err(RuntimeError::InvalidInput(error)) => error,
                other => {
                    panic!("expected indexed invalid input for {malformed}/{action}, got {other:?}")
                }
            };
            assert_eq!(error, "edits[1].delete must be a boolean");
            assert_eq!(f.store.puts.load(Ordering::SeqCst), before_puts);
            assert_eq!(object_count(&f.blobs), before_objects);
            assert_eq!(entries(&f, &base).await, before_entries);
            assert_eq!(body(&f, &before_entries, "a").await, b"original a");
            assert_eq!(body(&f, &before_entries, "keep").await, b"original keep");
        }
    }

    for (index, flag) in [None, Some(Value::Null), Some(json!(false))]
        .into_iter()
        .enumerate()
    {
        for action in ["content", "ref"] {
            let expected = if action == "content" {
                format!("accepted content {index}")
            } else {
                "existing replacement".to_owned()
            };
            let mut edit = json!({"path": "a"});
            edit[action] = if action == "content" {
                json!(expected)
            } else {
                json!(replacement.as_str())
            };
            if let Some(flag) = &flag {
                edit["delete"] = flag.clone();
            }
            let count_before = object_count(&f.blobs);
            let puts_before = f.store.puts.load(Ordering::SeqCst);
            let result = f.registry.dispatch("exec.tree_put", json!({"tree": base, "edits": [
                {"path": format!("good-{index}-{action}"), "content": format!("accepted good sibling {index} {action}")},
                edit,
            ]})).await.unwrap();
            let next = entries(&f, result["tree"].as_str().unwrap()).await;
            assert_eq!(next.as_array().unwrap().len(), 3);
            assert_eq!(body(&f, &next, "a").await, expected.as_bytes());
            assert_eq!(body(&f, &next, "keep").await, b"original keep");
            assert_eq!(next[0]["mode"], 755);
            assert!(f.store.puts.load(Ordering::SeqCst) > puts_before);
            assert!(object_count(&f.blobs) > count_before);
            assert_eq!(entries(&f, &base).await, before_entries);
        }
    }

    let deleted = f
        .registry
        .dispatch(
            "exec.tree_put",
            json!({"tree": base, "edits": [{"path": "a", "delete": true}]}),
        )
        .await
        .unwrap();
    let next = entries(&f, deleted["tree"].as_str().unwrap()).await;
    assert_eq!(
        next,
        json!([{"path": "keep", "ref": keep.as_str(), "mode": 644}])
    );
    assert_eq!(deleted["changed"][0]["op"], "deleted");
    assert_eq!(entries(&f, &base).await, before_entries);

    for (edit, named) in [
        (
            json!({"path": "a", "delete": true, "content": "conflict"}),
            2,
        ),
        (
            json!({"path": "a", "delete": true, "ref": replacement.as_str()}),
            2,
        ),
        (json!({"path": "a", "delete": false}), 0),
        (json!({"path": "a", "delete": null}), 0),
    ] {
        let puts_before = f.store.puts.load(Ordering::SeqCst);
        let count_before = object_count(&f.blobs);
        let result = f
            .registry
            .dispatch("exec.tree_put", json!({"tree": base, "edits": [edit]}))
            .await;
        let error = match result {
            Err(RuntimeError::InvalidInput(error)) => error,
            other => panic!("expected action-count refusal, got {other:?}"),
        };
        assert_eq!(
            error,
            format!("edits[0] names {named} of ref, content and delete; exactly one is required")
        );
        assert_eq!(f.store.puts.load(Ordering::SeqCst), puts_before);
        assert_eq!(object_count(&f.blobs), count_before);
    }
}
