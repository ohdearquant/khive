use std::collections::BTreeMap;

use khive_runtime::{NamespaceToken, RuntimeError};
use khive_storage::{SqlRow, SqlStatement, SqlValue};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::GtdPack;

const SQL: &str = include_str!("../sql/task-timestamp-census.sql");
const CANDIDATE_SQL: &str = include_str!("../sql/task-timestamp-candidates.sql");
const BUCKETS: [&str; 7] = [
    "null",
    "nonnumeric",
    "epoch_zero",
    "magnitude_10_digits",
    "magnitude_13_digits",
    "magnitude_16_digits",
    "other",
];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CensusParams {
    #[serde(default)]
    include_candidates: bool,
    limit: Option<u32>,
    cursor: Option<CandidateCursor>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CandidateCursor {
    namespace: String,
    id: String,
}

fn invalid_candidate_result() -> RuntimeError {
    RuntimeError::Internal("invalid task timestamp candidate result".into())
}

fn text_column<'a>(row: &'a SqlRow, name: &str) -> Result<&'a str, RuntimeError> {
    match row.get(name) {
        Some(SqlValue::Text(value)) => Ok(value),
        _ => Err(invalid_candidate_result()),
    }
}

/// JSON source text preserves stored integer precision and JSON property lexemes.
/// A missing JSON member is SQL NULL; an explicit JSON null is the text "null".
fn json_source_column(row: &SqlRow, name: &str) -> Result<Value, RuntimeError> {
    match row.get(name) {
        Some(SqlValue::Text(value)) => Ok(Value::String(value.clone())),
        Some(SqlValue::Null) => Ok(Value::Null),
        _ => Err(invalid_candidate_result()),
    }
}

fn scalar_source_column(row: &SqlRow, name: &str) -> Result<Value, RuntimeError> {
    let source = match row.get(name) {
        Some(SqlValue::Null) => "null".to_owned(),
        Some(SqlValue::Integer(value)) => value.to_string(),
        Some(SqlValue::Float(value)) if value.is_finite() => {
            serde_json::to_string(value).map_err(|_| invalid_candidate_result())?
        }
        Some(SqlValue::Text(value)) => {
            serde_json::to_string(value).map_err(|_| invalid_candidate_result())?
        }
        // BLOB and nonfinite REAL have no faithful JSON scalar representation.
        _ => return Err(invalid_candidate_result()),
    };
    Ok(Value::String(source))
}

fn candidate_value(row: &SqlRow, namespaces: &[String]) -> Result<Value, RuntimeError> {
    let id = text_column(row, "id")?;
    let parsed_id = uuid::Uuid::parse_str(id).map_err(|_| invalid_candidate_result())?;
    if parsed_id.to_string() != id {
        return Err(invalid_candidate_result());
    }
    let namespace = text_column(row, "namespace")?;
    if !namespaces.iter().any(|visible| visible == namespace) {
        return Err(invalid_candidate_result());
    }
    let mut buckets = serde_json::Map::new();
    for (field, column) in [
        ("created_at", "created_bucket"),
        ("updated_at", "updated_bucket"),
        ("archived_at", "archived_bucket"),
    ] {
        let bucket = text_column(row, column)?;
        if !BUCKETS.contains(&bucket) {
            return Err(invalid_candidate_result());
        }
        buckets.insert(field.into(), json!(bucket));
    }
    Ok(json!({
        "id": id,
        "namespace": namespace,
        "stored_status": json_source_column(row, "stored_status_json")?,
        "raw": {
            "created_at": scalar_source_column(row, "created_at")?,
            "updated_at": scalar_source_column(row, "updated_at")?,
            "archived_at": json_source_column(row, "archived_json")?,
        },
        "buckets": buckets,
    }))
}

