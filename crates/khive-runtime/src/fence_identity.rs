//! The live-holder read and the identity predicate, shared by every route that
//! fences on a key.
//!
//! A caller that read a key pins the note it read, so an observation does not
//! survive that note's recreation: a recreated note starts at version 1, and a
//! version comparison alone cannot tell the note the caller read from a
//! different note that happens to sit at the same number. The read and the
//! refusal live here rather than at either call site because the guarantee must
//! not differ by route.

use khive_storage::types::{SqlStatement, SqlValue};
use khive_storage::{SqlWriter, StorageError};
use uuid::Uuid;

/// The live note holding a `(kind, key)`, as read inside the writing
/// transaction. Absence is the caller's `None`, which is the version half's
/// business rather than the identity half's: there is no identity to name.
pub(crate) struct Holder {
    pub id: Uuid,
    pub version: i64,
}

/// Read the live holder of `(kind, key)` in `namespace`.
pub(crate) async fn read_holder(
    writer: &mut dyn SqlWriter,
    namespace: &str,
    kind: &str,
    key: &str,
    label: &'static str,
) -> Result<Option<Holder>, StorageError> {
    let row = writer
        .query_row(SqlStatement {
            sql: "SELECT id, version FROM notes WHERE namespace=?1 AND kind=?2 AND key=?3 AND deleted_at IS NULL".into(),
            params: vec![
                SqlValue::Text(namespace.into()),
                SqlValue::Text(kind.into()),
                SqlValue::Text(key.into()),
            ],
            label: Some(label.into()),
        })
        .await?;
    row.map(|row| {
        let id = match row.get("id") {
            Some(SqlValue::Text(id)) => Uuid::parse_str(id)
                .map_err(|_| StorageError::Internal("invalid fenced note identity".into()))?,
            _ => {
                return Err(StorageError::Internal(
                    "invalid fenced note identity".into(),
                ))
            }
        };
        let version = match row.get("version") {
            Some(SqlValue::Integer(version)) => *version,
            _ => return Err(StorageError::Internal("invalid fenced note version".into())),
        };
        Ok(Holder { id, version })
    })
    .transpose()
}

/// The evidence fields an identity refusal reports, in the order both routes
/// report them.
pub(crate) fn identity_evidence(asserted: Uuid, current: Uuid) -> Vec<(&'static str, String)> {
    vec![
        ("id", asserted.to_string()),
        ("current_id", current.to_string()),
    ]
}
