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

// Retain the credential fixture lock within each isolated child. Store
// construction receives fixed credentials before child startup.
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn clear_aws_env() {
    crate::test_process::remove_var("AWS_ACCESS_KEY_ID");
    crate::test_process::remove_var("AWS_SECRET_ACCESS_KEY");
    crate::test_process::remove_var("AWS_SESSION_TOKEN");
}

#[test]
fn credentials_from_env_errors_when_both_missing() {
    if crate::test_process::run_in_child(|_| {}) {
        return;
    }
    let _guard = ENV_LOCK.lock().unwrap();
    clear_aws_env();
    let err = S3Credentials::from_env().unwrap_err();
    assert!(err.to_string().contains("AWS_ACCESS_KEY_ID"));
    clear_aws_env();
}

#[test]
fn credentials_from_env_errors_when_only_access_key_set() {
    if crate::test_process::run_in_child(|_| {}) {
        return;
    }
    let _guard = ENV_LOCK.lock().unwrap();
    clear_aws_env();
    crate::test_process::set_var("AWS_ACCESS_KEY_ID", "AKIAEXAMPLE");
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
    if crate::test_process::run_in_child(|_| {}) {
        return;
    }
    let _guard = ENV_LOCK.lock().unwrap();
    clear_aws_env();
    crate::test_process::set_var("AWS_SECRET_ACCESS_KEY", "supersecret");
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
    if crate::test_process::run_in_child(|_| {}) {
        return;
    }
    let _guard = ENV_LOCK.lock().unwrap();
    clear_aws_env();
    crate::test_process::set_var("AWS_ACCESS_KEY_ID", "AKIAEXAMPLE");
    crate::test_process::set_var("AWS_SECRET_ACCESS_KEY", "supersecret");
    let creds = S3Credentials::from_env().unwrap();
    assert_eq!(creds.access_key_id, "AKIAEXAMPLE");
    assert_eq!(creds.secret_access_key, "supersecret");
    assert!(creds.session_token.is_none());
    clear_aws_env();
}

#[test]
fn credentials_from_env_reads_optional_session_token() {
    if crate::test_process::run_in_child(|_| {}) {
        return;
    }
    let _guard = ENV_LOCK.lock().unwrap();
    clear_aws_env();
    crate::test_process::set_var("AWS_ACCESS_KEY_ID", "AKIAEXAMPLE");
    crate::test_process::set_var("AWS_SECRET_ACCESS_KEY", "supersecret");
    crate::test_process::set_var("AWS_SESSION_TOKEN", "sessiontoken");
    let creds = S3Credentials::from_env().unwrap();
    assert_eq!(creds.session_token.as_deref(), Some("sessiontoken"));
    clear_aws_env();
}

fn store_for_key_tests() -> S3BlobStore {
    let _guard = ENV_LOCK.lock().unwrap();
    S3BlobStore::new(valid_config()).unwrap()
}

#[test]
fn shard_key_matches_fs_blob_store_shape() {
    if crate::test_process::run_in_child(|command| {
        command
            .env("AWS_ACCESS_KEY_ID", "AKIAEXAMPLE")
            .env("AWS_SECRET_ACCESS_KEY", "supersecret")
            .env_remove("AWS_SESSION_TOKEN");
    }) {
        return;
    }
    let store = store_for_key_tests();
    let content_ref = ContentRef::from_hex("a".repeat(64)).unwrap();
    let key = store.shard_key(&content_ref);
    let key_str: &str = key.as_ref();
    assert_eq!(key_str, format!("blobs/aa/aa/{}", "a".repeat(64)));
}

#[test]
fn parse_shard_key_roundtrips_a_valid_key() {
    if crate::test_process::run_in_child(|command| {
        command
            .env("AWS_ACCESS_KEY_ID", "AKIAEXAMPLE")
            .env("AWS_SECRET_ACCESS_KEY", "supersecret")
            .env_remove("AWS_SESSION_TOKEN");
    }) {
        return;
    }
    let store = store_for_key_tests();
    let content_ref = ContentRef::from_hex("b".repeat(64)).unwrap();
    let key = store.shard_key(&content_ref);
    assert_eq!(store.parse_shard_key(&key), Some(content_ref));
}

