//! Dependency decisions and actual SQLite work through the registered GTD hooks.

mod common;

use common::{assign, pack, rt};
use khive_runtime::Namespace;
use khive_storage::note::Note;
use serde_json::{json, Value};
use uuid::Uuid;

fn task(id: Uuid, dependencies: &[Uuid]) -> Note {
    let mut note = Note::new("local", "task", "dependency fixture");
    note.id = id;
    note.properties = Some(json!({"status": "next", "depends_on": dependencies}));
    note
}

fn full_batches(statements: &[String]) -> usize {
    statements
        .iter()
        .filter(|sql| {
            sql.starts_with("SELECT id, namespace, kind, status,")
                && sql.contains("FROM notes WHERE id IN (")
        })
        .count()
}

fn visibility_batches(statements: &[String]) -> usize {
    statements
        .iter()
        .filter(|sql| sql.starts_with("SELECT id, namespace, deleted_at FROM notes WHERE id IN ("))
        .count()
}

fn point_notes(statements: &[String]) -> usize {
    statements
        .iter()
        .filter(|sql| sql.contains("FROM notes WHERE id = ?1"))
        .count()
}

#[tokio::test]
async fn dependency_diagnostics_batch_missing_ids_preserve_every_blocker_state() {
    let runtime = rt();
    let fixture = pack(runtime.clone());
    let token = runtime.authorize(Namespace::local()).expect("local token");
    let store = runtime.notes(&token).expect("notes");
    let ids: Vec<_> = (0..7).map(|_| Uuid::new_v4()).collect();
    let missing: Vec<_> = (0..901).map(|_| Uuid::new_v4()).collect();
    let mut notes: Vec<_> = ids.iter().map(|id| task(*id, &[])).collect();
    notes[0].properties = Some(json!({"status": "done"}));
    notes[1].properties = Some(json!({"status": "active"}));
    notes[2].properties = Some(json!({"status": "cancelled"}));
    notes[3].kind = "observation".into();
    notes[4].properties = Some(json!({"status": "legacy-invalid"}));
    notes[5].deleted_at = Some(1);
    notes[6].namespace = "other".into();
    notes[6].deleted_at = Some(1);
    let mut dependencies: Vec<Value> = ids.iter().map(|id| json!(id)).collect();
    dependencies.extend(missing.iter().map(|id| json!(id)));
    dependencies.extend([json!(42), json!("not-a-uuid")]);
    let mut dependent = task(Uuid::new_v4(), &[]);
    dependent.properties = Some(json!({
        "status": "next", "assignee": "dependency-reader", "depends_on": dependencies
    }));
    let dependent_id = dependent.id;
    notes.push(dependent);
    store
        .upsert_notes(notes)
        .await
        .expect("seed persisted notes");

    let args = json!({"assignee": "dependency-reader", "limit": 200});
    let before = fixture
        .dispatch("gtd.tasks", args.clone())
        .await
        .expect("warm tasks");
    let pool = runtime.backend().pool();
    let reader_before = pool.reader_acquisition_snapshot();
    let observation = pool
        .observe_test_statement_starts(2048)
        .expect("statement observation");
    let result = fixture.dispatch("gtd.tasks", args).await.expect("tasks");
    let starts = observation
        .started_statements()
        .expect("complete observation");
    let reader_after = pool.reader_acquisition_snapshot();
    drop(observation);
    let sql: Vec<_> = starts.iter().map(|start| start.sql.clone()).collect();

    assert_eq!(result, before, "observation must not change task decisions");
    let rows = result.as_array().expect("task array");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["full_id"], json!(dependent_id));
    assert_eq!(rows[0]["dependency_state"], "broken");
    assert_eq!(rows[0]["actionable"], false);
    let mut expected = vec![
        json!({"id": ids[1], "state": "pending", "status": "active"}),
        json!({"id": ids[2], "state": "cancelled"}),
        json!({"id": ids[3], "state": "wrong_kind"}),
        json!({"id": ids[4], "state": "invalid", "status": "legacy-invalid"}),
        json!({"id": ids[5], "state": "soft_deleted"}),
        json!({"id": ids[6], "state": "different_namespace"}),
    ];
    expected.extend(
        missing
            .iter()
            .map(|id| json!({"id": id, "state": "missing"})),
    );
    expected.extend([
        json!({"id": 42, "state": "invalid"}),
        json!({"id": "not-a-uuid", "state": "invalid"}),
    ]);
    assert_eq!(
        rows[0]["blocked_by"],
        json!(expected),
        "input order and namespace precedence"
    );
    assert_eq!(
        full_batches(&sql),
        2,
        "908 dependency IDs cross the 900-ID chunk"
    );
    assert_eq!(
        visibility_batches(&sql),
        2,
        "903 missing-from-live IDs use a projection batch"
    );
    assert_eq!(
        point_notes(&sql),
        0,
        "missing and deleted dependencies cannot cause point reads"
    );
    assert_eq!(
        reader_after.pooled_checkouts - reader_before.pooled_checkouts,
        5
    );
    assert!(starts
        .iter()
        .filter(|start| start.sql.contains("FROM notes"))
        .all(|start| start.readonly));
}

