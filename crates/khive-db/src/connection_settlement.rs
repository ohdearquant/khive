use crate::error::SqliteError;
use rusqlite::hooks::{AuthContext, Authorization};
use rusqlite::Connection;

/// The caller owns this connection and retains its physical-volume lease.
/// Clearing an authorizer is permitted only after the connection is retired.
///
/// Returns `Ok` once the connection is closed or back in autocommit. When it
/// can neither roll back nor close, the outcome of its transaction is unknown:
/// the result is [`SqliteError::WriterSettlementUnknown`]. The caller must then
/// refuse every later write on that database and may release its lease, and the
/// host decides whether the process exits.
pub(crate) fn close_retired_connection(
    original: Connection,
    database_path: &str,
    volume_key: &str,
) -> Result<(), SqliteError> {
    let authorizer_error = original
        .authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
        .err();
    #[cfg(test)]
    if FORCE_ROLLBACK_DENIAL.with(std::cell::Cell::get) {
        original
            .authorizer(Some(|context: AuthContext<'_>| match context.action {
                rusqlite::hooks::AuthAction::Transaction {
                    operation: rusqlite::hooks::TransactionOperation::Rollback,
                } => Authorization::Deny,
                _ => Authorization::Allow,
            }))
            .unwrap();
    }
    let rollback = if original.is_autocommit() {
        Ok(())
    } else {
        original.execute_batch("ROLLBACK")
    };
    let Err((unclosed, close_error)) = original.close() else {
        return Ok(());
    };
    if unclosed.is_autocommit() {
        tracing::warn!(
            database_path,
            volume_key,
            %close_error,
            "retired SQLite connection is settled but could not close"
        );
        #[cfg(test)]
        eprintln!(
            "OWNED_SETTLEMENT_AUTOCOMMIT_RECOVERED: path={database_path} \
             volume={volume_key} close_error={close_error}"
        );
        drop(unclosed);
        return Ok(());
    }
    let rollback_error = rollback
        .err()
        .map(|error| error.to_string())
        .unwrap_or_else(|| "ROLLBACK returned success without restoring autocommit".to_string());
    eprintln!(
        "owned SQLite settlement failed; outcome unknown: path={database_path} \
         volume={volume_key} rollback_error={rollback_error} close_error={close_error} \
         authorizer_error={authorizer_error:?}"
    );
    tracing::error!(
        database_path,
        volume_key,
        %rollback_error,
        %close_error,
        ?authorizer_error,
        "owned SQLite settlement failed; outcome unknown"
    );
    // The handle cannot be closed, so SQLite keeps it open with its
    // transaction. Dropping the wrapper does not change that.
    drop(unclosed);
    Err(SqliteError::WriterSettlementUnknown)
}

#[cfg(test)]
thread_local! {
    static FORCE_ROLLBACK_DENIAL: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(all(test, unix))]
pub(crate) fn force_rollback_denial_for_test() {
    FORCE_ROLLBACK_DENIAL.with(|force| force.set(true));
}

#[cfg(all(test, unix))]
#[path = "connection_settlement_tests.rs"]
mod tests;
