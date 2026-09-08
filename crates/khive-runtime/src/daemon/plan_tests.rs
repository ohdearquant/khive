async fn plan_raw_round_trip(payload: serde_json::Value) -> (DaemonResponseFrame, usize) {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let dispatcher = MockDispatch {
        namespace: "local".into(),
        config_id: "plan-config".into(),
        dispatch_calls: Arc::clone(&calls),
        pool: None,
        dispatch_err: None,
    };
    let (mut client, server) = UnixStream::pair().unwrap();
    let handle = tokio::spawn(async move {
        handle_conn(server, dispatcher).await;
    });
    write_frame(&mut client, &serde_json::to_vec(&payload).unwrap())
        .await
        .unwrap();
    let raw = read_frame(&mut client).await.unwrap();
    handle.await.unwrap();
    (
        serde_json::from_slice(&raw).unwrap(),
        calls.load(std::sync::atomic::Ordering::SeqCst),
    )
}

#[tokio::test]
async fn plan_daemon_frame_returns_plan_without_dispatch() {
    let payload = serde_json::json!({
        "ops":"missing_verb()", "namespace":"", "plan":true,
        "config_id":"plan-config", "protocol_version":PROTOCOL_VERSION
    });
    let (response, calls) = plan_raw_round_trip(payload).await;
    assert!(response.ok);
    assert_eq!(calls, 0);
    let result: serde_json::Value =
        serde_json::from_str(response.result.as_deref().unwrap()).unwrap();
    assert_eq!(result["parsed"], true);
    assert_eq!(result["stages"][0]["known"], false);
    assert!(response.request_id.is_none());
}

#[tokio::test]
async fn plan_daemon_refuses_each_present_companion_including_null() {
    for (field, value) in [
        ("presentation", serde_json::json!("verbose")),
        ("presentation_per_op", serde_json::json!([null])),
        ("format", serde_json::json!("json")),
        ("format_per_op", serde_json::json!([null])),
        ("request_id", serde_json::json!(7)),
    ] {
        for value in [
            value,
            serde_json::Value::Null,
            serde_json::json!({"invalid":"type"}),
        ] {
            let mut payload = serde_json::json!({
                "ops":"missing_verb()", "namespace":"", "plan":true,
                "config_id":"plan-config", "protocol_version":PROTOCOL_VERSION
            });
            payload[field] = value;
            let (response, calls) = plan_raw_round_trip(payload).await;
            assert!(!response.ok, "{field}");
            assert_eq!(calls, 0, "{field}");
            assert_eq!(
                response.error_detail.as_ref().unwrap()["domain_disposition"],
                "not_committed"
            );
            let error = response.error.unwrap();
            assert!(
                error.contains("invalid_params") && error.contains(field),
                "{error}"
            );
        }
    }
}

#[tokio::test]
async fn plan_daemon_protocol_rejects_previous_version_without_dispatch() {
    assert_eq!(PROTOCOL_VERSION, 5);
    let (response, calls) = plan_raw_round_trip(serde_json::json!({
        "ops":"missing_verb()", "namespace":"", "plan":true,
        "config_id":"plan-config", "protocol_version":4
    }))
    .await;
    assert!(!response.ok);
    assert!(response.version_mismatch);
    assert_eq!(calls, 0);
}

#[tokio::test]
async fn plan_daemon_duplicate_flags_do_not_reach_dispatch() {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let dispatcher = MockDispatch {
        namespace: "local".into(),
        config_id: "plan-config".into(),
        dispatch_calls: Arc::clone(&calls),
        pool: None,
        dispatch_err: None,
    };
    let (mut client, peer) = UnixStream::pair().unwrap();
    let task = tokio::spawn(async move {
        handle_conn(peer, dispatcher).await;
    });
    let payload = format!(
        r#"{{"ops":"missing_verb()","namespace":"","config_id":"plan-config","protocol_version":{PROTOCOL_VERSION},"plan":true,"plan":false}}"#
    );
    write_frame(&mut client, payload.as_bytes()).await.unwrap();
    let response = tokio::time::timeout(std::time::Duration::from_secs(5), read_frame(&mut client))
        .await
        .unwrap();
    assert!(response.is_err(), "duplicate fields must fail decoding");
    task.await.unwrap();
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
}
