//! Deterministic corpus export from one read statement and its SQLite snapshot.

use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError};
use khive_storage::{SqlStatement, SqlValue};
use serde::Deserialize;
use serde_json::{json, Value};

use super::schema::SectionType;
use super::util::{deser, sql_err};
use super::KnowledgeHandlers;

#[derive(Default, Deserialize)]
#[serde(rename_all = "lowercase")]
enum ExportFormat {
    #[default]
    Jsonl,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExportParams {
    #[serde(default)]
    format: ExportFormat,
}

impl KnowledgeHandlers {
    pub(crate) async fn export(
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
        params: Value,
    ) -> Result<Value, RuntimeError> {
        let ExportParams {
            format: ExportFormat::Jsonl,
        } = deser(params)?;
        let namespace = token.namespace().as_str();
        let sql = runtime.sql();
        let mut reader = sql
            .reader()
            .await
            .map_err(|e| sql_err("export reader", e))?;
        // One UNION statement keeps all three tables in the same read snapshot
        // without taking a writer or leaving a read transaction open.
        let rows = reader
            .query_all(SqlStatement {
                sql: khive_runtime::sql!("knowledge_export").into(),
                params: vec![SqlValue::Text(namespace.to_owned())],
                label: Some("knowledge.export".into()),
            })
            .await
            .map_err(|e| sql_err("export corpus", e))?;
        drop(reader);

        let (mut atoms, mut domains, mut sections) = (0usize, 0usize, 0usize);
        let mut data = String::new();
        for row in rows {
            let record_type = row
                .text("record_type")
                .map_err(|e| sql_err("export row type", e))?;
            let mut record: Value =
                serde_json::from_str(row.text("record").map_err(|e| sql_err("export row", e))?)
                    .map_err(|e| sql_err("export JSON", e))?;
            match record_type {
                "atom" => atoms += 1,
                "domain" => domains += 1,
                "section" => {
                    sections += 1;
                    if record
                        .get("section_type")
                        .and_then(Value::as_str)
                        .is_some_and(SectionType::is_retired_name)
                    {
                        record["retired"] = json!(true);
                    }
                }
                other => {
                    return Err(RuntimeError::Internal(format!(
                        "unknown export row type {other:?}"
                    )))
                }
            }
            // Explicit even when serde_json's current Map uses a BTreeMap: a
            // downstream preserve_order feature must not change the dump bytes.
            record.sort_all_objects();
            data.push_str(&serde_json::to_string(&record).map_err(|e| sql_err("export JSON", e))?);
            data.push('\n');
        }
        Ok(json!({
            "format": "jsonl", "namespace": namespace, "data": data,
            "counts": {"atoms": atoms, "domains": domains, "sections": sections},
        }))
    }
}
