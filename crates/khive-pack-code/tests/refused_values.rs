use chrono::{TimeZone, Utc};
use khive_pack_code::{ingest_findings_json, CodeIngestError, CodeIngestOptions, CodePack};
use khive_pack_kg::KgPack;
use khive_runtime::{
    secret_gate, KhiveRuntime, Namespace, RuntimeConfig, RuntimeError, VerbRegistry,
    VerbRegistryBuilder,
};
use serde_json::{json, Value};

const ORDINARY: &str = "not-an-enum-value";
const SECRET: &str = "ghp_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
const FIELDS: [(&str, &str); 3] = [
    ("severity", "critical, high, medium, low, info"),
    ("confidence", "high, medium, low"),
    ("kind_status", "open, resolved, wontfix, invalid"),
];

fn fixture() -> (KhiveRuntime, VerbRegistry) {
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        wal_ceiling_bytes: 0,
        wal_ceiling_configured_bytes: 0,
        wal_ceiling_env_raw: None,
        disk_guard_environment: khive_db::DiskGuardEnvironment::default(),
        disk_guard_config: None,
        volume_lock_dir: None,
        credentials: vec![],
        visibility_receipts: None,
        mounts: vec![],
        events_split: None,
        actor_id: None,
        brain_profile: None,
        packs: vec!["kg".into(), "code".into()],
        ..RuntimeConfig::no_embeddings()
    })
    .expect("isolated in-memory runtime");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(runtime.clone()));
    builder.register(CodePack::new(runtime.clone()));
    let registry = builder.build().expect("KG and code registry");
    runtime.install_edge_rules(registry.all_edge_rules());
    (runtime, registry)
}

fn expected_value(value: &str) -> String {
    let masked = secret_gate::mask_secrets(value).into_owned();
    if value == SECRET {
        secret_gate::check(value).expect_err("synthetic credential must trip the gate");
        assert_ne!(masked, value, "masking assertion must not be vacuous");
        assert!(!masked.contains(value));
    } else {
        assert_eq!(masked, value, "ordinary rejected value stays useful");
    }
    masked
}

fn assert_refusal(error: RuntimeError, field: &str, value: &str, valid: &str) {
    let expected = expected_value(value);
    let display = error.to_string();
    let debug = format!("{error:?}");
    let RuntimeError::InvalidInput(message) = error else {
        panic!("expected InvalidInput, got {debug}");
    };
    assert_eq!(
        message,
        format!("invalid {field} {expected:?}; valid: {valid}")
    );
    assert!(display.contains(&message));
    if value == SECRET {
        assert!(!display.contains(SECRET));
        assert!(!debug.contains(SECRET));
    }
}

#[tokio::test]
async fn create_refusals_echo_or_mask_each_finding_enum_before_any_write() {
    let (runtime, registry) = fixture();
    let token = runtime.authorize(Namespace::local()).unwrap();
    let notes = runtime.notes(&token).unwrap();
    for (field, valid) in FIELDS {
        for value in [ORDINARY, SECRET] {
            for top_level in [false, true] {
                let mut args =
                    json!({"kind": "finding", "title": "Bounds finding", "properties": {}});
                if top_level {
                    args[field] = json!(value);
                } else {
                    args["properties"][field] = json!(value);
                }
                let error = registry
                    .dispatch("create", args)
                    .await
                    .expect_err("invalid enum");
                assert_refusal(error, field, value, valid);
                assert_eq!(
                    notes.count_notes("local", Some("finding")).await.unwrap(),
                    0
                );
            }
        }
    }
    let created = registry
        .dispatch(
            "create",
            json!({
                "kind": "finding", "title": "Valid finding",
                "properties": {"severity": "low", "confidence": "high", "kind_status": "open"}
            }),
        )
        .await
        .expect("valid finding still creates");
    assert_eq!(created["properties"]["kind_status"], "open");
    assert_eq!(
        notes.count_notes("local", Some("finding")).await.unwrap(),
        1
    );
}

