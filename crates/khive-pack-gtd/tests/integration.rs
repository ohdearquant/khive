//! End-to-end tests for the GTD pack against an in-memory runtime.
//!
//! FILE SIZE JUSTIFICATION: All integration tests share a single `Fixture` helper and
//! a common `pack()` factory that wires KgPack + GtdPack against an in-memory runtime.
//! Splitting into multiple files would either duplicate this fixture or require exposing
//! it as a separate test-helper crate. The single-file layout keeps test discovery
//! straightforward and the shared setup code co-located with the tests that depend on it.

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

    // REASON: entity_kinds() is part of the Fixture helper API and may be used in
    // future tests; suppressing dead_code avoids noisy compiler warnings without
    // removing useful test infrastructure.
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
    // Mirror what the MCP transport does at startup: install pack-declared
    // edge endpoint rules so validation can consult them.
    rt.install_edge_rules(registry.all_edge_rules());
    Fixture { registry }
}

async fn assign(pack: &Fixture, body: Value) -> Value {
    pack.dispatch("gtd.assign", body).await.expect("assign ok")
}

#[tokio::test]
async fn pack_metadata_matches_trait_consts() {
    let pack = pack(rt());
    assert_eq!(pack.name(), "gtd");
    assert!(pack.note_kinds().contains(&"task"));
    let verbs: Vec<&str> = pack.verbs().iter().map(|v| v.name).collect();
    assert!(verbs.contains(&"gtd.assign"));
    assert!(verbs.contains(&"gtd.next"));
    assert!(verbs.contains(&"gtd.complete"));
    assert!(verbs.contains(&"gtd.tasks"));
    assert!(verbs.contains(&"gtd.transition"));
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
        .dispatch("gtd.assign", json!({"title": "  "}))
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("title must not be empty"), "got: {msg}");
}

#[tokio::test]
async fn assign_rejects_invalid_status_and_priority() {
    let pack = pack(rt());
    let err = pack
        .dispatch("gtd.assign", json!({"title": "x", "status": "bogus"}))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("invalid status"));

    let err = pack
        .dispatch("gtd.assign", json!({"title": "x", "priority": "p9"}))
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

    let resp = pack.dispatch("gtd.next", json!({})).await.unwrap();
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
        .dispatch("gtd.next", json!({"assignee": "alice"}))
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

    // UE2-H1: must transition to an actionable state before completing.
    pack.dispatch("gtd.transition", json!({"id": id, "status": "next"}))
        .await
        .expect("transition to next must succeed");

    let done = pack
        .dispatch("gtd.complete", json!({"id": id, "result": "shipped"}))
        .await
        .unwrap();
    assert_eq!(done["completed"], true);
    assert_eq!(done["from"], "next");
    assert_eq!(done["to"], "done");

    // Second complete must fail because "done" is a terminal state.
    let err = pack
        .dispatch("gtd.complete", json!({"id": id}))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("terminal state"));
}

#[tokio::test]
async fn complete_via_short_id_resolves_prefix() {
    let pack = pack(rt());
    let resp = assign(&pack, json!({"title": "via short id"})).await;
    let full_id = resp["full_id"].as_str().unwrap().to_string();
    let short = resp["id"].as_str().unwrap().to_string();
    assert_eq!(short.len(), 8);

    // UE2-H1: transition to next first.
    pack.dispatch("gtd.transition", json!({"id": full_id, "status": "next"}))
        .await
        .expect("transition to next must succeed");

    let done = pack
        .dispatch("gtd.complete", json!({"id": short}))
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
            &runtime.authorize(Namespace::local()).unwrap(),
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
            "gtd.complete",
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
        .dispatch("gtd.tasks", json!({"status": "next"}))
        .await
        .unwrap();
    let arr = resp.as_array().unwrap();
    assert_eq!(arr.len(), 2);

    let resp = pack
        .dispatch("gtd.tasks", json!({"status": "next", "priority": "p0"}))
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
        .dispatch("gtd.transition", json!({"id": id, "status": "active"}))
        .await
        .unwrap();
    assert_eq!(r["to"], "active");

    // active → inbox is NOT allowed.
    let err = pack
        .dispatch("gtd.transition", json!({"id": id, "status": "inbox"}))
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
        .dispatch("gtd.transition", json!({"id": id, "status": "next"}))
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
        .graph(&rt.authorize(Namespace::local()).unwrap())
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
        "task→task depends_on edge should exist; got targets {targets:?}"
    );
}

#[tokio::test]
async fn assign_rejects_depends_on_when_target_is_non_task_note() {
    use khive_storage::types::PageRequest;

    let rt = rt();
    let pack = pack(rt.clone());

    // Create a non-task note via runtime (e.g. an observation). The GTD edge
    // rule allows task→task only — task→observation should fail upfront so
    // the task is never persisted (no failure after successful write invariant).
    let other = rt
        .create_note(
            &rt.authorize(Namespace::local()).unwrap(),
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
            "gtd.assign",
            json!({"title": "depends on observation", "depends_on": [other_full]}),
        )
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("must be a task note"),
        "expected pack edge-rule rejection (task→task only); got: {msg}"
    );

    // Atomicity: the rejected `assign` must not leave a task row behind.
    let notes = rt
        .notes(&rt.authorize(Namespace::local()).unwrap())
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

// ── NoteKindSpec + lifecycle audit tests ─────────────────────────────────────

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

