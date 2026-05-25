//! End-to-end tests for the GTD pack against an in-memory runtime.

use khive_pack_gtd::GtdPack;
use khive_pack_kg::KgPack;
use khive_runtime::pack::HandlerDef;
use khive_runtime::{
    KhiveRuntime, Namespace, NoteKindSpec, RuntimeError, SchemaPlan, VerbRegistry,
    VerbRegistryBuilder,
};
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

    fn verbs(&self) -> Vec<&'static HandlerDef> {
        self.registry.all_verbs()
    }

    fn note_kinds(&self) -> Vec<&'static str> {
        self.registry.all_note_kinds()
    }

    #[allow(dead_code)]
    fn entity_kinds(&self) -> Vec<&'static str> {
        self.registry.all_entity_kinds()
    }

    fn name(&self) -> &'static str {
        "gtd"
    }
}

fn pack(rt: KhiveRuntime) -> Fixture {
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    builder.register(GtdPack::new(rt.clone()));
    let registry = builder.build().expect("registry builds");
    // Mirror what the MCP transport does at startup (ADR-031): install
    // pack-declared edge endpoint rules so validation can consult them.
    rt.install_edge_rules(registry.all_edge_rules());
    Fixture { registry }
}

async fn assign(pack: &Fixture, body: Value) -> Value {
    pack.dispatch("assign", body).await.expect("assign ok")
}

#[tokio::test]
async fn pack_metadata_matches_trait_consts() {
    let pack = pack(rt());
    assert_eq!(pack.name(), "gtd");
    assert!(pack.note_kinds().contains(&"task"));
    let verbs: Vec<&str> = pack.verbs().iter().map(|v| v.name).collect();
    assert!(verbs.contains(&"assign"));
    assert!(verbs.contains(&"next"));
    assert!(verbs.contains(&"complete"));
    assert!(verbs.contains(&"tasks"));
    assert!(verbs.contains(&"transition"));
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

    // Second complete must fail because "done" is a terminal state.
    let err = pack
        .dispatch("complete", json!({"id": id}))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("terminal state"));
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
        .create_note(
            &runtime.authorize(Namespace::local()),
            "observation",
            None,
            "hello",
            Some(0.5),
            None,
            vec![],
        )
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

#[tokio::test]
async fn assign_creates_depends_on_edge_between_tasks() {
    use khive_storage::types::{Direction, NeighborQuery};
    use khive_storage::EdgeRelation;

    let rt = rt();
    let pack = pack(rt.clone());

    let blocker = assign(&pack, json!({"title": "write spec"})).await;
    let blocker_full = blocker["full_id"].as_str().unwrap();
    let dependent = assign(
        &pack,
        json!({"title": "implement feature", "depends_on": [blocker_full]}),
    )
    .await;
    let dep_full = dependent["full_id"].as_str().unwrap();

    let dep_uuid = uuid::Uuid::parse_str(dep_full).unwrap();
    let blocker_uuid = uuid::Uuid::parse_str(blocker_full).unwrap();

    let graph = rt
        .graph(&rt.authorize(Namespace::local()))
        .expect("graph store");
    let neighbors = graph
        .neighbors(
            dep_uuid,
            NeighborQuery {
                direction: Direction::Out,
                relations: Some(vec![EdgeRelation::DependsOn]),
                limit: Some(16),
                min_weight: None,
            },
        )
        .await
        .expect("neighbors query");

    let targets: Vec<_> = neighbors.iter().map(|hit| hit.node_id).collect();
    assert!(
        targets.contains(&blocker_uuid),
        "ADR-031: task→task depends_on edge should exist; got targets {targets:?}"
    );
}

#[tokio::test]
async fn assign_rejects_depends_on_when_target_is_non_task_note() {
    use khive_storage::types::PageRequest;

    let rt = rt();
    let pack = pack(rt.clone());

    // Create a non-task note via runtime (e.g. an observation). The GTD edge
    // rule allows task→task only — task→observation should fail upfront so
    // the task is never persisted (ADR-030: no failure after successful write).
    let other = rt
        .create_note(
            &rt.authorize(Namespace::local()),
            "observation",
            None,
            "an observation",
            Some(0.5),
            None,
            vec![],
        )
        .await
        .expect("create observation");
    let other_full = other.id.as_hyphenated().to_string();

    let err = pack
        .dispatch(
            "assign",
            json!({"title": "depends on observation", "depends_on": [other_full]}),
        )
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("must be a task note"),
        "expected ADR-031 pack-rule rejection; got: {msg}"
    );

    // Atomicity: the rejected `assign` must not leave a task row behind.
    let notes = rt
        .notes(&rt.authorize(Namespace::local()))
        .expect("note store");
    let task_page = notes
        .query_notes(
            "local",
            Some("task"),
            PageRequest {
                offset: 0,
                limit: 64,
            },
        )
        .await
        .expect("query task notes");
    assert!(
        task_page.items.is_empty(),
        "rejected assign must not persist a task; found {:?}",
        task_page
            .items
            .iter()
            .filter_map(|n| n.name.clone())
            .collect::<Vec<_>>()
    );
}

