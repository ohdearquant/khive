//! Public attachment publication and selective hydration contracts.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use khive_db::stores::blob::FsBlobStore;
use khive_db::StorageBackend;
use khive_pack_kg::KgPack;
use khive_runtime::{
    BackendId, KhiveRuntime, Namespace, RuntimeConfig, RuntimeError, VerbRegistry,
    VerbRegistryBuilder,
};
use khive_storage::{
    BlobStore, ContentRef, Entity, Note, SqlStatement, StorageCapability, StorageError,
    StorageResult,
};
use serde_json::{json, Value};
use uuid::Uuid;

#[derive(Debug)]
struct ObservedBlobs {
    inner: Arc<dyn BlobStore>,
    puts: AtomicUsize,
    reads: AtomicUsize,
    fail_put: AtomicBool,
    block_size: AtomicBool,
    size_started: tokio::sync::Notify,
    published: Mutex<Vec<ContentRef>>,
}

#[async_trait]
impl BlobStore for ObservedBlobs {
    async fn put(&self, bytes: Vec<u8>) -> StorageResult<ContentRef> {
        self.puts.fetch_add(1, Ordering::SeqCst);
        if self.fail_put.load(Ordering::SeqCst) {
            return Err(StorageError::InvalidInput {
                capability: StorageCapability::Blob,
                operation: "attachment fixture put".into(),
                message: "injected blob publication failure".into(),
            });
        }
        let content_ref = self.inner.put(bytes).await?;
        self.published.lock().unwrap().push(content_ref.clone());
        Ok(content_ref)
    }

    async fn get_bounded_verified(&self, id: &ContentRef, max: u64) -> StorageResult<Vec<u8>> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        self.inner.get_bounded_verified(id, max).await
    }
    async fn exists(&self, id: &ContentRef) -> StorageResult<bool> {
        self.inner.exists(id).await
    }
    async fn size(&self, id: &ContentRef) -> StorageResult<Option<u64>> {
        if self.block_size.load(Ordering::SeqCst) {
            self.size_started.notify_one();
            std::future::pending::<()>().await;
        }
        self.inner.size(id).await
    }
    async fn delete(&self, id: &ContentRef) -> StorageResult<bool> {
        self.inner.delete(id).await
    }
}

