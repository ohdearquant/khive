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
