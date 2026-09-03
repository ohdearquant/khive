//! Per-substrate SQLite store implementations.
//!
//! Each module provides a concrete store struct implementing one or more
//! `khive-storage` capability traits against the shared connection pool.

use std::sync::Arc;

use khive_storage::error::StorageError;
use khive_storage::types::StorageResult;
use khive_storage::StorageCapability;

use crate::pool::ConnectionPool;

pub mod agents;
pub mod attachment;
pub mod blob;
pub mod blob_s3;
pub mod entity;
pub mod event;
pub mod graph;
pub mod note;
pub mod sparse;
pub mod text;
pub mod vectors;

/// Run one typed-store read through the pool's bounded reader admission.
///
/// File-backed and in-memory stores deliberately share this one route. Pool
/// exhaustion is returned as the canonical retryable `AdmissionTimeout`; a
/// cancelled request remains the non-retryable `Timeout`. There is no
/// standalone-reader fallback (ADR-165 Slice 2).
pub(crate) async fn run_pooled_store_read<F, R>(
    pool: Arc<ConnectionPool>,
    capability: StorageCapability,
    operation: &'static str,
    read: F,
) -> StorageResult<R>
where
    F: FnOnce(&rusqlite::Connection) -> Result<R, StorageError> + Send + 'static,
    R: Send + 'static,
{
    crate::read_cancellation::run_declared_interruptible_read(capability, operation, move |scope| {
        let mut guard = pool.resolve_reader_checkout(
            capability,
            operation,
            pool.reader_until(|| scope.should_stop()),
        )?;
        scope.run_pooled_reader(&mut guard, read)
    })
    .await
}