/// F100: GtdPack exposes NoteKindSpec for the task kind with lifecycle.
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

    // Lifecycle field must be "kind_status", NOT "status" (avoids collision
    // with NoteStatus, which is a row-visibility field).
    assert_eq!(
        task_spec.lifecycle.field, "kind_status",
        "lifecycle field must be 'kind_status' to avoid collision with NoteStatus"
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

/// Lifecycle transitions declared in NoteKindSpec must match the runtime schema.
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
            "gtd.transition",
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

    // UE2-H1: transition to actionable state first.
    fixture
        .dispatch("gtd.transition", json!({"id": task_id, "status": "next"}))
        .await
        .expect("transition to next should succeed");

    fixture
        .dispatch("gtd.complete", json!({"id": task_id, "result": "done!"}))
        .await
        .expect("complete should succeed");

    let sql = rt.sql();
    let mut reader = sql.reader().await.expect("sql reader");
    let rows = reader
        .query_all(SqlStatement {
            sql: "SELECT from_state, to_state FROM gtd_lifecycle_audit \
                  WHERE note_id = ?1 AND to_state = 'done'"
                .into(),
            params: vec![SqlValue::Text(task_id.clone())],
            label: None,
        })
        .await
        .expect("audit query");

    assert_eq!(
        rows.len(),
        1,
        "F101: complete must write exactly one audit row with to_state='done'; got {rows:?}"
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
    pack.dispatch("gtd.transition", json!({"id": id, "status": "done"}))
        .await
        .expect("transition to done must succeed");

    // Any further transition out of done must fail.
    for target in &["next", "active", "inbox", "waiting", "someday", "cancelled"] {
        let err = pack
            .dispatch("gtd.transition", json!({"id": id, "status": target}))
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
    pack.dispatch("gtd.transition", json!({"id": id, "status": "cancelled"}))
        .await
        .expect("transition to cancelled must succeed");

    // Any further transition out of cancelled must fail.
    for target in &["next", "active", "inbox", "waiting", "someday", "done"] {
        let err = pack
            .dispatch("gtd.transition", json!({"id": id, "status": target}))
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

    // UE2-H1: transition to actionable state first.
    pack.dispatch("gtd.transition", json!({"id": id, "status": "next"}))
        .await
        .expect("transition to next must succeed");

    // First complete succeeds.
    let done = pack
        .dispatch("gtd.complete", json!({"id": id, "result": "shipped"}))
        .await
        .expect("first complete must succeed");
    assert_eq!(done["to"], "done");

    // Second complete on an already-done task must fail with a clear error.
    let err = pack
        .dispatch("gtd.complete", json!({"id": id}))
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

    pack.dispatch("gtd.transition", json!({"id": full_id, "status": "active"}))
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

    // UE2-H1: transition to actionable state first.
    pack.dispatch("gtd.transition", json!({"id": full_id, "status": "next"}))
        .await
        .expect("transition to next must succeed");

    pack.dispatch("gtd.complete", json!({"id": full_id, "result": "shipped"}))
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
        .dispatch("gtd.transition", json!({"id": task_id, "status": "next"}))
        .await
        .expect("real transition should succeed");

    // Noop transition — must not write a second row.
    let r = fixture
        .dispatch("gtd.transition", json!({"id": task_id, "status": "next"}))
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
            "gtd.assign",
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
            "gtd.assign",
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
        .dispatch(
            "gtd.assign",
            json!({"title": "inbox task", "status": "inbox"}),
        )
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
            "gtd.assign",
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
            "gtd.assign",
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
        .dispatch(
            "gtd.assign",
            json!({"title": "vague due", "due": "tomorrow"}),
        )
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
            "gtd.assign",
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

    // UE2-H1: transition to actionable state first.
    pack.dispatch("gtd.transition", json!({"id": id, "status": "active"}))
        .await
        .expect("transition to active must succeed");

    let done = pack
        .dispatch("gtd.complete", json!({"id": id, "result": "shipped"}))
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

    // UE2-H1: transition to actionable state first.
    fixture
        .dispatch("gtd.transition", json!({"id": id, "status": "next"}))
        .await
        .expect("transition to next must succeed");

    fixture
        .dispatch("gtd.complete", json!({"id": id}))
        .await
        .expect("complete must succeed");

    let token = rt.authorize(khive_runtime::Namespace::local()).unwrap();
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
        .dispatch("gtd.transition", json!({"id": id, "status": "next"}))
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
    let tasks = pack.dispatch("gtd.tasks", json!({})).await.unwrap();
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

    // UE2-H1: transition to actionable state first.
    pack.dispatch("gtd.transition", json!({"id": id, "status": "next"}))
        .await
        .expect("transition to next must succeed");

    // complete response: completed_at must be RFC 3339.
    let done = pack
        .dispatch("gtd.complete", json!({"id": id}))
        .await
        .unwrap();
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

    // UE2-H1: transition to actionable state first.
    pack.dispatch("gtd.transition", json!({"id": id, "status": "next"}))
        .await
        .expect("transition to next must succeed");

    pack.dispatch("gtd.complete", json!({"id": id}))
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
    pack.dispatch("gtd.transition", json!({"id": id, "status": "next"}))
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
    pack.dispatch("gtd.transition", json!({"id": b_id, "status": "done"}))
        .await
        .expect("B→done");
    pack.dispatch("gtd.transition", json!({"id": d_id, "status": "cancelled"}))
        .await
        .expect("D→cancelled");

    // tasks(priority="p0") — no status filter — must return A and C only.
    let resp = pack
        .dispatch("gtd.tasks", json!({"priority": "p0"}))
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
        .dispatch("gtd.tasks", json!({"priority": "p0", "status": "done"}))
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
    let resp_all = pack.dispatch("gtd.tasks", json!({})).await.unwrap();
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

    pack.dispatch("gtd.transition", json!({"id": t2_id, "status": "done"}))
        .await
        .expect("done transition");

    let resp = pack.dispatch("gtd.next", json!({})).await.unwrap();
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

// ── UE2-H1: complete() state machine enforcement ─────────────────────────────

/// complete() from inbox must be rejected — task must be in next or active first.
#[tokio::test]
async fn complete_from_inbox_is_rejected() {
    let pack = pack(rt());
    let resp = assign(&pack, json!({"title": "inbox task"})).await;
    let id = resp["full_id"].as_str().unwrap().to_string();
    assert_eq!(resp["status"], "inbox");

    let err = pack
        .dispatch("gtd.complete", json!({"id": id}))
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("inbox"),
        "error must mention current state 'inbox'; got: {msg}"
    );
    assert!(
        msg.contains("transition to 'next' or 'active'"),
        "error must guide caller to transition first; got: {msg}"
    );
}

/// complete() from waiting must be rejected.
#[tokio::test]
async fn complete_from_waiting_is_rejected() {
    let pack = pack(rt());
    let resp = assign(&pack, json!({"title": "waiting task", "status": "waiting"})).await;
    let id = resp["full_id"].as_str().unwrap().to_string();

    let err = pack
        .dispatch("gtd.complete", json!({"id": id}))
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("waiting"),
        "error must mention current state 'waiting'; got: {msg}"
    );
}

/// complete() from someday must be rejected.
#[tokio::test]
async fn complete_from_someday_is_rejected() {
    let pack = pack(rt());
    let resp = assign(&pack, json!({"title": "someday task", "status": "someday"})).await;
    let id = resp["full_id"].as_str().unwrap().to_string();

    let err = pack
        .dispatch("gtd.complete", json!({"id": id}))
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("someday"),
        "error must mention current state 'someday'; got: {msg}"
    );
}

/// complete() from next must succeed.
#[tokio::test]
async fn complete_from_next_succeeds() {
    let pack = pack(rt());
    let resp = assign(&pack, json!({"title": "next task", "status": "next"})).await;
    let id = resp["full_id"].as_str().unwrap().to_string();

    let done = pack
        .dispatch("gtd.complete", json!({"id": id}))
        .await
        .expect("complete from next must succeed");
    assert_eq!(done["from"], "next");
    assert_eq!(done["to"], "done");
}

/// complete() from active must succeed.
#[tokio::test]
async fn complete_from_active_succeeds() {
    let pack = pack(rt());
    let resp = assign(&pack, json!({"title": "active task", "status": "active"})).await;
    let id = resp["full_id"].as_str().unwrap().to_string();

    let done = pack
        .dispatch("gtd.complete", json!({"id": id}))
        .await
        .expect("complete from active must succeed");
    assert_eq!(done["from"], "active");
    assert_eq!(done["to"], "done");
}

// ── Wave 4 regression tests (CC-1, ue-dsl-parallel C2, scenario-gtd C2) ───────

/// CC-1: complete(id, status="cancelled") must honour the status arg and
/// transition to "cancelled", NOT silently force "done".
#[tokio::test]
async fn cc1_complete_with_status_cancelled_reaches_cancelled() {
    let pack = pack(rt());
    let resp = assign(&pack, json!({"title": "cc1-cancel-test", "status": "next"})).await;
    let id = resp["full_id"].as_str().unwrap().to_string();

    let result = pack
        .dispatch("gtd.complete", json!({"id": id, "status": "cancelled"}))
        .await
        .expect("complete(status=cancelled) must succeed");

    assert_eq!(
        result["to"], "cancelled",
        "CC-1: complete(status=cancelled) must transition to 'cancelled', not 'done'; got: {result}"
    );
    assert_eq!(result["completed"], true);
    assert!(
        result["is_terminal"].as_bool().unwrap_or(false),
        "CC-1: cancelled must be a terminal state; got: {result}"
    );
}

/// CC-1: complete(id, status="done") must work as before.
#[tokio::test]
async fn cc1_complete_with_status_done_still_works() {
    let pack = pack(rt());
    let resp = assign(&pack, json!({"title": "cc1-done-test", "status": "next"})).await;
    let id = resp["full_id"].as_str().unwrap().to_string();

    let result = pack
        .dispatch("gtd.complete", json!({"id": id, "status": "done"}))
        .await
        .expect("complete(status=done) must succeed");

    assert_eq!(result["to"], "done", "CC-1: explicit status=done must work");
}

/// CC-1: complete(id) with no status still defaults to "done".
#[tokio::test]
async fn cc1_complete_default_is_done() {
    let pack = pack(rt());
    let resp = assign(
        &pack,
        json!({"title": "cc1-default-test", "status": "next"}),
    )
    .await;
    let id = resp["full_id"].as_str().unwrap().to_string();

    let result = pack
        .dispatch("gtd.complete", json!({"id": id}))
        .await
        .expect("complete() with no status must default to done");

    assert_eq!(result["to"], "done", "CC-1: default status must be 'done'");
}

/// CC-1: complete(id, status="bogus") must be rejected, not silently force "done".
#[tokio::test]
async fn cc1_complete_invalid_status_is_rejected() {
    let pack = pack(rt());
    let resp = assign(&pack, json!({"title": "cc1-bogus-test", "status": "next"})).await;
    let id = resp["full_id"].as_str().unwrap().to_string();

    let err = pack
        .dispatch("gtd.complete", json!({"id": id, "status": "bogus"}))
        .await
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("\"done\" or \"cancelled\""),
        "CC-1: invalid status must be rejected with helpful message; got: {msg}"
    );
}

