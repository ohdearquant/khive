use khive_runtime::{KhiveRuntime, Namespace, RuntimeError, VerbRegistryBuilder};
use serde_json::{json, Value};

fn invalid_input_reason(error: RuntimeError) -> String {
    match error {
        RuntimeError::InvalidInput(reason) => reason,
        other => panic!("expected a create validation refusal, got {other}"),
    }
}

#[tokio::test]
async fn singleton_scheduled_event_refusal_matches_create_dispatch() {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    let mut builder = VerbRegistryBuilder::new();
    builder.with_actor_id(Some("fixture-schedule-owner".into()));
    builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
    builder.register(crate::SchedulePack::new(runtime.clone()));
    let registry = builder.build().expect("real KG and schedule registry");
    let token = runtime.authorize(Namespace::local()).expect("local token");

    let ordinary = registry
        .dispatch(
            "schedule.schedule",
            json!({
                "action": "create(kind=\"note\", content=\"ordinary fixture\")",
                "at": "2099-06-01T09:00:00Z"
            }),
        )
        .await
        .expect("ordinary singleton note actions remain schedulable");
    assert_eq!(ordinary["status"], "pending");
    let baseline = runtime
        .list_notes(&token, Some("scheduled_event"), 20, 0)
        .await
        .expect("scheduled rows before refusals");
    assert_eq!(
        baseline.len(),
        1,
        "positive schedule guard must write one row"
    );

    for action in [
        "create(kind=\"scheduled_event\", content=\"fixture\")",
        "create(kind=\"note\", note_kind=\"scheduled_event\", content=\"fixture\")",
        "create(kind=\" SCHEDULED_EVENT \", content=\"fixture\")",
        "create(kind=\"note\", note_kind=\" SCHEDULED_EVENT \", content=\"fixture\")",
    ] {
        let parsed = khive_request::parse_request(action).expect("literal singleton action");
        assert_eq!(parsed.ops.len(), 1);
        let op = &parsed.ops[0];
        assert_eq!(op.tool, "create");
        // Feed exactly the action's arguments to the real create handler.
        let params = Value::Object(
            op.args
                .iter()
                .map(|(name, arg)| {
                    (
                        name.clone(),
                        arg.as_value().expect("literal argument").clone(),
                    )
                })
                .collect(),
        );
        let create_reason = invalid_input_reason(
            registry
                .dispatch("create", params)
                .await
                .expect_err("KG create must refuse scheduled_event"),
        );
        assert!(
            create_reason.contains("`created_by_actor` is a trust boundary"),
            "the create dispatch must reach its scheduled-event ownership refusal: {create_reason}"
        );
        let schedule_reason = invalid_input_reason(
            registry
                .dispatch(
                    "schedule.schedule",
                    json!({"action": action, "at": "2099-06-01T09:00:00Z"}),
                )
                .await
                .expect_err("schedule-time validation must refuse the same scheduled_event action"),
        );
        assert_eq!(schedule_reason, create_reason, "refusal drift for {action}");
        let after = runtime
            .list_notes(&token, Some("scheduled_event"), 20, 0)
            .await
            .expect("scheduled rows after refusal");
        assert_eq!(
            after.len(),
            baseline.len(),
            "refused action must not be stored"
        );
        assert_eq!(
            after[0].id, baseline[0].id,
            "existing schedule must survive"
        );
    }
}

fn singleton_alias_action(params: &Value) -> String {
    let args = params
        .as_object()
        .expect("create arguments")
        .iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join(", ");
    format!("create({args})")
}

