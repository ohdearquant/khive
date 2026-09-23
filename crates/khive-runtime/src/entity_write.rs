//! Entity revision preconditions, evaluated by the shared writer transaction.

use khive_storage::{SqlStatement, SqlValue, SqlWriter, StorageError};
use khive_types::{Details, KhiveError};
use uuid::Uuid;

use crate::{RuntimeError, RuntimeResult};

/// A caller's entity revision did not match the live writer-transaction row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntityVersionConflict {
    pub expected: i64,
    pub current: i64,
}

impl EntityVersionConflict {
    pub fn into_error(self) -> KhiveError {
        KhiveError::conflict("entity version precondition failed").with_details(Details::new_owned(
            vec![
                ("reason", "version_conflict".to_owned()),
                ("expected_version", self.expected.to_string()),
                ("current_version", self.current.to_string()),
            ],
        ))
    }
}

#[derive(Debug, Clone)]
pub(crate) struct EntityWriteGuard {
    pub(crate) id: Uuid,
    pub(crate) expected_version: i64,
}

pub(crate) fn validate_expected_version(expected: Option<i64>) -> RuntimeResult<()> {
    if expected.is_some_and(|value| value < 1) {
        return Err(RuntimeError::InvalidInput(
            "expected_version must be a positive integer".into(),
        ));
    }
    Ok(())
}

impl EntityWriteGuard {
    pub(crate) async fn check(
        &self,
        writer: &mut dyn SqlWriter,
    ) -> Result<Option<EntityVersionConflict>, StorageError> {
        let current = writer
            .query_scalar(SqlStatement {
                sql: "SELECT version FROM entities WHERE id=?1 AND deleted_at IS NULL".into(),
                params: vec![SqlValue::Text(self.id.to_string())],
                label: Some("entity-version-precondition".into()),
            })
            .await?;
        // A missing/deleted row is refused by the following snapshot CAS,
        // matching the note guard's missing-row behavior.
        Ok(match current {
            Some(SqlValue::Integer(current)) if current != self.expected_version => {
                Some(EntityVersionConflict {
                    expected: self.expected_version,
                    current,
                })
            }
            _ => None,
        })
    }
}

#[cfg(test)]
#[path = "entity_write_tests.rs"]
mod tests;
