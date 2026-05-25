//! End-to-end tests for the GTD pack against an in-memory runtime.

use khive_pack_gtd::GtdPack;
use khive_pack_kg::KgPack;
use khive_runtime::pack::HandlerDef;
use khive_runtime::{
    KhiveRuntime, Namespace, NoteKindSpec, SchemaPlan, RuntimeError, VerbRegistry,
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
    assert!(!plan.is_empty(), "GtdPack must return a non-empty SchemaPlan");
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
