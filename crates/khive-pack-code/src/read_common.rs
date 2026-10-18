//! Validation and sanitized failures shared by the registered-map read verbs.

use khive_runtime::RuntimeError;

pub(crate) fn invalid(message: impl Into<String>) -> RuntimeError {
    RuntimeError::InvalidInput(message.into())
}

pub(crate) fn db_error(_: rusqlite::Error) -> RuntimeError {
    // Database errors may contain the configured path or SQL input.
    RuntimeError::Internal("code map query failed".into())
}

pub(crate) fn required_text(value: &str, name: &str) -> Result<(), RuntimeError> {
    if value.trim().is_empty() {
        return Err(invalid(format!("{name} must be a non-empty string")));
    }
    Ok(())
}

pub(crate) fn bounded(
    value: Option<u64>,
    name: &str,
    default: u64,
    maximum: u64,
) -> Result<usize, RuntimeError> {
    let value = value.unwrap_or(default);
    if !(1..=maximum).contains(&value) {
        return Err(invalid(format!(
            "{name} must be an integer in 1..={maximum}"
        )));
    }
    Ok(value as usize)
}
