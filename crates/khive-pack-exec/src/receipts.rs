//! Durable receipts (`exec_runs`) and lifecycle events, including legacy
//! `exec_events` rows. A receipt is stored whole as JSON beside the columns the
//! listing verbs filter on, so the wire object and the durable row decode to
//! the same value.

use serde_json::{json, Value};

use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError};
use khive_storage::event::{Event, EventPageQuery};
use khive_storage::types::{PageRequest, SqlRow, SqlStatement, SqlValue};
use khive_storage::EventFilter;
use khive_types::{EventKind, SubstrateKind};
use uuid::Uuid;

pub use khive_storage::now_micros;

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
    SqlValue::from_opt_text(v)
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
    token: &NamespaceToken,
    run_id: &str,
    kind: &str,
    detail: Value,
) -> Result<(), RuntimeError> {
    let run_id = Uuid::parse_str(run_id)
        .map_err(|_| RuntimeError::Internal("exec lifecycle run id is not a UUID".into()))?;
    // The guarded store stamps attribution from the token, never the receipt actor.
    let event = Event::new(
        token.namespace().as_str(),
        "exec.run",
        EventKind::Audit,
        SubstrateKind::Event,
        "",
    )
    .with_target(run_id)
    .with_payload(json!({ "phase": kind, "detail": detail }));
    rt.events(token)?.append_event(event).await?;
    Ok(())
}

pub async fn events(
    rt: &KhiveRuntime,
    token: &NamespaceToken,
    run_id: Option<&str>,
    limit: u32,
) -> Result<Vec<Value>, RuntimeError> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let mut sql =
        String::from("SELECT id, run_id, kind, at, detail FROM exec_events WHERE namespace = ?1");
    let mut params = vec![SqlValue::Text(token.namespace().as_str().to_string())];
    if let Some(r) = run_id {
        params.push(SqlValue::Text(r.to_string()));
        sql.push_str(&format!(" AND run_id = ?{}", params.len()));
    }
    params.push(SqlValue::Integer(i64::from(limit)));
    sql.push_str(&format!(
        " ORDER BY at ASC, CAST(id AS TEXT) COLLATE BINARY ASC LIMIT ?{}",
        params.len()
    ));
    let mut reader = rt.sql().reader().await?;
    let rows = reader
        .query_all(SqlStatement {
            sql,
            params,
            label: Some("exec_events_list".into()),
        })
        .await?;
    drop(reader);
    let mut merged = rows
        .iter()
        .map(|r| {
            let id = int(r, "id")
                .ok_or_else(|| RuntimeError::Internal("exec_events row has no integer id".into()))?
                .to_string();
            let at = int(r, "at").ok_or_else(|| {
                RuntimeError::Internal("exec_events row has no integer time".into())
            })?;
            let value = json!({
                "id": id,
                "run_id": text(r, "run_id"),
                "kind": text(r, "kind"),
                "at": khive_runtime::micros_to_iso(at),
                "detail": text(r, "detail").and_then(|d| serde_json::from_str::<Value>(&d).ok()),
            });
            Ok((at, id, value))
        })
        .collect::<Result<Vec<_>, RuntimeError>>()?;
    let requested_run = match run_id.map(Uuid::parse_str).transpose() {
        Ok(id) => id,
        Err(_) => return Ok(merged.into_iter().map(|(_, _, value)| value).collect()),
    };
    let store = rt.events(token)?;
    if let Some(target_id) = requested_run {
        // A normal run has at most three lifecycle rows. Complete the indexed
        // subset before sorting; a full page cannot establish completeness.
        let page = store
            .query_events(
                EventFilter {
                    target_id: Some(target_id),
                    kinds: vec![EventKind::Audit],
                    verbs: vec!["exec.run".into()],
                    substrates: vec![SubstrateKind::Event],
                    ..Default::default()
                },
                PageRequest {
                    offset: 0,
                    limit: 4096,
                },
            )
            .await?;
        if page.items.len() < 4096 {
            merged.extend(
                page.items
                    .into_iter()
                    .filter_map(|event| lifecycle_row(event, requested_run)),
            );
            retain_oldest(&mut merged, limit);
            return Ok(merged.into_iter().map(|(_, _, value)| value).collect());
        }
    }
    let mut query = lifecycle_page_query();
    loop {
        let page = store.query_event_page(query.clone()).await?;
        let Some(last) = page.rows.last().map(|row| row.order_key.clone()) else {
            if page.budget_stop.is_some() {
                return Err(RuntimeError::Internal(
                    "exec.events cannot read an event within the store page byte budget".into(),
                ));
            }
            break;
        };
        if query.after.as_ref().is_some_and(|after| {
            (last.created_at_us, &last.physical_id) <= (after.created_at_us, &after.physical_id)
        }) {
            return Err(RuntimeError::Internal(
                "exec.events event page made no cursor progress".into(),
            ));
        }
        let exhausted = page.budget_stop.is_none() && page.rows.len() < query.max_rows as usize;
        merged.extend(
            page.rows
                .into_iter()
                .filter_map(|row| lifecycle_row(row.event, requested_run)),
        );
        retain_oldest(&mut merged, limit);
        // Finish the timestamp group: persisted UUID spellings need not have
        // the same lexical order as their canonical wire strings.
        if exhausted
            || (merged.len() == limit as usize
                && merged.last().is_some_and(|row| last.created_at_us > row.0))
        {
            break;
        }
        query.after = Some(last);
    }
    Ok(merged.into_iter().map(|(_, _, value)| value).collect())
}

type LifecycleRow = (i64, String, Value);

fn lifecycle_page_query() -> EventPageQuery {
    EventPageQuery {
        // The daemon validates both bounds as chrono timestamps. The upper
        // bound is exclusive, as required by the shared page contract.
        since_us: chrono::DateTime::<chrono::Utc>::MIN_UTC.timestamp_micros(),
        until_us: chrono::DateTime::<chrono::Utc>::MAX_UTC.timestamp_micros(),
        kinds: vec![EventKind::Audit],
        actors: Vec::new(),
        exclude_namespaces: Vec::new(),
        after: None,
        max_rows: 4096,
    }
}

fn retain_oldest(rows: &mut Vec<LifecycleRow>, limit: u32) {
    rows.sort_by(|left, right| (&left.0, &left.1).cmp(&(&right.0, &right.1)));
    rows.truncate(limit as usize);
}

fn lifecycle_row(event: Event, requested_run: Option<Uuid>) -> Option<LifecycleRow> {
    let target = event.target_id?;
    let phase @ ("materialized" | "launched" | "exited") = event.payload.get("phase")?.as_str()?
    else {
        return None;
    };
    if event.kind != EventKind::Audit
        || event.verb != "exec.run"
        || event.substrate != SubstrateKind::Event
        || requested_run.is_some_and(|id| id != target)
    {
        return None;
    }
    let detail = event.payload.get("detail")?;
    let id = event.id.to_string();
    let value = json!({
        "id": id,
        "run_id": target,
        "kind": phase,
        "at": khive_runtime::micros_to_iso(event.created_at),
        "detail": detail,
    });
    Some((event.created_at, id, value))
}

#[cfg(test)]
#[path = "receipts/tests.rs"]
mod tests;
