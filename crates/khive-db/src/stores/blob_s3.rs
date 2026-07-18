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
use std::time::Duration;

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

/// Bounded outer-retry defaults for the idempotent content-addressed `put`
/// (ADR-111 Amendment 2). `object_store` classifies
/// `PutMode::Create` as non-idempotent and therefore never retries it
/// internally on a timeout -- a single slow request would otherwise surface
/// immediately as `StorageError::Driver` even though the object may have been
/// created. Content-addressed `put` IS safe to retry from the outside: the
/// same bytes always hash to the same key, and a retry that lands on an
/// `AlreadyExists` from the first attempt's own write is a dedup success, not
/// a conflict.
pub const DEFAULT_PUT_MAX_ATTEMPTS: u32 = 3;
/// Per-request deadline applied to every network call this store makes
/// (`head`, `get`, `put_opts`, `delete`). A call that does not resolve within
/// this window is treated as a timeout: `put`'s conditional create retries it
/// (if attempts remain) before mapping to `StorageError::Timeout`; every
/// other operation maps a timeout directly, since only the idempotent
/// content-addressed create is safe to retry from here.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Base delay for jittered exponential backoff between `put` retry attempts.
pub const DEFAULT_PUT_RETRY_BASE_DELAY: Duration = Duration::from_millis(100);

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

fn timeout_error(op: &'static str) -> StorageError {
    StorageError::Timeout {
        operation: op.into(),
    }
}

/// True for the `object_store::Error` shape produced once a request has
/// exhausted `object_store`'s own HTTP/transport retry budget (its
/// `client::retry` module -- private to that crate, so its `RetryError`
/// cannot be downcast to directly -- maps a timeout, connection failure, or
/// exhausted 5xx into this catch-all variant when no more specific HTTP
/// status applies). This is the only shape our own outer retry treats as
/// transient: `PermissionDenied`/`Unauthenticated` are credential failures
/// that a retry cannot fix (ADR-111 Amendment 2: never retried), and the
/// remaining variants (`InvalidPath`, `NotSupported`, `NotImplemented`,
/// `UnknownConfigurationKey`, `Precondition`, `NotModified`) are
/// request-shape errors, not transient ones.
fn is_outer_retryable(e: &ObjectStoreError) -> bool {
    matches!(e, ObjectStoreError::Generic { .. })
}

