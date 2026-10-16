//! Environment readers shared with lower-level khive-types consumers.

pub use khive_types::env::{cached_env_flag, env_flag, env_parse_or};

#[cfg(test)]
mod tests {
    use super::{cached_env_flag, env_flag, env_parse_or};
    use crate::test_process::run_in_child;
    use std::sync::OnceLock;

    const KEY: &str = "KHIVE_DB_ENV_HELPERS_TEST_VALUE";

    #[test]
    fn parsed_defaults_preserve_exact_input_and_read_each_time() {
        if run_in_child(|command| {
            command.env_remove(KEY);
        }) {
            return;
        }
        assert_eq!(env_parse_or(KEY, 17_u64), 17);
        crate::test_process::set_var(KEY, "42");
        assert_eq!(env_parse_or(KEY, 17_u64), 42);
        crate::test_process::set_var(KEY, "-7");
        assert_eq!(env_parse_or(KEY, 17_i64), -7);
        assert_eq!(env_parse_or(KEY, 17_u64), 17);
        for value in ["", " 42 ", "invalid", "18446744073709551616"] {
            crate::test_process::set_var(KEY, value);
            assert_eq!(env_parse_or(KEY, 17_u64), 17, "{value:?}");
        }
        crate::test_process::set_var(KEY, "256");
        assert_eq!(env_parse_or(KEY, 9_u8), 9);
        crate::test_process::set_var(KEY, "NaN");
        assert!(env_parse_or(KEY, 1.0_f64).is_nan());
        crate::test_process::set_var(KEY, "41");
        assert_eq!(env_parse_or(KEY, 17_u64), 41);
    }

    #[test]
    fn flags_keep_explicit_false_distinct_from_fallback() {
        if run_in_child(|command| {
            command.env_remove(KEY);
        }) {
            return;
        }
        assert!(!env_flag(KEY, false));
        assert!(env_flag(KEY, true));
        for value in ["1", "true", "yes", "on", " TrUe ", "\tYES\n"] {
            crate::test_process::set_var(KEY, value);
            assert!(env_flag(KEY, false), "{value:?}");
        }
        for value in ["0", "false", "no", "off", " FaLsE ", "\tOFF\n"] {
            crate::test_process::set_var(KEY, value);
            assert!(!env_flag(KEY, true), "{value:?}");
        }
        for value in ["", " ", "enabled", "2"] {
            crate::test_process::set_var(KEY, value);
            assert!(!env_flag(KEY, false), "{value:?}");
            assert!(env_flag(KEY, true), "{value:?}");
        }
    }

    #[test]
    fn cached_flags_keep_the_first_value_and_separate_cells() {
        static FIRST: OnceLock<bool> = OnceLock::new();
        static SECOND: OnceLock<bool> = OnceLock::new();
        if run_in_child(|command| {
            command.env_remove(KEY);
        }) {
            return;
        }
        assert!(cached_env_flag(KEY, true, &FIRST));
        crate::test_process::set_var(KEY, "false");
        assert!(!env_flag(KEY, true));
        assert!(cached_env_flag(KEY, false, &FIRST));
        assert!(!cached_env_flag(KEY, true, &SECOND));
        crate::test_process::set_var(KEY, "true");
        assert!(env_flag(KEY, false));
        assert!(!cached_env_flag(KEY, false, &SECOND));
        crate::test_process::remove_var(KEY);
        assert!(!cached_env_flag(KEY, true, &SECOND));
    }

    #[cfg(unix)]
    #[test]
    fn non_unicode_values_use_the_supplied_defaults() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;
        static CACHE: OnceLock<bool> = OnceLock::new();
        if run_in_child(|command| {
            command.env_remove(KEY);
        }) {
            return;
        }
        crate::test_process::set_var(KEY, OsString::from_vec(vec![0xff]));
        assert_eq!(env_parse_or(KEY, 23_u32), 23);
        assert!(!env_flag(KEY, false));
        assert!(env_flag(KEY, true));
        assert!(cached_env_flag(KEY, true, &CACHE));
    }
}
