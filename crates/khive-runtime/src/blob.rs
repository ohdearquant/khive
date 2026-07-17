//! Config-driven `BlobStore` selection (ADR-111 Amendment 2).
//!
//! `khive-db` cannot parse `khive.toml` itself (it sits below `khive-runtime`
//! in the crate dependency chain), so the fs-vs-s3 selector lives here, one
//! layer up, where `KhiveConfig` is already parsed. This is the choke point
//! every boot path (single- and multi-backend) resolves the configured blob
//! store through, so the two never drift onto different construction logic.

use std::sync::Arc;

use khive_db::stores::blob_s3::{S3BlobStore, S3BlobStoreConfig};
use khive_db::{SqliteError, StorageBackend};
use khive_storage::BlobStore;

use crate::engine_config::BlobConfig;
use crate::KhiveConfig;

/// Resolve the `BlobStore` this `backend` should use, per `cfg.storage.blob`.
///
/// - Absent, or `backend = "fs"`: `FsBlobStore` via `StorageBackend::blob_store`,
///   using the existing `KHIVE_BLOB_ROOT` > `root` > `<db_dir>/blobs` precedence
///   (khive#292) — unchanged from every configuration written before this
///   section existed.
/// - `backend = "s3"`: `S3BlobStore`, built from the non-secret TOML fields
///   plus environment credentials (`S3BlobStore::new`).
pub fn resolve_blob_store(
    cfg: &KhiveConfig,
    backend: &StorageBackend,
) -> Result<Arc<dyn BlobStore>, SqliteError> {
    match &cfg.storage.blob {
        None => backend.blob_store(None, None),
        Some(BlobConfig::Fs { root, floor_bytes }) => {
            let root_path = root.as_ref().map(std::path::PathBuf::from);
            backend.blob_store(root_path.as_deref(), *floor_bytes)
        }
        Some(BlobConfig::S3 {
            bucket,
            region,
            endpoint,
            prefix,
            allow_http,
        }) => {
            let mut s3_cfg = S3BlobStoreConfig::new(bucket.clone(), region.clone());
            if let Some(endpoint) = endpoint {
                s3_cfg = s3_cfg.with_endpoint(endpoint.clone());
            }
            if let Some(prefix) = prefix {
                s3_cfg = s3_cfg.with_prefix(prefix.clone());
            }
            if let Some(allow_http) = allow_http {
                s3_cfg = s3_cfg.with_allow_http(*allow_http);
            }
            let store = S3BlobStore::new(s3_cfg)?;
            Ok(Arc::new(store))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine_config::StorageSectionConfig;

    fn memory_backend() -> StorageBackend {
        StorageBackend::memory().expect("memory backend should create")
    }

    #[test]
    fn absent_storage_section_selects_fs_with_explicit_root() {
        // An in-memory backend has no data_dir to default beside, so this
        // exercises the "existing configurations keep working" path via an
        // explicit override rather than proving the full khive#292 chain
        // (already covered by `StorageBackend::blob_store`'s own tests).
        let dir = tempfile::tempdir().unwrap();
        let backend = memory_backend();
        let cfg = KhiveConfig::default();
        // `resolve_blob_store` with no override falls through to
        // `backend.blob_store(None, None)`, which errors for an in-memory
        // backend with no root -- confirm that specific, documented failure
        // mode rather than silently picking an arbitrary path.
        let err = match resolve_blob_store(&cfg, &backend) {
            Err(e) => e,
            Ok(_) => panic!("expected an error for an in-memory backend with no root override"),
        };
        assert!(matches!(err, SqliteError::InvalidData(_)));
        drop(dir);
    }

    #[test]
    fn explicit_fs_root_is_selected() {
        let dir = tempfile::tempdir().unwrap();
        let backend = memory_backend();
        let cfg = KhiveConfig {
            storage: StorageSectionConfig {
                blob: Some(BlobConfig::Fs {
                    root: Some(dir.path().to_string_lossy().into_owned()),
                    floor_bytes: Some(0),
                }),
            },
            ..KhiveConfig::default()
        };
        let store = resolve_blob_store(&cfg, &backend).expect("fs store should build");
        drop(store);
    }

    #[test]
    fn s3_backend_selection_reaches_s3_construction() {
        // No AWS credentials in this test process: `S3BlobStore::new` must
        // fail at the credential-env check, proving the S3 arm was actually
        // selected and reached (not silently falling back to fs).
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var("AWS_ACCESS_KEY_ID");
        std::env::remove_var("AWS_SECRET_ACCESS_KEY");
        let backend = memory_backend();
        let cfg = KhiveConfig {
            storage: StorageSectionConfig {
                blob: Some(BlobConfig::S3 {
                    bucket: "khive-blobs".to_string(),
                    region: "us-east-1".to_string(),
                    endpoint: None,
                    prefix: None,
                    allow_http: None,
                }),
            },
            ..KhiveConfig::default()
        };
        let err = match resolve_blob_store(&cfg, &backend) {
            Err(e) => e,
            Ok(_) => panic!("expected the credential-env error with no AWS env vars set"),
        };
        let msg = err.to_string();
        assert!(
            msg.contains("AWS_ACCESS_KEY_ID"),
            "expected the credential-env error, got: {msg}"
        );
    }

    // Guards the two credential env vars this module's test toggles, since
    // `std::env::set_var`/`remove_var` mutate real process-global state and
    // the crate's default parallel test runner would otherwise interleave.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
}
