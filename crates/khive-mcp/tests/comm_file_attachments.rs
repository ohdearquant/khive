use std::sync::Arc;

use async_trait::async_trait;
use khive_db::stores::blob::FsBlobStore;
use khive_pack_blob as _;
use khive_pack_comm as _;
use khive_pack_kg as _;
use khive_runtime::{
    KhiveRuntime, PackRegistry, RequestIdentity, RuntimeError, VerbRegistry, VerbRegistryBuilder,
};
use khive_storage::{BlobStore, ContentRef, SqlStatement, SqlValue, StorageError, StorageResult};
use serde_json::{json, Value};
use uuid::Uuid;

static FILE_ENV: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const MAX_ATTACHMENT_BYTES: u64 = 64 * 1024 * 1024;

fn identity(actor: &str) -> RequestIdentity {
    RequestIdentity {
        namespace: "local".into(),
        actor_id: Some(actor.into()),
        ..Default::default()
    }
}

fn registry(runtime: KhiveRuntime) -> VerbRegistry {
    let mut builder = VerbRegistryBuilder::new();
    PackRegistry::register_packs(
        &["kg".into(), "comm".into(), "blob".into()],
        runtime,
        &mut builder,
    )
    .expect("register message packs");
    builder.build().expect("message registry")
}

fn fixture() -> (
    VerbRegistry,
    KhiveRuntime,
    Arc<FsBlobStore>,
    tempfile::TempDir,
) {
    let root = tempfile::tempdir().expect("private blob root");
    let store = Arc::new(FsBlobStore::new(root.path().to_path_buf(), 0).expect("blob store"));
    let file_policy: khive_runtime::KhiveConfig =
        serde_json::from_value(json!({"blob": {"file_transfers": true}}))
            .expect("explicit file transfer policy");
    let base = khive_runtime::RuntimeConfig {
        db_path: None,
        packs: vec!["kg".into()],
        brain_profile: None,
        actor_id: None,
        ..khive_runtime::RuntimeConfig::no_embeddings()
    };
    let config = khive_runtime::runtime::runtime_config_from_khive_config(&file_policy, base);
    let runtime = KhiveRuntime::new(config).expect("message runtime");
    runtime
        .install_blob_store(store.clone())
        .expect("install message blob store");
    (registry(runtime.clone()), runtime, store, root)
}

async fn count(runtime: &KhiveRuntime, table: &str) -> i64 {
    let sql = match table {
        "notes" => "SELECT COUNT(*) FROM notes",
        "attachments" => "SELECT COUNT(*) FROM attachments",
        _ => panic!("unknown fixture table"),
    };
    match runtime
        .sql()
        .reader()
        .await
        .expect("count reader")
        .query_scalar(SqlStatement {
            sql: sql.into(),
            params: vec![],
            label: Some("message-attachment-fixture-count".into()),
        })
        .await
        .expect("fixture count")
    {
        Some(SqlValue::Integer(value)) => value,
        other => panic!("count must be an integer: {other:?}"),
    }
}

async fn copy_ids(runtime: &KhiveRuntime) -> Vec<Uuid> {
    runtime
        .sql()
        .reader()
        .await
        .expect("copy reader")
        .query_all(SqlStatement {
            sql: "SELECT id FROM notes WHERE kind = 'message' ORDER BY id".into(),
            params: vec![],
            label: Some("message-attachment-fixture-copies".into()),
        })
        .await
        .expect("message copies")
        .iter()
        .map(|row| match row.get("id") {
            Some(SqlValue::Text(id)) => Uuid::parse_str(id).expect("message UUID"),
            other => panic!("copy id must be text: {other:?}"),
        })
        .collect()
}

fn assert_metadata(message: &Value, references: &[ContentRef], sizes: &[u64]) {
    let attachments = message["attachments"]
        .as_array()
        .expect("message attachment metadata array");
    assert_eq!(attachments.len(), references.len());
    for ((attachment, reference), size) in attachments.iter().zip(references).zip(sizes) {
        assert_eq!(attachment["content_ref"], reference.as_str());
        assert_eq!(attachment["size"], *size);
        assert!(attachment.get("media_type").is_some());
        assert!(attachment.get("bytes").is_none());
    }
}

