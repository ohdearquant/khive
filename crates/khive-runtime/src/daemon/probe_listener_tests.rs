/// #2230 review (Medium): a listener that accepts a connection but never
/// answers with a well-formed daemon response — e.g. an unrelated process
/// that happens to have bound the same socket path — must not be
/// classified as khived, and the probe must not hang past its own
/// bounded timeout.
#[tokio::test]
async fn socket_speaks_khived_protocol_rejects_a_non_protocol_listener() {
    mod timing {
        include!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../test_support/timing.rs"
        ));
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let sock_path = dir.path().join("fake.sock");
    let listener = UnixListener::bind(&sock_path).expect("bind fake listener");
    let held = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let held_for_task = held.clone();
    let accept_task = tokio::spawn(async move {
        if let Ok((stream, _)) = listener.accept().await {
            // Accept but never write anything back — the connection stays
            // open exactly like a foreign process that speaks a different
            // (or no) protocol on this socket.
            held_for_task.lock().await.push(stream);
        }
    });

    let before = tokio::time::Instant::now();
    let speaks = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        socket_speaks_khived_protocol(&sock_path, "probe-test"),
    )
    .await
    .expect("the protocol probe must finish within the hang watchdog");
    let elapsed = before.elapsed();

    assert!(
        !speaks,
        "a listener that accepts but never answers the probe frame must not be treated as khived"
    );
    if let Some(bound) = timing::duration_bound(DUPLICATE_PROBE_TIMEOUT * 4, None) {
        assert!(
            elapsed < bound,
            "the probe must finish within {bound:?}; took {elapsed:?}"
        );
    }

    accept_task.abort();
    let _ = accept_task.await;
    drop(held);
}