#[tokio::test]
async fn property_cycle_walk_batches_each_frontier_for_shared_ancestors() {
    let runtime = rt();
    let fixture = pack(runtime.clone());
    let token = runtime.authorize(Namespace::local()).expect("local token");
    let store = runtime.notes(&token).expect("notes");
    let created = assign(
        &fixture,
        json!({"title": "shared ancestor dependency source"}),
    )
    .await;
    let source = store
        .get_note(Uuid::parse_str(created["full_id"].as_str().expect("source ID")).expect("UUID"))
        .await
        .expect("load assigned source")
        .expect("source exists");
    let tail = task(Uuid::new_v4(), &[]);
    let ancestor = task(Uuid::new_v4(), &[tail.id]);
    let branches: Vec<_> = (0..4)
        .map(|_| task(Uuid::new_v4(), &[ancestor.id]))
        .collect();
    let branch_ids: Vec<_> = branches.iter().map(|note| note.id).collect();
    let roots: Vec<_> = (0..3).map(|_| task(Uuid::new_v4(), &branch_ids)).collect();
    let root_ids: Vec<_> = roots.iter().map(|note| note.id).collect();
    let mut notes = vec![source.clone(), tail.clone(), ancestor];
    notes.extend(branches);
    notes.extend(roots);
    store
        .upsert_notes(notes)
        .await
        .expect("seed shared-ancestor graph");

    let properties = json!({"depends_on": root_ids});
    let pool = runtime.backend().pool();
    let reader_before = pool.reader_acquisition_snapshot();
    let observation = pool
        .observe_test_statement_starts(128)
        .expect("statement observation");
    fixture
        .registry
        .validate_note_update_hook(&runtime, &token, &source, Some(&properties))
        .await
        .expect("each independent walk is acyclic");
    let starts = observation
        .started_statements()
        .expect("complete observation");
    let reader_after = pool.reader_acquisition_snapshot();
    drop(observation);
    let sql: Vec<_> = starts.iter().map(|start| start.sql.clone()).collect();
    assert_eq!(
        full_batches(&sql),
        15,
        "three root guards plus four frontiers per independent walk"
    );
    assert_eq!(
        point_notes(&sql),
        0,
        "shared ancestors are not hydrated per node"
    );
    assert_eq!(visibility_batches(&sql), 0);
    assert_eq!(
        reader_after.pooled_checkouts - reader_before.pooled_checkouts,
        15
    );
    assert!(starts.iter().all(|start| start.readonly));

    fixture
        .dispatch("update", json!({"id": source.id, "properties": properties}))
        .await
        .expect("real update accepts the same dependency graph");
    let persisted = store
        .get_note(source.id)
        .await
        .expect("load source")
        .expect("source exists");
    assert_eq!(
        persisted.properties.as_ref().unwrap()["depends_on"],
        json!(root_ids)
    );
    let error = fixture
        .dispatch(
            "update",
            json!({
                "id": tail.id, "properties": {"depends_on": [source.id]}
            }),
        )
        .await
        .expect_err("real update refuses the proposed transitive back edge");
    assert_eq!(
        error.to_string(),
        format!(
            "invalid input: depends_on update would create a dependency cycle: blocker {} already reaches task {}",
            source.id, tail.id
        )
    );
}

