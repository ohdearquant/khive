// Real MCP request admission and Unix framing with a controlled async peer.
#[tokio::test(start_paused = true)]
#[serial]
#[serial_test::serial(config_ledger)]
async fn mcp_long_poll_scope_allows_transport_margin_but_keeps_earlier_outer_deadline() {
    use khive_runtime::daemon::{
        read_frame, write_frame, DaemonRequestFrame, DaemonResponseFrame, PROTOCOL_VERSION,
    };

    struct Environment(Vec<(&'static str, Option<std::ffi::OsString>)>);
    impl Drop for Environment {
        fn drop(&mut self) {
            for (key, value) in self.0.drain(..) {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }
    struct AbortTask<T>(tokio::task::JoinHandle<T>);
    impl<T> Drop for AbortTask<T> {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    let _environment = Environment(
        ["KHIVE_SOCKET", "KHIVE_NO_DAEMON"]
            .into_iter()
            .map(|key| (key, std::env::var_os(key)))
            .collect(),
    );
    std::env::remove_var("KHIVE_NO_DAEMON");
    for earlier in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("s");
        std::env::set_var("KHIVE_SOCKET", &socket);
        let listener = tokio::net::UnixListener::bind(socket).unwrap();
        let server = KhiveMcpServer::from_registry(VerbRegistryBuilder::new().build().unwrap());
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let peer = AbortTask(tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let frame: DaemonRequestFrame =
                serde_json::from_slice(&read_frame(&mut stream).await.unwrap()).unwrap();
            ready_tx.send(()).unwrap();
            tokio::time::sleep(Duration::from_secs(31)).await;
            let response = DaemonResponseFrame {
                ok: true,
                result: Some(serde_json::json!({"results": [{"ok": true, "tool": "comm.inbox", "result": {"messages": []}}]}).to_string()),
                error: None,
                error_detail: None,
                namespace_mismatch: false,
                config_mismatch: false,
                served_config_id: Some(frame.config_id),
                version_mismatch: false,
                daemon_protocol_version: PROTOCOL_VERSION,
                metrics: None,
                request_id: frame.request_id,
            };
            write_frame(&mut stream, &serde_json::to_vec(&response).unwrap())
                .await
                .unwrap();
        }));
        // Keep Tokio runnable until real socket admission is observed. This
        // prevents paused time from auto-jumping to a deadline while OS I/O is
        // merely pending; the explicit advance below is the only clock jump.
        let _clock_guard = AbortTask(tokio::spawn(async {
            loop {
                tokio::task::yield_now().await;
            }
        }));
        let mut call = AbortTask(tokio::spawn(async move {
            let request = server.request(
                rmcp::handler::server::wrapper::Parameters(RequestParams {
                    ops: "comm.inbox(wait_ms=30000)".into(),
                    ..Default::default()
                }),
                tokio_util::sync::CancellationToken::new(),
            );
            if earlier {
                khive_storage::scope_request_read_deadline(Duration::from_secs(1), request).await
            } else {
                request.await
            }
        }));
        tokio::select! {
            ready = ready_rx => ready.unwrap(),
            result = &mut call.0 => panic!("request ended before the real frame was received: {result:?}"),
        }
        tokio::time::advance(Duration::from_secs(if earlier { 2 } else { 31 })).await;
        let result = (&mut call.0).await.unwrap();
        if earlier {
            assert!(
                result.is_err(),
                "long-poll allowance must not renew an earlier caller deadline"
            );
        } else {
            let body: Value = serde_json::from_str(
                &result.expect("MCP's scope must retain the transport margin"),
            )
            .unwrap();
            assert_eq!(
                body["results"][0]["result"]["messages"],
                serde_json::json!([])
            );
        }
        drop(peer);
    }
}
