//! Range refusals identify invalid operands without echoing recognized credentials.

use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use khive_db::stores::blob::FsBlobStore;
use khive_pack_blob::BlobPack;
use khive_runtime::{
    runtime_error_value, DomainDisposition, KhiveRuntime, RuntimeError, VerbRegistry,
    VerbRegistryBuilder,
};
use khive_storage::{BlobStore, ContentRef, StorageResult};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

#[derive(Debug)]
struct ObservedStore {
    inner: FsBlobStore,
    calls: [AtomicUsize; 5],
}

impl ObservedStore {
    fn snapshot(&self) -> [usize; 5] {
        self.calls
            .each_ref()
            .map(|count| count.load(Ordering::SeqCst))
    }
}

#[async_trait]
impl BlobStore for ObservedStore {
    async fn put(&self, bytes: Vec<u8>) -> StorageResult<ContentRef> {
        self.calls[0].fetch_add(1, Ordering::SeqCst);
        self.inner.put(bytes).await
    }

    async fn get_bounded_verified(
        &self,
        content_ref: &ContentRef,
        max_bytes: u64,
    ) -> StorageResult<Vec<u8>> {
        self.calls[1].fetch_add(1, Ordering::SeqCst);
        self.inner
            .get_bounded_verified(content_ref, max_bytes)
            .await
    }

    async fn exists(&self, content_ref: &ContentRef) -> StorageResult<bool> {
        self.calls[2].fetch_add(1, Ordering::SeqCst);
        self.inner.exists(content_ref).await
    }

    async fn size(&self, content_ref: &ContentRef) -> StorageResult<Option<u64>> {
        self.calls[3].fetch_add(1, Ordering::SeqCst);
        self.inner.size(content_ref).await
    }

    async fn delete(&self, content_ref: &ContentRef) -> StorageResult<bool> {
        self.calls[4].fetch_add(1, Ordering::SeqCst);
        self.inner.delete(content_ref).await
    }
}

async fn fixture() -> (VerbRegistry, Arc<ObservedStore>, String, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(ObservedStore {
        inner: FsBlobStore::new(dir.path().to_path_buf(), 0).unwrap(),
        calls: std::array::from_fn(|_| AtomicUsize::new(0)),
    });
    let runtime = KhiveRuntime::memory().unwrap();
    assert!(runtime.backend().pool().canonical_path().is_none());
    runtime.install_blob_store(store.clone()).unwrap();
    let mut builder = VerbRegistryBuilder::new();
    builder.register(BlobPack::new(runtime));
    let registry = builder.build().unwrap();
    let put = registry
        .dispatch("blob.put", json!({"bytes": BASE64.encode(b"abcdef")}))
        .await
        .unwrap();
    assert_eq!(put["size"], 6);
    assert_eq!(store.snapshot()[0], 1, "positive fixture performed a write");
    let reference = put["content_ref"].as_str().unwrap().to_owned();
    (registry, store, reference, dir)
}

fn credential() -> String {
    format!("sk-proj-{}", "A".repeat(80))
}

async fn refuse(
    registry: &VerbRegistry,
    store: &ObservedStore,
    reference: &str,
    range: Value,
    expected: String,
    secret: Option<&str>,
) {
    let before = store.snapshot();
    let error = registry
        .dispatch(
            "blob.get",
            json!({"content_ref": reference, "range": range}),
        )
        .await
        .expect_err("invalid range must be refused");
    assert_eq!(store.snapshot(), before, "refusal reached blob storage");
    let RuntimeError::InvalidInput(message) = &error else {
        panic!("range refusal changed error kind: {error:?}");
    };
    if let Some(secret) = secret {
        assert!(
            !message.contains(secret),
            "refusal exposed the synthetic credential"
        );
        assert!(message.contains("***MASKED***"), "credential mask missing");
    }
    let expected = format!("invalid blob.get arguments: {expected}");
    assert_eq!(message, &expected);
    assert_eq!(
        runtime_error_value(error, DomainDisposition::NotCommitted),
        json!({
            "kind": "runtime_error",
            "message": format!("invalid input: {expected}"),
            "domain_disposition": "not_committed"
        })
    );
}