#[tokio::test]
async fn dependency_walk_budget_is_independent_for_each_declared_blocker() {
    let runtime = rt();
    let fixture = pack(runtime.clone());
    let token = runtime.authorize(Namespace::local()).expect("local token");
    let store = runtime.notes(&token).expect("notes");
    let source = task(Uuid::new_v4(), &[]);
    let tail = task(Uuid::new_v4(), &[]);
    let ancestor = task(Uuid::new_v4(), &[tail.id]);
    let mut roots = Vec::new();
    for _ in 0..2 {
        let mut dependencies: Vec<_> = (0..10_000).map(|_| Uuid::new_v4()).collect();
        dependencies.push(ancestor.id);
        roots.push(task(Uuid::new_v4(), &dependencies));
    }
    let root_ids: Vec<_> = roots.iter().map(|note| note.id).collect();
    let mut notes = vec![source.clone(), tail, ancestor];
    notes.extend(roots);
    store
        .upsert_notes(notes)
        .await
        .expect("seed independent broad walks");
    fixture
        .dispatch(
            "update",
            json!({
                "id": source.id, "properties": {"depends_on": root_ids}
            }),
        )
        .await
        .expect("each walk visits 10003 IDs, not a cumulative 20004-ID walk");
    let persisted = store
        .get_note(source.id)
        .await
        .expect("load source")
        .expect("source exists");
    assert_eq!(
        persisted.properties.as_ref().unwrap()["depends_on"],
        json!(root_ids)
    );
}

#[tokio::test]
async fn dependency_walk_safety_errors_keep_edge_and_task_bounds_and_goal_order() {
    let runtime = rt();
    let fixture = pack(runtime.clone());
    let token = runtime.authorize(Namespace::local()).expect("local token");
    let store = runtime.notes(&token).expect("notes");
    let source = task(Uuid::new_v4(), &[]);
    let root = task(Uuid::new_v4(), &[]);
    let overflowing = task(Uuid::new_v4(), &vec![Uuid::new_v4(); 20_001]);
    let mut root = root;
    root.properties = Some(json!({"status": "next", "depends_on": [overflowing.id, source.id]}));
    store
        .upsert_notes(vec![source.clone(), root.clone(), overflowing])
        .await
        .expect("seed edge limit");
    let args = json!({"id": source.id, "properties": {"depends_on": [root.id]}});
    let edge = fixture
        .dispatch("update", args.clone())
        .await
        .expect_err("earlier frontier node exceeds edge budget before goal");
    assert_eq!(
        edge.to_string(),
        "invalid input: depends_on cycle validation exceeded the 20000-edge safety bound"
    );

    root.properties = Some(
        json!({"status": "next", "depends_on": (0..20_000).map(|_| Uuid::new_v4()).collect::<Vec<_>>()}),
    );
    store
        .upsert_note(root)
        .await
        .expect("seed task limit without an edge overflow");
    let tasks = fixture
        .dispatch("update", args)
        .await
        .expect_err("root plus 20000 distinct missing IDs exceeds task budget");
    assert_eq!(
        tasks.to_string(),
        "invalid input: depends_on cycle validation exceeded the 20000-task safety bound"
    );
    let persisted = store
        .get_note(source.id)
        .await
        .expect("load unchanged source")
        .expect("source exists");
    assert_eq!(
        persisted.properties.as_ref().unwrap()["depends_on"],
        json!([])
    );
}
