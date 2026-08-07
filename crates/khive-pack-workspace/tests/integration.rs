//! Integration tests for the workspace pack (issue #873 v0): entity-kind
//! registration, `REQUIRES`, the five `contains` endpoint rules (positive +
//! negative), and `name`/`schema_version` validation on create.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use khive_pack_git::GitPack;
use khive_pack_gtd::GtdPack;
use khive_pack_kg::KgPack;
use khive_pack_session::SessionPack;
use khive_pack_workspace::WorkspacePack;
use khive_runtime::pack::PackRuntime;
use khive_runtime::{
    KhiveRuntime, KindHook, NamespaceToken, RuntimeError, VerbRegistry, VerbRegistryBuilder,
};
use khive_types::{HandlerDef, Pack};
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
    registry
}

#[derive(Debug)]
struct CountingEntityHook {
    prepare_calls: Arc<AtomicUsize>,
    after_calls: Arc<AtomicUsize>,
}

#[async_trait]
impl KindHook for CountingEntityHook {
    async fn prepare_create(
        &self,
        _runtime: &KhiveRuntime,
        args: &mut serde_json::Value,
    ) -> Result<(), RuntimeError> {
        self.prepare_calls.fetch_add(1, Ordering::SeqCst);
        let properties = args
            .as_object_mut()
            .expect("create args are an object")
            .entry("properties")
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .ok_or_else(|| RuntimeError::InvalidInput("properties must be an object".into()))?;
        if properties
            .get("reject_hook")
            .and_then(serde_json::Value::as_bool)
            == Some(true)
        {
            return Err(RuntimeError::InvalidInput("hook rejected item".into()));
        }
        properties.insert("hook_prepared".to_string(), json!(true));
        Ok(())
    }

    async fn after_create(
        &self,
        _runtime: &KhiveRuntime,
        _id: Uuid,
        args: &serde_json::Value,
    ) -> Result<(), RuntimeError> {
        if args["properties"]["hook_prepared"] != json!(true) {
            return Err(RuntimeError::Internal(
                "after_create did not receive prepared params".into(),
            ));
        }
        self.after_calls.fetch_add(1, Ordering::SeqCst);
        if args["properties"]["fail_after"] == json!(true) {
            return Err(RuntimeError::Internal(
                "injected after_create failure".into(),
            ));
        }
        Ok(())
    }
}

struct HookProbePack {
    hook: Arc<CountingEntityHook>,
}

impl Pack for HookProbePack {
    const NAME: &'static str = "hook-probe";
    const NOTE_KINDS: &'static [&'static str] = &["hook_probe_note"];
    const ENTITY_KINDS: &'static [&'static str] = &["hook_probe"];
    const HANDLERS: &'static [HandlerDef] = &[];
    const REQUIRES: &'static [&'static str] = &["kg"];
}

#[async_trait]
impl PackRuntime for HookProbePack {
    fn name(&self) -> &str {
        <Self as Pack>::NAME
    }

    fn note_kinds(&self) -> &'static [&'static str] {
        <Self as Pack>::NOTE_KINDS
    }

    fn entity_kinds(&self) -> &'static [&'static str] {
        <Self as Pack>::ENTITY_KINDS
    }

    fn handlers(&self) -> &'static [HandlerDef] {
        <Self as Pack>::HANDLERS
    }

    fn requires(&self) -> &'static [&'static str] {
        <Self as Pack>::REQUIRES
    }

    fn kind_hook(&self, kind: &str) -> Option<Arc<dyn KindHook>> {
        if matches!(kind, "hook_probe" | "hook_probe_note") {
            Some(self.hook.clone())
        } else {
            None
        }
    }

    async fn dispatch(
        &self,
        verb: &str,
        _params: serde_json::Value,
        _registry: &VerbRegistry,
        _token: &NamespaceToken,
    ) -> Result<serde_json::Value, RuntimeError> {
        Err(RuntimeError::InvalidInput(format!(
            "HookProbePack does not handle verb {verb:?}"
        )))
    }
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
async fn bulk_create_workspace_runs_hook_per_item_in_best_effort_mode() {
    let registry = build_registry(rt());
    let response = registry
        .dispatch(
            "create",
            json!({
                "items": [
                    {
                        "kind": "workspace",
                        "name": "bulk-valid",
                        "properties": {"schema_version": 1}
                    },
                    {"kind": "workspace", "name": "bulk-missing-schema"}
                ]
            }),
        )
        .await
        .expect("best-effort bulk create returns ordered per-item outcomes");

    assert_eq!(response["attempted"], 2);
    assert_eq!(response["created"], 1);
    assert_eq!(response["failed"], 1);
    assert_eq!(response["results"][0]["ok"], true);
    assert_eq!(response["results"][1]["ok"], false);
    assert!(response["results"][1]["error"]
        .as_str()
        .is_some_and(|error| error.contains("schema_version")));

    let listed = registry
        .dispatch("list", json!({"kind": "workspace"}))
        .await
        .expect("workspace list succeeds");
    let names = listed["items"]
        .as_array()
        .expect("list returns items")
        .iter()
        .filter_map(|item| item["name"].as_str())
        .collect::<Vec<_>>();
    assert_eq!(names, vec!["bulk-valid"]);
}