#[test]
fn parse_shard_key_rejects_a_foreign_key_under_the_prefix() {
    if crate::test_process::run_in_child(|command| {
        command
            .env("AWS_ACCESS_KEY_ID", "AKIAEXAMPLE")
            .env("AWS_SECRET_ACCESS_KEY", "supersecret")
            .env_remove("AWS_SESSION_TOKEN");
    }) {
        return;
    }
    let store = store_for_key_tests();
    let foreign = ObjectPath::from("blobs/README.txt");
    assert_eq!(store.parse_shard_key(&foreign), None);
}

#[test]
fn parse_shard_key_rejects_mismatched_shard_segments() {
    if crate::test_process::run_in_child(|command| {
        command
            .env("AWS_ACCESS_KEY_ID", "AKIAEXAMPLE")
            .env("AWS_SECRET_ACCESS_KEY", "supersecret")
            .env_remove("AWS_SESSION_TOKEN");
    }) {
        return;
    }
    let store = store_for_key_tests();
    let hex = "c".repeat(64);
    // Shard segments deliberately don't match the hex's own prefix.
    let bad = ObjectPath::from(format!("blobs/00/00/{hex}"));
    assert_eq!(store.parse_shard_key(&bad), None);
}

#[test]
fn parse_shard_key_rejects_extra_path_segments() {
    if crate::test_process::run_in_child(|command| {
        command
            .env("AWS_ACCESS_KEY_ID", "AKIAEXAMPLE")
            .env("AWS_SECRET_ACCESS_KEY", "supersecret")
            .env_remove("AWS_SESSION_TOKEN");
    }) {
        return;
    }
    let store = store_for_key_tests();
    let hex = "d".repeat(64);
    let bad = ObjectPath::from(format!("blobs/dd/dd/{hex}/extra"));
    assert_eq!(store.parse_shard_key(&bad), None);
}

