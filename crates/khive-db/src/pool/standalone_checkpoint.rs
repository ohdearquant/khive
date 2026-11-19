//! Owned checkpoint capabilities with no ordinary SQL interface.

use rusqlite::Connection;

use super::{CheckpointResult, ConnectionPool};
use crate::checkpoint::{CheckpointConfig, CheckpointCoreOutcome, TruncateState};

/// The dedicated checkpoint task's opened file. Only fixed recovery
/// operations leave this module; its native connection cannot escape.
pub(crate) struct CheckpointFile {
    conn: Connection,
}

impl CheckpointFile {
    pub(super) fn new(conn: Connection) -> Self {
        if let Err(error) = conn.pragma_update(None, "wal_autocheckpoint", 0) {
            tracing::warn!(target: "khive_db::checkpoint", %error,
                "could not disable autocheckpoint on the dedicated checkpoint connection");
        }
        Self { conn }
    }

    pub(crate) fn run_core(
        &self,
        pool: &ConnectionPool,
        config: &CheckpointConfig,
        state: &mut TruncateState,
    ) -> Result<CheckpointCoreOutcome, rusqlite::Error> {
        crate::checkpoint::checkpoint_once_core(pool, &self.conn, config, state)
    }

    pub(crate) fn run(
        &self,
        pool: &ConnectionPool,
        config: &CheckpointConfig,
        state: &mut TruncateState,
    ) -> Result<u64, rusqlite::Error> {
        crate::checkpoint::checkpoint_once(pool, &self.conn, config, state)
    }

    fn checkpoint(&self, sql: &str) -> Result<CheckpointResult, rusqlite::Error> {
        self.conn.query_row(sql, [], |row| {
            Ok(CheckpointResult {
                busy: row.get(0)?,
                log_frames: row.get(1)?,
                checkpointed_frames: row.get(2)?,
            })
        })
    }

    #[cfg(test)]
    pub(crate) fn wal_autocheckpoint_pages_for_test(&self) -> rusqlite::Result<u32> {
        self.conn
            .query_row("PRAGMA wal_autocheckpoint", [], |row| row.get(0))
    }

    #[cfg(test)]
    pub(crate) fn truncate_requests_attribution_for_test(
        &self,
        pool: &ConnectionPool,
        config: &CheckpointConfig,
        wal_pages_before: u64,
        state: &mut TruncateState,
    ) -> bool {
        crate::checkpoint::truncate_requests_attribution_for_test(
            pool,
            &self.conn,
            config,
            wal_pages_before,
            state,
        )
    }
}

/// A retained standalone connection restricted to WAL checkpoints.
///
/// It remains available below the logical-write reserve. It cannot issue
/// arbitrary SQL or lend its connection to a caller:
/// ```compile_fail
/// use khive_db::ConnectionPool;
/// fn bypass(pool: &ConnectionPool) {
///     pool.open_standalone_checkpoint().unwrap()
///         .execute_batch("CREATE TABLE bypass(value INTEGER)");
/// }
/// ```
pub struct StandaloneCheckpoint<'pool> {
    pool: &'pool ConnectionPool,
    file: CheckpointFile,
}

impl<'pool> StandaloneCheckpoint<'pool> {
    pub(super) fn new(pool: &'pool ConnectionPool, file: CheckpointFile) -> Self {
        Self { pool, file }
    }

    /// Run the configured checkpoint cycle and update this pool's metrics.
    pub fn run(
        &self,
        config: &CheckpointConfig,
        state: &mut TruncateState,
    ) -> Result<u64, rusqlite::Error> {
        self.file.run(self.pool, config, state)
    }

    /// Run the fixed PASSIVE checkpoint without logical-write admission.
    pub fn passive(&self) -> Result<CheckpointResult, rusqlite::Error> {
        self.file.checkpoint("PRAGMA wal_checkpoint(PASSIVE)")
    }

    /// Run the fixed TRUNCATE checkpoint without logical-write admission.
    pub fn truncate(&self) -> Result<CheckpointResult, rusqlite::Error> {
        self.file.checkpoint("PRAGMA wal_checkpoint(TRUNCATE)")
    }
}