struct Fixture {
    runtime: KhiveRuntime,
    registry: VerbRegistry,
    blobs: Arc<ObservedBlobs>,
    _root: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let runtime = KhiveRuntime::memory().expect("private in-memory runtime");
        Self::from_runtime(runtime)
    }

    fn from_runtime(runtime: KhiveRuntime) -> Self {
        assert!(runtime.backend().pool().canonical_path().is_none());
        let root = tempfile::tempdir().unwrap();
        let blobs = Arc::new(ObservedBlobs {
            inner: Arc::new(FsBlobStore::new(root.path().join("blobs"), 0).unwrap()),
            puts: AtomicUsize::new(0),
            reads: AtomicUsize::new(0),
            fail_put: AtomicBool::new(false),
            block_size: AtomicBool::new(false),
            size_started: tokio::sync::Notify::new(),
            published: Mutex::new(Vec::new()),
        });
        runtime.install_blob_store(blobs.clone()).unwrap();
        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(runtime.clone()));
        let registry = builder.build().unwrap();
        Self {
            runtime,
            registry,
            blobs,
            _root: root,
        }
    }

    async fn seed(&self, note: bool, entity_type: Option<&str>) -> Uuid {
        let token = self.runtime.authorize(Namespace::local()).unwrap();
        if note {
            let mut note = Note::new("local", "observation", "attachment fixture");
            note.key = Some(format!("attachment-{}", note.id));
            let id = note.id;
            self.runtime
                .notes(&token)
                .unwrap()
                .upsert_note(note)
                .await
                .unwrap();
            id
        } else {
            let mut entity = Entity::new("local", "document", "Attachment fixture");
            entity.entity_type = entity_type.map(str::to_owned);
            let id = entity.id;
            self.runtime
                .entities(&token)
                .unwrap()
                .upsert_entity(entity)
                .await
                .unwrap();
            id
        }
    }

    async fn attach(&self, id: Uuid, role: &str, bytes: &[u8]) -> Result<Value, RuntimeError> {
        self.registry.dispatch("attach", json!({
            "id": id, "role": role, "bytes": BASE64.encode(bytes), "media_type": "text/markdown"
        })).await
    }

    async fn sql(&self, sql: &str) {
        self.runtime
            .sql()
            .writer()
            .await
            .unwrap()
            .execute_script(sql.to_owned())
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn secondary_publication_is_unsupported_before_blob_put_or_attachment_writes() {
    let main = Arc::new(StorageBackend::memory().unwrap());
    let secondary = Arc::new(StorageBackend::memory().unwrap());
    main.prepare_core_schema().unwrap();
    secondary.prepare_core_schema().unwrap();
    let config = RuntimeConfig {
        db_path: None,
        backend_id: BackendId::parse("secondary").unwrap(),
        actor_id: None,
        packs: vec!["kg".to_owned()],
        ..RuntimeConfig::no_embeddings()
    };
    let runtime = KhiveRuntime::from_backend(secondary, config).with_core_backend(main);
    let f = Fixture::from_runtime(runtime);
    let id = f.seed(false, None).await;
    let token = f.runtime.authorize(Namespace::local()).unwrap();
    assert!(f.runtime.resolve_by_id(&token, id).await.unwrap().is_some());
    let error = f
        .attach(id, "markdown", b"valid secondary owner")
        .await
        .unwrap_err();
    assert!(
        matches!(error, RuntimeError::Storage(StorageError::Unsupported {
        capability: StorageCapability::Attachments, ref operation, ref message,
    }) if operation == "attach" && message.contains("secondary") && message.contains("main")),
        "{error}"
    );
    assert_eq!(f.blobs.puts.load(Ordering::SeqCst), 0);
    assert!(f
        .runtime
        .core()
        .attachments()
        .unwrap()
        .list_attachments(id)
        .await
        .unwrap()
        .is_empty());
    assert!(f
        .runtime
        .backend()
        .attachments()
        .unwrap()
        .list_attachments(id)
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn entity_and_note_round_trip_metadata_selected_bytes_and_role_replacement() {
    for note in [false, true] {
        let f = Fixture::new();
        let id = f.seed(note, None).await;
        let bytes = b"# ADR rendition\n\0\xff";
        let first = f.attach(id, "markdown", bytes).await.unwrap();
        assert_eq!(first["id"], id.to_string());
        assert_eq!(first["replaced"], false);
        assert_eq!(first["size"], bytes.len());
        f.attach(id, "text", b"plain rendition").await.unwrap();
        let metadata = f.registry.dispatch("get", json!({"id": id})).await.unwrap();
        let roles = metadata["attachments"].as_object().expect("role-keyed map");
        assert_eq!(roles.len(), 2);
        assert_eq!(roles["markdown"]["content_ref"], first["content_ref"]);
        assert_eq!(roles["markdown"]["media_type"], "text/markdown");
        assert!(roles.values().all(|entry| entry.get("bytes").is_none()));
        assert_eq!(
            f.blobs.reads.load(Ordering::SeqCst),
            0,
            "metadata must not hydrate"
        );
        let hydrated = f
            .registry
            .dispatch("get", json!({"id": id, "hydrate": ["markdown"]}))
            .await
            .unwrap();
        assert_eq!(
            BASE64
                .decode(
                    hydrated["attachments"]["markdown"]["bytes"]
                        .as_str()
                        .unwrap()
                )
                .unwrap(),
            bytes
        );
        assert_eq!(
            hydrated["attachments"]["markdown"]["content_ref"],
            first["content_ref"]
        );
        assert!(hydrated["attachments"]["text"].get("bytes").is_none());
        assert_eq!(f.blobs.reads.load(Ordering::SeqCst), 1);
        let replacement = f.attach(id, "markdown", b"replacement").await.unwrap();
        assert_eq!(replacement["replaced"], true);
        assert_ne!(replacement["content_ref"], first["content_ref"]);
        let rows = f
            .runtime
            .attachments()
            .unwrap()
            .list_attachments(id)
            .await
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows.iter().filter(|row| row.role == "markdown").count(), 1);
        assert_eq!(
            rows.iter()
                .find(|row| row.role == "markdown")
                .unwrap()
                .content_ref
                .as_str(),
            replacement["content_ref"].as_str().unwrap()
        );
    }
}

#[tokio::test]
async fn attachment_row_failure_after_put_publishes_no_row_and_failed_replace_keeps_prior() {
    let f = Fixture::new();
    let id = f.seed(false, None).await;
    f.sql("CREATE TRIGGER refuse_attachment BEFORE INSERT ON attachments BEGIN SELECT RAISE(ABORT, 'injected attachment row refusal'); END").await;
    let error = f
        .attach(id, "markdown", b"published bytes, refused row")
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("injected attachment row refusal"),
        "{error}"
    );
    assert_eq!(f.blobs.puts.load(Ordering::SeqCst), 1);
    let published = f.blobs.published.lock().unwrap()[0].clone();
    assert!(
        f.blobs.exists(&published).await.unwrap(),
        "put truly completed before row refusal"
    );
    assert!(f
        .runtime
        .attachments()
        .unwrap()
        .list_attachments(id)
        .await
        .unwrap()
        .is_empty());

    f.sql("DROP TRIGGER refuse_attachment").await;
    let prior = f
        .attach(id, "markdown", b"prior committed rendition")
        .await
        .unwrap();
    let prior_rows = f
        .runtime
        .attachments()
        .unwrap()
        .list_attachments(id)
        .await
        .unwrap();
    f.sql("CREATE TRIGGER refuse_attachment_update BEFORE UPDATE ON attachments BEGIN SELECT RAISE(ABORT, 'injected attachment replacement refusal'); END").await;
    let error = f
        .attach(id, "markdown", b"replacement refused")
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("injected attachment replacement refusal"),
        "{error}"
    );
    assert_eq!(
        f.runtime
            .attachments()
            .unwrap()
            .list_attachments(id)
            .await
            .unwrap(),
        prior_rows
    );
    let after = f
        .registry
        .dispatch("get", json!({"id": id, "hydrate": ["markdown"]}))
        .await
        .unwrap();
    assert_eq!(
        after["attachments"]["markdown"]["content_ref"],
        prior["content_ref"]
    );
    assert_eq!(
        after["attachments"]["markdown"]["bytes"],
        BASE64.encode(b"prior committed rendition")
    );
}

