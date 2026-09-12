use super::*;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::SystemTime;

use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use khive_db::stores::blob::FsBlobStore;
use khive_runtime::{RequestIdentity, RuntimeConfig, VerbRegistryBuilder};
use khive_storage::{StorageError, StorageResult};

use crate::handlers::{handle_put_part, REQUEST_RESERVE};
use crate::BlobPack;

const IDLE: Duration = Duration::from_secs(3600);

#[test]
fn upload_handler_metadata_preserves_declaration_and_commissive_categories() {
    for (name, expected) in [
        ("blob.begin", khive_types::VerbCategory::Declaration),
        ("blob.put_part", khive_types::VerbCategory::Declaration),
        ("blob.commit", khive_types::VerbCategory::Declaration),
        ("blob.abort", khive_types::VerbCategory::Declaration),
        ("blob.put", khive_types::VerbCategory::Commissive),
    ] {
        let handler = crate::BLOB_HANDLERS
            .iter()
            .find(|handler| handler.name == name)
            .unwrap_or_else(|| panic!("missing handler metadata for {name}"));
        assert_eq!(handler.category, expected, "{name}");
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    store: Arc<FsBlobStore>,
    manager: UploadManager,
}

fn manager_with_store(runtime: KhiveRuntime, store: Arc<dyn BlobStore>) -> UploadManager {
    runtime.install_blob_store(store).unwrap();
    UploadManager {
        runtime,
        policy: UploadPolicy {
            idle_for: IDLE,
            sweep_interval: Duration::from_secs(600),
        },
        records: Mutex::new(HashMap::new()),
    }
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("blobs");
    let store = Arc::new(FsBlobStore::new(root.clone(), 0).unwrap());
    let manager = manager_with_store(KhiveRuntime::memory().unwrap(), store.clone());
    Fixture {
        _dir: dir,
        root,
        store,
        manager,
    }
}

fn upload_id(response: &Value) -> UploadId {
    UploadId::from_hex(response["upload_id"].as_str().unwrap()).unwrap()
}

async fn begin(manager: &UploadManager, size: u64) -> UploadId {
    upload_id(
        &manager
            .begin(size, None, "uploader:a".into())
            .await
            .unwrap(),
    )
}

fn staged(root: &Path, id: &UploadId) -> PathBuf {
    root.join(".uploads").join(id.as_str())
}

fn content_ref(bytes: &[u8]) -> ContentRef {
    ContentRef::from_digest_bytes(blake3::hash(bytes).as_bytes())
}

fn assert_unknown(error: RuntimeError) {
    assert!(matches!(error, RuntimeError::NotFound(_)), "{error}");
    assert!(error.to_string().contains("unknown upload"), "{error}");
}

async fn expire_record(manager: &UploadManager, id: &UploadId) {
    manager.record(id).unwrap().lock().await.last_part =
        Instant::now().checked_sub(IDLE * 2).unwrap();
}

fn age_file(path: &Path) {
    std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(1))
        .unwrap();
}

#[tokio::test]
async fn wrong_index_refuses_without_changing_upload() {
    let f = fixture();
    let id = begin(&f.manager, 3).await;
    let error = f.manager.put_part(&id, 1, b"a".to_vec()).await.unwrap_err();
    assert!(error.to_string().contains("expected index 0"));
    assert_eq!(std::fs::read(staged(&f.root, &id)).unwrap(), b"");
    f.manager.put_part(&id, 0, b"a".to_vec()).await.unwrap();
    f.manager.put_part(&id, 1, b"b".to_vec()).await.unwrap();
    let error = f.manager.put_part(&id, 0, b"a".to_vec()).await.unwrap_err();
    assert!(error.to_string().contains("expected index 2"));
    assert_eq!(std::fs::read(staged(&f.root, &id)).unwrap(), b"ab");
    f.manager.put_part(&id, 2, b"c".to_vec()).await.unwrap();
    let result = f.manager.commit(&id).await.unwrap();
    assert_eq!(result["content_ref"], content_ref(b"abc").to_string());
}

#[tokio::test]
async fn identical_tail_retry_preserves_hash_counts_and_idle_clocks() {
    let f = fixture();
    let id = begin(&f.manager, 6).await;
    let accepted = f.manager.put_part(&id, 0, b"abc".to_vec()).await.unwrap();
    let entry = f.manager.record(&id).unwrap();
    let (last_part, hash, tail) = {
        let mut record = entry.lock().await;
        record.last_part = Instant::now().checked_sub(Duration::from_secs(10)).unwrap();
        (record.last_part, record.hasher.finalize(), record.tail)
    };
    let path = staged(&f.root, &id);
    age_file(&path);
    let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
    let retried = f.manager.put_part(&id, 0, b"abc".to_vec()).await.unwrap();
    assert_eq!(retried, accepted);
    {
        let record = entry.lock().await;
        assert_eq!(record.received_bytes, 3);
        assert_eq!(record.next_index, 1);
        assert_eq!(record.hasher.finalize(), hash);
        assert_eq!(record.tail, tail);
        assert_eq!(record.last_part, last_part);
    }
    assert_eq!(
        std::fs::metadata(&path).unwrap().modified().unwrap(),
        modified
    );
    assert_eq!(std::fs::read(&path).unwrap(), b"abc");
    f.manager.put_part(&id, 1, b"def".to_vec()).await.unwrap();
    let result = f.manager.commit(&id).await.unwrap();
    assert_eq!(result["content_ref"], content_ref(b"abcdef").to_string());
}

