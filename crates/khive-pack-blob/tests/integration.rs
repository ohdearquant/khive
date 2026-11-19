//! End-to-end smoke test for the blob pack: put -> stat -> get round trip
//! through the `VerbRegistry` dispatch path, mirroring
//! `khive-pack-template/tests/integration.rs`.

use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use khive_db::stores::blob::FsBlobStore;
use khive_pack_blob::BlobPack;
use khive_runtime::{KhiveRuntime, RuntimeError, VerbRegistry, VerbRegistryBuilder};
use khive_storage::{BlobStore, ContentRef, StorageError, StorageResult};
use khive_types::Pack;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

#[derive(Debug)]
struct BoundedOnlyBlobStore {
    bytes: Vec<u8>,
    content_ref: ContentRef,
    reported_size: u64,
    bounded_calls: AtomicUsize,
    entered: Option<tokio::sync::mpsc::UnboundedSender<()>>,
    release: Option<Arc<tokio::sync::Semaphore>>,
}

impl BoundedOnlyBlobStore {
    fn new(bytes: Vec<u8>) -> Self {
        let content_ref = ContentRef::from_digest_bytes(blake3::hash(&bytes).as_bytes());
        let reported_size = bytes.len() as u64;
        Self {
            bytes,
            content_ref,
            reported_size,
            bounded_calls: AtomicUsize::new(0),
            entered: None,
            release: None,
        }
    }

    fn underreporting(bytes: Vec<u8>, reported_size: u64) -> Self {
        assert!(reported_size < bytes.len() as u64);
        let mut store = Self::new(bytes);
        store.reported_size = reported_size;
        store
    }

    fn gated(
        bytes: Vec<u8>,
        entered: tokio::sync::mpsc::UnboundedSender<()>,
        release: Arc<tokio::sync::Semaphore>,
    ) -> Self {
        let mut store = Self::new(bytes);
        store.entered = Some(entered);
        store.release = Some(release);
        store
    }
}

#[async_trait]
impl BlobStore for BoundedOnlyBlobStore {
    async fn put(&self, _bytes: Vec<u8>) -> StorageResult<ContentRef> {
        Err(StorageError::Internal("put is not used".to_string()))
    }

    async fn get_bounded_verified(
        &self,
        content_ref: &ContentRef,
        max_bytes: u64,
    ) -> StorageResult<Vec<u8>> {
        self.bounded_calls.fetch_add(1, Ordering::SeqCst);
        assert_eq!(content_ref, &self.content_ref);
        assert_eq!(max_bytes, self.reported_size);
        if let Some(entered) = &self.entered {
            entered.send(()).expect("test receiver remains open");
        }
        if let Some(release) = &self.release {
            let permit = release.acquire().await.expect("test gate remains open");
            permit.forget();
        }
        if self.bytes.len() as u64 > max_bytes {
            return Err(StorageError::BlobTooLarge {
                content_ref: content_ref.clone(),
                max_bytes,
                observed_at_least: self.bytes.len() as u64,
            });
        }
        Ok(self.bytes.clone())
    }

    async fn exists(&self, content_ref: &ContentRef) -> StorageResult<bool> {
        Ok(content_ref == &self.content_ref)
    }

    async fn size(&self, content_ref: &ContentRef) -> StorageResult<Option<u64>> {
        Ok((content_ref == &self.content_ref).then_some(self.reported_size))
    }

    async fn delete(&self, _content_ref: &ContentRef) -> StorageResult<bool> {
        Err(StorageError::Internal("delete is not used".to_string()))
    }
}

fn build_registry() -> (VerbRegistry, KhiveRuntime, tempfile::TempDir) {
    build_registry_with_runtime(KhiveRuntime::memory().expect("in-memory runtime"))
}

fn build_registry_with_runtime(
    runtime: KhiveRuntime,
) -> (VerbRegistry, KhiveRuntime, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend =
        Arc::new(khive_db::StorageBackend::sqlite_for_test(dir.path().join("main.db")).unwrap());
    backend.prepare_core_schema().unwrap();
    let runtime = KhiveRuntime::from_backend(backend, runtime.config().clone());
    let store = FsBlobStore::new(dir.path().to_path_buf(), 0).expect("fs blob store");
    runtime
        .install_blob_store(std::sync::Arc::new(store))
        .expect("install blob store");

    let mut builder = VerbRegistryBuilder::new();
    builder.register(BlobPack::new(runtime.clone()));
    let registry = builder.build().expect("registry builds");
    (registry, runtime, dir)
}

fn build_bounded_registry(store: Arc<BoundedOnlyBlobStore>) -> VerbRegistry {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    runtime
        .install_blob_store(store as Arc<dyn BlobStore>)
        .expect("install blob store");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(BlobPack::new(runtime));
    builder.build().expect("registry builds")
}

include!("integration_tests/metadata_and_roundtrip_tests.rs");

include!("integration_tests/bounded_hydration_tests.rs");

include!("integration_tests/range_and_lookup_tests.rs");

include!("integration_tests/resource_ceiling_tests.rs");

include!("integration_tests/range_validation_tests.rs");

include!("integration_tests/unknown_argument_tests.rs");
