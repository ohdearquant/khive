//! Deadline validation preserves ordinary diagnostics while masking recognized credentials.

use khive_pack_gtd::GtdPack;
use khive_pack_kg::KgPack;
use khive_runtime::{
    runtime_error_value, DomainDisposition, KhiveRuntime, RuntimeError, VerbRegistry,
    VerbRegistryBuilder,
};
use serde_json::{json, Value};

fn fixture() -> VerbRegistry {
    let runtime = KhiveRuntime::memory().expect("private memory runtime");
    assert!(runtime.backend().pool().canonical_path().is_none());
    assert!(runtime.default_embedder_name().is_empty());
    let mut builder = VerbRegistryBuilder::new();
    builder.with_actor_id(Some("gtd-deadline-refusal-fixture".into()));
    builder.with_default_namespace("local");
    builder.register(KgPack::new(runtime.clone()));
    builder.register(GtdPack::new(runtime.clone()));
    let registry = builder.build().unwrap();
    registry
        .apply_schema_plans_with_map(&Default::default(), runtime.backend())
        .expect("task schema");
    runtime.install_edge_rules(registry.all_edge_rules());
    registry
}

fn create_args(verb: &str, title: &str, fields: Value) -> Value {
    if verb == "gtd.assign" {
        let mut args = fields;
        args["title"] = json!(title);
        args
    } else {
        json!({"kind": "task", "name": title, "content": title, "properties": fields})
    }
}

fn credential() -> String {
    format!("sk-proj-{}", "A".repeat(80))
}

async fn tasks(registry: &VerbRegistry) -> Value {
    registry.dispatch("gtd.tasks", json!({})).await.unwrap()
}

async fn seed(registry: &VerbRegistry) -> Value {
    registry
        .dispatch(
            "gtd.assign",
            json!({
                "title": "Existing deadline", "description": "Original task body",
                "due": "2026-06-01", "timezone": "UTC", "priority": "p2"
            }),
        )
        .await
        .unwrap()["full_id"]
        .clone()
}

async fn refuse(
    registry: &VerbRegistry,
    verb: &str,
    args: Value,
    expected: String,
    secret: Option<&str>,
) {
    let before = tasks(registry).await;
    assert!(
        !before.as_array().unwrap().is_empty(),
        "populated task fixture"
    );
    let error = registry
        .dispatch(verb, args)
        .await
        .expect_err("deadline refusal");
    let RuntimeError::InvalidInput(message) = &error else {
        panic!("{verb} changed refusal kind: {error:?}");
    };
    if let Some(secret) = secret {
        assert!(
            !message.contains(secret),
            "deadline refusal exposed the synthetic credential"
        );
        assert!(message.contains("***MASKED***"), "credential mask missing");
    }
    assert_eq!(message, &expected, "{verb} deadline diagnostic");
    assert_eq!(
        runtime_error_value(error, DomainDisposition::NotCommitted),
        json!({
            "kind": "runtime_error",
            "message": format!("invalid input: {expected}"),
            "domain_disposition": "not_committed"
        })
    );
    assert_eq!(
        tasks(registry).await,
        before,
        "refused request changed task rows"
    );
}

#[tokio::test]
async fn assign_and_create_due_refusals_mask_credentials_without_writes() {
    let registry = fixture();
    seed(&registry).await;
    let secret = credential();
    for verb in ["gtd.assign", "create"] {
        for (raw, shown, masked) in [
            ("not-a-date", "not-a-date", false),
            (&secret, "***MASKED***", true),
        ] {
            refuse(
                &registry, verb, create_args(verb, "Refused task", json!({"due": raw, "timezone": "UTC"})),
                format!("due must be ISO-8601 (e.g., 2026-06-01T00:00:00Z or 2026-06-01); got {shown:?}"),
                masked.then_some(secret.as_str()),
            ).await;
        }
        refuse(
            &registry, verb, create_args(verb, "Skipped date", json!({"due": "2011-12-30", "timezone": "Pacific/Apia"})),
            "due date 2011-12-30 does not exist in the configured display timezone Pacific/Apia (no representable instant carries that local date); got \"2011-12-30\"".into(),
            None,
        ).await;
    }
}

