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
        default_carrier: Some(TelemetryCarrier::Ephemeral),
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
    registry_with_scope(telemetry, "worker", &[], &[])
}

fn registry_with_scope(
    telemetry: TelemetryConfig,
    actor: &str,
    visible: &[&str],
    fleet: &[&str],
) -> (VerbRegistry, KhiveRuntime) {
    let mut runtime_config = RuntimeConfig {
        db_path: None,
        packs: vec!["kg".into(), "telemetry".into()],
        brain_profile: None,
        actor_id: None,
        telemetry,
        ..RuntimeConfig::no_embeddings()
    };
    runtime_config.brain.fleet_readers = fleet.iter().map(|actor| actor.to_string()).collect();
    let runtime = KhiveRuntime::new(runtime_config).expect("in-memory runtime");
    let mut builder = VerbRegistryBuilder::new();
    builder.with_actor_id(Some(actor.into()));
    builder.with_visible_namespaces(
        visible
            .iter()
            .map(|ns| khive_runtime::Namespace::parse(ns).unwrap())
            .collect(),
    );
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

include!("integration_tests/policy_and_emission_tests.rs");

async fn append_at(registry: &VerbRegistry, runtime: &KhiveRuntime, mut record: Value, at: &str) {
    record
        .as_object_mut()
        .unwrap()
        .entry("actor")
        .or_insert(json!("worker"));
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

include!("integration_tests/rollup_cursor_and_count_tests.rs");

include!("integration_tests/parameter_admission_tests.rs");

include!("integration_tests/caller_attribution_and_scope_tests.rs");

#[tokio::test]
async fn coverage_reports_current_policy_without_hiding_historical_durable_rows() {
    let backend = std::sync::Arc::new(khive_runtime::StorageBackend::memory().unwrap());
    backend.prepare_core_schema().unwrap();
    let runtime = KhiveRuntime::from_backend(
        backend.clone(),
        RuntimeConfig {
            db_path: None,
            packs: vec!["kg".into(), "telemetry".into()],
            brain_profile: None,
            telemetry: config(),
            ..RuntimeConfig::no_embeddings()
        },
    );
    let mut builder = VerbRegistryBuilder::new();
    builder.with_actor_id(Some("worker".into()));
    builder.register(KgPack::new(runtime.clone()));
    builder.register(TelemetryPack::new(runtime.clone()));
    let before = builder.build().unwrap();
    let stored = emit(&before, "run.started", json!({"old_policy":"durable"})).await;
    emit(&before, "turn.delta", json!({"old_policy":"ephemeral"})).await;
    let mut changed = config();
    changed.channels[0].kinds = vec!["run.completed".into()];
    changed.channels[1].kinds.push("run.started".into());
    let read_runtime = KhiveRuntime::from_backend(backend, {
        let mut config = runtime.config().clone();
        config.telemetry = changed;
        config
    });
    let mut builder = VerbRegistryBuilder::new();
    builder.with_actor_id(Some("worker".into()));
    builder.register(KgPack::new(read_runtime.clone()));
    builder.register(TelemetryPack::new(read_runtime));
    let after = builder.build().unwrap();
    let mixed = after
        .dispatch(
            "telemetry.read",
            json!({"stream":"events", "kinds":["turn.delta","run.started","run.completed"]}),
        )
        .await
        .unwrap();
    assert_eq!(
        mixed["coverage"]["ephemeral"],
        json!(["run.started", "turn.delta"])
    );
    assert_eq!(mixed["coverage"]["classification_scope"], "requested_kinds");
    assert_eq!(mixed["events"].as_array().unwrap().len(), 1);
    assert_eq!(mixed["events"][0]["id"], stored["receipt_id"]);
    let durable = after
        .dispatch(
            "telemetry.read",
            json!({"stream":"events", "kinds":["run.completed"]}),
        )
        .await
        .unwrap();
    assert_eq!(durable["coverage"]["ephemeral"], json!([]));
    assert_eq!(durable["events"], json!([]));
    let unfiltered = after
        .dispatch("telemetry.read", json!({"stream":"events"}))
        .await
        .unwrap();
    assert!(unfiltered["coverage"]["ephemeral"].is_null());
    assert_eq!(unfiltered["coverage"]["classification_scope"], "all_kinds");
    assert_eq!(
        unfiltered["coverage"]["current_policy"]["default_carrier"],
        "ephemeral"
    );
    assert_eq!(unfiltered["events"].as_array().unwrap().len(), 1);
}