#[tokio::test]
async fn singleton_alias_shapes_match_create_before_schedule_writes() {
    let mut config = khive_runtime::RuntimeConfig::no_embeddings();
    config.db_path = None;
    config.wal_ceiling_bytes = 0;
    config.wal_ceiling_configured_bytes = 0;
    config.wal_ceiling_source = khive_runtime::WalCeilingSource::Default;
    config.wal_ceiling_env_raw = None;
    config.disk_guard_environment = Default::default();
    config.disk_guard_config = None;
    config.volume_lock_dir = None;
    config.credentials.clear();
    config.visibility_receipts = None;
    config.mounts.clear();
    config.events_split = None;
    config.actor_id = Some("fixture-alias-owner".into());
    config.default_namespace = Namespace::local();
    config.visible_namespaces.clear();
    config.allowed_outbound_namespaces.clear();
    config.brain_profile = None;
    config.brain = Default::default();
    config.packs = vec!["kg".into(), "schedule".into()];
    assert!(config.embedding_model.is_none());
    assert!(config.additional_embedding_models.is_empty());
    let runtime = KhiveRuntime::new(config).expect("explicit memory runtime");
    assert!(!runtime.backend().is_file_backed());
    assert!(runtime.backend_data_dir().is_none());
    assert!(runtime.backend_ann_root().is_none());
    let mut builder = VerbRegistryBuilder::new();
    builder.with_actor_id(Some("fixture-alias-owner".into()));
    builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
    builder.register(crate::SchedulePack::new(runtime.clone()));
    let registry = builder.build().expect("real KG and schedule registry");
    let token = runtime.authorize(Namespace::local()).expect("local token");

    let ordinary = registry
        .dispatch(
            "schedule.schedule",
            json!({
                "action": "create(kind=\"observation\", content=\"baseline fixture\")",
                "at": "2099-06-01T09:00:00Z"
            }),
        )
        .await
        .expect("positive scheduling control");
    assert_eq!(ordinary["status"], "pending");
    let baseline = runtime
        .list_notes(&token, Some("scheduled_event"), 100, 0)
        .await
        .expect("baseline scheduled rows");
    assert_eq!(baseline.len(), 1);

    for base in [
        json!({"kind": "concept", "name": "alias fixture"}),
        json!({"kind": "observation", "content": "alias fixture"}),
    ] {
        for field in ["entity_kind", "note_kind"] {
            for malformed in [
                json!(""),
                json!(" \t"),
                json!(0),
                json!(true),
                json!([]),
                json!({}),
            ] {
                let mut params = base.clone();
                params[field] = malformed.clone();
                let action = singleton_alias_action(&params);
                let create_reason = invalid_input_reason(
                    registry
                        .dispatch("create", params)
                        .await
                        .expect_err("KG must refuse malformed aliases, even when irrelevant"),
                );
                let expected_reason = if malformed.is_string() {
                    format!("create: `{field}` must not be empty")
                } else {
                    format!("create: `{field}` must be a string or null; got {malformed}")
                };
                assert_eq!(create_reason, expected_reason, "{action}");
                let schedule_reason = invalid_input_reason(
                    registry
                        .dispatch(
                            "schedule.schedule",
                            json!({"action": action, "at": "2099-06-01T09:00:00Z"}),
                        )
                        .await
                        .expect_err("scheduling must refuse the same malformed alias"),
                );
                assert_eq!(schedule_reason, create_reason, "{action}");
                let after = runtime
                    .list_notes(&token, Some("scheduled_event"), 100, 0)
                    .await
                    .expect("scheduled rows after refusal");
                assert_eq!(
                    serde_json::to_value(&after).unwrap(),
                    serde_json::to_value(&baseline).unwrap(),
                    "no scheduled write for {action}",
                );
            }
        }
    }

    // Null and omission are absence; nonblank strings retain the canonical
    // handler's selected-kind reconciliation and irrelevant-alias behavior.
    for params in [
        json!({"kind": "concept", "name": "omitted aliases"}),
        json!({"kind": "observation", "content": "omitted aliases"}),
        json!({"kind": "concept", "name": "null aliases", "entity_kind": null, "note_kind": null}),
        json!({"kind": "observation", "content": "null aliases", "entity_kind": null, "note_kind": null}),
        json!({"kind": "concept", "name": "selected alias", "entity_kind": " CONCEPT "}),
        json!({"kind": "observation", "content": "selected alias", "note_kind": " OBSERVATION "}),
        json!({"kind": "concept", "name": "irrelevant alias", "note_kind": "not-a-note-kind"}),
        json!({"kind": "observation", "content": "irrelevant alias", "entity_kind": "not-an-entity-kind"}),
        // Singleton aliases remain irrelevant to the existing bulk early return.
        json!({"items": [{"kind": "observation", "content": "bulk fixture"}], "entity_kind": false, "note_kind": 0}),
    ] {
        let action = singleton_alias_action(&params);
        registry
            .dispatch("create", params)
            .await
            .expect("canonical create positive control");
        let scheduled = registry
            .dispatch(
                "schedule.schedule",
                json!({"action": action, "at": "2099-06-01T09:00:00Z"}),
            )
            .await
            .expect("valid aliases and bulk behavior remain schedulable");
        assert_eq!(scheduled["status"], "pending", "{action}");
    }
    let before_bad_bulk = runtime
        .list_notes(&token, Some("scheduled_event"), 100, 0)
        .await
        .expect("positive scheduled rows");
    assert_eq!(before_bad_bulk.len(), 10);
    let malformed_bulk =
        json!({"items": [{"kind": "observation", "content": "fixture", "typo": true}]});
    let action = singleton_alias_action(&malformed_bulk);
    let create_reason = invalid_input_reason(
        registry
            .dispatch("create", malformed_bulk)
            .await
            .expect_err("bad bulk field"),
    );
    assert!(create_reason.contains("unknown field"));
    let schedule_reason = invalid_input_reason(
        registry
            .dispatch(
                "schedule.schedule",
                json!({"action": action, "at": "2099-06-01T09:00:00Z"}),
            )
            .await
            .expect_err("existing bulk validation remains active"),
    );
    assert!(schedule_reason.contains("unknown field"));
    assert_eq!(
        serde_json::to_value(
            runtime
                .list_notes(&token, Some("scheduled_event"), 100, 0)
                .await
                .unwrap()
        )
        .unwrap(),
        serde_json::to_value(&before_bad_bulk).unwrap(),
    );
}
