use chrono::{DateTime, Utc};
use khive_pack_kg::KgPack;
use khive_pack_telemetry::TelemetryPack;
use khive_runtime::{
    KhiveRuntime, RuntimeConfig, TelemetryCarrier, TelemetryChannelConfig, TelemetryConfig,
    TelemetryFailurePosture, VerbRegistry, VerbRegistryBuilder,
};
use khive_storage::{SqlStatement, SqlValue};
use serde_json::{json, Value};

fn config() -> TelemetryConfig {
    TelemetryConfig {
        stream: "events".into(),
        default_carrier: TelemetryCarrier::Ephemeral,
        channels: vec![
            TelemetryChannelConfig {
                kinds: vec!["run.started".into(), "run.completed".into()],
                carrier: TelemetryCarrier::Durable,
                failure_posture: TelemetryFailurePosture::Stop,
            },
            TelemetryChannelConfig {
                kinds: vec!["turn.delta".into(), "*.heartbeat".into()],
                carrier: TelemetryCarrier::Ephemeral,
                failure_posture: TelemetryFailurePosture::Gap,
            },
        ],
    }
}

fn registry(telemetry: TelemetryConfig) -> (VerbRegistry, KhiveRuntime) {
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        packs: vec!["kg".into(), "telemetry".into()],
        brain_profile: None,
        actor_id: None,
        telemetry,
        ..RuntimeConfig::no_embeddings()
    })
    .expect("in-memory runtime");
    let mut builder = VerbRegistryBuilder::new();
    builder.with_actor_id(Some("worker".into()));
    builder.register(KgPack::new(runtime.clone()));
    builder.register(TelemetryPack::new(runtime.clone()));
    (builder.build().expect("registry"), runtime)
}

async fn emit(registry: &VerbRegistry, kind: &str, payload: Value) -> Value {
    registry
        .dispatch("telemetry.emit", json!({"kind": kind, "payload": payload}))
        .await
        .expect("emit")
}

async fn existing_read(registry: &VerbRegistry, stream: &str) -> Value {
    registry
        .dispatch("stream.read", json!({"stream": stream, "limit": 100}))
        .await
        .expect("existing stream.read")
}

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
    assert_eq!(emitted["dropped"], false);
    assert_eq!(emitted["receipt_persisted"], true);
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
            "actor": "worker", "run_id": "run-1",
        })
    );
}