#[tokio::test]
async fn different_length_tail_retry_aborts_and_commit_is_unknown() {
    let f = fixture();
    let id = begin(&f.manager, 6).await;
    f.manager.put_part(&id, 0, b"abc".to_vec()).await.unwrap();
    let error = f
        .manager
        .put_part(&id, 0, b"ab".to_vec())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("tail retry differs"));
    assert!(!staged(&f.root, &id).exists());
    assert_unknown(f.manager.commit(&id).await.unwrap_err());
    assert!(!f.store.exists(&content_ref(b"abc")).await.unwrap());
}

#[tokio::test]
async fn same_length_different_bytes_tail_retry_aborts_and_commit_is_unknown() {
    let f = fixture();
    let id = begin(&f.manager, 6).await;
    f.manager.put_part(&id, 0, b"abc".to_vec()).await.unwrap();
    let error = f
        .manager
        .put_part(&id, 0, b"abd".to_vec())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("tail retry differs"));
    assert!(!staged(&f.root, &id).exists());
    assert_unknown(f.manager.commit(&id).await.unwrap_err());
    assert!(!f.store.exists(&content_ref(b"abc")).await.unwrap());
}

#[tokio::test]
async fn zero_byte_part_advances_index_and_is_retryable() {
    let f = fixture();
    let id = begin(&f.manager, 1).await;
    let params = json!({"upload_id": id.to_string(), "index": 0, "bytes": ""});
    let accepted = handle_put_part(&f.manager, params.clone()).await.unwrap();
    assert_eq!(accepted, json!({"next_index": 1, "received_bytes": 0}));
    assert_eq!(handle_put_part(&f.manager, params).await.unwrap(), accepted);
    f.manager.put_part(&id, 1, b"a".to_vec()).await.unwrap();
    assert_eq!(
        f.manager.commit(&id).await.unwrap()["content_ref"],
        content_ref(b"a").to_string()
    );
}

#[tokio::test]
async fn zero_byte_object_commits_without_a_part() {
    let f = fixture();
    let id = begin(&f.manager, 0).await;
    let result = f.manager.commit(&id).await.unwrap();
    let reference = content_ref(b"");
    assert_eq!(
        result,
        json!({"content_ref": reference.to_string(), "size": 0})
    );
    assert_eq!(
        f.store.get_bounded_verified(&reference, 0).await.unwrap(),
        b""
    );
    assert!(!staged(&f.root, &id).exists());
    assert_unknown(f.manager.commit(&id).await.unwrap_err());
}

#[tokio::test]
async fn expected_hash_mismatch_aborts_and_cleans_staging() {
    let f = fixture();
    let id = upload_id(
        &f.manager
            .begin(3, Some(content_ref(b"abd")), "uploader:a".into())
            .await
            .unwrap(),
    );
    f.manager.put_part(&id, 0, b"abc".to_vec()).await.unwrap();
    let error = f.manager.commit(&id).await.unwrap_err();
    assert!(error.to_string().contains("content_ref mismatch"));
    assert!(!staged(&f.root, &id).exists());
    assert_unknown(f.manager.commit(&id).await.unwrap_err());
    assert!(!f.store.exists(&content_ref(b"abc")).await.unwrap());
    assert!(!f.store.exists(&content_ref(b"abd")).await.unwrap());
}

#[tokio::test]
async fn partial_commit_refuses_then_remaining_parts_can_complete() {
    let f = fixture();
    let id = begin(&f.manager, 6).await;
    f.manager.put_part(&id, 0, b"abc".to_vec()).await.unwrap();
    let error = f.manager.commit(&id).await.unwrap_err();
    assert!(error.to_string().contains("received 3 bytes, expected 6"));
    assert_eq!(std::fs::read(staged(&f.root, &id)).unwrap(), b"abc");
    f.manager.put_part(&id, 1, b"def".to_vec()).await.unwrap();
    let reference = content_ref(b"abcdef");
    assert_eq!(
        f.manager.commit(&id).await.unwrap(),
        json!({"content_ref": reference.to_string(), "size": 6})
    );
    assert_eq!(
        f.store.get_bounded_verified(&reference, 6).await.unwrap(),
        b"abcdef"
    );
    assert!(!staged(&f.root, &id).exists());
}

#[tokio::test]
async fn part_crossing_declared_size_aborts_without_publishing() {
    let f = fixture();
    let id = begin(&f.manager, 3).await;
    f.manager.put_part(&id, 0, b"ab".to_vec()).await.unwrap();
    let error = f
        .manager
        .put_part(&id, 1, b"cd".to_vec())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("exceeds declared size"));
    assert!(!staged(&f.root, &id).exists());
    assert_unknown(f.manager.commit(&id).await.unwrap_err());
    assert!(!f.store.exists(&content_ref(b"abcd")).await.unwrap());
}

#[tokio::test]
async fn begin_rejects_above_64_mib_before_creating_staging() {
    let f = fixture();
    assert_eq!(MAX_OBJECT_BYTES, 64 * 1024 * 1024);
    let error = f
        .manager
        .begin(MAX_OBJECT_BYTES + 1, None, "uploader:a".into())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("size exceeds"));
    assert!(f.manager.records.lock().unwrap().is_empty());
    assert!(!f.root.join(".uploads").exists());
    let id = begin(&f.manager, MAX_OBJECT_BYTES).await;
    assert_eq!(std::fs::metadata(staged(&f.root, &id)).unwrap().len(), 0);
    f.manager.abort(&id).await.unwrap();
}