#[tokio::test]
async fn local_send_attaches_every_copy_and_metadata_view() {
    let _guard = FILE_ENV.lock().await;
    let (registry, runtime, store, _root) = fixture();
    let refs = vec![
        store.put(b"first attachment".to_vec()).await.unwrap(),
        store.put(b"second attachment".to_vec()).await.unwrap(),
    ];
    let sizes = [16, 17];
    let sent = registry
        .dispatch_with_identity(
            "comm.send",
            json!({"to": "actor:recipient", "content": "two files", "attachments": refs}),
            Some(identity("actor:sender")),
        )
        .await
        .expect("local send with two published attachments");
    let files = FileRoots::new();
    for (index, reference) in refs.iter().enumerate() {
        let path = format!("attachment-{index}.bin");
        let exported = registry
            .dispatch(
                "blob.export",
                json!({"content_ref": reference, "path": path}),
            )
            .await
            .expect("export a message attachment");
        assert_eq!(exported["size"], sizes[index]);
        assert!(exported.get("bytes").is_none());
        let original: &[u8] = if index == 0 {
            b"first attachment"
        } else {
            b"second attachment"
        };
        assert_eq!(std::fs::read(files.export_path(&path)).unwrap(), original);
    }
    let ids = copy_ids(&runtime).await;
    assert_eq!(ids.len(), 2, "one outbound and one inbound copy");
    for id in ids {
        let rows = runtime
            .attachments()
            .unwrap()
            .list_attachments(id)
            .await
            .unwrap();
        assert_eq!(rows.len(), 2, "every message copy owns both attachments");
        for ((row, reference), size) in rows.iter().zip(&refs).zip(sizes) {
            assert_eq!(row.substrate, khive_storage::AttachmentSubstrate::Note);
            assert_eq!(&row.content_ref, reference);
            assert_eq!(row.size_bytes, Some(size));
            assert_ne!(row.role, "quarantine-original");
        }
    }
    let inbox = registry
        .dispatch_with_identity(
            "comm.inbox",
            json!({"status": "all"}),
            Some(identity("actor:recipient")),
        )
        .await
        .expect("recipient inbox");
    let received = inbox["messages"].as_array().unwrap();
    assert_eq!(received.len(), 1);
    assert_metadata(&received[0], &refs, &sizes);
    let read = registry
        .dispatch_with_identity(
            "comm.read",
            json!({"id": received[0]["id"]}),
            Some(identity("actor:recipient")),
        )
        .await
        .expect("recipient reads message metadata");
    assert_eq!(read["status"], "success");
    assert_metadata(&read, &refs, &sizes);
    let thread = registry
        .dispatch_with_identity(
            "comm.thread",
            json!({"id": sent["thread_id"], "fields": ["id", "attachments"]}),
            Some(identity("actor:recipient")),
        )
        .await
        .expect("attachment projection in thread");
    let messages = thread["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 1);
    assert_metadata(&messages[0], &refs, &sizes);
}

#[tokio::test]
async fn local_reply_attaches_both_new_copies() {
    let (registry, runtime, store, _root) = fixture();
    registry
        .dispatch_with_identity(
            "comm.send",
            json!({"to": "actor:recipient", "content": "reply root"}),
            Some(identity("actor:sender")),
        )
        .await
        .expect("ordinary local root");
    let inbox = registry
        .dispatch_with_identity("comm.inbox", json!({}), Some(identity("actor:recipient")))
        .await
        .unwrap();
    let reference = store.put(b"reply file".to_vec()).await.unwrap();
    registry
        .dispatch_with_identity(
            "comm.reply",
            json!({"id": inbox["messages"][0]["id"], "content": "attached reply", "attachments": [reference]}),
            Some(identity("actor:recipient")),
        )
        .await
        .expect("reply with an existing blob");
    assert_eq!(count(&runtime, "notes").await, 4);
    assert_eq!(count(&runtime, "attachments").await, 2);
    let mut attached = 0;
    for id in copy_ids(&runtime).await {
        let rows = runtime
            .attachments()
            .unwrap()
            .list_attachments(id)
            .await
            .unwrap();
        if !rows.is_empty() {
            attached += 1;
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].content_ref, reference);
        }
    }
    assert_eq!(attached, 2);
}

