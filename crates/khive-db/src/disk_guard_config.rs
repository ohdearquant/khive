use std::ffi::{OsStr, OsString};

use crate::SqliteError;

const RESERVE_ENV: &str = "KHIVE_SQLITE_DISK_RESERVE_BYTES";
const LEGACY_RESERVE_ENV: &str = "KHIVE_DB_FREE_SPACE_FLOOR_BYTES";
const DEADLINE_ENV: &str = "KHIVE_SQLITE_DISK_GUARD_DEADLINE_MS";
pub(crate) const DEFAULT_DISK_RESERVE_BYTES: u64 = 1_073_741_824;
pub(crate) const DEFAULT_DISK_GUARD_DEADLINE_MS: u64 = 2_000;

/// Provenance is diagnostic; daemon compatibility uses only effective numbers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiskGuardConfigSource {
    Backend,
    Environment,
    LegacyEnvironment,
    Default,
}

impl DiskGuardConfigSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Backend => "backend",
            Self::Environment => "environment",
            Self::LegacyEnvironment => "legacy_environment",
            Self::Default => "default",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EffectiveDiskGuardConfig {
    pub reserve_bytes: u64,
    pub guard_deadline_ms: u64,
    pub reserve_source: DiskGuardConfigSource,
    pub deadline_source: DiskGuardConfigSource,
    pub legacy_environment_present: bool,
}

impl Default for EffectiveDiskGuardConfig {
    fn default() -> Self {
        Self {
            reserve_bytes: DEFAULT_DISK_RESERVE_BYTES,
            guard_deadline_ms: DEFAULT_DISK_GUARD_DEADLINE_MS,
            reserve_source: DiskGuardConfigSource::Default,
            deadline_source: DiskGuardConfigSource::Default,
            legacy_environment_present: false,
        }
    }
}

impl EffectiveDiskGuardConfig {
    pub fn validate(&self) -> Result<(), SqliteError> {
        if !(100..=10_000).contains(&self.guard_deadline_ms) {
            return Err(SqliteError::InvalidConfig(format!(
                "disk_guard_deadline_ms must be in [100, 10000] ms, got {}",
                self.guard_deadline_ms
            )));
        }
        Ok(())
    }
}

/// One construction-time snapshot shared by backend opening and config identity.
/// Raw values remain intact so invalid Unicode and overflow fail at validation.
#[derive(Clone, Debug, Default)]
pub struct DiskGuardEnvironment {
    pub reserve: Option<OsString>,
    pub legacy_reserve: Option<OsString>,
    pub deadline: Option<OsString>,
}

impl DiskGuardEnvironment {
    pub fn capture() -> Self {
        Self {
            reserve: std::env::var_os(RESERVE_ENV),
            legacy_reserve: std::env::var_os(LEGACY_RESERVE_ENV),
            deadline: std::env::var_os(DEADLINE_ENV),
        }
    }

    /// The legacy setting remains a fallback. Equal old/new values are accepted;
    /// conflicting values must be removed even when a backend overrides them.
    pub fn resolve(
        &self,
        reserve_override: Option<u64>,
        deadline_override: Option<u64>,
    ) -> Result<EffectiveDiskGuardConfig, SqliteError> {
        let reserve = parse_optional(RESERVE_ENV, self.reserve.as_deref())?;
        let legacy = parse_optional(LEGACY_RESERVE_ENV, self.legacy_reserve.as_deref())?;
        let deadline = parse_optional(DEADLINE_ENV, self.deadline.as_deref())?;
        if let (Some(new), Some(old)) = (reserve, legacy) {
            if new != old {
                return Err(SqliteError::InvalidConfig(format!(
                    "{RESERVE_ENV} and {LEGACY_RESERVE_ENV} conflict"
                )));
            }
        }
        let (reserve_bytes, reserve_source) = if let Some(value) = reserve_override {
            (value, DiskGuardConfigSource::Backend)
        } else if let Some(value) = reserve {
            (value, DiskGuardConfigSource::Environment)
        } else if let Some(value) = legacy {
            (value, DiskGuardConfigSource::LegacyEnvironment)
        } else {
            (DEFAULT_DISK_RESERVE_BYTES, DiskGuardConfigSource::Default)
        };
        let (guard_deadline_ms, deadline_source) = if let Some(value) = deadline_override {
            (value, DiskGuardConfigSource::Backend)
        } else if let Some(value) = deadline {
            (value, DiskGuardConfigSource::Environment)
        } else {
            (
                DEFAULT_DISK_GUARD_DEADLINE_MS,
                DiskGuardConfigSource::Default,
            )
        };
        let policy = EffectiveDiskGuardConfig {
            reserve_bytes,
            guard_deadline_ms,
            reserve_source,
            deadline_source,
            legacy_environment_present: legacy.is_some(),
        };
        policy.validate()?;
        Ok(policy)
    }
}

