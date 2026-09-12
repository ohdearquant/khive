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
    // Split the header as in the secret-gate unit fixtures.
    let synthetic_header = ["-----BEGIN RSA", " PRIVATE KEY-----"].concat(); // gitleaks:allow
    let incident = emit(
        &registry,
        "run.started",
        json!({"synthetic_secret_shape":synthetic_header}),
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

#[tokio::test]
async fn arbitrary_stream_rollup_groups_and_excludes_existing_rows_at_both_boundaries() {
    let (registry, runtime) = registry_with_scope(config(), "worker", &[], &["worker"]);
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
                "stream":"other-stream", "all_actors":true,
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
                "stream":"other-stream", "all_actors":true,
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
                "stream":"other-stream", "all_actors":true, "kinds":["absent"],
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
        .map(|_| json!({"op":"append", "stream":"paged", "record":{"kind":"first","actor":"worker"}, "embed":false}))
        .collect();
    registry
        .dispatch("stream.batch", json!({"ops":ops,"atomic":true}))
        .await
        .expect("seed first full page");
    registry
        .dispatch(
            "stream.append",
            json!({"stream":"paged", "record":{"kind":"last","actor":"worker"}, "embed":false}),
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

#[tokio::test]
async fn classification_and_caller_attribution_are_visible_in_every_emit() {
    let (registry, _) = registry(config());
    let mut recorded = Vec::new();
    for actor in [None, Some("worker"), Some("forged-worker")] {
        let mut params = json!({"kind":"run.started", "payload":null});
        if let Some(actor) = actor {
            params["actor"] = actor.into();
        }
        let result = registry.dispatch("telemetry.emit", params).await.unwrap();
        assert_eq!(result["actor"], "actor:worker");
        assert_eq!(result["classified"], true);
        assert_eq!(result["outcome"], "recorded");
        assert!(result.get("error").is_none());
        assert!(result.get("dropped").is_none());
        if actor == Some("forged-worker") {
            assert_eq!(result["actor_argument_ignored"], true);
        } else {
            assert!(result.get("actor_argument_ignored").is_none());
        }
        recorded.push(result);
    }
    let page = existing_read(&registry, "events").await;
    for (entry, emitted) in page["entries"].as_array().unwrap().iter().zip(recorded) {
        assert_eq!(entry["id"], emitted["receipt_id"]);
        assert_eq!(entry["seq"], emitted["seq"]);
        assert_eq!(entry["record"]["actor"], "actor:worker");
    }
    let classified = emit(&registry, "turn.delta", Value::Null).await;
    let fallback = emit(&registry, "new.kind", Value::Null).await;
    assert_eq!(classified["classified"], true);
    assert_eq!(fallback["classified"], false);
    for result in [classified, fallback] {
        assert_eq!(result["carrier"], "ephemeral");
        assert_eq!(result["outcome"], "dropped");
        assert!(result["receipt_id"].is_null());
        assert!(result.get("seq").is_none());
        assert!(result.get("error").is_none());
    }
    assert_eq!(
        existing_read(&registry, "events").await["entries"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    let counts = registry.dispatch("telemetry.counts", json!({"stream":"events", "window":{"since":"2000-01-01T00:00:00Z"}, "group_by":["actor"]})).await.unwrap();
    assert_eq!(
        counts["rows"],
        json!([{"key":{"actor":"actor:worker"},"count":3}])
    );
}

#[tokio::test]
async fn server_owned_actor_stamp_cannot_be_sourced_from_arguments_or_payload() {
    let (registry, _) = registry(config());
    for assertion in ["worker", "actor:worker", "foreign-worker"] {
        let payload = json!({"actor":"foreign-worker", "kind":"foreign.kind"});
        let result = registry
            .dispatch(
                "telemetry.emit",
                json!({
                    "kind":"run.started", "actor":assertion, "payload":payload,
                }),
            )
            .await
            .unwrap();
        assert_eq!(result["actor"], "actor:worker");
        assert_eq!(
            result
                .get("actor_argument_ignored")
                .and_then(Value::as_bool),
            (assertion == "foreign-worker").then_some(true)
        );
    }
    let page = existing_read(&registry, "events").await;
    let entries = page["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 3);
    for entry in entries {
        assert_eq!(entry["record"]["actor"], "actor:worker");
        assert_eq!(entry["record"]["kind"], "run.started");
        assert_eq!(entry["record"]["payload"]["actor"], "foreign-worker");
    }
    let counts = registry
        .dispatch(
            "telemetry.counts",
            json!({
                "stream":"events", "window":{"since":"2000-01-01T00:00:00Z"}, "group_by":["actor"],
            }),
        )
        .await
        .unwrap();
    assert_eq!(
        counts["rows"],
        json!([{"key":{"actor":"actor:worker"}, "count":3}])
    );
}

#[tokio::test]
async fn read_and_counts_scope_to_caller_and_gate_foreign_and_all_actor_requests() {
    for fleet in [false, true] {
        let readers: &[&str] = if fleet { &["worker"] } else { &[] };
        let (registry, runtime) =
            registry_with_scope(config(), "worker", &["visible-peer"], readers);
        for actor in [
            "actor:worker",
            "worker",
            "actor:visible-peer",
            "actor:hidden-peer",
        ] {
            append_at(
                &registry,
                &runtime,
                json!({"kind":"run.started", "actor":actor}),
                "2026-01-02T00:00:00Z",
            )
            .await;
        }
        for verb in ["telemetry.read", "telemetry.counts"] {
            let base = json!({"stream":"other-stream"});
            let mut params = base;
            if verb == "telemetry.counts" {
                params["window"] = json!({"since":"2000-01-01T00:00:00Z"});
            }
            let result = registry.dispatch(verb, params.clone()).await.unwrap();
            let count = |v: &Value| {
                if verb == "telemetry.read" {
                    v["events"].as_array().unwrap().len() as u64
                } else {
                    v["total"].as_u64().unwrap()
                }
            };
            assert_eq!(
                count(&result),
                2,
                "{verb} default must include only caller rows"
            );
            params["actor"] = "visible-peer".into();
            assert_eq!(
                count(&registry.dispatch(verb, params.clone()).await.unwrap()),
                1
            );
            params["actor"] = "hidden-peer".into();
            assert!(registry
                .dispatch(verb, params.clone())
                .await
                .unwrap_err()
                .to_string()
                .contains("not visible"));
            params.as_object_mut().unwrap().remove("actor");
            params["all_actors"] = true.into();
            let all = registry.dispatch(verb, params.clone()).await;
            if fleet {
                assert_eq!(count(&all.unwrap()), 4);
            } else {
                assert!(all.unwrap_err().to_string().contains("fleet reader"));
            }
            params["actor"] = "worker".into();
            assert!(registry
                .dispatch(verb, params)
                .await
                .unwrap_err()
                .to_string()
                .contains("cannot be combined"));
        }
    }
}

#[tokio::test]
async fn explicit_stamped_actor_is_not_coalesced_with_another_principal() {
    let (registry, runtime) = registry_with_scope(config(), "actor:peer", &["peer"], &[]);
    for actor in ["actor:actor:peer", "actor:peer", "peer"] {
        append_at(
            &registry,
            &runtime,
            json!({"kind":"run.started", "actor":actor}),
            "2026-01-02T00:00:00Z",
        )
        .await;
    }
    let own = registry
        .dispatch("telemetry.read", json!({"stream":"other-stream"}))
        .await
        .unwrap();
    assert_eq!(own["events"].as_array().unwrap().len(), 1);
    assert_eq!(own["events"][0]["record"]["actor"], "actor:actor:peer");
    let explicit = registry
        .dispatch(
            "telemetry.read",
            json!({"stream":"other-stream", "actor":"actor:peer"}),
        )
        .await
        .unwrap();
    assert_eq!(explicit["events"].as_array().unwrap().len(), 1);
    assert_eq!(explicit["events"][0]["record"]["actor"], "actor:peer");
}

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
