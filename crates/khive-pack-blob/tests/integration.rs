//! End-to-end smoke test for the blob pack: put -> stat -> get round trip
//! through the `VerbRegistry` dispatch path, mirroring
//! `khive-pack-template/tests/integration.rs`.

use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use khive_db::stores::blob::FsBlobStore;
use khive_pack_blob::BlobPack;
use khive_runtime::{KhiveRuntime, VerbRegistry, VerbRegistryBuilder};
use khive_storage::{BlobStore, ContentRef, StorageError, StorageResult};
use khive_types::Pack;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

#[derive(Debug)]
struct BoundedOnlyBlobStore {
    bytes: Vec<u8>,
    content_ref: ContentRef,
    bounded_calls: AtomicUsize,
}

impl BoundedOnlyBlobStore {
    fn new(bytes: Vec<u8>) -> Self {
        let content_ref = ContentRef::from_digest_bytes(blake3::hash(&bytes).as_bytes());
        Self {
            bytes,
            content_ref,
            bounded_calls: AtomicUsize::new(0),
        }
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
        assert_eq!(max_bytes, khive_storage::MAX_BLOB_WHOLE_BYTES);
        Ok(self.bytes.clone())
    }

    async fn exists(&self, content_ref: &ContentRef) -> StorageResult<bool> {
        Ok(content_ref == &self.content_ref)
    }

    async fn size(&self, content_ref: &ContentRef) -> StorageResult<Option<u64>> {
        Ok((content_ref == &self.content_ref).then_some(self.bytes.len() as u64))
    }

    async fn delete(&self, _content_ref: &ContentRef) -> StorageResult<bool> {
        Err(StorageError::Internal("delete is not used".to_string()))
    }
}

fn build_registry() -> (VerbRegistry, KhiveRuntime, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = FsBlobStore::new(dir.path().to_path_buf(), 0).expect("fs blob store");

    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    runtime
        .install_blob_store(std::sync::Arc::new(store))
        .expect("install blob store");

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
async fn get_uses_the_runtime_hydrator_instead_of_unbounded_store_get() {
    let bytes = b"runtime-owned verified hydration".to_vec();
    let store = Arc::new(BoundedOnlyBlobStore::new(bytes.clone()));
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    runtime
        .install_blob_store(Arc::clone(&store) as Arc<dyn BlobStore>)
        .expect("install blob store");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(BlobPack::new(runtime));
    let registry = builder.build().expect("registry builds");

    let get = registry
        .dispatch(
            "blob.get",
            serde_json::json!({ "content_ref": store.content_ref.to_string() }),
        )
        .await
        .expect("blob.get must use bounded verified hydration");

    assert_eq!(
        BASE64.decode(get["bytes"].as_str().unwrap()).unwrap(),
        bytes
    );
    assert_eq!(store.bounded_calls.load(Ordering::SeqCst), 1);
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
    assert!(err.to_string().contains("missing field `bytes`"));
}

// Security regression guard: `path` was removed from `blob.put` because reading
// a server-local file is an exfiltration surface for any caller reaching the
// verb. A `path` field is rejected by name before any store or file access.
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
        err.to_string().contains("unknown field `path`"),
        "path-only put must fail as an unknown argument, got: {err}"
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
    // (64 MiB, ADR-111's v1 object ceiling) plus one byte so blob.get's
    // independent ceiling check is what actually rejects the read.
    let store = khive_db::stores::blob::FsBlobStore::new(dir.path().to_path_buf(), 0)
        .expect("fs blob store for oversized write");
    let oversized = vec![0u8; 64 * 1024 * 1024 + 1];
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
async fn put_rejects_a_payload_over_the_adr111_ceiling() {
    let (registry, _rt, _dir) = build_registry();

    // One byte over ADR-111's 64 MiB v1 object ceiling, matching
    // MAX_OBJECT_BYTES in crates/khive-pack-blob/src/handlers.rs.
    let oversized = vec![0u8; 64 * 1024 * 1024 + 1];
    let err = registry
        .dispatch(
            "blob.put",
            serde_json::json!({ "bytes": BASE64.encode(&oversized) }),
        )
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("exceeding"),
        "expected a ceiling-exceeded error, got: {err}"
    );
}

