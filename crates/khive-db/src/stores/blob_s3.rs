//! S3-compatible `BlobStore` backend (ADR-111 Amendment 2).
//!
//! Second implementation of the unchanged `khive_storage::blob::BlobStore`
//! trait, beside `FsBlobStore` (`stores::blob`). Same content-addressed CAS
//! contract — `ContentRef`, dedup-on-identical-bytes, offline-maintenance-only
//! `delete`/`orphan_sweep` — over an S3-compatible object store via the
//! `object_store` crate's `aws` feature. No provider type crosses the
//! `khive-storage` trait boundary: callers still hold only `Arc<dyn
//! BlobStore>`.

use std::sync::Arc;

use async_trait::async_trait;
use futures::stream::StreamExt;
use object_store::aws::AmazonS3Builder;
use object_store::path::Path as ObjectPath;
use object_store::{
    Error as ObjectStoreError, ObjectMeta, ObjectStore, ObjectStoreExt, PutMode, PutPayload,
};

use khive_storage::blob::{BlobOrphanSweepConfig, BlobOrphanSweepResult, BlobStore, ContentRef};
use khive_storage::error::StorageError;
use khive_storage::types::StorageResult;
use khive_storage::StorageCapability;

use crate::error::SqliteError;

/// Supported object size ceiling for v1 (ADR-111 Amendment 2, Fork 2): the
/// existing whole-buffer `Vec<u8>` trait is accepted only up to this size.
/// `put` rejects a larger buffer; `get` checks metadata before collecting a
/// larger response. A streaming amendment is required before khive supports
/// larger blobs.
pub const MAX_OBJECT_BYTES: u64 = 64 * 1024 * 1024;

/// Default object-key prefix when a caller doesn't override it.
pub const DEFAULT_PREFIX: &str = "blobs";

/// Non-secret S3 backend configuration (ADR-111 Amendment 2). Credentials
/// are never part of this struct — `S3BlobStore::new` reads them from the
/// process environment only, never TOML.
#[derive(Clone, Debug)]
pub struct S3BlobStoreConfig {
    pub bucket: String,
    pub region: String,
    /// Compatibility knob for Cloudflare R2, MinIO, Tigris, and similar
    /// services. `None` means real AWS S3's normal regional endpoint.
    pub endpoint: Option<String>,
    pub prefix: String,
    /// Escape hatch for a trusted local test endpoint. `false` by default;
    /// an `http://` endpoint is rejected unless this is set.
    pub allow_http: bool,
}

impl S3BlobStoreConfig {
    pub fn new(bucket: impl Into<String>, region: impl Into<String>) -> Self {
        Self {
            bucket: bucket.into(),
            region: region.into(),
            endpoint: None,
            prefix: DEFAULT_PREFIX.to_string(),
            allow_http: false,
        }
    }

    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = Some(endpoint.into());
        self
    }

    pub fn with_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = prefix.into();
        self
    }

    pub fn with_allow_http(mut self, allow_http: bool) -> Self {
        self.allow_http = allow_http;
        self
    }

    fn validate(&self) -> Result<(), SqliteError> {
        if self.bucket.trim().is_empty() {
            return Err(SqliteError::InvalidData(
                "S3 blob store: bucket must not be empty".to_string(),
            ));
        }
        if self.region.trim().is_empty() {
            return Err(SqliteError::InvalidData(
                "S3 blob store: region must not be empty".to_string(),
            ));
        }
        validate_prefix(&self.prefix)?;
        if let Some(endpoint) = &self.endpoint {
            if endpoint.starts_with("http://") && !self.allow_http {
                return Err(SqliteError::InvalidData(format!(
                    "S3 blob store: endpoint {endpoint:?} uses http:// but allow_http is false \
                     (set allow_http=true only for a trusted local test endpoint)"
                )));
            }
        }
        Ok(())
    }
}

