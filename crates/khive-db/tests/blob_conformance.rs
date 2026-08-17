//! Shared `BlobStore` conformance suite (ADR-111 Amendment 2, CI layer 1).
//!
//! The same behavioral contract exercised against every `BlobStore`
//! implementation this crate ships. `FsBlobStore` runs unconditionally.
//! `S3BlobStore` runs only when `KHIVE_S3_TEST_ENDPOINT` (plus bucket/region
//! and AWS credential env vars) is set -- normally by the pinned-MinIO CI job
//! (`.github/workflows/ci.yml`, `minio-blob-compat`) -- and is skipped with an
//! explicit message everywhere else, since it needs a live S3-compatible
//! endpoint to mean anything.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::stream::StreamExt;
use khive_db::stores::blob::FsBlobStore;
use khive_db::stores::blob_s3::{S3BlobStore, S3BlobStoreConfig};
use khive_storage::blob::{BlobOrphanSweepConfig, BlobStore, ContentRef, MAX_BLOB_WHOLE_BYTES};
use khive_storage::{StorageCapability, StorageError, StorageResult};

/// Scriptable in-memory backend used to keep the required method honest for
/// test/future implementations, including metadata/body corruption that a
/// normal content-addressed `put` cannot create.
#[derive(Debug, Default)]
struct FixtureBlobStore {
    objects: Mutex<HashMap<ContentRef, (u64, Vec<u8>)>>,
    bounded_backend_calls: AtomicUsize,
}

impl FixtureBlobStore {
    fn insert_raw(&self, content_ref: ContentRef, metadata_bytes: u64, body: Vec<u8>) {
        self.objects
            .lock()
            .unwrap()
            .insert(content_ref, (metadata_bytes, body));
    }

    fn not_found(content_ref: &ContentRef) -> StorageError {
        StorageError::NotFound {
            capability: StorageCapability::Blob,
            resource: "blob",
            key: content_ref.to_string(),
        }
    }
}

#[async_trait]
impl BlobStore for FixtureBlobStore {
    async fn put(&self, bytes: Vec<u8>) -> StorageResult<ContentRef> {
        let content_ref = ContentRef::from_digest_bytes(blake3::hash(&bytes).as_bytes());
        self.objects
            .lock()
            .unwrap()
            .insert(content_ref.clone(), (bytes.len() as u64, bytes));
        Ok(content_ref)
    }

    async fn get_bounded_verified(
        &self,
        content_ref: &ContentRef,
        max_bytes: u64,
    ) -> StorageResult<Vec<u8>> {
        if max_bytes > MAX_BLOB_WHOLE_BYTES {
            return Err(StorageError::InvalidInput {
                capability: StorageCapability::Blob,
                operation: "get_bounded_verified".into(),
                message: "maximum exceeds portable envelope".into(),
            });
        }
        self.bounded_backend_calls.fetch_add(1, Ordering::SeqCst);
        let guard = self.objects.lock().unwrap();
        let (metadata_bytes, body) = guard
            .get(content_ref)
            .ok_or_else(|| Self::not_found(content_ref))?;
        if *metadata_bytes > max_bytes {
            return Err(StorageError::BlobTooLarge {
                content_ref: content_ref.clone(),
                max_bytes,
                observed_at_least: *metadata_bytes,
            });
        }
        if body.len() as u64 > max_bytes {
            return Err(StorageError::BlobTooLarge {
                content_ref: content_ref.clone(),
                max_bytes,
                observed_at_least: max_bytes + 1,
            });
        }
        if *metadata_bytes != body.len() as u64 {
            return Err(StorageError::BlobSizeMismatch {
                content_ref: content_ref.clone(),
                metadata_bytes: *metadata_bytes,
                actual_bytes: body.len() as u64,
            });
        }
        let actual = ContentRef::from_digest_bytes(blake3::hash(body).as_bytes());
        if actual != *content_ref {
            return Err(StorageError::BlobDigestMismatch {
                expected: content_ref.clone(),
                actual,
            });
        }
        Ok(body.clone())
    }

    async fn exists(&self, content_ref: &ContentRef) -> StorageResult<bool> {
        Ok(self.objects.lock().unwrap().contains_key(content_ref))
    }

