//! End-to-end tests for the GTD pack against an in-memory runtime.

use khive_pack_gtd::GtdPack;
use khive_runtime::pack::VerbDef;
use khive_runtime::{KhiveRuntime, RuntimeError, VerbRegistry, VerbRegistryBuilder};
use serde_json::{json, Value};

fn rt() -> KhiveRuntime {
    KhiveRuntime::memory().expect("memory runtime")
}

/// Test fixture: a `VerbRegistry` containing a freshly registered `GtdPack`,
/// with pass-through metadata methods so existing tests keep working.
struct Fixture {
    registry: VerbRegistry,
}

impl Fixture {
    async fn dispatch(&self, verb: &str, args: Value) -> Result<Value, RuntimeError> {
        self.registry.dispatch(verb, args).await
    }

    fn verbs(&self) -> Vec<&'static VerbDef> {
        self.registry.all_verbs()
    }

    fn note_kinds(&self) -> Vec<&'static str> {
        self.registry.all_note_kinds()
    }

    fn entity_kinds(&self) -> Vec<&'static str> {
        self.registry.all_entity_kinds()
    }

    fn name(&self) -> &'static str {
        "gtd"
    }
}

fn pack(rt: KhiveRuntime) -> Fixture {
    let mut builder = VerbRegistryBuilder::new();
    builder.register(GtdPack::new(rt));
    Fixture {
        registry: builder.build(),
    }
}

async fn assign(pack: &Fixture, body: Value) -> Value {
    pack.dispatch("assign", body).await.expect("assign ok")
}

#[tokio::test]
async fn pack_metadata_matches_trait_consts() {
    let pack = pack(rt());
    assert_eq!(pack.name(), "gtd");
    assert_eq!(pack.note_kinds(), &["task"]);
    assert!(pack.entity_kinds().is_empty());
    let verbs: Vec<&str> = pack.verbs().iter().map(|v| v.name).collect();
    assert_eq!(
        verbs,
        vec!["assign", "next", "complete", "tasks", "transition"]
    );
}

#[tokio::test]
async fn assign_creates_a_task_with_defaults() {
    let pack = pack(rt());
    let resp = assign(&pack, json!({"title": "write README", "priority": "p1"})).await;
    assert_eq!(resp["kind"], "task");
    assert_eq!(resp["title"], "write README");
    assert_eq!(resp["status"], "inbox");
    assert_eq!(resp["priority"], "p1");
    assert!(resp["id"].as_str().unwrap().len() == 8);
    assert!(resp["full_id"].as_str().unwrap().contains('-'));
}

#[tokio::test]
async fn assign_rejects_empty_title() {
    let pack = pack(rt());
    let err = pack
        .dispatch("assign", json!({"title": "  "}))
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("title must not be empty"), "got: {msg}");
}

#[tokio::test]
async fn assign_rejects_invalid_status_and_priority() {
    let pack = pack(rt());
    let err = pack
        .dispatch("assign", json!({"title": "x", "status": "bogus"}))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("invalid status"));

    let err = pack
        .dispatch("assign", json!({"title": "x", "priority": "p9"}))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("invalid priority"));
}

#[tokio::test]
async fn assign_alias_status_normalizes_to_canonical() {
    let pack = pack(rt());
    let resp = assign(
        &pack,
        json!({"title": "ship feature", "status": "in_progress"}),
    )
    .await;
    assert_eq!(resp["status"], "active");
}

#[tokio::test]
async fn next_returns_only_actionable_in_priority_order() {
    let pack = pack(rt());

    assign(
        &pack,
        json!({"title": "low", "status": "next", "priority": "p3"}),
    )
    .await;
    let _ = assign(&pack, json!({"title": "later", "status": "someday"})).await;
    assign(
        &pack,
        json!({"title": "urgent", "status": "next", "priority": "p0"}),
    )
    .await;
    assign(
        &pack,
        json!({"title": "mid", "status": "active", "priority": "p2"}),
    )
    .await;

    let resp = pack.dispatch("next", json!({})).await.unwrap();
    let arr = resp.as_array().unwrap();
    assert_eq!(arr.len(), 3, "only next/active count as actionable");
    let titles: Vec<&str> = arr.iter().map(|t| t["title"].as_str().unwrap()).collect();
    assert_eq!(titles, vec!["urgent", "mid", "low"]);
}

