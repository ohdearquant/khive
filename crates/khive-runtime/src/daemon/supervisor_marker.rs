//! Where the supervisor launcher's marker file lives.
#![cfg(unix)]
use std::path::PathBuf;

use super::{default_socket_path, khive_dir, socket_path};

/// Marker file the supervisor's launcher publishes before it execs `khived`.
/// It records the job label, launcher/daemon PID, restart interval in seconds,
/// and the launcher's incarnation claim. The launcher or deliberate-stop procedure
/// removes its own claim; the daemon never writes or removes it. A client
/// waits up to three restart intervals before a logged degraded bootstrap,
/// bounded by its caller deadline, rather than racing normal supervisor
/// startup. Reading and acting on this file is the client's decision
/// (`khive-mcp`); this module
/// only resolves where it lives. The default socket keeps `khived.supervisor`;
/// a private socket appends `.supervisor-marker` to its complete pathname.
///
/// Overridable via the `KHIVE_SUPERVISOR_MARKER` env var (for tests).
pub fn supervisor_marker_path() -> PathBuf {
    if let Ok(p) = std::env::var("KHIVE_SUPERVISOR_MARKER") {
        if !p.is_empty() {
            return PathBuf::from(p);
        }
    }
    let socket = socket_path();
    if socket.as_os_str() == default_socket_path().as_os_str() {
        khive_dir().join("khived.supervisor")
    } else {
        let mut marker = socket.into_os_string();
        marker.push(".supervisor-marker");
        PathBuf::from(marker)
    }
}
