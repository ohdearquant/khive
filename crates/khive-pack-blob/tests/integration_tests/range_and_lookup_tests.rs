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