#[tokio::test]
async fn assign_and_create_timezone_refusals_mask_credentials_before_due() {
    let registry = fixture();
    seed(&registry).await;
    let secret = credential();
    for verb in ["gtd.assign", "create"] {
        for (raw, shown, masked) in [
            ("Mars/Olympus", "Mars/Olympus", false),
            (&secret, "***MASKED***", true),
        ] {
            // Both are invalid: zone validation must still win over due parsing.
            refuse(
                &registry,
                verb,
                create_args(
                    verb,
                    "Refused zone",
                    json!({"due": "not-a-date", "timezone": raw}),
                ),
                format!(
                    "timezone must be an IANA zone name (e.g. \"America/New_York\"); got {shown:?}"
                ),
                masked.then_some(secret.as_str()),
            )
            .await;
        }
    }
}

#[tokio::test]
async fn task_update_due_refusals_mask_credentials_and_preserve_the_snapshot() {
    let registry = fixture();
    let id = seed(&registry).await;
    let secret = credential();
    for (raw, shown, masked) in [
        ("not-a-date", "not-a-date", false),
        (&secret, "***MASKED***", true),
    ] {
        refuse(
            &registry,
            "update",
            json!({"id": id, "properties": {"due": raw}}),
            format!(
                "due must be ISO-8601 (e.g., 2026-06-01T00:00:00Z or 2026-06-01); got {shown:?}"
            ),
            masked.then_some(secret.as_str()),
        )
        .await;
    }
    registry
        .dispatch(
            "update",
            json!({"id": id, "properties": {
                "due": "2026-07-01", "timezone": "Asia/Tokyo"
            }}),
        )
        .await
        .unwrap();
    let rows = tasks(&registry).await;
    assert_eq!(rows[0]["due"], "2026-07-01T00:00:00+09:00");
    assert_eq!(rows[0]["due_timezone"], "Asia/Tokyo");
    assert_eq!(rows[0]["full_id"], id);
    assert_eq!(rows[0]["status"], "inbox");
    assert_eq!(rows[0]["priority"], "p2");
    registry
        .dispatch("update", json!({"id": id, "properties": {"due": null}}))
        .await
        .unwrap();
    let rows = tasks(&registry).await;
    assert!(rows[0]["due"].is_null());
    assert!(rows[0]["due_timezone"].is_null());
}

#[tokio::test]
async fn valid_deadlines_keep_normalization_aliases_and_unused_timezone_behavior() {
    let registry = fixture();
    for verb in ["gtd.assign", "create"] {
        for (index, fields, due, zone) in [
            (
                0,
                json!({"due": "2026-06-01", "timezone": "America/New_York"}),
                json!("2026-06-01T00:00:00-04:00"),
                json!("America/New_York"),
            ),
            (
                1,
                json!({"due": "2026-06-01T12:00:00+02:00", "timezone": "Asia/Tokyo"}),
                json!("2026-06-01T10:00:00+00:00"),
                json!("Asia/Tokyo"),
            ),
            (
                2,
                json!({"timezone": "unused-invalid-zone"}),
                Value::Null,
                Value::Null,
            ),
            (
                3,
                json!({"due": null, "timezone": "unused-invalid-zone"}),
                Value::Null,
                Value::Null,
            ),
        ] {
            let title = format!("{verb} accepted deadline {index}");
            let mut args = create_args(verb, &title, fields);
            if verb == "gtd.assign" {
                args["status"] = json!("in_progress");
                args["priority"] = json!("P1");
            } else {
                args["properties"]["status"] = json!("in_progress");
                args["properties"]["priority"] = json!("P1");
            }
            registry.dispatch(verb, args).await.unwrap();
            let rows = tasks(&registry).await;
            let row = rows
                .as_array()
                .unwrap()
                .iter()
                .find(|r| r["title"] == title)
                .unwrap();
            assert_eq!(row["due"], due);
            assert_eq!(row["due_timezone"], zone);
            assert_eq!(row["status"], "active");
            assert_eq!(row["priority"], "p1");
        }
    }
    assert_eq!(tasks(&registry).await.as_array().unwrap().len(), 8);
}

