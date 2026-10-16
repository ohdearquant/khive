use khive_storage::types::{SqlRow, SqlStatement, SqlValue};

use crate::sql::sql;

pub(crate) fn statement(
    project_id: &str,
    kind: &str,
    checkpoint_kind: &str,
    max_bytes: i64,
    label: &str,
) -> SqlStatement {
    SqlStatement::new(
        sql!("ingest_cursor_snapshot_select"),
        vec![
            SqlValue::Text(project_id.into()),
            SqlValue::Text(kind.into()),
            SqlValue::Text(checkpoint_kind.into()),
            SqlValue::Integer(max_bytes),
        ],
    )
    .labelled(label)
}

#[derive(Debug)]
pub(crate) struct InvalidColumn;

// Decode fields separately so each consumer retains its validation order and cap policy.
pub(crate) fn value_bytes(row: &SqlRow) -> Result<Option<i64>, InvalidColumn> {
    match row.get("value_bytes") {
        Some(SqlValue::Integer(value)) if *value >= 0 => Ok(Some(*value)),
        Some(SqlValue::Null) => Ok(None),
        _ => Err(InvalidColumn),
    }
}

pub(crate) fn value(row: &SqlRow) -> Result<Option<&[u8]>, InvalidColumn> {
    match row.get("value") {
        Some(SqlValue::Blob(value)) => Ok(Some(value)),
        Some(SqlValue::Null) => Ok(None),
        _ => Err(InvalidColumn),
    }
}