/// CC-1: complete(status="cancelled") must also write the audit record with to="cancelled".
#[tokio::test]
async fn cc1_complete_cancelled_writes_audit_record() {
    use khive_storage::{SqlStatement, SqlValue};

    let rt = rt();
    let fixture = pack(rt.clone());

    let resp = assign(
        &fixture,
        json!({"title": "cc1-audit-cancel", "status": "next"}),
    )
    .await;
    let task_id = resp["full_id"].as_str().unwrap().to_string();

    fixture
        .dispatch(
            "gtd.complete",
            json!({"id": task_id, "status": "cancelled"}),
        )
        .await
        .expect("complete(status=cancelled) must succeed");

    let sql = rt.sql();
    let mut reader = sql.reader().await.expect("sql reader");
    let rows = reader
        .query_all(SqlStatement {
            sql: "SELECT from_state, to_state FROM gtd_lifecycle_audit \
                  WHERE note_id = ?1 AND to_state = 'cancelled'"
                .into(),
            params: vec![SqlValue::Text(task_id.clone())],
            label: None,
        })
        .await
        .expect("audit query");

    assert_eq!(
        rows.len(),
        1,
        "CC-1: complete(status=cancelled) must write audit row with to_state='cancelled'"
    );
}

/// ue-dsl-parallel C2 / CC-race: simulating parallel complete() via sequential
/// calls. After the first complete() succeeds, the second must return an error
/// because the task is in terminal state — it must NOT return a false success.
/// (True concurrent race requires tokio::join!, but the atomic SQL ensures the
/// second loses even in that case; this test covers the serial leg first.)
#[tokio::test]
async fn dsl_parallel_c2_double_complete_second_must_fail() {
    let pack = pack(rt());
    let resp = assign(&pack, json!({"title": "race-test", "status": "next"})).await;
    let id = resp["full_id"].as_str().unwrap().to_string();

    // First complete succeeds.
    let first = pack
        .dispatch("gtd.complete", json!({"id": id, "result": "op-A"}))
        .await
        .expect("first complete must succeed");
    assert_eq!(first["to"], "done");

    // Second complete must fail because "done" is terminal.
    let err = pack
        .dispatch("gtd.complete", json!({"id": id, "result": "op-B"}))
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("terminal state"),
        "dsl-parallel C2: second complete must fail with terminal-state error; got: {err}"
    );
}

/// ue-dsl-parallel C2: true concurrent complete() race using tokio::join!.
/// Both tasks run concurrently; exactly ONE must succeed and ONE must fail.
#[tokio::test]
async fn dsl_parallel_c2_concurrent_complete_one_wins_one_loses() {
    let rt = rt();
    let fixture = pack(rt.clone());

    let resp = assign(
        &fixture,
        json!({"title": "concurrent-race", "status": "next"}),
    )
    .await;
    let id = resp["full_id"].as_str().unwrap().to_string();
    let id2 = id.clone();

    let pack_a = std::sync::Arc::new(fixture);
    let pack_b = pack_a.clone();

    let (res_a, res_b) = tokio::join!(
        pack_a.dispatch("gtd.complete", json!({"id": id, "result": "op-A"})),
        pack_b.dispatch("gtd.complete", json!({"id": id2, "result": "op-B"})),
    );

    let successes = [res_a.is_ok(), res_b.is_ok()]
        .iter()
        .filter(|&&ok| ok)
        .count();
    let failures = [res_a.is_err(), res_b.is_err()]
        .iter()
        .filter(|&&e| e)
        .count();

    assert_eq!(
        successes, 1,
        "dsl-parallel C2: exactly one concurrent complete() must succeed; got {successes} successes"
    );
    assert_eq!(
        failures, 1,
        "dsl-parallel C2: exactly one concurrent complete() must fail; got {failures} failures"
    );
}

