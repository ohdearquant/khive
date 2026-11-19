//! Pure statement builders for caller-owned SQL execution.
//!
//! These helpers do not acquire storage, open transactions or grant namespace
//! access. The executor retains native errors, settlement and retry policy.

use khive_storage::{SqlStatement, SqlValue, StorageCapability, StorageError};
use uuid::Uuid;

pub use crate::migrations::schema_version_probe;

/// Read the live keyed-note holder, without choosing a writer or diagnostic label.
pub fn live_holder_read(namespace: &str, kind: &str, key: &str) -> SqlStatement {
    SqlStatement::new(
        include_str!("../sql/live-holder-read.sql").trim_end(),
        vec![
            SqlValue::Text(namespace.into()),
            SqlValue::Text(kind.into()),
            SqlValue::Text(key.into()),
        ],
    )
}

/// Build a properties patch for `notes` or `entities`, guarded by scalar values.
///
/// Keys are full dotted JSON paths (`$.status`, `$.receipt.state`) with nonempty
/// ASCII alphanumeric/underscore segments. Every path and value is bound.
/// Expected values use SQLite `IS` equality after JSON scalar extraction:
/// absence and JSON null are equivalent, booleans compare as 1/0, and numeric
/// coercion follows SQLite. This is not structural JSON or SQL-variant equality.
/// Blob, complex expected JSON and non-finite Float values are refused.
///
/// SET Json values retain their JSON types; ordinary Bool binds store 1/0 and
/// ordinary Text binds remain strings. Duplicate/overlapping SET paths retain
/// caller order. Validation precedes an empty SET's mutation-free empty list.
/// An empty expected list is an id/live/object-guarded patch, not a fence.
///
/// Notes retain their automatic version trigger; entity updates explicitly
/// advance the version once, as required by their existing version guard.
/// The caller owns namespace/kind admission, updated_at, status/due projections,
/// lease/removal guards and coupled writes in the same owning atomic unit.
/// The returned statement has no label or transaction-control statements.
pub fn property_cas_statements(
    table: &str,
    id: Uuid,
    expected: &[(&str, SqlValue)],
    set: &[(&str, SqlValue)],
) -> Result<Vec<SqlStatement>, StorageError> {
    let (prefix, advance_version) = match table {
        "notes" => (
            "UPDATE notes SET properties = json_set(COALESCE(properties, '{}')",
            false,
        ),
        "entities" => (
            "UPDATE entities SET properties = json_set(COALESCE(properties, '{}')",
            true,
        ),
        _ => return Err(invalid("unsupported property CAS table".into())),
    };
    validate_entries(expected, true)?;
    validate_entries(set, false)?;
    if set.is_empty() {
        return Ok(Vec::new());
    }

    let mut params = vec![SqlValue::Uuid(id)];
    let mut sql = prefix.to_owned();
    for (path, value) in set {
        params.push(SqlValue::Text((*path).into()));
        let path_index = params.len();
        params.push(value.clone());
        let value_index = params.len();
        if matches!(value, SqlValue::Json(_)) {
            sql.push_str(&format!(", ?{path_index}, json(?{value_index})"));
        } else {
            sql.push_str(&format!(", ?{path_index}, ?{value_index}"));
        }
    }
    sql.push(')');
    if advance_version {
        sql.push_str(", version = version + 1");
    }
    sql.push_str(
        " WHERE id=?1 AND deleted_at IS NULL AND (properties IS NULL OR json_type(properties)='object')",
    );
    for (path, value) in expected {
        params.push(SqlValue::Text((*path).into()));
        let path_index = params.len();
        params.push(value.clone());
        let value_index = params.len();
        sql.push_str(&format!(" AND json_extract(properties, ?{path_index}) IS "));
        if matches!(value, SqlValue::Json(_)) {
            sql.push_str(&format!("json_extract(?{value_index}, '$')"));
        } else {
            sql.push_str(&format!("?{value_index}"));
        }
    }
    Ok(vec![SqlStatement::new(sql, params)])
}

fn validate_entries(entries: &[(&str, SqlValue)], expected: bool) -> Result<(), StorageError> {
    let list = if expected { "expected" } else { "set" };
    for (index, (path, value)) in entries.iter().enumerate() {
        if !path.strip_prefix("$.").is_some_and(|tail| {
            tail.split('.').all(|part| {
                !part.is_empty() && part.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
            })
        }) {
            return Err(invalid(format!(
                "invalid JSON path in {list} entry {index}"
            )));
        }
        let refusal = match value {
            SqlValue::Blob(_) => Some("unsupported Blob"),
            SqlValue::Float(value) if !value.is_finite() => Some("non-finite Float"),
            SqlValue::Json(value) if expected && (value.is_array() || value.is_object()) => {
                Some("unsupported expected JSON container")
            }
            _ => None,
        };
        if let Some(refusal) = refusal {
            return Err(invalid(format!("{refusal} in {list} entry {index}")));
        }
    }
    Ok(())
}

fn invalid(message: String) -> StorageError {
    StorageError::InvalidInput {
        capability: StorageCapability::Sql,
        operation: "property_cas_statements".into(),
        message,
    }
}

#[cfg(test)]
#[path = "statement_lists_tests.rs"]
mod tests;
