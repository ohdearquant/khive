use std::any::Any;
use std::collections::{BTreeMap, BTreeSet};

use chrono::Utc;
use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError};
use khive_storage::{SqlRow, SqlStatement, SqlValue, StorageCapability, StorageError};
use serde::Deserialize;
use serde_json::{json, Map, Value};

use crate::schema::TASK_STATUSES;
use crate::GtdPack;

const SNAPSHOT_SQL: &str = include_str!("../sql/task-repair-snapshot.sql");
const UPDATE_SQL: &str = include_str!("../sql/task-repair-update.sql");
const AUDIT_SQL: &str = concat!(
    "INSERT INTO gtd_lifecycle_audit (note_id, from_state, to_state, note, at, namespace)\n",
    "VALUES (?1, ?2, ?3, ?4, ?5, ?6)\n",
);
const FIELDS: [&str; 3] = ["created_at", "updated_at", "status"];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RepairParams {
    items: Vec<RepairItem>,
    #[serde(default)]
    apply: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RepairItem {
    id: String,
    changes: BTreeMap<String, RepairChange>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RepairChange {
    observed: Value,
    value: Value,
}

struct PreparedRepair {
    result: Value,
    statements: Option<(SqlStatement, SqlStatement)>,
}

fn invalid(message: impl Into<String>) -> RuntimeError {
    RuntimeError::InvalidInput(format!("gtd.repair: {}", message.into()))
}

fn parse_params(params: Value) -> Result<RepairParams, RuntimeError> {
    if !params.is_object() {
        return Err(invalid("arguments must be an object"));
    }
    let items = params
        .get("items")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("items must be an array"))?;
    if !(1..=100).contains(&items.len()) {
        return Err(invalid("items must contain 1..=100 rows"));
    }
    for item in items {
        if !item.is_object() {
            return Err(invalid("each item must be an object"));
        }
        let changes = item
            .get("changes")
            .and_then(Value::as_object)
            .filter(|changes| !changes.is_empty())
            .ok_or_else(|| invalid("changes must be a nonempty object"))?;
        for (field, change) in changes {
            if !change.is_object()
                || change.get("observed").is_none()
                || change.get("value").is_none()
            {
                return Err(invalid(format!(
                    "{field} must supply observed and value in an object"
                )));
            }
        }
    }
    let params: RepairParams =
        serde_json::from_value(params).map_err(|error| invalid(format!("bad params: {error}")))?;
    let mut ids = BTreeSet::new();
    for item in &params.items {
        let id = uuid::Uuid::parse_str(&item.id)
            .map_err(|_| invalid("id must be a canonical lowercase dashed full UUID"))?;
        if id.to_string() != item.id {
            return Err(invalid("id must be a canonical lowercase dashed full UUID"));
        }
        if !ids.insert(&item.id) {
            return Err(invalid(format!("duplicate id {}", item.id)));
        }
    }
    Ok(params)
}

fn column<'a>(row: &'a SqlRow, name: &str) -> Result<&'a SqlValue, RuntimeError> {
    row.get(name).ok_or_else(|| {
        RuntimeError::Internal(format!("gtd.repair: missing snapshot column {name}"))
    })
}

fn text<'a>(row: &'a SqlRow, name: &str) -> Result<&'a str, RuntimeError> {
    match column(row, name)? {
        SqlValue::Text(value) => Ok(value),
        _ => Err(RuntimeError::Internal(format!(
            "gtd.repair: invalid snapshot column {name}"
        ))),
    }
}

fn scalar_source(value: &SqlValue) -> Option<Value> {
    let source = match value {
        SqlValue::Null => "null".to_owned(),
        SqlValue::Integer(value) => value.to_string(),
        SqlValue::Float(value) if value.is_finite() => serde_json::to_string(value).ok()?,
        SqlValue::Text(value) => serde_json::to_string(value).ok()?,
        _ => return None,
    };
    Some(Value::String(source))
}

fn status_source(row: &SqlRow) -> Result<Value, RuntimeError> {
    match column(row, "stored_status_json")? {
        SqlValue::Null => Ok(Value::Null),
        SqlValue::Text(source) => Ok(Value::String(source.clone())),
        _ => Err(RuntimeError::Internal(
            "gtd.repair: invalid stored status source".into(),
        )),
    }
}

fn refuse(mut result: Value, reason: &str, message: impl Into<String>) -> PreparedRepair {
    result["reason"] = json!(reason);
    result["message"] = json!(message.into());
    PreparedRepair {
        result,
        statements: None,
    }
}

