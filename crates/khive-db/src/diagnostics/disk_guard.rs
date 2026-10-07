use super::*;

/// The configured write-admission reserve and the volume metadata used to
/// sample it. The reserve is an admission floor, not reserved disk capacity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DiskGuardDiagnostics {
    pub enabled: bool,
    pub effective_reserve_bytes: Option<u64>,
    pub reserve_source: Option<String>,
    pub guard_deadline_ms: Option<u64>,
    pub deadline_source: Option<String>,
    pub probe_path: Option<String>,
    pub volume_identity: Option<String>,
    pub volume_root: Option<String>,
    pub unavailable_reason: Option<String>,
}

impl DiskGuardDiagnostics {
    pub(super) fn snapshot(pool: &ConnectionPool) -> Self {
        let Some(policy) = pool.effective_disk_guard_config() else {
            return Self {
                enabled: false,
                effective_reserve_bytes: None,
                reserve_source: None,
                guard_deadline_ms: None,
                deadline_source: None,
                probe_path: None,
                volume_identity: None,
                volume_root: None,
                unavailable_reason: Some(if pool.config().read_only {
                    "read-only backend: no disk-write admission".to_string()
                } else {
                    "in-memory backend: no filesystem volume to guard".to_string()
                }),
            };
        };
        let identity = pool.write_admission().volume_identity();
        let (probe_path, volume_identity, volume_root, unavailable_reason) = match identity {
            Ok(Some(volume)) => (
                Some(volume.probe_path().display().to_string()),
                Some(volume.diagnostic_key()),
                volume.volume_root().map(|path| path.display().to_string()),
                None,
            ),
            Ok(None) => (
                None,
                None,
                None,
                Some("writable disk guard has no canonical database path".to_string()),
            ),
            Err(error) => (None, None, None, Some(error.to_string())),
        };
        Self {
            enabled: true,
            effective_reserve_bytes: Some(policy.reserve_bytes),
            reserve_source: Some(policy.reserve_source.as_str().to_string()),
            guard_deadline_ms: Some(policy.guard_deadline_ms),
            deadline_source: Some(policy.deadline_source.as_str().to_string()),
            probe_path,
            volume_identity,
            volume_root,
            unavailable_reason,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::disk_guard::VolumeIdentity;
    use crate::pool::PoolConfig;

    #[test]
    fn disk_guard_diagnostics_report_effective_policy_and_stable_volume() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("disk-guard-diagnostics.db");
        let captured_volume = VolumeIdentity::resolve(&path).expect("initial volume identity");
        let pool = ConnectionPool::new(PoolConfig {
            path: Some(path),
            disk_guard_config: Some(
                crate::DiskGuardEnvironment::default()
                    .resolve(Some(123), Some(500))
                    .unwrap(),
            ),
            write_queue_enabled: Some(false),
            ..PoolConfig::for_test()
        })
        .expect("writable pool");
        let diagnostics = DiskGuardDiagnostics::snapshot(&pool);
        assert!(diagnostics.enabled);
        assert_eq!(diagnostics.effective_reserve_bytes, Some(123));
        assert_eq!(diagnostics.reserve_source.as_deref(), Some("backend"));
        assert_eq!(diagnostics.guard_deadline_ms, Some(500));
        assert_eq!(diagnostics.deadline_source.as_deref(), Some("backend"));
        assert_eq!(
            diagnostics.volume_identity,
            Some(captured_volume.diagnostic_key())
        );
        let probe_path = captured_volume.probe_path().display().to_string();
        assert_eq!(diagnostics.probe_path.as_deref(), Some(probe_path.as_str()));
        assert!(diagnostics.unavailable_reason.is_none());
        let json = serde_json::to_value(&diagnostics).unwrap();
        assert_eq!(json["effective_reserve_bytes"], 123);
        assert_eq!(json["reserve_source"], "backend");
        #[cfg(any(unix, windows))]
        {
            pool.write_admission()
                .set_test_current_volume(Some(captured_volume.different_volume_for_test()));
            let unavailable = DiskGuardDiagnostics::snapshot(&pool);
            assert!(unavailable.volume_identity.is_none());
            assert!(unavailable.probe_path.is_none());
            assert!(unavailable
                .unavailable_reason
                .as_deref()
                .unwrap()
                .contains("database volume changed"));
            pool.write_admission().set_test_current_volume(None);
            assert_eq!(DiskGuardDiagnostics::snapshot(&pool), diagnostics);
        }
    }
}