#[tokio::test]
async fn blob_publication_failure_does_not_commit_an_attachment_row() {
    let f = Fixture::new();
    let id = f.seed(false, None).await;
    f.blobs.fail_put.store(true, Ordering::SeqCst);
    let error = f
        .attach(id, "markdown", b"refused bytes")
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("injected blob publication failure"),
        "{error}"
    );
    assert_eq!(f.blobs.puts.load(Ordering::SeqCst), 1);
    assert!(f.blobs.published.lock().unwrap().is_empty());
    assert!(f
        .runtime
        .attachments()
        .unwrap()
        .list_attachments(id)
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn missing_record_unknown_hydrate_base64_and_authenticated_roles_name_the_refusal() {
    let f = Fixture::new();
    let missing = Uuid::new_v4();
    let error = f.attach(missing, "markdown", b"body").await.unwrap_err();
    assert!(error.to_string().contains(&missing.to_string()), "{error}");
    let id = f.seed(false, None).await;
    let error = f
        .registry
        .dispatch(
            "attach",
            json!({"id": id, "role": "markdown", "bytes": "%%%"}),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("base64"), "{error}");
    let error = f
        .registry
        .dispatch("get", json!({"id": id, "hydrate": ["missing-role"]}))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("missing-role"), "{error}");
    for (entity_type, role) in [
        (Some("moodboard_model"), "fann-network"),
        (Some("visual_asset"), "content"),
        (Some("moodboard_model"), "content"),
    ] {
        let owned = f.seed(false, entity_type).await;
        let error = f
            .attach(owned, role, b"unauthenticated replacement")
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("moodboard") && error.to_string().contains(role),
            "{error}"
        );
        assert!(f
            .runtime
            .attachments()
            .unwrap()
            .list_attachments(owned)
            .await
            .unwrap()
            .is_empty());
    }
    assert_eq!(
        f.blobs.puts.load(Ordering::SeqCst),
        0,
        "all refusals precede put"
    );
    assert_eq!(f.blobs.reads.load(Ordering::SeqCst), 0);
    let ordinary = f
        .attach(id, "content", b"ordinary document body")
        .await
        .unwrap();
    assert_eq!(
        ordinary["role"], "content",
        "generic document content remains available"
    );
    let ordinary = f
        .attach(id, "fann-network", b"ordinary rendition")
        .await
        .unwrap();
    assert_eq!(
        ordinary["role"], "fann-network",
        "roles are not globally reserved"
    );
}