fn repair_history(row: &SqlRow) -> Result<Option<Value>, RuntimeError> {
    let source = match column(row, "repair_history_json")? {
        SqlValue::Null => return Ok(Some(json!({"originals": {}}))),
        SqlValue::Text(source) => source,
        _ => return Ok(None),
    };
    let Ok(history) = serde_json::from_str::<Value>(source) else {
        return Ok(None);
    };
    let Some(object) = history.as_object() else {
        return Ok(None);
    };
    if object
        .keys()
        .any(|key| !matches!(key.as_str(), "originals" | "last"))
    {
        return Ok(None);
    }
    let Some(originals) = history.get("originals").and_then(Value::as_object) else {
        return Ok(None);
    };
    for (field, original) in originals {
        let Some(record) = original.as_object() else {
            return Ok(None);
        };
        if !FIELDS.contains(&field.as_str())
            || record.len() != 3
            || !record
                .get("value")
                .is_some_and(|value| value.is_string() || value.is_null())
            || record
                .get("at")
                .is_none_or(|value| value.as_i64().is_none())
            || !record.get("actor").is_some_and(Value::is_string)
        {
            return Ok(None);
        }
    }
    // Only originals are carried forward; the previous last repair remains
    // in the mandatory audit record and is replaced by this repair's entry.
    Ok(Some(json!({"originals": originals})))
}

