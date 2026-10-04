//! SQLite construction constrained by an already held physical file claim.
use super::*;
use crate::file_identity::DatabaseFileIdentity;

impl StorageBackend {
    /// Refuse any database other than `expected` before initializing identity or WAL.
    /// `read_only` retains the explicit topology mode; a writable request still
    /// honors an existing file's filesystem read-only mode.
    ///
    /// On Unix, the host must initialize the claimed-file observer at process
    /// startup before any SQLite I/O, under `pool::initialize_claimed_file_observer`'s
    /// unsafe precondition. Unsupported or uninitialized Unix hosts refuse.
    /// Native open-time I/O is outside the pre-SQL refusal guarantee.
    pub fn sqlite_with_claimed_file_identity(
        path: impl AsRef<Path>,
        max_readers: Option<usize>,
        wal_ceiling: WalCeilingPolicy,
        read_only: bool,
        expected: DatabaseFileIdentity,
    ) -> Result<Self, SqliteError> {
        let config = PoolConfig {
            expected_file_identity: Some(expected),
            wal_ceiling,
            ..PoolConfig::default()
        };
        if read_only {
            Self::sqlite_read_only_with_pool_config(path, config, max_readers)
        } else {
            Self::sqlite_with_pool_config(path, config, max_readers)
        }
    }
}
