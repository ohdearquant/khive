//! Integration tests for the workspace pack (issue #873 v0): entity-kind
//! registration, `REQUIRES`, the five `contains` endpoint rules (positive +
//! negative), and `name`/`schema_version` validation on create.

use khive_pack_git::GitPack;
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
    builder.register(GitPack::new(rt.clone()));
    builder.register(SessionPack::new(rt.clone()));
    builder.register(WorkspacePack::new(rt.clone()));
    let registry = builder.build().expect("registry builds");
    rt.install_edge_rules(registry.all_edge_rules());
    // #2943: reach the generic entity `update` path through the same
    // runtime-layer aggregate the production boot sequence installs, so
    // these tests exercise the real dispatch, not a bypassed one.
    rt.install_entity_kind_hooks(registry.entity_kind_hooks());
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
fn workspace_pack_requires_four_packs() {
    assert_eq!(
        WorkspacePack::REQUIRES,
        &["kg", "git", "gtd", "session"],
        "REQUIRES must list all four hard v0 dependencies per the SPEC-gate ruling"
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
async fn bulk_workspace_owner_validation_preserves_atomic_and_per_item_results() {
    for atomic in [true, false] {
        for properties in [
            None,
            Some(json!({})),
            Some(json!({"schema_version": null})),
            Some(json!({"schema_version": "1"})),
            Some(json!({"schema_version": 1.5})),
            Some(json!({"schema_version": true})),
            Some(json!({"schema_version": []})),
        ] {
            let runtime = rt();
            let registry = build_registry(runtime.clone());
            let mut invalid = json!({
                "kind": "entity", "entity_kind": "workspace", "name": "invalid workspace",
            });
            if let Some(properties) = properties {
                invalid["properties"] = properties;
            }
            let response = registry
                .dispatch(
                    "create",
                    json!({
                        "atomic": atomic,
                        "verbose": true,
                        "items": [
                            {"kind": "workspace", "name": "valid workspace", "properties": {"schema_version": 1}},
                            invalid,
                        ],
                    }),
                )
                .await;
            let expected_count = if atomic {
                let error = response.expect_err("owner refusal rejects the atomic batch");
                assert!(error.to_string().contains("schema_version"), "{error}");
                0
            } else {
                let response = response.expect("owner refusal is a per-item failure");
                assert_eq!(response["attempted"], 2, "{response}");
                assert_eq!(response["created"], 1, "{response}");
                assert_eq!(response["skipped"], 0, "{response}");
                assert_eq!(response["failed"], 1, "{response}");
                assert_eq!(response["errors"].as_array().unwrap().len(), 1);
                assert_eq!(response["errors"][0]["index"], 1);
                assert!(response["errors"][0]["error"]
                    .as_str()
                    .unwrap()
                    .contains("schema_version"));
                assert_eq!(response["entities"].as_array().unwrap().len(), 1);
                assert_eq!(response["entities"][0]["name"], "valid workspace");
                1
            };
            let listed = registry
                .dispatch("list", json!({"kind": "entity"}))
                .await
                .unwrap();
            assert_eq!(listed["items"].as_array().unwrap().len(), expected_count);
            let token = runtime
                .authorize(khive_runtime::Namespace::local())
                .unwrap();
            assert_eq!(
                runtime
                    .text(&token)
                    .unwrap()
                    .count(khive_storage::TextFilter::default())
                    .await
                    .unwrap(),
                expected_count as u64,
                "entity and FTS writes must share the same outcome"
            );
        }
    }
}

#[tokio::test]
async fn bulk_workspace_owner_validation_accepts_both_kind_spellings() {
    for atomic in [true, false] {
        let registry = build_registry(rt());
        let response = registry
            .dispatch(
                "create",
                json!({
                    "atomic": atomic,
                    "verbose": true,
                    "items": [
                        {"kind": "workspace", "name": "first", "properties": {"schema_version": 0}},
                        {"kind": "entity", "entity_kind": "workspace", "name": "second", "properties": {"schema_version": 2}},
                    ],
                }),
            )
            .await
            .unwrap();
        assert_eq!(response["attempted"], 2);
        assert_eq!(response["created"], 2);
        assert_eq!(response["failed"], 0);
        for (index, version) in [0, 2].into_iter().enumerate() {
            let entity = registry
                .dispatch("get", json!({"id": response["entities"][index]["id"]}))
                .await
                .unwrap();
            assert_eq!(entity["kind"], "workspace");
            assert_eq!(entity["properties"]["schema_version"], version);
        }
    }
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
async fn workspace_contains_issue_is_allowed() {
    let registry = build_registry(rt());
    let ws = create_workspace(&registry, "ws-issue").await;
    let issue = registry
        .dispatch(
            "create",
            json!({
                "kind": "note", "note_kind": "issue", "content": "issue body",
                "properties": {"number": 1, "project_id": Uuid::new_v4().to_string()},
            }),
        )
        .await
        .expect("issue create ok");
    let issue_id = issue["id"].as_str().unwrap();

    registry
        .dispatch(
            "link",
            json!({"source_id": ws, "target_id": issue_id, "relation": "contains"}),
        )
        .await
        .expect("workspace contains issue must be allowed");
}

#[tokio::test]
async fn workspace_contains_pull_request_is_allowed() {
    let registry = build_registry(rt());
    let ws = create_workspace(&registry, "ws-pr").await;
    let pr = registry
        .dispatch(
            "create",
            json!({
                "kind": "note", "note_kind": "pull_request", "content": "pr body",
                "properties": {"number": 7, "project_id": Uuid::new_v4().to_string()},
            }),
        )
        .await
        .expect("pull_request create ok");
    let pr_id = pr["id"].as_str().unwrap();

    registry
        .dispatch(
            "link",
            json!({"source_id": ws, "target_id": pr_id, "relation": "contains"}),
        )
        .await
        .expect("workspace contains pull_request must be allowed");
}

#[tokio::test]
async fn workspace_contains_commit_is_allowed() {
    let registry = build_registry(rt());
    let ws = create_workspace(&registry, "ws-commit").await;
    let commit = registry
        .dispatch(
            "create",
            json!({
                "kind": "note", "note_kind": "commit", "content": "commit body",
                "properties": {"sha": "a".repeat(40)},
            }),
        )
        .await
        .expect("commit create ok");
    let commit_id = commit["id"].as_str().unwrap();

    registry
        .dispatch(
            "link",
            json!({"source_id": ws, "target_id": commit_id, "relation": "contains"}),
        )
        .await
        .expect("workspace contains commit must be allowed");
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
async fn workspace_depends_on_issue_is_rejected() {
    let registry = build_registry(rt());
    let ws = create_workspace(&registry, "ws-negative-relation").await;
    let issue = registry
        .dispatch(
            "create",
            json!({
                "kind": "note", "note_kind": "issue", "content": "issue body",
                "properties": {"number": 2, "project_id": Uuid::new_v4().to_string()},
            }),
        )
        .await
        .expect("issue create ok");
    let issue_id = issue["id"].as_str().unwrap();

    let err = registry
        .dispatch(
            "link",
            json!({"source_id": ws, "target_id": issue_id, "relation": "depends_on"}),
        )
        .await
        .unwrap_err();
    assert!(
        !err.to_string().is_empty(),
        "workspace -[depends_on]-> issue must be rejected (only contains is extended)"
    );
}

// -----------------------------------------------------------------------
// #2943: a workspace entity's `properties.schema_version` invariant, which
// `prepare_create` enforces, was reachable through the generic entity
// `update` verb with no check at all — the acceptance witness named in the
// issue.
// -----------------------------------------------------------------------

#[tokio::test]
async fn update_workspace_rejects_non_integer_schema_version() {
    let registry = build_registry(rt());
    let ws = create_workspace(&registry, "ws-update-bad-schema").await;

    let err = registry
        .dispatch(
            "update",
            json!({"id": ws, "properties": {"schema_version": "not-an-int"}}),
        )
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("schema_version"),
        "error should mention schema_version; got: {err}"
    );
}

#[tokio::test]
async fn update_workspace_accepts_integer_schema_version() {
    let registry = build_registry(rt());
    let ws = create_workspace(&registry, "ws-update-good-schema").await;

    let resp = registry
        .dispatch(
            "update",
            json!({"id": ws, "properties": {"schema_version": 2}}),
        )
        .await
        .expect("update with a valid integer schema_version succeeds");
    assert_eq!(resp["properties"]["schema_version"], 2);
}

/// Class-closing guard (Leo ruling 2026-09-18 04:52Z, issue #2943): every
/// pack in this registry that declares an entity kind AND registers a
/// `KindHook` must be on the allowlist below, with a matching update-path
/// acceptance test (the `update_workspace_*` pair above, for `workspace`).
///
/// This enumerates `VerbRegistry::entity_kind_hooks()` over the registered
/// pack set rather than asserting `kind == "workspace"` directly, so a
/// second pack in THIS registry (kg/gtd/git/session/workspace) that adds an
/// entity-kind hook reddens this test the moment it lands, rather than
/// shipping with its update path unvalidated. It does not cover a pack
/// outside this registry (e.g. code/comm/memory/brain) gaining an
/// entity-kind hook — none of those declare a non-empty `ENTITY_KINDS`
/// today.
#[tokio::test]
async fn every_registered_entity_kind_hook_is_on_the_validated_allowlist() {
    const VALIDATED_ENTITY_KIND_HOOKS: &[&str] = &["workspace"];

    let registry = build_registry(rt());
    let hooked: std::collections::BTreeSet<String> = registry
        .entity_kind_hooks()
        .into_iter()
        .map(|(kind, _)| kind)
        .collect();
    let allowed: std::collections::BTreeSet<String> = VALIDATED_ENTITY_KIND_HOOKS
        .iter()
        .map(|s| s.to_string())
        .collect();
    assert_eq!(
        hooked, allowed,
        "a registered pack's entity-kind hook set changed; add/remove the matching \
         update-path acceptance test before updating VALIDATED_ENTITY_KIND_HOOKS"
    );
}