fn parse_optional(name: &str, raw: Option<&OsStr>) -> Result<Option<u64>, SqliteError> {
    raw.map(|raw| {
        raw.to_str()
            .filter(|value| !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
            .and_then(|value| value.parse().ok())
            .ok_or_else(|| {
                SqliteError::InvalidConfig(format!("{name} must be an unsigned decimal integer"))
            })
    })
    .transpose()
}

pub fn resolve_disk_guard_config(
    reserve_override: Option<u64>,
    deadline_override: Option<u64>,
) -> Result<EffectiveDiskGuardConfig, SqliteError> {
    DiskGuardEnvironment::capture().resolve(reserve_override, deadline_override)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effective_numbers_follow_backend_environment_legacy_default_precedence() {
        let empty = DiskGuardEnvironment::default();
        assert_eq!(
            empty.resolve(None, None).unwrap(),
            EffectiveDiskGuardConfig::default()
        );
        let legacy = DiskGuardEnvironment {
            legacy_reserve: Some("123".into()),
            deadline: Some("2500".into()),
            ..Default::default()
        };
        let policy = legacy.resolve(None, None).unwrap();
        assert_eq!(
            (policy.reserve_bytes, policy.guard_deadline_ms),
            (123, 2500)
        );
        assert_eq!(
            policy.reserve_source,
            DiskGuardConfigSource::LegacyEnvironment
        );
        assert!(policy.legacy_environment_present);
        let current = DiskGuardEnvironment {
            reserve: Some("123".into()),
            ..legacy
        };
        assert_eq!(
            current.resolve(None, None).unwrap().reserve_source,
            DiskGuardConfigSource::Environment
        );
        let overridden = current.resolve(Some(0), Some(100)).unwrap();
        assert_eq!(
            (overridden.reserve_bytes, overridden.guard_deadline_ms),
            (0, 100)
        );
        assert_eq!(overridden.reserve_source, DiskGuardConfigSource::Backend);
        assert_eq!(overridden.deadline_source, DiskGuardConfigSource::Backend);
        assert!(empty.resolve(Some(u64::MAX), Some(10_000)).is_ok());
    }

    #[test]
    fn conflicting_legacy_environment_is_never_silently_ignored() {
        let environment = DiskGuardEnvironment {
            reserve: Some("100".into()),
            legacy_reserve: Some("200".into()),
            ..Default::default()
        };
        for override_value in [None, Some(300)] {
            assert!(matches!(environment.resolve(override_value, None),
                Err(SqliteError::InvalidConfig(message)) if message.contains("conflict")));
        }
    }

    #[test]
    fn malformed_environment_and_unbounded_deadlines_are_rejected() {
        for raw in ["", "-1", "+1", "1.5", "1 ", "18446744073709551616"] {
            for environment in [
                DiskGuardEnvironment {
                    reserve: Some(raw.into()),
                    ..Default::default()
                },
                DiskGuardEnvironment {
                    legacy_reserve: Some(raw.into()),
                    ..Default::default()
                },
                DiskGuardEnvironment {
                    deadline: Some(raw.into()),
                    ..Default::default()
                },
            ] {
                assert!(matches!(
                    environment.resolve(Some(0), Some(2000)),
                    Err(SqliteError::InvalidConfig(_))
                ));
            }
        }
        for deadline in [0, 99, 10_001, u64::MAX] {
            assert!(EffectiveDiskGuardConfig {
                guard_deadline_ms: deadline,
                ..Default::default()
            }
            .validate()
            .is_err());
            assert!(DiskGuardEnvironment::default()
                .resolve(None, Some(deadline))
                .is_err());
            assert!(DiskGuardEnvironment {
                deadline: Some(deadline.to_string().into()),
                ..Default::default()
            }
            .resolve(None, None)
            .is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn non_unicode_environment_is_a_configuration_error() {
        use std::os::unix::ffi::OsStringExt;
        let environment = DiskGuardEnvironment {
            reserve: Some(OsString::from_vec(vec![0xff])),
            ..Default::default()
        };
        assert!(matches!(
            environment.resolve(None, None),
            Err(SqliteError::InvalidConfig(_))
        ));
    }
}
