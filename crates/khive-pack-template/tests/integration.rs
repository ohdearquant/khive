//! Smoke tests for the template pack.
//!
//! Copy and adapt this file when scaffolding a new pack.

use khive_pack_template::TemplatePack;
use khive_runtime::{KhiveRuntime, VerbRegistry, VerbRegistryBuilder};
use khive_types::Pack;

fn build_registry() -> (VerbRegistry, KhiveRuntime) {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    let mut builder = VerbRegistryBuilder::new();
    builder.register(khive_pack_kg::KgPack::new(runtime.clone()));
    builder.register(TemplatePack::new(runtime.clone()));
    let registry = builder.build().expect("registry builds");
    (registry, runtime)
}

#[test]
fn template_pack_name_is_stable() {
    assert_eq!(TemplatePack::NAME, "template");
}

#[test]
fn template_pack_declares_expected_note_kind() {
    assert!(TemplatePack::NOTE_KINDS.contains(&"template_note"));
}

#[test]
fn template_pack_requires_kg() {
    assert_eq!(TemplatePack::REQUIRES, &["kg"]);
}

#[tokio::test]
async fn my_verb_returns_ok_with_valid_name() {
    let (registry, _rt) = build_registry();

    let result = registry
        .dispatch("template.my_verb", serde_json::json!({ "name": "hello" }))
        .await
        .expect("template.my_verb dispatches");

    assert_eq!(result["ok"], true);
    assert_eq!(result["name"], "hello");
}

#[tokio::test]
async fn my_verb_errors_on_missing_name() {
    let (registry, _rt) = build_registry();

    let err = registry
        .dispatch("template.my_verb", serde_json::json!({}))
        .await
        .unwrap_err();

    assert!(
        err.to_string().contains("name"),
        "error should mention the missing field; got: {err}"
    );
}

#[tokio::test]
async fn my_verb_errors_on_empty_name() {
    let (registry, _rt) = build_registry();

    let err = registry
        .dispatch("template.my_verb", serde_json::json!({ "name": "" }))
        .await
        .unwrap_err();

    assert!(
        err.to_string().contains("name"),
        "error should mention the invalid field; got: {err}"
    );
}

#[tokio::test]
async fn my_verb_help_declares_required_name_param() {
    let (registry, _rt) = build_registry();

    let result = registry
        .dispatch("template.my_verb", serde_json::json!({ "help": true }))
        .await
        .expect("template.my_verb help dispatches");

    let params = result["params"].as_array().expect("params is an array");
    assert_eq!(
        params.len(),
        1,
        "params should declare the name field; got: {params:?}"
    );
    assert_eq!(params[0]["name"], "name");
    assert_eq!(params[0]["type"], "string");
    assert_eq!(params[0]["required"], true);
    assert!(
        params[0]["description"]
            .as_str()
            .is_some_and(|s| !s.is_empty()),
        "description should be a non-empty string; got: {:?}",
        params[0]["description"]
    );
}

#[tokio::test]
async fn unknown_verb_returns_error() {
    let (registry, _rt) = build_registry();

    let err = registry
        .dispatch("no_such_verb_xyz", serde_json::Value::Null)
        .await
        .unwrap_err();

    assert!(
        err.to_string().contains("no_such_verb_xyz") || err.to_string().contains("unknown verb")
    );
}