/// A non-empty, canonical key prefix: no leading/trailing slash and no
/// empty, `.`, or `..` segment (ADR-111 Amendment 2).
fn validate_prefix(prefix: &str) -> Result<(), SqliteError> {
    if prefix.is_empty() {
        return Err(SqliteError::InvalidData(
            "S3 blob store: prefix must not be empty".to_string(),
        ));
    }
    if prefix.starts_with('/') || prefix.ends_with('/') {
        return Err(SqliteError::InvalidData(format!(
            "S3 blob store: prefix {prefix:?} must not have a leading or trailing slash"
        )));
    }
    for segment in prefix.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            return Err(SqliteError::InvalidData(format!(
                "S3 blob store: prefix {prefix:?} must not contain an empty, '.', or '..' segment"
            )));
        }
    }
    Ok(())
}

/// AWS credentials read from the process environment only (ADR-111
/// Amendment 2: never TOML, never printed, never in errors).
/// `AWS_ACCESS_KEY_ID`/`AWS_SECRET_ACCESS_KEY` are an all-or-nothing pair;
/// `AWS_SESSION_TOKEN` is optional. Startup errors name the missing
/// variable, never a value.
struct S3Credentials {
    access_key_id: String,
    secret_access_key: String,
    session_token: Option<String>,
}

