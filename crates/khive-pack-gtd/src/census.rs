use std::collections::BTreeMap;

use khive_runtime::{NamespaceToken, RuntimeError};
use khive_storage::{SqlStatement, SqlValue};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::GtdPack;

const SQL: &str = include_str!("../sql/task-timestamp-census.sql");
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
struct CensusParams {}

impl GtdPack {
    pub(crate) async fn handle_census(
        &self,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let _: CensusParams = serde_json::from_value(params)
            .map_err(|error| RuntimeError::InvalidInput(format!("bad params: {error}")))?;
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
        let namespace_json = serde_json::to_string(&namespaces)
            .map_err(|error| RuntimeError::Internal(format!("census scope: {error}")))?;
        let sql = self.runtime().sql();
        let mut reader = sql.reader().await.map_err(RuntimeError::Storage)?;
        let rows = reader
            .query_all(SqlStatement {
                sql: SQL.to_owned(),
                params: vec![SqlValue::Text(namespace_json)],
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
        Ok(json!({
            "schema_version": 1,
            "scope": {"kind": "task", "rows": "live_only", "namespaces": namespaces},
            "total_tasks": total_tasks,
            "created_at": created,
            "archived_at": archived,
            "created_at_gt_archived_at_raw": raw_greater,
            "interpretation": "Magnitude buckets do not establish timestamp units. created_at_gt_archived_at_raw compares numeric values only and is NOT temporal ordering. No timestamps are changed.",
        }))
    }
}