impl GtdPack {
    pub(crate) async fn handle_census(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let has_limit = params.get("limit").is_some();
        let has_cursor = params.get("cursor").is_some();
        let params: CensusParams = serde_json::from_value(params)
            .map_err(|error| RuntimeError::InvalidInput(format!("bad params: {error}")))?;
        if !params.include_candidates && (has_limit || has_cursor) {
            return Err(RuntimeError::InvalidInput(
                "limit and cursor require include_candidates=true".into(),
            ));
        }
        if (has_limit && params.limit.is_none()) || (has_cursor && params.cursor.is_none()) {
            return Err(RuntimeError::InvalidInput(
                "limit and cursor must not be null".into(),
            ));
        }
        let limit = params.limit.unwrap_or(100);
        if !(1..=200).contains(&limit) {
            return Err(RuntimeError::InvalidInput("limit must be 1..=200".into()));
        }
        let mut namespaces: Vec<String> = if token.visible_namespaces().len() > 1 {
            token
                .visible_namespaces()
                .iter()
                .map(|namespace| namespace.as_str().to_owned())
                .collect()
        } else {
            vec![token.namespace().as_str().to_owned()]
        };
        namespaces.sort();
        namespaces.dedup();
        if let Some(cursor) = &params.cursor {
            if !namespaces.contains(&cursor.namespace) {
                return Err(RuntimeError::InvalidInput(
                    "cursor namespace must be in the current census scope".into(),
                ));
            }
            let id = uuid::Uuid::parse_str(&cursor.id).map_err(|_| {
                RuntimeError::InvalidInput("cursor id must be a canonical full UUID".into())
            })?;
            if id.to_string() != cursor.id {
                return Err(RuntimeError::InvalidInput(
                    "cursor id must be a canonical full UUID".into(),
                ));
            }
        }
        let namespace_json = serde_json::to_string(&namespaces)
            .map_err(|error| RuntimeError::Internal(format!("census scope: {error}")))?;
        let sql = self.runtime().sql();
        let mut reader = sql.reader().await.map_err(RuntimeError::Storage)?;
        let rows = reader
            .query_all(SqlStatement {
                sql: SQL.to_owned(),
                params: vec![SqlValue::Text(namespace_json.clone())],
                label: Some("gtd-task-timestamp-census".into()),
            })
            .await
            .map_err(RuntimeError::Storage)?;

        let mut created: BTreeMap<&str, u64> = BUCKETS.into_iter().map(|key| (key, 0)).collect();
        let mut archived = created.clone();
        let mut raw_greater = None;
        for row in rows {
            let (
                Some(SqlValue::Text(field)),
                Some(SqlValue::Text(bucket)),
                Some(SqlValue::Integer(count)),
            ) = (row.get("field"), row.get("bucket"), row.get("count"))
            else {
                return Err(RuntimeError::Internal("invalid task census result".into()));
            };
            let count = u64::try_from(*count)
                .map_err(|_| RuntimeError::Internal("negative task census count".into()))?;
            if field == "comparison" && bucket == "created_at_gt_archived_at_raw" {
                raw_greater = Some(count);
                continue;
            }
            let histogram = match field.as_str() {
                "created_at" => &mut created,
                "archived_at" => &mut archived,
                _ => return Err(RuntimeError::Internal("unknown task census field".into())),
            };
            let slot = histogram
                .get_mut(bucket.as_str())
                .ok_or_else(|| RuntimeError::Internal("unknown task census bucket".into()))?;
            *slot = count;
        }
        let total = |histogram: &BTreeMap<&str, u64>| {
            histogram.values().try_fold(0_u64, |total, count| {
                total
                    .checked_add(*count)
                    .ok_or_else(|| RuntimeError::Internal("task census count overflow".into()))
            })
        };
        let total_tasks = total(&created)?;
        if total_tasks != total(&archived)? {
            return Err(RuntimeError::Internal(
                "inconsistent task census totals".into(),
            ));
        }
        let raw_greater = raw_greater
            .ok_or_else(|| RuntimeError::Internal("missing task census comparison".into()))?;
        let mut response = json!({
            "schema_version": 1,
            "scope": {"kind": "task", "rows": "live_only", "namespaces": namespaces},
            "total_tasks": total_tasks,
            "created_at": created,
            "archived_at": archived,
            "created_at_gt_archived_at_raw": raw_greater,
            "interpretation": "Magnitude buckets do not establish timestamp units. created_at_gt_archived_at_raw compares numeric values only and is NOT temporal ordering. No timestamps are changed.",
        });
        if params.include_candidates {
            let (cursor_namespace, cursor_id) = match params.cursor {
                Some(cursor) => (SqlValue::Text(cursor.namespace), SqlValue::Text(cursor.id)),
                None => (SqlValue::Null, SqlValue::Null),
            };
            let rows = reader
                .query_all(SqlStatement {
                    sql: CANDIDATE_SQL.to_owned(),
                    params: vec![
                        SqlValue::Text(namespace_json),
                        cursor_namespace,
                        cursor_id,
                        SqlValue::Integer(i64::from(limit) + 1),
                    ],
                    label: Some("gtd-task-timestamp-candidates".into()),
                })
                .await
                .map_err(RuntimeError::Storage)?;
            let mut candidates = rows
                .iter()
                .map(|row| candidate_value(row, &namespaces))
                .collect::<Result<Vec<_>, _>>()?;
            let limit = usize::try_from(limit).map_err(|_| invalid_candidate_result())?;
            let has_more = candidates.len() > limit;
            candidates.truncate(limit);
            let next_cursor = if has_more {
                let last = candidates.last().ok_or_else(invalid_candidate_result)?;
                json!({"namespace": last["namespace"], "id": last["id"]})
            } else {
                Value::Null
            };
            response["candidates"] = json!({
                "rows": candidates,
                "next_cursor": next_cursor,
                "expected_buckets": {
                    "created_at": "magnitude_16_digits",
                    "updated_at": "magnitude_16_digits",
                },
            });
        }
        Ok(response)
    }
}