#[tokio::test]
async fn atomic_bulk_create_workspace_hook_failure_writes_nothing() {
    let registry = build_registry(rt());
    let error = registry
        .dispatch(
            "create",
            json!({
                "atomic": true,
                "items": [
                    {
                        "kind": "workspace",
                        "name": "atomic-valid",
                        "properties": {"schema_version": 1}
                    },
                    {"kind": "workspace", "name": "atomic-missing-schema"}
                ]
            }),
        )
        .await
        .expect_err("a hook-invalid item rejects the atomic bulk create");
    assert!(error.to_string().contains("schema_version"));

    let listed = registry
        .dispatch("list", json!({"kind": "workspace"}))
        .await
        .expect("workspace list succeeds");
    assert_eq!(listed["items"].as_array().map(Vec::len), Some(0));
}

#[tokio::test]
async fn bulk_entity_hook_runs_after_create_only_for_persisted_items() {
    let runtime = rt();
    let prepare_calls = Arc::new(AtomicUsize::new(0));
    let after_calls = Arc::new(AtomicUsize::new(0));
    let hook = Arc::new(CountingEntityHook {
        prepare_calls: prepare_calls.clone(),
        after_calls: after_calls.clone(),
    });
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(runtime));
    builder.register(HookProbePack { hook });
    let registry = builder.build().expect("registry builds");

    let response = registry
        .dispatch(
            "create",
            json!({
                "items": [
                    {"kind": "hook_probe", "name": "persisted"},
                    {
                        "kind": "hook_probe",
                        "name": "rejected",
                        "properties": {"reject_hook": true}
                    }
                ]
            }),
        )
        .await
        .expect("best-effort bulk create returns per-item outcomes");

    assert_eq!(response["created"], 1);
    assert_eq!(response["failed"], 1);
    assert_eq!(prepare_calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        after_calls.load(Ordering::SeqCst),
        1,
        "after_create must run only for the persisted item and receive the prepared params"
    );
    let listed = registry
        .dispatch("list", json!({"kind": "hook_probe"}))
        .await
        .expect("probe list succeeds");
    assert_eq!(listed["items"].as_array().map(Vec::len), Some(1));
    assert_eq!(listed["items"][0]["properties"]["hook_prepared"], true);
}

#[tokio::test]
async fn singleton_note_after_create_failure_is_returned_as_structured_repair_stage() {
    let runtime = rt();
    let prepare_calls = Arc::new(AtomicUsize::new(0));
    let after_calls = Arc::new(AtomicUsize::new(0));
    let hook = Arc::new(CountingEntityHook {
        prepare_calls,
        after_calls: after_calls.clone(),
    });
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(runtime));
    builder.register(HookProbePack { hook });
    let registry = builder.build().expect("registry builds");

    let response = registry
        .dispatch(
            "create",
            json!({
                "kind": "hook_probe_note",
                "content": "singleton hook diagnostic",
                "properties": {"fail_after": true}
            }),
        )
        .await
        .expect("after_create failure must not erase the committed note");

    assert_eq!(after_calls.load(Ordering::SeqCst), 1);
    assert_eq!(response["post_commit_failures"][0], response["id"]);
    assert_eq!(
        response["post_commit_failure_details"][0]["stages"][0]["stage"],
        "after_create"
    );
    assert!(response["post_commit_failure_details"][0]["stages"][0]
        .get("model")
        .is_none());
}

#[tokio::test]
async fn bulk_note_after_create_failure_is_returned_as_structured_repair_stage() {
    let runtime = rt();
    let prepare_calls = Arc::new(AtomicUsize::new(0));
    let after_calls = Arc::new(AtomicUsize::new(0));
    let hook = Arc::new(CountingEntityHook {
        prepare_calls,
        after_calls: after_calls.clone(),
    });
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(runtime));
    builder.register(HookProbePack { hook });
    let registry = builder.build().expect("registry builds");

    let response = registry
        .dispatch(
            "create",
            json!({
                "items": [{
                    "kind": "hook_probe_note",
                    "content": "bulk hook diagnostic",
                    "properties": {"fail_after": true}
                }]
            }),
        )
        .await
        .expect("after_create failure must not erase the committed bulk note");

    assert_eq!(after_calls.load(Ordering::SeqCst), 1);
    assert_eq!(response["created"], 1);
    assert_eq!(
        response["post_commit_failures"][0],
        response["results"][0]["id"]
    );
    assert_eq!(
        response["post_commit_failure_details"][0]["stages"][0]["stage"],
        "after_create"
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