// ── ADR-004 / ADR-019 cluster-15 tests ───────────────────────────────────────

/// F100: GtdPack exposes a schema_plan() returning the gtd_lifecycle_audit DDL.
#[tokio::test]
async fn pack_runtime_exposes_schema_plan() {
    use khive_runtime::PackRuntime;
    let pack = GtdPack::new(rt());
    let plan: SchemaPlan = pack.schema_plan();
    assert!(
        !plan.is_empty(),
        "GtdPack must return a non-empty SchemaPlan"
    );
    assert_eq!(plan.pack, "gtd");
    assert!(
        !plan.statements.is_empty(),
        "schema plan must have at least one DDL statement"
    );
    let combined = plan.statements.join(" ");
    assert!(
        combined.contains("gtd_lifecycle_audit"),
        "schema plan must reference gtd_lifecycle_audit table; got: {combined}"
    );
    assert!(
        combined.contains("CREATE TABLE IF NOT EXISTS"),
        "schema plan DDL must be idempotent (CREATE TABLE IF NOT EXISTS)"
    );
}

/// F100: VerbRegistry aggregates schema plans from loaded packs.
#[tokio::test]
async fn verb_registry_aggregates_schema_plans() {
    let fixture = pack(rt());
    let plans = fixture.registry.all_schema_plans();
    assert!(
        plans.iter().any(|p| p.pack == "gtd"),
        "registry must expose GTD schema plan; got packs: {:?}",
        plans.iter().map(|p| p.pack).collect::<Vec<_>>()
    );
}

/// F100 + ADR-004: GtdPack exposes NoteKindSpec for the task kind with lifecycle.
#[tokio::test]
async fn pack_runtime_exposes_note_kind_spec_for_task() {
    use khive_runtime::PackRuntime;
    let pack = GtdPack::new(rt());
    let specs: &[NoteKindSpec] = pack.note_kind_specs();
    assert!(
        !specs.is_empty(),
        "GtdPack must declare at least one NoteKindSpec"
    );

    let task_spec = specs
        .iter()
        .find(|s| s.kind == "task")
        .expect("GtdPack must have NoteKindSpec for 'task'");

    // ADR-004: lifecycle field must be "kind_status", NOT "status".
    assert_eq!(
        task_spec.lifecycle.field, "kind_status",
        "ADR-004: lifecycle field must be 'kind_status' to avoid collision with NoteStatus"
    );
    assert_eq!(
        task_spec.lifecycle.initial, "inbox",
        "task lifecycle must start at 'inbox'"
    );
    assert!(
        task_spec.lifecycle.terminal.contains(&"done"),
        "terminal states must include 'done'"
    );
    assert!(
        task_spec.lifecycle.terminal.contains(&"cancelled"),
        "terminal states must include 'cancelled'"
    );
}

/// F100: VerbRegistry aggregates NoteKindSpecs from loaded packs.
#[tokio::test]
async fn verb_registry_aggregates_note_kind_specs() {
    let fixture = pack(rt());
    let specs = fixture.registry.all_note_kind_specs();
    assert!(
        specs.iter().any(|s| s.kind == "task"),
        "registry must aggregate task NoteKindSpec"
    );
}

/// ADR-004: lifecycle transitions in NoteKindSpec match the runtime schema.
#[tokio::test]
async fn note_kind_spec_transitions_match_runtime_schema() {
    use khive_pack_gtd::schema::{can_transition, is_terminal};
    use khive_runtime::PackRuntime;

    let pack = GtdPack::new(rt());
    let specs = pack.note_kind_specs();
    let task_spec = specs.iter().find(|s| s.kind == "task").unwrap();

    // Every declared transition in the spec must agree with can_transition().
    for &(from, to) in task_spec.lifecycle.transitions {
        assert!(
            can_transition(from, to),
            "NoteKindSpec declares ({from}→{to}) but schema::can_transition disagrees"
        );
    }
    // Every terminal status in the spec must agree with is_terminal().
    for &t in task_spec.lifecycle.terminal {
        assert!(
            is_terminal(t),
            "NoteKindSpec declares '{t}' as terminal but schema::is_terminal disagrees"
        );
    }
}

