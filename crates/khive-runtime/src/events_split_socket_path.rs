//! Socket pathname admission for the events split.

use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use super::absolutize;
use crate::config::RuntimeConfig;
use crate::error::{RuntimeError, RuntimeResult};

/// Refuse an events socket pathname that cannot fit the platform address field.
/// The daemon anchors relative paths before binding, so preflight counts that
/// same absolute spelling, including the terminating NUL.
pub fn validate_events_socket_path(socket_path: &Path) -> RuntimeResult<()> {
    let socket_path = absolutize(socket_path);
    // SAFETY: sockaddr_un contains only integer fields and a character array;
    // all-zero is valid. No syscall uses this value; only the field size is read.
    let address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    let limit = address.sun_path.len();
    let path_bytes = socket_path.as_os_str().as_bytes().len();
    let required_bytes = path_bytes.saturating_add(1);
    if required_bytes > limit {
        return Err(RuntimeError::InvalidInput(format!(
            "events socket path {socket_path:?} uses {path_bytes} path bytes \
             ({required_bytes} including NUL), exceeding the platform sun_path limit of {limit} bytes"
        )));
    }
    Ok(())
}

/// Validate the configured forwarding socket of a file-backed runtime before
/// its backend is opened. An in-memory runtime never forwards, so its socket
/// is not checked.
pub(crate) fn validate_configured_events_socket(config: &RuntimeConfig) -> RuntimeResult<()> {
    if config.db_path.is_none() {
        return Ok(());
    }
    match config
        .events_split
        .as_ref()
        .and_then(|split| split.socket_path.as_deref())
    {
        Some(socket) => validate_events_socket_path(socket),
        None => Ok(()),
    }
}
