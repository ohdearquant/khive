//! Dedicated code-map runtime construction.

use khive_db::StorageBackend;

use crate::{KhiveRuntime, RuntimeConfig, RuntimeError, RuntimeResult};

impl KhiveRuntime {
    /// Construct a dedicated code-map runtime. Every connection in its pool
    /// selects the native handle-proving VFS before the first schema read or
    /// migration. The caller prepares the omitted-db parent; an explicit
    /// target receives no path transform or directory creation here, so its
    /// parent directory must already exist. A code-map database runs in
    /// rollback-journal mode, so a configured WAL ceiling does not apply to it.
    pub fn new_code_map(
        mut config: RuntimeConfig,
        protected_main: Vec<std::path::PathBuf>,
        protected_events: Vec<std::path::PathBuf>,
    ) -> RuntimeResult<Self> {
        if config.db_path.is_none() {
            return Err(RuntimeError::InvalidInput(
                "code-map runtime requires a guarded file-backed database".into(),
            ));
        }
        let disk_guard = config
            .resolve_disk_guard_policy(false)?
            .expect("code-map has a file-backed disk policy");
        let volume_lock_dir = config.volume_lock_dir.clone();
        Self::new_with_file_backend(config, false, move |path| {
            StorageBackend::sqlite_code_map_with_policies(
                path,
                &protected_main,
                &protected_events,
                disk_guard,
                khive_db::require_volume_lock_dir(volume_lock_dir)?,
            )
        })
    }
}
