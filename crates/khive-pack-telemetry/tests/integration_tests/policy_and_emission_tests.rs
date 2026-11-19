#[tokio::test]
async fn telemetry_retry_policy_distinguishes_reads_from_emit() {
    let (registry, _) = registry(config());
    for (verb, retry_safe) in [
        ("telemetry.channels", true),
        ("telemetry.counts", true),
        ("telemetry.read", true),
        ("telemetry.emit", false),
    ] {
        assert_eq!(
            registry.is_retry_safe_after_frame_omission(verb),
            retry_safe,
            "{verb}"
        );
        assert!(!registry.is_read_replay_safe(verb), "{verb}");
    }
}

#[tokio::test]
async fn durable_kind_is_visible_through_existing_stream_read() {
    let (registry, _) = registry(config());
    let emitted = registry
        .dispatch(
            "telemetry.emit",
            json!({"kind": "run.started", "payload": {"extra": [1, true]}, "run_id": "run-1", "actor": "worker"}),
        )
        .await
        .unwrap();
    assert_eq!(emitted["carrier"], "durable");
    assert_eq!(emitted["outcome"], "recorded");
    assert!(emitted.get("error").is_none());
    assert!(emitted.get("dropped").is_none());
    assert_eq!(emitted["seq"], 1);
    assert_eq!(emitted["cursor_kind"], "log");
    let page = existing_read(&registry, "events").await;
    assert_eq!(page["entries"].as_array().unwrap().len(), 1);
    let entry = &page["entries"][0];
    assert_eq!(entry["id"], emitted["receipt_id"]);
    assert_eq!(entry["created_at"], emitted["created_at"]);
    assert_eq!(
        entry["record"],
        json!({
            "kind": "run.started", "payload": {"extra": [1, true]},
            "actor": "actor:worker", "run_id": "run-1",
        })
    );
}

