//! Context metadata and its owned annotation commit as one guarded update.

mod common;

use common::{assign, pack, rt, Fixture};
use khive_runtime::atomic_prepare::prepare_update_from_note_snapshot;
use khive_runtime::{run_atomic_unit, AtomicRunOutcome, KhiveRuntime, Namespace, RuntimeError};
use khive_storage::{EdgeRelation, Note};
use serde_json::{json, Value};
use uuid::Uuid;

async fn entity(fixture: &Fixture, name: &str, namespace: &str) -> Uuid {
    let result = fixture
        .dispatch(
            "create",
            json!({
                "kind": "concept", "name": name, "namespace": namespace,
                "skip_dedup_check": true,
            }),
        )
        .await
        .unwrap();
    Uuid::parse_str(result["id"].as_str().unwrap()).unwrap()
}

async fn note(runtime: &KhiveRuntime, id: Uuid) -> Note {
    let token = runtime.authorize(Namespace::local()).unwrap();
    runtime
        .notes(&token)
        .unwrap()
        .get_note(id)
        .await
        .unwrap()
        .unwrap()
}

async fn annotation(runtime: &KhiveRuntime, ns: &str, from: Uuid, to: Uuid) -> Value {
    let token = runtime.authorize(Namespace::local()).unwrap();
    serde_json::to_value(
        runtime
            .get_edge_by_natural_key_including_deleted(
                &token,
                ns,
                from,
                to,
                EdgeRelation::Annotates,
            )
            .await
            .unwrap(),
    )
    .unwrap()
}

async fn update(
    fixture: &Fixture,
    runtime: &KhiveRuntime,
    atomic: bool,
    mut args: Value,
) -> Result<(), RuntimeError> {
    if !atomic {
        return fixture.dispatch("update", args).await.map(|_| ());
    }
    let token = runtime.authorize(Namespace::local()).unwrap();
    let id = Uuid::parse_str(args["id"].as_str().unwrap()).unwrap();
    let snapshot = note(runtime, id).await;
    fixture
        .registry
        .prepare_note_update_hook(runtime, &token, &snapshot, &mut args)
        .await?;
    let (_, plan) = prepare_update_from_note_snapshot(
        runtime,
        &token,
        &args,
        None,
        snapshot,
        &fixture.registry,
    )
    .await?;
    match run_atomic_unit(runtime.sql().as_ref(), vec![plan])
        .await
        .map_err(|error| RuntimeError::Internal(error.to_string()))?
    {
        AtomicRunOutcome::Committed { .. } => Ok(()),
        AtomicRunOutcome::RolledBack { failure, .. } => {
            Err(RuntimeError::Internal(format!("rolled back: {failure:?}")))
        }
    }
}

