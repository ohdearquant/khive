//! Lazy core capabilities retain the backend's readiness and repair ownership.

use khive_storage::{StorageCapability, StorageError};

use crate::error::SqliteError;

mod entity;
mod event;
mod graph;
mod note;
mod sql;

#[cfg(test)]
mod index_tests;
#[cfg(test)]
mod tests;

pub(super) fn map_open_error(
    error: SqliteError,
    capability: StorageCapability,
    operation: &'static str,
) -> StorageError {
    match error {
        SqliteError::RequestReadStopped(error) => error,
        error => error.into_storage_error(capability, operation),
    }
}
