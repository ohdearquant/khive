//! End-to-end smoke test for the blob pack: put -> stat -> get round trip
//! through the `VerbRegistry` dispatch path, mirroring
//! `khive-pack-template/tests/integration.rs`.

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use khive_db::stores::blob::FsBlobStore;
use khive_pack_blob::BlobPack;
use khive_runtime::{KhiveRuntime, VerbRegistry, VerbRegistryBuilder};
use khive_storage::BlobStore;
use khive_types::Pack;

fn build_registry() -> (VerbRegistry, KhiveRuntime, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = FsBlobStore::new(dir.path().to_path_buf(), 0).expect("fs blob store");

    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    runtime.install_blob_store(std::sync::Arc::new(store));

    let mut builder = VerbRegistryBuilder::new();
    builder.register(BlobPack::new(runtime.clone()));
    let registry = builder.build().expect("registry builds");
    (registry, runtime, dir)
}

#[test]
fn blob_pack_name_and_requires_are_stable() {
    assert_eq!(BlobPack::NAME, "blob");
    assert!(BlobPack::REQUIRES.is_empty());
    assert!(BlobPack::NOTE_KINDS.is_empty());
    assert!(BlobPack::ENTITY_KINDS.is_empty());
}

#[tokio::test]
async fn put_stat_get_round_trips_and_put_is_idempotent() {
    let (registry, _rt, _dir) = build_registry();

    let payload = b"khive blob verbs phase 1".to_vec();
    let b64 = BASE64.encode(&payload);

    let put1 = registry
        .dispatch("blob.put", serde_json::json!({ "bytes": b64.clone() }))
        .await
        .expect("blob.put dispatches");
    let content_ref = put1["content_ref"]
        .as_str()
        .expect("content_ref string")
        .to_string();
    assert_eq!(put1["size"], payload.len());
    assert_eq!(
        content_ref.len(),
        64,
        "ContentRef must be a 64-char BLAKE3 hex digest"
    );

    // Idempotent: identical bytes return the same ref.
    let put2 = registry
        .dispatch("blob.put", serde_json::json!({ "bytes": b64 }))
        .await
        .expect("second blob.put dispatches");
    assert_eq!(put2["content_ref"], content_ref);

    let stat = registry
        .dispatch(
            "blob.stat",
            serde_json::json!({ "content_ref": content_ref }),
        )
        .await
        .expect("blob.stat dispatches");
    assert_eq!(stat["exists"], true);
    assert_eq!(stat["size"], payload.len());

    let get = registry
        .dispatch(
            "blob.get",
            serde_json::json!({ "content_ref": content_ref }),
        )
        .await
        .expect("blob.get dispatches");
    let round_tripped = BASE64
        .decode(get["bytes"].as_str().expect("bytes field"))
        .expect("valid base64");
    assert_eq!(round_tripped, payload);
    assert_eq!(get["size"], payload.len());
}

#[tokio::test]
async fn get_supports_byte_range() {
    let (registry, _rt, _dir) = build_registry();

    let payload = b"0123456789".to_vec();
    let put = registry
        .dispatch(
            "blob.put",
            serde_json::json!({ "bytes": BASE64.encode(&payload) }),
        )
        .await
        .expect("blob.put dispatches");
    let content_ref = put["content_ref"].as_str().unwrap().to_string();

    let get = registry
        .dispatch(
            "blob.get",
            serde_json::json!({ "content_ref": content_ref, "range": { "offset": 3, "length": 4 } }),
        )
        .await
        .expect("ranged blob.get dispatches");
    let sliced = BASE64.decode(get["bytes"].as_str().unwrap()).unwrap();
    assert_eq!(sliced, b"3456");
    assert_eq!(get["range"]["offset"], 3);
    assert_eq!(get["range"]["length"], 4);
}

#[tokio::test]
async fn stat_on_unknown_ref_reports_not_existing() {
    let (registry, _rt, _dir) = build_registry();
    let unknown_ref = "a".repeat(64);

    let stat = registry
        .dispatch(
            "blob.stat",
            serde_json::json!({ "content_ref": unknown_ref }),
        )
        .await
        .expect("blob.stat dispatches for an absent ref");
    assert_eq!(stat["exists"], false);
}

#[tokio::test]
async fn get_on_unknown_ref_errors_not_found() {
    let (registry, _rt, _dir) = build_registry();
    let unknown_ref = "b".repeat(64);

    let err = registry
        .dispatch(
            "blob.get",
            serde_json::json!({ "content_ref": unknown_ref }),
        )
        .await
        .unwrap_err();
    assert!(
        err.to_string().to_lowercase().contains("not found"),
        "expected a not-found error, got: {err}"
    );
}

#[tokio::test]
async fn put_rejects_missing_bytes() {
    let (registry, _rt, _dir) = build_registry();

    let err = registry
        .dispatch("blob.put", serde_json::json!({}))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("requires"));
}

// Security regression guard: `path` was removed from `blob.put` because reading
// a server-local file is an exfiltration surface for any caller reaching the
// verb. A `path` field must be inert (treated as absent, never read), so a put
// carrying only `path` fails the missing-`bytes` check instead of reading it.
#[tokio::test]
async fn put_does_not_read_a_server_local_path() {
    let (registry, _rt, _dir) = build_registry();

    let mut src = tempfile::NamedTempFile::new().expect("named temp file");
    use std::io::Write as _;
    src.write_all(b"secret-bytes-that-must-not-be-read")
        .expect("write temp file");

    let err = registry
        .dispatch(
            "blob.put",
            serde_json::json!({ "path": src.path().to_str().unwrap() }),
        )
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("requires"),
        "path-only put must fail as missing bytes, got: {err}"
    );
}

