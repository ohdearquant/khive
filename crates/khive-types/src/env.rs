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