async fn assert_refusal(
    registry: &VerbRegistry,
    runtime: &KhiveRuntime,
    params: Value,
    reason: &str,
) -> String {
    let error = registry
        .dispatch_with_identity("comm.send", params, Some(identity("actor:sender")))
        .await
        .expect_err("attachment policy must refuse before publication");
    let rendered = error.to_string();
    assert!(
        matches!(error.refusal_source(), RuntimeError::InvalidInput(_)),
        "typed bad arguments: {error:?}"
    );
    assert!(
        rendered.contains(reason),
        "specific policy refusal: {rendered}"
    );
    assert!(
        !rendered.contains("unknown field"),
        "unknown parameters do not prove attachment validation"
    );
    assert_eq!(count(runtime, "notes").await, 0);
    assert_eq!(count(runtime, "attachments").await, 0);
    rendered
}

#[tokio::test]
async fn missing_attachment_ref_refuses_without_rows() {
    let (registry, runtime, _store, _root) = fixture();
    let missing = "ab".repeat(32);
    let error = assert_refusal(
        &registry,
        &runtime,
        json!({"to": "actor:recipient", "content": "missing file", "attachments": [missing]}),
        "no object exists for attachment",
    )
    .await;
    assert!(
        error.contains(&missing),
        "missing reference must be named: {error}"
    );
}

#[tokio::test]
async fn nine_attachment_refs_refuse_without_rows() {
    let (registry, runtime, store, _root) = fixture();
    let mut references = Vec::new();
    for index in 0..9 {
        references.push(store.put(vec![index]).await.unwrap());
    }
    assert_refusal(
        &registry,
        &runtime,
        json!({"to": "actor:recipient", "content": "too many files", "attachments": references}),
        "at most 8 attachments",
    )
    .await;
}

#[tokio::test]
async fn duplicate_attachment_refs_refuse_without_rows() {
    let (registry, runtime, store, _root) = fixture();
    let reference = store.put(b"duplicate".to_vec()).await.unwrap();
    assert_refusal(
        &registry,
        &runtime,
        json!({"to": "actor:recipient", "content": "duplicate file", "attachments": [reference, reference]}),
        "duplicate attachment",
    )
    .await;
}

#[derive(Debug)]
struct MetadataStore;

#[async_trait]
impl BlobStore for MetadataStore {
    async fn put(&self, _bytes: Vec<u8>) -> StorageResult<ContentRef> {
        Err(StorageError::Internal("metadata-only fixture".into()))
    }
    async fn get_bounded_verified(
        &self,
        _reference: &ContentRef,
        _max: u64,
    ) -> StorageResult<Vec<u8>> {
        Err(StorageError::Internal("metadata-only fixture".into()))
    }
    async fn exists(&self, _reference: &ContentRef) -> StorageResult<bool> {
        Ok(true)
    }
    async fn size(&self, _reference: &ContentRef) -> StorageResult<Option<u64>> {
        Ok(Some(MAX_ATTACHMENT_BYTES / 2 + 1))
    }
    async fn delete(&self, _reference: &ContentRef) -> StorageResult<bool> {
        Err(StorageError::Internal("metadata-only fixture".into()))
    }
}

#[tokio::test]
async fn oversized_attachment_total_refuses_without_rows() {
    let runtime = KhiveRuntime::memory().unwrap();
    runtime.install_blob_store(Arc::new(MetadataStore)).unwrap();
    let registry = registry(runtime.clone());
    assert_refusal(
        &registry,
        &runtime,
        json!({"to": "actor:recipient", "content": "large files", "attachments": ["ab".repeat(32), "cd".repeat(32)]}),
        "attachment total exceeds",
    )
    .await;
}

