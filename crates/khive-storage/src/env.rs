//! Shared parsing for numeric environment settings.

/// Read a trimmed environment value as a number.
///
/// Missing, non-Unicode, empty and unparseable values return `None`. The
/// caller chooses the numeric type and retains its own range checks and
/// default; this helper neither logs nor supplies a fallback.
pub fn read_env_number<T: std::str::FromStr>(name: &str) -> Option<T> {
    std::env::var(name).ok()?.trim().parse().ok()
}
