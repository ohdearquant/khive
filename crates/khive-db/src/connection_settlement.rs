use rusqlite::hooks::{AuthContext, Authorization};
use rusqlite::Connection;

/// The caller owns this connection and retains its physical-volume lease.
/// Clearing an authorizer is permitted only after the connection is retired.
pub(crate) fn close_retired_connection(
    original: Connection,
    database_path: &str,
    volume_key: &str,
) {
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
    if let Err((unclosed, close_error)) = original.close() {
        if unclosed.is_autocommit() {
            tracing::warn!(database_path, volume_key, %close_error, "retired SQLite connection is settled but could not close");
            #[cfg(test)]
            eprintln!("OWNED_SETTLEMENT_AUTOCOMMIT_RECOVERED: path={database_path} volume={volume_key} close_error={close_error}");
            drop(unclosed);
            return;
        }
        let rollback_error = rollback
            .err()
            .map(|error| error.to_string())
            .unwrap_or_else(|| {
                "ROLLBACK returned success without restoring autocommit".to_string()
            });
        eprintln!("owned SQLite settlement failed; aborting: path={database_path} volume={volume_key} rollback_error={rollback_error} close_error={close_error} authorizer_error={authorizer_error:?}");
        tracing::error!(database_path, volume_key, %rollback_error, %close_error, ?authorizer_error, "owned SQLite settlement failed; aborting");
        std::process::abort();
    }
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