#[tokio::test]
async fn external_attachment_recipient_refuses_without_rows() {
    for recipient in [
        "email:recipient@example.com",
        "telegram:123",
        "khive1:example/00000000-0000-0000-0000-000000000000",
    ] {
        let (registry, runtime, store, _root) = fixture();
        let reference = store.put(b"local file".to_vec()).await.unwrap();
        assert_refusal(
            &registry,
            &runtime,
            json!({"to": recipient, "content": "external file", "attachments": [reference]}),
            "attachments require a local recipient",
        )
        .await;
    }
}

#[tokio::test]
async fn inbound_attachment_failure_rolls_back_both_copies() {
    let (registry, runtime, store, _root) = fixture();
    runtime
        .sql()
        .writer()
        .await
        .unwrap()
        .execute(SqlStatement {
            sql: "CREATE TRIGGER reject_inbound_file BEFORE INSERT ON attachments \
                  WHEN EXISTS (SELECT 1 FROM notes WHERE id = NEW.record_uuid \
                  AND json_extract(properties, '$.direction') = 'inbound') \
                  BEGIN SELECT RAISE(ABORT, 'inbound file fixture refusal'); END"
                .into(),
            params: vec![],
            label: Some("message-attachment-fixture-trigger".into()),
        })
        .await
        .expect("inbound attachment fault trigger");
    let reference = store.put(b"transaction file".to_vec()).await.unwrap();
    let error = registry
        .dispatch_with_identity(
            "comm.send",
            json!({"to": "actor:recipient", "content": "rollback files", "attachments": [reference]}),
            Some(identity("actor:sender")),
        )
        .await
        .expect_err("recipient attachment failure must roll back the full pair");
    assert!(
        error.to_string().contains("inbound file fixture refusal"),
        "{error}"
    );
    assert_eq!(count(&runtime, "notes").await, 0);
    assert_eq!(count(&runtime, "attachments").await, 0);
}

struct FileRoots {
    root: tempfile::TempDir,
    old_import: Option<std::ffi::OsString>,
    old_export: Option<std::ffi::OsString>,
}
impl FileRoots {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().canonicalize().unwrap();
        let old_import = std::env::var_os("KHIVE_IMPORT_FROM_ROOT");
        let old_export = std::env::var_os("KHIVE_SAVE_TO_ROOT");
        std::env::set_var("KHIVE_IMPORT_FROM_ROOT", path.join("imports"));
        std::env::set_var("KHIVE_SAVE_TO_ROOT", path.join("exports"));
        Self {
            root,
            old_import,
            old_export,
        }
    }
    fn export_path(&self, name: &str) -> std::path::PathBuf {
        self.root.path().join("exports").join(name)
    }
}
impl Drop for FileRoots {
    fn drop(&mut self) {
        for (name, old) in [
            ("KHIVE_IMPORT_FROM_ROOT", &self.old_import),
            ("KHIVE_SAVE_TO_ROOT", &self.old_export),
        ] {
            match old {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }
}

async fn attachment_publication_snapshot(runtime: &KhiveRuntime) -> Value {
    let mut reader = runtime
        .sql()
        .reader()
        .await
        .expect("publication snapshot reader");
    let mut rows = Vec::new();
    for sql in [
        "SELECT * FROM notes ORDER BY id",
        "SELECT * FROM attachments ORDER BY record_uuid COLLATE BINARY, role COLLATE BINARY",
        "SELECT * FROM events ORDER BY id",
        "SELECT * FROM event_observations ORDER BY event_id, role, position",
    ] {
        rows.push(
            serde_json::to_value(
                reader
                    .query_all(SqlStatement {
                        sql: sql.into(),
                        params: vec![],
                        label: Some("attachment-refusal-publication-snapshot".into()),
                    })
                    .await
                    .expect("publication snapshot query"),
            )
            .expect("snapshot SQL column values"),
        );
    }
    json!(rows)
}

#[tokio::test]
async fn reply_to_email_ingested_message_refuses_attachments_before_publication() {
    // A local reply must reach the actual attachment publication path first.
    let (local_registry, local_runtime, local_store, _local_root) = fixture();
    let local_root = local_registry.dispatch_with_identity(
        "comm.send",
        json!({"to": "actor:recipient", "content": "local root", "idempotency_key": "local-root"}),
        Some(identity("actor:sender")),
    ).await.expect("local keyed root");
    let local_ref = local_store
        .put(b"local reply file".to_vec())
        .await
        .expect("local reply blob");
    let local_reply = local_registry.dispatch_with_identity(
        "comm.reply",
        json!({"id": local_root["recipient_id"], "content": "local reply", "attachments": [local_ref], "idempotency_key": "local-reply"}),
        Some(identity("actor:recipient")),
    ).await.expect("local reply accepts the actual file");
    assert_eq!(count(&local_runtime, "notes").await, 4);
    assert_eq!(count(&local_runtime, "attachments").await, 2);
    for field in ["full_id", "recipient_id"] {
        let id =
            Uuid::parse_str(local_reply[field].as_str().expect("local reply copy UUID")).unwrap();
        let rows = local_runtime
            .attachments()
            .unwrap()
            .list_attachments(id)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].role, "message-attachment:0");
        assert_eq!(rows[0].content_ref, local_ref);
        assert_eq!(rows[0].size_bytes, Some(16));
    }