/// scenario-gtd C2: `next()` must not return tasks whose `depends_on` includes
/// tasks that are NOT in `done` status.
#[tokio::test]
async fn scenario_gtd_c2_next_excludes_tasks_with_incomplete_deps() {
    let pack = pack(rt());

    // Blocker task: starts inbox (not done).
    let blocker = assign(&pack, json!({"title": "blocker", "status": "inbox"})).await;
    let blocker_id = blocker["full_id"].as_str().unwrap().to_string();

    // Dependent task: depends on the blocker, status=next.
    let dependent = assign(
        &pack,
        json!({
            "title": "dependent-task",
            "status": "next",
            "depends_on": [blocker_id]
        }),
    )
    .await;
    let dep_id = dependent["full_id"].as_str().unwrap().to_string();

    // next() must NOT return the dependent task (its dep is not done).
    let result = pack.dispatch("gtd.next", json!({})).await.unwrap();
    let titles: Vec<&str> = result
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["title"].as_str().unwrap_or("?"))
        .collect();
    assert!(
        !titles.contains(&"dependent-task"),
        "scenario-gtd C2: next() must not return tasks with incomplete deps; got: {titles:?}"
    );

    // Now complete the blocker.
    pack.dispatch(
        "gtd.transition",
        json!({"id": blocker_id, "status": "done"}),
    )
    .await
    .expect("blocker→done");

    // next() must now include the dependent task.
    let result2 = pack.dispatch("gtd.next", json!({})).await.unwrap();
    let titles2: Vec<&str> = result2
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["title"].as_str().unwrap_or("?"))
        .collect();
    assert!(
        titles2.contains(&"dependent-task"),
        "scenario-gtd C2: after blocker is done, next() must include dependent task; got: {titles2:?}"
    );

    let _ = dep_id; // suppress unused warning
}

/// scenario-gtd C2: a task with NO depends_on must always appear in next().
#[tokio::test]
async fn scenario_gtd_c2_next_includes_tasks_with_no_deps() {
    let pack = pack(rt());
    assign(
        &pack,
        json!({"title": "no-deps-task", "status": "next", "priority": "p1"}),
    )
    .await;

    let result = pack.dispatch("gtd.next", json!({})).await.unwrap();
    let titles: Vec<&str> = result
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["title"].as_str().unwrap_or("?"))
        .collect();
    assert!(
        titles.contains(&"no-deps-task"),
        "scenario-gtd C2: task with no deps must appear in next(); got: {titles:?}"
    );
}

/// scenario-gtd C2: a task whose ALL deps are done must appear in next().
#[tokio::test]
async fn scenario_gtd_c2_next_includes_tasks_with_all_deps_done() {
    let pack = pack(rt());

    let b1 = assign(&pack, json!({"title": "dep-done-1", "status": "inbox"})).await;
    let b1_id = b1["full_id"].as_str().unwrap().to_string();
    let b2 = assign(&pack, json!({"title": "dep-done-2", "status": "inbox"})).await;
    let b2_id = b2["full_id"].as_str().unwrap().to_string();

    // Complete both blockers.
    pack.dispatch("gtd.transition", json!({"id": b1_id, "status": "done"}))
        .await
        .unwrap();
    pack.dispatch("gtd.transition", json!({"id": b2_id, "status": "done"}))
        .await
        .unwrap();

    // Dependent task with two done deps.
    let dep = assign(
        &pack,
        json!({
            "title": "all-deps-done",
            "status": "next",
            "depends_on": [b1_id, b2_id]
        }),
    )
    .await;
    let dep_id = dep["full_id"].as_str().unwrap().to_string();

    let result = pack.dispatch("gtd.next", json!({})).await.unwrap();
    let titles: Vec<&str> = result
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["title"].as_str().unwrap_or("?"))
        .collect();
    assert!(
        titles.contains(&"all-deps-done"),
        "scenario-gtd C2: task with all deps done must appear in next(); got: {titles:?}"
    );

    let _ = dep_id;
}

/// Regression: `next()` must surface a task whose completed blocker lives
/// outside the 500-task scan window (older than the 500 newest task rows).
///
/// This exercises the batch-fetch path added to `handle_next` that resolves
/// dependency statuses for UUIDs absent from the initial list_notes page.
#[tokio::test]
async fn next_resolves_deps_older_than_500_task_window() {
    use khive_storage::note::Note;
    use khive_storage::types::PageRequest;
    use serde_json::json;

    let runtime = rt();
    let token = runtime
        .authorize(khive_runtime::Namespace::local())
        .unwrap();
    let note_store = runtime.notes(&token).expect("note store");

    // Create a blocker task with `done` status directly in storage, timestamped
    // old enough that list_notes (newest-500) will never include it once we
    // pad the database with 500 newer tasks.
    let blocker_id = uuid::Uuid::new_v4();
    let old_ts = chrono::Utc::now().timestamp_micros() - 1_000_000_000_000; // ~11 days ago
    let blocker = Note {
        id: blocker_id,
        namespace: "local".to_string(),
        kind: "task".to_string(),
        status: "active".to_string(),
        name: Some("ancient-blocker".to_string()),
        content: "ancient blocker task".to_string(),
        salience: None,
        decay_factor: None,
        expires_at: None,
        properties: Some(json!({"status": "done"})),
        created_at: old_ts,
        updated_at: old_ts,
        deleted_at: None,
    };
    note_store
        .upsert_note(blocker)
        .await
        .expect("insert blocker");

    // Pad the database with 500 filler tasks (all inbox, not the blocker).
    // These are newer than the blocker so they dominate the list_notes window.
    let now = chrono::Utc::now().timestamp_micros();
    let fillers: Vec<Note> = (0..500_u32)
        .map(|i| Note {
            id: uuid::Uuid::new_v4(),
            namespace: "local".to_string(),
            kind: "task".to_string(),
            status: "active".to_string(),
            name: Some(format!("filler-{i}")),
            content: format!("filler task {i}"),
            salience: None,
            decay_factor: None,
            expires_at: None,
            properties: Some(json!({"status": "inbox"})),
            created_at: now + i64::from(i),
            updated_at: now + i64::from(i),
            deleted_at: None,
        })
        .collect();
    note_store
        .upsert_notes(fillers)
        .await
        .expect("insert fillers");

    // Build pack AFTER storage is populated (same runtime).
    let fixture = pack(runtime);

    // Create the dependent task pointing to the ancient-done blocker.
    let blocker_full = blocker_id.as_hyphenated().to_string();
    let dep = assign(
        &fixture,
        json!({
            "title": "unblocked-by-ancient",
            "status": "next",
            "depends_on": [blocker_full]
        }),
    )
    .await;
    let dep_id = dep["full_id"].as_str().unwrap().to_string();

    // next() must include the dependent task: its blocker is done even though
    // it is outside the 500-task scan window.
    let result = fixture.dispatch("gtd.next", json!({})).await.unwrap();
    let found = result
        .as_array()
        .unwrap()
        .iter()
        .any(|t| t["title"].as_str() == Some("unblocked-by-ancient"));
    assert!(
        found,
        "next() must surface task whose done blocker is older than 500-task window; \
         result: {result:?}"
    );

    let _ = dep_id;
    let _ = PageRequest {
        offset: 0,
        limit: 1,
    }; // suppress unused import warning
}

