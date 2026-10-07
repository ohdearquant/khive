use super::*;

/// A refused standalone admission, or the SQLite error of the admitted write.
pub(super) enum StandaloneWriteError<E> {
    Refused(StorageError),
    Sql(E),
}

/// Admission for one write on the write-queue-off standalone path, the same
/// steps a queued request takes at dequeue: the volume lease first (unless an
/// enclosing atomic unit already holds it), then the reserve sample, which for
/// VACUUM also compares its copy-sized headroom. The caller keeps the returned
/// lease until the operation has settled.
pub(super) fn admit_standalone_operation(
    pool: &ConnectionPool,
    unit_holds_lease: bool,
    operation: &'static str,
    vacuum: bool,
) -> Result<Option<crate::disk_guard::VolumeLease>, StorageError> {
    let admission = pool.write_admission();
    let to_storage =
        |error: SqliteError| error.into_storage_error(StorageCapability::Sql, operation);
    let lease = acquire_standalone_lease(pool, unit_holds_lease, operation)?;
    if vacuum {
        admission.check_for_vacuum()
    } else {
        admission.check()
    }
    .map_err(to_storage)?;
    Ok(lease)
}

/// The volume lease for one queue-off standalone write, unless an enclosing
/// atomic unit already holds it. `execute_batch` takes only this before its
/// `BEGIN IMMEDIATE` and samples the reserve inside the transaction.
pub(super) fn acquire_standalone_lease(
    pool: &ConnectionPool,
    unit_holds_lease: bool,
    operation: &'static str,
) -> Result<Option<crate::disk_guard::VolumeLease>, StorageError> {
    if unit_holds_lease {
        return Ok(None);
    }
    pool.write_admission()
        .acquire()
        .map_err(|error| error.into_storage_error(StorageCapability::Sql, operation))
}

/// Take the volume lease for a whole manual atomic unit on a blocking thread
/// (acquisition polls an advisory lock). The lease then travels with the unit
/// across threads, so it stops naming the thread that took it: a later request
/// from that pool thread waits like any other contender instead of reading as
/// re-entry.
pub(super) async fn acquire_unit_lease(
    pool: Arc<ConnectionPool>,
) -> khive_storage::types::StorageResult<Option<crate::disk_guard::DetachedVolumeLease>> {
    tokio::task::spawn_blocking(move || {
        let lease = pool
            .write_admission()
            .acquire()
            .map_err(|error| error.into_storage_error(StorageCapability::Sql, "atomic_unit"))?;
        Ok(lease.map(crate::disk_guard::VolumeLease::detach_from_thread))
    })
    .await
    .map_err(|error| StorageError::driver(StorageCapability::Sql, "atomic_unit", error))?
}
