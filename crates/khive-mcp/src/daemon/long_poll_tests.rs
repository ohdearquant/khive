// Included in daemon::tests::handover to reuse its isolated socket fixtures.

async fn real_response(server: crate::server::KhiveMcpServer, request_bytes: &[u8]) -> Vec<u8> {
    let (mut client, peer) = UnixStream::pair().unwrap();
    let task = tokio::spawn(khive_runtime::daemon::serve_connection_for_test(
        peer, server,
    ));
    write_frame(&mut client, request_bytes).await.unwrap();
    let response = read_frame(&mut client).await.unwrap();
    task.await.unwrap();
    response
}

#[tokio::test]
#[serial]
#[serial_test::serial(config_ledger)]
async fn long_poll_ceiling_returns_empty_pages_over_real_transport_including_one_replay() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }

    let _cleanup = RecoveryTestGuard::new();
    let dir = tempfile::tempdir().unwrap();
    isolate(dir.path());
    struct RestoreTimeout(Option<std::ffi::OsString>);
    impl Drop for RestoreTimeout {
        fn drop(&mut self) {
            match self.0.take() {
                Some(value) => std::env::set_var("KHIVE_REQUEST_READ_TIMEOUT_SECS", value),
                None => std::env::remove_var("KHIVE_REQUEST_READ_TIMEOUT_SECS"),
            }
        }
    }
    let _timeout = RestoreTimeout(std::env::var_os("KHIVE_REQUEST_READ_TIMEOUT_SECS"));
    std::env::remove_var("KHIVE_REQUEST_READ_TIMEOUT_SECS");
    let server = make_comm_test_server(Some("reader"));
    let mut first = server.wire_daemon_frame(&crate::tools::request::RequestParams {
        ops: "comm.inbox(status=\"unread\", limit=20, wait_ms=30000)".into(),
        request_id: Some(17),
        ..Default::default()
    });
    let second = server.wire_daemon_frame(&crate::tools::request::RequestParams {
        ops: first.ops.clone(),
        request_id: Some(18),
        ..Default::default()
    });
    first.from_wire = false; // The first request uses the CLI's operator frame.
    let listener = tokio::net::UnixListener::bind(socket_path()).unwrap();
    let peer = tokio::spawn(async move {
        let mut served = tokio::task::JoinSet::new();
        let mut dropped = false;
        for _ in 0..3 {
            let (mut stream, _) = listener.accept().await.unwrap();
            let bytes = read_frame(&mut stream).await.unwrap();
            let frame: DaemonRequestFrame = serde_json::from_slice(&bytes).unwrap();
            if frame.request_id == Some(18) && !dropped {
                dropped = true;
                continue; // Fully received frame, injected response loss.
            }
            let server = server.clone();
            served.spawn(async move {
                let response = real_response(server, &bytes).await;
                write_frame(&mut stream, &response).await.unwrap();
            });
        }
        while let Some(result) = served.join_next().await {
            result.unwrap();
        }
        assert!(dropped);
    });
    // Real SQLite blocking tasks and socket readiness make automatic paused
    // time inappropriate here. Both real 30-second waits run concurrently;
    // the outer timeout bounds the entire fixture to 45 seconds.
    let (first_result, second_result) = tokio::time::timeout(Duration::from_secs(45), async {
        tokio::join!(
            forward_or_spawn_with_config_and_packs(&first, None, None, None),
            khive_storage::scope_request_read_deadline(
                crate::request_policy::read_timeout(&second.ops, Duration::from_secs(30)),
                forward_or_spawn_with_config_and_packs(&second, None, None, None),
            ),
        )
    })
    .await
    .expect("both ceiling polls must finish within the fixture bound");
    for result in [first_result, second_result] {
        let body: serde_json::Value = serde_json::from_str(
            &result
                .expect("no local fallback")
                .expect("long poll response survives transport"),
        )
        .unwrap();
        assert_eq!(body["results"][0]["ok"], true, "{body}");
        // An empty page renders `count: 0`; the compact presentation omits
        // the empty `messages` array, so accept absent or empty.
        let result = &body["results"][0]["result"];
        assert_eq!(result["count"], 0, "{body}");
        assert!(
            result["messages"].is_null() || result["messages"] == serde_json::json!([]),
            "{body}"
        );
    }
    peer.await.unwrap();
    assert_eq!(KILL_COUNT.load(Ordering::SeqCst), 0);
}

