//! Shared hydration admission through the public web.extract dispatch path.

use super::seed_page;
use async_trait::async_trait;
use khive_runtime::{KhiveRuntime, Namespace, RuntimeConfig, RuntimeError, VerbRegistryBuilder};
use khive_storage::{
    BlobStore, ContentRef, EdgeRelation, StorageError, StorageResult, MAX_BLOB_WHOLE_BYTES,
};
use serde_json::json;
use std::future::{poll_fn, Future};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::Poll;
use std::time::Duration;
use tokio::sync::{Notify, Semaphore};

#[derive(Clone, Copy, Debug)]
enum ReadFailure {
    Digest,
    TooLarge,
}

#[derive(Debug)]
struct ObservedBlobStore {
    inner: Arc<dyn BlobStore>,
    reads: AtomicUsize,
    puts: AtomicUsize,
    fail_next_read: Mutex<Option<ReadFailure>>,
    block_next_put: AtomicBool,
    put_started: Notify,
    put_release: Semaphore,
}

#[async_trait]
impl BlobStore for ObservedBlobStore {
    async fn put(&self, bytes: Vec<u8>) -> StorageResult<ContentRef> {
        self.puts.fetch_add(1, Ordering::SeqCst);
        if self.block_next_put.swap(false, Ordering::SeqCst) {
            self.put_started.notify_one();
            self.put_release.acquire().await.unwrap().forget();
        }
        self.inner.put(bytes).await
    }

    async fn get_bounded_verified(
        &self,
        content_ref: &ContentRef,
        max_bytes: u64,
    ) -> StorageResult<Vec<u8>> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        assert_eq!(
            max_bytes, MAX_BLOB_WHOLE_BYTES,
            "preserve the whole-body bound"
        );
        let failure = self.fail_next_read.lock().unwrap().take();
        match failure {
            Some(ReadFailure::Digest) => Err(StorageError::BlobDigestMismatch {
                expected: content_ref.clone(),
                actual: ContentRef::from_digest_bytes(blake3::hash(b"corrupt body").as_bytes()),
            }),
            Some(ReadFailure::TooLarge) => Err(StorageError::BlobTooLarge {
                content_ref: content_ref.clone(),
                max_bytes,
                observed_at_least: max_bytes + 1,
            }),
            None => {
                self.inner
                    .get_bounded_verified(content_ref, max_bytes)
                    .await
            }
        }
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

fn fixture() -> (
    KhiveRuntime,
    khive_runtime::VerbRegistry,
    Arc<ObservedBlobStore>,
    tempfile::TempDir,
) {
    let dir = tempfile::tempdir().unwrap();
    let inner = khive_db::stores::blob::FsBlobStore::new(dir.path().to_path_buf(), 0).unwrap();
    let store = Arc::new(ObservedBlobStore {
        inner: Arc::new(inner),
        reads: AtomicUsize::new(0),
        puts: AtomicUsize::new(0),
        fail_next_read: Mutex::new(None),
        block_next_put: AtomicBool::new(false),
        put_started: Notify::new(),
        put_release: Semaphore::new(0),
    });
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        actor_id: None,
        blob_hydration_bytes: MAX_BLOB_WHOLE_BYTES,
        ..RuntimeConfig::no_embeddings()
    })
    .unwrap();
    runtime.install_blob_store(store.clone()).unwrap();
    let registry = registry(&runtime);
    (runtime, registry, store, dir)
}

fn registry(runtime: &KhiveRuntime) -> khive_runtime::VerbRegistry {
    let mut builder = VerbRegistryBuilder::new();
    builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
    builder.register(crate::WebPack::new(runtime.clone()));
    let registry = builder.build().unwrap();
    runtime.install_edge_rules(registry.all_edge_rules());
    registry
}

