//! Durable receipts (`exec_runs`) and the append-only audit rows
//! (`exec_events`). A receipt is stored whole as JSON beside the columns the
//! listing verbs filter on, so the wire object and the durable row decode to
//! the same value.

use serde_json::{json, Value};

use khive_runtime::{KhiveRuntime, RuntimeError};
use khive_storage::types::{SqlRow, SqlStatement, SqlValue};

pub fn now_micros() -> i64 {
    chrono::Utc::now().timestamp_micros()
}

fn text(row: &SqlRow, col: &str) -> Option<String> {
    match row.get(col) {
        Some(SqlValue::Text(s)) => Some(s.clone()),
        Some(SqlValue::Json(v)) => Some(v.to_string()),
        _ => None,
    }
}

fn int(row: &SqlRow, col: &str) -> Option<i64> {
    match row.get(col) {
        Some(SqlValue::Integer(i)) => Some(*i),
        _ => None,
    }
}

fn opt_text(v: Option<&str>) -> SqlValue {
    match v {
        Some(s) => SqlValue::Text(s.to_string()),
        None => SqlValue::Null,
    }
}

/// Insert the receipt row and return its per-session `seq`. The number is
/// allocated inside the insert statement itself and read back from the row
/// (ADR-181 Amendment 3 item 2), so two runs of one session never share one;
/// the unique index on session rows is the backstop.
pub async fn insert(
    rt: &KhiveRuntime,
    ns: &str,
    receipt: &Value,
) -> Result<Option<i64>, RuntimeError> {
    let id = receipt["id"].as_str().unwrap_or_default().to_string();
    let actor = receipt["actor"].as_str().unwrap_or_default().to_string();
    let tool = receipt["tool"].as_str().unwrap_or_default().to_string();
    let session_id = receipt["session_id"].as_str().map(str::to_string);
    let mut writer = rt.sql().writer().await?;
    writer
        .execute(SqlStatement {
            sql: "INSERT INTO exec_runs (id, namespace, actor, tool, session_id, seq, receipt, created_at) \
                  VALUES (?1, ?2, ?3, ?4, ?5, \
                          CASE WHEN ?5 IS NULL THEN NULL ELSE \
                          (SELECT COALESCE(MAX(seq), 0) + 1 FROM exec_runs \
                           WHERE namespace = ?2 AND session_id = ?5) END, \
                          ?6, ?7)"
                .into(),
            params: vec![
                SqlValue::Text(id.clone()),
                SqlValue::Text(ns.to_string()),
                SqlValue::Text(actor),
                SqlValue::Text(tool),
                opt_text(session_id.as_deref()),
                SqlValue::Text(receipt.to_string()),
                SqlValue::Integer(now_micros()),
            ],
            label: Some("exec_runs_insert".into()),
        })
        .await?;
    if session_id.is_none() {
        return Ok(None);
    }
    let row = writer
        .query_row(SqlStatement {
            sql: "SELECT seq FROM exec_runs WHERE id = ?1".into(),
            params: vec![SqlValue::Text(id)],
            label: Some("exec_runs_seq".into()),
        })
        .await?;
    Ok(row.as_ref().and_then(|r| int(r, "seq")))
}

/// The `seq` column is authoritative; the stored JSON was written before the
/// number was allocated.
fn decode(row: &SqlRow) -> Option<Value> {
    let raw = text(row, "receipt")?;
    let mut value: Value = serde_json::from_str(&raw).ok()?;
    value["seq"] = int(row, "seq").map_or(Value::Null, Value::from);
    Some(value)
}

pub async fn get(rt: &KhiveRuntime, ns: &str, id: &str) -> Result<Value, RuntimeError> {
    let mut reader = rt.sql().reader().await?;
    let rows = reader
        .query_all(SqlStatement {
            sql: "SELECT receipt, seq FROM exec_runs WHERE namespace = ?1 AND id = ?2".into(),
            params: vec![
                SqlValue::Text(ns.to_string()),
                SqlValue::Text(id.to_string()),
            ],
            label: Some("exec_runs_get".into()),
        })
        .await?;
    rows.first()
        .and_then(decode)
        .ok_or_else(|| RuntimeError::NotFound(format!("exec receipt {id} not found")))
}

pub async fn list(
    rt: &KhiveRuntime,
    ns: &str,
    actor: &str,
    tool: Option<&str>,
    session_id: Option<&str>,
    limit: u32,
) -> Result<Vec<Value>, RuntimeError> {
    let mut sql =
        String::from("SELECT receipt, seq FROM exec_runs WHERE namespace = ?1 AND actor = ?2");
    let mut params = vec![
        SqlValue::Text(ns.to_string()),
        SqlValue::Text(actor.to_string()),
    ];
    if let Some(t) = tool {
        params.push(SqlValue::Text(t.to_string()));
        sql.push_str(&format!(" AND tool = ?{}", params.len()));
    }
    if let Some(s) = session_id {
        params.push(SqlValue::Text(s.to_string()));
        sql.push_str(&format!(" AND session_id = ?{}", params.len()));
    }
    params.push(SqlValue::Integer(i64::from(limit)));
    sql.push_str(&format!(
        " ORDER BY created_at DESC, rowid DESC LIMIT ?{}",
        params.len()
    ));
    let mut reader = rt.sql().reader().await?;
    let rows = reader
        .query_all(SqlStatement {
            sql,
            params,
            label: Some("exec_runs_list".into()),
        })
        .await?;
    Ok(rows.iter().filter_map(decode).collect())
}

pub async fn event(
    rt: &KhiveRuntime,
    ns: &str,
    run_id: &str,
    kind: &str,
    detail: Value,
) -> Result<(), RuntimeError> {
    let mut writer = rt.sql().writer().await?;
    writer
        .execute(SqlStatement {
            sql: "INSERT INTO exec_events (namespace, run_id, kind, at, detail) VALUES (?1, ?2, ?3, ?4, ?5)"
                .into(),
            params: vec![
                SqlValue::Text(ns.to_string()),
                SqlValue::Text(run_id.to_string()),
                SqlValue::Text(kind.to_string()),
                SqlValue::Integer(now_micros()),
                SqlValue::Text(detail.to_string()),
            ],
            label: Some("exec_events_insert".into()),
        })
        .await?;
    Ok(())
}

pub async fn events(
    rt: &KhiveRuntime,
    ns: &str,
    run_id: Option<&str>,
    limit: u32,
) -> Result<Vec<Value>, RuntimeError> {
    let mut sql =
        String::from("SELECT id, run_id, kind, at, detail FROM exec_events WHERE namespace = ?1");
    let mut params = vec![SqlValue::Text(ns.to_string())];
    if let Some(r) = run_id {
        params.push(SqlValue::Text(r.to_string()));
        sql.push_str(&format!(" AND run_id = ?{}", params.len()));
    }
    params.push(SqlValue::Integer(i64::from(limit)));
    sql.push_str(&format!(" ORDER BY id ASC LIMIT ?{}", params.len()));
    let mut reader = rt.sql().reader().await?;
    let rows = reader
        .query_all(SqlStatement {
            sql,
            params,
            label: Some("exec_events_list".into()),
        })
        .await?;
    Ok(rows
        .iter()
        .map(|r| {
            json!({
                "id": int(r, "id"),
                "run_id": text(r, "run_id"),
                "kind": text(r, "kind"),
                "at": int(r, "at").map(khive_runtime::micros_to_iso),
                "detail": text(r, "detail").and_then(|d| serde_json::from_str::<Value>(&d).ok()),
            })
        })
        .collect())
}