    async fn size(&self, content_ref: &ContentRef) -> StorageResult<Option<u64>> {
        Ok(self
            .objects
            .lock()
            .unwrap()
            .get(content_ref)
            .map(|(metadata_bytes, _)| *metadata_bytes))
    }

    async fn delete(&self, content_ref: &ContentRef) -> StorageResult<bool> {
        Ok(self.objects.lock().unwrap().remove(content_ref).is_some())
    }

    async fn orphan_sweep(
        &self,
        config: &BlobOrphanSweepConfig,
    ) -> StorageResult<khive_storage::BlobOrphanSweepResult> {
        let mut objects = self.objects.lock().unwrap();
        let orphaned: Vec<ContentRef> = objects
            .keys()
            .filter(|content_ref| !config.live_refs.contains(*content_ref))
            .cloned()
            .collect();
        let scanned = objects.len() as u64;
        if !config.dry_run {
            for content_ref in &orphaned {
                objects.remove(content_ref);
            }
        }
        Ok(khive_storage::BlobOrphanSweepResult {
            scanned,
            deleted: if config.dry_run {
                0
            } else {
                orphaned.len() as u64
            },
            would_delete: orphaned.len() as u64,
            grace_period_skipped: 0,
        })
    }
}

async fn assert_conforms(store: Arc<dyn BlobStore>) {
    let bytes = b"khive blob conformance suite".to_vec();

    // put is dedup-idempotent: two puts of the same bytes return the same
    // ContentRef and never error on the second write.
    let ref_a = store.put(bytes.clone()).await.expect("first put");
    let ref_b = store.put(bytes.clone()).await.expect("second put (dedup)");
    assert_eq!(ref_a, ref_b);

    assert!(store.exists(&ref_a).await.expect("exists"));
    assert_eq!(
        store.size(&ref_a).await.expect("size"),
        Some(bytes.len() as u64)
    );

    // ADR-160 D2: the required whole-buffer read succeeds exactly at the
    // caller's actual-byte limit and returns only digest-verified bytes.
    let verified = store
        .get_bounded_verified(&ref_a, bytes.len() as u64)
        .await
        .expect("bounded verified get at the exact limit");
    assert_eq!(verified, bytes);

    // The first byte beyond the caller's limit is refused with the same
    // typed result on every backend. For this normally-published object the
    // authoritative metadata knows the complete observed size.
    let too_small_max = bytes.len() as u64 - 1;
    let err = store
        .get_bounded_verified(&ref_a, too_small_max)
        .await
        .expect_err("bounded get below the object size must refuse");
    match err {
        StorageError::BlobTooLarge {
            content_ref,
            max_bytes,
            observed_at_least,
        } => {
            assert_eq!(content_ref, ref_a);
            assert_eq!(max_bytes, too_small_max);
            assert_eq!(observed_at_least, bytes.len() as u64);
        }
        other => panic!("expected BlobTooLarge, got {other:?}"),
    }

    // max_bytes=0 is a real boundary, not a synonym for "unbounded".
    let empty_ref = store.put(Vec::new()).await.expect("put empty object");
    assert_eq!(
        store
            .get_bounded_verified(&empty_ref, 0)
            .await
            .expect("empty object at a zero-byte limit"),
        Vec::<u8>::new()
    );
    assert!(matches!(
        store.get_bounded_verified(&ref_a, 0).await,
        Err(StorageError::BlobTooLarge {
            max_bytes: 0,
            observed_at_least,
            ..
        }) if observed_at_least >= 1
    ));

    // A content ref that was never written does not exist and refuses a read.
    let never_written = ContentRef::from_digest_bytes(&[0xAB; 32]);
    assert!(!store.exists(&never_written).await.expect("exists (absent)"));
    assert!(matches!(
        store
            .get_bounded_verified(&never_written, MAX_BLOB_WHOLE_BYTES)
            .await,
        Err(StorageError::NotFound { .. })
    ));

    // Argument validation has deterministic precedence over not-found and
    // must happen before a backend lookup.
    assert!(matches!(
        store
            .get_bounded_verified(&never_written, MAX_BLOB_WHOLE_BYTES + 1)
            .await,
        Err(StorageError::InvalidInput { .. })
    ));

    // orphan_sweep dry-run against an empty live set reports would_delete
    // for the object we just wrote, without touching it.
    let sweep = store
        .orphan_sweep(&BlobOrphanSweepConfig {
            live_refs: Default::default(),
            dry_run: true,
        })
        .await
        .expect("orphan_sweep dry-run");
    assert!(sweep.would_delete >= 1);
    assert!(store
        .exists(&ref_a)
        .await
        .expect("still exists after dry-run"));

    // delete is idempotent-shaped: true the first time, false thereafter.
    assert!(store.delete(&ref_a).await.expect("delete"));
    assert!(!store.exists(&ref_a).await.expect("exists after delete"));
    assert!(!store
        .delete(&ref_a)
        .await
        .expect("second delete is a no-op"));
    assert!(store.delete(&empty_ref).await.expect("delete empty object"));
}