async fn domain_counts(runtime: &KhiveRuntime) -> [i64; 3] {
    let rows = runtime
        .sql()
        .reader()
        .await
        .unwrap()
        .query_all(khive_storage::types::SqlStatement {
            sql: "SELECT (SELECT COUNT(*) FROM entities) AS entities, \
                  (SELECT COUNT(*) FROM graph_edges) AS edges, \
                  (SELECT COUNT(*) FROM notes) AS notes"
                .into(),
            params: vec![],
            label: Some("extract_hydration_domain_counts".into()),
        })
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    ["entities", "edges", "notes"].map(|key| match rows[0].get(key) {
        Some(khive_storage::types::SqlValue::Integer(count)) => *count,
        other => panic!("expected integer {key} count, got {other:?}"),
    })
}

#[tokio::test]
async fn extract_retains_shared_hydration_admission_through_persistence() {
    // The second body forces owned lossy decoding. Both paths must retain the
    // original VerifiedBlob lease, even after parsing has produced derived text.
    for (body, expected) in [
        (b"<p>Hello world</p>".as_slice(), "Hello world"),
        (
            b"<p>Hello \xff world</p>".as_slice(),
            "Hello \u{fffd} world",
        ),
    ] {
        let (runtime, registry, store, _dir) = fixture();
        let token = runtime.authorize(Namespace::local()).unwrap();
        let page_id = seed_page(
            &runtime,
            &token,
            "https://hydration.example.test/page",
            "text/html",
            body,
        )
        .await;
        let content_ref = ContentRef::from_digest_bytes(blake3::hash(body).as_bytes());
        let hydrator = runtime.blob_hydrator().unwrap();
        assert_eq!(hydrator.budget_bytes(), MAX_BLOB_WHOLE_BYTES);

        store.block_next_put.store(true, Ordering::SeqCst);
        let extraction =
            registry.dispatch("web.extract", json!({ "id": page_id, "kinds": ["text"] }));
        tokio::pin!(extraction);
        // Drive the real handler to a deterministic persistence barrier.
        // The timeout is only a deadlock guard, never an admission assertion.
        tokio::time::timeout(Duration::from_secs(5), async {
            tokio::select! {
                result = &mut extraction => panic!("extraction skipped the put barrier: {result:?}"),
                _ = store.put_started.notified() => {},
            }
        })
        .await
        .expect("extraction reaches derived-text persistence");
        assert_eq!(store.reads.load(Ordering::SeqCst), 1);

        let (cancel, cancelled) = tokio::sync::watch::channel(false);
        let competitor = khive_storage::scope_request_read_cancellation(
            cancelled,
            hydrator.hydrate_verified(&content_ref, MAX_BLOB_WHOLE_BYTES),
        );
        tokio::pin!(competitor);
        // A hydrator polls admission before its first backend await. Poll once
        // while the handler is suspended, then cancel: the named phase proves
        // which wait was active without relying on elapsed time or task scheduling.
        poll_fn(|cx| {
            assert!(competitor.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        cancel.send(true).unwrap();
        let error = competitor.await.unwrap_err();
        assert!(
            matches!(error, RuntimeError::Storage(StorageError::Timeout { ref operation })
                if operation == "blob_hydration_admission"),
            "source lease must block the installed hydrator during persistence: {error}"
        );
        assert_eq!(
            store.reads.load(Ordering::SeqCst),
            1,
            "queued cancellation starts no read"
        );

        store.put_release.add_permits(1);
        let reply = tokio::time::timeout(Duration::from_secs(5), &mut extraction)
            .await
            .expect("released extraction completes")
            .unwrap();
        let text_id =
            uuid::Uuid::parse_str(reply["result"]["text"]["id"].as_str().unwrap()).unwrap();
        let text = runtime
            .entities(&token)
            .unwrap()
            .get_entity(text_id)
            .await
            .unwrap()
            .unwrap();
        let text_ref =
            ContentRef::from_hex(text.properties.unwrap()["blob_ref"].as_str().unwrap()).unwrap();
        let text_bytes = store
            .inner
            .get_bounded_verified(&text_ref, MAX_BLOB_WHOLE_BYTES)
            .await
            .unwrap();
        assert_eq!(text_bytes, expected.as_bytes());
        let links = runtime
            .neighbors(
                &token,
                text_id,
                khive_storage::Direction::Out,
                None,
                Some(vec![EdgeRelation::DerivedFrom]),
            )
            .await
            .unwrap();
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].node_id, page_id);

        let next = tokio::time::timeout(
            Duration::from_secs(5),
            hydrator.hydrate_verified(&content_ref, MAX_BLOB_WHOLE_BYTES),
        )
        .await
        .expect("finished extraction releases the shared budget")
        .unwrap();
        assert_eq!(next.bytes(), body);
        assert_eq!(store.reads.load(Ordering::SeqCst), 2);
    }
}

#[tokio::test]
async fn extract_hydration_errors_preserve_refusals_and_release_admission_without_writes() {
    // Inject typed storage failures at the bounded read contract, not a fake
    // parser outcome. The success control still uses the real verified FS store.
    for failure in [ReadFailure::Digest, ReadFailure::TooLarge] {
        let (runtime, registry, store, _dir) = fixture();
        let token = runtime.authorize(Namespace::local()).unwrap();
        let page_id = seed_page(
            &runtime,
            &token,
            "https://hydration.example.test/errors",
            "text/html",
            b"<p>valid body</p><a href='/target'>Target</a>",
        )
        .await;
        let before = domain_counts(&runtime).await;
        let puts_before = store.puts.load(Ordering::SeqCst);
        *store.fail_next_read.lock().unwrap() = Some(failure);
        let error = registry
            .dispatch("web.extract", json!({ "id": page_id }))
            .await
            .unwrap_err();
        match failure {
            ReadFailure::Digest => assert!(matches!(
                error,
                RuntimeError::Storage(StorageError::BlobDigestMismatch { .. })
            )),
            ReadFailure::TooLarge => assert!(matches!(
                error,
                RuntimeError::Storage(StorageError::BlobTooLarge {
                    max_bytes: MAX_BLOB_WHOLE_BYTES,
                    ..
                })
            )),
        }
        assert_eq!(domain_counts(&runtime).await, before);
        assert_eq!(store.puts.load(Ordering::SeqCst), puts_before);
        assert_eq!(store.reads.load(Ordering::SeqCst), 1);

        let reply = tokio::time::timeout(
            Duration::from_secs(5),
            registry.dispatch("web.extract", json!({ "id": page_id })),
        )
        .await
        .expect("failed hydration returns its admission capacity")
        .unwrap();
        assert_eq!(reply["result"]["links"]["edges_created"], 1);
        assert!(reply["result"]["text"]["id"].is_string());
        assert_eq!(store.reads.load(Ordering::SeqCst), 2);
    }
}

#[tokio::test]
async fn extract_without_installed_hydrator_preserves_unconfigured_refusal() {
    let runtime = KhiveRuntime::memory().unwrap();
    let registry = registry(&runtime);
    let token = runtime.authorize(Namespace::local()).unwrap();
    let id = uuid::Uuid::new_v4();
    crate::entities::get_or_create(
        &runtime,
        &token,
        id,
        "document",
        "page",
        "https://hydration.example.test/unconfigured",
        json!({
            "url": "https://hydration.example.test/unconfigured",
            "blob_ref": ContentRef::from_digest_bytes(blake3::hash(b"body").as_bytes()).to_string(),
        }),
    )
    .await
    .unwrap();
    let before = domain_counts(&runtime).await;
    let error = registry
        .dispatch("web.extract", json!({ "id": id }))
        .await
        .unwrap_err();
    assert!(matches!(error, RuntimeError::Unconfigured(ref message)
        if message == "no BlobStore installed on this server (configure [storage.blob] in khive.toml, or KHIVE_BLOB_ROOT)"));
    assert_eq!(domain_counts(&runtime).await, before);
}
