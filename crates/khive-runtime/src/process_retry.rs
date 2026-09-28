//! Bounded retry for a process image temporarily busy during replacement.

use std::io::{ErrorKind, Result};
use std::time::Duration;

/// Three short delays permit four total spawn attempts.
pub const EXECUTABLE_BUSY_BACKOFF_MS: [u64; 3] = [5, 20, 50];

fn is_executable_busy(error: &std::io::Error) -> bool {
    error.kind() == ErrorKind::ExecutableFileBusy
}

/// Retry only before a child exists; a successful command is never repeated.
/// Every other error, including the final busy error, is returned unchanged.
pub async fn spawn_retrying_executable_busy_async<T>(
    delays_ms: &[u64],
    mut spawn: impl FnMut() -> Result<T>,
) -> Result<T> {
    for delay_ms in delays_ms {
        match spawn() {
            Err(error) if is_executable_busy(&error) => {
                tokio::time::sleep(Duration::from_millis(*delay_ms)).await;
            }
            outcome => return outcome,
        }
    }
    spawn()
}

/// Blocking counterpart for synchronous subprocess call sites.
pub fn spawn_retrying_executable_busy<T>(
    delays_ms: &[u64],
    mut spawn: impl FnMut() -> Result<T>,
) -> Result<T> {
    for delay_ms in delays_ms {
        match spawn() {
            Err(error) if is_executable_busy(&error) => {
                std::thread::sleep(Duration::from_millis(*delay_ms));
            }
            outcome => return outcome,
        }
    }
    spawn()
}