/// Medium: Race regression — concurrent `complete()` from two OS threads.
///
/// Two tasks contend to complete the same task.  Exactly one must win and
/// exactly one must lose.  The loser must fail with the expected terminal-state
/// or rows-affected-0 conflict error, NOT a generic SQL / lock error.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_complete_two_threads_one_wins_one_loses_atomic() {
    use std::sync::Arc;

    let runtime = rt();
    let fixture = Arc::new(pack(runtime));

    let resp = assign(&fixture, json!({"title": "mt-race-task", "status": "next"})).await;
    let id = resp["full_id"].as_str().unwrap().to_string();

    // Barrier ensures both spawned tasks attempt complete() simultaneously.
    let barrier = Arc::new(tokio::sync::Barrier::new(2));

    let (tx_a, rx_a) = tokio::sync::oneshot::channel::<Result<Value, RuntimeError>>();
    let (tx_b, rx_b) = tokio::sync::oneshot::channel::<Result<Value, RuntimeError>>();

    let pack_a = fixture.clone();
    let pack_b = fixture.clone();
    let bar_a = barrier.clone();
    let bar_b = barrier.clone();
    let id_a = id.clone();
    let id_b = id.clone();

    tokio::spawn(async move {
        bar_a.wait().await;
        let res = pack_a
            .dispatch("gtd.complete", json!({"id": id_a, "result": "thread-A"}))
            .await;
        let _ = tx_a.send(res);
    });

    tokio::spawn(async move {
        bar_b.wait().await;
        let res = pack_b
            .dispatch("gtd.complete", json!({"id": id_b, "result": "thread-B"}))
            .await;
        let _ = tx_b.send(res);
    });

    let res_a = rx_a.await.expect("thread A result");
    let res_b = rx_b.await.expect("thread B result");

    let successes = [res_a.is_ok(), res_b.is_ok()]
        .iter()
        .filter(|&&ok| ok)
        .count();
    let failures = [res_a.is_err(), res_b.is_err()]
        .iter()
        .filter(|&&e| e)
        .count();

    assert_eq!(
        successes, 1,
        "exactly one complete() must succeed in concurrent race; got {successes} successes"
    );
    assert_eq!(
        failures, 1,
        "exactly one complete() must fail in concurrent race; got {failures} failures"
    );

    // The loser must fail with the expected conflict error — terminal-state
    // rejection or rows_affected==0 guard — NOT a generic storage/lock error.
    let loser_err = match (res_a, res_b) {
        (Err(e), _) => e,
        (_, Err(e)) => e,
        _ => panic!("expected exactly one failure; both succeeded"),
    };
    let msg = loser_err.to_string();
    assert!(
        msg.contains("terminal state") || msg.contains("rows_affected"),
        "losing complete() must fail with terminal-state or rows_affected conflict; got: {msg}"
    );
}

// ── #522 regression: complete after explicit transition to active ─────────────

/// Regression for #522: assign → transition(active) → gtd.complete must succeed.
#[tokio::test]
async fn complete_after_transition_to_active_succeeds() {
    let pack = pack(rt());
    let resp = assign(&pack, json!({"title": "transition then complete"})).await;
    let id = resp["full_id"].as_str().unwrap().to_string();

    pack.dispatch("gtd.transition", json!({"id": id, "status": "active"}))
        .await
        .expect("transition to active must succeed");

    let done = pack
        .dispatch("gtd.complete", json!({"id": id}))
        .await
        .expect("complete from active after transition must succeed");

    assert_eq!(done["completed"].as_bool(), Some(true));
    assert_eq!(done["from"].as_str(), Some("active"));
    assert_eq!(done["to"].as_str(), Some("done"));
    assert_eq!(done["is_terminal"].as_bool(), Some(true));
    let completed_at = done["completed_at"]
        .as_str()
        .expect("completed_at must be present");
    chrono::DateTime::parse_from_rfc3339(completed_at)
        .unwrap_or_else(|e| panic!("completed_at not RFC 3339: {completed_at} - {e}"));
}

// ── #520 regression: context_entity_id on gtd.assign ─────────────────────────

/// Regression for #520: context_entity_id round-trips through assign, tasks, and get.
#[tokio::test]
async fn assign_context_entity_id_round_trips_through_tasks_and_get() {
    let rt = rt();
    let pack = pack(rt);

    let entity = pack
        .dispatch(
            "create",
            json!({"kind": "concept", "name": "Context Entity"}),
        )
        .await
        .expect("context entity create must succeed");
    let context_id = entity["id"].as_str().unwrap().to_string();

    let assigned = assign(
        &pack,
        json!({"title": "task with context", "context_entity_id": context_id}),
    )
    .await;
    let task_id = assigned["full_id"].as_str().unwrap().to_string();

    assert_eq!(
        assigned["context_entity_id"].as_str(),
        Some(context_id.as_str())
    );
    assert_eq!(
        assigned["properties"]["context_entity_id"].as_str(),
        Some(context_id.as_str())
    );

    let tasks = pack
        .dispatch("gtd.tasks", json!({"status": "inbox"}))
        .await
        .expect("tasks listing must succeed");
    let task = tasks
        .as_array()
        .unwrap()
        .iter()
        .find(|task| task["full_id"].as_str() == Some(task_id.as_str()))
        .expect("created task must be in tasks(status=inbox)");
    assert_eq!(
        task["context_entity_id"].as_str(),
        Some(context_id.as_str())
    );
    assert_eq!(
        task["properties"]["context_entity_id"].as_str(),
        Some(context_id.as_str())
    );

    let got = pack
        .dispatch("get", json!({"id": task_id}))
        .await
        .expect("get task must succeed");
    assert_eq!(
        got["properties"]["context_entity_id"].as_str(),
        Some(context_id.as_str())
    );

    let neighbors = pack
        .dispatch("neighbors", json!({"id": task_id, "direction": "out"}))
        .await
        .expect("neighbors must succeed");
    let has_annotates_edge = neighbors.as_array().unwrap().iter().any(|n| {
        n.to_string().contains("annotates") && n.to_string().contains(context_id.as_str())
    });
    assert!(
        has_annotates_edge,
        "task should have an annotates edge to the context entity; neighbors: {neighbors}"
    );
}