#[tokio::test]
async fn context_edges_move_clear_noop_and_first_set_through_both_update_paths() {
    for atomic in [false, true] {
        let runtime = rt();
        let fixture = pack(runtime.clone());
        let a = entity(&fixture, "context A", "local").await;
        let b = entity(&fixture, "context B", "local").await;
        let c = entity(&fixture, "unrelated annotation", "local").await;
        let task = assign(&fixture, json!({"title": "moving", "context_entity_id": a})).await;
        let id = Uuid::parse_str(task["full_id"].as_str().unwrap()).unwrap();
        let other = assign(&fixture, json!({"title": "other", "context_entity_id": a})).await;
        let other_id = Uuid::parse_str(other["full_id"].as_str().unwrap()).unwrap();
        fixture
            .dispatch(
                "link",
                json!({"source_id": id, "target_id": c,
            "relation": "annotates", "weight": 0.3, "metadata": {"keep": true}}),
            )
            .await
            .unwrap();
        let unrelated = annotation(&runtime, "local", id, c).await;
        let other_annotation = annotation(&runtime, "local", other_id, a).await;
        update(
            &fixture,
            &runtime,
            atomic,
            json!({"id": id,
            "properties": {"context_entity_id": b}}),
        )
        .await
        .unwrap();
        assert_eq!(
            note(&runtime, id).await.properties.unwrap()["context_entity_id"],
            b.to_string()
        );
        assert!(!annotation(&runtime, "local", id, a).await["deleted_at"].is_null());
        let b_edge = annotation(&runtime, "local", id, b).await;
        assert!(!b_edge.is_null());
        assert!(b_edge["deleted_at"].is_null());
        let same_note = note(&runtime, id).await;
        for spelling in [
            b.to_string(),
            b.to_string().to_uppercase(),
            b.simple().to_string(),
        ] {
            update(
                &fixture,
                &runtime,
                atomic,
                json!({"id": id,
                "properties": {"context_entity_id": spelling}}),
            )
            .await
            .unwrap();
            assert_eq!(note(&runtime, id).await, same_note);
            assert_eq!(annotation(&runtime, "local", id, b).await, b_edge);
        }
        update(
            &fixture,
            &runtime,
            atomic,
            json!({"id": id,
            "properties": {"unrelated": "allowed"}}),
        )
        .await
        .unwrap();
        assert_eq!(annotation(&runtime, "local", id, b).await, b_edge);
        update(
            &fixture,
            &runtime,
            atomic,
            json!({"id": id,
            "properties": {"context_entity_id": null}}),
        )
        .await
        .unwrap();
        assert_eq!(
            note(&runtime, id).await.properties.unwrap()["context_entity_id"],
            Value::Null
        );
        assert!(!annotation(&runtime, "local", id, b).await["deleted_at"].is_null());
        let cleared = note(&runtime, id).await;
        let cleared_edge = annotation(&runtime, "local", id, b).await;
        update(
            &fixture,
            &runtime,
            atomic,
            json!({"id": id,
            "properties": {"context_entity_id": null}}),
        )
        .await
        .unwrap();
        assert_eq!(note(&runtime, id).await, cleared);
        assert_eq!(annotation(&runtime, "local", id, b).await, cleared_edge);
        update(
            &fixture,
            &runtime,
            atomic,
            json!({"id": id,
            "properties": {"context_entity_id": a}}),
        )
        .await
        .unwrap();
        assert!(annotation(&runtime, "local", id, a).await["deleted_at"].is_null());
        assert_eq!(annotation(&runtime, "local", id, c).await, unrelated);
        assert_eq!(
            annotation(&runtime, "local", other_id, a).await,
            other_annotation
        );

        let fresh = assign(&fixture, json!({"title": "first context"})).await;
        let fresh_id = Uuid::parse_str(fresh["full_id"].as_str().unwrap()).unwrap();
        assert!(annotation(&runtime, "local", fresh_id, a).await.is_null());
        update(
            &fixture,
            &runtime,
            atomic,
            json!({"id": fresh_id,
            "properties": {"context_entity_id": a}}),
        )
        .await
        .unwrap();
        let first_edge = annotation(&runtime, "local", fresh_id, a).await;
        assert!(!first_edge.is_null());
        assert!(first_edge["deleted_at"].is_null());
    }
}

#[tokio::test]
async fn context_edges_preserve_an_existing_annotation_to_the_new_context() {
    for atomic in [false, true] {
        let runtime = rt();
        let fixture = pack(runtime.clone());
        let a = entity(&fixture, "A", "local").await;
        let b = entity(&fixture, "B", "local").await;
        let task = assign(
            &fixture,
            json!({"title": "existing annotation", "context_entity_id": a}),
        )
        .await;
        let id = Uuid::parse_str(task["full_id"].as_str().unwrap()).unwrap();
        fixture
            .dispatch(
                "link",
                json!({"source_id": id, "target_id": b,
            "relation": "annotates", "weight": 0.4, "metadata": {"authored": "separately"}}),
            )
            .await
            .unwrap();
        let before = annotation(&runtime, "local", id, b).await;
        let edge_id = before["id"].as_str().unwrap();
        runtime
            .sql()
            .writer()
            .await
            .unwrap()
            .execute_script(format!(
                "CREATE TRIGGER reject_annotation_rewrite BEFORE UPDATE ON graph_edges \
             WHEN OLD.id = '{edge_id}' BEGIN SELECT RAISE(ABORT, 'must preserve annotation'); END;"
            ))
            .await
            .unwrap();
        update(
            &fixture,
            &runtime,
            atomic,
            json!({"id": id,
            "properties": {"context_entity_id": b}}),
        )
        .await
        .unwrap();
        assert_eq!(annotation(&runtime, "local", id, b).await, before);
        assert!(!annotation(&runtime, "local", id, a).await["deleted_at"].is_null());
    }
}

#[tokio::test]
async fn context_edges_invalid_updates_leave_note_and_annotations_unchanged() {
    for atomic in [false, true] {
        let runtime = rt();
        let fixture = pack(runtime.clone());
        let a = entity(&fixture, "A", "local").await;
        let b = entity(&fixture, "B", "local").await;
        let foreign = entity(&fixture, "foreign", "foreign").await;
        let deleted = entity(&fixture, "deleted", "local").await;
        fixture
            .dispatch("delete", json!({"id": deleted}))
            .await
            .unwrap();
        let task = assign(
            &fixture,
            json!({"title": "unchanged", "context_entity_id": a}),
        )
        .await;
        let id = Uuid::parse_str(task["full_id"].as_str().unwrap()).unwrap();
        let before = note(&runtime, id).await;
        let before_edge = annotation(&runtime, "local", id, a).await;
        for invalid in [
            json!("invalid"),
            json!(42),
            json!(Uuid::new_v4()),
            json!(id),
            json!(foreign),
            json!(deleted),
        ] {
            update(
                &fixture,
                &runtime,
                atomic,
                json!({"id": id, "content": "must roll back",
                "properties": {"context_entity_id": invalid}}),
            )
            .await
            .expect_err("invalid reference");
            assert_eq!(note(&runtime, id).await, before);
            assert_eq!(annotation(&runtime, "local", id, a).await, before_edge);
        }
        update(
            &fixture,
            &runtime,
            atomic,
            json!({"id": id,
            "expected_version": before.version + 1,
            "properties": {"context_entity_id": b}}),
        )
        .await
        .expect_err("stale version");
        assert_eq!(note(&runtime, id).await, before);
        assert_eq!(annotation(&runtime, "local", id, a).await, before_edge);
        assert!(annotation(&runtime, "local", id, b).await.is_null());
    }
}

