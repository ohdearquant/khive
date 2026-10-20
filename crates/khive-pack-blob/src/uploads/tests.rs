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
    let mut manager = UploadManager::new(runtime);
    manager.policy = UploadPolicy {
        idle_for: IDLE,
        sweep_interval: Duration::from_secs(600),
        max_active: 128,
        max_per_actor: 16,
    };
    manager
}

fn durable_runtime(dir: &Path) -> KhiveRuntime {
    let backend = Arc::new(khive_db::StorageBackend::sqlite_for_test(dir.join("main.db")).unwrap());
    backend.prepare_core_schema().unwrap();
    KhiveRuntime::from_backend(backend, RuntimeConfig::no_embeddings())
}

fn staged_count(root: &Path) -> usize {
    std::fs::read_dir(root.join(".uploads"))
        .unwrap()
        .filter(|entry| {
            UploadId::from_hex(
                entry
                    .as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .as_ref(),
            )
            .is_ok()
        })
        .count()
}

fn lease(root: &Path, id: &UploadId) -> Value {
    serde_json::from_slice(&std::fs::read(staged(root, id).with_extension("lease")).unwrap())
        .unwrap()
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("blobs");
    let store = Arc::new(FsBlobStore::new(root.clone(), 0).unwrap());
    let manager = manager_with_store(durable_runtime(dir.path()), store.clone());
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

fn assert_upload_ceiling(error: RuntimeError, ceiling: &str, limit: usize) {
    match error {
        RuntimeError::InvalidInput(message) => assert_eq!(
            message,
            format!("blob.begin: {ceiling} active-upload ceiling of {limit} reached")
        ),
        other => panic!("expected named InvalidInput upload ceiling, got {other}"),
    }
}

include!("tests/ceiling_parser_and_idle_policy_tests.rs");

#[tokio::test]
async fn main_identity_is_authoritative_and_memory_refuses_before_staging() {
    let f = fixture();
    let main_path = f._dir.path().join("lease-owner-main.db");
    let main = khive_db::StorageBackend::sqlite_for_test(&main_path).unwrap();
    main.prepare_core_schema().unwrap();
    let other_path = f._dir.path().join("secondary.db");
    let other = Arc::new(khive_db::StorageBackend::sqlite_for_test(&other_path).unwrap());
    other.prepare_core_schema().unwrap();
    let owner = main.database_owner_identity().unwrap().durable_id();
    assert_ne!(owner, other.database_owner_identity().unwrap().durable_id());
    let pool = main.pool_arc();
    assert!(!pool.writer_task_join_was_stored());
    let lifetime = Arc::downgrade(&pool);
    drop(pool);
    drop(main);
    assert!(lifetime.upgrade().is_none());
    let settled = rusqlite::Connection::open(&main_path).unwrap();
    let mode: String = settled
        .pragma_update_and_check(None, "journal_mode", "DELETE", |row| row.get(0))
        .unwrap();
    assert_eq!(mode.to_ascii_lowercase(), "delete");
    settled.close().unwrap();
    let readonly =
        Arc::new(khive_db::StorageBackend::sqlite_read_only_for_test(&main_path).unwrap());
    assert!(readonly.is_read_only());
    let acquisitions = readonly.pool().writer_acquisition_snapshot();
    let runtime = KhiveRuntime::from_backend(
        other,
        RuntimeConfig {
            db_path: Some(other_path),
            backend_id: khive_runtime::config::BackendId::parse("assets").unwrap(),
            ..RuntimeConfig::no_embeddings()
        },
    )
    .with_core_backend(readonly.clone());
    assert!(!runtime.is_read_only() && runtime.core().is_read_only());
    let manager = manager_with_store(runtime, f.store.clone());
    assert!(Arc::ptr_eq(
        &manager.runtime.require_blob_store().unwrap(),
        &manager.runtime.require_blob_store().unwrap()
    ));
    let id = begin(&manager, 0).await;
    assert_eq!(lease(&f.root, &id)["owner"], owner.to_string());
    manager.abort(&id).await.unwrap();
    assert_eq!(readonly.pool().writer_acquisition_snapshot(), acquisitions);
    assert_eq!(
        readonly.database_owner_identity().unwrap().durable_id(),
        owner
    );
    let memory = manager_with_store(KhiveRuntime::memory().unwrap(), f.store.clone());
    let known = f.store.put(b"known".to_vec()).await.unwrap();
    assert_eq!(
        memory
            .begin(5, Some(known.clone()), "actor".into())
            .await
            .unwrap()["content_ref"],
        known.to_string()
    );
    let error = memory.begin(0, None, "actor".into()).await.unwrap_err();
    assert!(error
        .to_string()
        .contains("durable MAIN database for staged filesystem uploads"));
    assert_eq!(staged_count(&f.root), 0);
    assert!(std::fs::read_dir(f.root.join(".uploads"))
        .unwrap()
        .next()
        .is_none());
    assert!(
        memory.records.lock().unwrap().is_empty()
            && memory.slots.by_actor.lock().unwrap().is_empty()
    );
}

fn memory_fs_registry(
    f: &Fixture,
    transfers: bool,
) -> (khive_runtime::VerbRegistry, Arc<UploadManager>) {
    let mut config = RuntimeConfig::no_embeddings();
    config.blob.file_transfers = transfers;
    let runtime = KhiveRuntime::from_backend(
        Arc::new(khive_db::StorageBackend::memory().unwrap()),
        config,
    );
    runtime.install_blob_store(f.store.clone()).unwrap();
    let pack = BlobPack::new(runtime);
    let manager = pack.uploads.clone();
    let mut builder = VerbRegistryBuilder::new();
    builder.register(pack);
    (builder.build().unwrap(), manager)
}

fn assert_memory_durability_refusal(error: RuntimeError, manager: &UploadManager, root: &Path) {
    assert!(matches!(error, RuntimeError::Unconfigured(_)), "{error}");
    assert_eq!(
        error.to_string(),
        "unconfigured: durable MAIN database for staged filesystem uploads (identity \
         unavailable: an in-memory database cannot own a durable filesystem root) is not set"
    );
    assert!(!root.join(".uploads").exists());
    assert!(manager.records.lock().unwrap().is_empty());
    assert!(manager.slots.by_actor.lock().unwrap().is_empty());
}

#[tokio::test]
async fn public_begin_memory_main_reports_unconfigured_before_staging() {
    let f = fixture();
    let (registry, manager) = memory_fs_registry(&f, false);
    let error = registry
        .dispatch("blob.begin", json!({"size": 0}))
        .await
        .unwrap_err();
    assert_memory_durability_refusal(error, &manager, &f.root);
}

#[tokio::test]
async fn public_import_memory_main_reports_unconfigured_before_staging() {
    if khive_storage::test_support::run_exact_test_in_child(
        "KHIVE_MEMORY_IMPORT_CHILD",
        false,
        |_| {},
    ) {
        return;
    }
    let f = fixture();
    let root = f._dir.path().canonicalize().unwrap();
    std::fs::create_dir(root.join("imports")).unwrap();
    std::fs::create_dir(root.join("exports")).unwrap();
    std::env::set_var("KHIVE_IMPORT_FROM_ROOT", root.join("imports"));
    std::env::set_var("KHIVE_SAVE_TO_ROOT", root.join("exports"));
    let bytes = b"valid confined import";
    std::fs::write(root.join("imports/source.bin"), bytes).unwrap();
    let (registry, manager) = memory_fs_registry(&f, true);
    let error = registry
        .dispatch("blob.import", json!({"path": "source.bin"}))
        .await
        .unwrap_err();
    assert_memory_durability_refusal(error, &manager, &f.root);
    assert_eq!(
        std::fs::read(root.join("imports/source.bin")).unwrap(),
        bytes
    );
    assert!(!f.store.exists(&content_ref(bytes)).await.unwrap());
}

include!("tests/active_upload_ceiling_tests.rs");

include!("tests/begin_reservation_and_cleanup_tests.rs");

include!("tests/part_index_and_tail_retry_tests.rs");

include!("tests/commit_and_size_limit_tests.rs");

include!("tests/decoded_part_limit_tests.rs");

include!("tests/known_reference_and_expiry_tests.rs");

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
    let readonly = manager_with_store(runtime, f.store.clone());
    readonly
        .records
        .lock()
        .unwrap()
        .insert(id.clone(), f.manager.record(&id).unwrap());
    let before = std::fs::read(staged(&f.root, &id).with_extension("lease")).unwrap();
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
    assert_eq!(
        std::fs::read(staged(&f.root, &id).with_extension("lease")).unwrap(),
        before
    );
    assert!(readonly.records.lock().unwrap().contains_key(&id));
    assert!(!f.store.exists(&content_ref(b"a")).await.unwrap());
    f.manager.abort(&id).await.unwrap();
}