#[tokio::test]
async fn scripted_fixture_blob_store_conforms() {
    let store: Arc<dyn BlobStore> = Arc::new(FixtureBlobStore::default());
    assert_conforms(store).await;
}

#[tokio::test]
async fn scripted_fixture_pins_corruption_and_error_precedence() {
    let store = FixtureBlobStore::default();
    let missing = ContentRef::from_hex("a".repeat(64)).unwrap();
    assert!(matches!(
        store
            .get_bounded_verified(&missing, MAX_BLOB_WHOLE_BYTES + 1)
            .await,
        Err(StorageError::InvalidInput { .. })
    ));
    assert_eq!(
        store.bounded_backend_calls.load(Ordering::SeqCst),
        0,
        "invalid argument must win before fixture backend work"
    );

    let metadata_oversize = ContentRef::from_hex("b".repeat(64)).unwrap();
    store.insert_raw(metadata_oversize.clone(), 5, b"bad".to_vec());
    assert!(matches!(
        store.get_bounded_verified(&metadata_oversize, 4).await,
        Err(StorageError::BlobTooLarge {
            observed_at_least: 5,
            ..
        })
    ));

    let streamed_oversize = ContentRef::from_hex("c".repeat(64)).unwrap();
    store.insert_raw(streamed_oversize.clone(), 1, b"12345".to_vec());
    assert!(matches!(
        store.get_bounded_verified(&streamed_oversize, 4).await,
        Err(StorageError::BlobTooLarge {
            observed_at_least: 5,
            ..
        })
    ));

    // A size mismatch wins before the intentionally-wrong digest.
    let size_mismatch = ContentRef::from_hex("d".repeat(64)).unwrap();
    store.insert_raw(size_mismatch.clone(), 3, b"four".to_vec());
    assert!(matches!(
        store.get_bounded_verified(&size_mismatch, 4).await,
        Err(StorageError::BlobSizeMismatch {
            metadata_bytes: 3,
            actual_bytes: 4,
            ..
        })
    ));

    let truncated_body = ContentRef::from_hex("e".repeat(64)).unwrap();
    store.insert_raw(truncated_body.clone(), 5, b"four".to_vec());
    assert!(matches!(
        store.get_bounded_verified(&truncated_body, 5).await,
        Err(StorageError::BlobSizeMismatch {
            metadata_bytes: 5,
            actual_bytes: 4,
            ..
        })
    ));

    let expected = ContentRef::from_digest_bytes(blake3::hash(b"expected").as_bytes());
    let actual = ContentRef::from_digest_bytes(blake3::hash(b"mutated!").as_bytes());
    store.insert_raw(expected.clone(), 8, b"mutated!".to_vec());
    assert!(matches!(
        store.get_bounded_verified(&expected, 8).await,
        Err(StorageError::BlobDigestMismatch {
            expected: got_expected,
            actual: got_actual,
        }) if got_expected == expected && got_actual == actual
    ));
}

#[tokio::test]
async fn fs_blob_store_conforms() {
    let dir = tempfile::tempdir().unwrap();
    // Explicit floor_bytes=0, not the default 100GB — the free space on
    // whatever volume runs this test is not this test's concern (and a
    // dev machine or CI runner legitimately may not clear 100GB free).
    // The floor guard itself is covered by unit tests in stores/blob.rs.
    // Zero orphan-sweep grace period — this suite's orphan_sweep assertions
    // exercise immediate-deletion behavior, not the publish-grace window
    // (covered separately in stores/blob.rs).
    let store: Arc<dyn BlobStore> = Arc::new(
        FsBlobStore::new(dir.path().to_path_buf(), 0)
            .expect("FsBlobStore::new")
            .with_orphan_sweep_grace(std::time::Duration::ZERO),
    );
    assert_conforms(store).await;
}

