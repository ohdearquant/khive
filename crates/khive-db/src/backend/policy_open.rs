use super::*;

impl StorageBackend {
    /// File-backed SQLite database whose volume advisory lock files live in
    /// `volume_lock_dir`. Every cooperating process must choose the same directory.
    pub fn sqlite_with_volume_lock_dir(
        path: impl AsRef<Path>,
        volume_lock_dir: std::path::PathBuf,
    ) -> Result<Self, SqliteError> {
        Self::sqlite_with_pool_config(
            path,
            PoolConfig {
                volume_lock_dir: Some(volume_lock_dir),
                ..PoolConfig::default()
            },
            None,
        )
    }

    /// The host supplies the same captured policies used for daemon identity.
    pub fn sqlite_with_max_readers_and_policies(
        path: impl AsRef<Path>,
        max_readers: Option<usize>,
        wal_ceiling: WalCeilingPolicy,
        disk_guard_config: crate::EffectiveDiskGuardConfig,
        volume_lock_dir: std::path::PathBuf,
    ) -> Result<Self, SqliteError> {
        Self::sqlite_with_pool_config(
            path,
            PoolConfig {
                wal_ceiling,
                disk_guard_config: Some(disk_guard_config),
                volume_lock_dir: Some(volume_lock_dir),
                ..PoolConfig::default()
            },
            max_readers,
        )
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn sqlite_for_test_with_policies(
        path: impl AsRef<Path>,
        wal_ceiling: WalCeilingPolicy,
        disk_guard_config: crate::EffectiveDiskGuardConfig,
    ) -> Result<Self, SqliteError> {
        Self::sqlite_with_pool_config(
            path,
            PoolConfig {
                wal_ceiling,
                disk_guard_config: Some(disk_guard_config),
                ..PoolConfig::for_test()
            },
            Some(2),
        )
    }
}
