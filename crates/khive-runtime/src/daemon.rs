//! khived daemon server — persistent warm runtime over a Unix socket.
//!
//! The daemon binds `~/.khive/khived.sock`, accepts length-prefixed request
//! frames, dispatches them through a [`DaemonDispatch`] implementor, and serves
//! results back. It is transport-agnostic: the MCP crate provides the dispatch
//! impl, but any future client (CLI, HTTP gateway) can reuse this server.
//!
//! The client side (forwarding, auto-spawn) lives in the transport crate
//! (e.g. `khive-mcp`), not here.

#[cfg(all(test, unix))]
use std::sync::Arc;

mod locks;
#[cfg(all(test, unix))]
use locks::open_lock_file;
#[cfg(unix)]
pub use locks::{
    acquire_daemon_boot_guard, acquire_recovery_lock, try_acquire_daemon_boot_guard_until,
    try_acquire_recoverer_lock_until, DaemonBootGuard,
};
#[cfg(unix)]
pub(crate) use locks::{peer_uid, uid_is_permitted};
#[cfg(unix)]
use locks::{socket_identity, SocketIdentity};

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

#[cfg(all(test, unix))]
use std::io::Write as _;
#[cfg(all(test, unix))]
use std::os::unix::fs::{MetadataExt, PermissionsExt};
#[cfg(all(test, unix))]
use std::os::unix::io::AsRawFd;
#[cfg(all(test, unix))]
use std::path::PathBuf;

#[cfg(all(test, unix))]
use async_trait::async_trait;
#[cfg(all(test, unix))]
use tokio::io::AsyncReadExt;
#[cfg(all(test, unix))]
use tokio::io::AsyncWriteExt;
#[cfg(all(test, unix))]
use tokio::net::{UnixListener, UnixStream};

#[cfg(all(test, unix))]
use crate::pack::RequestIdentity;
#[cfg(all(test, unix))]
use khive_db::{run_checkpoint_task, CheckpointConfig, ConnectionPool};

mod lifecycle;
#[cfg(unix)]
use lifecycle::khive_dir;
#[cfg(test)]
use lifecycle::khive_root_from;
#[cfg(unix)]
use lifecycle::{
    default_socket_path, ensure_rendezvous_overrides_paired, DaemonLifecycle, PID_PATH_ENV,
    SOCKET_PATH_ENV,
};
pub use lifecycle::{
    lock_path, volume_lock_dir, DaemonLifecyclePhase, DaemonLifecycleSnapshot, DaemonLifetime,
    DaemonOptions, DaemonShutdownReason, DaemonStartupReport, DEFAULT_DEMAND_IDLE_SECS,
};
#[cfg(unix)]
pub use lifecycle::{pid_path, recoverer_lock_path, socket_path, SUPERVISOR_CLAIM_ENV};

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
#[cfg(unix)]
pub use wire_frames::{read_frame, write_frame};
#[cfg(all(test, unix))]
use wire_frames::{read_initial_frame, take_daemon_lexical_timeout_marker};
pub use wire_frames::{
    CheckpointStoreMetrics, DaemonDispatchError, DaemonRequestFrame, DaemonResponseFrame,
    MetricsSnapshot, ERROR_DETAIL_NESTING_DEPTH_LIMIT,
};

// ── dispatch trait ────────────────────────────────────────────────────────────

mod dispatch_trait;
#[cfg(unix)]
pub use dispatch_trait::DaemonDispatch;
pub use dispatch_trait::{
    active_phase_names, background_task_count, background_task_names, daemon_shutdown_token,
    is_warm_index_host, mark_warm_index_host, register_active_phase, spawn_named_tracked_task,
    spawn_tracked_task, track_background_task, track_named_background_task, PhaseGuard,
    UNNAMED_BACKGROUND_TASK,
};
#[cfg(unix)]
use dispatch_trait::{
    build_metrics_snapshot, checkpoint_task_specs, idle_retirement_blockers, wait_for_idle,
};

mod connection;
#[cfg(all(unix, feature = "fault-injection"))]
pub use connection::handle_conn_for_test;
#[cfg(all(unix, any(test, feature = "test-internals")))]
pub use connection::serve_connection_for_test;
#[cfg(all(test, unix))]
use connection::{handle_conn, handle_conn_with_shutdown, write_response_frame};
#[cfg(unix)]
use connection::{handle_conn_with_lifecycle, spawn_connection_task};

mod drain;
pub use drain::drain_timeout;
#[cfg(all(test, unix))]
use drain::drain_with_timeout;
#[cfg(unix)]
use drain::{drain, drain_for_idle, finish_connection_tasks};

mod process_files;
#[cfg(all(test, unix))]
use process_files::{
    classify_kill_result, is_process_running, pid_can_name_incumbent,
    socket_speaks_khived_protocol, PidLiveness, DUPLICATE_PROBE_TIMEOUT,
};
#[cfg(unix)]
use process_files::{
    cleanup_stale_daemon, pid_file_names_a_reachable_daemon, remove_pid_file_if_owned,
    shutdown_cleanup_if_owned, write_pid_file_exclusive, Incumbent,
};

mod run_daemon;
#[cfg(unix)]
pub(crate) use run_daemon::ensure_socket_dir_is_trusted;
#[cfg(all(unix, any(test, feature = "fault-injection")))]
pub use run_daemon::run_daemon_in_process_test;
#[cfg(unix)]
pub use run_daemon::{
    ensure_pid_file_dir_is_trusted, run_daemon, run_daemon_with_boot_guard,
    run_daemon_with_boot_guard_and_start, run_daemon_with_options_and_boot_guard_and_start,
};
#[cfg(all(test, unix))]
use run_daemon::{
    ensure_rendezvous_dir_is_trusted, ensure_socket_path_is_swap_resistant, RendezvousPathRole,
};

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

#[cfg(all(test, unix))]
#[path = "daemon_tests.rs"]
mod tests;
