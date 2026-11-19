use super::*;

impl BackendConfig {
    /// Resolve from the host's captured environment; never sample live environment here.
    pub fn resolve_disk_guard(
        &self,
        environment: &khive_db::DiskGuardEnvironment,
    ) -> Result<Option<khive_db::EffectiveDiskGuardConfig>, ConfigError> {
        let invalid = |reason: String| ConfigError::InvalidBackendDiskGuard {
            name: self.name.clone(),
            reason,
        };
        if self.kind == BackendKind::Memory && self.disk_reserve_bytes.is_some_and(|n| n != 0) {
            return Err(invalid(
                "nonzero disk_reserve_bytes requires a file-backed SQLite backend".into(),
            ));
        }
        if self
            .disk_guard_deadline_ms
            .is_some_and(|n| !(100..=10_000).contains(&n))
        {
            return Err(invalid(
                "disk_guard_deadline_ms must be between 100 and 10000".into(),
            ));
        }
        if self.kind == BackendKind::Memory || self.read_only {
            return Ok(None);
        }
        environment
            .resolve(self.disk_reserve_bytes, self.disk_guard_deadline_ms)
            .map(Some)
            .map_err(|e| invalid(e.to_string()))
    }
}