#[tokio::test]
async fn get_rejects_malformed_content_ref() {
    let (registry, _rt, _dir) = build_registry();

    let err = registry
        .dispatch(
            "blob.get",
            serde_json::json!({ "content_ref": "not-a-ref" }),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("invalid content_ref"));
}

#[tokio::test]
async fn stat_reports_size_without_a_corrupt_field_for_a_present_object() {
    let (registry, _rt, _dir) = build_registry();

    let payload = b"stat must not hydrate this".to_vec();
    let put = registry
        .dispatch(
            "blob.put",
            serde_json::json!({ "bytes": BASE64.encode(&payload) }),
        )
        .await
        .expect("blob.put dispatches");
    let content_ref = put["content_ref"].as_str().unwrap().to_string();

    let stat = registry
        .dispatch(
            "blob.stat",
            serde_json::json!({ "content_ref": content_ref }),
        )
        .await
        .expect("blob.stat dispatches");
    assert_eq!(stat["exists"], true);
    assert_eq!(stat["size"], payload.len());
    assert!(
        stat.get("corrupt").is_none(),
        "stat answers existence+size from BlobStore::size only, never hydrates bytes to \
         digest-verify: {stat:?}"
    );
}

#[tokio::test]
async fn stat_reports_absent_for_an_unknown_ref() {
    let (registry, _rt, _dir) = build_registry();
    let unknown_ref = "9".repeat(64);

    let stat = registry
        .dispatch(
            "blob.stat",
            serde_json::json!({ "content_ref": unknown_ref }),
        )
        .await
        .expect("blob.stat dispatches for an absent ref");
    assert_eq!(stat["exists"], false);
    assert!(stat.get("size").is_none());
}

#[tokio::test]
async fn get_rejects_an_object_over_the_hydration_ceiling() {
    let (registry, _rt, dir) = build_registry();

    // Bypass blob.put's own ceiling by writing directly through the store,
    // matching MAX_OBJECT_BYTES in crates/khive-pack-blob/src/handlers.rs
    // (128 MiB) plus one byte so blob.get's independent ceiling check is
    // what actually rejects the read.
    let store = khive_db::stores::blob::FsBlobStore::new(dir.path().to_path_buf(), 0)
        .expect("fs blob store for oversized write");
    let oversized = vec![0u8; 128 * 1024 * 1024 + 1];
    let content_ref = store.put(oversized).await.expect("direct store put");

    let err = registry
        .dispatch(
            "blob.get",
            serde_json::json!({ "content_ref": content_ref.to_string() }),
        )
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("exceeding"),
        "expected a ceiling-exceeded error, got: {err}"
    );
}

#[tokio::test]
async fn get_rejects_a_non_object_range() {
    let (registry, _rt, _dir) = build_registry();

    let put = registry
        .dispatch(
            "blob.put",
            serde_json::json!({ "bytes": BASE64.encode(b"range validation") }),
        )
        .await
        .expect("blob.put dispatches");
    let content_ref = put["content_ref"].as_str().unwrap().to_string();

    let err = registry
        .dispatch(
            "blob.get",
            serde_json::json!({ "content_ref": content_ref, "range": "not-an-object" }),
        )
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("range must be a JSON object"),
        "got: {err}"
    );
}

#[tokio::test]
async fn get_rejects_a_string_range_offset() {
    let (registry, _rt, _dir) = build_registry();

    let put = registry
        .dispatch(
            "blob.put",
            serde_json::json!({ "bytes": BASE64.encode(b"range validation") }),
        )
        .await
        .expect("blob.put dispatches");
    let content_ref = put["content_ref"].as_str().unwrap().to_string();

    let err = registry
        .dispatch(
            "blob.get",
            serde_json::json!({ "content_ref": content_ref, "range": { "offset": "3" } }),
        )
        .await
        .unwrap_err();
    assert!(
        err.to_string()
            .contains("range.offset must be a non-negative integer"),
        "got: {err}"
    );
}

#[tokio::test]
async fn get_rejects_a_negative_range_offset() {
    let (registry, _rt, _dir) = build_registry();

    let put = registry
        .dispatch(
            "blob.put",
            serde_json::json!({ "bytes": BASE64.encode(b"range validation") }),
        )
        .await
        .expect("blob.put dispatches");
    let content_ref = put["content_ref"].as_str().unwrap().to_string();

    let err = registry
        .dispatch(
            "blob.get",
            serde_json::json!({ "content_ref": content_ref, "range": { "offset": -1 } }),
        )
        .await
        .unwrap_err();
    assert!(
        err.to_string()
            .contains("range.offset must be a non-negative integer"),
        "got: {err}"
    );
}

#[tokio::test]
async fn get_rejects_a_float_range_length() {
    let (registry, _rt, _dir) = build_registry();

    let put = registry
        .dispatch(
            "blob.put",
            serde_json::json!({ "bytes": BASE64.encode(b"range validation") }),
        )
        .await
        .expect("blob.put dispatches");
    let content_ref = put["content_ref"].as_str().unwrap().to_string();

    let err = registry
        .dispatch(
            "blob.get",
            serde_json::json!({ "content_ref": content_ref, "range": { "offset": 0, "length": 2.5 } }),
        )
        .await
        .unwrap_err();
    assert!(
        err.to_string()
            .contains("range.length must be a non-negative integer"),
        "got: {err}"
    );
}