#[tokio::test]
async fn get_rejects_a_response_that_would_exceed_the_daemon_frame_cap() {
    let (registry, _rt, dir) = build_registry();

    // Under the 64 MiB object ceiling but, once base64-encoded, over the
    // daemon's 8 MiB MAX_FRAME_BYTES IPC cap -- blob.get must reject this
    // before ever hydrating the object, not just before storing it.
    let store = khive_db::stores::blob::FsBlobStore::new(dir.path().to_path_buf(), 0)
        .expect("fs blob store for a frame-cap-busting write");
    let frame_busting = vec![0u8; 7 * 1024 * 1024];
    let content_ref = store.put(frame_busting).await.expect("direct store put");

    let err = registry
        .dispatch(
            "blob.get",
            serde_json::json!({ "content_ref": content_ref.to_string() }),
        )
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("daemon frame cap"),
        "expected a frame-cap error, got: {err}"
    );
}

#[tokio::test]
async fn get_with_range_under_the_frame_cap_still_succeeds_on_a_large_object() {
    let (registry, _rt, dir) = build_registry();

    // The full object exceeds the frame cap, but a small ranged read of it
    // must still succeed -- the frame-cap check applies to the requested
    // slice, not the stored object's total size.
    let store = khive_db::stores::blob::FsBlobStore::new(dir.path().to_path_buf(), 0)
        .expect("fs blob store for a large object");
    let large = vec![7u8; 7 * 1024 * 1024];
    let content_ref = store.put(large).await.expect("direct store put");

    let get = registry
        .dispatch(
            "blob.get",
            serde_json::json!({
                "content_ref": content_ref.to_string(),
                "range": { "offset": 0, "length": 16 },
            }),
        )
        .await
        .expect("ranged get of a small slice from a large object must succeed");
    let sliced = BASE64.decode(get["bytes"].as_str().unwrap()).unwrap();
    assert_eq!(sliced, vec![7u8; 16]);
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

fn blob_files(root: &std::path::Path) -> Vec<(std::path::PathBuf, Option<Vec<u8>>)> {
    fn visit(
        root: &std::path::Path,
        path: &std::path::Path,
        files: &mut Vec<(std::path::PathBuf, Option<Vec<u8>>)>,
    ) {
        for entry in std::fs::read_dir(path).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let relative = path.strip_prefix(root).unwrap().to_path_buf();
            if entry.file_type().unwrap().is_dir() {
                files.push((relative, None));
                visit(root, &path, files);
            } else {
                files.push((relative, Some(std::fs::read(path).unwrap())));
            }
        }
    }
    let mut files = Vec::new();
    visit(root, root, &mut files);
    files.sort_by(|a, b| a.0.cmp(&b.0));
    files
}

