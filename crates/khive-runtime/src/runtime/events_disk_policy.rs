//! The disk policy and lock directory the events sidecar shares with the main pool.

use super::*;

impl KhiveRuntime {
    /// The sidecar shares main's actual policy, including programmatic overrides.
    pub fn events_disk_guard_policy(&self) -> RuntimeResult<khive_db::EffectiveDiskGuardConfig> {
        let pool = self.core_backend.as_ref().unwrap_or(&self.backend).pool();
        pool.effective_disk_guard_config()
            .map(Ok)
            .unwrap_or_else(|| {
                self.config
                    .disk_guard_environment
                    .resolve(None, None)
                    .map_err(Into::into)
            })
    }

    /// The sidecar shares main's lock directory. `None` means neither the main
    /// pool nor this runtime's configuration could resolve one.
    pub fn events_volume_lock_dir(&self) -> Option<std::path::PathBuf> {
        self.core_backend
            .as_ref()
            .unwrap_or(&self.backend)
            .pool()
            .config()
            .volume_lock_dir
            .clone()
            .or_else(|| self.config.volume_lock_dir.clone())
    }
}