/// F101: transition writes an audit record to gtd_lifecycle_audit.
#[tokio::test]
async fn transition_writes_lifecycle_audit_record() {
    use khive_storage::{SqlStatement, SqlValue};

    let rt = rt();
    let fixture = pack(rt.clone());

    let resp = assign(
        &fixture,
        json!({"title": "audit test task", "status": "inbox"}),
    )
    .await;
    let task_id = resp["full_id"].as_str().unwrap().to_string();

    fixture
        .dispatch(
            "transition",
            json!({"id": task_id, "status": "next", "note": "moved to next"}),
        )
        .await
        .expect("transition should succeed");

    // Query the audit table.
    let sql = rt.sql();
    let mut reader = sql.reader().await.expect("sql reader");
    let rows = reader
        .query_all(SqlStatement {
            sql: "SELECT note_id, from_state, to_state, note FROM gtd_lifecycle_audit \
                  WHERE note_id = ?1"
                .into(),
            params: vec![SqlValue::Text(task_id.clone())],
            label: None,
        })
        .await
        .expect("audit query");

    assert_eq!(
        rows.len(),
        1,
        "F101: transition must write exactly one audit row; got {rows:?}"
    );
    let row = &rows[0];
    assert_eq!(
        row.get("from_state").and_then(|v| {
            if let SqlValue::Text(s) = v {
                Some(s.as_str())
            } else {
                None
            }
        }),
        Some("inbox"),
        "audit from_state must be 'inbox'"
    );
    assert_eq!(
        row.get("to_state").and_then(|v| {
            if let SqlValue::Text(s) = v {
                Some(s.as_str())
            } else {
                None
            }
        }),
        Some("next"),
        "audit to_state must be 'next'"
    );
    assert_eq!(
        row.get("note").and_then(|v| {
            if let SqlValue::Text(s) = v {
                Some(s.as_str())
            } else {
                None
            }
        }),
        Some("moved to next"),
        "audit note field must be recorded"
    );
}

/// F101: complete writes an audit record to gtd_lifecycle_audit.
#[tokio::test]
async fn complete_writes_lifecycle_audit_record() {
    use khive_storage::{SqlStatement, SqlValue};

    let rt = rt();
    let fixture = pack(rt.clone());

    let resp = assign(&fixture, json!({"title": "audit complete test"})).await;
    let task_id = resp["full_id"].as_str().unwrap().to_string();

    fixture
        .dispatch("complete", json!({"id": task_id, "result": "done!"}))
        .await
        .expect("complete should succeed");

    let sql = rt.sql();
    let mut reader = sql.reader().await.expect("sql reader");
    let rows = reader
        .query_all(SqlStatement {
            sql: "SELECT from_state, to_state FROM gtd_lifecycle_audit WHERE note_id = ?1".into(),
            params: vec![SqlValue::Text(task_id.clone())],
            label: None,
        })
        .await
        .expect("audit query");

    assert_eq!(
        rows.len(),
        1,
        "F101: complete must write one audit row; got {rows:?}"
    );
    let row = &rows[0];
    assert_eq!(
        row.get("to_state").and_then(|v| {
            if let SqlValue::Text(s) = v {
                Some(s.as_str())
            } else {
                None
            }
        }),
        Some("done"),
        "audit to_state must be 'done'"
    );
}

// ── #273: terminal-state enforcement tests ───────────────────────────────────

/// Transitioning out of `done` must be rejected with a clear terminal-state error.
#[tokio::test]
async fn test_transition_from_done_rejected() {
    let pack = pack(rt());
    let resp = assign(&pack, json!({"title": "terminal done test"})).await;
    let id = resp["full_id"].as_str().unwrap().to_string();

    // Move to done.
    pack.dispatch("transition", json!({"id": id, "status": "done"}))
        .await
        .expect("transition to done must succeed");

    // Any further transition out of done must fail.
    for target in &["next", "active", "inbox", "waiting", "someday", "cancelled"] {
        let err = pack
            .dispatch("transition", json!({"id": id, "status": target}))
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("terminal state"),
            "transition from done to {target:?} must mention terminal state; got: {msg}"
        );
        assert!(
            msg.contains("done"),
            "error must include current state 'done'; got: {msg}"
        );
    }
}