/// Jittered exponential backoff delay before retry attempt `attempt` (1-based:
/// the delay before the *second* attempt uses `attempt = 1`).
fn retry_backoff(base: Duration, attempt: u32) -> Duration {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    // 1.0x-2.0x jitter multiplier, scaled by the attempt number for a simple
    // exponential-ish backoff without pulling in a `rand` runtime dependency.
    let jitter = 1.0 + (nanos % 1000) as f64 / 1000.0;
    base.mul_f64(jitter * attempt as f64)
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
#[derive(Debug)]
pub struct S3BlobStore {
    client: Arc<dyn ObjectStore>,
    prefix: String,
    put_max_attempts: u32,
    request_timeout: Duration,
    put_retry_base_delay: Duration,
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
            put_max_attempts: DEFAULT_PUT_MAX_ATTEMPTS,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            put_retry_base_delay: DEFAULT_PUT_RETRY_BASE_DELAY,
        })
    }

    /// Test-only seam: build a store around an injected `ObjectStore` (a fake
    /// client) instead of a real `AmazonS3`, with retry/timeout parameters
    /// small enough for a unit test to exercise exhaustion paths quickly.
    /// `ObjectStore` (from `object_store`, not a khive-authored trait) is
    /// already the natural seam here -- `S3BlobStore` only ever holds an
    /// `Arc<dyn ObjectStore>`, never a concrete `AmazonS3`.
    #[cfg(test)]
    fn from_client_for_test(
        client: Arc<dyn ObjectStore>,
        prefix: impl Into<String>,
        put_max_attempts: u32,
        request_timeout: Duration,
        put_retry_base_delay: Duration,
    ) -> Result<Self, SqliteError> {
        let prefix = prefix.into();
        validate_prefix(&prefix)?;
        Ok(Self {
            client,
            prefix,
            put_max_attempts,
            request_timeout,
            put_retry_base_delay,
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
        // makes this put a no-op, same contract as FsBlobStore. A single
        // timed-out HEAD is not retried here -- the conditional create below
        // covers the same dedup outcome (`AlreadyExists`) if a HEAD timeout
        // masked an object that already exists.
        match tokio::time::timeout(self.request_timeout, self.client.head(&key)).await {
            Ok(Ok(_)) => return Ok(content_ref),
            Ok(Err(ObjectStoreError::NotFound { .. })) => {}
            Ok(Err(e)) => return Err(map_object_store_err(e, "put_head")),
            Err(_elapsed) => {
                return Err(StorageError::Timeout {
                    operation: "put_head".into(),
                });
            }
        }

        // Bounded outer retry around the conditional create (ADR-111
        // Amendment 2). `object_store` marks `PutMode::Create`
        // non-idempotent and never retries it internally on a timeout, so a
        // single slow request would otherwise surface immediately as
        // `Driver` even though the service may have accepted the write. The
        // create IS safe to retry from here: it is content-addressed, so a
        // retry either lands its own create or observes `AlreadyExists` from
        // an attempt that actually succeeded -- both are dedup success.
        let mut attempt: u32 = 0;
        loop {
            attempt += 1;
            // Rebuild the payload each attempt: `PutPayload` is consumed by
            // `put_opts`, and `Bytes::clone()` is a cheap refcount bump, not
            // a copy, so re-deriving it from `bytes` costs nothing material.
            let payload = PutPayload::from(bytes.clone());
            let put_fut = self.client.put_opts(&key, payload, PutMode::Create.into());
            match tokio::time::timeout(self.request_timeout, put_fut).await {
                Ok(Ok(_)) => return Ok(content_ref),
                // A concurrent identical writer published this exact
                // content-addressed key (from another caller, or from a
                // prior attempt of this same retry loop that actually
                // succeeded despite a timed-out response). CAS semantics
                // make that a success, not a conflict.
                Ok(Err(ObjectStoreError::AlreadyExists { .. })) => return Ok(content_ref),
                // Exhausted the retry budget on a transient (non-timeout)
                // error: fall through to the catch-all arm below, which
                // preserves this final source under `Driver`.
                Ok(Err(e)) if is_outer_retryable(&e) && attempt < self.put_max_attempts => {
                    tokio::time::sleep(retry_backoff(self.put_retry_base_delay, attempt)).await;
                    continue;
                }
                Ok(Err(e)) => return Err(map_object_store_err(e, "put")),
                Err(_elapsed) if attempt < self.put_max_attempts => {
                    tokio::time::sleep(retry_backoff(self.put_retry_base_delay, attempt)).await;
                    continue;
                }
                // Exhausted the retry budget and the final attempt was itself
                // a timeout: the deadline, not a transient failure, is what
                // gave up -- classify as `Timeout`, not `Driver`.
                Err(_elapsed) => {
                    return Err(StorageError::Timeout {
                        operation: "put".into(),
                    });
                }
            }
        }
    }

    async fn get(&self, content_ref: &ContentRef) -> StorageResult<Vec<u8>> {
        let key = self.shard_key(content_ref);
        let result = match tokio::time::timeout(self.request_timeout, self.client.get(&key)).await {
            Ok(Ok(result)) => result,
            Ok(Err(ObjectStoreError::NotFound { .. })) => {
                return Err(StorageError::NotFound {
                    capability: StorageCapability::Blob,
                    resource: "blob",
                    key: content_ref.to_string(),
                });
            }
            Ok(Err(other)) => return Err(map_object_store_err(other, "get")),
            Err(_elapsed) => return Err(timeout_error("get")),
        };

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

        let bytes = match tokio::time::timeout(self.request_timeout, result.bytes()).await {
            Ok(Ok(bytes)) => bytes,
            Ok(Err(e)) => return Err(map_object_store_err(e, "get")),
            Err(_elapsed) => return Err(timeout_error("get")),
        };
        Ok(bytes.to_vec())
    }

    async fn exists(&self, content_ref: &ContentRef) -> StorageResult<bool> {
        let key = self.shard_key(content_ref);
        match tokio::time::timeout(self.request_timeout, self.client.head(&key)).await {
            Ok(Ok(_)) => Ok(true),
            Ok(Err(ObjectStoreError::NotFound { .. })) => Ok(false),
            Ok(Err(e)) => Err(map_object_store_err(e, "exists")),
            Err(_elapsed) => Err(timeout_error("exists")),
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
        match tokio::time::timeout(self.request_timeout, self.client.head(&key)).await {
            Ok(Ok(_)) => {}
            Ok(Err(ObjectStoreError::NotFound { .. })) => return Ok(false),
            Ok(Err(e)) => return Err(map_object_store_err(e, "delete_head")),
            Err(_elapsed) => return Err(timeout_error("delete_head")),
        }
        match tokio::time::timeout(self.request_timeout, self.client.delete(&key)).await {
            Ok(Ok(_)) => Ok(true),
            Ok(Err(e)) => Err(map_object_store_err(e, "delete")),
            Err(_elapsed) => Err(timeout_error("delete")),
        }
    }

    // Offline-maintenance-only -- see `BlobStore::orphan_sweep`'s doc
    // comment for the concurrency hazard. `config.live_refs` is a snapshot;
    // this method performs no DB coordination, only a diff against whatever
    // set the caller handed it. Pagination is bounded (at most
    // `PAGE_SIZE` remote-listing entries held at once) and deletes run at
    // bounded concurrency; a list or delete failure aborts with an error
    // rather than reporting a partial scan as complete.
    //
    // Every network step -- each `stream.next()` poll and each concurrent
    // delete -- is wrapped in `self.request_timeout`, same as `put`/`get`/
    // `exists`/`delete` above (ADR-111 Amendment 2): a
    // deadline elapsing maps to `StorageError::Timeout`, and any other
    // error the provider returns keeps the `Driver` classification.
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
                match tokio::time::timeout(self.request_timeout, stream.next()).await {
                    Ok(Some(Ok(meta))) => page.push(meta),
                    Ok(Some(Err(e))) => {
                        return Err(map_object_store_err(e, "orphan_sweep_list"));
                    }
                    Ok(None) => break,
                    Err(_elapsed) => return Err(timeout_error("orphan_sweep_list")),
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
                let request_timeout = self.request_timeout;
                let results: Vec<StorageResult<()>> =
                    futures::stream::iter(to_delete.into_iter().map(|path| {
                        let client = Arc::clone(&self.client);
                        async move {
                            match tokio::time::timeout(request_timeout, client.delete(&path)).await
                            {
                                Ok(Ok(())) => Ok(()),
                                Ok(Err(e)) => Err(map_object_store_err(e, "orphan_sweep_delete")),
                                Err(_elapsed) => Err(timeout_error("orphan_sweep_delete")),
                            }
                        }
                    }))
                    .buffer_unordered(DELETE_CONCURRENCY)
                    .collect()
                    .await;
                for r in results {
                    r?;
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

    // ── Fake-client error-mapping tests (ADR-111 Amendment 2) ─
    //
    // `S3BlobStore` only ever holds an `Arc<dyn ObjectStore>` -- `ObjectStore`
    // (from the `object_store` crate, not a khive-authored trait) is already
    // the seam. `FakeObjectStore` implements it with fully scripted
    // outcomes, so these tests exercise the retry/timeout/classification
    // logic in `put`/`get`/`exists` without any network dependency.

    mod fake_client {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Mutex;

        use bytes::Bytes;
        use futures::stream::{self, BoxStream};
        use object_store::{
            GetOptions, GetResult, GetResultPayload, ListResult, MultipartUpload,
            PutMultipartOptions, PutOptions, PutResult,
        };

        use super::*;

        /// One scripted outcome for a single call to the fake's `put_opts`
        /// or `get_opts`.
        #[derive(Clone, Debug)]
        pub enum Outcome {
            Ok,
            NotFound,
            AlreadyExists,
            PermissionDenied,
            /// The catch-all shape a real exhausted/timed-out request lands
            /// in once `object_store`'s own retry budget is spent (see
            /// `is_outer_retryable`'s doc comment) -- what our own outer
            /// retry treats as transient.
            Generic,
            /// Never resolves within the test's configured
            /// `request_timeout`, so the caller's `tokio::time::timeout`
            /// elapses first.
            Hang,
        }

        fn outcome_to_result<T>(outcome: &Outcome, make_ok: impl FnOnce() -> T) -> Result<T> {
            match outcome {
                Outcome::Ok => Ok(make_ok()),
                Outcome::NotFound => Err(ObjectStoreError::NotFound {
                    path: "fake".into(),
                    source: "not found".into(),
                }),
                Outcome::AlreadyExists => Err(ObjectStoreError::AlreadyExists {
                    path: "fake".into(),
                    source: "already exists".into(),
                }),
                Outcome::PermissionDenied => Err(ObjectStoreError::PermissionDenied {
                    path: "fake".into(),
                    source: "access denied".into(),
                }),
                Outcome::Generic => Err(ObjectStoreError::Generic {
                    store: "fake",
                    source: "exhausted transient failure".into(),
                }),
                Outcome::Hang => unreachable!("Hang must be intercepted before this call"),
            }
        }

        type Result<T> = std::result::Result<T, ObjectStoreError>;

        /// One scripted outcome for a single entry the fake's `list` stream
        /// yields (ADR-111 Amendment 2 -- partial-page
        /// error/timeout coverage for `orphan_sweep`).
        #[derive(Clone, Debug)]
        pub enum ListOutcome {
            /// Yield one valid `ObjectMeta` at this object-store key.
            Entry(String),
            /// The list stream errors at this position (mid-page failure).
            Err,
            /// Never resolves within the test's configured `request_timeout`.
            Hang,
        }

        /// A scripted `ObjectStore` fake. `put_script`/`get_opts_script` are
        /// consumed in order, one entry per call; running past the end of a
        /// script repeats its last entry (so a short script can still cover
        /// an unbounded outer-retry loop in a test). `list_script` and
        /// `delete_script` are Arc-wrapped because `delete_stream`/`list`
        /// must return a `'static` stream that outlives the `&self` borrow.
        #[derive(Debug)]
        pub struct FakeObjectStore {
            put_script: Mutex<Vec<Outcome>>,
            get_script: Mutex<Vec<Outcome>>,
            list_script: Arc<Mutex<Vec<ListOutcome>>>,
            delete_script: Arc<Mutex<Vec<Outcome>>>,
            pub put_calls: AtomicUsize,
            pub get_calls: AtomicUsize,
            pub delete_calls: Arc<AtomicUsize>,
            /// How long a `Hang` outcome sleeps before resolving --
            /// deliberately far longer than the test's configured
            /// `request_timeout`, so `tokio::time::timeout` always wins the
            /// race deterministically instead of depending on scheduler
            /// timing.
            hang_delay: Duration,
        }

        impl std::fmt::Display for FakeObjectStore {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "FakeObjectStore")
            }
        }

        impl FakeObjectStore {
            pub fn new(put_script: Vec<Outcome>, get_script: Vec<Outcome>) -> Self {
                Self {
                    put_script: Mutex::new(put_script),
                    get_script: Mutex::new(get_script),
                    list_script: Arc::new(Mutex::new(Vec::new())),
                    delete_script: Arc::new(Mutex::new(vec![Outcome::Ok])),
                    put_calls: AtomicUsize::new(0),
                    get_calls: AtomicUsize::new(0),
                    delete_calls: Arc::new(AtomicUsize::new(0)),
                    hang_delay: Duration::from_secs(3600),
                }
            }

            /// Script the fake's `list` stream: the sweep
            /// tests below use this to yield N valid entries then an error,
            /// or a `Hang` entry to exercise the per-page timeout.
            pub fn with_list_script(self, script: Vec<ListOutcome>) -> Self {
                *self.list_script.lock().unwrap() = script;
                self
            }

            /// Script the fake's `delete` outcomes. Defaults
            /// to always-`Ok` so existing sweep behavior needs no script.
            pub fn with_delete_script(self, script: Vec<Outcome>) -> Self {
                *self.delete_script.lock().unwrap() = script;
                self
            }

            fn next_outcome(script: &Mutex<Vec<Outcome>>, calls: &AtomicUsize) -> Outcome {
                let idx = calls.fetch_add(1, Ordering::SeqCst);
                let script = script.lock().unwrap();
                script[idx.min(script.len().saturating_sub(1))].clone()
            }
        }

        #[async_trait::async_trait]
        impl ObjectStore for FakeObjectStore {
            async fn put_opts(
                &self,
                _location: &ObjectPath,
                _payload: PutPayload,
                _opts: PutOptions,
            ) -> Result<PutResult> {
                let outcome = Self::next_outcome(&self.put_script, &self.put_calls);
                if matches!(outcome, Outcome::Hang) {
                    tokio::time::sleep(self.hang_delay).await;
                }
                outcome_to_result(&outcome, || PutResult {
                    e_tag: None,
                    version: None,
                    extensions: Default::default(),
                })
            }

            async fn put_multipart_opts(
                &self,
                _location: &ObjectPath,
                _opts: PutMultipartOptions,
            ) -> Result<Box<dyn MultipartUpload>> {
                unimplemented!("not exercised by these tests")
            }

            async fn get_opts(
                &self,
                location: &ObjectPath,
                _opts: GetOptions,
            ) -> Result<GetResult> {
                let outcome = Self::next_outcome(&self.get_script, &self.get_calls);
                if matches!(outcome, Outcome::Hang) {
                    tokio::time::sleep(self.hang_delay).await;
                }
                outcome_to_result(&outcome, || GetResult {
                    payload: GetResultPayload::Stream(
                        stream::once(async { Ok(Bytes::new()) }).boxed(),
                    ),
                    meta: ObjectMeta {
                        location: location.clone(),
                        last_modified: chrono::Utc::now(),
                        size: 0,
                        e_tag: None,
                        version: None,
                    },
                    range: 0..0,
                    attributes: Default::default(),
                    extensions: Default::default(),
                })
            }

            fn delete_stream(
                &self,
                locations: BoxStream<'static, Result<ObjectPath>>,
            ) -> BoxStream<'static, Result<ObjectPath>> {
                let delete_script = Arc::clone(&self.delete_script);
                let delete_calls = Arc::clone(&self.delete_calls);
                let hang_delay = self.hang_delay;
                locations
                    .then(move |loc_result| {
                        let delete_script = Arc::clone(&delete_script);
                        let delete_calls = Arc::clone(&delete_calls);
                        async move {
                            let location = loc_result?;
                            let outcome =
                                Self::next_outcome(delete_script.as_ref(), delete_calls.as_ref());
                            if matches!(outcome, Outcome::Hang) {
                                tokio::time::sleep(hang_delay).await;
                            }
                            outcome_to_result(&outcome, || location.clone())
                        }
                    })
                    .boxed()
            }

            fn list(&self, _prefix: Option<&ObjectPath>) -> BoxStream<'static, Result<ObjectMeta>> {
                let script = self.list_script.lock().unwrap().clone();
                let hang_delay = self.hang_delay;
                if script.is_empty() {
                    return stream::empty().boxed();
                }
                stream::iter(script)
                    .then(move |outcome| async move {
                        match outcome {
                            ListOutcome::Entry(location) => Ok(ObjectMeta {
                                location: ObjectPath::from(location),
                                last_modified: chrono::Utc::now(),
                                size: 0,
                                e_tag: None,
                                version: None,
                            }),
                            ListOutcome::Err => Err(ObjectStoreError::Generic {
                                store: "fake",
                                source: "list exhausted transient failure".into(),
                            }),
                            ListOutcome::Hang => {
                                tokio::time::sleep(hang_delay).await;
                                unreachable!("Hang must be intercepted before this resolves")
                            }
                        }
                    })
                    .boxed()
            }

            async fn list_with_delimiter(
                &self,
                _prefix: Option<&ObjectPath>,
            ) -> Result<ListResult> {
                unimplemented!("not exercised by these tests")
            }

            async fn copy_opts(
                &self,
                _from: &ObjectPath,
                _to: &ObjectPath,
                _opts: object_store::CopyOptions,
            ) -> Result<()> {
                unimplemented!("not exercised by these tests")
            }
        }
    }

    use fake_client::{FakeObjectStore, ListOutcome, Outcome};
    use std::sync::atomic::Ordering;

    fn fake_store(
        put_script: Vec<Outcome>,
        get_script: Vec<Outcome>,
    ) -> (Arc<FakeObjectStore>, S3BlobStore) {
        let fake = Arc::new(FakeObjectStore::new(put_script, get_script));
        let store = S3BlobStore::from_client_for_test(
            Arc::clone(&fake) as Arc<dyn ObjectStore>,
            "blobs",
            3,
            Duration::from_millis(50),
            Duration::from_millis(1),
        )
        .unwrap();
        (fake, store)
    }

    #[tokio::test]
    async fn put_succeeds_on_first_attempt() {
        let (fake, store) = fake_store(vec![Outcome::Ok], vec![Outcome::NotFound]);
        let result = store.put(b"hello".to_vec()).await;
        assert!(result.is_ok());
        assert_eq!(fake.put_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn put_already_exists_is_dedup_success_not_conflict() {
        let (_fake, store) = fake_store(vec![Outcome::AlreadyExists], vec![Outcome::NotFound]);
        let result = store.put(b"hello".to_vec()).await;
        assert!(result.is_ok(), "AlreadyExists must be treated as success");
    }

    #[tokio::test]
    async fn put_permission_denied_maps_to_driver_without_retry() {
        let (fake, store) = fake_store(vec![Outcome::PermissionDenied], vec![Outcome::NotFound]);
        let err = store.put(b"hello".to_vec()).await.unwrap_err();
        assert!(matches!(err, StorageError::Driver { .. }), "got {err:?}");
        assert_eq!(
            fake.put_calls.load(Ordering::SeqCst),
            1,
            "a credential failure must never be retried"
        );
    }

    #[tokio::test]
    async fn put_exhausted_transient_failure_maps_to_driver() {
        let (fake, store) = fake_store(vec![Outcome::Generic], vec![Outcome::NotFound]);
        let err = store.put(b"hello".to_vec()).await.unwrap_err();
        assert!(matches!(err, StorageError::Driver { .. }), "got {err:?}");
        assert_eq!(
            fake.put_calls.load(Ordering::SeqCst),
            3,
            "must retry up to the configured max attempts before giving up"
        );
    }

    #[tokio::test]
    async fn put_transient_then_success_recovers_within_the_retry_budget() {
        let (fake, store) =
            fake_store(vec![Outcome::Generic, Outcome::Ok], vec![Outcome::NotFound]);
        let result = store.put(b"hello".to_vec()).await;
        assert!(result.is_ok());
        assert_eq!(fake.put_calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn put_exhausted_timeout_maps_to_timeout_not_driver() {
        let (fake, store) = fake_store(vec![Outcome::Hang], vec![Outcome::NotFound]);
        let err = store.put(b"hello".to_vec()).await.unwrap_err();
        assert!(matches!(err, StorageError::Timeout { .. }), "got {err:?}");
        assert_eq!(fake.put_calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn get_not_found_maps_to_storage_not_found() {
        let (_fake, store) = fake_store(vec![], vec![Outcome::NotFound]);
        let content_ref = ContentRef::from_hex("a".repeat(64)).unwrap();
        let err = store.get(&content_ref).await.unwrap_err();
        assert!(matches!(err, StorageError::NotFound { .. }), "got {err:?}");
    }

    #[tokio::test]
    async fn get_timeout_maps_to_storage_timeout() {
        let (_fake, store) = fake_store(vec![], vec![Outcome::Hang]);
        let content_ref = ContentRef::from_hex("b".repeat(64)).unwrap();
        let err = store.get(&content_ref).await.unwrap_err();
        assert!(matches!(err, StorageError::Timeout { .. }), "got {err:?}");
    }

    #[tokio::test]
    async fn exists_authorization_failure_maps_to_driver() {
        let (_fake, store) = fake_store(vec![], vec![Outcome::PermissionDenied]);
        let content_ref = ContentRef::from_hex("c".repeat(64)).unwrap();
        let err = store.exists(&content_ref).await.unwrap_err();
        assert!(matches!(err, StorageError::Driver { .. }), "got {err:?}");
    }

    #[tokio::test]
    async fn exists_timeout_maps_to_storage_timeout() {
        let (_fake, store) = fake_store(vec![], vec![Outcome::Hang]);
        let content_ref = ContentRef::from_hex("d".repeat(64)).unwrap();
        let err = store.exists(&content_ref).await.unwrap_err();
        assert!(matches!(err, StorageError::Timeout { .. }), "got {err:?}");
    }

    // ── `orphan_sweep` timeout/error-mapping tests (ADR-111 Amendment 2) ──

    fn valid_shard_key(byte: char) -> String {
        let hex = byte.to_string().repeat(64);
        format!("blobs/{}/{}/{}", &hex[0..2], &hex[2..4], hex)
    }

    fn fake_sweep_store(list_script: Vec<ListOutcome>) -> (Arc<FakeObjectStore>, S3BlobStore) {
        let fake = Arc::new(FakeObjectStore::new(vec![], vec![]).with_list_script(list_script));
        let store = S3BlobStore::from_client_for_test(
            Arc::clone(&fake) as Arc<dyn ObjectStore>,
            "blobs",
            3,
            Duration::from_millis(50),
            Duration::from_millis(1),
        )
        .unwrap();
        (fake, store)
    }

    #[tokio::test]
    async fn orphan_sweep_list_timeout_maps_to_storage_timeout() {
        let (_fake, store) = fake_sweep_store(vec![ListOutcome::Hang]);
        let config = BlobOrphanSweepConfig {
            live_refs: Default::default(),
            dry_run: true,
        };
        let err = store.orphan_sweep(&config).await.unwrap_err();
        assert!(matches!(err, StorageError::Timeout { .. }), "got {err:?}");
    }

    #[tokio::test]
    async fn orphan_sweep_partial_page_then_error_returns_err_not_partial_success() {
        // Two valid entries followed by a list error -- the sweep must
        // never report the two valid entries as a completed (partial) scan.
        let (_fake, store) = fake_sweep_store(vec![
            ListOutcome::Entry(valid_shard_key('1')),
            ListOutcome::Entry(valid_shard_key('2')),
            ListOutcome::Err,
        ]);
        let config = BlobOrphanSweepConfig {
            live_refs: Default::default(),
            dry_run: true,
        };
        let err = store.orphan_sweep(&config).await.unwrap_err();
        assert!(matches!(err, StorageError::Driver { .. }), "got {err:?}");
    }

    #[tokio::test]
    async fn orphan_sweep_delete_timeout_maps_to_storage_timeout() {
        let fake = Arc::new(
            FakeObjectStore::new(vec![], vec![])
                .with_list_script(vec![ListOutcome::Entry(valid_shard_key('3'))])
                .with_delete_script(vec![Outcome::Hang]),
        );
        let store = S3BlobStore::from_client_for_test(
            Arc::clone(&fake) as Arc<dyn ObjectStore>,
            "blobs",
            3,
            Duration::from_millis(50),
            Duration::from_millis(1),
        )
        .unwrap();
        // Empty `live_refs`: the single scripted entry is orphaned, so a
        // non-dry-run sweep issues exactly one delete, which hangs past the
        // configured request_timeout.
        let config = BlobOrphanSweepConfig {
            live_refs: Default::default(),
            dry_run: false,
        };
        let err = store.orphan_sweep(&config).await.unwrap_err();
        assert!(matches!(err, StorageError::Timeout { .. }), "got {err:?}");
    }
}
