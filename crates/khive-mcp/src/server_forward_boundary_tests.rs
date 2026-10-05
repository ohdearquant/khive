    #[cfg(unix)]
    #[tokio::test]
    #[serial_test::serial]
    #[serial_test::serial(config_ledger)]
    async fn bridge_executable_replacement_refuses_at_request_boundary() {
        use crate::daemon::{
            fire_pending_self_heal, reset_self_heal_counters, REEXEC_INVOKED_COUNT,
        };

        reset_self_heal_counters();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bridge");
        std::fs::write(&path, b"binary image").unwrap();
        let executable = crate::daemon::executable::BridgeExecutable::at(path.clone()).unwrap();
        let mut server = KhiveMcpServer::from_registry(VerbRegistryBuilder::new().build().unwrap());
        server.bridge_executable = Some(Arc::new(std::sync::Mutex::new(executable)));
        std::fs::copy(&path, dir.path().join("next")).unwrap();
        std::fs::rename(dir.path().join("next"), path).unwrap();

        for params in [
            RequestParams {
                ops: "stats()".to_string(),
                plan: Some(true),
                ..Default::default()
            },
            RequestParams {
                ops: "stats(".to_string(),
                ..Default::default()
            },
        ] {
            let error = server.request_with_cancellation(params).await.expect_err(
                "the stale stdio bridge must refuse before planning, parsing or dispatch",
            );
            assert_eq!(error.data.unwrap()["reason"], "executable_replaced");
        }
        assert_eq!(REEXEC_INVOKED_COUNT.load(Ordering::SeqCst), 0);
        fire_pending_self_heal();
        fire_pending_self_heal();
        assert_eq!(REEXEC_INVOKED_COUNT.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn bridge_request_id_is_unconditional_and_preserves_caller_value() {
        let mut generated = RequestParams {
            ops: "stats()".to_string(),
            ..Default::default()
        };
        let first = ensure_bridge_request_id(&mut generated);
        assert_ne!(first, 0, "bridge-generated request ids are nonzero");
        assert_eq!(generated.request_id, Some(first));
        assert_eq!(
            ensure_bridge_request_id(&mut generated),
            first,
            "one admitted MCP attempt keeps one stable id"
        );

        let mut supplied = RequestParams {
            ops: "stats()".to_string(),
            request_id: Some(42),
            ..Default::default()
        };
        assert_eq!(ensure_bridge_request_id(&mut supplied), 42);
        assert_eq!(supplied.request_id, Some(42));
    }

    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn wire_dispatch_retains_raw_one_mib_input_limit() {
        let runtime = KhiveRuntime::new(RuntimeConfig {
            db_path: None,
            embedding_model: None,
            additional_embedding_models: vec![],
            packs: vec!["kg".to_string()],
            ..RuntimeConfig::default()
        })
        .expect("in-memory runtime");
        let server = KhiveMcpServer::new(runtime).expect("server builds with kg");
        let params = RequestParams {
            plan: None,
            ops: json!({
                "tool": "stats",
                "args": {"payload": "x".repeat(khive_request::MAX_OPS_INPUT_LEN + 1)},
            })
            .to_string(),
            presentation: Some("verbose".to_string()),
            presentation_per_op: None,
            save_to: None,
            format: Some("json".to_string()),
            format_per_op: None,
            request_id: None,
        };

        let error = server
            .dispatch_request_wire(params)
            .await
            .expect_err("the public wire path must reject raw ops above 1 MiB");

        assert_eq!(error.code, rmcp::model::ErrorCode::INVALID_PARAMS);
        assert!(error.message.contains("ops input is"), "{error}");
        assert_eq!(
            error.data.as_ref().and_then(|data| data["reason"].as_str()),
            Some("parse-error")
        );
    }

    #[cfg(unix)]
    fn forward_test_runtime(db_path: Option<std::path::PathBuf>, packs: &[&str]) -> KhiveRuntime {
        KhiveRuntime::new(RuntimeConfig {
            db_path,
            actor_id: Some("local".to_string()),
            embedding_model: None,
            additional_embedding_models: vec![],
            events_split: None,
            packs: packs.iter().map(|pack| (*pack).to_string()).collect(),
            ..RuntimeConfig::default()
        })
        .expect("forwarding fixture runtime")
    }

    #[cfg(unix)]
    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn in_memory_request_skips_forward_and_dispatches_locally() {
        static FORWARD_CALLS: AtomicUsize = AtomicUsize::new(0);
        fn spy_forward(
            _frame: khive_runtime::DaemonRequestFrame,
            _packs: Option<Vec<String>>,
            _replay_read_only: bool,
        ) -> ForwardFuture {
            FORWARD_CALLS.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Some(Ok("unexpected-forward".to_string())) })
        }

        FORWARD_CALLS.store(0, Ordering::SeqCst);
        let runtime = forward_test_runtime(None, &["kg"]);
        assert!(!runtime.backend().is_file_backed());
        let server = KhiveMcpServer::new(runtime).expect("in-memory server");
        let response = server
            .request_with_forward(
                RequestParams {
                    ops: "stats()".to_string(),
                    ..Default::default()
                },
                spy_forward,
            )
            .await
            .expect("ordinary in-memory request dispatches locally");

        assert_eq!(FORWARD_CALLS.load(Ordering::SeqCst), 0);
        let response: Value = serde_json::from_str(&response).expect("local JSON envelope");
        assert_eq!(response["results"][0]["tool"], "stats");
        assert_eq!(response["results"][0]["ok"], true);
        assert_eq!(response["results"][0]["result"]["entities"], 0);
        assert_eq!(response["summary"]["succeeded"], 1);
    }

    #[cfg(unix)]
    #[tokio::test]
    #[serial_test::serial(config_ledger)]
    async fn file_backed_request_reaches_forward_seam() {
        static FORWARD_CALLS: AtomicUsize = AtomicUsize::new(0);
        fn spy_forward(
            _frame: khive_runtime::DaemonRequestFrame,
            _packs: Option<Vec<String>>,
            _replay_read_only: bool,
        ) -> ForwardFuture {
            FORWARD_CALLS.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Some(Ok("forwarded-file-backed-request".to_string())) })
        }

        FORWARD_CALLS.store(0, Ordering::SeqCst);
        let dir = tempfile::tempdir().expect("forwarding fixture directory");
        let runtime = forward_test_runtime(Some(dir.path().join("main.db")), &["kg"]);
        assert!(runtime.backend().is_file_backed());
        let server = KhiveMcpServer::new(runtime).expect("file-backed server");
        let response = server
            .request_with_forward(
                RequestParams {
                    ops: "stats()".to_string(),
                    ..Default::default()
                },
                spy_forward,
            )
            .await
            .expect("ordinary file-backed request reaches forwarding");

        assert_eq!(FORWARD_CALLS.load(Ordering::SeqCst), 1);
        assert_eq!(response, "forwarded-file-backed-request");
    }
