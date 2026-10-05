#[cfg(test)]
mod khive_root_tests {
    use super::khive_root_from;
    use std::path::PathBuf;

    #[test]
    fn home_wins_when_set() {
        assert_eq!(
            khive_root_from(Some("/base/home".into()), Some("/other".into())),
            PathBuf::from("/base/home/.khive")
        );
    }

    #[test]
    fn userprofile_backfills_missing_home() {
        assert_eq!(
            khive_root_from(None, Some("/profile/home".into())),
            PathBuf::from("/profile/home/.khive")
        );
    }

    #[test]
    fn blank_values_are_skipped() {
        assert_eq!(
            khive_root_from(Some("  ".into()), Some("/profile/home".into())),
            PathBuf::from("/profile/home/.khive")
        );
    }

    #[cfg(not(unix))]
    #[test]
    fn fallback_is_never_cwd_relative() {
        // On non-unix the lock anchor must not depend on the caller's working
        // directory even when no home variable is set — a relative path here
        // would give the same database different lock files per cwd.
        assert!(khive_root_from(None, None).is_absolute());
    }

    #[cfg(unix)]
    #[test]
    fn unix_fallback_stays_historical_not_shared_tmp() {
        // On unix the no-HOME last resort deliberately stays "./.khive"
        // rather than a shared world-writable anchor like /tmp, which a
        // local attacker could pre-claim to intercept the daemon socket.
        assert_eq!(khive_root_from(None, None), PathBuf::from("./.khive"));
    }
}