#[tokio::test]
async fn context_edges_missing_endpoint_at_apply_must_roll_back_note_and_old_edge() {
    for (atomic, reused) in [(false, false), (true, false), (false, true), (true, true)] {
        let runtime = rt();
        let fixture = pack(runtime.clone());
        let a = entity(&fixture, "A", "local").await;
        let b = entity(&fixture, "B", "local").await;
        let task = assign(
            &fixture,
            json!({"title": "rollback", "context_entity_id": a}),
        )
        .await;
        let id = Uuid::parse_str(task["full_id"].as_str().unwrap()).unwrap();
        if reused {
            fixture
                .dispatch(
                    "link",
                    json!({"source_id": id, "target_id": b,
                "relation": "annotates", "weight": 0.2, "metadata": {"keep": true}}),
                )
                .await
                .unwrap();
        }
        let before = note(&runtime, id).await;
        let before_edge = annotation(&runtime, "local", id, a).await;
        let before_b = annotation(&runtime, "local", id, b).await;
        // A deterministic transaction-boundary race: B exists throughout
        // validation/plan preparation, then disappears before the link applies.
        // If companions are omitted, this call succeeds and the test MUST fail.
        runtime
            .sql()
            .writer()
            .await
            .unwrap()
            .execute_script(format!(
                "CREATE TRIGGER remove_context_target AFTER UPDATE OF properties ON notes \
             WHEN NEW.id = '{id}' BEGIN UPDATE entities SET deleted_at=1 WHERE id='{b}'; END;"
            ))
            .await
            .unwrap();
        update(
            &fixture,
            &runtime,
            atomic,
            json!({"id": id,
            "properties": {"context_entity_id": b}}),
        )
        .await
        .expect_err("missing link endpoint must fail the entire update");
        assert_eq!(
            note(&runtime, id).await,
            before,
            "including version and timestamps"
        );
        assert_eq!(annotation(&runtime, "local", id, a).await, before_edge);
        assert_eq!(annotation(&runtime, "local", id, b).await, before_b);
        // The injected deletion was in the same transaction and also rolled back.
        fixture.dispatch("get", json!({"id": b})).await.unwrap();
    }
}

#[tokio::test]
async fn context_edges_cross_namespace_uuid_updates_keep_annotation_ownership_stable() {
    let runtime = rt();
    let fixture = pack(runtime.clone());
    let a = entity(&fixture, "foreign A", "foreign").await;
    let b = entity(&fixture, "local B", "local").await;
    let c = entity(&fixture, "local C", "local").await;
    let task = assign(
        &fixture,
        json!({"title": "foreign task", "namespace": "foreign",
        "context_entity_id": a}),
    )
    .await;
    let id = Uuid::parse_str(task["full_id"].as_str().unwrap()).unwrap();
    for next in [b, c] {
        fixture
            .dispatch(
                "update",
                json!({"id": id, "namespace": "local",
            "properties": {"context_entity_id": next}}),
            )
            .await
            .unwrap();
        let edge = annotation(&runtime, "foreign", id, next).await;
        assert!(!edge.is_null());
        assert!(edge["deleted_at"].is_null());
        assert!(annotation(&runtime, "local", id, next).await.is_null());
    }
    assert!(!annotation(&runtime, "foreign", id, a).await["deleted_at"].is_null());
    assert!(!annotation(&runtime, "foreign", id, b).await["deleted_at"].is_null());
    fixture
        .dispatch(
            "update",
            json!({"id": id, "namespace": "local",
        "properties": {"context_entity_id": null}}),
        )
        .await
        .unwrap();
    assert!(!annotation(&runtime, "foreign", id, c).await["deleted_at"].is_null());
}