/// Transitioning out of `cancelled` must be rejected with a clear terminal-state error.
#[tokio::test]
async fn test_transition_from_cancelled_rejected() {
    let pack = pack(rt());
    let resp = assign(&pack, json!({"title": "terminal cancelled test"})).await;
    let id = resp["full_id"].as_str().unwrap().to_string();

    // Move to cancelled.
    pack.dispatch("transition", json!({"id": id, "status": "cancelled"}))
        .await
        .expect("transition to cancelled must succeed");

    // Any further transition out of cancelled must fail.
    for target in &["next", "active", "inbox", "waiting", "someday", "done"] {
        let err = pack
            .dispatch("transition", json!({"id": id, "status": target}))
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("terminal state"),
            "transition from cancelled to {target:?} must mention terminal state; got: {msg}"
        );
        assert!(
            msg.contains("cancelled"),
            "error must include current state 'cancelled'; got: {msg}"
        );
    }
}

/// Calling `complete` on an already-done task must return an explicit terminal-state error.
#[tokio::test]
async fn test_complete_on_already_done_returns_clear_error() {
    let pack = pack(rt());
    let resp = assign(&pack, json!({"title": "double complete test"})).await;
    let id = resp["full_id"].as_str().unwrap().to_string();

    // First complete succeeds.
    let done = pack
        .dispatch("complete", json!({"id": id, "result": "shipped"}))
        .await
        .expect("first complete must succeed");
    assert_eq!(done["to"], "done");

    // Second complete on an already-done task must fail with a clear error.
    let err = pack
        .dispatch("complete", json!({"id": id}))
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("terminal state"),
        "complete on already-done must mention terminal state; got: {msg}"
    );
    assert!(
        msg.contains("done"),
        "error must name the current terminal state; got: {msg}"
    );
}

// ── Response-layer status remap (note.status vs task.status) ─────────────────
//
// Option A fix: when a note kind carries a pack-owned lifecycle in
// `properties.status`, the KG `get` and `list` response serialization layer
// promotes `properties.status` to the top-level `status` field and moves the
// row-visibility value to `lifecycle`.  These tests verify that contract from
// the consumer's perspective.

/// assign(title="t") → get(id) → data.status == "inbox"
///
/// Verifies that the KG `get` verb exposes the GTD status at `data.status`,
/// NOT the row-visibility value ("active").
#[tokio::test]
async fn get_task_exposes_gtd_status_not_row_visibility() {
    let pack = pack(rt());
    let resp = assign(&pack, json!({"title": "status remap test"})).await;
    let full_id = resp["full_id"].as_str().unwrap().to_string();

    let got = pack
        .dispatch("get", json!({"id": full_id}))
        .await
        .expect("get must succeed");

    // P-H2: get returns flat — note fields at top level, no data wrapper.
    assert!(
        got.get("data").is_none(),
        "get must NOT wrap in {{data: ...}} (P-H2); got: {got}"
    );
    // status must be the GTD lifecycle value.
    assert_eq!(
        got["status"], "inbox",
        "get(task) must expose GTD status 'inbox' at top-level status; got: {got}"
    );
    // lifecycle must hold the row-visibility value.
    assert_eq!(
        got["lifecycle"], "active",
        "get(task) must move row-visibility to top-level lifecycle; got: {got}"
    );
}

/// assign → transition(active) → get → data.status == "active"
///
/// When the GTD status happens to equal the row-visibility string ("active"),
/// the remap still produces the correct result: `status` = GTD "active",
/// `lifecycle` = row-visibility "active".  Both fields agree here but they
/// are semantically distinct.
#[tokio::test]
async fn get_task_after_transition_exposes_updated_gtd_status() {
    let pack = pack(rt());
    let resp = assign(&pack, json!({"title": "transition remap test"})).await;
    let full_id = resp["full_id"].as_str().unwrap().to_string();

    pack.dispatch("transition", json!({"id": full_id, "status": "active"}))
        .await
        .expect("transition to active must succeed");

    let got = pack
        .dispatch("get", json!({"id": full_id}))
        .await
        .expect("get after transition must succeed");

    // P-H2: flat response.
    assert_eq!(
        got["status"], "active",
        "after transition to active, status must be 'active' (GTD); got: {got}"
    );
    assert_eq!(
        got["lifecycle"], "active",
        "row-visibility must remain 'active' for a live task; got: {got}"
    );
}

/// assign → complete → get → data.status == "done"
///
/// Verifies the "done" terminal state is surfaced correctly.
#[tokio::test]
async fn get_task_after_complete_exposes_done_status() {
    let pack = pack(rt());
    let resp = assign(&pack, json!({"title": "complete remap test"})).await;
    let full_id = resp["full_id"].as_str().unwrap().to_string();

    pack.dispatch("complete", json!({"id": full_id, "result": "shipped"}))
        .await
        .expect("complete must succeed");

    let got = pack
        .dispatch("get", json!({"id": full_id}))
        .await
        .expect("get after complete must succeed");

    // P-H2: flat response.
    assert_eq!(
        got["status"], "done",
        "after complete, status must be 'done'; got: {got}"
    );
    assert_eq!(
        got["lifecycle"], "active",
        "soft-completed task row-visibility is still 'active'; got: {got}"
    );
}