#[test]
fn parse_shard_key_rejects_key_outside_the_configured_prefix() {
    if crate::test_process::run_in_child(|command| {
        command
            .env("AWS_ACCESS_KEY_ID", "AKIAEXAMPLE")
            .env("AWS_SECRET_ACCESS_KEY", "supersecret")
            .env_remove("AWS_SESSION_TOKEN");
    }) {
        return;
    }
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
// logic in `put`/`get_bounded_verified`/`exists` without any network
// dependency.

mod fake_client {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    use bytes::Bytes;
    use futures::stream::{self, BoxStream};
    use object_store::{
        GetOptions, GetResult, GetResultPayload, ListResult, MultipartUpload, PutMultipartOptions,
        PutOptions, PutResult,
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
        pub(super) put_payloads: Mutex<Vec<Bytes>>,
        get_script: Mutex<Vec<Outcome>>,
        list_script: Arc<Mutex<Vec<ListOutcome>>>,
        delete_script: Arc<Mutex<Vec<Outcome>>>,
        /// Overrides the default empty-body `Outcome::Ok` response for
        /// `get_opts`: `(reported_meta_size, pre-chunked stream body,
        /// per-chunk delay)`. Lets tests simulate an object store whose
        /// response metadata under-reports the real body length
        /// (ADR-160: bounded verified reads must cap the actual stream,
        /// not trust this), control exactly how many
        /// `stream.next()` calls the body arrives over (e.g. one
        /// oversized chunk vs many small ones), and — via the delay —
        /// simulate a peer trickling bytes slower than the total
        /// hydration deadline while still resolving each individual
        /// `stream.next()` call well within it (end-to-end vs per-chunk
        /// deadline coverage).
        get_body_override: Mutex<Option<(u64, Vec<Bytes>, Duration)>>,
        /// Sleep applied inside `get_opts` before resolving an `Ok`
        /// outcome — simulates a slow initial request that still
        /// resolves within `request_timeout`, so tests can prove the
        /// hydration deadline spans request + consume rather than
        /// granting each phase a fresh window.
        get_pre_delay: Mutex<Option<Duration>>,
        pub put_calls: AtomicUsize,
        /// Total `get_opts` calls, retained for existing retry/error
        /// tests whose script is shared by GET and HEAD.
        pub get_calls: AtomicUsize,
        pub head_calls: AtomicUsize,
        pub body_get_calls: AtomicUsize,
        pub body_polls: Arc<AtomicUsize>,
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
                put_payloads: Mutex::new(Vec::new()),
                get_script: Mutex::new(get_script),
                list_script: Arc::new(Mutex::new(Vec::new())),
                delete_script: Arc::new(Mutex::new(vec![Outcome::Ok])),
                get_body_override: Mutex::new(None),
                get_pre_delay: Mutex::new(None),
                put_calls: AtomicUsize::new(0),
                get_calls: AtomicUsize::new(0),
                head_calls: AtomicUsize::new(0),
                body_get_calls: AtomicUsize::new(0),
                body_polls: Arc::new(AtomicUsize::new(0)),
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

        /// Override the body an `Outcome::Ok` `get_opts` response
        /// streams, independent of the `reported_size` its `ObjectMeta`
        /// claims — used to simulate an object store whose response
        /// metadata under-reports the real streamed length. `chunks`
        /// controls exactly how many `stream.next()` calls the body
        /// arrives over (e.g. one oversized chunk vs many small ones),
        /// so tests can exercise the running-total bound both within a
        /// single chunk and across several.
        pub fn with_get_body_chunks(self, reported_size: u64, chunks: Vec<Bytes>) -> Self {
            *self.get_body_override.lock().unwrap() = Some((reported_size, chunks, Duration::ZERO));
            self
        }

        /// Same as [`Self::with_get_body_chunks`], but sleeps
        /// `per_chunk_delay` before yielding each chunk from the stream —
        /// lets a test drive each `stream.next()` call under a per-chunk
        /// timeout while the whole stream still exceeds an end-to-end one.
        pub fn with_get_body_chunks_delayed(
            self,
            reported_size: u64,
            chunks: Vec<Bytes>,
            per_chunk_delay: Duration,
        ) -> Self {
            *self.get_body_override.lock().unwrap() =
                Some((reported_size, chunks, per_chunk_delay));
            self
        }

        /// Sleep `delay` inside `get_opts` before resolving an `Ok`
        /// outcome (see `get_pre_delay`).
        pub fn with_get_pre_delay(self, delay: Duration) -> Self {
            *self.get_pre_delay.lock().unwrap() = Some(delay);
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
            payload: PutPayload,
            _opts: PutOptions,
        ) -> Result<PutResult> {
            self.put_payloads
                .lock()
                .unwrap()
                .push(payload.iter().next().cloned().unwrap_or_default());
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

        async fn get_opts(&self, location: &ObjectPath, opts: GetOptions) -> Result<GetResult> {
            if opts.head {
                self.head_calls.fetch_add(1, Ordering::SeqCst);
            } else {
                self.body_get_calls.fetch_add(1, Ordering::SeqCst);
            }
            let outcome = Self::next_outcome(&self.get_script, &self.get_calls);
            if matches!(outcome, Outcome::Hang) {
                tokio::time::sleep(self.hang_delay).await;
            }
            let pre_delay = *self.get_pre_delay.lock().unwrap();
            if let Some(delay) = pre_delay {
                tokio::time::sleep(delay).await;
            }
            let override_body = self.get_body_override.lock().unwrap().clone();
            let body_polls = Arc::clone(&self.body_polls);
            outcome_to_result(&outcome, || {
                let (reported_size, body_chunks, per_chunk_delay) =
                    override_body.unwrap_or((0, Vec::new(), Duration::ZERO));
                let body_stream = if per_chunk_delay.is_zero() {
                    stream::iter(body_chunks.into_iter().map(move |chunk| {
                        body_polls.fetch_add(1, Ordering::SeqCst);
                        Ok(chunk)
                    }))
                    .boxed()
                } else {
                    stream::iter(body_chunks)
                        .then(move |chunk| {
                            let body_polls = Arc::clone(&body_polls);
                            async move {
                                body_polls.fetch_add(1, Ordering::SeqCst);
                                tokio::time::sleep(per_chunk_delay).await;
                                Ok(chunk)
                            }
                        })
                        .boxed()
                };
                GetResult {
                    payload: GetResultPayload::Stream(body_stream),
                    meta: ObjectMeta {
                        location: location.clone(),
                        last_modified: chrono::Utc::now(),
                        size: reported_size,
                        e_tag: None,
                        version: None,
                    },
                    range: 0..0,
                    attributes: Default::default(),
                    extensions: Default::default(),
                }
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

        async fn list_with_delimiter(&self, _prefix: Option<&ObjectPath>) -> Result<ListResult> {
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

use bytes::Bytes;
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
async fn put_head_timeout_then_already_exists_is_dedup_success() {
    let (fake, store) = fake_store(vec![Outcome::AlreadyExists], vec![Outcome::Hang]);
    assert!(store.put(b"head timeout dedup".to_vec()).await.is_ok());
    assert_eq!(fake.head_calls.load(Ordering::SeqCst), 1);
    assert_eq!(fake.put_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn put_head_timeout_then_create_succeeds() {
    let (fake, store) = fake_store(vec![Outcome::Ok], vec![Outcome::Hang]);
    assert!(store.put(b"head timeout create".to_vec()).await.is_ok());
    assert_eq!(fake.head_calls.load(Ordering::SeqCst), 1);
    assert_eq!(fake.put_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn put_head_permission_denied_still_fails_before_create() {
    let (fake, store) = fake_store(vec![Outcome::Ok], vec![Outcome::PermissionDenied]);
    let error = store
        .put(b"head permission denied".to_vec())
        .await
        .unwrap_err();
    assert!(matches!(error, StorageError::Driver { .. }), "{error:?}");
    assert_eq!(fake.head_calls.load(Ordering::SeqCst), 1);
    assert_eq!(fake.put_calls.load(Ordering::SeqCst), 0);
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
    let (fake, store) = fake_store(vec![Outcome::Generic, Outcome::Ok], vec![Outcome::NotFound]);
    let result = store.put(b"hello".to_vec()).await;
    assert!(result.is_ok());
    assert_eq!(fake.put_calls.load(Ordering::SeqCst), 2);
    let payloads = fake.put_payloads.lock().unwrap();
    assert_eq!(payloads.len(), 2);
    assert_eq!(payloads[0], Bytes::from_static(b"hello"));
    assert_eq!(payloads[1], payloads[0]);
    assert_eq!(
        payloads[0].as_ptr(),
        payloads[1].as_ptr(),
        "retry payloads must share the same backing buffer"
    );
}

#[tokio::test]
async fn put_exhausted_timeout_maps_to_timeout_not_driver() {
    let (fake, store) = fake_store(vec![Outcome::Hang], vec![Outcome::NotFound]);
    let err = store.put(b"hello".to_vec()).await.unwrap_err();
    assert!(matches!(err, StorageError::Timeout { .. }), "got {err:?}");
    assert_eq!(fake.put_calls.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn bounded_get_rejects_invalid_limit_before_provider_work() {
    let (fake, store) = fake_store(vec![], vec![Outcome::Hang]);
    let content_ref = ContentRef::from_hex("b".repeat(64)).unwrap();
    let err = store
        .get_bounded_verified(&content_ref, MAX_BLOB_WHOLE_BYTES + 1)
        .await
        .unwrap_err();
    assert!(
        matches!(err, StorageError::InvalidInput { .. }),
        "got {err:?}"
    );
    assert_eq!(
        fake.get_calls.load(Ordering::SeqCst),
        0,
        "argument validation must not construct or poll a provider GET"
    );
}

#[tokio::test]
async fn bounded_get_not_found_precedes_object_inspection() {
    let (_fake, store) = fake_store(vec![], vec![Outcome::NotFound]);
    let content_ref = ContentRef::from_hex("b".repeat(64)).unwrap();
    let err = store
        .get_bounded_verified(&content_ref, 0)
        .await
        .unwrap_err();
    assert!(matches!(err, StorageError::NotFound { .. }), "got {err:?}");
}

#[tokio::test]
async fn bounded_get_accepts_a_digest_matching_body_at_the_exact_limit() {
    let body = b"verified body".to_vec();
    let content_ref = ContentRef::from_digest_bytes(blake3::hash(&body).as_bytes());
    let fake = Arc::new(
        FakeObjectStore::new(vec![], vec![Outcome::Ok]).with_get_body_chunks(
            body.len() as u64,
            vec![
                Bytes::from(body[..4].to_vec()),
                Bytes::from(body[4..].to_vec()),
            ],
        ),
    );
    let store = S3BlobStore::from_client_for_test(
        Arc::clone(&fake) as Arc<dyn ObjectStore>,
        "blobs",
        3,
        Duration::from_secs(1),
        Duration::from_millis(1),
    )
    .unwrap();

    let result = store
        .get_bounded_verified(&content_ref, body.len() as u64)
        .await
        .unwrap();
    assert_eq!(result, body);
    assert_eq!(fake.body_get_calls.load(Ordering::SeqCst), 1);
    assert_eq!(fake.head_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn bounded_get_rejects_over_limit_metadata_before_consuming_body() {
    let fake = Arc::new(
        FakeObjectStore::new(vec![], vec![Outcome::Ok])
            .with_get_body_chunks(5, vec![Bytes::from_static(b"x")]),
    );
    let store = S3BlobStore::from_client_for_test(
        Arc::clone(&fake) as Arc<dyn ObjectStore>,
        "blobs",
        3,
        Duration::from_secs(1),
        Duration::from_millis(1),
    )
    .unwrap();
    let content_ref = ContentRef::from_hex("c".repeat(64)).unwrap();

    let err = store
        .get_bounded_verified(&content_ref, 4)
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        StorageError::BlobTooLarge {
            content_ref: ref got,
            max_bytes: 4,
            observed_at_least: 5,
        } if got == &content_ref
    ));
    assert_eq!(
        fake.body_polls.load(Ordering::SeqCst),
        0,
        "over-limit metadata must refuse before the body stream is polled"
    );
}

#[tokio::test]
async fn bounded_get_stops_false_small_metadata_at_the_actual_byte_limit() {
    let fake = Arc::new(
        FakeObjectStore::new(vec![], vec![Outcome::Ok]).with_get_body_chunks(
            1,
            vec![
                Bytes::from_static(b"abc"),
                Bytes::from_static(b"de"),
                Bytes::from_static(b"must-not-be-polled"),
            ],
        ),
    );
    let store = S3BlobStore::from_client_for_test(
        Arc::clone(&fake) as Arc<dyn ObjectStore>,
        "blobs",
        3,
        Duration::from_secs(1),
        Duration::from_millis(1),
    )
    .unwrap();
    let content_ref = ContentRef::from_hex("d".repeat(64)).unwrap();

    let err = store
        .get_bounded_verified(&content_ref, 4)
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        StorageError::BlobTooLarge {
            content_ref: ref got,
            max_bytes: 4,
            observed_at_least: 5,
        } if got == &content_ref
    ));
    assert_eq!(
        fake.body_polls.load(Ordering::SeqCst),
        2,
        "the stream must stop at the first chunk that crosses the limit"
    );
}

#[tokio::test]
async fn bounded_get_reports_metadata_body_size_mismatch_before_digest() {
    let fake = Arc::new(
        FakeObjectStore::new(vec![], vec![Outcome::Ok])
            .with_get_body_chunks(3, vec![Bytes::from_static(b"four")]),
    );
    let store = S3BlobStore::from_client_for_test(
        Arc::clone(&fake) as Arc<dyn ObjectStore>,
        "blobs",
        3,
        Duration::from_secs(1),
        Duration::from_millis(1),
    )
    .unwrap();
    let content_ref = ContentRef::from_hex("e".repeat(64)).unwrap();

    let err = store
        .get_bounded_verified(&content_ref, 4)
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        StorageError::BlobSizeMismatch {
            content_ref: ref got,
            metadata_bytes: 3,
            actual_bytes: 4,
        } if got == &content_ref
    ));
}

#[tokio::test]
async fn bounded_get_reports_truncated_body_size_mismatch_before_digest() {
    let fake = Arc::new(
        FakeObjectStore::new(vec![], vec![Outcome::Ok])
            .with_get_body_chunks(5, vec![Bytes::from_static(b"four")]),
    );
    let store = S3BlobStore::from_client_for_test(
        Arc::clone(&fake) as Arc<dyn ObjectStore>,
        "blobs",
        3,
        Duration::from_secs(1),
        Duration::from_millis(1),
    )
    .unwrap();
    let content_ref = ContentRef::from_hex("e".repeat(64)).unwrap();

    let err = store
        .get_bounded_verified(&content_ref, 5)
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        StorageError::BlobSizeMismatch {
            content_ref: ref got,
            metadata_bytes: 5,
            actual_bytes: 4,
        } if got == &content_ref
    ));
}

#[tokio::test]
async fn bounded_get_reports_digest_mismatch_only_after_size_consistency() {
    let expected_bytes = b"expected";
    let actual_bytes = b"mutated!";
    let expected = ContentRef::from_digest_bytes(blake3::hash(expected_bytes).as_bytes());
    let actual = ContentRef::from_digest_bytes(blake3::hash(actual_bytes).as_bytes());
    let fake = Arc::new(
        FakeObjectStore::new(vec![], vec![Outcome::Ok]).with_get_body_chunks(
            actual_bytes.len() as u64,
            vec![Bytes::from_static(actual_bytes)],
        ),
    );
    let store = S3BlobStore::from_client_for_test(
        Arc::clone(&fake) as Arc<dyn ObjectStore>,
        "blobs",
        3,
        Duration::from_secs(1),
        Duration::from_millis(1),
    )
    .unwrap();

    let err = store
        .get_bounded_verified(&expected, actual_bytes.len() as u64)
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        StorageError::BlobDigestMismatch {
            expected: ref got_expected,
            actual: ref got_actual,
        } if got_expected == &expected && got_actual == &actual
    ));
}

#[tokio::test]
async fn bounded_get_deadline_spans_provider_get_and_body_consumption() {
    let chunks = vec![
        Bytes::from_static(b"12345678"),
        Bytes::from_static(b"abcdefgh"),
    ];
    let fake = Arc::new(
        FakeObjectStore::new(vec![], vec![Outcome::Ok])
            .with_get_pre_delay(Duration::from_millis(80))
            .with_get_body_chunks_delayed(16, chunks, Duration::from_millis(50)),
    );
    let store = S3BlobStore::from_client_for_test(
        Arc::clone(&fake) as Arc<dyn ObjectStore>,
        "blobs",
        3,
        Duration::from_millis(150),
        Duration::from_millis(1),
    )
    .unwrap();
    let content_ref = ContentRef::from_hex("f".repeat(64)).unwrap();

    let err = store
        .get_bounded_verified(&content_ref, 16)
        .await
        .unwrap_err();
    assert!(matches!(err, StorageError::Timeout { .. }), "got {err:?}");
}

#[tokio::test]
async fn size_returns_none_for_a_missing_object() {
    let (_fake, store) = fake_store(vec![], vec![Outcome::NotFound]);
    let content_ref = ContentRef::from_hex("1".repeat(64)).unwrap();
    assert_eq!(store.size(&content_ref).await.unwrap(), None);
}

#[tokio::test]
async fn size_timeout_maps_to_storage_timeout() {
    let (_fake, store) = fake_store(vec![], vec![Outcome::Hang]);
    let content_ref = ContentRef::from_hex("2".repeat(64)).unwrap();
    let err = store.size(&content_ref).await.unwrap_err();
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
