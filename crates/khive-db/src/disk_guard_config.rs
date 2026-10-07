use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

use crate::SqliteError;

const RESERVE_ENV: &str = "KHIVE_SQLITE_DISK_RESERVE_BYTES";
const LEGACY_RESERVE_ENV: &str = "KHIVE_DB_FREE_SPACE_FLOOR_BYTES";
const DEADLINE_ENV: &str = "KHIVE_SQLITE_DISK_GUARD_DEADLINE_MS";
const VOLUME_LOCK_DIR_ENV: &str = "KHIVE_VOLUME_LOCK_DIR";
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
        let reserve = parse_optional(RESERVE_ENV, self.reserve.as_deref(), false)?;
        let legacy = parse_optional(LEGACY_RESERVE_ENV, self.legacy_reserve.as_deref(), true)?;
        let deadline = parse_optional(DEADLINE_ENV, self.deadline.as_deref(), false)?;
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

/// The legacy reserve keeps the grammar it had before this resolver, which
/// accepted one leading `+`; existing installations must not start failing.
fn parse_optional(
    name: &str,
    raw: Option<&OsStr>,
    legacy: bool,
) -> Result<Option<u64>, SqliteError> {
    raw.map(|raw| {
        raw.to_str()
            .map(|value| match value.strip_prefix('+') {
                Some(unsigned) if legacy => unsigned,
                _ => value,
            })
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

/// The directory that holds the volume advisory lock files shared by every
/// khive process of one user.
///
/// `KHIVE_VOLUME_LOCK_DIR` wins when it is set and not blank. Otherwise the
/// per-user runtime namespace `<home>/.khive/sqlite-volume-locks` is used, where
/// `<home>` is the first non-blank of `HOME` and `USERPROFILE`. Every process of
/// that user therefore resolves the same directory, whatever its working
/// directory. With none of these set, or when the resolved directory is not
/// absolute, the result is a configuration error rather than a relative path,
/// because a working-directory-relative lock directory would give two processes
/// two different lock files.
///
/// A process carrying the workspace test marker `KHIVE_TEST_HARNESS=1` (cargo
/// sets it for every test and test-spawned binary) uses
/// `<temp>/khive-test-sqlite-volume-locks` in place of the per-user namespace,
/// so tests share one namespace among themselves and never wait on a lease
/// held by an installed process of the same user. The explicit override still
/// wins under the marker.
pub fn default_volume_lock_dir() -> Result<PathBuf, SqliteError> {
    let test_harness = std::env::var(crate::pool::TEST_HARNESS_ENV).as_deref() == Ok("1");
    volume_lock_dir_from(
        std::env::var_os(VOLUME_LOCK_DIR_ENV),
        test_harness.then(|| std::env::temp_dir().join(TEST_HARNESS_LOCK_SUBDIR)),
        std::env::var_os("HOME"),
        std::env::var_os("USERPROFILE"),
    )
}

const TEST_HARNESS_LOCK_SUBDIR: &str = "khive-test-sqlite-volume-locks";

/// A caller's optional lock directory, or the configuration error that
/// [`default_volume_lock_dir`] reports when no directory can be resolved.
pub fn require_volume_lock_dir(configured: Option<PathBuf>) -> Result<PathBuf, SqliteError> {
    configured.ok_or_else(unresolved_volume_lock_dir)
}

/// Env-free core of [`default_volume_lock_dir`], so the order is testable
/// without mutating process-global environment variables.
fn volume_lock_dir_from(
    override_dir: Option<OsString>,
    test_harness_dir: Option<PathBuf>,
    home: Option<OsString>,
    userprofile: Option<OsString>,
) -> Result<PathBuf, SqliteError> {
    let is_set = |value: &OsString| !value.to_str().is_some_and(|text| text.trim().is_empty());
    let directory = match (override_dir.filter(is_set), test_harness_dir) {
        (Some(directory), _) => PathBuf::from(directory),
        (None, Some(directory)) => directory,
        (None, None) => {
            let home = home
                .filter(is_set)
                .or_else(|| userprofile.filter(is_set))
                .ok_or_else(unresolved_volume_lock_dir)?;
            PathBuf::from(home)
                .join(".khive")
                .join("sqlite-volume-locks")
        }
    };
    if !directory.is_absolute() {
        return Err(SqliteError::InvalidConfig(format!(
            "SQLite volume-lock directory {directory:?} is not absolute: {VOLUME_LOCK_DIR_ENV} \
             must be absolute, and so must HOME or USERPROFILE when it is unset"
        )));
    }
    Ok(directory)
}

fn unresolved_volume_lock_dir() -> SqliteError {
    SqliteError::InvalidConfig(format!(
        "no SQLite volume-lock directory: {VOLUME_LOCK_DIR_ENV} is unset and neither HOME nor \
         USERPROFILE names a home directory; set {VOLUME_LOCK_DIR_ENV} to an absolute directory \
         shared by every khive process of this user"
    ))
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
        for raw in ["", "-1", "++1", "+", "1.5", "1 ", "18446744073709551616"] {
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

    #[test]
    fn only_the_legacy_reserve_accepts_a_leading_plus() {
        let legacy = DiskGuardEnvironment {
            legacy_reserve: Some("+1073741824".into()),
            ..Default::default()
        }
        .resolve(None, None)
        .unwrap();
        assert_eq!(legacy.reserve_bytes, 1_073_741_824);
        assert_eq!(
            legacy.reserve_source,
            DiskGuardConfigSource::LegacyEnvironment
        );

        for environment in [
            DiskGuardEnvironment {
                reserve: Some("+1073741824".into()),
                ..Default::default()
            },
            DiskGuardEnvironment {
                deadline: Some("+2000".into()),
                ..Default::default()
            },
        ] {
            assert!(matches!(
                environment.resolve(None, None),
                Err(SqliteError::InvalidConfig(_))
            ));
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

    fn lock_dir(
        override_dir: Option<&str>,
        home: Option<&str>,
        userprofile: Option<&str>,
    ) -> Result<PathBuf, SqliteError> {
        volume_lock_dir_from(
            override_dir.map(OsString::from),
            None,
            home.map(OsString::from),
            userprofile.map(OsString::from),
        )
    }

    fn per_user_lock_dir(home: &str) -> PathBuf {
        PathBuf::from(home)
            .join(".khive")
            .join("sqlite-volume-locks")
    }

    #[test]
    fn volume_lock_dir_follows_override_then_home_then_userprofile() {
        assert_eq!(
            lock_dir(Some("/locks"), Some("/home/a"), Some("/profile/a")).unwrap(),
            PathBuf::from("/locks")
        );
        assert_eq!(
            lock_dir(None, Some("/home/a"), Some("/profile/a")).unwrap(),
            per_user_lock_dir("/home/a")
        );
        assert_eq!(
            lock_dir(None, None, Some("/profile/a")).unwrap(),
            per_user_lock_dir("/profile/a")
        );
    }

    #[test]
    fn volume_lock_dir_skips_empty_and_blank_values() {
        assert_eq!(
            lock_dir(Some(""), Some("/home/a"), None).unwrap(),
            per_user_lock_dir("/home/a")
        );
        assert_eq!(
            lock_dir(None, Some(""), Some("/profile/a")).unwrap(),
            per_user_lock_dir("/profile/a")
        );
        assert_eq!(
            lock_dir(None, Some("  "), Some("/profile/a")).unwrap(),
            per_user_lock_dir("/profile/a")
        );
        for blank in ["   ", "\t", " \n "] {
            assert_eq!(
                lock_dir(Some(blank), Some("/home/a"), Some("/profile/a")).unwrap(),
                per_user_lock_dir("/home/a")
            );
        }
        assert_eq!(
            lock_dir(Some("   "), None, Some("/profile/a")).unwrap(),
            per_user_lock_dir("/profile/a")
        );
    }

    #[test]
    fn volume_lock_dir_refuses_a_relative_directory() {
        for (override_dir, home, userprofile) in [
            (Some("locks"), Some("/home/a"), None),
            (Some("./locks"), None, Some("/profile/a")),
            (None, Some("home/a"), Some("/profile/a")),
            (None, None, Some("profile/a")),
            (None, Some("."), None),
        ] {
            match lock_dir(override_dir, home, userprofile) {
                Err(SqliteError::InvalidConfig(message)) => {
                    assert!(message.contains("KHIVE_VOLUME_LOCK_DIR"), "{message}");
                    assert!(message.contains("must be absolute"), "{message}");
                }
                other => panic!(
                    "expected a configuration error for {override_dir:?} {home:?} \
                     {userprofile:?}, got {other:?}"
                ),
            }
        }
    }

    #[test]
    fn volume_lock_dir_without_a_home_is_an_error_naming_the_override_variable() {
        for (override_dir, home, userprofile) in [
            (None, None, None),
            (Some(""), Some(""), Some("")),
            (None, Some("  "), Some("\t")),
        ] {
            match lock_dir(override_dir, home, userprofile) {
                Err(SqliteError::InvalidConfig(message)) => {
                    assert!(message.contains("KHIVE_VOLUME_LOCK_DIR"), "{message}");
                }
                other => panic!("expected a configuration error, got {other:?}"),
            }
        }
    }

    #[test]
    fn volume_lock_dir_under_the_test_marker_is_the_harness_namespace() {
        let harness = PathBuf::from("/tmp/khive-test-sqlite-volume-locks");
        let resolve = |override_dir: Option<&str>, harness: Option<PathBuf>| {
            volume_lock_dir_from(
                override_dir.map(OsString::from),
                harness,
                Some(OsString::from("/home/a")),
                None,
            )
            .unwrap()
        };
        assert_eq!(resolve(None, Some(harness.clone())), harness);
        assert_eq!(resolve(None, None), per_user_lock_dir("/home/a"));
        assert_eq!(
            resolve(Some("/locks"), Some(harness)),
            PathBuf::from("/locks"),
            "the explicit override still wins under the marker"
        );
    }

    #[test]
    fn default_volume_lock_dir_with_the_test_marker_is_the_temp_namespace() {
        if crate::test_process::run_in_child(|command| {
            command
                .env_remove(VOLUME_LOCK_DIR_ENV)
                .env(crate::pool::TEST_HARNESS_ENV, "1")
                .env("HOME", "/home/marker-present");
        }) {
            return;
        }
        assert_eq!(
            default_volume_lock_dir().unwrap(),
            std::env::temp_dir().join(TEST_HARNESS_LOCK_SUBDIR)
        );
    }

    #[test]
    fn default_volume_lock_dir_without_the_test_marker_is_the_per_user_namespace() {
        if crate::test_process::run_in_child(|command| {
            command
                .env_remove(VOLUME_LOCK_DIR_ENV)
                .env_remove(crate::pool::TEST_HARNESS_ENV)
                .env("HOME", "/home/marker-absent");
        }) {
            return;
        }
        assert_eq!(
            default_volume_lock_dir().unwrap(),
            per_user_lock_dir("/home/marker-absent")
        );
    }

    #[test]
    fn required_volume_lock_dir_names_the_override_variable_when_none_resolved() {
        assert_eq!(
            require_volume_lock_dir(Some(PathBuf::from("/locks"))).unwrap(),
            PathBuf::from("/locks")
        );
        assert!(matches!(
            require_volume_lock_dir(None),
            Err(SqliteError::InvalidConfig(message)) if message.contains("KHIVE_VOLUME_LOCK_DIR")
        ));
    }
}