/// list(kind=task) → each item's `status` == GTD status, not row-visibility
///
/// The `list` path in the KG handler also applies the remap.
#[tokio::test]
async fn list_task_exposes_gtd_status_not_row_visibility() {
    let pack = pack(rt());
    assign(&pack, json!({"title": "list remap inbox"})).await;
    assign(&pack, json!({"title": "list remap next", "status": "next"})).await;

    let list_resp = pack
        .dispatch("list", json!({"kind": "task"}))
        .await
        .expect("list must succeed");
    let items = list_resp.as_array().expect("list must return array");

    // Collect statuses from the response.
    let statuses: Vec<&str> = items.iter().filter_map(|n| n["status"].as_str()).collect();

    // Both GTD statuses must appear, neither should be "active" (row-visibility)
    // unless a task was explicitly assigned with status="active".
    assert!(
        statuses.contains(&"inbox"),
        "list(task) must expose 'inbox' GTD status; got: {statuses:?}"
    );
    assert!(
        statuses.contains(&"next"),
        "list(task) must expose 'next' GTD status; got: {statuses:?}"
    );
    // Row-visibility "active" must NOT appear as a status unless one of the tasks
    // actually has GTD status="active" (none assigned above).
    assert!(
        !statuses.iter().all(|&s| s == "active"),
        "list(task) must NOT return row-visibility 'active' as the only status; got: {statuses:?}"
    );

    // Every item must also carry `lifecycle` = "active" (row-visibility for live rows).
    for item in items {
        assert_eq!(
            item["lifecycle"], "active",
            "list(task) must include lifecycle field for row-visibility; got item: {item}"
        );
    }
}

/// F101: idempotent same-status transition does NOT write an audit record.
///
/// Strategy: perform one real transition (inbox → next) to initialize the audit
/// schema and record a baseline row, then attempt a noop (next → next) and
/// confirm only the baseline row exists (count stays at 1, not 2).
#[tokio::test]
async fn noop_transition_does_not_write_audit_record() {
    use khive_storage::{SqlStatement, SqlValue};

    let rt = rt();
    let fixture = pack(rt.clone());

    let resp = assign(
        &fixture,
        json!({"title": "noop audit test", "status": "inbox"}),
    )
    .await;
    let task_id = resp["full_id"].as_str().unwrap().to_string();

    // Real transition — initializes the audit schema and writes one row.
    fixture
        .dispatch("transition", json!({"id": task_id, "status": "next"}))
        .await
        .expect("real transition should succeed");

    // Noop transition — must not write a second row.
    let r = fixture
        .dispatch("transition", json!({"id": task_id, "status": "next"}))
        .await
        .expect("noop transition should return ok");
    assert_eq!(
        r["transitioned"], false,
        "noop must return transitioned=false"
    );

    // Should still have exactly ONE audit row (from the real transition above).
    let sql = rt.sql();
    let mut reader = sql.reader().await.expect("sql reader");
    let rows = reader
        .query_all(SqlStatement {
            sql: "SELECT COUNT(*) as cnt FROM gtd_lifecycle_audit WHERE note_id = ?1".into(),
            params: vec![SqlValue::Text(task_id.clone())],
            label: None,
        })
        .await
        .expect("audit count query");

    let count = rows
        .first()
        .and_then(|r| r.get("cnt"))
        .and_then(|v| {
            if let SqlValue::Integer(n) = v {
                Some(*n)
            } else {
                None
            }
        })
        .unwrap_or(-1);

    assert_eq!(
        count, 1,
        "noop transition must not insert an audit row (expected 1 baseline row, got {count})"
    );
}

// ── Wave-1 fix tests ──────────────────────────────────────────────────────────

/// Fix 1: assign(status="done") must be rejected at creation time.
#[tokio::test]
async fn assign_rejects_terminal_status_done() {
    let pack = pack(rt());
    let err = pack
        .dispatch(
            "assign",
            json!({"title": "terminal task", "status": "done"}),
        )
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("cannot create task in terminal state"),
        "expected terminal-state rejection; got: {msg}"
    );
    assert!(
        msg.contains("done"),
        "error must name the bad status; got: {msg}"
    );
}

/// Fix 1: assign(status="cancelled") must be rejected at creation time.
#[tokio::test]
async fn assign_rejects_terminal_status_cancelled() {
    let pack = pack(rt());
    let err = pack
        .dispatch(
            "assign",
            json!({"title": "terminal task", "status": "cancelled"}),
        )
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("cannot create task in terminal state"),
        "expected terminal-state rejection; got: {msg}"
    );
    assert!(
        msg.contains("cancelled"),
        "error must name the bad status; got: {msg}"
    );
}

