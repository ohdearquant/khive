//! Small environment readers for parsed defaults and explicit boolean flags.
//!
//! These helpers do not emit configuration audit events or apply caller-specific
//! range constraints. Presence-only flags and non-Unicode-sensitive policies
//! need their existing readers.

use std::str::FromStr;
use std::sync::OnceLock;

/// Read once and parse the exact Unicode value, without trimming or caching.
///
/// Missing, non-Unicode and unparseable values use `default`. Values accepted
/// by `T::from_str`, including non-finite floats, need any caller-specific checks.
pub fn env_parse_or<T: FromStr>(name: &str, default: T) -> T {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

/// Read a trimmed, ASCII-case-insensitive explicit boolean flag.
///
/// `1`, `true`, `yes` and `on` enable; `0`, `false`, `no` and `off` disable.
/// Missing, non-Unicode and unrecognized values use `default`.
pub fn env_flag(name: &str, default: bool) -> bool {
    std::env::var(name)
        .ok()
        .map(|value| match value.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => true,
            "0" | "false" | "no" | "off" => false,
            _ => default,
        })
        .unwrap_or(default)
}

/// Resolve an explicit flag once using the caller's process-lifetime cell.
///
/// Dedicate each cell to one setting. The first initialization fixes its value;
/// later changes to the environment, name or default do not replace that value.
/// Callers that require a configuration audit event must retain that policy.
pub fn cached_env_flag(name: &str, default: bool, cache: &'static OnceLock<bool>) -> bool {
    *cache.get_or_init(|| env_flag(name, default))
}

#[cfg(test)]
mod tests {
    use super::{cached_env_flag, env_flag, env_parse_or};
    use crate::test_process::{remove_var, run_in_child, set_var};
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
        set_var(KEY, "42");
        assert_eq!(env_parse_or(KEY, 17_u64), 42);
        set_var(KEY, "-7");
        assert_eq!(env_parse_or(KEY, 17_i64), -7);
        assert_eq!(env_parse_or(KEY, 17_u64), 17);
        for value in ["", " 42 ", "invalid", "18446744073709551616"] {
            set_var(KEY, value);
            assert_eq!(env_parse_or(KEY, 17_u64), 17, "{value:?}");
        }
        set_var(KEY, "256");
        assert_eq!(env_parse_or(KEY, 9_u8), 9);
        set_var(KEY, "NaN");
        assert!(env_parse_or(KEY, 1.0_f64).is_nan());
        set_var(KEY, "41");
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
            set_var(KEY, value);
            assert!(env_flag(KEY, false), "{value:?}");
        }
        for value in ["0", "false", "no", "off", " FaLsE ", "\tOFF\n"] {
            set_var(KEY, value);
            assert!(!env_flag(KEY, true), "{value:?}");
        }
        for value in ["", " ", "enabled", "2"] {
            set_var(KEY, value);
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
        set_var(KEY, "false");
        assert!(!env_flag(KEY, true));
        assert!(cached_env_flag(KEY, false, &FIRST));
        assert!(!cached_env_flag(KEY, true, &SECOND));
        set_var(KEY, "true");
        assert!(env_flag(KEY, false));
        assert!(!cached_env_flag(KEY, false, &SECOND));
        remove_var(KEY);
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
        set_var(KEY, OsString::from_vec(vec![0xff]));
        assert_eq!(env_parse_or(KEY, 23_u32), 23);
        assert!(!env_flag(KEY, false));
        assert!(env_flag(KEY, true));
        assert!(cached_env_flag(KEY, true, &CACHE));
    }
}
