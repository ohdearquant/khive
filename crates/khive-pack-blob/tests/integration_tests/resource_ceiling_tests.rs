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