/// Fix 1: assign(status="inbox") must succeed (non-terminal initial status).
#[tokio::test]
async fn assign_accepts_inbox_status() {
    let pack = pack(rt());
    let resp = pack
        .dispatch("assign", json!({"title": "inbox task", "status": "inbox"}))
        .await
        .expect("inbox is a valid initial status");
    assert_eq!(resp["status"], "inbox");
}

/// Fix 2: assign(due="2026-06-01T00:00:00Z") must succeed and store RFC 3339.
#[tokio::test]
async fn assign_due_iso8601_full_accepted() {
    let pack = pack(rt());
    let resp = pack
        .dispatch(
            "assign",
            json!({"title": "iso due", "due": "2026-06-01T00:00:00Z"}),
        )
        .await
        .expect("full ISO-8601 due must be accepted");
    let due = resp["due"].as_str().expect("due must be a string");
    // Must be parseable as RFC 3339.
    chrono::DateTime::parse_from_rfc3339(due)
        .unwrap_or_else(|e| panic!("due not RFC 3339: {due} — {e}"));
}

/// Fix 2: assign(due="2026-06-01") (date-only) must succeed and store RFC 3339.
#[tokio::test]
async fn assign_due_date_only_accepted() {
    let pack = pack(rt());
    let resp = pack
        .dispatch(
            "assign",
            json!({"title": "date-only due", "due": "2026-06-01"}),
        )
        .await
        .expect("date-only due must be accepted");
    let due = resp["due"].as_str().expect("due must be a string");
    chrono::DateTime::parse_from_rfc3339(due)
        .unwrap_or_else(|e| panic!("due not RFC 3339: {due} — {e}"));
}

/// Fix 2: assign(due="tomorrow") must be rejected with a clear error.
#[tokio::test]
async fn assign_due_free_text_rejected() {
    let pack = pack(rt());
    let err = pack
        .dispatch("assign", json!({"title": "vague due", "due": "tomorrow"}))
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("due must be ISO-8601"),
        "expected ISO-8601 error; got: {msg}"
    );
    assert!(
        msg.contains("tomorrow"),
        "error must echo the bad value; got: {msg}"
    );
}

/// Fix 2: assign(due="June 1st 2026") must be rejected.
#[tokio::test]
async fn assign_due_natural_language_rejected() {
    let pack = pack(rt());
    let err = pack
        .dispatch(
            "assign",
            json!({"title": "vague due", "due": "June 1st 2026"}),
        )
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("due must be ISO-8601"),
        "expected ISO-8601 error; got: {msg}"
    );
}

/// Fix 3: complete response must include completed_at field.
#[tokio::test]
async fn complete_response_includes_completed_at() {
    let pack = pack(rt());
    let resp = assign(&pack, json!({"title": "track completion time"})).await;
    let id = resp["full_id"].as_str().unwrap().to_string();

    let done = pack
        .dispatch("complete", json!({"id": id, "result": "shipped"}))
        .await
        .expect("complete must succeed");

    let completed_at = done["completed_at"]
        .as_str()
        .expect("completed_at must be in response");
    chrono::DateTime::parse_from_rfc3339(completed_at)
        .unwrap_or_else(|e| panic!("completed_at not RFC 3339: {completed_at} — {e}"));
}

/// Fix 3: get(id) after complete must show GTD status as "done".
#[tokio::test]
async fn complete_sets_properties_status_to_done() {
    let rt = rt();
    let fixture = pack(rt.clone());

    let resp = assign(&fixture, json!({"title": "check status after complete"})).await;
    let id = resp["full_id"].as_str().unwrap().to_string();
    let uuid = uuid::Uuid::parse_str(&id).unwrap();

    fixture
        .dispatch("complete", json!({"id": id}))
        .await
        .expect("complete must succeed");

    let token = rt.authorize(khive_runtime::Namespace::local());
    let note = rt
        .notes(&token)
        .expect("note store")
        .get_note(uuid)
        .await
        .expect("get_note")
        .expect("note must exist");

    let gtd_status = note
        .properties
        .as_ref()
        .and_then(|p| p.get("status"))
        .and_then(|v| v.as_str())
        .expect("properties.status must be set");
    assert_eq!(
        gtd_status, "done",
        "properties.status must be 'done' after complete"
    );

    let has_completed_at = note
        .properties
        .as_ref()
        .and_then(|p| p.get("completed_at"))
        .is_some();
    assert!(
        has_completed_at,
        "properties.completed_at must be set after complete"
    );
}

