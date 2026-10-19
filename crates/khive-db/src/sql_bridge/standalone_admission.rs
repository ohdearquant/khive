use super::*;

/// A refused standalone admission, or the SQLite error of the admitted write.
pub(super) enum StandaloneWriteError<E> {
    Refused(StorageError),
    Sql(E),
}

/// Admission for one write on the write-queue-off standalone path, the same
/// steps a queued request takes at dequeue: the volume lease first, then the
/// reserve sample, which for VACUUM also compares its copy-sized headroom.
/// The caller keeps the returned
/// lease until the operation has settled.
pub(super) fn admit_standalone_operation(
    pool: &ConnectionPool,
    operation: &'static str,
    vacuum: bool,
) -> Result<Option<crate::disk_guard::VolumeLease>, StorageError> {
    let admission = pool.write_admission();
    let to_storage =
        |error: SqliteError| error.into_storage_error(StorageCapability::Sql, operation);
    let lease = acquire_standalone_lease(pool, operation)?;
    if vacuum {
        admission.check_for_vacuum()
    } else {
        admission.check()
    }
    .map_err(to_storage)?;
    Ok(lease)
}

/// The volume lease for one queue-off standalone write. `execute_batch` takes
/// only this before its `BEGIN IMMEDIATE` and samples the reserve inside the
/// transaction.
pub(super) fn acquire_standalone_lease(
    pool: &ConnectionPool,
    operation: &'static str,
) -> Result<Option<crate::disk_guard::VolumeLease>, StorageError> {
    pool.write_admission()
        .acquire()
        .map_err(|error| error.into_storage_error(StorageCapability::Sql, operation))
}