#[tokio::test]
async fn hydrate_checks_stored_size_and_cancels_a_legacy_stat() {
    let f = Fixture::new();
    let id = f.seed(false, None).await;
    f.attach(id, "markdown", b"body").await.unwrap();
    f.sql("UPDATE attachments SET size_bytes = 9").await;
    let error = f
        .registry
        .dispatch("get", json!({"id": id, "hydrate": ["markdown"]}))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("stored size"), "{error}");
    f.sql("UPDATE attachments SET size_bytes = NULL").await;
    let legacy = f
        .registry
        .dispatch("get", json!({"id": id, "hydrate": ["markdown"]}))
        .await
        .unwrap();
    assert_eq!(
        legacy["attachments"]["markdown"]["bytes"],
        BASE64.encode(b"body")
    );
    f.blobs.block_size.store(true, Ordering::SeqCst);
    let (cancel, receiver) = tokio::sync::watch::channel(false);
    let read = khive_storage::scope_request_read_cancellation(
        receiver,
        f.registry
            .dispatch("get", json!({"id": id, "hydrate": ["markdown"]})),
    );
    tokio::pin!(read);
    tokio::select! {
        result = &mut read => panic!("read finished before blocked stat: {result:?}"),
        () = f.blobs.size_started.notified() => {}
    }
    // Start the test watchdog only after real fixture I/O and the blocked stat.
    // Virtual time makes a broken cancellation path fail rather than hang.
    tokio::time::pause();
    cancel.send(true).unwrap();
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), read)
        .await
        .expect("cancelled attachment stat must settle");
    assert!(
        matches!(result, Err(RuntimeError::Storage(StorageError::Timeout { ref operation }))
        if operation == "attachment_blob_size"),
        "{result:?}"
    );
    assert_eq!(
        f.blobs.reads.load(Ordering::SeqCst),
        2,
        "cancelled stat must not begin a third body read"
    );
}

#[tokio::test]
async fn soft_delete_preserves_attachment_liveness_and_hard_delete_removes_it() {
    for note in [false, true] {
        let f = Fixture::new();
        let id = f.seed(note, None).await;
        let attached = f
            .attach(id, "markdown", b"keep while recoverable")
            .await
            .unwrap();
        let reference = ContentRef::from_hex(attached["content_ref"].as_str().unwrap()).unwrap();
        let before = f
            .runtime
            .attachments()
            .unwrap()
            .list_attachments(id)
            .await
            .unwrap();
        assert_eq!(before.len(), 1);
        f.registry
            .dispatch("delete", json!({"id": id}))
            .await
            .unwrap();
        assert_eq!(
            f.runtime
                .attachments()
                .unwrap()
                .list_attachments(id)
                .await
                .unwrap(),
            before
        );
        let deleted = f
            .registry
            .dispatch(
                "get",
                json!({"id": id, "include_deleted": true, "hydrate": ["markdown"]}),
            )
            .await
            .unwrap();
        assert_eq!(
            deleted["attachments"]["markdown"]["bytes"],
            BASE64.encode(b"keep while recoverable")
        );
        f.registry
            .dispatch("delete", json!({"id": id, "hard": true}))
            .await
            .unwrap();
        assert!(f
            .runtime
            .attachments()
            .unwrap()
            .list_attachments(id)
            .await
            .unwrap()
            .is_empty());
        assert!(
            f.blobs.exists(&reference).await.unwrap(),
            "hard delete releases the row, not bytes inline"
        );
    }
}

#[tokio::test]
async fn current_epoch_sweep_refuses_before_reporting_attached_or_deleted_blobs() {
    let f = Fixture::new();
    let id = f.seed(false, None).await;
    let attached = f
        .attach(id, "markdown", b"current schema GC witness")
        .await
        .unwrap();
    let reference = ContentRef::from_hex(attached["content_ref"].as_str().unwrap()).unwrap();
    let mut reader = f.runtime.sql().reader().await.unwrap();
    let epoch = reader
        .query_scalar(SqlStatement::new(
            "SELECT MAX(version) FROM _schema_migrations",
            vec![],
        ))
        .await
        .unwrap();
    assert!(matches!(epoch, Some(khive_storage::SqlValue::Integer(version)) if version > 21));
    drop(reader);
    for hard_deleted in [false, true] {
        if hard_deleted {
            f.registry
                .dispatch("delete", json!({"id": id, "hard": true}))
                .await
                .unwrap();
        }
        for dry_run in [true, false] {
            let error = f
                .blobs
                .inner
                .transactional_orphan_sweep(f.runtime.sql().as_ref(), dry_run)
                .await
                .unwrap_err();
            assert!(
                matches!(
                    error,
                    StorageError::Unsupported {
                        capability: StorageCapability::Blob,
                        ..
                    }
                ),
                "dry_run={dry_run}, hard_deleted={hard_deleted}: {error}"
            );
            assert!(error.to_string().contains("V21"));
            assert!(f.blobs.exists(&reference).await.unwrap());
        }
    }
}
