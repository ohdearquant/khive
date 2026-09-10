#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn plan_mcp_preserves_graph_events_and_request_identity() {
    let server = make_daemon_save_to_test_server();
    let store = server.event_store().unwrap();
    let ops = r#"create(kind="entity", entity_kind="concept", name="first") | create(kind="entity", entity_kind="concept", name="second")"#;
    let baseline = server
        .dispatch_request_local(RequestParams {
            ops: "stats()".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    let events = store.count_events(EventFilter::default()).await.unwrap();
    let log = SearchCapturedLog::default();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .without_time()
        .with_ansi(false)
        .with_writer(log.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let params = serde_json::from_value(json!({"ops":ops,"plan":true})).unwrap();
    let plan = server
        .request(
            Parameters(params),
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
    let plan: Value = serde_json::from_str(&plan).unwrap();
    assert_eq!(plan["parsed"], true);
    assert_eq!(plan["stage_count"], 2);
    assert!(!log.contents().contains("bridge correlation id"));
    assert!(!log.contents().contains("RequestIdentity"));
    assert_eq!(
        store.count_events(EventFilter::default()).await.unwrap(),
        events
    );
    let after = server
        .dispatch_request_local(RequestParams {
            ops: "stats()".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(
        stats_without_request_local_usage(&after),
        stats_without_request_local_usage(&baseline)
    );
    let before_control = store.count_events(EventFilter::default()).await.unwrap();
    let result = server
        .dispatch_request_local(RequestParams {
            ops: ops.into(),
            request_id: Some(8123),
            ..Default::default()
        })
        .await
        .unwrap();
    let result: Value = serde_json::from_str(&result).unwrap();
    assert_eq!(result["summary"]["succeeded"], 2);
    assert!(store.count_events(EventFilter::default()).await.unwrap() > before_control);
    assert!(find_audit_event_with_request_id(&store, 8123)
        .await
        .is_some());
    let final_stats = server
        .dispatch_request_local(RequestParams {
            ops: "stats()".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_ne!(
        stats_without_request_local_usage(&final_stats),
        stats_without_request_local_usage(&baseline)
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn plan_mcp_uses_loaded_catalog_and_preserves_parser_errors() {
    let server = make_daemon_save_to_test_server();
    for ops in [
        "stats() | missing_verb(id=$prev.id) | stats(x=$prev)",
        "stats(",
        "",
    ] {
        let params = serde_json::from_value(json!({"ops":ops,"plan":true})).unwrap();
        let plan = server
            .request(
                Parameters(params),
                tokio_util::sync::CancellationToken::new(),
            )
            .await
            .unwrap();
        let plan: Value = serde_json::from_str(&plan).unwrap();
        match parse_request(ops) {
            Ok(_) => {
                assert_eq!(plan["stage_count"], 3);
                assert_eq!(plan["stages"][0]["pack"], "kg");
                assert_eq!(plan["stages"][1]["known"], false);
                assert_eq!(plan["stages"][1]["prev_refs"], json!(["id"]));
                assert_eq!(plan["stages"][2]["prev_refs"], json!([""]));
            }
            Err(_) => {
                let error = server
                    .dispatch_request_local(RequestParams {
                        ops: ops.into(),
                        ..Default::default()
                    })
                    .await
                    .unwrap_err();
                assert_eq!(plan["parsed"], false);
                assert_eq!(plan["error"], error.message.as_ref());
                assert!(plan.get("stages").is_none());
            }
        }
    }
}

#[cfg(unix)]
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn plan_daemon_preserves_graph_events_and_identity_until_dispatch_control() {
    use khive_runtime::daemon::{read_frame, write_frame, DaemonResponseFrame, PROTOCOL_VERSION};
    let server = make_daemon_save_to_test_server();
    let store = server.event_store().unwrap();
    let ops = r#"create(kind="entity", entity_kind="concept", name="first") | create(kind="entity", entity_kind="concept", name="second")"#;
    let baseline = server
        .dispatch_request_local(RequestParams {
            ops: "stats()".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    let events = store.count_events(EventFilter::default()).await.unwrap();
    let log = SearchCapturedLog::default();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .without_time()
        .with_ansi(false)
        .with_writer(log.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    for plan in [true, false] {
        let (mut client, peer) = tokio::net::UnixStream::pair().unwrap();
        let dispatch_server = server.clone();
        let task = tokio::spawn(async move {
            khive_runtime::daemon::handle_conn_for_test(peer, dispatch_server).await;
        });
        let payload = json!({"ops":ops,"plan":plan,"namespace":"test",
            "config_id":server.config_id(),"protocol_version":PROTOCOL_VERSION});
        write_frame(&mut client, &serde_json::to_vec(&payload).unwrap())
            .await
            .unwrap();
        let response: DaemonResponseFrame =
            serde_json::from_slice(&read_frame(&mut client).await.unwrap()).unwrap();
        task.await.unwrap();
        assert!(response.ok, "{:?}", response.error);
        if plan {
            let result: Value = serde_json::from_str(response.result.as_deref().unwrap()).unwrap();
            assert_eq!(result["stage_count"], 2);
            assert_eq!(
                store.count_events(EventFilter::default()).await.unwrap(),
                events
            );
            assert!(!log.contents().contains("RequestIdentity"));
        } else {
            let result: Value = serde_json::from_str(response.result.as_deref().unwrap()).unwrap();
            assert_eq!(result["summary"]["succeeded"], 2);
            assert!(store.count_events(EventFilter::default()).await.unwrap() > events);
            assert!(log.contents().contains("RequestIdentity"));
        }
        let stats = server
            .dispatch_request_local(RequestParams {
                ops: "stats()".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        if plan {
            assert_eq!(
                stats_without_request_local_usage(&stats),
                stats_without_request_local_usage(&baseline)
            );
        } else {
            assert_ne!(
                stats_without_request_local_usage(&stats),
                stats_without_request_local_usage(&baseline)
            );
        }
    }
}
