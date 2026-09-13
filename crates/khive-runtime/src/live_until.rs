//! The `live_until` deadline predicate, shared by every route that carries one.
//!
//! A caller pins liveness by naming a dotted path into the fenced document that
//! holds an RFC 3339 deadline. The store re-reads that path inside the writing
//! transaction and compares it against one clock reading taken after writer
//! admission, which closes the window between the caller's own liveness read and
//! the commit. The predicate lives here rather than at either call site because
//! the guarantee must not differ by route: a batch observation and a single-head
//! fence resolve the same path, against the same clock, and refuse for the same
//! two reasons.

use crate::presentation::micros_to_iso;
use khive_storage::types::{SqlStatement, SqlValue};
use khive_storage::{SqlWriter, StorageError};
use serde_json::Value;

/// Why a `live_until` path did not admit the write. The caller learns the
/// deadline it pinned when that value parsed, and only the JSON type otherwise:
/// the path is caller-chosen, so echoing whatever it lands on would read an
/// arbitrary field of the document back out.
pub(crate) enum DeadlineRefusal {
    Expired { value: String, now: String },
    Unreadable { value_type: &'static str },
}

impl DeadlineRefusal {
    pub(crate) fn reason(&self) -> &'static str {
        match self {
            Self::Expired { .. } => "expired",
            Self::Unreadable { .. } => "live_until_unreadable",
        }
    }

    /// The evidence fields, in the order both routes report them.
    pub(crate) fn details(self) -> Vec<(&'static str, String)> {
        match self {
            Self::Expired { value, now } => vec![("value", value), ("now", now)],
            Self::Unreadable { value_type } => vec![("value_type", value_type.into())],
        }
    }
}

/// One clock reading for a writing transaction, taken after writer admission so
/// every deadline in that transaction is compared against the same instant.
pub(crate) async fn writer_clock(
    writer: &mut dyn SqlWriter,
    label: &'static str,
) -> Result<i64, StorageError> {
    match writer
        .query_scalar(SqlStatement {
            sql: "SELECT khive_now_micros()".into(),
            params: vec![],
            label: Some(label.into()),
        })
        .await?
    {
        Some(SqlValue::Integer(now)) => Ok(now),
        _ => Err(StorageError::Internal(
            "invalid live_until writer clock".into(),
        )),
    }
}

/// Resolve `field` in the live note at `(kind, key)` and compare it to `now`.
/// `Ok(None)` admits the write.
pub(crate) async fn evaluate(
    writer: &mut dyn SqlWriter,
    namespace: &str,
    kind: &str,
    key: &str,
    field: &str,
    now: i64,
    label: &'static str,
) -> Result<Option<DeadlineRefusal>, StorageError> {
    let content = writer
        .query_scalar(SqlStatement {
            sql: "SELECT content FROM notes WHERE namespace=?1 AND kind=?2 AND key=?3 AND deleted_at IS NULL".into(),
            params: vec![
                SqlValue::Text(namespace.into()),
                SqlValue::Text(kind.into()),
                SqlValue::Text(key.into()),
            ],
            label: Some(label.into()),
        })
        .await?;
    let doc: Value = match content {
        Some(SqlValue::Text(content)) => serde_json::from_str(&content).unwrap_or(Value::Null),
        _ => Value::Null,
    };
    let found = field
        .split('.')
        .try_fold(&doc, |value, part| value.get(part));
    let value = found.unwrap_or(&Value::Null);
    let deadline = value
        .as_str()
        .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok());
    let clock = chrono::DateTime::from_timestamp_micros(now)
        .ok_or_else(|| StorageError::Internal("invalid live_until writer clock".into()))?;
    Ok(match deadline {
        Some(deadline) if deadline > clock => None,
        Some(_) => Some(DeadlineRefusal::Expired {
            value: value.to_string(),
            now: micros_to_iso(now),
        }),
        None => Some(DeadlineRefusal::Unreadable {
            value_type: found.map_or("absent", json_type_name),
        }),
    })
}

/// The JSON type of a `live_until` field, which an unreadable refusal reports in
/// place of the value itself.
fn json_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}
