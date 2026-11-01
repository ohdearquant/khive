//! khived daemon server — persistent warm runtime over a Unix socket.
//!
//! The daemon binds `~/.khive/khived.sock`, accepts length-prefixed request
//! frames, dispatches them through a [`DaemonDispatch`] implementor, and serves
//! results back. It is transport-agnostic: the MCP crate provides the dispatch
//! impl, but any future client (CLI, HTTP gateway) can reuse this server.
//!
//! The client side (forwarding, auto-spawn) lives in the transport crate
//! (e.g. `khive-mcp`), not here.

use std::sync::Arc;

mod boot_guard;
mod connection;
mod dispatch_trait;
mod locks;
mod process_files;
mod run_daemon;

pub use dispatch_trait::*;

#[cfg(all(unix, feature = "fault-injection"))]
pub use connection::handle_conn_for_test;
#[cfg(unix)]
use connection::*;

#[cfg(unix)]
pub use run_daemon::*;

#[cfg(unix)]
pub use boot_guard::*;

#[cfg(all(test, unix))]
use locks::open_lock_file;
#[cfg(unix)]
pub use locks::{
    acquire_daemon_boot_guard, acquire_recovery_lock, open_mutating_daemon_lock,
    try_acquire_daemon_boot_guard_until, try_acquire_recoverer_lock_until, DaemonBootGuard,
};
#[cfg(unix)]
pub(crate) use locks::{peer_uid, uid_is_permitted};
#[cfg(unix)]
use locks::{socket_identity, SocketIdentity};
pub use process_files::*;

mod store_guard;
mod store_identity;
#[cfg(unix)]
use store_guard::ensure_claimed_parent_identity;
#[cfg(unix)]
pub use store_guard::{acquire_daemon_store_guards, bind_daemon_store_files, claim_stores};
pub use store_guard::{assert_daemon_store_identities, DaemonStoreGuard};
#[cfg(unix)]
pub use store_identity::claimed_daemon_store_identity;
mod supervisor_marker;
#[cfg(unix)]
pub use supervisor_marker::supervisor_marker_path;

#[cfg(unix)]
use std::io::Write as _;
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, PermissionsExt};
#[cfg(unix)]
use std::os::unix::io::AsRawFd;
#[cfg(all(test, unix))]
use std::path::PathBuf;

#[cfg(unix)]
use async_trait::async_trait;
#[cfg(unix)]
use libc;
#[cfg(unix)]
use serde::Deserialize;
#[cfg(unix)]
use tokio::io::AsyncReadExt;
#[cfg(all(test, unix))]
use tokio::io::AsyncWriteExt;
#[cfg(unix)]
use tokio::net::{UnixListener, UnixStream};

#[cfg(unix)]
use crate::pack::RequestIdentity;
#[cfg(unix)]
use khive_db::{run_checkpoint_task, CheckpointConfig, CheckpointLifecycleOwner, ConnectionPool};

// Unix-only: gated inside the file, because the error-constructor census
// admits only attribute-free submodule declarations.
mod exit_record;
mod lifecycle;
#[cfg(unix)]
use lifecycle::khive_dir;
#[cfg(test)]
use lifecycle::khive_root_from;
#[cfg(unix)]
use lifecycle::{
    current_supervisor_claim, default_socket_path, ensure_rendezvous_overrides_paired,
    read_supervisor_marker_claim, DaemonLifecycle, PID_PATH_ENV, SOCKET_PATH_ENV,
};
pub use lifecycle::{
    lock_path, volume_lock_dir, DaemonLifecyclePhase, DaemonLifecycleSnapshot, DaemonLifetime,
    DaemonOptions, DaemonShutdownReason, DaemonStartupReport, DEFAULT_DEMAND_IDLE_SECS,
};
#[cfg(unix)]
pub use lifecycle::{
    pid_path, recoverer_lock_path, rendezvous_split, socket_path, RendezvousSplit,
    SUPERVISOR_CLAIM_ENV,
};

mod load_limits;
#[cfg(unix)]
use load_limits::{admit_or_refuse_busy, ConnectionAdmission};
pub use load_limits::{
    recall_ledger_snapshot, track_recall_ledger_task, ConnectionCapSnapshot, RecallLedgerSnapshot,
};

/// Maximum frame size accepted in either direction.
pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

/// Wire protocol version for the daemon IPC framing.
///
/// Increment this constant whenever the request or response frame shape
/// changes in a backward-incompatible way. The client sends its version
/// in every request; the daemon rejects mismatches with an explicit error
/// that names both sides so the operator knows exactly what to do
/// (`make local` rebuilds the client binary).
/// See `docs/api/daemon.md#protocol_version` for the version-by-version history.
pub const PROTOCOL_VERSION: u32 = 8;

/// Internal signal carried in a dispatch result until the daemon moves it to
/// response-frame metadata. It must never be sent in `result`: older v8
/// clients publish that string without inspecting its contents.
#[doc(hidden)]
pub const DAEMON_LEXICAL_TIMEOUT_MARKER: &str = "__khive_daemon_lexical_timeout";

const DEFAULT_DRAIN_TIMEOUT_SECS: u64 = 10;
/// An accepted local socket must finish its first frame within this window.
/// Dispatch deadlines start only after decoding, so they cannot reap peers
/// that connect and then stop sending request bytes.
#[cfg(unix)]
const INITIAL_FRAME_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

#[cfg(unix)]
fn next_accept_error_backoff(previous: Option<std::time::Duration>) -> std::time::Duration {
    previous
        .map(|delay| delay.saturating_mul(2))
        .unwrap_or_else(|| std::time::Duration::from_millis(10))
        .min(std::time::Duration::from_secs(1))
}

// ── wire types ────────────────────────────────────────────────────────────────

mod config_id;
#[cfg(test)]
use config_id::parse_config_id;
pub use config_id::{
    config_id_extra_embedder_exclusions, config_ids_compatible, first_config_mismatch_field,
};

mod wire_frames;
pub use wire_frames::{
    drop_error_detail_iteratively, error_detail_value_within_limit, CheckpointStoreMetrics,
    DaemonDispatchError, DaemonRequestFrame, DaemonResponseFrame, MetricsSnapshot,
    ERROR_DETAIL_NESTING_DEPTH_LIMIT,
};
#[cfg(unix)]
pub use wire_frames::{read_frame, write_frame};
#[cfg(unix)]
use wire_frames::{read_initial_frame, take_daemon_lexical_timeout_marker};

/// Returns `true` for non-empty env values that are not `"0"` or `"false"`.
#[cfg(unix)]
pub fn env_truthy(key: &str) -> bool {
    std::env::var(key)
        .map(|v| {
            let v = v.trim();
            !v.is_empty() && v != "0" && !v.eq_ignore_ascii_case("false")
        })
        .unwrap_or(false)
}

include!("daemon_khive_root_tests.rs");

/// Serve one already-admitted test connection through the production frame handler.
///
/// This seam owns no socket path, PID, boot guard, background components, or
/// process-wide shutdown state. The caller owns and joins the connection task.
/// It deliberately does not exercise listener admission or daemon lifecycle.
#[cfg(all(unix, any(test, feature = "test-internals")))]
#[doc(hidden)]
pub async fn serve_connection_for_test<D: DaemonDispatch>(stream: UnixStream, dispatcher: D) {
    handle_conn_with_shutdown(
        stream,
        dispatcher,
        None,
        tokio::time::Instant::now() + INITIAL_FRAME_READ_TIMEOUT,
    )
    .await;
}

#[cfg(all(test, unix))]
#[path = "daemon_tests.rs"]
mod tests;