#[tokio::test]
async fn task_update_timezone_strings_mask_credentials_and_preserve_the_snapshot() {
    let registry = fixture();
    let id = seed(&registry).await;
    let before = registry.dispatch("get", json!({"id": id})).await.unwrap();
    let secret = credential();
    for (field, other) in [("timezone", "due_timezone"), ("due_timezone", "timezone")] {
        for (raw, shown, masked) in [
            ("Mars/Olympus", "Mars/Olympus", false),
            (secret.as_str(), "***MASKED***", true),
        ] {
            // A valid companion alias must not hide either invalid spelling;
            // zone refusal must still precede parsing this invalid due value.
            let mut properties = json!({"due": "not-a-date"});
            properties[field] = json!(raw);
            properties[other] = json!("UTC");
            refuse(
                &registry,
                "update",
                json!({"id": id, "properties": properties}),
                format!(
                    "timezone must be an IANA zone name (e.g. \"America/New_York\"); got {shown:?}"
                ),
                masked.then_some(secret.as_str()),
            )
            .await;
            assert_eq!(
                registry.dispatch("get", json!({"id": id})).await.unwrap(),
                before
            );
        }
    }
}

#[tokio::test]
async fn task_update_timezone_types_mask_nested_credentials_and_preserve_the_snapshot() {
    use khive_runtime::secret_gate::{mask_for_redaction_surface, RedactionSurface};

    let registry = fixture();
    let id = seed(&registry).await;
    let before = registry.dispatch("get", json!({"id": id})).await.unwrap();
    let secret = credential();
    for (field, other) in [("timezone", "due_timezone"), ("due_timezone", "timezone")] {
        for (value, masked) in [
            (json!(17), false),
            (json!(["Mars/Olympus"]), false),
            (json!([{"credential": secret}]), true),
        ] {
            let serialized = value.to_string();
            let shown = mask_for_redaction_surface(RedactionSurface::GateProbe, &serialized);
            if masked {
                assert!(!shown.contains(&secret), "synthetic fixture must be masked");
            } else {
                assert_eq!(shown, serialized, "ordinary JSON operand must be retained");
            }
            let mut properties = json!({"due": "not-a-date"});
            properties[field] = value;
            properties[other] = json!("UTC");
            refuse(
                &registry,
                "update",
                json!({"id": id, "properties": properties}),
                format!(
                    "{field} must be an IANA zone name string (e.g. \"America/New_York\"); got {shown}"
                ),
                masked.then_some(secret.as_str()),
            )
            .await;
            assert_eq!(
                registry.dispatch("get", json!({"id": id})).await.unwrap(),
                before
            );
        }
    }
}

fn json_refusal_fixture() -> VerbRegistry {
    let runtime = KhiveRuntime::new(khive_runtime::RuntimeConfig {
        db_path: None,
        events_split: None,
        actor_id: Some("gtd-json-refusal-fixture".into()),
        packs: vec!["kg".into()],
        ..khive_runtime::RuntimeConfig::no_embeddings()
    })
    .expect("private memory runtime for JSON refusals");
    assert!(runtime.backend().pool().canonical_path().is_none());
    assert!(runtime.backend_data_dir().is_none());
    assert!(runtime.backend_ann_root().is_none());
    assert!(runtime.default_embedder_name().is_empty());
    let mut builder = VerbRegistryBuilder::new();
    builder.with_actor_id(Some("gtd-json-refusal-fixture".into()));
    builder.with_default_namespace("local");
    builder.register(KgPack::new(runtime.clone()));
    builder.register(GtdPack::new(runtime.clone()));
    let registry = builder.build().expect("JSON refusal registry");
    registry
        .apply_schema_plans_with_map(&Default::default(), runtime.backend())
        .expect("task schema");
    runtime.install_edge_rules(registry.all_edge_rules());
    registry
}