/// Fix 4: transition response must include full task snapshot fields.
#[tokio::test]
async fn transition_response_includes_task_fields() {
    let pack = pack(rt());
    let resp = assign(
        &pack,
        json!({"title": "snapshot task", "priority": "p1", "assignee": "alice"}),
    )
    .await;
    let id = resp["full_id"].as_str().unwrap().to_string();

    let r = pack
        .dispatch("transition", json!({"id": id, "status": "next"}))
        .await
        .expect("transition must succeed");

    assert_eq!(r["transitioned"], true);
    assert_eq!(r["title"], "snapshot task", "response must include title");
    assert_eq!(r["priority"], "p1", "response must include priority");
    assert_eq!(r["assignee"], "alice", "response must include assignee");
    // due was not set; must be present but null.
    assert!(
        r.get("due").is_some(),
        "response must include due (null if unset)"
    );
}

/// Fix 5: timestamp format is RFC 3339 across assign/tasks/complete/get.
#[tokio::test]
async fn timestamps_are_rfc3339_across_verbs() {
    let pack = pack(rt());
    let resp = assign(
        &pack,
        json!({"title": "ts check", "due": "2026-06-01T00:00:00Z"}),
    )
    .await;
    let id = resp["full_id"].as_str().unwrap().to_string();

    // assign response: created_at, updated_at must be RFC 3339.
    for field in &["created_at", "updated_at"] {
        let ts = resp[field]
            .as_str()
            .unwrap_or_else(|| panic!("{field} missing"));
        chrono::DateTime::parse_from_rfc3339(ts)
            .unwrap_or_else(|e| panic!("{field} not RFC 3339: {ts} — {e}"));
    }
    // due must be RFC 3339 after parsing.
    let due = resp["due"].as_str().expect("due must be a string");
    chrono::DateTime::parse_from_rfc3339(due)
        .unwrap_or_else(|e| panic!("due not RFC 3339: {due} — {e}"));

    // tasks listing: same fields.
    let tasks = pack.dispatch("tasks", json!({})).await.unwrap();
    let task = tasks
        .as_array()
        .unwrap()
        .first()
        .expect("at least one task");
    for field in &["created_at", "updated_at"] {
        let ts = task[field]
            .as_str()
            .unwrap_or_else(|| panic!("tasks.{field} missing"));
        chrono::DateTime::parse_from_rfc3339(ts)
            .unwrap_or_else(|e| panic!("tasks.{field} not RFC 3339: {ts} — {e}"));
    }

    // complete response: completed_at must be RFC 3339.
    let done = pack.dispatch("complete", json!({"id": id})).await.unwrap();
    let completed_at = done["completed_at"].as_str().expect("completed_at missing");
    chrono::DateTime::parse_from_rfc3339(completed_at)
        .unwrap_or_else(|e| panic!("completed_at not RFC 3339: {completed_at} — {e}"));
}
// ---- Fix 3: complete/transition write GTD status to notes.status column ----

/// After `complete`, a `get` on the task must show `data.status = "done"`,
/// not always "active". Regression for Fix 3.
#[tokio::test]
async fn complete_writes_status_column_to_done() {
    let pack = pack(rt());
    let resp = assign(&pack, json!({"title": "Write notes.status on complete"})).await;
    let id = resp["full_id"].as_str().unwrap().to_string();

    pack.dispatch("complete", json!({"id": id}))
        .await
        .expect("complete must succeed");

    // `get` round-trips through the kg pack's note handler.
    let fetched = pack
        .dispatch("get", json!({"id": id}))
        .await
        .expect("get after complete must succeed");

    // P-H2: get returns flat — status at top level.
    let status = fetched["status"].as_str().unwrap_or("<missing>");
    assert_eq!(
        status, "done",
        "notes.status column must be 'done' after complete (Fix 3); got: {status}"
    );
}

/// After `transition` to `active`, a `get` on the task must show
/// `status = "next"`. Regression for Fix 3 (P-H2: flat response).
#[tokio::test]
async fn transition_writes_status_column() {
    let pack = pack(rt());
    let resp = assign(&pack, json!({"title": "Write notes.status on transition"})).await;
    let id = resp["full_id"].as_str().unwrap().to_string();

    // inbox → next
    pack.dispatch("transition", json!({"id": id, "status": "next"}))
        .await
        .expect("transition inbox→next must succeed");

    let fetched = pack
        .dispatch("get", json!({"id": id}))
        .await
        .expect("get after transition must succeed");

    // P-H2: get returns flat — status at top level.
    let status = fetched["status"].as_str().unwrap_or("<missing>");
    assert_eq!(
        status, "next",
        "notes.status column must be 'next' after transition (Fix 3); got: {status}"
    );
}

