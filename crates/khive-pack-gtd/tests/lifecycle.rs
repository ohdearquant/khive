//! Tests for GTD lifecycle: transitions, terminal states, complete behavior.

mod common;

use common::{assign, pack, rt};
use khive_runtime::Namespace;
use serde_json::json;

#[tokio::test]
async fn complete_marks_task_done_and_is_idempotent_via_load_check() {
    let pack = pack(rt());
    let resp = assign(&pack, json!({"title": "do thing"})).await;
    let id = resp["full_id"].as_str().unwrap().to_string();

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
async fn transition_enforces_lifecycle_rules() {
    let pack = pack(rt());
    let resp = assign(&pack, json!({"title": "ship"})).await;
    let id = resp["full_id"].as_str().unwrap().to_string();

    let r = pack
        .dispatch("gtd.transition", json!({"id": id, "status": "active"}))
        .await
        .unwrap();
    assert_eq!(r["to"], "active");

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
async fn test_transition_from_done_rejected() {
    let pack = pack(rt());
    let resp = assign(&pack, json!({"title": "terminal done test"})).await;
    let id = resp["full_id"].as_str().unwrap().to_string();

    pack.dispatch("gtd.transition", json!({"id": id, "status": "done"}))
        .await
        .expect("transition to done must succeed");

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

#[tokio::test]
async fn test_transition_from_cancelled_rejected() {
    let pack = pack(rt());
    let resp = assign(&pack, json!({"title": "terminal cancelled test"})).await;
    let id = resp["full_id"].as_str().unwrap().to_string();

    pack.dispatch("gtd.transition", json!({"id": id, "status": "cancelled"}))
        .await
        .expect("transition to cancelled must succeed");

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

#[tokio::test]
async fn test_complete_on_already_done_returns_clear_error() {
    let pack = pack(rt());
    let resp = assign(&pack, json!({"title": "double complete test"})).await;
    let id = resp["full_id"].as_str().unwrap().to_string();

    pack.dispatch("gtd.transition", json!({"id": id, "status": "next"}))
        .await
        .expect("transition to next must succeed");

    let done = pack
        .dispatch("gtd.complete", json!({"id": id, "result": "shipped"}))
        .await
        .expect("first complete must succeed");
    assert_eq!(done["to"], "done");

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

#[tokio::test]
async fn complete_from_inbox_succeeds_per_lifecycle_table() {
    let pack = pack(rt());
    let resp = assign(&pack, json!({"title": "inbox task"})).await;
    let id = resp["full_id"].as_str().unwrap().to_string();
    assert_eq!(resp["status"], "inbox");

    let done = pack
        .dispatch("gtd.complete", json!({"id": id}))
        .await
        .expect("ADR-019 permits inbox -> done");
    assert_eq!(done["from"], "inbox");
    assert_eq!(done["to"], "done");
}

#[tokio::test]
async fn legacy_tasks_missing_status_can_transition_and_complete_from_semantic_inbox() {
    let runtime = rt();
    let token = runtime.authorize(Namespace::local()).unwrap();

    let mut transition_task = khive_storage::Note::new("local", "task", "legacy transition body");
    transition_task.name = Some("legacy transition body".to_string());
    transition_task.properties = Some(json!({
        "priority": "p2",
        "description": "legacy transition body",
    }));
    let transition_id = transition_task.id;
    let transition_revision = transition_task.updated_at;

    let mut complete_task = khive_storage::Note::new("local", "task", "legacy complete body");
    complete_task.name = Some("legacy complete body".to_string());
    complete_task.properties = Some(json!({
        "priority": "p2",
        "description": "legacy complete body",
    }));
    let complete_id = complete_task.id;
    let complete_revision = complete_task.updated_at;

    let store = runtime.notes(&token).expect("note store");
    store
        .upsert_note(transition_task)
        .await
        .expect("seed legacy transition task");
    store
        .upsert_note(complete_task)
        .await
        .expect("seed legacy complete task");

    let pack = pack(runtime.clone());
    let transitioned = pack
        .dispatch(
            "gtd.transition",
            json!({"id": transition_id.to_string(), "status": "next"}),
        )
        .await
        .expect("missing status must transition from semantic inbox");
    assert_eq!(transitioned["from"], "inbox");
    assert_eq!(transitioned["to"], "next");
    assert_eq!(transitioned["transitioned"], true);
    assert!(transitioned["audit_persisted"].is_boolean());

    let completed = pack
        .dispatch(
            "gtd.complete",
            json!({"id": complete_id.to_string(), "result": "legacy shipped"}),
        )
        .await
        .expect("missing status must complete from semantic inbox");
    assert_eq!(completed["from"], "inbox");
    assert_eq!(completed["to"], "done");
    assert_eq!(completed["completed"], true);
    assert!(completed["audit_persisted"].is_boolean());

    let transitioned_note = runtime
        .notes(&token)
        .expect("note store")
        .get_note(transition_id)
        .await
        .expect("read transitioned task")
        .expect("transitioned task exists");
    assert!(transitioned_note.updated_at > transition_revision);
    assert_eq!(
        transitioned_note
            .properties
            .as_ref()
            .and_then(|properties| properties.get("status"))
            .and_then(|status| status.as_str()),
        Some("next")
    );
    assert_eq!(
        transitioned_note
            .properties
            .as_ref()
            .and_then(|properties| properties.get("description"))
            .and_then(|description| description.as_str()),
        Some(transitioned_note.content.as_str())
    );

    let completed_note = runtime
        .notes(&token)
        .expect("note store")
        .get_note(complete_id)
        .await
        .expect("read completed task")
        .expect("completed task exists");
    assert!(completed_note.updated_at > complete_revision);
    assert_eq!(
        completed_note
            .properties
            .as_ref()
            .and_then(|properties| properties.get("status"))
            .and_then(|status| status.as_str()),
        Some("done")
    );
    assert_eq!(
        completed_note
            .properties
            .as_ref()
            .and_then(|properties| properties.get("description"))
            .and_then(|description| description.as_str()),
        Some(completed_note.content.as_str())
    );
}

#[tokio::test]
async fn complete_from_waiting_succeeds_per_lifecycle_table() {
    let pack = pack(rt());
    let resp = assign(&pack, json!({"title": "waiting task", "status": "waiting"})).await;
    let id = resp["full_id"].as_str().unwrap().to_string();

    let done = pack
        .dispatch("gtd.complete", json!({"id": id}))
        .await
        .expect("ADR-019 permits waiting -> done");
    assert_eq!(done["from"], "waiting");
    assert_eq!(done["to"], "done");
}

#[tokio::test]
async fn complete_from_someday_succeeds_per_lifecycle_table() {
    let pack = pack(rt());
    let resp = assign(&pack, json!({"title": "someday task", "status": "someday"})).await;
    let id = resp["full_id"].as_str().unwrap().to_string();

    let done = pack
        .dispatch("gtd.complete", json!({"id": id}))
        .await
        .expect("ADR-019 permits someday -> done");
    assert_eq!(done["from"], "someday");
    assert_eq!(done["to"], "done");
}

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
        "complete(status=cancelled) must transition to 'cancelled', not 'done'; got: {result}"
    );
    assert_eq!(result["completed"], true);
    assert!(
        result["is_terminal"].as_bool().unwrap_or(false),
        "cancelled must be a terminal state; got: {result}"
    );
}

#[tokio::test]
async fn cc1_complete_with_status_done_still_works() {
    let pack = pack(rt());
    let resp = assign(&pack, json!({"title": "cc1-done-test", "status": "next"})).await;
    let id = resp["full_id"].as_str().unwrap().to_string();

    let result = pack
        .dispatch("gtd.complete", json!({"id": id, "status": "done"}))
        .await
        .expect("complete(status=done) must succeed");

    assert_eq!(result["to"], "done", "explicit status=done must work");
}

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

    assert_eq!(result["to"], "done", "default status must be 'done'");
}

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
        "invalid status must be rejected with helpful message; got: {msg}"
    );
}