fn json_operand_shown(value: &Value, secret: Option<&str>) -> String {
    use khive_runtime::secret_gate::{mask_for_redaction_surface, RedactionSurface};
    let serialized = value.to_string();
    let shown = mask_for_redaction_surface(RedactionSurface::GateProbe, &serialized);
    if let Some(secret) = secret {
        assert!(
            serialized.contains(secret),
            "raw JSON fixture must contain the synthetic credential"
        );
        assert!(
            !shown.contains(secret),
            "existing masker must recognize the synthetic fixture"
        );
        assert!(
            shown.contains("***MASKED***"),
            "existing canonical mask marker"
        );
    } else {
        assert_eq!(
            shown, serialized,
            "ordinary JSON diagnostics must be retained"
        );
    }
    shown.into_owned()
}

async fn refuse_json_update(
    registry: &VerbRegistry,
    id: &Value,
    properties: Value,
    expected: String,
    secret: Option<&str>,
) {
    assert!(
        id.as_str().is_some_and(|id| !id.is_empty()),
        "actual seeded task identifier"
    );
    let before = registry
        .dispatch("get", json!({"id": id}))
        .await
        .expect("actual stored task before refusal");
    assert!(before.is_object(), "actual populated task readback");
    refuse(
        registry,
        "update",
        json!({"id": id, "properties": properties}),
        expected,
        secret,
    )
    .await;
    assert_eq!(
        registry.dispatch("get", json!({"id": id})).await.unwrap(),
        before,
        "refused JSON update changed the complete task snapshot"
    );
}

#[tokio::test]
async fn task_update_due_json_refusals_mask_credentials_and_preserve_snapshot() {
    let registry = json_refusal_fixture();
    let id = seed(&registry).await;
    let secret = credential();
    for (value, masked) in [
        (json!(17), false),
        (json!({"label": "ordinary"}), false),
        (json!({"credential": secret.clone()}), true),
        (json!([{"credential": secret.clone()}]), true),
    ] {
        let secret = masked.then_some(secret.as_str());
        let shown = json_operand_shown(&value, secret);
        refuse_json_update(
            &registry,
            &id,
            json!({"due": value}),
            format!("due must be an ISO-8601 string or null; got {shown}"),
            secret,
        )
        .await;
    }
    registry
        .dispatch(
            "update",
            json!({"id": id, "properties": {"due": "2026-07-01", "timezone": "UTC"}}),
        )
        .await
        .expect("valid due update still succeeds");
    assert_eq!(
        tasks(&registry).await[0]["due"],
        "2026-07-01T00:00:00+00:00"
    );
    registry
        .dispatch("update", json!({"id": id, "properties": {"due": null}}))
        .await
        .expect("due clear still succeeds");
    let rows = tasks(&registry).await;
    assert!(rows[0]["due"].is_null());
    assert!(rows[0]["due_timezone"].is_null());
}