async fn assert_original_blob(registry: &VerbRegistry, store: &ObservedStore, reference: &str) {
    let value = registry
        .dispatch("blob.get", json!({"content_ref": reference}))
        .await
        .unwrap();
    assert_eq!(value["bytes"], BASE64.encode(b"abcdef"));
    assert_eq!(value["size"], 6);
    assert_eq!(store.snapshot()[0], 1, "refusals wrote another blob");
    assert_eq!(store.snapshot()[4], 0, "refusals deleted a blob");
    assert!(
        store.snapshot()[1] > 0,
        "positive get reached verified hydration"
    );
}

#[tokio::test]
async fn range_container_refusals_echo_operands_and_mask_credentials() {
    let (registry, store, reference, _dir) = fixture().await;
    let secret = credential();
    for (value, shown, masked) in [
        (json!("ordinary"), json!("ordinary").to_string(), false),
        (json!([1, 2]), "[1,2]".into(), false),
        (json!(42), "42".into(), false),
        (json!(false), "false".into(), false),
        (json!(secret), json!("***MASKED***").to_string(), true),
        (json!([secret]), json!(["***MASKED***"]).to_string(), true),
    ] {
        refuse(
            &registry,
            &store,
            &reference,
            value,
            format!("range must be a JSON object with optional offset/length; got {shown}"),
            masked.then_some(secret.as_str()),
        )
        .await;
    }
    assert_original_blob(&registry, &store, &reference).await;
}

async fn integer_refusals(field: &str) {
    let (registry, store, reference, _dir) = fixture().await;
    let secret = credential();
    for (value, shown, masked) in [
        (json!(-1), "-1".into(), false),
        (json!(1.5), "1.5".into(), false),
        (json!("3"), json!("3").to_string(), false),
        (json!(false), "false".into(), false),
        (json!([1]), "[1]".into(), false),
        (json!(secret), json!("***MASKED***").to_string(), true),
        (
            json!({"nested": secret}),
            json!({"nested": "***MASKED***"}).to_string(),
            true,
        ),
    ] {
        let mut range = json!({});
        range[field] = value;
        refuse(
            &registry,
            &store,
            &reference,
            range,
            format!("range.{field} must be a non-negative integer, got {shown}"),
            masked.then_some(secret.as_str()),
        )
        .await;
    }
    assert_original_blob(&registry, &store, &reference).await;
}

#[tokio::test]
async fn range_offset_refusals_mask_credentials_before_storage() {
    integer_refusals("offset").await;
}

#[tokio::test]
async fn range_length_refusals_mask_credentials_before_storage() {
    integer_refusals("length").await;
}

#[tokio::test]
async fn valid_ranges_keep_defaults_bounds_and_refusal_precedence() {
    let (registry, store, reference, _dir) = fixture().await;
    for (range, expected, range_out) in [
        (Value::Null, "abcdef", None),
        (json!({}), "abcdef", Some(json!({"offset": 0, "length": 6}))),
        (
            json!({"length": null}),
            "abcdef",
            Some(json!({"offset": 0, "length": 6})),
        ),
        (
            json!({"offset": 2, "length": 2}),
            "cd",
            Some(json!({"offset": 2, "length": 2})),
        ),
        (
            json!({"offset": 4, "length": 99}),
            "ef",
            Some(json!({"offset": 4, "length": 2})),
        ),
        (
            json!({"offset": 6}),
            "",
            Some(json!({"offset": 6, "length": 0})),
        ),
        (
            json!({"length": 0}),
            "",
            Some(json!({"offset": 0, "length": 0})),
        ),
    ] {
        let value = registry
            .dispatch(
                "blob.get",
                json!({"content_ref": reference, "range": range}),
            )
            .await
            .unwrap();
        assert_eq!(value["bytes"], BASE64.encode(expected));
        assert_eq!(value["size"], 6);
        assert_eq!(value.get("range"), range_out.as_ref());
    }
    refuse(
        &registry,
        &store,
        "not-a-content-ref",
        json!({"offset": null}),
        "range.offset must be a non-negative integer, got null".into(),
        None,
    )
    .await;
    let error = registry
        .dispatch(
            "blob.get",
            json!({
                "content_ref": reference, "range": {"offset": 7}
            }),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, RuntimeError::InvalidInput(ref message)
        if message == "blob.get: range offset 7 exceeds object size 6"));
    assert_original_blob(&registry, &store, &reference).await;
}