#[tokio::test]
async fn complete_response_includes_completed_at() {
    let pack = pack(rt());
    let resp = assign(&pack, json!({"title": "track completion time"})).await;
    let id = resp["full_id"].as_str().unwrap().to_string();

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

#[tokio::test]
async fn complete_sets_properties_status_to_done() {
    let rt = rt();
    let fixture = pack(rt.clone());

    let resp = assign(&fixture, json!({"title": "check status after complete"})).await;
    let id = resp["full_id"].as_str().unwrap().to_string();
    let uuid = uuid::Uuid::parse_str(&id).unwrap();

    fixture
        .dispatch("gtd.transition", json!({"id": id, "status": "next"}))
        .await
        .expect("transition to next must succeed");

    fixture
        .dispatch("gtd.complete", json!({"id": id}))
        .await
        .expect("complete must succeed");

    let token = rt.authorize(Namespace::local()).unwrap();
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

#[tokio::test]
async fn dsl_parallel_c2_double_complete_second_must_fail() {
    let pack = pack(rt());
    let resp = assign(&pack, json!({"title": "race-test", "status": "next"})).await;
    let id = resp["full_id"].as_str().unwrap().to_string();

    let first = pack
        .dispatch("gtd.complete", json!({"id": id, "result": "op-A"}))
        .await
        .expect("first complete must succeed");
    assert_eq!(first["to"], "done");

    let err = pack
        .dispatch("gtd.complete", json!({"id": id, "result": "op-B"}))
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("terminal state"),
        "dsl-parallel C2: second complete must fail with terminal-state error; got: {err}"
    );
}

#[tokio::test]
async fn noop_transition_without_note_omits_note_recorded_field() {
    let pack = pack(rt());
    let resp = assign(
        &pack,
        json!({"title": "noop no-note field test", "status": "next"}),
    )
    .await;
    let id = resp["full_id"].as_str().unwrap().to_string();

    let r = pack
        .dispatch("gtd.transition", json!({"id": id, "status": "next"}))
        .await
        .unwrap();
    assert_eq!(r["transitioned"], false);
    assert_eq!(r["note"], "already in target status");
    assert!(
        r.get("note_recorded").is_none(),
        "note_recorded must be absent (not just false) when no note was supplied — issue #15"
    );
}

#[tokio::test]
async fn noop_transition_with_note_persists_transition_note_in_properties() {
    use khive_storage::{SqlStatement, SqlValue};

    let rt = rt();
    let pack = pack(rt.clone());
    let resp = assign(
        &pack,
        json!({"title": "noop note persistence test", "status": "next"}),
    )
    .await;
    let id = resp["full_id"].as_str().unwrap().to_string();

    // issue #15: current == target, but the note must still land in the
    // task's stored properties, not be silently dropped.
    let r = pack
        .dispatch(
            "gtd.transition",
            json!({"id": id, "status": "next", "note": "blocked on review"}),
        )
        .await
        .expect("noop transition with note should succeed");
    assert_eq!(r["transitioned"], false);
    assert_eq!(r["note_recorded"], true);

    let sql = rt.sql();
    let mut reader = sql.reader().await.expect("sql reader");
    let rows = reader
        .query_all(SqlStatement {
            sql: "SELECT json_extract(properties, '$.transition_note') as tn \
                  FROM notes WHERE id = ?1"
                .into(),
            params: vec![SqlValue::Text(id.clone())],
            label: None,
        })
        .await
        .expect("properties query");

    assert_eq!(rows.len(), 1, "task row must exist");
    assert_eq!(
        rows[0].get("tn").and_then(|v| {
            if let SqlValue::Text(s) = v {
                Some(s.as_str())
            } else {
                None
            }
        }),
        Some("blocked on review"),
        "properties.transition_note must be persisted on a noop transition (issue #15)"
    );
}

#[tokio::test]
async fn repeated_noop_notes_are_last_write_wins() {
    use khive_storage::{SqlStatement, SqlValue};

    let rt = rt();
    let pack = pack(rt.clone());
    let resp = assign(
        &pack,
        json!({"title": "noop note overwrite test", "status": "next"}),
    )
    .await;
    let id = resp["full_id"].as_str().unwrap().to_string();

    for note in ["first update", "second update"] {
        let r = pack
            .dispatch(
                "gtd.transition",
                json!({"id": id, "status": "next", "note": note}),
            )
            .await
            .expect("noop transition with note should succeed");
        assert_eq!(r["transitioned"], false);
        assert_eq!(r["note_recorded"], true);
    }

    let sql = rt.sql();
    let mut reader = sql.reader().await.expect("sql reader");
    let rows = reader
        .query_all(SqlStatement {
            sql: "SELECT json_extract(properties, '$.transition_note') as tn \
                  FROM notes WHERE id = ?1"
                .into(),
            params: vec![SqlValue::Text(id.clone())],
            label: None,
        })
        .await
        .expect("properties query");
    assert_eq!(
        rows[0].get("tn").and_then(|v| {
            if let SqlValue::Text(s) = v {
                Some(s.as_str())
            } else {
                None
            }
        }),
        Some("second update"),
        "a later noop note must overwrite the stored transition_note (last write wins)"
    );
}

#[tokio::test]
async fn empty_string_noop_note_is_persisted() {
    use khive_storage::{SqlStatement, SqlValue};

    let rt = rt();
    let pack = pack(rt.clone());
    let resp = assign(
        &pack,
        json!({"title": "noop empty note test", "status": "next"}),
    )
    .await;
    let id = resp["full_id"].as_str().unwrap().to_string();

    let r = pack
        .dispatch(
            "gtd.transition",
            json!({"id": id, "status": "next", "note": ""}),
        )
        .await
        .expect("noop transition with empty note should succeed");
    assert_eq!(r["transitioned"], false);
    assert_eq!(r["note_recorded"], true);

    let sql = rt.sql();
    let mut reader = sql.reader().await.expect("sql reader");
    let rows = reader
        .query_all(SqlStatement {
            sql: "SELECT json_extract(properties, '$.transition_note') as tn \
                  FROM notes WHERE id = ?1"
                .into(),
            params: vec![SqlValue::Text(id.clone())],
            label: None,
        })
        .await
        .expect("properties query");
    assert_eq!(
        rows[0].get("tn").and_then(|v| {
            if let SqlValue::Text(s) = v {
                Some(s.as_str())
            } else {
                None
            }
        }),
        Some(""),
        "an explicitly supplied empty note is stored as the empty string, not dropped"
    );
}