#[tokio::test]
async fn ephemeral_kind_is_accepted_but_absent_from_stream() {
    let (registry, _) = registry(config());
    let result = emit(&registry, "turn.delta", json!({"delta": "hello"})).await;
    assert_eq!(result["accepted"], true);
    assert_eq!(result["carrier"], "ephemeral");
    assert_eq!(result["dropped"], true);
    assert_eq!(result["receipt_persisted"], false);
    assert_eq!(result["cursor_kind"], "none");
    assert!(result.get("seq").is_none());
    assert!(uuid::Uuid::parse_str(result["receipt_id"].as_str().unwrap()).is_ok());
    assert!(existing_read(&registry, "events").await["entries"]
        .as_array()
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn unlisted_kind_uses_fail_cheap_default_and_is_absent() {
    let (registry, _) = registry(config());
    let result = emit(&registry, "new.kind", Value::Null).await;
    assert_eq!(result["carrier"], "ephemeral");
    assert_eq!(result["dropped"], true);
    assert!(existing_read(&registry, "events").await["entries"]
        .as_array()
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn configured_durable_default_routes_an_unlisted_kind_to_the_stream() {
    let mut config = config();
    config.default_carrier = TelemetryCarrier::Durable;
    let (registry, _) = registry(config);
    let result = emit(&registry, "new.kind", json!(42)).await;
    assert_eq!(result["carrier"], "durable");
    assert_eq!(result["dropped"], false);
    assert_eq!(
        existing_read(&registry, "events").await["entries"][0]["record"]["payload"],
        42
    );
}

#[tokio::test]
async fn suffix_channel_is_effective_and_channels_explain_drop_retention() {
    let mut config = config();
    config.default_carrier = TelemetryCarrier::Durable;
    let (registry, _) = registry(config);
    let result = emit(&registry, "runner.heartbeat", json!([])).await;
    assert_eq!(result["carrier"], "ephemeral");
    assert_eq!(result["dropped"], true);
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
    let error = registry
        .dispatch(
            "telemetry.emit",
            json!({"kind": "run.started", "payload": {}, "actor": "another-worker"}),
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("actor"));
    assert_eq!(
        existing_read(&registry, "events").await["entries"]
            .as_array()
            .unwrap()
            .len(),
        5
    );
}

async fn append_at(registry: &VerbRegistry, runtime: &KhiveRuntime, record: Value, at: &str) {
    let appended = registry
        .dispatch(
            "stream.append",
            json!({"stream": "other-stream", "record": record, "embed": false}),
        )
        .await
        .expect("seed arbitrary stream");
    let micros = DateTime::parse_from_rfc3339(at).unwrap().timestamp_micros();
    let mut writer = runtime.sql().writer().await.unwrap();
    writer
        .execute(SqlStatement {
            sql: "UPDATE notes SET created_at = ?1 WHERE id = ?2".into(),
            params: vec![
                SqlValue::Integer(micros),
                SqlValue::Text(appended["id"].as_str().unwrap().into()),
            ],
            label: Some("telemetry_test_timestamp".into()),
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn arbitrary_stream_rollup_groups_and_excludes_existing_rows_at_both_boundaries() {
    let (registry, runtime) = registry(config());
    for (at, kind, actor, verb) in [
        ("2026-01-01T00:00:00Z", "tool", "a", "old"),
        ("2026-01-02T00:00:00Z", "tool", "a", "read"),
        ("2026-01-02T12:00:00Z", "tool", "a", "read"),
        ("2026-01-02T18:00:00Z", "tool", "b", "write"),
        ("2026-01-03T00:00:00Z", "tool", "a", "new"),
    ] {
        append_at(
            &registry,
            &runtime,
            json!({"kind":kind,"actor":actor,"payload":{"verb":verb}}),
            at,
        )
        .await;
    }
    let result = registry
        .dispatch(
            "telemetry.counts",
            json!({
                "stream":"other-stream",
                "window":{"since":"2026-01-02T00:00:00Z","until":"2026-01-03T00:00:00Z"},
                "group_by":["kind","actor","payload.verb"],
            }),
        )
        .await
        .unwrap();
    assert_eq!(result["total"], 3);
    assert_eq!(result["scanned"], 5);
    assert_eq!(result["head_seq"], 5);
    assert_eq!(
        result["rows"],
        json!([
            {"key":{"kind":"tool","actor":"a","payload.verb":"read"},"count":2},
            {"key":{"kind":"tool","actor":"b","payload.verb":"write"},"count":1},
        ])
    );
    assert_eq!(
        existing_read(&registry, "other-stream").await["entries"]
            .as_array()
            .unwrap()
            .len(),
        5
    );
    assert!(existing_read(&registry, "events").await["entries"]
        .as_array()
        .unwrap()
        .is_empty());
    let ungrouped = registry
        .dispatch(
            "telemetry.counts",
            json!({
                "stream":"other-stream",
                "window":{"since":"2026-01-01T00:00:00Z","until":"2026-01-04T00:00:00Z"}
            }),
        )
        .await
        .unwrap();
    assert_eq!(ungrouped["total"], 5);
    assert_eq!(
        ungrouped["rows"],
        json!([{"key":{"kind":"tool"},"count":5}])
    );
    let excluded = registry
        .dispatch(
            "telemetry.counts",
            json!({
                "stream":"other-stream", "kinds":["absent"],
                "window":{"since":"2026-01-01T00:00:00Z","until":"2026-01-04T00:00:00Z"}
            }),
        )
        .await
        .unwrap();
    assert_eq!(excluded["total"], 0);
    assert_eq!(excluded["scanned"], 5);
}

#[tokio::test]
async fn read_cursor_advances_across_filtered_rows_and_resumes_at_tail() {
    let (registry, _) = registry(config());
    emit(&registry, "run.started", Value::Null).await;
    emit(&registry, "run.started", Value::Null).await;
    let excluded = registry
        .dispatch(
            "telemetry.read",
            json!({
                "stream":"events", "limit":1, "kinds":["run.completed"]
            }),
        )
        .await
        .unwrap();
    assert_eq!(excluded["events"], json!([]));
    assert_eq!(excluded["next_cursor"], 1);
    assert_eq!(excluded["has_more"], true);
    let tail = registry
        .dispatch(
            "telemetry.read",
            json!({
                "stream":"events", "since":excluded["next_cursor"], "kinds":["run.completed"]
            }),
        )
        .await
        .unwrap();
    assert_eq!(tail["events"], json!([]));
    assert_eq!(tail["next_cursor"], 2);
    assert_eq!(tail["has_more"], false);
    assert!(tail.get("gap").is_none());
    emit(&registry, "run.completed", Value::Null).await;
    let resumed = registry
        .dispatch(
            "telemetry.read",
            json!({
                "stream":"events", "since":tail["next_cursor"], "kinds":["run.completed"]
            }),
        )
        .await
        .unwrap();
    assert_eq!(resumed["events"].as_array().unwrap().len(), 1);
    assert_eq!(resumed["next_cursor"], 3);
    assert_eq!(resumed["ephemeral_retention"], "none");
}

#[tokio::test]
async fn counts_default_until_is_now_and_missing_dimensions_are_null() {
    let (registry, runtime) = registry(config());
    append_at(
        &registry,
        &runtime,
        json!({"kind":"tool"}),
        "2000-01-01T00:00:00Z",
    )
    .await;
    let before = Utc::now();
    let result = registry
        .dispatch(
            "telemetry.counts",
            json!({
                "stream":"other-stream", "window":{"since":"1999-01-01T00:00:00Z"},
                "group_by":["payload.verb"],
            }),
        )
        .await
        .unwrap();
    let until = DateTime::parse_from_rfc3339(result["window"]["until"].as_str().unwrap())
        .unwrap()
        .with_timezone(&Utc);
    assert!(until >= before && until <= Utc::now());
    assert_eq!(
        result["rows"],
        json!([{"key":{"payload.verb":null},"count":1}])
    );
}

#[tokio::test]
async fn counts_reads_more_than_one_stream_page() {
    let (registry, _) = registry(config());
    let ops: Vec<_> = (0..1_000)
        .map(|_| json!({"op":"append", "stream":"paged", "record":{"kind":"first"}, "embed":false}))
        .collect();
    registry
        .dispatch("stream.batch", json!({"ops":ops,"atomic":true}))
        .await
        .expect("seed first full page");
    registry
        .dispatch(
            "stream.append",
            json!({"stream":"paged", "record":{"kind":"last"}, "embed":false}),
        )
        .await
        .expect("seed second page");
    let result = registry
        .dispatch(
            "telemetry.counts",
            json!({
                "stream":"paged",
                "window":{"since":"2000-01-01T00:00:00Z", "until":"2100-01-01T00:00:00Z"}
            }),
        )
        .await
        .unwrap();
    assert_eq!(result["total"], 1_001);
    assert_eq!(result["scanned"], 1_001);
    assert_eq!(result["head_seq"], 1_001);
    assert_eq!(
        result["rows"],
        json!([
            {"key":{"kind":"first"}, "count":1_000},
            {"key":{"kind":"last"}, "count":1},
        ])
    );
}

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

#[tokio::test]
async fn direct_runtime_configuration_also_fails_closed_before_dispatch() {
    let mut config = config();
    config.channels[0].kinds.clear();
    let (registry, _) = registry(config);
    let error = registry
        .dispatch("telemetry.emit", json!({"kind":"unknown","payload":{}}))
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("telemetry.channels[0]"),
        "{error}"
    );
}