// ── G-C2: tasks() default excludes terminal statuses (regression) ─────────────

/// `tasks(priority=X)` without `status=` must exclude done/cancelled tasks.
/// `tasks(priority=X, status="done")` must still return done tasks.
#[tokio::test]
async fn tasks_priority_filter_excludes_terminal_by_default() {
    let pack = pack(rt());

    // Create 4 tasks: A(p0,inbox), B(p0,done), C(p0,next), D(p0,cancelled).
    let a = assign(
        &pack,
        json!({"title": "A", "priority": "p0", "status": "inbox"}),
    )
    .await;
    let b = assign(
        &pack,
        json!({"title": "B", "priority": "p0", "status": "inbox"}),
    )
    .await;
    let _c = assign(
        &pack,
        json!({"title": "C", "priority": "p0", "status": "next"}),
    )
    .await;
    let d = assign(
        &pack,
        json!({"title": "D", "priority": "p0", "status": "inbox"}),
    )
    .await;

    // Transition B → done, D → cancelled.
    let b_id = b["full_id"].as_str().unwrap().to_string();
    let d_id = d["full_id"].as_str().unwrap().to_string();
    pack.dispatch("transition", json!({"id": b_id, "status": "done"}))
        .await
        .expect("B→done");
    pack.dispatch("transition", json!({"id": d_id, "status": "cancelled"}))
        .await
        .expect("D→cancelled");

    // tasks(priority="p0") — no status filter — must return A and C only.
    let resp = pack
        .dispatch("tasks", json!({"priority": "p0"}))
        .await
        .unwrap();
    let arr = resp.as_array().unwrap();
    let titles: Vec<&str> = arr
        .iter()
        .map(|t| t["title"].as_str().unwrap_or("?"))
        .collect();
    assert!(
        !titles.contains(&"B"),
        "tasks(priority=p0) must exclude done task B; got: {titles:?}"
    );
    assert!(
        !titles.contains(&"D"),
        "tasks(priority=p0) must exclude cancelled task D; got: {titles:?}"
    );
    assert!(
        titles.contains(&"A"),
        "tasks(priority=p0) must include inbox task A; got: {titles:?}"
    );
    assert!(
        titles.contains(&"C"),
        "tasks(priority=p0) must include next task C; got: {titles:?}"
    );
    assert_eq!(arr.len(), 2, "expected exactly A and C; got: {titles:?}");

    // tasks(priority="p0", status="done") — explicit status — must return only B.
    let resp_done = pack
        .dispatch("tasks", json!({"priority": "p0", "status": "done"}))
        .await
        .unwrap();
    let arr_done = resp_done.as_array().unwrap();
    assert_eq!(
        arr_done.len(),
        1,
        "explicit status=done must return exactly B"
    );
    assert_eq!(arr_done[0]["title"], "B");

    // tasks() — no filter at all — must not include B or D.
    let resp_all = pack.dispatch("tasks", json!({})).await.unwrap();
    let all_titles: Vec<&str> = resp_all
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["title"].as_str().unwrap_or("?"))
        .collect();
    assert!(
        !all_titles.contains(&"B"),
        "tasks() default must exclude done task B; got: {all_titles:?}"
    );
    assert!(
        !all_titles.contains(&"D"),
        "tasks() default must exclude cancelled task D; got: {all_titles:?}"
    );

    // Also confirm the unused `a` ID is valid (suppress unused-variable lint).
    let _ = a["full_id"].as_str();
}

/// `next()` must already correctly filter to actionable tasks only.
/// This test ensures the G-C2 fix does not regress `next`.
#[tokio::test]
async fn next_excludes_terminal_tasks() {
    let pack = pack(rt());

    let t1 = assign(&pack, json!({"title": "active-task", "status": "next"})).await;
    let t2 = assign(&pack, json!({"title": "done-task", "status": "inbox"})).await;
    let t2_id = t2["full_id"].as_str().unwrap().to_string();

    pack.dispatch("transition", json!({"id": t2_id, "status": "done"}))
        .await
        .expect("done transition");

    let resp = pack.dispatch("next", json!({})).await.unwrap();
    let titles: Vec<&str> = resp
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["title"].as_str().unwrap_or("?"))
        .collect();

    assert!(
        titles.contains(&"active-task"),
        "next must include actionable task; got: {titles:?}"
    );
    assert!(
        !titles.contains(&"done-task"),
        "next must not include done task; got: {titles:?}"
    );

    let _ = t1["full_id"].as_str();
}