#[tokio::test]
#[serial]
#[serial_test::serial(config_ledger)]
async fn cli_forward_replays_classified_reads_once_but_never_mutations_unknown_or_mixed() {
    if crate::test_isolation::rerun_with_private_home() {
        return;
    }

    let _cleanup = RecoveryTestGuard::new();
    for (ops, replay, creates_message) in [
        ("comm.inbox(limit=20)", true, false),
        ("search(kind=\"entity\", query=\"missing\")", true, false),
        (
            "comm.send(to=\"reader\", content=\"committed once\", self_send=true)",
            false,
            true,
        ),
        ("unknown.read()", false, false),
        (
            "[comm.unread(), comm.send(to=\"reader\", content=\"mixed committed once\", self_send=true)]",
            false,
            true,
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        isolate(dir.path());
        let server = make_comm_test_server(Some("reader"));
        if replay {
            let seed = server
                .dispatch_request_local(crate::tools::request::RequestParams {
                    ops: "comm.send(to=\"reader\", content=\"successful page\", self_send=true)".into(),
                    ..Default::default()
                })
                .await
                .unwrap();
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&seed).unwrap()["results"][0]["ok"],
                true,
                "{seed}"
            );
        }
        let mut frame = server.wire_daemon_frame(&crate::tools::request::RequestParams {
            ops: ops.into(),
            request_id: Some(4045),
            ..Default::default()
        });
        frame.from_wire = false;
        let expected = serde_json::to_value(&frame).unwrap();
        let listener = tokio::net::UnixListener::bind(socket_path()).unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let peer_calls = calls.clone();
        let peer_server = server.clone();
        let (done_tx, mut done_rx) = tokio::sync::oneshot::channel::<()>();
        let peer = tokio::spawn(async move {
            loop {
                let (mut stream, _) = tokio::select! {
                    _ = &mut done_rx => break,
                    accepted = listener.accept() => accepted.unwrap(),
                };
                let bytes = read_frame(&mut stream).await.unwrap();
                assert_eq!(
                    serde_json::from_slice::<serde_json::Value>(&bytes).unwrap(),
                    expected
                );
                let attempt = peer_calls.fetch_add(1, Ordering::SeqCst);
                let response = real_response(peer_server.clone(), &bytes).await;
                if attempt > 0 {
                    write_frame(&mut stream, &response).await.unwrap();
                }
                // Attempt zero really executed through the production frame
                // handler, then loses its response after any domain commit.
            }
        });
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            forward_or_spawn_with_config_and_packs(&frame, None, None, None),
        )
        .await
        .unwrap()
        .expect("a fully sent request can never fall back locally");
        let _ = done_tx.send(());
        peer.await.unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            if replay { 2 } else { 1 },
            "{ops}"
        );
        if replay {
            let body: serde_json::Value =
                serde_json::from_str(&result.expect("read retry succeeds")).unwrap();
            assert_eq!(body["results"][0]["ok"], true, "{body}");
            if ops.starts_with("comm.inbox") {
                assert_eq!(
                    body["results"][0]["result"]["messages"]
                        .as_array()
                        .unwrap()
                        .len(),
                    1
                );
            }
        } else {
            assert!(result
                .unwrap_err()
                .message
                .contains("not retrying or locally dispatching"));
        }
        let inbox: serde_json::Value = serde_json::from_str(
            &server
                .dispatch_request_local(crate::tools::request::RequestParams {
                    ops: "comm.inbox(limit=20)".into(),
                    ..Default::default()
                })
                .await
                .unwrap(),
        )
        .unwrap();
        // An empty inbox page carries no `messages` key.
        assert_eq!(
            inbox["results"][0]["result"]["messages"]
                .as_array()
                .map_or(0, Vec::len),
            usize::from(replay || creates_message),
            "{ops}: {inbox}"
        );
        assert_eq!(KILL_COUNT.load(Ordering::SeqCst), 0);
    }
}