#[tokio::test]
async fn shared_note_plan_preserves_canonical_short_id_with_omitted_fence() {
    let runtime = rt();
    let fixture = pack(runtime.clone());
    let target = entity(&fixture, "anchor", "local").await;
    let task = assign(&fixture, json!({"title": "short-ID task"})).await;
    let id = Uuid::parse_str(task["full_id"].as_str().unwrap()).unwrap();
    let prefix = id.simple().to_string()[..12].to_owned();
    fixture
        .dispatch(
            "update",
            json!({"id": prefix,
        "properties": {"context_entity_id": target}}),
        )
        .await
        .unwrap();
    assert!(!annotation(&runtime, "local", id, target).await.is_null());

    let ordinary = fixture
        .dispatch(
            "create",
            json!({"kind": "observation",
        "content": "ordinary note"}),
        )
        .await
        .unwrap();
    fixture
        .dispatch(
            "update",
            json!({"id": ordinary["id"],
        "content": "updated ordinary note", "properties": {"context_entity_id": "plain metadata"}}),
        )
        .await
        .unwrap();
    let stored = fixture
        .dispatch("get", json!({"id": ordinary["id"]}))
        .await
        .unwrap();
    assert_eq!(stored["content"], "updated ordinary note");
    assert_eq!(stored["properties"]["context_entity_id"], "plain metadata");
}

#[tokio::test]
async fn shared_note_plan_rejects_outer_null_fence_but_accepts_absence_predicate() {
    for atomic in [false, true] {
        let runtime = rt();
        let fixture = pack(runtime.clone());
        let a = entity(&fixture, "old context", "local").await;
        let b = entity(&fixture, "new context", "local").await;
        let task = assign(
            &fixture,
            json!({"title": "fence shape", "context_entity_id": a}),
        )
        .await;
        let id = Uuid::parse_str(task["full_id"].as_str().unwrap()).unwrap();
        let before_note = note(&runtime, id).await;
        let before_a = annotation(&runtime, "local", id, a).await;
        let before_b = annotation(&runtime, "local", id, b).await;
        let error = update(
            &fixture,
            &runtime,
            atomic,
            json!({"id": id, "fence": null, "properties": {"context_entity_id": b}}),
        )
        .await
        .expect_err("outer null is not an absent fence parameter");
        assert!(matches!(&error, RuntimeError::InvalidInput(_)), "{error}");
        assert!(
            error
                .to_string()
                .contains("fence requires an object or a non-empty list"),
            "{error}"
        );
        assert_eq!(note(&runtime, id).await, before_note);
        assert_eq!(annotation(&runtime, "local", id, a).await, before_a);
        assert_eq!(annotation(&runtime, "local", id, b).await, before_b);

        // ADR-172 A4 makes only an entry's expected_version nullable.
        update(&fixture, &runtime, atomic,
            json!({"id": id, "fence": {"kind": "observation", "key": "gtd/r3/absent", "expected_version": null},
                "properties": {"context_entity_id": b}}))
            .await.expect("an object asserting an absent key remains valid");
        let after_note = note(&runtime, id).await;
        assert_eq!(
            after_note.properties.as_ref().unwrap()["context_entity_id"],
            b.to_string()
        );
        assert!(!annotation(&runtime, "local", id, a).await["deleted_at"].is_null());
        let after_b = annotation(&runtime, "local", id, b).await;
        assert!(!after_b.is_null());
        assert!(after_b["deleted_at"].is_null());
    }
}

#[tokio::test]
async fn context_edges_retargeted_old_annotation_must_roll_back_the_whole_update() {
    for atomic in [false, true] {
        let runtime = rt();
        let fixture = pack(runtime.clone());
        let a = entity(&fixture, "old A", "local").await;
        let b = entity(&fixture, "new B", "local").await;
        let c = entity(&fixture, "retargeted C", "local").await;
        let task = assign(
            &fixture,
            json!({"title": "retarget race", "context_entity_id": a}),
        )
        .await;
        let id = Uuid::parse_str(task["full_id"].as_str().unwrap()).unwrap();
        let before = note(&runtime, id).await;
        let before_edge = annotation(&runtime, "local", id, a).await;
        let edge_id = before_edge["id"].as_str().unwrap();
        runtime
            .sql()
            .writer()
            .await
            .unwrap()
            .execute_script(format!(
                "CREATE TRIGGER retarget_old_context AFTER UPDATE OF properties ON notes \
             WHEN NEW.id = '{id}' BEGIN UPDATE graph_edges SET target_id='{c}', \
             updated_at=updated_at+1 WHERE id='{edge_id}'; END;"
            ))
            .await
            .unwrap();
        update(
            &fixture,
            &runtime,
            atomic,
            json!({"id": id,
            "properties": {"context_entity_id": b}}),
        )
        .await
        .expect_err("must not delete a retargeted annotation");
        assert_eq!(note(&runtime, id).await, before);
        assert_eq!(annotation(&runtime, "local", id, a).await, before_edge);
        assert!(annotation(&runtime, "local", id, b).await.is_null());
        assert!(annotation(&runtime, "local", id, c).await.is_null());
    }
}