async fn prepare_row(
    runtime: &KhiveRuntime,
    actor: &str,
    item: &RepairItem,
) -> Result<PreparedRepair, RuntimeError> {
    let mut result = json!({
        "id": item.id,
        "accepted": false,
        "applied": false,
        "stored": {},
        "proposed": item.changes.iter().map(|(field, change)| (field.clone(), change.value.clone())).collect::<Map<_, _>>(),
        "reason": null,
    });
    let sql = runtime.sql();
    let row = {
        let mut reader = sql.reader().await?;
        reader
            .query_row(SqlStatement {
                sql: SNAPSHOT_SQL.into(),
                params: vec![SqlValue::Text(item.id.clone())],
                label: Some("gtd_repair_snapshot".into()),
            })
            .await?
    };
    let Some(row) = row else {
        return Ok(refuse(
            result,
            "not_found",
            "the full id does not name a note",
        ));
    };
    if text(&row, "kind")? != "task" {
        return Ok(refuse(result, "not_task", "repair requires a task note"));
    }
    if !matches!(column(&row, "deleted_at")?, SqlValue::Null) {
        return Ok(refuse(
            result,
            "deleted",
            "deleted task rows cannot be repaired",
        ));
    }
    if text(&row, "properties_type")? != "object" {
        return Ok(refuse(
            result,
            "invalid_properties",
            "task properties must be an object or SQL NULL",
        ));
    }
    let stored_status = status_source(&row)?;
    for field in item.changes.keys() {
        let stored = match field.as_str() {
            "created_at" | "updated_at" => {
                let Some(source) = scalar_source(column(&row, field)?) else {
                    return Ok(refuse(
                        result,
                        "unsupported_stored_value",
                        format!("{field} has no faithful JSON scalar source"),
                    ));
                };
                source
            }
            "status" => stored_status.clone(),
            _ => {
                return Ok(refuse(
                    result,
                    "unsupported_field",
                    format!(
                        "unsupported field {field}; allowed fields: created_at, updated_at, status"
                    ),
                ))
            }
        };
        result["stored"][field] = stored;
    }
    for (field, change) in &item.changes {
        if change.observed != result["stored"][field] {
            return Ok(refuse(
                result,
                "stale_observed",
                format!("{field} differs from the exact observed JSON source"),
            ));
        }
        match field.as_str() {
            "created_at" | "updated_at" if change.value.as_i64().is_none() => {
                return Ok(refuse(
                    result,
                    "invalid_target",
                    format!("{field} value must be a signed 64-bit integer; no units are inferred"),
                ));
            }
            "status" => {
                if !matches!(change.value.as_str(), Some("done" | "cancelled")) {
                    return Ok(refuse(
                        result,
                        "invalid_target",
                        "status value must be done or cancelled",
                    ));
                }
                if !matches!(column(&row, "stored_status_type")?, SqlValue::Text(kind) if kind == "text")
                {
                    return Ok(refuse(result, "lifecycle_status", "absent, null, and non-text statuses retain the inbox fallback; use gtd.transition or gtd.complete"));
                }
                let current: String = serde_json::from_str(
                    stored_status
                        .as_str()
                        .ok_or_else(|| invalid("missing status source"))?,
                )
                .map_err(|_| {
                    RuntimeError::Internal("gtd.repair: invalid text status source".into())
                })?;
                if TASK_STATUSES.contains(&current.as_str()) {
                    return Ok(refuse(
                        result,
                        "canonical_status",
                        "canonical status changes belong to gtd.transition or gtd.complete",
                    ));
                }
            }
            _ => {}
        }
    }
    let Some(mut history) = repair_history(&row)? else {
        return Ok(refuse(
            result,
            "invalid_repair_history",
            "existing gtd_repair originals cannot be replaced or reinterpreted",
        ));
    };
    let now = Utc::now().timestamp_micros();
    let mut changes = Map::new();
    for (field, change) in &item.changes {
        let originals = history["originals"]
            .as_object_mut()
            .ok_or_else(|| RuntimeError::Internal("gtd.repair: invalid originals map".into()))?;
        originals
            .entry(field.clone())
            .or_insert_with(|| json!({"value": change.observed, "at": now, "actor": actor}));
        changes.insert(
            field.clone(),
            json!({"observed": change.observed, "value": change.value}),
        );
    }
    history["last"] = json!({"at": now, "actor": actor, "changes": changes});
    let supplied = |field: &str| SqlValue::Integer(i64::from(item.changes.contains_key(field)));
    let timestamp = |field: &str| {
        item.changes
            .get(field)
            .and_then(|change| change.value.as_i64())
            .map_or(SqlValue::Null, SqlValue::Integer)
    };
    let new_status = item
        .changes
        .get("status")
        .and_then(|change| change.value.as_str());
    let history_json = serde_json::to_string(&history)
        .map_err(|error| RuntimeError::Internal(format!("gtd.repair history: {error}")))?;
    let update = SqlStatement {
        sql: UPDATE_SQL.into(),
        params: vec![
            SqlValue::Text(item.id.clone()),
            column(&row, "properties")?.clone(),
            column(&row, "version")?.clone(),
            column(&row, "created_at")?.clone(),
            column(&row, "updated_at")?.clone(),
            column(&row, "created_type")?.clone(),
            column(&row, "updated_type")?.clone(),
            supplied("created_at"),
            timestamp("created_at"),
            supplied("updated_at"),
            timestamp("updated_at"),
            supplied("status"),
            new_status.map_or(SqlValue::Null, |value| SqlValue::Text(value.into())),
            SqlValue::Text(history_json),
        ],
        label: Some("gtd_repair_update".into()),
    };
    let from = if matches!(column(&row, "stored_status_type")?, SqlValue::Text(kind) if kind == "text")
    {
        serde_json::from_str::<String>(
            stored_status
                .as_str()
                .ok_or_else(|| invalid("missing status source"))?,
        )
        .map_err(|_| RuntimeError::Internal("gtd.repair: invalid audit status source".into()))?
    } else {
        "inbox".into()
    };
    let audit = SqlStatement {
        sql: AUDIT_SQL.into(),
        params: vec![
            SqlValue::Text(item.id.clone()), SqlValue::Text(from.clone()),
            SqlValue::Text(new_status.unwrap_or(&from).into()),
            SqlValue::Text(json!({"operation": "gtd.repair", "at": now, "actor": actor, "stored_status": stored_status, "changes": changes}).to_string()),
            SqlValue::Integer(now), SqlValue::Text(text(&row, "namespace")?.into()),
        ],
        label: Some("gtd_repair_audit".into()),
    };
    result["accepted"] = json!(true);
    Ok(PreparedRepair {
        result,
        statements: Some((update, audit)),
    })
}

async fn commit_prepared(
    runtime: &KhiveRuntime,
    update: SqlStatement,
    audit: SqlStatement,
) -> Result<bool, RuntimeError> {
    let result = runtime
        .sql()
        .atomic_unit(Box::new(move |writer| {
            Box::pin(async move {
                let affected = writer.execute(update).await?;
                if affected == 0 {
                    return Ok(Box::new(false) as Box<dyn Any + Send>);
                }
                if affected != 1 || writer.execute(audit).await? != 1 {
                    return Err(StorageError::InvalidInput {
                        capability: StorageCapability::Sql,
                        operation: "gtd_repair".into(),
                        message: "repair and audit must each affect exactly one row".into(),
                    });
                }
                Ok(Box::new(true) as Box<dyn Any + Send>)
            })
        }))
        .await?;
    result
        .downcast::<bool>()
        .map(|applied| *applied)
        .map_err(|_| RuntimeError::Internal("gtd.repair: invalid transaction outcome".into()))
}