#[tokio::test]
async fn create_task_json_string_field_refusals_mask_credentials_without_writes() {
    let registry = json_refusal_fixture();
    seed(&registry).await;
    registry
        .dispatch(
            "create",
            json!({"kind": "task", "name": "Healthy top-level fields", "content": "Safe task body",
        "due": "2026-06-01", "priority": "P1", "properties": {"timezone": "UTC"}}),
        )
        .await
        .expect("actual create accepts top-level due and priority with nested timezone");
    assert_eq!(tasks(&registry).await.as_array().unwrap().len(), 2);
    let secret = credential();
    // Generic create exposes due at the top level and in properties;
    // timezone is supported only through properties at this public boundary.
    for (field, nested) in [("due", false), ("due", true), ("timezone", true)] {
        for (value, masked) in [
            (json!({"label": "ordinary"}), false),
            (json!(["ordinary"]), false),
            (json!({"credential": secret.clone()}), true),
            (json!([{"credential": secret.clone()}]), true),
        ] {
            let secret = masked.then_some(secret.as_str());
            let shown = json_operand_shown(&value, secret);
            let mut args = json!({"kind": "task", "name": "Refused JSON task", "content": "Safe task body", "due": "2026-06-01", "properties": {}});
            let display_name = if nested {
                // Parsing is eager: the valid winning top-level due must
                // not hide a malformed properties.due value.
                args["properties"][field] = value;
                format!("properties.{field}")
            } else {
                args[field] = value;
                field.to_owned()
            };
            refuse(
                &registry,
                "create",
                args,
                format!("{display_name} must be a string or null; got {shown}"),
                secret,
            )
            .await;
        }
    }
}

#[tokio::test]
async fn task_update_priority_json_refusals_mask_credentials_and_preserve_snapshot() {
    let registry = json_refusal_fixture();
    let id = seed(&registry).await;
    registry
        .dispatch(
            "update",
            json!({"id": id, "properties": {"priority": "P1"}}),
        )
        .await
        .expect("valid priority update still succeeds");
    assert_eq!(tasks(&registry).await[0]["priority"], "p1");
    let secret = credential();
    for (value, masked) in [
        (json!(17), false),
        (json!(["p1"]), false),
        (json!({"credential": secret.clone()}), true),
        (json!([{"credential": secret.clone()}]), true),
    ] {
        let secret = masked.then_some(secret.as_str());
        let shown = json_operand_shown(&value, secret);
        refuse_json_update(
            &registry,
            &id,
            json!({"priority": value}),
            format!("priority must be one of p0, p1, p2, p3, or null; got {shown}"),
            secret,
        )
        .await;
    }
}

#[tokio::test]
async fn task_update_priority_string_refusals_mask_credentials_and_preserve_snapshot() {
    let registry = json_refusal_fixture();
    let id = seed(&registry).await;
    let secret = credential();
    for (raw, shown, masked) in [
        ("bogus", "bogus", false),
        (secret.as_str(), "***MASKED***", true),
    ] {
        refuse_json_update(
            &registry,
            &id,
            json!({"priority": raw}),
            format!("invalid priority {shown:?} — valid: p0, p1, p2, p3"),
            masked.then_some(secret.as_str()),
        )
        .await;
    }
    registry
        .dispatch(
            "update",
            json!({"id": id, "properties": {"priority": "P1"}}),
        )
        .await
        .expect("valid priority string still succeeds");
    assert_eq!(tasks(&registry).await[0]["priority"], "p1");
}

#[tokio::test]
async fn assign_json_due_timezone_refusals_preserve_masked_decoder_contract() {
    let registry = json_refusal_fixture();
    let id = seed(&registry).await;
    let before = tasks(&registry).await;
    assert_eq!(
        before.as_array().unwrap().len(),
        1,
        "healthy assign populated the actual store"
    );
    let task_before = registry.dispatch("get", json!({"id": id})).await.unwrap();
    let secret = credential();
    for field in ["due", "timezone"] {
        for value in [
            json!({"credential": secret.clone()}),
            json!([{"credential": secret.clone()}]),
        ] {
            json_operand_shown(&value, Some(secret.as_str()));
            // Assign has typed string fields and no properties convention.
            // Its already-masked decoder refuses before task-create input.
            let mut args = json!({"title": "Refused assign JSON", "due": "2026-06-01"});
            args[field] = value;
            let error = registry
                .dispatch("gtd.assign", args)
                .await
                .expect_err("typed assign refusal");
            let RuntimeError::InvalidInput(message) = error else {
                panic!("assign changed refusal kind: {error:?}");
            };
            assert!(
                message.starts_with("bad params: "),
                "assign must reach its existing decoder: {message}"
            );
            assert!(
                message.contains("expected a string"),
                "useful existing type diagnostic: {message}"
            );
            assert!(
                !message.contains(&secret),
                "assign decoder exposed the synthetic credential"
            );
            assert_eq!(
                tasks(&registry).await,
                before,
                "refused assign created or changed a task"
            );
            assert_eq!(
                registry.dispatch("get", json!({"id": id})).await.unwrap(),
                task_before
            );
        }
    }
}