#[tokio::test]
async fn next_supports_assignee_filter() {
    let pack = pack(rt());
    assign(
        &pack,
        json!({"title": "alice's job", "status": "next", "assignee": "alice"}),
    )
    .await;
    assign(
        &pack,
        json!({"title": "bob's job", "status": "next", "assignee": "bob"}),
    )
    .await;

    let resp = pack
        .dispatch("next", json!({"assignee": "alice"}))
        .await
        .unwrap();
    let arr = resp.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["title"], "alice's job");
}

#[tokio::test]
async fn complete_marks_task_done_and_is_idempotent_via_load_check() {
    let pack = pack(rt());
    let resp = assign(&pack, json!({"title": "do thing"})).await;
    let id = resp["full_id"].as_str().unwrap().to_string();

    let done = pack
        .dispatch("complete", json!({"id": id, "result": "shipped"}))
        .await
        .unwrap();
    assert_eq!(done["completed"], true);
    assert_eq!(done["from"], "inbox");
    assert_eq!(done["to"], "done");

    // Second complete must fail because "done" → "done" isn't an allowed transition.
    let err = pack
        .dispatch("complete", json!({"id": id}))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("cannot transition"));
}

#[tokio::test]
async fn complete_via_short_id_resolves_prefix() {
    let pack = pack(rt());
    let resp = assign(&pack, json!({"title": "via short id"})).await;
    let short = resp["id"].as_str().unwrap().to_string();
    assert_eq!(short.len(), 8);

    let done = pack
        .dispatch("complete", json!({"id": short}))
        .await
        .unwrap();
    assert_eq!(done["to"], "done");
}

#[tokio::test]
async fn complete_rejects_non_task_notes() {
    // Reach around the pack and create a kg-shaped "observation" note to prove
    // the task-kind guard fires.
    let runtime = rt();
    let note = runtime
        .create_note(None, "observation", None, "hello", 0.5, None, vec![])
        .await
        .unwrap();
    let pack = pack(runtime);
    let err = pack
        .dispatch(
            "complete",
            json!({"id": note.id.as_hyphenated().to_string()}),
        )
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("expected kind=\"task\""),
        "msg: {err}"
    );
}

#[tokio::test]
async fn tasks_filters_by_status_and_priority() {
    let pack = pack(rt());
    assign(
        &pack,
        json!({"title": "p0 waiting", "priority": "p0", "status": "waiting"}),
    )
    .await;
    assign(
        &pack,
        json!({"title": "p2 next", "priority": "p2", "status": "next"}),
    )
    .await;
    assign(
        &pack,
        json!({"title": "p0 next", "priority": "p0", "status": "next"}),
    )
    .await;

    let resp = pack
        .dispatch("tasks", json!({"status": "next"}))
        .await
        .unwrap();
    let arr = resp.as_array().unwrap();
    assert_eq!(arr.len(), 2);

    let resp = pack
        .dispatch("tasks", json!({"status": "next", "priority": "p0"}))
        .await
        .unwrap();
    let arr = resp.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["title"], "p0 next");
}

#[tokio::test]
async fn transition_enforces_lifecycle_rules() {
    let pack = pack(rt());
    let resp = assign(&pack, json!({"title": "ship"})).await;
    let id = resp["full_id"].as_str().unwrap().to_string();

    // inbox → done is allowed.
    let r = pack
        .dispatch("transition", json!({"id": id, "status": "active"}))
        .await
        .unwrap();
    assert_eq!(r["to"], "active");

    // active → inbox is NOT allowed.
    let err = pack
        .dispatch("transition", json!({"id": id, "status": "inbox"}))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("cannot transition"));
}

#[tokio::test]
async fn transition_to_same_status_is_idempotent_noop() {
    let pack = pack(rt());
    let resp = assign(&pack, json!({"title": "noop", "status": "next"})).await;
    let id = resp["full_id"].as_str().unwrap().to_string();

    let r = pack
        .dispatch("transition", json!({"id": id, "status": "next"}))
        .await
        .unwrap();
    assert_eq!(r["transitioned"], false);
    assert_eq!(r["note"], "already in target status");
}

#[tokio::test]
async fn unknown_verb_returns_invalid_input() {
    let pack = pack(rt());
    let err = pack.dispatch("retire", json!({})).await.unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("unknown verb"), "got: {msg}");
    assert!(msg.contains("retire"), "got: {msg}");
}