#[derive(Debug)]
struct ControlledStore {
    inner: Arc<FsBlobStore>,
    begin_calls: AtomicUsize,
    begin_failures: AtomicUsize,
    pause_after_begin: AtomicBool,
    begun_ids: Mutex<Vec<UploadId>>,
    begun: tokio::sync::Notify,
    release_begin: tokio::sync::Notify,
    abort_failures: AtomicUsize,
    abort_calls: AtomicUsize,
    pause_after_append: AtomicBool,
    fail_after_append: AtomicBool,
    wrong_append_length: AtomicBool,
    appended: tokio::sync::Notify,
    fail_renew: AtomicBool,
    pause_renew: AtomicBool,
    renewing: tokio::sync::Notify,
}

impl ControlledStore {
    fn new(inner: Arc<FsBlobStore>) -> Self {
        Self {
            inner,
            begin_calls: AtomicUsize::new(0),
            begin_failures: AtomicUsize::new(0),
            pause_after_begin: AtomicBool::new(false),
            begun_ids: Mutex::new(Vec::new()),
            begun: tokio::sync::Notify::new(),
            release_begin: tokio::sync::Notify::new(),
            abort_failures: AtomicUsize::new(0),
            abort_calls: AtomicUsize::new(0),
            pause_after_append: AtomicBool::new(false),
            fail_after_append: AtomicBool::new(false),
            wrong_append_length: AtomicBool::new(false),
            appended: tokio::sync::Notify::new(),
            fail_renew: AtomicBool::new(false),
            pause_renew: AtomicBool::new(false),
            renewing: tokio::sync::Notify::new(),
        }
    }
}