async fn refuse_task_shape_with_snapshots(
    registry: &VerbRegistry,
    verb: &str,
    args: Value,
    expected: String,
    secret: Option<&str>,
    retained_ids: &[Value],
) {
    assert!(!retained_ids.is_empty(), "populated retained task fixture");
    let mut snapshots = Vec::new();
    for id in retained_ids {
        assert!(
            id.as_str().is_some_and(|id| !id.is_empty()),
            "actual retained task identifier"
        );
        let snapshot = registry
            .dispatch("get", json!({"id": id}))
            .await
            .expect("complete retained task before refusal");
        assert!(snapshot.is_object(), "actual retained task readback");
        snapshots.push((id.clone(), snapshot));
    }
    // The existing helper checks the actual InvalidInput before projection and
    // compares the complete populated gtd.tasks census after every refusal.
    refuse(registry, verb, args, expected, secret).await;
    for (id, before) in snapshots {
        assert_eq!(
            registry.dispatch("get", json!({"id": id})).await.unwrap(),
            before,
            "refused shape changed a complete retained task snapshot"
        );
    }
}

#[tokio::test]
async fn task_update_name_type_refusals_mask_credentials_and_preserve_snapshot() {
    let registry = json_refusal_fixture();
    let id = seed(&registry).await;
    let other_id = seed(&registry).await;
    assert_ne!(id, other_id, "two distinct actual task rows");
    registry
        .dispatch("update", json!({"id": id, "name": "Accepted task title"}))
        .await
        .expect("valid task name update");
    let stored = registry.dispatch("get", json!({"id": id})).await.unwrap();
    assert_eq!(stored["name"], "Accepted task title");
    assert_eq!(stored["content"], "Original task body");
    assert_eq!(tasks(&registry).await.as_array().unwrap().len(), 2);
    let retained_ids = [id.clone(), other_id];
    let secret = credential();
    for (value, masked) in [
        (json!(17), false),
        (json!({"label": "ordinary"}), false),
        (json!(["ordinary"]), false),
        (json!({"credential": secret.clone()}), true),
        (json!([{"credential": secret.clone()}]), true),
    ] {
        let secret = masked.then_some(secret.as_str());
        let shown = json_operand_shown(&value, secret);
        refuse_task_shape_with_snapshots(
            &registry,
            "update",
            json!({"id": id, "name": value}),
            format!("task update field `name` must be a string; got {shown}"),
            secret,
            &retained_ids,
        )
        .await;
    }
}

#[tokio::test]
async fn task_update_description_type_refusals_mask_credentials_and_preserve_snapshot() {
    let registry = json_refusal_fixture();
    let id = seed(&registry).await;
    let other_id = seed(&registry).await;
    assert_ne!(id, other_id, "two distinct actual task rows");
    registry
        .dispatch(
            "update",
            json!({"id": id, "properties": {"description": "Accepted task description"}}),
        )
        .await
        .expect("valid task description update");
    let stored = registry.dispatch("get", json!({"id": id})).await.unwrap();
    assert_eq!(stored["content"], "Accepted task description");
    assert_eq!(
        stored["properties"]["description"],
        "Accepted task description"
    );
    assert_eq!(tasks(&registry).await.as_array().unwrap().len(), 2);
    let retained_ids = [id.clone(), other_id];
    let secret = credential();
    for (value, masked) in [
        (json!(17), false),
        (json!({"label": "ordinary"}), false),
        (json!(["ordinary"]), false),
        (json!({"credential": secret.clone()}), true),
        (json!([{"credential": secret.clone()}]), true),
    ] {
        let secret = masked.then_some(secret.as_str());
        let shown = json_operand_shown(&value, secret);
        refuse_task_shape_with_snapshots(
            &registry,
            "update",
            json!({"id": id, "properties": {"description": value}}),
            format!("properties.description must be a string or null; got {shown}"),
            secret,
            &retained_ids,
        )
        .await;
    }
}