#[tokio::test]
async fn every_blob_verb_rejects_unknown_arguments_before_side_effects() {
    use serde_json::json;
    let (registry, _runtime, dir) = build_registry();
    let existing = registry
        .dispatch("blob.put", json!({"bytes": BASE64.encode(b"existing")}))
        .await
        .unwrap();
    let upload = registry
        .dispatch("blob.begin", json!({"size": 1, "content_ref": null}))
        .await
        .unwrap();
    let id = upload["upload_id"].as_str().unwrap();
    let reference = existing["content_ref"].as_str().unwrap();
    let cases = [
        (
            "blob.put",
            json!({"bytes": BASE64.encode(b"refused object")}),
        ),
        ("blob.get", json!({"content_ref": reference})),
        ("blob.stat", json!({"content_ref": reference})),
        ("blob.begin", json!({"size": 1})),
        (
            "blob.put_part",
            json!({"upload_id": id, "index": 0, "bytes": BASE64.encode(b"A")}),
        ),
        ("blob.commit", json!({"upload_id": id})),
        ("blob.abort", json!({"upload_id": id})),
    ];
    let registered: std::collections::BTreeSet<_> = BlobPack::HANDLERS
        .iter()
        .map(|handler| handler.name)
        .collect();
    let covered: std::collections::BTreeSet<_> = cases.iter().map(|(verb, _)| *verb).collect();
    assert_eq!(
        covered, registered,
        "every registered verb needs a valid fixture"
    );
    for (verb, params) in [
        ("blob.put", json!([BASE64.encode(b"refused object")])),
        ("blob.get", json!([reference, null])),
        ("blob.stat", json!([reference])),
        ("blob.begin", json!([1, null])),
        ("blob.put_part", json!([id, 0, BASE64.encode(b"A")])),
        ("blob.commit", json!([id])),
        ("blob.abort", json!([id])),
    ] {
        let before = blob_files(dir.path());
        let error = registry
            .dispatch(verb, params)
            .await
            .expect_err("blob verb accepted a positional argument array");
        assert!(
            error
                .to_string()
                .contains("arguments must be a JSON object"),
            "{verb} did not refuse a positional argument array: {error}"
        );
        assert_eq!(
            blob_files(dir.path()),
            before,
            "{verb} mutated state for a positional array"
        );
    }
    let before = blob_files(dir.path());
    for (verb, mut params) in cases {
        params["zzz_not_a_real_param"] = json!(true);
        let outcome = registry.dispatch(verb, params).await;
        assert!(outcome.is_err(), "{verb} accepted an unknown argument");
        let error = outcome.unwrap_err().to_string();
        assert!(
            error.contains("unknown field `zzz_not_a_real_param`"),
            "{verb} did not reject the unknown argument by name: {error}"
        );
        for parameter in BlobPack::HANDLERS
            .iter()
            .find(|handler| handler.name == verb)
            .unwrap()
            .params
        {
            assert!(
                error.contains(parameter.name),
                "{verb} must name allowed argument {}: {error}",
                parameter.name
            );
        }
        assert_eq!(
            blob_files(dir.path()),
            before,
            "{verb} changed blob or staging files"
        );
    }
    // Refused put_part/abort must leave the same upload usable, with no accepted tail.
    let part = registry
        .dispatch(
            "blob.put_part",
            json!({"upload_id": id, "index": 0, "bytes": BASE64.encode(b"B")}),
        )
        .await
        .unwrap();
    assert_eq!(part["received_bytes"], 1);
    assert_eq!(part["next_index"], 1);
    // Exercise a commit that WOULD succeed if the unknown field were ignored.
    let before_commit = blob_files(dir.path());
    let error = registry
        .dispatch(
            "blob.commit",
            json!({"upload_id": id, "zzz_not_a_real_param": true}),
        )
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("unknown field `zzz_not_a_real_param`"),
        "{error}"
    );
    assert_eq!(blob_files(dir.path()), before_commit);
    let committed = registry
        .dispatch("blob.commit", json!({"upload_id": id}))
        .await
        .unwrap();
    let stored = registry
        .dispatch("blob.get", json!({"content_ref": committed["content_ref"]}))
        .await
        .unwrap();
    assert_eq!(
        BASE64.decode(stored["bytes"].as_str().unwrap()).unwrap(),
        b"B"
    );
}

#[tokio::test]
async fn get_range_rejects_unknown_fields_and_preserves_nullable_defaults() {
    use serde_json::json;
    let (registry, _runtime, _dir) = build_registry();
    let stored = registry
        .dispatch("blob.put", json!({"bytes": BASE64.encode(b"abcd")}))
        .await
        .unwrap();
    let reference = stored["content_ref"].as_str().unwrap();
    let error = registry
        .dispatch(
            "blob.get",
            json!({"content_ref": reference, "range": {"offset": 1, "lenght": 2}}),
        )
        .await
        .expect_err("blob.get accepted an unknown range field");
    let error = error.to_string();
    assert!(
        error.contains("unknown field `lenght`")
            && error.contains("offset")
            && error.contains("length"),
        "{error}"
    );
    for range in [serde_json::Value::Null, json!({}), json!({"length": null})] {
        let result = registry
            .dispatch(
                "blob.get",
                json!({"content_ref": reference, "range": range}),
            )
            .await
            .unwrap();
        assert_eq!(
            BASE64.decode(result["bytes"].as_str().unwrap()).unwrap(),
            b"abcd"
        );
    }
    for range in [
        json!([1, 2]),
        json!({"offset": null}),
        json!({"offset": -1}),
        json!({"length": "2"}),
        json!({"length": 1.5}),
    ] {
        assert!(registry
            .dispatch(
                "blob.get",
                json!({"content_ref": reference, "range": range})
            )
            .await
            .is_err());
    }
}
