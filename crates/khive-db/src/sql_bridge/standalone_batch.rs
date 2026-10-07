use super::*;

/// One standalone-`execute_batch` failure, paired with the reason the handle
/// was poisoned (dropped instead of restored), if it was.
pub(super) struct BatchFailure {
    pub(super) error: rusqlite::Error,
    pub(super) poison_reason: Option<BatchPoisonReason>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum BatchHandleDisposition {
    Retain,
    Poison,
}

/// Execute one standalone batch while every prepared statement remains scoped
/// to the borrowed connection. The owned [`StandaloneHandle`] stays outside
/// this helper, so it can be restored or dropped only after all statement
/// borrows have ended.
///
/// `admit` runs once `BEGIN IMMEDIATE` holds the database's write lock, the
/// same point at which the queued path samples the reserve; a refusal rolls
/// the empty transaction back and nothing in the batch runs.
pub(super) fn execute_standalone_batch(
    conn: &rusqlite::Connection,
    statements: &[SqlStatement],
    origin: khive_storage::tx_registry::TxOrigin,
    event_rows: Option<&AtomicEventRows>,
    admit: impl FnOnce() -> Result<(), StorageError>,
) -> (
    BatchHandleDisposition,
    Result<u64, StandaloneWriteError<BatchFailure>>,
) {
    let prepared = match prepare_batch_statements(conn, statements) {
        Ok(prepared) => prepared,
        Err(error) => {
            return (
                BatchHandleDisposition::Retain,
                Err(StandaloneWriteError::Sql(BatchFailure {
                    error,
                    poison_reason: None,
                })),
            );
        }
    };
    if let Err(begin_error) = conn.execute_batch("BEGIN IMMEDIATE") {
        // Busy/locked is transient contention (another writer held SQLite's
        // write lock past `busy_timeout`); the connection itself is untouched,
        // so the handle remains reusable. Any other failure leaves transaction
        // state suspect and poisons the handle.
        drop(prepared);
        let (disposition, poison_reason) = if crate::timeout_sink::is_busy_or_locked(&begin_error) {
            (BatchHandleDisposition::Retain, None)
        } else {
            tracing::warn!(
                %begin_error,
                "execute_batch: BEGIN IMMEDIATE failed non-transiently; \
                 poisoning the standalone connection — the handle is \
                 dropped and must be re-acquired"
            );
            (
                BatchHandleDisposition::Poison,
                Some(BatchPoisonReason::BeginFailed),
            )
        };
        return (
            disposition,
            Err(StandaloneWriteError::Sql(BatchFailure {
                error: begin_error,
                poison_reason,
            })),
        );
    }

    if let Err(refused) = admit() {
        drop(prepared);
        if conn.execute_batch("ROLLBACK").is_ok() && conn.is_autocommit() {
            return (
                BatchHandleDisposition::Retain,
                Err(StandaloneWriteError::Refused(refused)),
            );
        }
        tracing::warn!(
            %refused,
            "execute_batch: ROLLBACK after an admission refusal did not restore \
             autocommit; poisoning the standalone connection"
        );
        return (
            BatchHandleDisposition::Poison,
            Err(StandaloneWriteError::Refused(
                SqliteError::WriterSettlementUnknown
                    .into_storage_error(StorageCapability::Sql, "execute_batch"),
            )),
        );
    }

    // Registered only after BEGIN succeeds, and retained through COMMIT or
    // ROLLBACK so the registry never reports a transaction as finished early.
    let _tx_handle =
        khive_storage::tx_registry::register_scoped(Some("execute_batch".to_string()), origin);
    let result = (|| -> Result<u64, rusqlite::Error> {
        let total = execute_prepared_batch(conn, prepared, statements, event_rows)?;
        conn.execute_batch("COMMIT")?;
        Ok(total)
    })();

    let mut disposition = BatchHandleDisposition::Retain;
    let mut poison_reason = None;
    if let Err(error) = &result {
        if let Err(rollback_error) = conn.execute_batch("ROLLBACK") {
            // A failed ROLLBACK leaves the connection in an unknown
            // transaction state. Preserve the original statement error while
            // making the poison cause explicit to the caller.
            tracing::warn!(
                %error,
                %rollback_error,
                "execute_batch: ROLLBACK after statement failure failed; \
                 poisoning the standalone connection — the handle is \
                 dropped and must be re-acquired"
            );
            disposition = BatchHandleDisposition::Poison;
            poison_reason = Some(BatchPoisonReason::RollbackFailed(rollback_error));
        }
    }

    (
        disposition,
        result.map_err(|error| {
            StandaloneWriteError::Sql(BatchFailure {
                error,
                poison_reason,
            })
        }),
    )
}

#[derive(Debug)]
pub(super) enum BatchPoisonReason {
    BeginFailed,
    RollbackFailed(rusqlite::Error),
}

impl std::fmt::Display for BatchPoisonReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BeginFailed => f.write_str(
                "BEGIN IMMEDIATE failed non-transiently; connection transaction state is suspect",
            ),
            Self::RollbackFailed(error) => {
                write!(f, "ROLLBACK after statement failure failed: {error}")
            }
        }
    }
}

/// A `rusqlite::Error` whose display carries the poison context, so a
/// poisoned handle is visible to the caller in the returned error instead of
/// being discoverable only through later calls' generic "connection already
/// consumed" failures.
#[derive(Debug)]
pub(super) struct PoisonedBatchError {
    pub(super) original: rusqlite::Error,
    pub(super) poison_reason: BatchPoisonReason,
}

impl std::fmt::Display for PoisonedBatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}; original error: {}",
            self.poison_reason, self.original
        )
    }
}

impl std::error::Error for PoisonedBatchError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.original)
    }
}