#[tokio::test]
async fn s3_blob_store_conforms_against_a_live_endpoint() {
    let Ok(endpoint) = std::env::var("KHIVE_S3_TEST_ENDPOINT") else {
        eprintln!(
            "skipping s3_blob_store_conforms_against_a_live_endpoint: \
             KHIVE_S3_TEST_ENDPOINT is not set (no live S3-compatible endpoint configured). \
             This leg runs in CI's pinned-MinIO job; it is not exercised by a plain \
             `cargo test` with no S3 endpoint available."
        );
        return;
    };
    let bucket =
        std::env::var("KHIVE_S3_TEST_BUCKET").unwrap_or_else(|_| "khive-blob-conformance".into());
    let region = std::env::var("KHIVE_S3_TEST_REGION").unwrap_or_else(|_| "us-east-1".into());

    let config = S3BlobStoreConfig::new(bucket, region)
        .with_endpoint(endpoint)
        .with_allow_http(true)
        .with_prefix(format!("conformance-{}", uuid::Uuid::new_v4()));
    let store: Arc<dyn BlobStore> =
        Arc::new(S3BlobStore::new(config).expect("S3BlobStore::new against MinIO"));
    assert_conforms(store).await;
}

/// ADR-111 Amendment 2: `orphan_sweep`'s `ListObjectsV2`
/// pagination is untested unless a real sweep crosses the 1,000-key page
/// boundary. This populates `PAGE_CROSSING_OBJECT_COUNT` (> 1,000) tiny,
/// distinct-content objects under a scratch prefix and confirms the sweep
/// scans every one of them and reports zero orphans (all are in
/// `live_refs`) -- proof the multi-page LIST loop in
/// `S3BlobStore::orphan_sweep` actually continues past the first page rather
/// than only ever exercising a single-page listing, as every other test in
/// this suite does.
#[tokio::test]
async fn s3_blob_store_orphan_sweep_paginates_past_the_1000_key_page_boundary() {
    let Ok(endpoint) = std::env::var("KHIVE_S3_TEST_ENDPOINT") else {
        eprintln!(
            "skipping s3_blob_store_orphan_sweep_paginates_past_the_1000_key_page_boundary: \
             KHIVE_S3_TEST_ENDPOINT is not set (no live S3-compatible endpoint configured). \
             This leg runs in CI's pinned-MinIO job; it is not exercised by a plain \
             `cargo test` with no S3 endpoint available."
        );
        return;
    };
    let bucket =
        std::env::var("KHIVE_S3_TEST_BUCKET").unwrap_or_else(|_| "khive-blob-conformance".into());
    let region = std::env::var("KHIVE_S3_TEST_REGION").unwrap_or_else(|_| "us-east-1".into());

    const PAGE_CROSSING_OBJECT_COUNT: usize = 1200;
    const PUT_CONCURRENCY: usize = 64;

    let config = S3BlobStoreConfig::new(bucket, region)
        .with_endpoint(endpoint)
        .with_allow_http(true)
        .with_prefix(format!("pagination-{}", uuid::Uuid::new_v4()));
    let store = S3BlobStore::new(config).expect("S3BlobStore::new against MinIO");

    let live_refs: std::collections::HashSet<ContentRef> =
        futures::stream::iter((0..PAGE_CROSSING_OBJECT_COUNT).map(|i| {
            let store = &store;
            async move {
                store
                    .put(format!("khive pagination object {i}").into_bytes())
                    .await
                    .expect("put a page-boundary object")
            }
        }))
        .buffer_unordered(PUT_CONCURRENCY)
        .collect()
        .await;
    assert_eq!(live_refs.len(), PAGE_CROSSING_OBJECT_COUNT);

    let sweep = store
        .orphan_sweep(&BlobOrphanSweepConfig {
            live_refs,
            dry_run: true,
        })
        .await
        .expect("orphan_sweep must complete across every LIST page");
    assert!(
        sweep.scanned >= PAGE_CROSSING_OBJECT_COUNT as u64,
        "sweep must scan every populated object across all LIST pages, got {sweep:?}"
    );
    assert_eq!(
        sweep.would_delete, 0,
        "every populated object is in live_refs; a paginated dry-run sweep must report \
         zero orphans, got {sweep:?}"
    );
}
