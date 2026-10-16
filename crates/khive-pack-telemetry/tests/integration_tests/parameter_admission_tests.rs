#[tokio::test]
async fn malformed_parameters_refuse_without_writes() {
    let (registry, _) = registry(config());
    for (verb, params) in [
        ("telemetry.emit", json!({"kind":"run.started"})),
        ("telemetry.emit", json!({"kind":"", "payload":{}})),
        ("telemetry.emit", json!({"kind":1, "payload":{}})),
        (
            "telemetry.emit",
            json!({"kind":"run started", "payload":{}}),
        ),
        (
            "telemetry.emit",
            json!({"kind":"*.heartbeat", "payload":{}}),
        ),
        (
            "telemetry.emit",
            json!({"kind":"run.started", "payload":{}, "unexpected":true}),
        ),
        ("telemetry.read", json!({"stream":"events", "since":-1})),
        ("telemetry.read", json!({"stream":"events", "limit":1001})),
        ("telemetry.read", json!({"stream":"events", "kinds":[]})),
        (
            "telemetry.read",
            json!({"stream":"events", "kinds":["*.heartbeat"]}),
        ),
        ("telemetry.counts", json!({"stream":"events"})),
        (
            "telemetry.counts",
            json!({"stream":"events", "window":{"since":"2026-01-01"}}),
        ),
        (
            "telemetry.counts",
            json!({"stream":"events", "window":{"since":"2026-01-01T00:00:00Z","until":"2025-01-01T00:00:00Z"}}),
        ),
        (
            "telemetry.counts",
            json!({"stream":"events", "window":{"since":"2026-01-01T00:00:00Z"}, "group_by":["kind","kind"]}),
        ),
        ("telemetry.channels", json!({"unused":true})),
    ] {
        assert!(
            registry.dispatch(verb, params.clone()).await.is_err(),
            "{verb}: {params}"
        );
    }
    assert!(existing_read(&registry, "events").await["entries"]
        .as_array()
        .unwrap()
        .is_empty());
}

#[test]
fn direct_runtime_configuration_fails_at_registry_activation() {
    let mut telemetry = config();
    telemetry.channels[0].kinds.clear();
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        actor_id: None,
        brain_profile: None,
        telemetry,
        ..RuntimeConfig::no_embeddings()
    })
    .unwrap();
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(runtime.clone()));
    builder.register(TelemetryPack::new(runtime));
    let error = builder
        .build()
        .err()
        .expect("invalid channel refused on load");
    assert!(
        error.to_string().contains("telemetry.channels[0]"),
        "{error}"
    );
}