#[tokio::test]
async fn advertised_part_limit_matches_live_formula_and_parser_accepts_maximum_part() {
    let f = fixture();
    let budget =
        khive_request::MAX_OPS_INPUT_LEN.min(khive_runtime::daemon::MAX_FRAME_BYTES) as u64;
    assert_eq!(REQUEST_RESERVE, 8192);
    assert!(budget > REQUEST_RESERVE);
    let limit = (budget - REQUEST_RESERVE) * 3 / 4;
    let response = f
        .manager
        .begin(limit, None, "uploader:a".into())
        .await
        .unwrap();
    assert_eq!(response["part_limit"], limit);
    assert_eq!(max_request_part_raw_bytes(), limit);
    let id = upload_id(&response);
    let bytes = vec![b'x'; limit as usize];
    let params = json!({"upload_id": id.to_string(), "index": 0, "bytes": BASE64.encode(&bytes)});
    let ops = serde_json::to_string(&json!({"tool": "blob.put_part", "args": params})).unwrap();
    assert!(ops.len() <= khive_request::MAX_OPS_INPUT_LEN);
    let parsed = khive_request::parse_request(&ops).unwrap();
    assert_eq!(parsed.ops.len(), 1);
    assert_eq!(parsed.ops[0].tool, "blob.put_part");
    assert!(
        serde_json::to_vec(&json!({"ops": ops, "actor_id": "uploader:a", "namespace": "local"}))
            .unwrap()
            .len()
            <= khive_runtime::daemon::MAX_FRAME_BYTES
    );
    let decoded = Value::Object(
        parsed.ops[0]
            .args
            .iter()
            .map(|(name, argument)| (name.clone(), argument.as_value().unwrap().clone()))
            .collect(),
    );
    assert_eq!(
        handle_put_part(&f.manager, decoded).await.unwrap(),
        json!({"next_index": 1, "received_bytes": limit})
    );
    assert_eq!(
        f.manager.commit(&id).await.unwrap()["content_ref"],
        content_ref(&bytes).to_string()
    );
}

#[tokio::test]
async fn part_limit_plus_one_is_rejected_on_decoded_length_without_aborting() {
    let f = fixture();
    let limit = max_request_part_raw_bytes();
    let id = begin(&f.manager, limit + 1).await;
    let params = json!({"upload_id": id.to_string(), "index": 0, "bytes": BASE64.encode(vec![b'x'; limit as usize + 1])});
    let error = handle_put_part(&f.manager, params).await.unwrap_err();
    assert!(
        error.to_string().contains(&format!(
            "decoded length {} exceeds part_limit {limit}",
            limit + 1
        )),
        "{error}"
    );
    assert_eq!(std::fs::metadata(staged(&f.root, &id)).unwrap().len(), 0);
    let entry = f.manager.record(&id).unwrap();
    let record = entry.lock().await;
    assert_eq!(record.received_bytes, 0);
    assert_eq!(record.next_index, 0);
    assert!(record.phase == UploadPhase::Active);
    drop(record);
    f.manager.abort(&id).await.unwrap();
}

fn parsed_part_params(id: &UploadId, index: u64, bytes: &[u8]) -> Value {
    let params =
        json!({"upload_id": id.to_string(), "index": index, "bytes": BASE64.encode(bytes)});
    let ops = serde_json::to_string(&json!({"tool": "blob.put_part", "args": params})).unwrap();
    assert!(ops.len() < khive_request::MAX_OPS_INPUT_LEN);
    let parsed = khive_request::parse_request(&ops).unwrap();
    assert_eq!(parsed.ops.len(), 1);
    assert_eq!(parsed.ops[0].tool, "blob.put_part");
    Value::Object(
        parsed.ops[0]
            .args
            .iter()
            .map(|(name, argument)| (name.clone(), argument.as_value().unwrap().clone()))
            .collect(),
    )
}

#[tokio::test]
async fn oversized_tail_retry_aborts_for_decoded_and_predecode_limits() {
    let limit = max_request_part_raw_bytes();
    for excess in [1, 6] {
        let f = fixture();
        let id = begin(&f.manager, limit * 2).await;
        f.manager.put_part(&id, 0, b"abc".to_vec()).await.unwrap();
        let params = parsed_part_params(&id, 0, &vec![b'x'; (limit + excess) as usize]);
        assert_eq!(
            params["bytes"].as_str().unwrap().len() as u64 > limit * 4 / 3 + 4,
            excess == 6
        );
        let error = handle_put_part(&f.manager, params).await.unwrap_err();
        assert!(matches!(error, RuntimeError::InvalidInput(_)), "{error}");
        assert!(
            error.to_string().contains(if excess == 1 {
                "tail retry differs"
            } else {
                "base64 input exceeds"
            }),
            "{error}"
        );
        assert!(!staged(&f.root, &id).exists(), "excess {excess}");
        assert_unknown(f.manager.commit(&id).await.unwrap_err());
        assert!(!f.store.exists(&content_ref(b"abc")).await.unwrap());
    }
}