#[async_trait]
impl BlobStore for ControlledStore {
    async fn put(&self, bytes: Vec<u8>) -> StorageResult<ContentRef> {
        self.inner.put(bytes).await
    }

    fn upload_lease_idle_cap(&self) -> Option<Duration> {
        self.inner.upload_lease_idle_cap()
    }

    async fn renew_upload(&self, id: &UploadId) -> StorageResult<()> {
        if self.pause_renew.load(Ordering::SeqCst) {
            self.renewing.notify_one();
            std::future::pending::<()>().await;
        }
        if self.fail_renew.load(Ordering::SeqCst) {
            return Err(StorageError::Internal("injected renewal failure".into()));
        }
        self.inner.renew_upload(id).await
    }

    async fn begin_upload_with_lease(
        &self,
        size: u64,
        config: UploadLeaseConfig,
    ) -> StorageResult<UploadId> {
        self.begin_calls.fetch_add(1, Ordering::SeqCst);
        if self
            .begin_failures
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
        {
            return Err(StorageError::Internal("injected begin failure".into()));
        }
        let id = self.inner.begin_upload_with_lease(size, config).await?;
        self.begun_ids.lock().unwrap().push(id.clone());
        if self.pause_after_begin.load(Ordering::SeqCst) {
            self.begun.notify_one();
            self.release_begin.notified().await;
        }
        Ok(id)
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

include!("tests/append_and_cleanup_failure_tests.rs");

include!("tests/capability_actor_tests.rs");

#[tokio::test]
async fn transactional_gc_preserves_committed_and_staging_then_upload_sweep_only_reaps_staging() {
    let f = fixture();
    let backend = khive_db::StorageBackend::sqlite_for_test(f._dir.path().join("gc.db")).unwrap();
    {
        backend
            .pool()
            .writer()
            .unwrap()
            .autocommit_sqlite(|conn| {
                conn.execute_batch(include_str!(
                    "../../../khive-db/sql/schema-migrations-table.sql"
                ))
            })
            .unwrap();
        for migration in khive_db::MIGRATIONS
            .iter()
            .filter(|migration| migration.version <= 20)
        {
            backend
                .pool()
                .writer()
                .unwrap()
                .transaction(|conn| {
                    conn.execute_batch(migration.up).unwrap();
                    conn.execute(
                        "INSERT INTO _schema_migrations (version, name, applied_at) VALUES (?1, ?2, 0)",
                        (migration.version, migration.name),
                    )
                    .unwrap();
                    Ok(())
                })
                .unwrap();
        }
    }
    let owner = khive_db::stores::blob::acquire_database_gc_owner(backend.sql().as_ref())
        .await
        .unwrap();
    backend.stage_attachment_cutover(&owner).unwrap();
    backend.finalize_attachment_cutover(&owner).unwrap();
    assert_eq!(
        backend.schema_version().unwrap(),
        khive_db::migrations::ATTACHMENT_CUTOVER_VERSION
    );
    drop(owner);
    let reference = f.store.put(b"committed control".to_vec()).await.unwrap();
    {
        backend
            .pool()
            .writer()
            .unwrap()
            .autocommit(|conn| {
                conn.execute_batch(
                    "INSERT INTO entities \
                 (id, namespace, kind, name, tags, created_at, updated_at, deleted_at) \
                 VALUES ('11111111-1111-4111-8111-111111111111', 'local', 'document', \
                         'live blob control', '[]', 1, 1, NULL);",
                )
                .unwrap();
                conn.execute(
                    "INSERT INTO attachments \
                 (record_uuid, substrate, role, content_ref, created_at) \
                 VALUES ('11111111-1111-4111-8111-111111111111', \
                         'entity', 'content', ?1, 1)",
                    [reference.as_str()],
                )
                .unwrap();
                Ok(())
            })
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

/// The verb ceiling is the storage whole-buffer ceiling. Callers that bound
/// their inputs by `khive_storage::blob::MAX_BLOB_WHOLE_BYTES`, such as the
/// email channel's message limit, rely on `blob.put` accepting exactly that
/// many bytes.
#[test]
fn verb_object_ceiling_equals_the_storage_whole_buffer_ceiling() {
    assert_eq!(MAX_OBJECT_BYTES, khive_storage::blob::MAX_BLOB_WHOLE_BYTES);
}