impl GtdPack {
    pub(crate) async fn handle_repair(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let params = parse_params(params)?;
        let mut results = Vec::with_capacity(params.items.len());
        for item in params.items {
            let mut plan = prepare_row(self.runtime(), &token.actor().id, &item).await?;
            if params.apply {
                if let Some((update, audit)) = plan.statements {
                    if commit_prepared(self.runtime(), update, audit).await? {
                        plan.result["applied"] = json!(true);
                    } else {
                        plan.result["accepted"] = json!(false);
                        plan.result["reason"] = json!("concurrent_change");
                        plan.result["message"] = json!("task changed after preparation; read its raw values again before retrying");
                    }
                }
            }
            results.push(plan.result);
        }
        let accepted = results.iter().filter(|row| row["accepted"] == true).count();
        let applied = results.iter().filter(|row| row["applied"] == true).count();
        Ok(
            json!({"apply": params.apply, "accepted": accepted, "applied": applied, "refused": results.len() - accepted, "results": results}),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use khive_runtime::Namespace;
    use khive_storage::Note;

    async fn snapshot(runtime: &KhiveRuntime, id: &str) -> Value {
        let mut reader = runtime.sql().reader().await.expect("snapshot reader");
        let row = reader
            .query_row(SqlStatement {
                sql: "SELECT * FROM notes WHERE id = ?1".into(),
                params: vec![SqlValue::Text(id.into())],
                label: Some("repair_race_snapshot".into()),
            })
            .await
            .expect("snapshot query")
            .expect("task snapshot");
        serde_json::to_value(row).expect("raw snapshot")
    }

    #[tokio::test]
    async fn prepared_repair_refuses_concurrent_snapshot_change() {
        let runtime = KhiveRuntime::memory().expect("memory runtime");
        let token = runtime.authorize(Namespace::local()).expect("test token");
        crate::handlers::ensure_audit_schema(&runtime).await;
        let store = runtime.notes(&token).expect("notes store");
        for mutation in [
            "properties = json_set(properties, '$.concurrent', true)",
            "deleted_at = 1772323200000001",
            "version = version + 1",
        ] {
            let mut note = Note::new("local", "task", "repair race fixture");
            note.created_at = 1772323200;
            note.updated_at = 1772323200;
            note.properties = Some(json!({"status": "archived", "archived_at": 1772323200000_i64}));
            let id = note.id.to_string();
            store.upsert_note(note).await.expect("seed task");
            let item = RepairItem {
                id: id.clone(),
                changes: BTreeMap::from([(
                    "created_at".into(),
                    RepairChange {
                        observed: json!("1772323200"),
                        value: json!(1772323200000000_i64),
                    },
                )]),
            };
            let plan = prepare_row(&runtime, &token.actor().id, &item)
                .await
                .expect("prepare repair");
            assert_eq!(
                plan.result["accepted"], true,
                "REPAIR_RACE_FIXTURE_ACCEPTED"
            );
            let (update, audit) = plan.statements.expect("prepared repair statements");
            {
                let mut writer = runtime.sql().writer().await.expect("concurrent writer");
                writer
                    .execute(SqlStatement {
                        sql: format!("UPDATE notes SET {mutation} WHERE id = ?1"),
                        params: vec![SqlValue::Text(id.clone())],
                        label: Some("repair_race_mutation".into()),
                    })
                    .await
                    .expect("concurrent mutation");
            }
            let before_commit = snapshot(&runtime, &id).await;
            assert!(
                !commit_prepared(&runtime, update, audit)
                    .await
                    .expect("guarded commit"),
                "REPAIR_PREPARED_SNAPSHOT_CHANGE_REFUSED: {mutation}"
            );
            assert_eq!(
                snapshot(&runtime, &id).await,
                before_commit,
                "REPAIR_RACE_PRESERVES_COMPETING_WRITE"
            );
            let mut reader = runtime.sql().reader().await.expect("audit reader");
            let audits = reader
                .query_scalar(SqlStatement {
                    sql: "SELECT COUNT(*) FROM gtd_lifecycle_audit WHERE note_id = ?1".into(),
                    params: vec![SqlValue::Text(id)],
                    label: Some("repair_race_audit_count".into()),
                })
                .await
                .expect("audit count");
            assert!(
                matches!(audits, Some(SqlValue::Integer(0))),
                "REPAIR_RACE_APPENDS_NO_AUDIT"
            );
        }
    }
}