/// Regression for #520: malformed context_entity_id must produce a clear error.
#[tokio::test]
async fn assign_rejects_malformed_context_entity_id() {
    let pack = pack(rt());
    let err = pack
        .dispatch(
            "gtd.assign",
            json!({"title": "bad context", "context_entity_id": "not-a-uuid"}),
        )
        .await
        .unwrap_err();

    let msg = err.to_string();
    assert!(
        msg.contains("context_entity_id"),
        "error must name the bad field; got: {msg}"
    );
    assert!(
        msg.contains("full UUID"),
        "error must explain expected shape; got: {msg}"
    );
    assert!(
        msg.contains("not-a-uuid"),
        "error must echo the malformed value; got: {msg}"
    );
}

/// Regression for GTD-AUD-006: `gtd.next` must produce a stable, deterministic
/// ordering even when tasks share the same priority and `created_at` timestamp.
/// The final tie-breaker is UUID ascending so callers always observe the same order.
#[tokio::test]
async fn next_ordering_is_deterministic_on_equal_priority_and_timestamp() {
    let pack = pack(rt());

    // Create two tasks at the same priority. Because in-memory runtime uses
    // microsecond timestamps, rapid successive creates may produce the same
    // `created_at`. We create several and verify that repeated calls to
    // `gtd.next` return them in the same order every time.
    for title in &["task-a", "task-b", "task-c"] {
        assign(
            &pack,
            json!({"title": title, "status": "next", "priority": "p1"}),
        )
        .await;
    }

    let first = pack.dispatch("gtd.next", json!({})).await.unwrap();
    let second = pack.dispatch("gtd.next", json!({})).await.unwrap();

    assert_eq!(
        first, second,
        "gtd.next must return identical ordering on repeated calls with the same task set"
    );
}

// ── Secret-gate regression tests ─────────────────────────────────────────────

fn is_secret_detected(err: &RuntimeError) -> bool {
    matches!(err, RuntimeError::SecretDetected(_))
}

/// gtd.complete with a credential in the result field must be rejected before
/// the task is loaded from storage.
#[tokio::test]
async fn complete_blocks_secret_in_result() {
    let pack = pack(rt());

    // Create and activate a task so we can attempt to complete it.
    let task = assign(
        &pack,
        json!({"title": "secret-result-task", "status": "next"}),
    )
    .await;
    let id = task["id"].as_str().expect("task id").to_owned();

    // Attempting to complete with a credential in result must be rejected.
    let result = pack
        .dispatch(
            "gtd.complete",
            json!({
                "id": id,
                "result": "Completed with key AKIAFAKEKEY000000000", // gitleaks:allow
            }),
        )
        .await;
    assert!(
        result.as_ref().err().is_some_and(is_secret_detected),
        "gtd.complete with secret in result must be rejected; got: {result:?}"
    );
}

// ── Issue #772: fixed-window pre-fetch-then-filter regression tests ─────────

/// `gtd.next` must surface an actionable task even when 500+ newer,
/// non-actionable task notes exist. Before the fix, `handle_next` pre-fetched
/// only the newest 500 task notes (unfiltered) via `list_notes` and applied
/// the actionable-status filter afterward in Rust — an older `next`/`active`
/// task falling outside that fixed window was silently invisible regardless
/// of priority.
#[tokio::test]
async fn next_finds_actionable_task_older_than_fixed_window() {
    use khive_storage::note::Note;

    let runtime = rt();
    let token = runtime
        .authorize(khive_runtime::Namespace::local())
        .unwrap();
    let note_store = runtime.notes(&token).expect("note store");

    // An old, actionable p0 task — created long before any filler task.
    let old_ts = chrono::Utc::now().timestamp_micros() - 1_000_000_000_000; // ~11 days ago
    let old_task = Note {
        id: uuid::Uuid::new_v4(),
        namespace: "local".to_string(),
        kind: "task".to_string(),
        status: "active".to_string(),
        name: Some("ancient-p0".to_string()),
        content: "ancient p0 task".to_string(),
        salience: None,
        decay_factor: None,
        expires_at: None,
        properties: Some(json!({"status": "next", "priority": "p0"})),
        created_at: old_ts,
        updated_at: old_ts,
        deleted_at: None,
    };
    note_store
        .upsert_note(old_task)
        .await
        .expect("insert old task");

    // Pad with 501 newer, non-actionable (inbox) task notes — more than the
    // legacy fixed 500-row window, none of which are actionable themselves.
    let now = chrono::Utc::now().timestamp_micros();
    let fillers: Vec<Note> = (0..501_u32)
        .map(|i| Note {
            id: uuid::Uuid::new_v4(),
            namespace: "local".to_string(),
            kind: "task".to_string(),
            status: "active".to_string(),
            name: Some(format!("filler-{i}")),
            content: format!("filler task {i}"),
            salience: None,
            decay_factor: None,
            expires_at: None,
            properties: Some(json!({"status": "inbox"})),
            created_at: now + i64::from(i),
            updated_at: now + i64::from(i),
            deleted_at: None,
        })
        .collect();
    note_store
        .upsert_notes(fillers)
        .await
        .expect("insert fillers");

    let pack = pack(runtime);
    let result = pack.dispatch("gtd.next", json!({})).await.unwrap();
    let arr = result.as_array().unwrap();
    assert!(
        arr.iter()
            .any(|t| t["title"].as_str() == Some("ancient-p0")),
        "gtd.next must surface an actionable p0 task older than 500 newer \
         non-actionable tasks (issue #772); result: {result:?}"
    );
    assert_eq!(
        arr[0]["title"], "ancient-p0",
        "the ancient p0 task must sort first by priority"
    );
}