/// Redacted on purpose: credentials must never appear in a panic message
/// (e.g. an `unwrap_err` on the wrong variant in a test) or a log line.
impl std::fmt::Debug for S3Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3Credentials")
            .field("access_key_id", &"<redacted>")
            .field("secret_access_key", &"<redacted>")
            .field(
                "session_token",
                &self.session_token.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

impl S3Credentials {
    fn from_env() -> Result<Self, SqliteError> {
        let access_key_id = non_empty_env("AWS_ACCESS_KEY_ID");
        let secret_access_key = non_empty_env("AWS_SECRET_ACCESS_KEY");
        let (access_key_id, secret_access_key) = match (access_key_id, secret_access_key) {
            (Some(a), Some(s)) => (a, s),
            (Some(_), None) => {
                return Err(SqliteError::InvalidData(
                    "S3 blob store: AWS_ACCESS_KEY_ID is set but AWS_SECRET_ACCESS_KEY is \
                     missing -- both or neither must be set"
                        .to_string(),
                ));
            }
            (None, Some(_)) => {
                return Err(SqliteError::InvalidData(
                    "S3 blob store: AWS_SECRET_ACCESS_KEY is set but AWS_ACCESS_KEY_ID is \
                     missing -- both or neither must be set"
                        .to_string(),
                ));
            }
            (None, None) => {
                return Err(SqliteError::InvalidData(
                    "S3 blob store: AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY must both be \
                     set in the process environment"
                        .to_string(),
                ));
            }
        };
        Ok(Self {
            access_key_id,
            secret_access_key,
            session_token: non_empty_env("AWS_SESSION_TOKEN"),
        })
    }
}

fn non_empty_env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

fn map_object_store_err(e: ObjectStoreError, op: &'static str) -> StorageError {
    StorageError::driver(StorageCapability::Blob, op, e)
}

/// An `S3BlobStore` backed by an S3-compatible object store (ADR-111
/// Amendment 2). Object keys follow the same two-level BLAKE3 shard shape as
/// `FsBlobStore`: `{prefix}/{hex[0..2]}/{hex[2..4]}/{hex}`.
///
/// # Bucket versioning is an operator obligation, not a preflight check
///
/// ADR-111 Amendment 2 requires the target bucket to be unversioned (a
/// versioned or versioning-suspended bucket lets `DELETE Object` leave a
/// delete marker while retaining prior bytes, breaking `delete`/
/// `orphan_sweep`'s physical-deletion contract). `object_store`'s
/// `ObjectStore` trait has no `GetBucketVersioning`-equivalent operation, and
/// adding one would mean a raw signed HTTP call or `aws-sdk-s3` outside the
/// dependency this amendment pins -- out of scope for v1. `S3BlobStore::new`
/// does **not** verify bucket versioning state; the operator provisioning
/// the bucket must ensure it is unversioned.
pub struct S3BlobStore {
    client: Arc<dyn ObjectStore>,
    prefix: String,
}

impl S3BlobStore {
    /// Build a store from non-secret config plus environment credentials.
    ///
    /// Addressing style (spec-gate rider, ADR-111 Amendment 2): AWS has
    /// deprecated path-style requests for new buckets, so when `endpoint` is
    /// omitted (real AWS S3), this uses virtual-hosted-style requests.  When
    /// an explicit `endpoint` is set (R2, MinIO, Tigris, or a local test
    /// double), it uses path-style, matching how those services are
    /// conventionally reached and how `object_store`'s own examples
    /// configure a custom endpoint.
    pub fn new(config: S3BlobStoreConfig) -> Result<Self, SqliteError> {
        config.validate()?;
        let credentials = S3Credentials::from_env()?;

        let mut builder = AmazonS3Builder::new()
            .with_bucket_name(&config.bucket)
            .with_region(&config.region)
            .with_access_key_id(&credentials.access_key_id)
            .with_secret_access_key(&credentials.secret_access_key)
            .with_allow_http(config.allow_http);
        if let Some(token) = &credentials.session_token {
            builder = builder.with_token(token);
        }
        builder = match &config.endpoint {
            Some(endpoint) => builder
                .with_endpoint(endpoint.clone())
                .with_virtual_hosted_style_request(false),
            None => builder.with_virtual_hosted_style_request(true),
        };
        // Conditional-create defaults to `S3ConditionalPut::ETagMatch`
        // (object_store's own default), which is what turns `PutMode::Create`
        // below into an `If-None-Match: *` request -- supported by AWS S3
        // and by R2/MinIO-class S3-compatible stores alike. Not overridden
        // here on purpose.

        let client = builder.build().map_err(|e| {
            SqliteError::InvalidData(format!("S3 blob store: failed to build client: {e}"))
        })?;

        Ok(Self {
            client: Arc::new(client),
            prefix: config.prefix,
        })
    }

    fn shard_key(&self, content_ref: &ContentRef) -> ObjectPath {
        let hex = content_ref.as_str();
        ObjectPath::from(format!(
            "{}/{}/{}/{}",
            self.prefix,
            &hex[0..2],
            &hex[2..4],
            hex
        ))
    }

    /// Parse an object key back into a `ContentRef`, validating that it
    /// matches this store's exact `{prefix}/{h[0..2]}/{h[2..4]}/{h}` shard
    /// shape. Anything else under the prefix (a foreign key, a partial
    /// upload artifact from another tool, a key with extra path segments) is
    /// not recognized -- `orphan_sweep` must never delete it.
    fn parse_shard_key(&self, location: &ObjectPath) -> Option<ContentRef> {
        let full: &str = location.as_ref();
        let rest = full.strip_prefix(&self.prefix)?.strip_prefix('/')?;
        let mut parts = rest.split('/');
        let shard1 = parts.next()?;
        let shard2 = parts.next()?;
        let hex = parts.next()?;
        if parts.next().is_some() {
            return None;
        }
        let content_ref = ContentRef::from_hex(hex.to_string()).ok()?;
        let full_hex = content_ref.as_str();
        if &full_hex[0..2] != shard1 || &full_hex[2..4] != shard2 {
            return None;
        }
        Some(content_ref)
    }
}

#[async_trait]
impl BlobStore for S3BlobStore {
    async fn put(&self, bytes: Vec<u8>) -> StorageResult<ContentRef> {
        if bytes.len() as u64 > MAX_OBJECT_BYTES {
            return Err(StorageError::InvalidInput {
                capability: StorageCapability::Blob,
                operation: "put".into(),
                message: format!(
                    "payload of {} bytes exceeds the {MAX_OBJECT_BYTES}-byte v1 ceiling \
                     (ADR-111 Amendment 2)",
                    bytes.len()
                ),
            });
        }

        let digest = blake3::hash(&bytes);
        let content_ref = ContentRef::from_digest_bytes(digest.as_bytes());
        let key = self.shard_key(&content_ref);

        // HEAD fast path: content-addressed dedup means an existing object
        // makes this put a no-op, same contract as FsBlobStore.
        match self.client.head(&key).await {
            Ok(_) => return Ok(content_ref),
            Err(ObjectStoreError::NotFound { .. }) => {}
            Err(e) => return Err(map_object_store_err(e, "put_head")),
        }

        let payload = PutPayload::from(bytes);
        match self
            .client
            .put_opts(&key, payload, PutMode::Create.into())
            .await
        {
            Ok(_) => Ok(content_ref),
            // A concurrent identical writer published this exact
            // content-addressed key between our HEAD and this conditional
            // PUT. CAS semantics make that a success, not a conflict.
            Err(ObjectStoreError::AlreadyExists { .. }) => Ok(content_ref),
            Err(e) => Err(map_object_store_err(e, "put")),
        }
    }

    async fn get(&self, content_ref: &ContentRef) -> StorageResult<Vec<u8>> {
        let key = self.shard_key(content_ref);
        let result = self.client.get(&key).await.map_err(|e| match e {
            ObjectStoreError::NotFound { .. } => StorageError::NotFound {
                capability: StorageCapability::Blob,
                resource: "blob",
                key: content_ref.to_string(),
            },
            other => map_object_store_err(other, "get"),
        })?;

        if result.meta.size > MAX_OBJECT_BYTES {
            return Err(StorageError::InvalidInput {
                capability: StorageCapability::Blob,
                operation: "get".into(),
                message: format!(
                    "object of {} bytes exceeds the {MAX_OBJECT_BYTES}-byte v1 ceiling \
                     (ADR-111 Amendment 2)",
                    result.meta.size
                ),
            });
        }

        let bytes = result
            .bytes()
            .await
            .map_err(|e| map_object_store_err(e, "get"))?;
        Ok(bytes.to_vec())
    }

    async fn exists(&self, content_ref: &ContentRef) -> StorageResult<bool> {
        let key = self.shard_key(content_ref);
        match self.client.head(&key).await {
            Ok(_) => Ok(true),
            Err(ObjectStoreError::NotFound { .. }) => Ok(false),
            Err(e) => Err(map_object_store_err(e, "exists")),
        }
    }

    async fn delete(&self, content_ref: &ContentRef) -> StorageResult<bool> {
        let key = self.shard_key(content_ref);
        // HEAD-then-DELETE (ADR-111 Amendment 2): S3 DELETE is idempotent
        // and does not report prior existence, so the trait's `true`/`false`
        // contract needs the preliminary HEAD. Offline-maintenance-only,
        // same as FsBlobStore -- see `BlobStore::delete`'s doc comment; this
        // backend adds no additional coordination against a racing entity
        // write.
        match self.client.head(&key).await {
            Ok(_) => {}
            Err(ObjectStoreError::NotFound { .. }) => return Ok(false),
            Err(e) => return Err(map_object_store_err(e, "delete_head")),
        }
        self.client
            .delete(&key)
            .await
            .map_err(|e| map_object_store_err(e, "delete"))?;
        Ok(true)
    }

    // Offline-maintenance-only -- see `BlobStore::orphan_sweep`'s doc
    // comment for the concurrency hazard. `config.live_refs` is a snapshot;
    // this method performs no DB coordination, only a diff against whatever
    // set the caller handed it. Pagination is bounded (at most
    // `PAGE_SIZE` remote-listing entries held at once) and deletes run at
    // bounded concurrency; a list or delete failure aborts with an error
    // rather than reporting a partial scan as complete.
    async fn orphan_sweep(
        &self,
        config: &BlobOrphanSweepConfig,
    ) -> StorageResult<BlobOrphanSweepResult> {
        const PAGE_SIZE: usize = 1000;
        const DELETE_CONCURRENCY: usize = 32;

        let prefix_path = ObjectPath::from(self.prefix.clone());
        let mut stream = self.client.list(Some(&prefix_path));

        let mut scanned = 0u64;
        let mut deleted = 0u64;
        let mut would_delete = 0u64;

        loop {
            let mut page: Vec<ObjectMeta> = Vec::with_capacity(PAGE_SIZE);
            while page.len() < PAGE_SIZE {
                match stream.next().await {
                    Some(Ok(meta)) => page.push(meta),
                    Some(Err(e)) => return Err(map_object_store_err(e, "orphan_sweep_list")),
                    None => break,
                }
            }
            if page.is_empty() {
                break;
            }
            let page_len = page.len();

            let mut to_delete: Vec<ObjectPath> = Vec::new();
            for meta in page {
                scanned += 1;
                let Some(content_ref) = self.parse_shard_key(&meta.location) else {
                    // Foreign or malformed key under the prefix -- never
                    // recognized as live or orphaned, mirroring
                    // FsBlobStore's handling of stray `.tmp-*` files.
                    continue;
                };
                if config.live_refs.contains(&content_ref) {
                    continue;
                }
                would_delete += 1;
                if !config.dry_run {
                    to_delete.push(meta.location);
                }
            }

            if !to_delete.is_empty() {
                let results: Vec<Result<(), ObjectStoreError>> =
                    futures::stream::iter(to_delete.into_iter().map(|path| {
                        let client = Arc::clone(&self.client);
                        async move { client.delete(&path).await }
                    }))
                    .buffer_unordered(DELETE_CONCURRENCY)
                    .collect()
                    .await;
                for r in results {
                    r.map_err(|e| map_object_store_err(e, "orphan_sweep_delete"))?;
                    deleted += 1;
                }
            }

            if page_len < PAGE_SIZE {
                break;
            }
        }

        Ok(BlobOrphanSweepResult {
            scanned,
            deleted,
            would_delete,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_config() -> S3BlobStoreConfig {
        S3BlobStoreConfig::new("khive-blobs", "us-east-1")
    }

    #[test]
    fn config_rejects_empty_bucket() {
        let mut cfg = valid_config();
        cfg.bucket = String::new();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn config_rejects_empty_region() {
        let mut cfg = valid_config();
        cfg.region = String::new();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn config_default_prefix_is_blobs() {
        assert_eq!(valid_config().prefix, "blobs");
    }

    #[test]
    fn config_rejects_leading_slash_prefix() {
        let cfg = valid_config().with_prefix("/blobs");
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn config_rejects_trailing_slash_prefix() {
        let cfg = valid_config().with_prefix("blobs/");
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn config_rejects_empty_prefix() {
        let cfg = valid_config().with_prefix("");
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn config_rejects_dot_segment_in_prefix() {
        let cfg = valid_config().with_prefix("a/./b");
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn config_rejects_dotdot_segment_in_prefix() {
        let cfg = valid_config().with_prefix("a/../b");
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn config_rejects_double_slash_prefix() {
        let cfg = valid_config().with_prefix("a//b");
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn config_accepts_nested_prefix() {
        let cfg = valid_config().with_prefix("a/b/c");
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn config_rejects_http_endpoint_without_allow_http() {
        let cfg = valid_config().with_endpoint("http://localhost:9000");
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn config_accepts_http_endpoint_with_allow_http() {
        let cfg = valid_config()
            .with_endpoint("http://localhost:9000")
            .with_allow_http(true);
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn config_accepts_https_endpoint_without_allow_http() {
        let cfg = valid_config().with_endpoint("https://objects.example.invalid");
        assert!(cfg.validate().is_ok());
    }

    // `std::env::set_var`/`remove_var` mutate real process-global state, so
    // the credential-precedence tests below must not interleave under the
    // crate's default parallel test runner.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn clear_aws_env() {
        std::env::remove_var("AWS_ACCESS_KEY_ID");
        std::env::remove_var("AWS_SECRET_ACCESS_KEY");
        std::env::remove_var("AWS_SESSION_TOKEN");
    }

    #[test]
    fn credentials_from_env_errors_when_both_missing() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_aws_env();
        let err = S3Credentials::from_env().unwrap_err();
        assert!(err.to_string().contains("AWS_ACCESS_KEY_ID"));
        clear_aws_env();
    }

    #[test]
    fn credentials_from_env_errors_when_only_access_key_set() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_aws_env();
        std::env::set_var("AWS_ACCESS_KEY_ID", "AKIAEXAMPLE");
        let err = S3Credentials::from_env().unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("AWS_SECRET_ACCESS_KEY"));
        assert!(
            !msg.contains("AKIAEXAMPLE"),
            "error must never contain a credential value: {msg}"
        );
        clear_aws_env();
    }

    #[test]
    fn credentials_from_env_errors_when_only_secret_key_set() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_aws_env();
        std::env::set_var("AWS_SECRET_ACCESS_KEY", "supersecret");
        let err = S3Credentials::from_env().unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("AWS_ACCESS_KEY_ID"));
        assert!(
            !msg.contains("supersecret"),
            "error must never contain a credential value: {msg}"
        );
        clear_aws_env();
    }

    #[test]
    fn credentials_from_env_accepts_the_required_pair() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_aws_env();
        std::env::set_var("AWS_ACCESS_KEY_ID", "AKIAEXAMPLE");
        std::env::set_var("AWS_SECRET_ACCESS_KEY", "supersecret");
        let creds = S3Credentials::from_env().unwrap();
        assert_eq!(creds.access_key_id, "AKIAEXAMPLE");
        assert_eq!(creds.secret_access_key, "supersecret");
        assert!(creds.session_token.is_none());
        clear_aws_env();
    }

    #[test]
    fn credentials_from_env_reads_optional_session_token() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_aws_env();
        std::env::set_var("AWS_ACCESS_KEY_ID", "AKIAEXAMPLE");
        std::env::set_var("AWS_SECRET_ACCESS_KEY", "supersecret");
        std::env::set_var("AWS_SESSION_TOKEN", "sessiontoken");
        let creds = S3Credentials::from_env().unwrap();
        assert_eq!(creds.session_token.as_deref(), Some("sessiontoken"));
        clear_aws_env();
    }

    fn store_for_key_tests() -> S3BlobStore {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_aws_env();
        std::env::set_var("AWS_ACCESS_KEY_ID", "AKIAEXAMPLE");
        std::env::set_var("AWS_SECRET_ACCESS_KEY", "supersecret");
        let store = S3BlobStore::new(valid_config()).unwrap();
        clear_aws_env();
        store
    }

    #[test]
    fn shard_key_matches_fs_blob_store_shape() {
        let store = store_for_key_tests();
        let content_ref = ContentRef::from_hex("a".repeat(64)).unwrap();
        let key = store.shard_key(&content_ref);
        let key_str: &str = key.as_ref();
        assert_eq!(key_str, format!("blobs/aa/aa/{}", "a".repeat(64)));
    }

    #[test]
    fn parse_shard_key_roundtrips_a_valid_key() {
        let store = store_for_key_tests();
        let content_ref = ContentRef::from_hex("b".repeat(64)).unwrap();
        let key = store.shard_key(&content_ref);
        assert_eq!(store.parse_shard_key(&key), Some(content_ref));
    }

    #[test]
    fn parse_shard_key_rejects_a_foreign_key_under_the_prefix() {
        let store = store_for_key_tests();
        let foreign = ObjectPath::from("blobs/README.txt");
        assert_eq!(store.parse_shard_key(&foreign), None);
    }

    #[test]
    fn parse_shard_key_rejects_mismatched_shard_segments() {
        let store = store_for_key_tests();
        let hex = "c".repeat(64);
        // Shard segments deliberately don't match the hex's own prefix.
        let bad = ObjectPath::from(format!("blobs/00/00/{hex}"));
        assert_eq!(store.parse_shard_key(&bad), None);
    }

    #[test]
    fn parse_shard_key_rejects_extra_path_segments() {
        let store = store_for_key_tests();
        let hex = "d".repeat(64);
        let bad = ObjectPath::from(format!("blobs/dd/dd/{hex}/extra"));
        assert_eq!(store.parse_shard_key(&bad), None);
    }

    #[test]
    fn parse_shard_key_rejects_key_outside_the_configured_prefix() {
        let store = store_for_key_tests();
        let hex = "e".repeat(64);
        let bad = ObjectPath::from(format!("other-prefix/ee/ee/{hex}"));
        assert_eq!(store.parse_shard_key(&bad), None);
    }
}