#[tokio::test]
async fn oversized_next_part_crossing_declared_size_aborts_for_both_limits() {
    let limit = max_request_part_raw_bytes();
    for excess in [1, 6] {
        let f = fixture();
        let id = begin(&f.manager, limit + 6).await;
        f.manager
            .put_part(&id, 0, b"prefix".to_vec())
            .await
            .unwrap();
        let params = parsed_part_params(&id, 1, &vec![b'x'; (limit + excess) as usize]);
        assert_eq!(
            params["bytes"].as_str().unwrap().len() as u64 > limit * 4 / 3 + 4,
            excess == 6
        );
        let error = handle_put_part(&f.manager, params).await.unwrap_err();
        assert!(matches!(error, RuntimeError::InvalidInput(_)), "{error}");
        assert!(
            error.to_string().contains(if excess == 1 {
                "exceeds declared size"
            } else {
                "base64 input exceeds"
            }),
            "{error}"
        );
        assert!(!staged(&f.root, &id).exists(), "excess {excess}");
        assert_unknown(f.manager.commit(&id).await.unwrap_err());
        assert!(!f.store.exists(&content_ref(b"prefix")).await.unwrap());
    }
    for (excess, padding) in [(6, 0), (7, 2), (8, 1)] {
        let f = fixture();
        let raw_length = limit + excess;
        let id = begin(&f.manager, raw_length - 1).await;
        let params = parsed_part_params(&id, 0, &vec![b'x'; raw_length as usize]);
        let encoded = params["bytes"].as_str().unwrap();
        assert!(encoded.len() as u64 > limit * 4 / 3 + 4);
        assert_eq!(
            encoded
                .bytes()
                .rev()
                .take_while(|byte| *byte == b'=')
                .count(),
            padding
        );
        let error = handle_put_part(&f.manager, params).await.unwrap_err();
        assert!(matches!(error, RuntimeError::InvalidInput(_)), "{error}");
        assert!(
            error.to_string().contains("base64 input exceeds"),
            "{error}"
        );
        assert!(
            !staged(&f.root, &id).exists(),
            "raw excess {excess}, padding {padding}"
        );
        assert_unknown(f.manager.commit(&id).await.unwrap_err());
    }
}

#[tokio::test]
async fn oversized_first_part_within_declared_size_keeps_upload_usable_for_both_limits() {
    let limit = max_request_part_raw_bytes();
    for excess in [1, 6] {
        let f = fixture();
        let id = begin(&f.manager, limit + 6).await;
        let params = parsed_part_params(&id, 0, &vec![b'x'; (limit + excess) as usize]);
        assert_eq!(
            params["bytes"].as_str().unwrap().len() as u64 > limit * 4 / 3 + 4,
            excess == 6
        );
        let error = handle_put_part(&f.manager, params).await.unwrap_err();
        assert!(matches!(error, RuntimeError::InvalidInput(_)), "{error}");
        assert!(
            error.to_string().contains(if excess == 1 {
                "decoded length"
            } else {
                "base64 input exceeds"
            }),
            "{error}"
        );
        assert_eq!(std::fs::metadata(staged(&f.root, &id)).unwrap().len(), 0);
        {
            let entry = f.manager.record(&id).unwrap();
            let record = entry.lock().await;
            assert_eq!(record.received_bytes, 0);
            assert_eq!(record.next_index, 0);
            assert_eq!(record.hasher.finalize(), blake3::hash(b""));
            assert!(record.phase == UploadPhase::Active);
        }
        assert_eq!(
            handle_put_part(&f.manager, parsed_part_params(&id, 0, b"a"))
                .await
                .unwrap(),
            json!({"next_index": 1, "received_bytes": 1})
        );
        assert_eq!(
            handle_put_part(&f.manager, parsed_part_params(&id, 1, b"bc"))
                .await
                .unwrap(),
            json!({"next_index": 2, "received_bytes": 3})
        );
        assert_eq!(std::fs::read(staged(&f.root, &id)).unwrap(), b"abc");
        assert_eq!(
            f.manager.abort(&id).await.unwrap(),
            json!({"aborted": true})
        );
        assert!(!staged(&f.root, &id).exists());
    }
    for malformed in ["inner padding", "noncanonical terminal padding bits"] {
        let f = fixture();
        let id = begin(&f.manager, 3).await;
        let mut encoded = BASE64.encode(vec![b'x'; (limit + 7) as usize]);
        assert!(encoded.len() as u64 > limit * 4 / 3 + 4);
        if malformed == "inner padding" {
            encoded.replace_range(4..5, "=");
        } else {
            assert!(encoded.ends_with("eA=="));
            let offset = encoded.len() - 3;
            encoded.replace_range(offset..offset + 1, "B");
        }
        let ops = serde_json::to_string(&json!({
            "tool": "blob.put_part",
            "args": {"upload_id": id.to_string(), "index": 0, "bytes": encoded}
        }))
        .unwrap();
        assert!(ops.len() < khive_request::MAX_OPS_INPUT_LEN);
        let parsed = khive_request::parse_request(&ops).unwrap();
        let params = Value::Object(
            parsed.ops[0]
                .args
                .iter()
                .map(|(name, argument)| (name.clone(), argument.as_value().unwrap().clone()))
                .collect(),
        );
        let error = handle_put_part(&f.manager, params).await.unwrap_err();
        assert!(matches!(error, RuntimeError::InvalidInput(_)), "{error}");
        assert!(
            error.to_string().contains("invalid base64"),
            "{malformed}: {error}"
        );
        assert_eq!(std::fs::metadata(staged(&f.root, &id)).unwrap().len(), 0);
        {
            let entry = f.manager.record(&id).unwrap();
            let record = entry.lock().await;
            assert_eq!(record.received_bytes, 0);
            assert_eq!(record.next_index, 0);
            assert_eq!(record.hasher.finalize(), blake3::hash(b""));
            assert!(record.phase == UploadPhase::Active);
        }
        handle_put_part(&f.manager, parsed_part_params(&id, 0, b"a"))
            .await
            .unwrap();
        handle_put_part(&f.manager, parsed_part_params(&id, 1, b"bc"))
            .await
            .unwrap();
        assert_eq!(
            f.manager.commit(&id).await.unwrap(),
            json!({"content_ref": content_ref(b"abc").to_string(), "size": 3})
        );
        assert!(!staged(&f.root, &id).exists());
        assert_eq!(
            f.store
                .get_bounded_verified(&content_ref(b"abc"), 3)
                .await
                .unwrap(),
            b"abc"
        );
    }
}