/// `gtd.tasks(status="done")` must surface a done task even when 500+ newer,
/// non-done task notes exist. Before the fix, `handle_tasks` pre-fetched
/// `offset + limit + 500` task notes (unfiltered, always from offset 0) via
/// `list_notes` and applied the status filter afterward in Rust — an older
/// `done` task falling outside that window was silently invisible.
#[tokio::test]
async fn tasks_finds_done_task_older_than_fixed_window() {
    use khive_storage::note::Note;

    let runtime = rt();
    let token = runtime
        .authorize(khive_runtime::Namespace::local())
        .unwrap();
    let note_store = runtime.notes(&token).expect("note store");

    let old_ts = chrono::Utc::now().timestamp_micros() - 1_000_000_000_000;
    let old_task = Note {
        id: uuid::Uuid::new_v4(),
        namespace: "local".to_string(),
        kind: "task".to_string(),
        status: "active".to_string(),
        name: Some("ancient-done".to_string()),
        content: "ancient done task".to_string(),
        salience: None,
        decay_factor: None,
        expires_at: None,
        properties: Some(json!({"status": "done"})),
        created_at: old_ts,
        updated_at: old_ts,
        deleted_at: None,
    };
    note_store
        .upsert_note(old_task)
        .await
        .expect("insert old done task");

    // The legacy default window was offset(0) + limit(50) + 500 = 550; 600
    // newer non-done fillers exceed that so the always-offset-0 fetch never
    // reached the ancient done task.
    let now = chrono::Utc::now().timestamp_micros();
    let fillers: Vec<Note> = (0..600_u32)
        .map(|i| Note {
            id: uuid::Uuid::new_v4(),
            namespace: "local".to_string(),
            kind: "task".to_string(),
            status: "active".to_string(),
            name: Some(format!("filler-{i}")),
            content: format!("filler task {i}"),
            salience: None,
            decay_factor: None,
            expires_at: None,
            properties: Some(json!({"status": "inbox"})),
            created_at: now + i64::from(i),
            updated_at: now + i64::from(i),
            deleted_at: None,
        })
        .collect();
    note_store
        .upsert_notes(fillers)
        .await
        .expect("insert fillers");

    let pack = pack(runtime);
    let result = pack
        .dispatch("gtd.tasks", json!({"status": "done"}))
        .await
        .unwrap();
    let arr = result.as_array().unwrap();
    assert!(
        arr.iter()
            .any(|t| t["title"].as_str() == Some("ancient-done")),
        "gtd.tasks(status=\"done\") must surface a done task older than the \
         legacy fixed pre-fetch window (issue #772); result: {result:?}"
    );
}

/// #772 follow-up (Major finding): when more tasks match the actionable
/// filter than the scan safety bound covers, `gtd.next` must return an
/// explicit error asking the caller to narrow the query instead of silently
/// sorting and truncating a partial candidate set — a partial set can hide
/// an older, higher-priority task that fell outside the scan window.
#[tokio::test]
async fn next_returns_explicit_error_when_matches_exceed_scan_bound() {
    use khive_storage::note::Note;

    let runtime = rt();
    let token = runtime
        .authorize(khive_runtime::Namespace::local())
        .unwrap();
    let note_store = runtime.notes(&token).expect("note store");

    let now = chrono::Utc::now().timestamp_micros();
    let tasks: Vec<Note> = (0..20_001_u32)
        .map(|i| Note {
            id: uuid::Uuid::new_v4(),
            namespace: "local".to_string(),
            kind: "task".to_string(),
            status: "active".to_string(),
            name: Some(format!("task-{i}")),
            content: format!("task {i}"),
            salience: None,
            decay_factor: None,
            expires_at: None,
            properties: Some(json!({"status": "next"})),
            created_at: now + i64::from(i),
            updated_at: now + i64::from(i),
            deleted_at: None,
        })
        .collect();
    note_store
        .upsert_notes(tasks)
        .await
        .expect("insert tasks over scan bound");

    let pack = pack(runtime);
    let err = pack
        .dispatch("gtd.next", json!({}))
        .await
        .expect_err("gtd.next must reject a query matching more rows than the scan bound covers");
    let msg = err.to_string();
    assert!(
        msg.contains("exceeds") && msg.contains("scan bound"),
        "error must explain the scan bound was exceeded; got: {msg}"
    );
}

/// #825 boundary test: exactly `TASK_SCAN_MAX_ROWS` (20,000) matching
/// rows must succeed — the bound is "reject when more than 20,000 rows
/// match", not "reject at or above 20,000".
#[tokio::test]
async fn next_succeeds_when_matches_exactly_at_scan_bound() {
    use khive_storage::note::Note;

    let runtime = rt();
    let token = runtime
        .authorize(khive_runtime::Namespace::local())
        .unwrap();
    let note_store = runtime.notes(&token).expect("note store");

    let now = chrono::Utc::now().timestamp_micros();
    let tasks: Vec<Note> = (0..20_000_u32)
        .map(|i| Note {
            id: uuid::Uuid::new_v4(),
            namespace: "local".to_string(),
            kind: "task".to_string(),
            status: "active".to_string(),
            name: Some(format!("task-{i}")),
            content: format!("task {i}"),
            salience: None,
            decay_factor: None,
            expires_at: None,
            properties: Some(json!({"status": "next"})),
            created_at: now + i64::from(i),
            updated_at: now + i64::from(i),
            deleted_at: None,
        })
        .collect();
    note_store
        .upsert_notes(tasks)
        .await
        .expect("insert tasks at scan bound");

    let pack = pack(runtime);
    let result = pack
        .dispatch("gtd.next", json!({"limit": 5}))
        .await
        .expect("gtd.next must succeed when matches are exactly at the scan bound");
    let arr = result.as_array().unwrap();
    assert_eq!(
        arr.len(),
        5,
        "gtd.next must still honor the requested limit"
    );
}

/// Regression for a push-down bug where an explicit `status="inbox"` filter
/// used a plain `Eq` predicate against `json_extract(properties, '$.status')`.
/// `json_extract` on a legacy row with no stored `status` key evaluates to
/// SQL `NULL`, which `Eq` never matches, even though every other code path
/// (`task_status`, `render_task`) treats a missing `status` as `"inbox"`.
/// The filter must use `EqOrMissing` so `status="inbox"` also surfaces tasks
/// that predate the `status` property being written at all.
#[tokio::test]
async fn tasks_status_inbox_filter_matches_legacy_task_missing_status_property() {
    use khive_storage::note::Note;

    let runtime = rt();
    let token = runtime
        .authorize(khive_runtime::Namespace::local())
        .unwrap();
    let note_store = runtime.notes(&token).expect("note store");

    let now = chrono::Utc::now().timestamp_micros();
    let legacy_task = Note {
        id: uuid::Uuid::new_v4(),
        namespace: "local".to_string(),
        kind: "task".to_string(),
        status: "active".to_string(),
        name: Some("legacy-no-status".to_string()),
        content: "task predating the status property".to_string(),
        salience: None,
        decay_factor: None,
        expires_at: None,
        properties: Some(json!({})),
        created_at: now,
        updated_at: now,
        deleted_at: None,
    };
    note_store
        .upsert_note(legacy_task)
        .await
        .expect("insert legacy task");

    let pack = pack(runtime);
    let result = pack
        .dispatch("gtd.tasks", json!({"status": "inbox"}))
        .await
        .unwrap();
    let arr = result.as_array().unwrap();
    assert!(
        arr.iter()
            .any(|t| t["title"].as_str() == Some("legacy-no-status")),
        "gtd.tasks(status=\"inbox\") must surface a legacy task with no stored \
         status property, since a missing status defaults to inbox everywhere \
         else; result: {result:?}"
    );
}