    let (registry, runtime, store, _root) = fixture();
    let token = runtime
        .authorize(khive_runtime::Namespace::local())
        .expect("ingest namespace");
    let pack = khive_pack_comm::CommPack::new_with_channel_ingest_capability(
        runtime.clone(),
        khive_runtime::ChannelIngestCapability::grant_for_direct_composition(),
    );
    let recipient = "email:sender@example.com";
    // Channel ingestion is an internal subhandler, invoked by a trusted
    // composition rather than exposed as an ordinary request verb.
    let ingested = khive_runtime::pack::PackRuntime::dispatch(
        &pack,
        "comm.ingest",
        json!({
            "from": recipient, "to": "local", "default_inbound_actor": "actor:recipient",
            "content": "external reply root", "subject": "email root",
            "channel_kind": "email", "channel_slug": "attachment-reply-email",
            "external_id": "imap:attachment-reply-email:1:1",
        }),
        &registry,
        &token,
    )
    .await
    .expect("actual email-channel ingest");
    assert_eq!(ingested["deduplicated"], false);
    let id = Uuid::parse_str(ingested["full_id"].as_str().expect("ingested UUID")).unwrap();
    let original = runtime
        .notes(&token)
        .unwrap()
        .get_note(id)
        .await
        .unwrap()
        .expect("stored inbound root");
    let properties = original
        .properties
        .as_ref()
        .expect("ingested message properties");
    assert_eq!(properties["from_actor"], recipient);
    assert_eq!(properties["to_actor"], "actor:recipient");
    assert_eq!(properties["direction"], "inbound");
    let reference = store
        .put(b"external reply file".to_vec())
        .await
        .expect("external reply blob");
    assert_eq!(store.size(&reference).await.unwrap(), Some(19));
    assert_eq!(count(&runtime, "notes").await, 1);
    assert_eq!(count(&runtime, "attachments").await, 0);
    let before = attachment_publication_snapshot(&runtime).await;
    let result = registry.dispatch_with_identity(
        "comm.reply",
        json!({"id": ingested["full_id"], "content": "reply with a file", "attachments": [reference], "idempotency_key": "external-reply-file"}),
        Some(identity("actor:recipient")),
    ).await;
    assert!(
        result.is_err(),
        "an email reply must refuse its file before publication: {result:?}"
    );
    let error = result.unwrap_err();
    assert!(
        matches!(error.refusal_source(), RuntimeError::InvalidInput(_)),
        "typed attachment policy refusal: {error:?}"
    );
    let rendered = error.to_string();
    assert!(
        rendered.contains("attachments require a local recipient"),
        "{rendered}"
    );
    assert!(
        rendered.contains(recipient),
        "the actual reply recipient must be named: {rendered}"
    );
    assert!(
        !rendered.contains("unknown field"),
        "a parser refusal is not the channel policy: {rendered}"
    );
    assert_eq!(
        attachment_publication_snapshot(&runtime).await,
        before,
        "refusal leaves notes, attachments, keys, versions, events and projections unchanged"
    );
}
