//! Integration tests for the workspace pack (issue #873 v0): entity-kind
//! registration, `REQUIRES`, the five `contains` endpoint rules (positive +
//! negative), and `name`/`schema_version` validation on create.

use khive_pack_gtd::GtdPack;
use khive_pack_kg::KgPack;
use khive_pack_session::SessionPack;
use khive_pack_workspace::WorkspacePack;
use khive_runtime::{KhiveRuntime, VerbRegistry, VerbRegistryBuilder};
use khive_types::Pack;
use serde_json::json;
use uuid::Uuid;

fn rt() -> KhiveRuntime {
    KhiveRuntime::memory().expect("memory runtime")
}

fn build_registry(rt: KhiveRuntime) -> VerbRegistry {
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    builder.register(GtdPack::new(rt.clone()));
    builder.register(SessionPack::new(rt.clone()));
    builder.register(WorkspacePack::new(rt.clone()));
    let registry = builder.build().expect("registry builds");
    rt.install_edge_rules(registry.all_edge_rules());
    registry
}

async fn create_workspace(registry: &VerbRegistry, name: &str) -> String {
    let resp = registry
        .dispatch(
            "create",
            json!({"kind": "workspace", "name": name, "properties": {"schema_version": 1}}),
        )
        .await
        .expect("workspace create ok");
    resp["id"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn workspace_entity_kind_registers() {
    let registry = build_registry(rt());
    assert!(registry.all_entity_kinds().contains(&"workspace"));
}

#[test]
fn workspace_pack_requires_three_packs() {
    assert_eq!(
        WorkspacePack::REQUIRES,
        &["kg", "gtd", "session"],
        "REQUIRES must list all three hard v0 dependencies bundled in this distribution"
    );
}

#[test]
fn workspace_pack_declares_no_new_verbs() {
    assert!(
        WorkspacePack::HANDLERS.is_empty(),
        "v0 exposes no convenience verbs  -  create/link only"
    );
}

#[tokio::test]
async fn create_workspace_succeeds_with_name_and_schema_version() {
    let registry = build_registry(rt());
    let id = create_workspace(&registry, "sprint-42").await;
    assert!(Uuid::parse_str(&id).is_ok());
}

#[tokio::test]
async fn create_workspace_rejects_missing_schema_version() {
    let registry = build_registry(rt());
    let err = registry
        .dispatch("create", json!({"kind": "workspace", "name": "no-schema"}))
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("schema_version"),
        "error should mention schema_version; got: {err}"
    );
}

#[tokio::test]
async fn create_workspace_rejects_missing_name() {
    let registry = build_registry(rt());
    let err = registry
        .dispatch(
            "create",
            json!({"kind": "workspace", "properties": {"schema_version": 1}}),
        )
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("name"),
        "error should mention the missing name field; got: {err}"
    );
}

#[tokio::test]
async fn create_workspace_accepts_optional_filesystem_path() {
    let registry = build_registry(rt());
    let resp = registry
        .dispatch(
            "create",
            json!({
                "kind": "workspace",
                "name": "with-path",
                "properties": {"schema_version": 1, "filesystem_path": ".khive/workspaces/2026-07-11/pack-workspace"},
            }),
        )
        .await
        .expect("workspace with filesystem_path creates ok");
    assert_eq!(
        resp["properties"]["filesystem_path"],
        ".khive/workspaces/2026-07-11/pack-workspace"
    );
}

#[tokio::test]
async fn workspace_contains_task_is_allowed() {
    let registry = build_registry(rt());
    let ws = create_workspace(&registry, "ws-task").await;
    let task = registry
        .dispatch(
            "create",
            json!({"kind": "note", "note_kind": "task", "title": "do the thing"}),
        )
        .await
        .expect("task create ok");
    let task_id = task["id"].as_str().unwrap();

    registry
        .dispatch(
            "link",
            json!({"source_id": ws, "target_id": task_id, "relation": "contains"}),
        )
        .await
        .expect("workspace contains task must be allowed");
}

#[tokio::test]
async fn workspace_contains_session_is_allowed() {
    let registry = build_registry(rt());
    let ws = create_workspace(&registry, "ws-session").await;
    let session = registry
        .dispatch(
            "create",
            json!({"kind": "note", "note_kind": "session", "content": "session transcript"}),
        )
        .await
        .expect("session note create ok");
    let session_id = session["id"].as_str().unwrap();

    registry
        .dispatch(
            "link",
            json!({"source_id": ws, "target_id": session_id, "relation": "contains"}),
        )
        .await
        .expect("workspace contains session must be allowed");
}

#[tokio::test]
async fn workspace_contains_unrelated_entity_kind_is_rejected() {
    let registry = build_registry(rt());
    let ws = create_workspace(&registry, "ws-negative").await;
    let concept = registry
        .dispatch(
            "create",
            json!({"kind": "concept", "name": "unrelated concept"}),
        )
        .await
        .expect("concept create ok");
    let concept_id = concept["id"].as_str().unwrap();

    let err = registry
        .dispatch(
            "link",
            json!({"source_id": ws, "target_id": concept_id, "relation": "contains"}),
        )
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("relation") || err.to_string().contains("Invalid"),
        "workspace->concept contains must be rejected; got: {err}"
    );
}

#[tokio::test]
async fn workspace_depends_on_task_is_rejected() {
    let registry = build_registry(rt());
    let ws = create_workspace(&registry, "ws-negative-relation").await;
    let task = registry
        .dispatch(
            "create",
            json!({"kind": "note", "note_kind": "task", "title": "unrelated relation probe"}),
        )
        .await
        .expect("task create ok");
    let task_id = task["id"].as_str().unwrap();

    let err = registry
        .dispatch(
            "link",
            json!({"source_id": ws, "target_id": task_id, "relation": "depends_on"}),
        )
        .await
        .unwrap_err();
    assert!(
        !err.to_string().is_empty(),
        "workspace -[depends_on]-> task must be rejected (only contains is extended)"
    );
}