/// Same bug as above, for `priority="p2"`: a legacy task with no stored
/// `priority` property renders as `p2` (`priority_rank`, `render_task`), so
/// an explicit `priority="p2"` filter must also match it via `EqOrMissing`.
#[tokio::test]
async fn tasks_priority_p2_filter_matches_legacy_task_missing_priority_property() {
    use khive_storage::note::Note;

    let runtime = rt();
    let token = runtime
        .authorize(khive_runtime::Namespace::local())
        .unwrap();
    let note_store = runtime.notes(&token).expect("note store");

    let now = chrono::Utc::now().timestamp_micros();
    let legacy_task = Note {
        id: uuid::Uuid::new_v4(),
        namespace: "local".to_string(),
        kind: "task".to_string(),
        status: "active".to_string(),
        name: Some("legacy-no-priority".to_string()),
        content: "task predating the priority property".to_string(),
        salience: None,
        decay_factor: None,
        expires_at: None,
        properties: Some(json!({"status": "next"})),
        created_at: now,
        updated_at: now,
        deleted_at: None,
    };
    note_store
        .upsert_note(legacy_task)
        .await
        .expect("insert legacy task");

    let pack = pack(runtime);
    let result = pack
        .dispatch("gtd.tasks", json!({"priority": "p2"}))
        .await
        .unwrap();
    let arr = result.as_array().unwrap();
    assert!(
        arr.iter()
            .any(|t| t["title"].as_str() == Some("legacy-no-priority")),
        "gtd.tasks(priority=\"p2\") must surface a legacy task with no stored \
         priority property, since a missing priority renders as p2 everywhere \
         else; result: {result:?}"
    );
}

/// `gtd.tasks` pages must not overlap, and must stay complete, even when
/// 500+ newer *non-matching* task notes exist alongside the matching set.
///
/// A weaker version of this test (matching rows only, no filler) would still
/// pass under the pre-#772 implementation: with nothing to fill the old
/// fixed-size unfiltered pre-fetch window, plain offset slicing over an
/// all-matching set can look correct by coincidence. This version plants
/// 600 newer `done` filler rows (excluded by the default status filter) so
/// the old "pre-fetch `offset + limit + 500` unfiltered rows, then filter in
/// Rust" behavior would have its window consumed by fillers and either drop
/// matching rows or return short/overlapping pages. Real SQL-side
/// `LIMIT`/`OFFSET` pagination over the pushed-down filter must still return
/// full, disjoint pages containing only the expected matching records.
#[tokio::test]
async fn tasks_pagination_returns_disjoint_pages() {
    use khive_storage::note::Note;
    use std::collections::HashSet;

    let runtime = rt();
    let token = runtime
        .authorize(khive_runtime::Namespace::local())
        .unwrap();
    let note_store = runtime.notes(&token).expect("note store");

    // Matching set: 60 non-terminal tasks with older timestamps.
    let base_ts = chrono::Utc::now().timestamp_micros() - 1_000_000_000_000;
    let tasks: Vec<Note> = (0..60_u32)
        .map(|i| Note {
            id: uuid::Uuid::new_v4(),
            namespace: "local".to_string(),
            kind: "task".to_string(),
            status: "active".to_string(),
            name: Some(format!("task-{i}")),
            content: format!("task {i}"),
            salience: None,
            decay_factor: None,
            expires_at: None,
            properties: Some(json!({"status": "next"})),
            created_at: base_ts + i64::from(i),
            updated_at: base_ts + i64::from(i),
            deleted_at: None,
        })
        .collect();
    let expected_ids: HashSet<uuid::Uuid> = tasks.iter().map(|t| t.id).collect();
    note_store.upsert_notes(tasks).await.expect("insert tasks");

    // Non-matching filler: 600 `done` tasks, all newer than the matching set,
    // excluded by `gtd.tasks`' default status filter (done/cancelled).
    let now = chrono::Utc::now().timestamp_micros();
    let fillers: Vec<Note> = (0..600_u32)
        .map(|i| Note {
            id: uuid::Uuid::new_v4(),
            namespace: "local".to_string(),
            kind: "task".to_string(),
            status: "done".to_string(),
            name: Some(format!("filler-{i}")),
            content: format!("filler task {i}"),
            salience: None,
            decay_factor: None,
            expires_at: None,
            properties: Some(json!({"status": "done"})),
            created_at: now + i64::from(i),
            updated_at: now + i64::from(i),
            deleted_at: None,
        })
        .collect();
    note_store
        .upsert_notes(fillers)
        .await
        .expect("insert fillers");

    let pack = pack(runtime);
    let page1 = pack
        .dispatch("gtd.tasks", json!({"limit": 20, "offset": 0}))
        .await
        .unwrap();
    let page2 = pack
        .dispatch("gtd.tasks", json!({"limit": 20, "offset": 20}))
        .await
        .unwrap();
    let page3 = pack
        .dispatch("gtd.tasks", json!({"limit": 20, "offset": 40}))
        .await
        .unwrap();

    let ids1: HashSet<&str> = page1
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["full_id"].as_str().unwrap())
        .collect();
    let ids2: HashSet<&str> = page2
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["full_id"].as_str().unwrap())
        .collect();
    let ids3: HashSet<&str> = page3
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["full_id"].as_str().unwrap())
        .collect();

    assert_eq!(ids1.len(), 20, "page1 must be full; got {ids1:?}");
    assert_eq!(ids2.len(), 20, "page2 must be full; got {ids2:?}");
    assert_eq!(ids3.len(), 20, "page3 must be full; got {ids3:?}");
    assert!(
        ids1.is_disjoint(&ids2) && ids2.is_disjoint(&ids3) && ids1.is_disjoint(&ids3),
        "pages at different offsets must not overlap (issue #772 offset-0 \
         refetch bug); page1={ids1:?} page2={ids2:?} page3={ids3:?}"
    );

    let all_returned: HashSet<uuid::Uuid> = ids1
        .iter()
        .chain(ids2.iter())
        .chain(ids3.iter())
        .map(|s| uuid::Uuid::parse_str(s).unwrap())
        .collect();
    assert_eq!(
        all_returned.len(),
        60,
        "all 60 matching tasks must be covered across the 3 pages; got {all_returned:?}"
    );
    assert!(
        all_returned.is_subset(&expected_ids),
        "returned tasks must be exactly the matching set, no filler rows leaked in"
    );
}