#[tokio::test]
async fn task_update_properties_type_refusals_mask_credentials_and_preserve_snapshot() {
    let registry = json_refusal_fixture();
    let id = seed(&registry).await;
    let other_id = seed(&registry).await;
    assert_ne!(id, other_id, "two distinct actual task rows");
    registry
        .dispatch(
            "update",
            json!({"id": id, "properties": {"description": "Accepted object properties"}}),
        )
        .await
        .expect("object properties remains a valid task update");
    let stored = registry.dispatch("get", json!({"id": id})).await.unwrap();
    assert_eq!(stored["content"], "Accepted object properties");
    assert_eq!(
        stored["properties"]["description"],
        "Accepted object properties"
    );
    assert_eq!(tasks(&registry).await.as_array().unwrap().len(), 2);
    let retained_ids = [id.clone(), other_id];
    let secret = credential();
    for (value, masked) in [
        (json!(17), false),
        (json!(["ordinary"]), false),
        (json!("ordinary"), false),
        (json!([{"credential": secret.clone()}]), true),
        (json!(secret.clone()), true),
    ] {
        let secret = masked.then_some(secret.as_str());
        let shown = json_operand_shown(&value, secret);
        refuse_task_shape_with_snapshots(
            &registry,
            "update",
            json!({"id": id, "properties": value}),
            format!("properties on a `task` note must be patched with an object; got {shown}"),
            secret,
            &retained_ids,
        )
        .await;
    }
}

#[tokio::test]
async fn create_task_properties_type_refusals_mask_credentials_without_writes() {
    let registry = json_refusal_fixture();
    let id = seed(&registry).await;
    registry
        .dispatch(
            "create",
            json!({"kind": "task", "name": "Accepted object task", "content": "Accepted task body",
                "properties": {"description": "Accepted task body"}}),
        )
        .await
        .expect("object properties remains valid for actual task create");
    let rows = tasks(&registry).await;
    assert_eq!(rows.as_array().unwrap().len(), 2);
    let created_id = rows
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["title"] == "Accepted object task")
        .expect("actual newly created task in populated census")["full_id"]
        .clone();
    assert_ne!(id, created_id, "healthy create wrote a distinct task");
    let stored = registry
        .dispatch("get", json!({"id": created_id}))
        .await
        .unwrap();
    assert_eq!(stored["name"], "Accepted object task");
    assert_eq!(stored["content"], "Accepted task body");
    assert_eq!(stored["properties"]["description"], "Accepted task body");
    let retained_ids = [id, created_id];
    let secret = credential();
    for (value, masked) in [
        (json!(17), false),
        (json!(["ordinary"]), false),
        (json!("ordinary"), false),
        (json!([{"credential": secret.clone()}]), true),
        (json!(secret.clone()), true),
    ] {
        let secret = masked.then_some(secret.as_str());
        let shown = json_operand_shown(&value, secret);
        refuse_task_shape_with_snapshots(
            &registry,
            "create",
            json!({"kind": "task", "name": "Refused object task", "content": "Safe task body", "properties": value}),
            format!("properties must be an object or null; got {shown}"),
            secret,
            &retained_ids,
        )
        .await;
    }
}
