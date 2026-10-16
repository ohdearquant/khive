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
async fn get_refuses_when_reported_size_understates_stored_bytes() {
    let actual = b"the authoritative object is longer".to_vec();
    let reported_size = 4;
    let store = Arc::new(BoundedOnlyBlobStore::underreporting(
        actual.clone(),
        reported_size,
    ));
    let registry = build_bounded_registry(Arc::clone(&store));

    let error = registry
        .dispatch(
            "blob.get",
            serde_json::json!({ "content_ref": store.content_ref.to_string() }),
        )
        .await
        .expect_err("an underreported object must not escape the bounded hydrator");
    match error {
        RuntimeError::Storage(StorageError::BlobTooLarge {
            content_ref,
            max_bytes,
            observed_at_least,
        }) => {
            assert_eq!(content_ref, store.content_ref);
            assert_eq!(max_bytes, reported_size);
            assert_eq!(observed_at_least, actual.len() as u64);
        }
        other => panic!("expected typed BlobTooLarge, got: {other}"),
    }
    assert_eq!(store.bounded_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn ranged_get_reserves_the_complete_object_before_slicing() {
    let bytes = b"0123456789".to_vec();
    let store = Arc::new(BoundedOnlyBlobStore::new(bytes.clone()));
    let registry = build_bounded_registry(Arc::clone(&store));

    let get = registry
        .dispatch(
            "blob.get",
            serde_json::json!({
                "content_ref": store.content_ref.to_string(),
                "range": { "offset": 3, "length": 1 },
            }),
        )
        .await
        .expect("ranged get must verify the complete object");
    assert_eq!(BASE64.decode(get["bytes"].as_str().unwrap()).unwrap(), b"3");
    assert_eq!(get["size"], bytes.len());
    assert_eq!(get["range"]["length"], 1);
    assert_eq!(store.bounded_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn empty_get_reserves_zero_bytes_and_returns_empty_object() {
    let store = Arc::new(BoundedOnlyBlobStore::new(Vec::new()));
    let registry = build_bounded_registry(Arc::clone(&store));

    let get = registry
        .dispatch(
            "blob.get",
            serde_json::json!({ "content_ref": store.content_ref.to_string() }),
        )
        .await
        .expect("empty objects require a valid zero-byte reservation");
    assert_eq!(get["size"], 0);
    assert_eq!(get["bytes"], "");
    assert_eq!(store.bounded_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn five_small_blob_gets_enter_hydration_without_serializing_on_budget() {
    let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let store = Arc::new(BoundedOnlyBlobStore::gated(
        b"small hydration".to_vec(),
        entered_tx,
        Arc::clone(&release),
    ));
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    runtime
        .install_blob_store(Arc::clone(&store) as Arc<dyn BlobStore>)
        .expect("install blob store");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(BlobPack::new(runtime));
    let registry = Arc::new(builder.build().expect("registry builds"));
    let mut requests = Vec::new();
    for _ in 0..5 {
        let registry = Arc::clone(&registry);
        let reference = store.content_ref.to_string();
        requests.push(tokio::spawn(async move {
            registry
                .dispatch("blob.get", serde_json::json!({"content_ref": reference}))
                .await
        }));
    }

    let mut entered = 0;
    for _ in 0..5 {
        if tokio::time::timeout(std::time::Duration::from_secs(5), entered_rx.recv())
            .await
            .ok()
            .flatten()
            .is_some()
        {
            entered += 1;
        } else {
            break;
        }
    }
    // Release held store reads before asserting, so a failing old admission
    // policy does not strand a tracked hydration task after the test panics.
    release.add_permits(5);
    for request in requests {
        request
            .await
            .expect("request task")
            .expect("blob.get succeeds");
    }
    assert_eq!(
        entered, 5,
        "all five tiny reads must pass byte admission together"
    );
}