#[tokio::test]
async fn existing_expected_reference_returns_stored_size_without_staging() {
    let f = fixture();
    let reference = f.store.put(b"existing".to_vec()).await.unwrap();
    let result = f
        .manager
        .begin(1, Some(reference.clone()), "uploader:a".into())
        .await
        .unwrap();
    assert_eq!(
        result,
        json!({"content_ref": reference.to_string(), "size": 8})
    );
    assert!(result.get("upload_id").is_none());
    assert!(f.manager.records.lock().unwrap().is_empty());
    assert!(!f.root.join(".uploads").exists());
}

#[tokio::test]
async fn abort_removes_staging_and_consumes_capability() {
    let f = fixture();
    let id = begin(&f.manager, 3).await;
    f.manager.put_part(&id, 0, b"abc".to_vec()).await.unwrap();
    assert_eq!(
        f.manager.abort(&id).await.unwrap(),
        json!({"aborted": true})
    );
    assert!(!staged(&f.root, &id).exists());
    assert_unknown(f.manager.abort(&id).await.unwrap_err());
    assert_unknown(f.manager.commit(&id).await.unwrap_err());
}

#[tokio::test]
async fn verb_expiry_rejects_stale_part_without_a_sweeper() {
    let f = fixture();
    let id = begin(&f.manager, 2).await;
    f.manager.put_part(&id, 0, b"a".to_vec()).await.unwrap();
    expire_record(&f.manager, &id).await;
    assert!(staged(&f.root, &id).exists());
    assert_unknown(f.manager.put_part(&id, 1, b"b".to_vec()).await.unwrap_err());
    assert!(!staged(&f.root, &id).exists());
    assert_unknown(f.manager.commit(&id).await.unwrap_err());
}

#[tokio::test]
async fn verb_expiry_rejects_stale_complete_commit_without_a_sweeper() {
    let f = fixture();
    let id = begin(&f.manager, 1).await;
    f.manager.put_part(&id, 0, b"a".to_vec()).await.unwrap();
    expire_record(&f.manager, &id).await;
    assert_unknown(f.manager.commit(&id).await.unwrap_err());
    assert!(!staged(&f.root, &id).exists());
    assert!(!f.store.exists(&content_ref(b"a")).await.unwrap());
}

#[tokio::test]
async fn live_expiry_sweep_removes_staging_without_a_verb_call() {
    let f = fixture();
    let id = begin(&f.manager, 2).await;
    f.manager.put_part(&id, 0, b"a".to_vec()).await.unwrap();
    expire_record(&f.manager, &id).await;
    assert_eq!(f.manager.sweep().await.unwrap(), 1);
    assert!(!staged(&f.root, &id).exists());
    assert!(!f.manager.records.lock().unwrap().contains_key(&id));
    assert_eq!(f.manager.sweep().await.unwrap(), 0);
}

#[tokio::test]
async fn restart_orphan_sweep_removes_staging_without_a_live_record() {
    let f = fixture();
    let id = begin(&f.manager, 2).await;
    f.manager.put_part(&id, 0, b"a".to_vec()).await.unwrap();
    let runtime = f.manager.runtime.clone();
    drop(f.manager);
    let restarted = UploadManager {
        runtime,
        policy: UploadPolicy {
            idle_for: IDLE,
            sweep_interval: Duration::from_secs(600),
        },
        records: Mutex::new(HashMap::new()),
    };
    assert_unknown(restarted.commit(&id).await.unwrap_err());
    assert!(staged(&f.root, &id).exists());
    age_file(&staged(&f.root, &id));
    assert_eq!(restarted.sweep().await.unwrap(), 1);
    assert!(!staged(&f.root, &id).exists());
    let new_id = begin(&restarted, 2).await;
    assert_ne!(new_id, id);
    restarted
        .put_part(&new_id, 0, b"ab".to_vec())
        .await
        .unwrap();
    assert_eq!(
        restarted.commit(&new_id).await.unwrap()["content_ref"],
        content_ref(b"ab").to_string()
    );
}

#[tokio::test]
async fn sweep_preserves_fresh_upload_and_committed_object() {
    let f = fixture();
    let committed = f.store.put(b"committed".to_vec()).await.unwrap();
    let id = begin(&f.manager, 2).await;
    f.manager.put_part(&id, 0, b"a".to_vec()).await.unwrap();
    assert_eq!(f.manager.sweep().await.unwrap(), 0);
    assert_eq!(std::fs::read(staged(&f.root, &id)).unwrap(), b"a");
    assert!(f.store.exists(&committed).await.unwrap());
    f.manager.abort(&id).await.unwrap();
}