#[tokio::test]
async fn ephemeral_kind_is_accepted_but_absent_from_stream() {
    let (registry, _) = registry(config());
    let result = emit(&registry, "turn.delta", json!({"delta": "hello"})).await;
    assert!(result.get("error").is_none());
    assert_eq!(result["carrier"], "ephemeral");
    assert_eq!(result["outcome"], "dropped");
    assert!(result["receipt_id"].is_null());
    assert_eq!(result["cursor_kind"], "none");
    assert!(result.get("seq").is_none());
    assert!(result.get("dropped").is_none());
    assert!(existing_read(&registry, "events").await["entries"]
        .as_array()
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn dispatch_refusal_secret_blocked_append_and_ephemeral_policy_have_distinct_shapes() {
    let mut config = config();
    config.channels[0].failure_posture = TelemetryFailurePosture::Gap;
    let (registry, runtime) = registry(config);
    let recorded = emit(&registry, "run.started", json!({"control":true})).await;
    assert_eq!(recorded["outcome"], "recorded");
    let before = existing_read(&registry, "events").await;

    // Namespace validation happens before the pack is reached.
    let refused = registry
        .dispatch_with_disposition(
            "telemetry.emit",
            json!({"kind":"run.started", "payload":{}, "namespace":3}),
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(
        refused.disposition(),
        khive_runtime::DomainDisposition::NotCommitted
    );
    let disposition = refused.disposition();
    let error = khive_runtime::runtime_error_value(refused.into_source(), disposition);
    assert!(error.get("outcome").is_none());

    // Synthetic detector input, not key material. The append's real note
    // preparation rejects this payload before storage; no ledger guards change.
    // Split the header as in the secret-gate unit fixtures, and give it one
    // line of key-block width under it: the detector needs a body, a bare
    // header is a mention of the format.
    let synthetic_header = ["-----BEGIN RSA", " PRIVATE KEY-----"].concat(); // gitleaks:allow
    let synthetic_block = format!(
        "{synthetic_header}\nMIIEowIBAAKCAQEA0Z3VS5JJcds3xfn/ygWyF8PbnGYPYFqHlZ4kUmUqQ7fEd7Uw"
    );
    let incident = emit(
        &registry,
        "run.started",
        json!({"synthetic_secret_shape":synthetic_block}),
    )
    .await;
    let policy = emit(&registry, "turn.delta", json!({"policy":true})).await;
    for result in [&incident, &policy] {
        assert_eq!(result["outcome"], "dropped");
        assert!(result["receipt_id"].is_null());
        assert!(result.get("seq").is_none());
    }
    assert_eq!(incident["carrier"], "durable");
    assert!(incident["error"].is_object());
    assert!(incident["error"]["message"]
        .as_str()
        .unwrap()
        .contains("pem-private-key"));
    assert_eq!(policy["carrier"], "ephemeral");
    assert!(policy.get("error").is_none());
    assert_eq!(existing_read(&registry, "events").await, before);
    let access = runtime.sql();
    let mut reader = access.reader().await.unwrap();
    assert!(matches!(
        reader
            .query_scalar(SqlStatement {
                sql: "SELECT COUNT(*) FROM note_streams WHERE namespace = ?1 AND stream = ?2"
                    .into(),
                params: vec![
                    SqlValue::Text("local".into()),
                    SqlValue::Text("events".into())
                ],
                label: Some("telemetry_test_refused_append_count".into()),
            })
            .await
            .unwrap(),
        Some(SqlValue::Integer(1))
    ));
}

#[tokio::test]
async fn unlisted_kind_uses_declared_default_and_reports_unclassified() {
    let (registry, _) = registry(config());
    let result = emit(&registry, "new.kind", Value::Null).await;
    assert_eq!(result["carrier"], "ephemeral");
    assert_eq!(result["outcome"], "dropped");
    assert!(existing_read(&registry, "events").await["entries"]
        .as_array()
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn configured_durable_default_routes_an_unlisted_kind_to_the_stream() {
    let mut config = config();
    config.default_carrier = Some(TelemetryCarrier::Durable);
    let (registry, _) = registry(config);
    let result = emit(&registry, "new.kind", json!(42)).await;
    assert_eq!(result["carrier"], "durable");
    assert_eq!(result["outcome"], "recorded");
    assert_eq!(
        existing_read(&registry, "events").await["entries"][0]["record"]["payload"],
        42
    );
}

#[tokio::test]
async fn suffix_channel_is_effective_and_channels_explain_drop_retention() {
    let mut config = config();
    config.default_carrier = Some(TelemetryCarrier::Durable);
    let (registry, _) = registry(config);
    let result = emit(&registry, "runner.heartbeat", json!([])).await;
    assert_eq!(result["carrier"], "ephemeral");
    assert_eq!(result["outcome"], "dropped");
    let table = registry
        .dispatch("telemetry.channels", json!({}))
        .await
        .unwrap();
    assert_eq!(table["stream"], "events");
    assert_eq!(table["default_carrier"], "durable");
    assert_eq!(table["channels"][0]["cursor_kind"], "log");
    assert_eq!(table["channels"][1]["cursor_kind"], "none");
    assert_eq!(table["ephemeral_retention"], "none");
}

#[tokio::test]
async fn generic_payload_has_no_kind_specific_schema_and_actor_cannot_be_forged() {
    let (registry, _) = registry(config());
    for payload in [
        Value::Null,
        json!(false),
        json!(42),
        json!("text"),
        json!([]),
    ] {
        emit(&registry, "run.started", payload).await;
    }
    let result = registry
        .dispatch(
            "telemetry.emit",
            json!({"kind":"run.started", "payload":{}, "actor":"another-worker"}),
        )
        .await
        .unwrap();
    assert_eq!(result["actor"], "actor:worker");
    assert_eq!(result["actor_argument_ignored"], true);
    assert_eq!(
        existing_read(&registry, "events").await["entries"]
            .as_array()
            .unwrap()
            .len(),
        6
    );
}
