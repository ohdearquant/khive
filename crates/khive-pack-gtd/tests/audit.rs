//! Tests for GTD lifecycle audit records.

mod common;

use common::{assign, pack, rt};
use khive_storage::{SqlStatement, SqlValue};
use serde_json::json;

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

    fixture
        .dispatch(
            "gtd.transition",
            json!({"id": task_id, "status": "next", "note": "moved to next"}),
        )
        .await
        .expect("transition should succeed");

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
        "CC-1: complete(status=cancelled) must write audit row with to_state='cancelled'"
    );
}
