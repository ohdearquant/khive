//! Shared `BlobStore` conformance suite (ADR-111 Amendment 2, CI layer 1).
//!
//! The same behavioral contract exercised against every `BlobStore`
//! implementation this crate ships. `FsBlobStore` runs unconditionally.
//! `S3BlobStore` runs only when `KHIVE_S3_TEST_ENDPOINT` (plus bucket/region
//! and AWS credential env vars) is set -- normally by the pinned-MinIO CI job
//! (`.github/workflows/ci.yml`, `minio-blob-compat`) -- and is skipped with an
//! explicit message everywhere else, since it needs a live S3-compatible
//! endpoint to mean anything.

use std::sync::Arc;

use khive_db::stores::blob::FsBlobStore;
use khive_db::stores::blob_s3::{S3BlobStore, S3BlobStoreConfig};
use khive_storage::blob::{BlobOrphanSweepConfig, BlobStore, ContentRef};

async fn assert_conforms(store: Arc<dyn BlobStore>) {
    let bytes = b"khive blob conformance suite".to_vec();

    // put is dedup-idempotent: two puts of the same bytes return the same
    // ContentRef and never error on the second write.
    let ref_a = store.put(bytes.clone()).await.expect("first put");
    let ref_b = store.put(bytes.clone()).await.expect("second put (dedup)");
    assert_eq!(ref_a, ref_b);

    assert!(store.exists(&ref_a).await.expect("exists"));

    let round_tripped = store.get(&ref_a).await.expect("get");
    assert_eq!(round_tripped, bytes);

    // A content ref that was never written does not exist and 404s on get.
    let never_written = ContentRef::from_digest_bytes(&[0xAB; 32]);
    assert!(!store.exists(&never_written).await.expect("exists (absent)"));
    assert!(store.get(&never_written).await.is_err());

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
}

#[tokio::test]
async fn fs_blob_store_conforms() {
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn BlobStore> = Arc::new(
        FsBlobStore::new(dir.path().to_path_buf(), FsBlobStore::DEFAULT_FLOOR_BYTES)
            .expect("FsBlobStore::new"),
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
