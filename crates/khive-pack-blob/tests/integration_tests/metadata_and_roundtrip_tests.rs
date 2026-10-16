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