#[tokio::test]
async fn unconfigured_runtime_refuses_every_upload_operation() {
    let manager = UploadManager::new(KhiveRuntime::memory().unwrap());
    let id = UploadId::from_bytes(&[0; 16]);
    for result in [
        manager.begin(0, None, "uploader:a".into()).await,
        manager.put_part(&id, 0, Vec::new()).await,
        manager.commit(&id).await,
        manager.abort(&id).await,
        manager.sweep().await.map(Value::from),
    ] {
        let error = result.unwrap_err();
        assert!(matches!(error, RuntimeError::Unconfigured(_)), "{error}");
    }
    assert!(manager.records.lock().unwrap().is_empty());
}

#[tokio::test]
async fn read_only_runtime_refuses_every_upload_operation_without_mutation() {
    let f = fixture();
    let id = begin(&f.manager, 1).await;
    f.manager.put_part(&id, 0, b"a".to_vec()).await.unwrap();
    let config = RuntimeConfig {
        db_path: Some(f._dir.path().join("readonly.db")),
        packs: vec!["kg".into()],
        ..RuntimeConfig::no_embeddings()
    };
    let writable = KhiveRuntime::new(config.clone()).unwrap();
    let pool = writable.backend().pool_arc();
    // This constructor only prepares the schema synchronously and has no
    // embedding models to register. Prove that no standalone writer task
    // was spawned and no pool owner survives, rather than assuming drop
    // settles arbitrary runtime clones or detached writer tasks.
    assert!(!pool.writer_task_join_was_stored());
    let pool_lifetime = Arc::downgrade(&pool);
    drop(pool);
    drop(writable);
    assert!(pool_lifetime.upgrade().is_none());

    // The pool drops its writer before its readers, so closed connections
    // can still leave WAL sidecars. Let SQLite checkpoint and settle them
    // on one explicitly closed connection; never unlink uncheckpointed WAL.
    let settled = rusqlite::Connection::open(config.db_path.as_ref().unwrap()).unwrap();
    let mode: String = settled
        .pragma_update_and_check(None, "journal_mode", "DELETE", |row| row.get(0))
        .unwrap();
    assert_eq!(mode.to_ascii_lowercase(), "delete");
    settled.close().unwrap();
    let runtime = KhiveRuntime::new_readonly(config).unwrap();
    assert!(runtime.is_read_only());
    let mut readonly = manager_with_store(runtime, f.store.clone());
    readonly
        .records
        .get_mut()
        .unwrap()
        .insert(id.clone(), f.manager.record(&id).unwrap());
    for result in [
        readonly.begin(0, None, "uploader:a".into()).await,
        readonly.put_part(&id, 0, b"a".to_vec()).await,
        readonly.commit(&id).await,
        readonly.abort(&id).await,
        readonly.sweep().await.map(Value::from),
    ] {
        let error = result.unwrap_err();
        assert!(error.to_string().contains("read-only"), "{error}");
    }
    assert_eq!(std::fs::read(staged(&f.root, &id)).unwrap(), b"a");
    assert!(readonly.records.lock().unwrap().contains_key(&id));
    assert!(!f.store.exists(&content_ref(b"a")).await.unwrap());
    f.manager.abort(&id).await.unwrap();
}

#[derive(Debug)]
struct ControlledStore {
    inner: Arc<FsBlobStore>,
    abort_failures: AtomicUsize,
    abort_calls: AtomicUsize,
    pause_after_append: AtomicBool,
    fail_after_append: AtomicBool,
    wrong_append_length: AtomicBool,
    appended: tokio::sync::Notify,
}

impl ControlledStore {
    fn new(inner: Arc<FsBlobStore>) -> Self {
        Self {
            inner,
            abort_failures: AtomicUsize::new(0),
            abort_calls: AtomicUsize::new(0),
            pause_after_append: AtomicBool::new(false),
            fail_after_append: AtomicBool::new(false),
            wrong_append_length: AtomicBool::new(false),
            appended: tokio::sync::Notify::new(),
        }
    }
}

#[async_trait]
impl BlobStore for ControlledStore {
    async fn put(&self, bytes: Vec<u8>) -> StorageResult<ContentRef> {
        self.inner.put(bytes).await
    }

    async fn begin_upload(&self, size: u64) -> StorageResult<UploadId> {
        self.inner.begin_upload(size).await
    }

    async fn append_part(&self, id: &UploadId, bytes: Vec<u8>) -> StorageResult<u64> {
        let length = self.inner.append_part(id, bytes).await?;
        if self.pause_after_append.load(Ordering::SeqCst) {
            self.appended.notify_one();
            std::future::pending::<()>().await;
        }
        if self.fail_after_append.load(Ordering::SeqCst) {
            return Err(StorageError::Internal(
                "injected append failure after write".into(),
            ));
        }
        Ok(length + u64::from(self.wrong_append_length.load(Ordering::SeqCst)))
    }

    async fn commit_upload(&self, id: &UploadId, reference: &ContentRef) -> StorageResult<()> {
        self.inner.commit_upload(id, reference).await
    }

    async fn abort_upload(&self, id: &UploadId) -> StorageResult<()> {
        self.abort_calls.fetch_add(1, Ordering::SeqCst);
        if self
            .abort_failures
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
        {
            return Err(StorageError::Internal("injected abort failure".into()));
        }
        self.inner.abort_upload(id).await
    }

