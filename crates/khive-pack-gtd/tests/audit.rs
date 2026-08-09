//! Tests for GTD lifecycle audit records.

mod common;

use common::{assign, pack, rt};
use khive_storage::{SqlStatement, SqlValue};
use serde_json::json;
use uuid::Uuid;

#[tokio::test]
async fn public_write_audit_record_compatibility_api_returns_unit() {
    let rt = rt();
    khive_pack_gtd::handlers::ensure_audit_schema(&rt).await;
    let note_id = Uuid::new_v4();
    let result =
        khive_pack_gtd::handlers::write_audit_record(&rt, note_id, "inbox", "next", None, "local")
            .await;
    let _: () = result;

    let mut reader = rt.sql().reader().await.expect("sql reader");
    let rows = reader
        .query_all(SqlStatement {
            sql: "SELECT COUNT(*) AS count FROM gtd_lifecycle_audit WHERE note_id = ?1".into(),
            params: vec![SqlValue::Text(note_id.to_string())],
            label: None,
        })
        .await
        .expect("audit count");
    assert!(matches!(rows[0].get("count"), Some(SqlValue::Integer(1))));
}

#[tokio::test]
async fn transition_writes_lifecycle_audit_record() {
    let rt = rt();
    let fixture = pack(rt.clone());

    let resp = assign(
        &fixture,
        json!({"title": "audit test task", "status": "inbox"}),
    )
    .await;
    let task_id = resp["full_id"].as_str().unwrap().to_string();

    let transition = fixture
        .dispatch(
            "gtd.transition",
            json!({"id": task_id, "status": "next", "note": "moved to next"}),
        )
        .await
        .expect("transition should succeed");
    assert_eq!(transition["audit_persisted"], true);

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

#[tokio::test]
async fn complete_writes_lifecycle_audit_record() {
    let rt = rt();
    let fixture = pack(rt.clone());

    let resp = assign(&fixture, json!({"title": "audit complete test"})).await;
    let task_id = resp["full_id"].as_str().unwrap().to_string();

    fixture
        .dispatch("gtd.transition", json!({"id": task_id, "status": "next"}))
        .await
        .expect("transition to next should succeed");

    let completed = fixture
        .dispatch("gtd.complete", json!({"id": task_id, "result": "done!"}))
        .await
        .expect("complete should succeed");
    assert_eq!(completed["audit_persisted"], true);

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

#[tokio::test]
async fn lifecycle_success_reports_audit_degradation_when_insert_fails() {
    let rt = rt();
    khive_pack_gtd::handlers::ensure_audit_schema(&rt).await;
    {
        let mut writer = rt.sql().writer().await.expect("sql writer");
        writer
            .execute_script(
                "CREATE TRIGGER reject_gtd_audit_insert \
                 BEFORE INSERT ON gtd_lifecycle_audit \
                 BEGIN SELECT RAISE(FAIL, 'forced audit failure'); END;"
                    .into(),
            )
            .await
            .expect("failure-injection trigger");
    }
    let fixture = pack(rt.clone());

    let transition_task = assign(
        &fixture,
        json!({"title": "audit-degraded transition", "status": "inbox"}),
    )
    .await;
    let transition = fixture
        .dispatch(
            "gtd.transition",
            json!({"id": transition_task["full_id"], "status": "next"}),
        )
        .await
        .expect("domain transition remains successful");
    assert_eq!(transition["transitioned"], true);
    assert_eq!(transition["to"], "next");
    assert_eq!(
        transition["audit_persisted"], false,
        "caller must see that the best-effort audit append was lost"
    );

    let complete_task = assign(
        &fixture,
        json!({"title": "audit-degraded complete", "status": "inbox"}),
    )
    .await;
    let completed = fixture
        .dispatch(
            "gtd.complete",
            json!({"id": complete_task["full_id"], "result": "finished during triage"}),
        )
        .await
        .expect("domain completion remains successful");
    assert_eq!(completed["completed"], true);
    assert_eq!(completed["to"], "done");
    assert_eq!(
        completed["audit_persisted"], false,
        "caller must see that the best-effort audit append was lost"
    );

    let noop_task = assign(
        &fixture,
        json!({"title": "audit-degraded no-op note", "status": "next"}),
    )
    .await;
    let noop = fixture
        .dispatch(
            "gtd.transition",
            json!({
                "id": noop_task["full_id"],
                "status": "next",
                "note": "persist note despite degraded audit",
            }),
        )
        .await
        .expect("canonical note-bearing no-op remains successful");
    assert_eq!(noop["transitioned"], false);
    assert_eq!(noop["note_recorded"], true);
    assert_eq!(noop["audit_persisted"], false);

    let mut reader = rt.sql().reader().await.expect("sql reader");
    let rows = reader
        .query_all(SqlStatement {
            sql: "SELECT COUNT(*) AS count FROM gtd_lifecycle_audit".into(),
            params: vec![],
            label: None,
        })
        .await
        .expect("audit count");
    assert!(matches!(rows[0].get("count"), Some(SqlValue::Integer(0))));
}

#[tokio::test]
async fn noop_transition_does_not_write_audit_record() {
    let rt = rt();
    let fixture = pack(rt.clone());

    let resp = assign(
        &fixture,
        json!({"title": "noop audit test", "status": "inbox"}),
    )
    .await;
    let task_id = resp["full_id"].as_str().unwrap().to_string();

    fixture
        .dispatch("gtd.transition", json!({"id": task_id, "status": "next"}))
        .await
        .expect("real transition should succeed");

    let r = fixture
        .dispatch("gtd.transition", json!({"id": task_id, "status": "next"}))
        .await
        .expect("noop transition should return ok");
    assert_eq!(
        r["transitioned"], false,
        "noop must return transitioned=false"
    );

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

/// F3 regression: a `gtd_lifecycle_audit` table created by an older pack
/// version (no `namespace` column) must be upgraded in place on the next
/// `ensure_audit_schema` call, and the transition must still write an audit row.
#[tokio::test]
async fn transition_upgrades_namespace_less_audit_table_and_writes_row() {
    let rt = rt();

    {
        let mut writer = rt.sql().writer().await.expect("sql writer");
        writer
            .execute_script(
                "CREATE TABLE gtd_lifecycle_audit (\
                    note_id TEXT NOT NULL,\
                    from_state TEXT NOT NULL,\
                    to_state TEXT NOT NULL,\
                    note TEXT,\
                    at INTEGER NOT NULL\
                )"
                .into(),
            )
            .await
            .expect("old audit table");
    }

    let fixture = pack(rt.clone());

    let resp = assign(
        &fixture,
        json!({"title": "legacy audit table task", "status": "inbox"}),
    )
    .await;
    let task_id = resp["full_id"].as_str().unwrap().to_string();

    fixture
        .dispatch("gtd.transition", json!({"id": task_id, "status": "next"}))
        .await
        .expect("transition should succeed against upgraded legacy table");

    let sql = rt.sql();
    let mut reader = sql.reader().await.expect("sql reader");

    let cols = reader
        .query_all(SqlStatement {
            sql: "PRAGMA table_info(gtd_lifecycle_audit)".into(),
            params: vec![],
            label: None,
        })
        .await
        .expect("table_info query");
    assert!(
        cols.iter().any(|row| matches!(
            row.get("name"),
            Some(SqlValue::Text(name)) if name == "namespace"
        )),
        "gtd_lifecycle_audit must be upgraded with a namespace column; got columns {cols:?}"
    );

    let rows = reader
        .query_all(SqlStatement {
            sql: "SELECT namespace FROM gtd_lifecycle_audit WHERE note_id = ?1".into(),
            params: vec![SqlValue::Text(task_id.clone())],
            label: None,
        })
        .await
        .expect("audit query");
    assert_eq!(
        rows.len(),
        1,
        "transition against an upgraded legacy table must write exactly one audit row; got {rows:?}"
    );
    assert_eq!(
        rows[0].get("namespace").and_then(|v| {
            if let SqlValue::Text(s) = v {
                Some(s.as_str())
            } else {
                None
            }
        }),
        Some("local"),
        "audit row namespace must be 'local'"
    );
}

/// #95: successive `gtd.transition` notes must all remain retrievable, not
/// just the last one. `transition_note` (a top-level `properties` field)
/// stays last-write-wins on purpose — every existing caller already reads it
/// as "the current/latest note" — but `transition_history` (new, additive,
/// same `properties` JSON blob — no schema change) must accumulate every
/// transition's own from/to/note/at, in order. Walk a task through three
/// distinct-note transitions and confirm all three survive a plain task read
/// (via `gtd.tasks`, exercising the same `render_task` path every caller
/// hits — not just a raw SQL query against `gtd_lifecycle_audit`).
#[tokio::test]
async fn transition_history_accumulates_across_multiple_transitions() {
    let rt = rt();
    let fixture = pack(rt.clone());

    let resp = assign(
        &fixture,
        json!({"title": "history test task", "status": "inbox"}),
    )
    .await;
    let task_id = resp["full_id"].as_str().unwrap().to_string();

    fixture
        .dispatch(
            "gtd.transition",
            json!({"id": task_id, "status": "next", "note": "triage note"}),
        )
        .await
        .expect("inbox -> next");
    fixture
        .dispatch(
            "gtd.transition",
            json!({"id": task_id, "status": "active", "note": "start note"}),
        )
        .await
        .expect("next -> active");
    fixture
        .dispatch(
            "gtd.transition",
            json!({"id": task_id, "status": "done", "note": "finish note"}),
        )
        .await
        .expect("active -> done");

    let tasks = fixture
        .dispatch("gtd.tasks", json!({"status": "done"}))
        .await
        .expect("tasks(status=done) ok");
    let arr = tasks.as_array().expect("tasks(status=..) stays bare array");
    let task = arr
        .iter()
        .find(|t| t["full_id"] == task_id)
        .expect("the transitioned task must be in the done listing");

    // Latest-note field is unchanged behavior: still last-write-wins.
    assert_eq!(
        task["properties"]["transition_note"], "finish note",
        "transition_note must still read as the LATEST note (unchanged behavior)"
    );

    let history = task["properties"]["transition_history"]
        .as_array()
        .expect("transition_history must be present and be an array");
    assert_eq!(
        history.len(),
        3,
        "all three transitions must be recorded; got: {history:?}"
    );
    let notes: Vec<&str> = history
        .iter()
        .map(|h| h["note"].as_str().unwrap_or("?"))
        .collect();
    assert_eq!(
        notes,
        vec!["triage note", "start note", "finish note"],
        "every transition note must be individually retrievable in order; got: {notes:?}"
    );
    assert_eq!(history[0]["from"], "inbox");
    assert_eq!(history[0]["to"], "next");
    assert_eq!(history[1]["from"], "next");
    assert_eq!(history[1]["to"], "active");
    assert_eq!(history[2]["from"], "active");
    assert_eq!(history[2]["to"], "done");
    for entry in history {
        assert!(
            entry["at"].as_str().is_some(),
            "every history entry must carry a timestamp; got: {entry:?}"
        );
    }
}

#[tokio::test]
async fn cc1_complete_cancelled_writes_audit_record() {
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
        "complete(status=cancelled) must write audit row with to_state='cancelled'"
    );
}

#[tokio::test]
async fn noop_transition_with_note_writes_audit_record_and_persists_note() {
    let rt = rt();
    let fixture = pack(rt.clone());

    let resp = assign(
        &fixture,
        json!({"title": "noop with note test", "status": "inbox"}),
    )
    .await;
    let task_id = resp["full_id"].as_str().unwrap().to_string();

    fixture
        .dispatch("gtd.transition", json!({"id": task_id, "status": "next"}))
        .await
        .expect("real transition should succeed");

    // issue #15: a noop transition (current == target) with a caller-supplied
    // `note` must persist the note instead of silently discarding it.
    let r = fixture
        .dispatch(
            "gtd.transition",
            json!({"id": task_id, "status": "next", "note": "still working on it"}),
        )
        .await
        .expect("noop transition with note should succeed");
    assert_eq!(
        r["transitioned"], false,
        "noop must still report transitioned=false"
    );
    assert_eq!(r["note"], "already in target status");
    assert_eq!(
        r["note_recorded"], true,
        "note_recorded must be true when a note is persisted"
    );
    assert_eq!(r["audit_persisted"], true);

    let sql = rt.sql();
    let mut reader = sql.reader().await.expect("sql reader");
    let rows = reader
        .query_all(SqlStatement {
            sql: "SELECT from_state, to_state, note FROM gtd_lifecycle_audit \
                  WHERE note_id = ?1 AND from_state = 'next' AND to_state = 'next'"
                .into(),
            params: vec![SqlValue::Text(task_id.clone())],
            label: None,
        })
        .await
        .expect("audit query");

    assert_eq!(
        rows.len(),
        1,
        "issue #15: a same-status transition carrying a note must write one \
         same-status audit row; got {rows:?}"
    );
    assert_eq!(
        rows[0].get("note").and_then(|v| {
            if let SqlValue::Text(s) = v {
                Some(s.as_str())
            } else {
                None
            }
        }),
        Some("still working on it"),
        "audit note text must match the caller-supplied note"
    );
}
