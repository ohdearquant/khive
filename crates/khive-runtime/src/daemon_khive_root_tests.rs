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

    // MUST-FAIL: building the lock directory on `khive_dir()` resolves the
    // relative last-resort root in the child instead of returning the error.
    // The child drops the test marker too, since under it the default is the
    // shared test namespace rather than the per-user one.
    #[test]
    fn volume_lock_dir_without_a_home_is_an_error_not_the_relative_socket_root() {
        let in_parent = khive_storage::test_support::run_exact_test_in_child(
            "KHIVE_NO_HOME_LOCK_DIR_CHILD",
            false,
            |command| {
                command
                    .env_remove("HOME")
                    .env_remove("USERPROFILE")
                    .env_remove("KHIVE_VOLUME_LOCK_DIR")
                    .env_remove("KHIVE_TEST_HARNESS");
            },
        );
        if in_parent {
            return;
        }
        let error =
            super::volume_lock_dir().expect_err("no home must not resolve a lock directory");
        assert!(
            error.to_string().contains("KHIVE_VOLUME_LOCK_DIR"),
            "{error}"
        );
        // The socket root keeps its historical relative last resort.
        #[cfg(unix)]
        assert_eq!(super::khive_dir(), PathBuf::from("./.khive"));
    }

    #[test]
    fn volume_lock_dir_is_the_storage_layer_rule() {
        assert_eq!(
            super::volume_lock_dir().map_err(|error| error.to_string()),
            khive_db::default_volume_lock_dir().map_err(|error| error.to_string())
        );
    }
}