    async fn sweep_uploads(&self, idle_for: Duration) -> StorageResult<u64> {
        self.inner.sweep_uploads(idle_for).await
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

#[tokio::test]
async fn failed_abort_is_retained_and_retried_by_successive_sweeps() {
    let f = fixture();
    let store = Arc::new(ControlledStore::new(f.store.clone()));
    store.abort_failures.store(2, Ordering::SeqCst);
    let manager = manager_with_store(KhiveRuntime::memory().unwrap(), store.clone());
    let id = begin(&manager, 1).await;
    assert!(manager
        .abort(&id)
        .await
        .unwrap_err()
        .to_string()
        .contains("injected abort failure"));
    assert!(manager.record(&id).unwrap().lock().await.phase == UploadPhase::Aborted);
    assert!(staged(&f.root, &id).exists());
    assert!(manager
        .sweep()
        .await
        .unwrap_err()
        .to_string()
        .contains("injected abort failure"));
    assert!(manager.records.lock().unwrap().contains_key(&id));
    assert!(staged(&f.root, &id).exists());
    assert_eq!(manager.sweep().await.unwrap(), 1);
    assert_eq!(store.abort_calls.load(Ordering::SeqCst), 3);
    assert!(!manager.records.lock().unwrap().contains_key(&id));
    assert!(!staged(&f.root, &id).exists());
}

#[tokio::test]
async fn failed_error_cleanup_retains_aborted_upload_until_sweep_retries() {
    let f = fixture();
    let store = Arc::new(ControlledStore::new(f.store.clone()));
    store.abort_failures.store(1, Ordering::SeqCst);
    let manager = manager_with_store(KhiveRuntime::memory().unwrap(), store.clone());
    let id = begin(&manager, 3).await;
    manager.put_part(&id, 0, b"abc".to_vec()).await.unwrap();
    let error = manager.put_part(&id, 0, b"abd".to_vec()).await.unwrap_err();
    assert!(error.to_string().contains("tail retry differs"));
    assert!(manager.record(&id).unwrap().lock().await.phase == UploadPhase::Aborted);
    assert_eq!(std::fs::read(staged(&f.root, &id)).unwrap(), b"abc");
    assert_eq!(manager.sweep().await.unwrap(), 1);
    assert_eq!(store.abort_calls.load(Ordering::SeqCst), 2);
    assert!(!staged(&f.root, &id).exists());
    assert_unknown(manager.commit(&id).await.unwrap_err());
}

#[tokio::test]
async fn append_failure_after_real_write_aborts_and_never_acknowledges() {
    let f = fixture();
    let store = Arc::new(ControlledStore::new(f.store.clone()));
    store.fail_after_append.store(true, Ordering::SeqCst);
    let manager = manager_with_store(KhiveRuntime::memory().unwrap(), store);
    let id = begin(&manager, 3).await;
    let error = manager.put_part(&id, 0, b"abc".to_vec()).await.unwrap_err();
    assert!(error
        .to_string()
        .contains("injected append failure after write"));
    assert!(!staged(&f.root, &id).exists());
    assert_unknown(manager.commit(&id).await.unwrap_err());
    assert!(!f.store.exists(&content_ref(b"abc")).await.unwrap());
}

#[tokio::test]
async fn inconsistent_backend_length_aborts_and_never_acknowledges() {
    let f = fixture();
    let store = Arc::new(ControlledStore::new(f.store.clone()));
    store.wrong_append_length.store(true, Ordering::SeqCst);
    let manager = manager_with_store(KhiveRuntime::memory().unwrap(), store);
    let id = begin(&manager, 3).await;
    let error = manager.put_part(&id, 0, b"abc".to_vec()).await.unwrap_err();
    assert!(error.to_string().contains("staged upload length differs"));
    assert!(!staged(&f.root, &id).exists());
    assert_unknown(manager.commit(&id).await.unwrap_err());
}

#[tokio::test]
async fn cancelled_append_never_acknowledges_inconsistent_staged_bytes() {
    let f = fixture();
    let store = Arc::new(ControlledStore::new(f.store.clone()));
    store.pause_after_append.store(true, Ordering::SeqCst);
    let manager = Arc::new(manager_with_store(
        KhiveRuntime::memory().unwrap(),
        store.clone(),
    ));
    let id = begin(&manager, 6).await;
    let task = tokio::spawn({
        let manager = manager.clone();
        let id = id.clone();
        async move { manager.put_part(&id, 0, b"abc".to_vec()).await }
    });
    tokio::time::timeout(Duration::from_secs(10), store.appended.notified())
        .await
        .unwrap();
    assert_eq!(std::fs::read(staged(&f.root, &id)).unwrap(), b"abc");
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    {
        let entry = manager.record(&id).unwrap();
        let record = entry.lock().await;
        assert!(record.phase == UploadPhase::InFlight);
        assert_eq!(record.received_bytes, 0);
        assert_eq!(record.next_index, 0);
        assert_eq!(record.hasher.finalize(), blake3::hash(b""));
    }
    store.pause_after_append.store(false, Ordering::SeqCst);
    assert_unknown(manager.put_part(&id, 0, b"abc".to_vec()).await.unwrap_err());
    assert!(!staged(&f.root, &id).exists());
    assert_unknown(manager.commit(&id).await.unwrap_err());
    assert!(!f.store.exists(&content_ref(b"abcabc")).await.unwrap());
}

#[tokio::test]
async fn independent_capabilities_allow_cross_actor_append_commit_and_abort() {
    let f = fixture();
    let mut builder = VerbRegistryBuilder::new();
    builder.register(BlobPack::new(f.manager.runtime.clone()));
    let registry = builder.build().unwrap();
    let actor_a = RequestIdentity {
        namespace: "local".into(),
        actor_id: Some("uploader:a".into()),
        ..Default::default()
    };
    let actor_b = RequestIdentity {
        namespace: "other".into(),
        actor_id: Some("uploader:b".into()),
        ..Default::default()
    };
    let first = registry
        .dispatch_with_identity("blob.begin", json!({"size": 3}), Some(actor_a.clone()))
        .await
        .unwrap();
    let second = registry
        .dispatch_with_identity("blob.begin", json!({"size": 3}), Some(actor_b.clone()))
        .await
        .unwrap();
    let first_id = upload_id(&first);
    let second_id = upload_id(&second);
    assert_ne!(first_id, second_id);
    registry
        .dispatch_with_identity(
            "blob.put_part",
            json!({"upload_id": first_id.to_string(), "index": 0, "bytes": BASE64.encode(b"abc")}),
            Some(actor_b.clone()),
        )
        .await
        .unwrap();
    registry
        .dispatch_with_identity(
            "blob.put_part",
            json!({"upload_id": second_id.to_string(), "index": 0, "bytes": BASE64.encode(b"def")}),
            Some(actor_a.clone()),
        )
        .await
        .unwrap();
    registry
        .dispatch_with_identity(
            "blob.abort",
            json!({"upload_id": second_id.to_string()}),
            Some(actor_a),
        )
        .await
        .unwrap();
    assert!(!staged(&f.root, &second_id).exists());
    assert_eq!(std::fs::read(staged(&f.root, &first_id)).unwrap(), b"abc");
    let result = registry
        .dispatch_with_identity(
            "blob.commit",
            json!({"upload_id": first_id.to_string()}),
            Some(actor_b),
        )
        .await
        .unwrap();
    assert_eq!(result["content_ref"], content_ref(b"abc").to_string());
    assert!(!f.store.exists(&content_ref(b"def")).await.unwrap());
}

#[tokio::test]
async fn transactional_gc_preserves_committed_and_staging_then_upload_sweep_only_reaps_staging() {
    let f = fixture();
    let backend = khive_db::StorageBackend::sqlite(f._dir.path().join("gc.db")).unwrap();
    {
        let mut writer = backend.pool().writer().unwrap();
        let conn = writer.conn_mut();
        conn.execute_batch(include_str!(
            "../../../khive-db/sql/schema-migrations-table.sql"
        ))
        .unwrap();
        for migration in khive_db::MIGRATIONS
            .iter()
            .filter(|migration| migration.version <= 20)
        {
            let tx = conn.transaction().unwrap();
            tx.execute_batch(migration.up).unwrap();
            tx.execute(
                "INSERT INTO _schema_migrations (version, name, applied_at) VALUES (?1, ?2, 0)",
                (migration.version, migration.name),
            )
            .unwrap();
            tx.commit().unwrap();
        }
        khive_db::migrations::stage_attachment_cutover(conn).unwrap();
        khive_db::migrations::finalize_attachment_cutover(conn).unwrap();
        assert_eq!(
            khive_db::migrations::read_schema_version(conn).unwrap(),
            khive_db::migrations::ATTACHMENT_CUTOVER_VERSION
        );
    }
    let reference = f.store.put(b"committed control".to_vec()).await.unwrap();
    {
        let writer = backend.pool().writer().unwrap();
        writer
            .conn()
            .execute_batch(
                "INSERT INTO entities \
                 (id, namespace, kind, name, tags, created_at, updated_at, deleted_at) \
                 VALUES ('11111111-1111-4111-8111-111111111111', 'local', 'document', \
                         'live blob control', '[]', 1, 1, NULL);",
            )
            .unwrap();
        writer
            .conn()
            .execute(
                "INSERT INTO attachments \
                 (record_uuid, substrate, role, content_ref, created_at) \
                 VALUES ('11111111-1111-4111-8111-111111111111', \
                         'entity', 'content', ?1, 1)",
                [reference.as_str()],
            )
            .unwrap();
    }
    let committed_path = f
        .root
        .join(&reference.as_str()[..2])
        .join(&reference.as_str()[2..4])
        .join(reference.as_str());
    age_file(&committed_path);
    assert!(
        std::fs::metadata(&committed_path)
            .unwrap()
            .modified()
            .unwrap()
            .elapsed()
            .unwrap()
            > FsBlobStore::DEFAULT_ORPHAN_SWEEP_GRACE
    );
    let id = begin(&f.manager, 3).await;
    f.manager.put_part(&id, 0, b"abc".to_vec()).await.unwrap();
    let result = f
        .store
        .transactional_orphan_sweep(backend.sql().as_ref(), false)
        .await
        .unwrap();
    assert_eq!(result.scanned, 1);
    assert_eq!(result.deleted, 0);
    assert_eq!(result.would_delete, 0);
    assert_eq!(result.grace_period_skipped, 0);
    assert_eq!(std::fs::read(staged(&f.root, &id)).unwrap(), b"abc");
    assert!(f.store.exists(&reference).await.unwrap());
    expire_record(&f.manager, &id).await;
    assert_eq!(f.manager.sweep().await.unwrap(), 1);
    assert!(!staged(&f.root, &id).exists());
    assert_eq!(
        f.store.get_bounded_verified(&reference, 32).await.unwrap(),
        b"committed control"
    );
}
