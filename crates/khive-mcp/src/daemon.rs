//! khived daemon client — forwarding + auto-spawn.
//!
//! The daemon server lives in `khive-runtime::daemon`. This module provides the
//! client side: [`forward_or_spawn`] connects to the daemon, auto-spawns it on
//! first use, and maps responses to MCP error types. Ordinary fallback paths
//! return `None` so the caller can dispatch locally. `KHIVE_DAEMON_STRICT=1`
//! (#947) is the exception: it turns a recordable fallback into a
//! caller-visible per-op error instead, via `fallback_or_reject`.
//! `KHIVE_NO_DAEMON` and `crate::server`'s `save_to` bypass remain
//! intentional, unconditional local paths — neither is affected by strict
//! mode, since nothing is ever recorded or falls back for them.
//!
//! Also provides the [`khive_runtime::daemon::DaemonDispatch`] impl for [`crate::server::KhiveMcpServer`].

use async_trait::async_trait;
use khive_runtime::daemon::{
    self, acquire_recovery_lock, env_truthy, pid_path, DaemonRequestFrame,
};
#[cfg(test)]
use khive_runtime::daemon::{
    read_frame, socket_path, write_frame, DaemonResponseFrame, MAX_FRAME_BYTES, PROTOCOL_VERSION,
};
#[cfg(test)]
use khive_runtime::process_retry::{spawn_retrying_executable_busy, EXECUTABLE_BUSY_BACKOFF_MS};
use rmcp::ErrorData as McpError;
use sha2::{Digest, Sha256};
#[cfg(test)]
use tokio::net::UnixStream;

use crate::tools::request::RequestParams;

mod dispatch;
pub(crate) mod executable;
mod fallback;
mod forward;
mod forward_or_spawn;
mod launch;
mod liveness;
mod recovery;
mod self_heal;

use recovery::*;

pub(crate) use fallback::{bridge_diagnostics_snapshot, FallbackReason, STRICT_FALLBACK_MARKER};
use fallback::{
    fallback_or_reject, is_daemon_strict_mode, opaque_config_id, record_daemon_slow,
    with_rendezvous_admission_snapshot, DaemonSlowPhase,
};

#[cfg(test)]
pub(crate) use fallback::{
    fallback_count, reset_fallback_counters, test_recordable_fallback_rejected,
};
#[cfg(test)]
use fallback::{
    fallback_strict_violations, fallback_total, first_config_mismatch_field, record_fallback,
    FallbackSeverity, FALLBACK_NO_SOCKET, FALLBACK_PARSE_FAILURE,
};

use forward::{
    acquire_supervisor_marker_lock, bounded_retry_deadline, daemon_mcp_error, map_response,
    protocol_mismatch_error, protocol_mismatch_message, read_supervisor_marker,
    recorded_daemon_is_alive, request_too_large_error, sleep_until_retry,
    try_forward_with_read_replay, wait_for_supervisor, ForwardOutcome, ReadReplayBudget,
    SupervisorMarker,
};

#[cfg(test)]
use forward::{
    classify_socket_connect_error, pid_file_directory_is_trusted_if_present,
    socket_exchange_deadline, supervisor_marker_lock_path, try_forward_inner,
    DEFAULT_SUPERVISOR_RESTART_INTERVAL,
};

pub(crate) use forward_or_spawn::forward_or_spawn_with_replay_policy;
#[cfg(any(test, feature = "test-forward-seam"))]
pub use forward_or_spawn::test_forward_seam;
#[cfg(test)]
#[allow(unused_imports)]
use forward_or_spawn::SupervisorDiscoveryHook;
use forward_or_spawn::BOOT_FENCE_PROBE_TIMEOUT_MS;
pub use forward_or_spawn::{
    forward_or_spawn, forward_or_spawn_with_config, forward_or_spawn_with_config_and_packs,
};
#[cfg(test)]
use forward_or_spawn::{
    forward_or_spawn_with, forward_or_spawn_with_exe, forward_or_spawn_with_policy,
    supervisor_discovery_hook, SupervisorDiscoveryPoint, SUPERVISOR_DISCOVERY_HOOK,
};

pub use launch::select_daemon_spawn_overrides;
use launch::{spawn_daemon, spawn_daemon_with_exe_and_config};

#[cfg(test)]
use launch::{
    daemon_launch_command, daemon_log_path, daemon_log_path_from_home, daemon_log_should_rotate,
    prepare_daemon_log_file_with_cap, spawn_daemon_with_exe, DAEMON_LOG_MAX_BYTES,
};

#[cfg(test)]
use liveness::{argv_is_khive_daemon, PidFileSnapshot};
use liveness::{
    kill_stale_daemon_inner, probe_daemon_identity, process_is_alive,
    remove_daemon_paths_if_still_stale, ProbeOutcome, RecoveryError, INCUMBENT_EXIT_TIMEOUT_SECS,
};

#[cfg(unix)]
pub use liveness::{
    probe_supervisor_socket, refuse_serving_socket_before_store_claim, request_supervisor_handover,
    supervisor_effective_uid, supervisor_pid_is_alive, SupervisorDaemonPeer, SupervisorSocketProbe,
};

use self_heal::{arm_executable_self_heal, trigger_bridge_self_heal};
pub(crate) use self_heal::{resumed_generation, SelfHealOnFlushTransport};
// Preserve the original crate-visible paths, including in non-test builds.
#[allow(unused_imports)]
pub(crate) use self_heal::{
    fire_pending_self_heal, schedule_drain_and_exit, schedule_reexec_on_mismatch,
};

#[cfg(all(test, unix))]
pub(crate) use self_heal::REEXEC_INVOKED_COUNT;
#[cfg(test)]
use self_heal::{
    clear_pending_self_heal, decide_mismatch_recovery, resumed_generation_from_args,
    MismatchRecovery, PENDING_SELF_HEAL,
};
#[cfg(test)]
pub(crate) use self_heal::{reset_self_heal_counters, DRAIN_EXIT_INVOKED_COUNT};

/// Snapshot the stdio bridge image before config discovery or database boot can
/// wait across an installation. Daemon and one-shot exec entrypoints omit this.
pub fn capture_bridge_executable() {
    let _ = crate::server::bridge_instance_id();
    executable::capture_at_startup();
}

#[cfg(test)]
mod bridge_diagnostics_tests;

#[cfg(test)]
mod memory_namespace_tests;

#[cfg(test)]
mod test_harness;

#[cfg(test)]
use test_harness::{
    reset_counters, DAEMON_DISPATCH, FORCED_CONNECT_ERROR, FORCE_PID_IS_DAEMON,
    FORCE_PID_IS_FOREIGN, KILL_COUNT, RECOVERY_RACE_BARRIER, SIGTERM_COUNT, SPAWN_COUNT,
};

// ── tests ─────────────────────────────────────────────────────────────────────

/// Let sibling unit tests compose the real frame mapping with a captured dispatch.
#[cfg(all(unix, test))]
pub(crate) fn map_response_for_test(
    response: DaemonResponseFrame,
    expected_config_id: &str,
    namespace: &str,
) -> Option<Result<String, McpError>> {
    map_response(response, expected_config_id, namespace)
}

#[cfg(test)]
mod demand_launch_tests;

#[cfg(all(test, unix))]
mod strict_default_tests;

#[cfg(all(test, unix))]
mod daemon_slow_tests;

#[cfg(all(test, unix))]
mod rendezvous_recovery_tests;

#[cfg(test)]
#[path = "daemon_tests.rs"]
mod tests;
