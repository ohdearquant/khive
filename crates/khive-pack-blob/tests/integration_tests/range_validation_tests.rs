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