#[tokio::test]
async fn update_refusals_echo_or_mask_each_enum_and_preserve_stored_note() {
    let (_, registry) = fixture();
    let created = registry
        .dispatch(
            "create",
            json!({
                "kind": "finding", "title": "Valid finding",
                "properties": {"severity": "low", "confidence": "high", "kind_status": "open"}
            }),
        )
        .await
        .unwrap();
    let id = created["id"].clone();
    let before = registry.dispatch("get", json!({"id": id})).await.unwrap();
    assert!(before["version"].is_number());
    for (field, valid) in FIELDS {
        for value in [ORDINARY, SECRET] {
            let mut args = json!({"id": id, "properties": {}});
            args["properties"][field] = json!(value);
            let error = registry
                .dispatch("update", args)
                .await
                .expect_err("invalid enum");
            assert_refusal(error, field, value, valid);
            let after = registry.dispatch("get", json!({"id": id})).await.unwrap();
            assert_eq!(after["properties"], before["properties"]);
            assert_eq!(after["version"], before["version"]);
            assert_eq!(after["content"], before["content"]);
        }
    }
    let updated = registry.dispatch("update", json!({
        "id": id, "properties": {"kind_status": "resolved", "severity": null, "confidence": null}
    })).await.expect("valid status and optional enum clearing remain supported");
    assert_eq!(updated["properties"]["kind_status"], "resolved");
    assert!(updated["properties"].get("severity").is_none());
    assert!(updated["properties"].get("confidence").is_none());
    let error = registry
        .dispatch(
            "update",
            json!({
                "id": id, "properties": {"kind_status": null}
            }),
        )
        .await
        .expect_err("mandatory status cannot be cleared");
    assert!(
        matches!(error, RuntimeError::InvalidInput(ref message) if message.starts_with("kind_status cannot be cleared;"))
    );
}

fn document() -> Value {
    json!({
        "audit": {"date": "2026-10-09", "scope": "fixture", "repo": "fixture", "branch": "main", "commit": "abc123", "standards_file": "checks.md"},
        "findings": [{"id": "one", "title": "Bounds finding", "severity": "low", "confidence": "high"}]
    })
}

fn options() -> CodeIngestOptions<'static> {
    CodeIngestOptions {
        namespace: "local",
        observed_at: Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
        source_run: Some("fixture-run"),
    }
}

#[test]
fn parser_refusals_mask_the_typed_value_and_both_error_renderings() {
    for (field, valid) in [
        ("severity", "critical | high | medium | low | info"),
        ("confidence", "high | medium | low"),
    ] {
        for value in [ORDINARY, SECRET] {
            let mut input = document();
            input["findings"][0][field] = json!(value);
            let bytes = serde_json::to_vec(&input).unwrap();
            let error = ingest_findings_json(&bytes, options()).expect_err("invalid finding enum");
            let expected = expected_value(value);
            let display = error.to_string();
            let debug = format!("{error:?}");
            match &error {
                CodeIngestError::InvalidValue {
                    field: actual_field,
                    value: actual_value,
                    valid: actual_valid,
                } => {
                    assert_eq!(*actual_field, field);
                    assert_eq!(actual_value, &expected);
                    assert_eq!(*actual_valid, valid);
                }
                other => panic!("expected InvalidValue, got {other:?}"),
            }
            assert_eq!(
                display,
                format!("invalid {field} {expected:?}; valid: {valid}")
            );
            assert!(debug.contains(&format!("value: {expected:?}")));
            if value == SECRET {
                assert!(!display.contains(SECRET));
                assert!(!debug.contains(SECRET));
            }
        }
    }
}

#[test]
fn parser_valid_control_and_late_refusal_preserve_whole_document_validation() {
    let mut input = document();
    let bytes = serde_json::to_vec(&input).unwrap();
    let batch = ingest_findings_json(&bytes, options()).expect("valid document");
    assert_eq!(batch.notes.len(), 1);
    assert_eq!(
        batch.notes[0].properties.as_ref().unwrap()["severity"],
        "low"
    );
    let mut late = input["findings"][0].clone();
    late["id"] = json!("two");
    late["confidence"] = json!(SECRET);
    input["findings"].as_array_mut().unwrap().push(late);
    let bytes = serde_json::to_vec(&input).unwrap();
    let error =
        ingest_findings_json(&bytes, options()).expect_err("no partial batch on late error");
    assert!(
        matches!(error, CodeIngestError::InvalidValue { field: "confidence", ref value, .. } if value == &expected_value(SECRET))
    );
}
