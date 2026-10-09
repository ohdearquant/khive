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
    self, acquire_recovery_lock, env_truthy, pid_path, DaemonRequestFrame, PROTOCOL_VERSION,
};
#[cfg(test)]
use khive_runtime::daemon::{
    read_frame, socket_path, write_frame, DaemonResponseFrame, MAX_FRAME_BYTES,
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
mod launch;
mod liveness;

pub(crate) use fallback::{bridge_diagnostics_snapshot, FallbackReason, STRICT_FALLBACK_MARKER};
use fallback::{fallback_or_reject, is_daemon_strict_mode, opaque_config_id};

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

// ── client ────────────────────────────────────────────────────────────────────

/// Launch seam for daemon recovery.
///
/// Production closures return a real [`std::process::Child`]. The test harness
/// supplies an in-process handle around the real [`daemon::run_daemon`] server,
/// allowing parallel recovery to assert server convergence without forking the
/// test binary with synthetic CLI arguments (#539/#544).
trait DaemonLauncher: Sync {
    type Handle: std::fmt::Debug;

    fn launch(&self) -> std::io::Result<Self::Handle>;
}

struct ProcessDaemonLauncher<'a, F>(&'a F);

impl<F> DaemonLauncher for ProcessDaemonLauncher<'_, F>
where
    F: Fn() -> std::io::Result<std::process::Child> + Sync,
{
    type Handle = std::process::Child;

    fn launch(&self) -> std::io::Result<Self::Handle> {
        (self.0)()
    }
}

/// Outcome returned by [`kill_and_respawn`] to the call site.
#[derive(Debug)]
enum RecoveryOutcome<H = std::process::Child> {
    /// A concurrent client already replaced the daemon; forward the real request
    /// via the normal path (no new spawn occurred).
    Skipped,
    /// This client killed the stale daemon and spawned a replacement; caller
    /// must wait for readiness then forward the real request. The production
    /// launcher returns a [`std::process::Child`] (#898), so
    /// `forward_or_spawn` can, once it is otherwise about to give up and fall
    /// back locally, positively confirm whether that specific respawn attempt
    /// already exited instead of ever binding the socket. The test launcher
    /// carries an in-process task handle with the same ownership role.
    Spawned(H),
    /// Could not obtain a positive confirmation either way within the
    /// deadline-bound recovery window (the recoverer lock or the boot/recovery
    /// lock stayed contended past its deadline) — #838. The
    /// caller's behavior is identical to `Skipped` (never kill on an
    /// unconfirmed state), but this is reported as a distinct variant rather
    /// than silently folded into `Skipped`, so logs/metrics do not conflate
    /// "positively confirmed alive" with "gave up without confirming".
    Uncertain,
}

/// Bounded number of quiescence-confirm rounds [`confirm_genuinely_dead`]
/// performs before trusting a `Dead` classification enough to kill+spawn.
const DEAD_CONFIRM_ROUNDS: u32 = 4;

/// Pacing between [`confirm_genuinely_dead`] rounds. Not a synchronization
/// mechanism by itself — the real synchronization is the bounded `flock` in
/// [`quiesce_then_probe_identity`]; this only avoids busy-spinning while
/// waiting for a peer that has not yet reached its own boot-guard call.
const DEAD_CONFIRM_POLL_MS: u64 = 75;

/// Deadline for each round's bounded wait on the boot/recovery lock inside
/// [`quiesce_then_probe_identity`]. #838: the previous
/// unbounded blocking `flock` meant `DEAD_CONFIRM_ROUNDS` bounded probe
/// *count*, not elapsed *time* — a wedged lock holder blocked recovery
/// forever. Bounding each round's lock wait makes the whole
/// `confirm_genuinely_dead` call bounded by
/// `DEAD_CONFIRM_ROUNDS * (BOOT_QUIESCENCE_LOCK_TIMEOUT_MS +
/// BOOT_FENCE_PROBE_TIMEOUT_MS + DEAD_CONFIRM_POLL_MS)` in the worst case.
const BOOT_QUIESCENCE_LOCK_TIMEOUT_MS: u64 = 500;

/// Block until no concurrent boot holds the shared boot/recovery lock (or the
/// bounded wait's deadline elapses), then re-probe daemon identity — reused
/// by [`confirm_genuinely_dead`] (#758). Deadline-bounded (unlike an
/// unbounded `flock`), so a wedged lock holder cannot block confirmation
/// rounds forever; a contended/failed acquisition returns the distinct
/// [`ProbeOutcome::LockContended`], not `Timeout` (#838). See
/// `crates/khive-mcp/docs/api/daemon-lifecycle.md`.
async fn quiesce_then_probe_identity(
    config_id: &str,
    namespace: &str,
    timeout_ms: u64,
) -> ProbeOutcome {
    let deadline = std::time::Instant::now()
        + std::time::Duration::from_millis(BOOT_QUIESCENCE_LOCK_TIMEOUT_MS);
    match tokio::task::spawn_blocking(move || {
        khive_runtime::daemon::try_acquire_daemon_boot_guard_until(deadline)
    })
    .await
    {
        Ok(Ok(Some(guard))) => drop(guard),
        Ok(Ok(None)) => {
            tracing::debug!(
                BOOT_QUIESCENCE_LOCK_TIMEOUT_MS,
                "boot/recovery lock still contended past its bounded wait; \
                 could not confirm quiescence this round"
            );
            return ProbeOutcome::LockContended;
        }
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "failed to probe boot/recovery lock state");
            return ProbeOutcome::LockContended;
        }
        Err(_join_err) => return ProbeOutcome::LockContended,
    }
    probe_daemon_identity(config_id, namespace, timeout_ms).await
}

/// Confirm a `Dead` probe result is not racing a peer's in-flight
/// `kill_and_respawn` or the daemon's own cold boot (#758) — closes the
/// fork-to-flock gap where `spawn_daemon()`'s child exists but has not yet
/// reached its own `acquire_daemon_boot_guard()` call. Retries
/// [`quiesce_then_probe_identity`] up to [`DEAD_CONFIRM_ROUNDS`] times,
/// paced by [`DEAD_CONFIRM_POLL_MS`]; returns as soon as a peer's boot is
/// observed completing (`Alive`) or going slow (`Timeout`, NEVER-KILL-SLOW).
/// Only `Dead` once every round agrees. See
/// `crates/khive-mcp/docs/api/daemon-lifecycle.md`.
async fn confirm_genuinely_dead(config_id: &str, namespace: &str) -> ProbeOutcome {
    confirm_genuinely_dead_with_round_observer(config_id, namespace, |_, _| {}).await
}

async fn confirm_genuinely_dead_with_round_observer(
    config_id: &str,
    namespace: &str,
    mut observe_round: impl FnMut(u32, &ProbeOutcome),
) -> ProbeOutcome {
    // `LockContended` means "still could not confirm this round" — the same
    // "keep polling" shape as `Dead`, not a terminal state like `Alive`/
    // `Timeout`. A peer's boot can legitimately hold the lock across several
    // rounds; only give up (return `LockContended`) once every round agreed
    // nobody could confirm, exactly mirroring how `Dead` only becomes trusted
    // once every round agreed the daemon was absent.
    //
    // #838: `LockContended` is STICKY across rounds — once
    // any round can't confirm quiescence, the aggregate must never collapse
    // back to `Dead` just because a LATER round happened to observe it. The
    // old code tracked only the last round's outcome, so a
    // LockContended-then-Dead sequence overwrote the earlier contention and
    // returned `Dead`, permitting kill+spawn on a call that never actually
    // established quiescence across every round. `Dead` is only trustworthy
    // when EVERY round agrees; a single contended round makes the whole call
    // `LockContended` regardless of what any other round returned.
    let mut saw_contention = false;
    for round in 0..DEAD_CONFIRM_ROUNDS {
        let outcome =
            quiesce_then_probe_identity(config_id, namespace, BOOT_FENCE_PROBE_TIMEOUT_MS).await;
        observe_round(round, &outcome);
        match outcome {
            ProbeOutcome::Dead => {}
            ProbeOutcome::LockContended => saw_contention = true,
            other => return other,
        }
        if round + 1 < DEAD_CONFIRM_ROUNDS {
            tokio::time::sleep(std::time::Duration::from_millis(DEAD_CONFIRM_POLL_MS)).await;
        }
    }
    if saw_contention {
        ProbeOutcome::LockContended
    } else {
        ProbeOutcome::Dead
    }
}

/// Deadline for acquiring the recoverer-only lock before starting the
/// dead-confirmation → kill → confirmed-exit → spawn critical section.
/// This exceeds the incumbent-exit deadline so a peer can finish a full
/// recovery turn before another recoverer treats the lock as wedged. See
/// `crates/khive-mcp/docs/api/daemon-lifecycle.md`.
const RECOVERER_LOCK_TIMEOUT_MS: u64 = 16_000;

/// Kill the stale daemon and spawn a fresh one, serialized against concurrent
/// recoverers by a dedicated recoverer-only lock (#838 double-checked
/// recovery). Outcomes: `Alive`/`Timeout` → `Skipped`, no kill
/// (NEVER-KILL-SLOW). `LockContended` (confirm rounds inconclusive, or the
/// recoverer lock itself timed out) → `Uncertain`, no kill — same safe
/// behavior as `Skipped` but reported distinctly. `Dead` (confirmed,
/// recoverer lock held) → signal + bounded exit confirmation + spawn →
/// `Spawned`; a PID still alive at the deadline returns
/// [`RecoveryError::IncumbentStillAlive`] without spawning. See
/// `crates/khive-mcp/docs/api/daemon-lifecycle.md` for why a second lock
/// file is required and how this avoids deadlocking a booting daemon.
async fn kill_and_respawn<F>(
    config_id: &str,
    namespace: &str,
    spawn: &F,
) -> Result<RecoveryOutcome, RecoveryError>
where
    F: Fn() -> std::io::Result<std::process::Child> + Sync,
{
    let launcher = ProcessDaemonLauncher(spawn);
    kill_and_respawn_with_launcher(config_id, namespace, &launcher).await
}

/// Test-only shim over [`kill_and_respawn_with_launcher_and_exit_timeout`]:
/// same production code path, with only the launcher construction and the
/// exit-timeout parameter (production passes its own constant through
/// [`kill_and_respawn`]) supplied by the test. Keep it a pure forwarder —
/// any logic added here would drift from the non-test path.
#[cfg(test)]
async fn kill_and_respawn_with_exit_timeout<F>(
    config_id: &str,
    namespace: &str,
    spawn: &F,
    exit_timeout: std::time::Duration,
) -> Result<RecoveryOutcome, RecoveryError>
where
    F: Fn() -> std::io::Result<std::process::Child> + Sync,
{
    let launcher = ProcessDaemonLauncher(spawn);
    kill_and_respawn_with_launcher_and_exit_timeout(config_id, namespace, &launcher, exit_timeout)
        .await
}

async fn kill_and_respawn_with_launcher<L>(
    config_id: &str,
    namespace: &str,
    launcher: &L,
) -> Result<RecoveryOutcome<L::Handle>, RecoveryError>
where
    L: DaemonLauncher,
{
    kill_and_respawn_with_launcher_and_exit_timeout(
        config_id,
        namespace,
        launcher,
        std::time::Duration::from_secs(INCUMBENT_EXIT_TIMEOUT_SECS),
    )
    .await
}

async fn kill_and_respawn_with_launcher_and_exit_timeout<L>(
    config_id: &str,
    namespace: &str,
    launcher: &L,
    exit_timeout: std::time::Duration,
) -> Result<RecoveryOutcome<L::Handle>, RecoveryError>
where
    L: DaemonLauncher,
{
    let initial_probe = {
        let _lock = acquire_recovery_lock();
        probe_daemon_identity(config_id, namespace, 500).await
    };
    match initial_probe {
        ProbeOutcome::Alive | ProbeOutcome::Timeout | ProbeOutcome::LockContended => {
            return Ok(RecoveryOutcome::Skipped);
        }
        ProbeOutcome::Dead => {}
    }

    // Test-only rendezvous (see `RECOVERY_RACE_BARRIER`): forces every
    // concurrent recoverer under test to reach "independently classified
    // Dead" at the same instant, so the recoverer lock below is what
    // actually determines mutual exclusion rather than scheduling order.
    #[cfg(test)]
    {
        let barrier = RECOVERY_RACE_BARRIER
            .lock()
            .expect("barrier mutex poisoned")
            .clone();
        if let Some(barrier) = barrier {
            barrier.wait().await;
        }
    }

    let recoverer_deadline =
        std::time::Instant::now() + std::time::Duration::from_millis(RECOVERER_LOCK_TIMEOUT_MS);
    let recoverer_guard = match tokio::time::timeout(
        std::time::Duration::from_millis(RECOVERER_LOCK_TIMEOUT_MS),
        tokio::task::spawn_blocking(move || {
            khive_runtime::daemon::try_acquire_recoverer_lock_until(recoverer_deadline)
        }),
    )
    .await
    {
        Ok(Ok(Ok(Some(guard)))) => guard,
        Ok(Ok(Ok(None))) => {
            tracing::warn!(
                RECOVERER_LOCK_TIMEOUT_MS,
                "recoverer lock still contended past its deadline; a peer recoverer \
                 is likely still mid dead-confirmation/kill/spawn — skipping without \
                 a positive confirmation rather than risking a double-spawn"
            );
            return Ok(RecoveryOutcome::Uncertain);
        }
        Ok(Ok(Err(e))) => {
            tracing::warn!(error = %e, "failed to acquire recoverer lock");
            return Ok(RecoveryOutcome::Uncertain);
        }
        Ok(Err(_)) | Err(_) => {
            tracing::warn!("recoverer lock acquisition task failed or exceeded its deadline");
            return Ok(RecoveryOutcome::Uncertain);
        }
    };

    // The bridge may have stopped waiting while this task waited for a peer
    // recoverer. Its detached task must not now classify or kill a daemon.
    if khive_storage::request_read_is_cancelled() {
        return Err(RecoveryError::RequestExpired);
    }

    let outcome = match confirm_genuinely_dead(config_id, namespace).await {
        ProbeOutcome::Alive | ProbeOutcome::Timeout => Ok(RecoveryOutcome::Skipped),
        ProbeOutcome::LockContended => {
            tracing::warn!(
                "confirm_genuinely_dead could not establish quiescence within its \
                 bounded rounds; skipping kill+spawn without a positive confirmation"
            );
            Ok(RecoveryOutcome::Uncertain)
        }
        ProbeOutcome::Dead => {
            let boot_lock = acquire_recovery_lock();
            match probe_daemon_identity(config_id, namespace, BOOT_FENCE_PROBE_TIMEOUT_MS).await {
                ProbeOutcome::Alive | ProbeOutcome::Timeout => return Ok(RecoveryOutcome::Skipped),
                ProbeOutcome::LockContended => return Ok(RecoveryOutcome::Uncertain),
                ProbeOutcome::Dead => {}
            }
            if khive_storage::request_read_is_cancelled() {
                return Err(RecoveryError::RequestExpired);
            }
            let expected_snapshot = kill_stale_daemon_inner(exit_timeout, boot_lock).await?;
            // Keep the recoverer-only lock throughout, but let graceful exit
            // take the boot lock. An independently started successor can win
            // while we wait, so recheck under the boot lock before unlink/spawn.
            let _boot_lock = acquire_recovery_lock();
            match probe_daemon_identity(config_id, namespace, BOOT_FENCE_PROBE_TIMEOUT_MS).await {
                ProbeOutcome::Alive | ProbeOutcome::Timeout => Ok(RecoveryOutcome::Skipped),
                ProbeOutcome::LockContended => Ok(RecoveryOutcome::Uncertain),
                ProbeOutcome::Dead => {
                    if !remove_daemon_paths_if_still_stale(&pid_path(), &expected_snapshot) {
                        return Ok(RecoveryOutcome::Uncertain);
                    }
                    launcher
                        .launch()
                        .map(RecoveryOutcome::Spawned)
                        .map_err(RecoveryError::Spawn)
                }
            }
        }
    };
    drop(recoverer_guard);
    outcome
}

/// Build the hard error returned when the real request frame was fully
/// written to the daemon socket but no trustworthy response came back.
///
/// #644: once `write_frame` has completed for a real (non-probe) frame, the
/// daemon may already be dispatching or have finished dispatching it. There is
/// no way from the client side to tell "never received" apart from "received,
/// executed, response lost" — so this case must never be retried (against the
/// same daemon or a freshly-spawned one) and must never silently fall back to
/// local dispatch, either of which could execute a mutation a second time.
fn ambiguous_forward_error() -> McpError {
    daemon_mcp_error(
        "daemon response lost after request was sent; not retrying or locally \
         dispatching to avoid duplicate execution",
        None,
    )
}

fn daemon_unreachable_error(
    frame: &DaemonRequestFrame,
    kind: std::io::ErrorKind,
    os_error_code: Option<i32>,
) -> McpError {
    let config_id = opaque_config_id(&frame.config_id);
    tracing::error!(
        reason = "daemon_unreachable",
        config_id = %config_id,
        namespace = %frame.namespace,
        ?kind,
        ?os_error_code,
        "daemon socket is unreachable from this process; lifecycle recovery suppressed"
    );
    daemon_mcp_error(
        "cannot reach daemon socket from this process; refusing daemon lifecycle recovery \
         because the daemon may still be healthy",
        Some(serde_json::json!({
            "reason": "daemon_unreachable",
            "os_error_kind": format!("{kind:?}"),
            "os_error_code": os_error_code,
        })),
    )
}

// ── #898: loud, unambiguous respawn-failure error ───────────────────────────
//
// Root cause (2026-07-12 incident): `spawn_daemon` already resolves the spawn
// target deterministically via `std::env::current_exe()` (never ambient
// `PATH`), so a version-skewed binary reaching `mcp --daemon` means THIS
// process's own on-disk binary predates (or otherwise rejects) that flag —
// respawning via `current_exe()` faithfully relaunches the very same stale
// binary. That relaunch fails immediately with a clap parse error
// (`error: Unrecognized option: 'daemon'`) written only to `khived.log`.
// Because `spawn_daemon` was fire-and-forget (the `Child` was discarded) and
// `forward_or_spawn` cannot distinguish "our own respawn attempt definitely
// already died" from "no daemon is configured to run here at all", every
// request repeated the same failing respawn, burned the full connect
// deadline plus the boot-quiescence wait, and then quietly completed via
// local dispatch (or, in `KHIVE_DAEMON_STRICT=1`, rejected with the generic
// `no_socket` reason) — a silent, forever-repeating failure invisible to the
// caller and to every metric except a `khived.log` grep.
//
// The fix: `spawn_daemon` now returns the live `Child` (see
// `RecoveryOutcome::Spawned`), and `forward_or_spawn` checks — only at the
// point it would otherwise fall back locally, never earlier, so the existing
// connect-retry window and #667's boot-quiescence fence are unchanged —
// whether the respawn attempt IT made has already exited. A confirmed exit is
// unambiguous (this process spawned that exact child); it is never treated as
// the legitimate ADR-049 no-daemon case and never silently swallowed, in
// either strict or non-strict mode.

#[derive(Clone, Copy)]
enum RespawnFailure {
    SpawnError { os_error_code: Option<i32> },
    ExitedBeforeBind { exit_code: Option<i32> },
}

/// Build the caller-visible error for a respawn attempt this process made and
/// can now positively confirm failed. Both the caller error and bridge tracing
/// expose only stable classifications and non-sensitive numeric status codes.
/// The error is returned regardless of
/// `KHIVE_DAEMON_STRICT`: unlike the ordinary "no daemon reachable" fallback
/// (which may be the legitimate ADR-049 no-daemon deployment), a respawn WE
/// attempted and can prove failed is never a case for quietly completing the
/// request via local dispatch. See #898.
fn respawn_failed_error(failure: RespawnFailure) -> McpError {
    match failure {
        RespawnFailure::SpawnError { os_error_code } => tracing::error!(
            reason = "respawn_failed",
            failure_category = "spawn_error",
            ?os_error_code,
            "daemon respawn attempt confirmed failed"
        ),
        RespawnFailure::ExitedBeforeBind { exit_code } => tracing::error!(
            reason = "respawn_failed",
            failure_category = "exited_before_bind",
            ?exit_code,
            "daemon respawn attempt confirmed failed"
        ),
    }
    // Under strict mode this is also a pre-dispatch rejection, so preserve
    // #947's request-envelope contract by tagging it for `server::request`.
    // Non-strict callers still receive the raw, loud MCP error introduced by
    // #898; in neither mode may the request fall through to local dispatch.
    let data = if is_daemon_strict_mode() {
        serde_json::json!({
            STRICT_FALLBACK_MARKER: true,
            "reason": "respawn_failed",
        })
    } else {
        serde_json::json!({"reason": "respawn_failed"})
    };
    daemon_mcp_error(
        "daemon respawn failed (respawn_failed); rebuild with `make local` and retry",
        Some(data),
    )
}

fn incumbent_still_alive_error(pid: u32) -> McpError {
    tracing::error!(
        reason = "incumbent_still_alive",
        pid,
        "daemon recovery refused because the incumbent did not exit before the deadline"
    );
    let mut data = serde_json::json!({
        "reason": "incumbent_still_alive",
        "pid": pid,
    });
    if is_daemon_strict_mode() {
        data[STRICT_FALLBACK_MARKER] = serde_json::Value::Bool(true);
    }
    daemon_mcp_error(
        format!("daemon recovery refused: incumbent PID {pid} is still alive after the deadline"),
        Some(data),
    )
}

fn untrusted_pid_file_directory_error(message: String) -> McpError {
    tracing::error!(
        reason = "untrusted_pid_file_directory",
        error = %message,
        "daemon recovery refused because its PID-file directory is not trusted"
    );
    let mut data = serde_json::json!({
        "reason": "untrusted_pid_file_directory",
        "error": message,
    });
    if is_daemon_strict_mode() {
        data[STRICT_FALLBACK_MARKER] = serde_json::Value::Bool(true);
    }
    daemon_mcp_error(format!("daemon recovery refused: {message}"), Some(data))
}

/// A socket-less rendezvous claimed by a supervisor returns a retryable error
/// when the caller's startup budget ends. It never falls through to local
/// dispatch or spawns a competing daemon while the claim is held.
fn supervised_daemon_error(marker: &SupervisorMarker) -> McpError {
    let mut data = serde_json::json!({
        "reason": "supervised_daemon_starting",
        "retryable": true,
        "job": marker.job,
        "pid": marker.pid,
        "pid_alive": marker.pid_is_alive(),
    });
    if is_daemon_strict_mode() {
        data[STRICT_FALLBACK_MARKER] = serde_json::Value::Bool(true);
    }
    daemon_mcp_error(
        format!(
            "supervised daemon starting: job \"{}\" (pid {}) has not bound the socket; \
             the request deadline expired or the request was cancelled before dispatch; retry",
            marker.job, marker.pid
        ),
        Some(data),
    )
}

/// No request bytes were sent while another launcher or client held the
/// marker lock. Retrying cannot duplicate dispatch.
fn supervisor_marker_lock_wait_error() -> McpError {
    let mut data = serde_json::json!({
        "reason": "supervised_daemon_starting",
        "retryable": true,
    });
    if is_daemon_strict_mode() {
        data[STRICT_FALLBACK_MARKER] = serde_json::Value::Bool(true);
    }
    daemon_mcp_error(
        "daemon startup ownership is in progress; the marker lock wait ended before dispatch; retry",
        Some(data),
    )
}

fn daemon_reconnect_expired_before_dispatch_error() -> McpError {
    daemon_mcp_error(
        "daemon reconnect deadline expired or request cancelled before dispatch; no lifecycle recovery attempted",
        Some(serde_json::json!({"reason": "daemon_reconnect_expired"})),
    )
}

// ── bridge self-heal: re-exec in place on ProtocolMismatch (#714) ───────────
//
// A long-lived stdio bridge process keeps running the OLD on-disk binary
// after `make local` rebuilds it: the daemon it spawns/forwards to is fresh,
// but this bridge process itself never picks up the new binary until its MCP
// client reconnects. `ProtocolMismatch` is exactly that scenario — the daemon
// is fine, this bridge is stale — so instead of leaving the connection dead
// forever, the bridge re-execs the freshest on-disk binary in place,
// preserving the PID and the open stdio file descriptors so the client's
// transport never sees EOF or a reset. See issue #714 for the evidence this
// design is based on (a live re-exec-mid-session test against the reference
// MCP Python SDK client): a first attempt that called `execv()` synchronously
// inside the tool handler, before the SDK's send loop had serialized and
// flushed the response, discarded the in-flight response and the client's
// call timed out with nothing ever written back.
//
// The fix is a true happens-after edge, not a fixed delay: [`arm_pending_self_heal`]
// records the chosen action *before* the mismatch response is even
// constructed (`trigger_bridge_self_heal` runs synchronously inside the
// request handler), and [`SelfHealOnFlushTransport`] — wrapped around the
// stdio transport in `server.rs::serve_stdio` — fires that action from
// [`fire_pending_self_heal`] only once a message has actually finished
// flushing to the client. Because arming always happens before the mismatch
// response is handed to the transport, and firing only ever happens after a
// flush completes, the very next successful flush is guaranteed to be at or
// after that response reached the client — never before, and never on a
// clock that can expire while a slow or backpressured stdout is still
// mid-write (a fixed-duration sleep could not make that guarantee: rmcp's
// send pipeline enqueues the response and returns almost immediately, then
// performs the real write+flush on a separately spawned task with no
// duration bound).

/// Pending self-heal action, armed by [`arm_pending_self_heal`] and taken by
/// [`fire_pending_self_heal`]. `None` on a healthy bridge for its entire
/// lifetime — the overwhelmingly common case.
struct PendingSelfHeal {
    action: MismatchRecovery,
    executable: Option<std::path::PathBuf>,
}

static PENDING_SELF_HEAL: std::sync::Mutex<Option<PendingSelfHeal>> = std::sync::Mutex::new(None);

fn arm_executable_self_heal(executable: std::path::PathBuf) {
    *PENDING_SELF_HEAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(PendingSelfHeal {
        action: MismatchRecovery::ReexecScheduled,
        executable: Some(executable),
    });
}

fn arm_pending_self_heal(action: MismatchRecovery) {
    let mut slot = PENDING_SELF_HEAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *slot = Some(PendingSelfHeal {
        action,
        executable: None,
    });
}

/// Take and perform whatever self-heal action is armed, if any. Called by
/// [`SelfHealOnFlushTransport::send`] after every message it successfully
/// flushes to the client — the load-bearing happens-after edge documented
/// above. A no-op when nothing is armed.
#[cfg(unix)]
pub(crate) fn fire_pending_self_heal() {
    let action = PENDING_SELF_HEAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    match action {
        Some(PendingSelfHeal {
            action: MismatchRecovery::ReexecScheduled,
            executable,
        }) => reexec_in_place(executable),
        Some(PendingSelfHeal {
            action: MismatchRecovery::DrainAndExit,
            ..
        }) => exit_process(),
        None => {}
    }
}

/// Re-exec self-heal requires `exec()` (POSIX-only) — [`schedule_reexec_on_mismatch`]'s
/// non-unix variant arms [`MismatchRecovery::DrainAndExit`] instead of
/// [`MismatchRecovery::ReexecScheduled`], so only the drain-and-exit arm is
/// ever actually armed on this target.
#[cfg(not(unix))]
pub(crate) fn fire_pending_self_heal() {
    let action = PENDING_SELF_HEAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    if action.is_some() {
        exit_process();
    }
}

/// argv marker carrying the exec-once loop-breaker generation counter.
///
/// Parsed here and defined on [`crate::args::Args`] as a hidden clap field of
/// the same name — the clap field exists purely so the CLI parser accepts the
/// flag on a resumed process instead of rejecting it as unknown; the actual
/// read path is this raw argv scan, independent of wherever in the call stack
/// `Args` was parsed. The counter travels with the exec by construction: it
/// is appended to argv immediately before `exec()`, so any process running
/// with it present is, by definition, a resumed generation.
const RESUMED_GENERATION_ARG_PREFIX: &str = "--resumed-generation=";

/// Whether this process is a resumed generation of a prior self-heal re-exec,
/// and if so, its generation counter. `None` on a normal (cold-started)
/// bridge — the overwhelmingly common case.
pub(crate) fn resumed_generation() -> Option<u32> {
    resumed_generation_from_args(std::env::args())
}

/// Pure argv-scan behind [`resumed_generation`], factored out so the parsing
/// logic is unit-testable without depending on this process's own real argv
/// (which never carries the marker inside `cargo test`).
fn resumed_generation_from_args(args: impl Iterator<Item = String>) -> Option<u32> {
    args.filter_map(|a| {
        a.strip_prefix(RESUMED_GENERATION_ARG_PREFIX)
            .map(str::to_owned)
    })
    .last()
    .and_then(|s| s.parse::<u32>().ok())
}

/// Recovery action chosen for a `ProtocolMismatch` observed inside
/// `forward_or_spawn`. Pure decision, factored out of [`trigger_bridge_self_heal`]
/// so the loop-breaker guard rail (#714 §2.2: exec at most once per mismatch
/// generation) is unit-testable without touching the process or the clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MismatchRecovery {
    /// First generation (no `--resumed-generation` marker) — schedule an
    /// in-place re-exec of the freshest on-disk binary.
    ReexecScheduled,
    /// Already a resumed generation and it hit `ProtocolMismatch` again — the
    /// on-disk binary is itself stale, or a second rebuild race — take the
    /// fallback instead of exec'ing a second time.
    DrainAndExit,
}

fn decide_mismatch_recovery(resumed_generation: Option<u32>) -> MismatchRecovery {
    match resumed_generation {
        None => MismatchRecovery::ReexecScheduled,
        Some(_) => MismatchRecovery::DrainAndExit,
    }
}

/// Trigger the bridge's self-heal recovery for a `ProtocolMismatch` outcome.
/// Called from both `forward_or_spawn` `ProtocolMismatch` arms; both already
/// construct and return the hard mismatch error, this schedules recovery
/// alongside that return, never in place of it. Accepted concurrency risk
/// (#714, not fixed by this change): a genuinely concurrent second in-flight
/// request could observe the wrong flush event. See
/// `crates/khive-mcp/docs/api/daemon-lifecycle.md`.
fn trigger_bridge_self_heal() {
    match decide_mismatch_recovery(resumed_generation()) {
        MismatchRecovery::ReexecScheduled => schedule_reexec_on_mismatch(),
        MismatchRecovery::DrainAndExit => {
            tracing::warn!(
                "resumed generation observed ProtocolMismatch again — loop-breaker \
                 tripped (#714 §2.2, exec-once guard); draining and exiting instead \
                 of re-exec'ing a second time"
            );
            schedule_drain_and_exit();
        }
    }
}

/// Arm an in-place re-exec of the freshest on-disk binary, to fire from
/// [`fire_pending_self_heal`] once the mismatch response has actually
/// flushed (see the module-level doc above for why that happens-after edge
/// — not a fixed delay — is load-bearing). Never execs synchronously and
/// never execs from this function directly. Only ever called for a
/// first-generation process — the loop-breaker guard rail lives in
/// [`trigger_bridge_self_heal`].
#[cfg(unix)]
pub(crate) fn schedule_reexec_on_mismatch() {
    tracing::warn!(
        client_version = PROTOCOL_VERSION,
        "protocol mismatch: arming in-place re-exec of the freshest on-disk binary, \
         to fire once the mismatch response has flushed to the client"
    );
    arm_pending_self_heal(MismatchRecovery::ReexecScheduled);
}

/// Re-exec self-heal requires `exec()` (POSIX-only); on any other target,
/// take the same drain-and-exit fallback a loop-breaker trip would.
#[cfg(not(unix))]
pub(crate) fn schedule_reexec_on_mismatch() {
    schedule_drain_and_exit();
}

/// Perform the actual re-exec: use the recorded path for an executable
/// replacement, or resolve the on-disk binary at *exec time* via
/// [`std::env::current_exe`] (the same primitive `spawn_daemon` already uses
/// for "pick up whatever `make local` just replaced" — see `spawn_daemon`
/// above), preserve the original argv, append the `--resumed-generation=1`
/// marker, and replace the process image via
/// [`std::os::unix::process::CommandExt::exec`] — which, unlike `spawn`,
/// keeps the same PID and the same open stdin/stdout/stderr file descriptors,
/// so the client's stdio transport never sees EOF or a reset.
///
/// `exec` only returns on failure; on failure this logs and returns, leaving
/// the process running under its stale binary (the hard mismatch error was
/// already sent to the client for this request; there is nothing safe to
/// retry from here).
#[cfg(all(unix, not(test)))]
fn reexec_in_place(executable: Option<std::path::PathBuf>) {
    use std::os::unix::process::CommandExt;

    let exe = match executable.map(Ok).unwrap_or_else(std::env::current_exe) {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(error = %e, "bridge self-heal re-exec failed: could not resolve current_exe");
            return;
        }
    };
    // Identity-triggered re-execs can recur across installs; retain one marker
    // so protocol mismatches still apply the existing resumed loop-breaker.
    let args: Vec<String> = std::env::args()
        .skip(1)
        .filter(|a| !a.starts_with(RESUMED_GENERATION_ARG_PREFIX))
        .chain(std::iter::once(format!("{RESUMED_GENERATION_ARG_PREFIX}1")))
        .collect();
    let err = std::process::Command::new(exe).args(&args).exec();
    tracing::error!(
        error = %err,
        "bridge self-heal re-exec failed; continuing under the stale binary"
    );
}

/// Arm this process to stop serving and exit, to fire from
/// [`fire_pending_self_heal`] for the same happens-after reason
/// [`schedule_reexec_on_mismatch`] arms its exec instead of performing it
/// directly. This is the fallback path (issue #714 §4): the MCP connection
/// dies and the client's own process-lifecycle management must restart it —
/// no worse than the pre-#714 hard-error-forever behavior.
pub(crate) fn schedule_drain_and_exit() {
    arm_pending_self_heal(MismatchRecovery::DrainAndExit);
}

#[cfg(not(test))]
fn exit_process() {
    std::process::exit(1);
}

#[cfg(all(test, unix))]
pub(crate) static REEXEC_INVOKED_COUNT: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
#[cfg(test)]
pub(crate) static DRAIN_EXIT_INVOKED_COUNT: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

#[cfg(all(test, unix))]
pub(crate) fn reset_self_heal_counters() {
    REEXEC_INVOKED_COUNT.store(0, std::sync::atomic::Ordering::SeqCst);
    DRAIN_EXIT_INVOKED_COUNT.store(0, std::sync::atomic::Ordering::SeqCst);
    clear_pending_self_heal();
}

#[cfg(all(test, not(unix)))]
pub(crate) fn reset_self_heal_counters() {
    DRAIN_EXIT_INVOKED_COUNT.store(0, std::sync::atomic::Ordering::SeqCst);
    clear_pending_self_heal();
}

/// Clear whatever `PENDING_SELF_HEAL` currently holds (#843). A test that
/// exercises `forward_or_spawn`'s protocol-mismatch path end to end (e.g.
/// `forward_or_spawn_rejects_old_daemon_and_returns_protocol_mismatch_error`)
/// arms it via `trigger_bridge_self_heal` but never takes the action back
/// out — that is `fire_pending_self_heal`'s job, and asserting the forwarding
/// behavior alone has no reason to call it. Left armed, that leftover slot
/// is invisible to `reset_self_heal_counters` resetting only the two
/// invocation counters, so a later test in the same binary (e.g.
/// `fire_pending_self_heal_is_a_no_op_when_nothing_is_armed`) can inherit it
/// under multi-threaded test ordering and observe a spurious fire. Every
/// existing test that intentionally arms the slot does so AFTER calling
/// `reset_self_heal_counters`, never before, so clearing it here changes no
/// test's semantics.
#[cfg(test)]
fn clear_pending_self_heal() {
    *PENDING_SELF_HEAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
}

/// Test double for [`reexec_in_place`]: a real `exec()` would replace the test
/// binary's own process image, killing the entire test run. Counts instead.
/// Gated `unix` like the production version above — it is the only thing that
/// calls it (`schedule_reexec_on_mismatch`'s `not(unix)` arm never reaches
/// `reexec_in_place` at all).
#[cfg(all(test, unix))]
fn reexec_in_place(_executable: Option<std::path::PathBuf>) {
    REEXEC_INVOKED_COUNT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
}

/// Wraps a [`rmcp::transport::Transport`] so every message it successfully
/// flushes to the client fires [`fire_pending_self_heal`] afterward — the
/// actual happens-after edge #714's self-heal design requires, not a
/// fixed-duration sleep. Wraps the transport (not the handler) because the
/// handler itself has no way to await the real write+flush, which `rmcp`
/// performs on a separately spawned task. See
/// `crates/khive-mcp/docs/api/daemon-lifecycle.md`.
pub(crate) struct SelfHealOnFlushTransport<T> {
    inner: T,
}

impl<T> SelfHealOnFlushTransport<T> {
    pub(crate) fn new(inner: T) -> Self {
        Self { inner }
    }
}

impl<T> rmcp::transport::Transport<rmcp::RoleServer> for SelfHealOnFlushTransport<T>
where
    T: rmcp::transport::Transport<rmcp::RoleServer>,
{
    type Error = T::Error;

    fn send(
        &mut self,
        item: rmcp::service::TxJsonRpcMessage<rmcp::RoleServer>,
    ) -> impl std::future::Future<Output = Result<(), Self::Error>> + Send + 'static {
        let send = self.inner.send(item);
        async move {
            let result = send.await;
            if result.is_ok() {
                fire_pending_self_heal();
            }
            result
        }
    }

    fn receive(
        &mut self,
    ) -> impl std::future::Future<Output = Option<rmcp::service::RxJsonRpcMessage<rmcp::RoleServer>>>
           + Send {
        self.inner.receive()
    }

    fn close(&mut self) -> impl std::future::Future<Output = Result<(), Self::Error>> + Send {
        self.inner.close()
    }
}

#[cfg(test)]
fn exit_process() {
    DRAIN_EXIT_INVOKED_COUNT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
}

/// Bounded probe timeout used by [`wait_for_boot_quiescence_then_reprobe`],
/// matching the 500ms bound already used by the identity probe inside
/// [`kill_and_respawn`].
const BOOT_FENCE_PROBE_TIMEOUT_MS: u64 = 500;

/// Outcome of waiting for a concurrent cold-boot (migrations + pack schema
/// plans / FTS DDL) to finish, then re-checking daemon liveness once quiescent.
enum BootFenceOutcome {
    /// Boot quiesced and a live, identity-matching daemon answered — keep
    /// sending the real frame to it.
    DaemonReady,
    /// Boot quiesced and the probe definitively found no daemon — safe to
    /// fall back to local dispatch.
    SafeLocalFallback,
    /// Daemon state is unknown (lock/join failure, or the post-quiescence
    /// probe itself timed out) — must not local-dispatch.
    HardError(McpError),
}

/// #667: the readiness-timeout branch of `forward_or_spawn`'s send loop used
/// to return `None` (silent local fallback) purely because a freshly spawned
/// daemon had not answered within the fixed deadline — including while that
/// daemon was still inside its boot guard running migrations/pack schema
/// plans (FTS DDL). A local writer/searcher racing in at exactly that moment
/// could observe or create a partially-initialized `notes`/`fts_notes` schema.
///
/// This blocks on the SAME boot guard the daemon holds across cold-boot
/// schema init (ADR-D3): acquiring it here can only succeed once no boot is
/// in progress, so acquiring-then-immediately-dropping it is a pure
/// quiescence wait. Only after that wait does it re-probe daemon identity —
/// distinguishing "was still booting, now ready" from "genuinely no daemon"
/// so the caller never has to guess which one caused the original timeout.
async fn wait_for_boot_quiescence_then_reprobe(frame: &DaemonRequestFrame) -> BootFenceOutcome {
    // `acquire_daemon_boot_guard` performs a blocking `flock`; run it on the
    // blocking pool rather than the async executor.
    let quiesced =
        tokio::task::spawn_blocking(khive_runtime::daemon::acquire_daemon_boot_guard).await;
    match quiesced {
        Ok(Ok(guard)) => {
            // The guard's only purpose here is proving quiescence; drop it
            // immediately so it does not itself block a real boot or the
            // re-probe below.
            drop(guard);
        }
        Ok(Err(e)) => {
            return BootFenceOutcome::HardError(daemon_mcp_error(
                format!(
                    "failed to acquire daemon boot/recovery lock while waiting for \
                     cold-boot quiescence: {e}"
                ),
                None,
            ));
        }
        Err(e) => {
            return BootFenceOutcome::HardError(daemon_mcp_error(
                format!("boot-quiescence wait task failed: {e}"),
                None,
            ));
        }
    }

    match probe_daemon_identity(
        &frame.config_id,
        &frame.namespace,
        BOOT_FENCE_PROBE_TIMEOUT_MS,
    )
    .await
    {
        ProbeOutcome::Alive => BootFenceOutcome::DaemonReady,
        ProbeOutcome::Dead => BootFenceOutcome::SafeLocalFallback,
        ProbeOutcome::Timeout => BootFenceOutcome::HardError(daemon_mcp_error(
            "daemon state uncertain after cold-boot quiescence; not falling back to \
             local dispatch to avoid racing a possibly still-initializing index",
            None,
        )),
        // `probe_daemon_identity` (unlike `quiesce_then_probe_identity`) never
        // constructs `LockContended` — it has no lock-acquisition step of its
        // own. Handled here only for match exhaustiveness over the shared
        // `ProbeOutcome` type; same fail-safe HardError as `Timeout` if it
        // were ever reached.
        ProbeOutcome::LockContended => BootFenceOutcome::HardError(daemon_mcp_error(
            "daemon state uncertain after cold-boot quiescence (lock probe unexpectedly \
             contended); not falling back to local dispatch",
            None,
        )),
    }
}

/// Forward a request to the daemon, auto-spawning it if absent.
///
/// Returns `None` only when nothing was ever written to the daemon and local
/// dispatch is therefore safe: `KHIVE_NO_DAEMON` is set, or the socket is
/// definitively absent/refused (`NoSocket`) — never when connection access is
/// denied (`Unreachable`) and never after the real frame has been written.
/// `Some(Ok)` / `Some(Err)` both mean the caller must not dispatch locally.
/// Under `KHIVE_DAEMON_STRICT=1` the `NoSocket` case instead becomes
/// `Some(Err(..))` (`KHIVE_NO_DAEMON` is unaffected — it is an explicit caller
/// opt-out, not a fallback).
///
/// This conservative entry point never replays a fully written frame.
/// The CLI/MCP policy-aware entry points additionally permit one classified
/// read replay after EOF/reset. A `NoSocket` outcome never writes anything,
/// so it is safe to recover the daemon and retry. Once the real frame IS
/// fully written (`ParseFailure`/`ProtocolMismatch`), this returns a hard
/// error immediately instead of killing/respawning/retrying or falling back
/// locally (#644). See `crates/khive-mcp/docs/api/daemon-lifecycle.md`.
pub async fn forward_or_spawn(frame: &DaemonRequestFrame) -> Option<Result<String, McpError>> {
    forward_or_spawn_with(frame, &spawn_daemon).await
}

/// Forward a request while preserving an explicit config selection (and an
/// ephemeral `:memory:` database override) on a daemon this call may need to
/// spawn.
///
/// An already-running daemon is still matched exclusively by `config_id`.
/// `config` and `db` only supply construction inputs for a missing daemon;
/// without them, `kkernel exec --config <path>` would spawn `kkernel mcp
/// --daemon` against automatic discovery and immediately disagree with the
/// request it was spawned to serve, and `kkernel exec --db :memory:` would
/// bind the fresh daemon to the config's declared persistent files.
///
/// `packs`, when given, is the caller's already-resolved pack list (whatever
/// mix of CLI `--pack`, `KHIVE_PACKS`, or config-file `[runtime].packs`
/// produced it) forwarded verbatim as explicit `--pack` args on the spawned
/// daemon — see the doc comment on `spawn_daemon_with_exe_and_config`.
pub async fn forward_or_spawn_with_config_and_packs(
    frame: &DaemonRequestFrame,
    config: Option<&std::path::Path>,
    db: Option<&str>,
    packs: Option<&[String]>,
) -> Option<Result<String, McpError>> {
    let replay_read_only = !frame.probe_only
        && !frame.metrics_only
        && !frame.plan
        && crate::request_policy::read_replay_safe(&frame.ops);
    forward_or_spawn_with_replay_policy(frame, config, db, packs, replay_read_only).await
}

pub(crate) async fn forward_or_spawn_with_replay_policy(
    frame: &DaemonRequestFrame,
    config: Option<&std::path::Path>,
    db: Option<&str>,
    packs: Option<&[String]>,
    replay_read_only: bool,
) -> Option<Result<String, McpError>> {
    #[cfg(any(test, feature = "test-forward-seam"))]
    if let Some(intercepted) = test_forward_seam::intercept(frame, packs) {
        return intercepted;
    }
    let spawn = || {
        let exe = std::env::current_exe()?;
        spawn_daemon_with_exe_and_config(&exe, config, db, packs)
    };
    forward_or_spawn_with_policy(frame, &spawn, replay_read_only).await
}

/// Forward a request, spawning the daemon if needed, without an explicit
/// pack list: the spawned daemon falls back to its own pack resolution
/// (config-file `[runtime].packs` or the built-in default set), exactly as
/// it did before pack forwarding existed.
///
/// Thin compatibility wrapper over
/// [`forward_or_spawn_with_config_and_packs`] preserving the pre-existing
/// three-argument public signature, so external callers of the published
/// crate keep compiling unchanged.
pub async fn forward_or_spawn_with_config(
    frame: &DaemonRequestFrame,
    config: Option<&std::path::Path>,
    db: Option<&str>,
) -> Option<Result<String, McpError>> {
    forward_or_spawn_with_config_and_packs(frame, config, db, None).await
}

/// Test-only capture hook armed at the real entry of
/// `forward_or_spawn_with_config_and_packs` — the actual adapter-boundary conversion
/// site (`packs.as_deref()` at each production call site) production code
/// runs through, as opposed to a `ForwardFnPtr` spy that stands in for the
/// whole adapter and never executes its conversion. Unarmed, `intercept`
/// costs one thread-local read and returns `None` immediately: the
/// production path is byte-for-byte unaffected.
///
/// Gated by `cfg(any(test, feature = "test-forward-seam"))` so both the
/// in-crate `khive-mcp` test suite (plain `cfg(test)`) and a cross-crate
/// `kkernel` test (which cannot see `khive-mcp`'s `cfg(test)` items, so it
/// enables the `test-forward-seam` feature via a `[dev-dependencies]`
/// re-declaration instead) can reach it.
#[cfg(any(test, feature = "test-forward-seam"))]
pub mod test_forward_seam {
    use super::{DaemonRequestFrame, McpError};
    use std::cell::Cell;

    thread_local! {
        static ARMED: Cell<bool> = const { Cell::new(false) };
        static CAPTURED: std::cell::RefCell<Option<Option<Vec<String>>>> =
            const { std::cell::RefCell::new(None) };
        static CAPTURED_REQUEST_ID: std::cell::RefCell<Option<Option<u64>>> =
            const { std::cell::RefCell::new(None) };
    }

    /// Arm the one-shot hook. The next call into
    /// `forward_or_spawn_with_config_and_packs` on this thread records its `packs`
    /// argument and returns a canned success without touching a socket or
    /// spawning a process; the hook then disarms itself.
    pub fn arm() {
        CAPTURED.with(|c| *c.borrow_mut() = None);
        CAPTURED_REQUEST_ID.with(|c| *c.borrow_mut() = None);
        ARMED.with(|a| a.set(true));
    }

    /// Take the captured `packs` argument from the most recent armed call,
    /// if any, and disarm the hook. Disarming here means a hook armed for a
    /// call that never reached `intercept` cannot survive to intercept a
    /// later, unrelated call on the same thread.
    pub fn take_captured() -> Option<Option<Vec<String>>> {
        ARMED.with(|a| a.set(false));
        CAPTURED.with(|c| c.borrow_mut().take())
    }

    /// Take the captured `frame.request_id` of the most recent armed call,
    /// if any. Companion to `take_captured` for tests proving the bridge
    /// correlation id (khive#948, khive-oss#2337) reaches the real
    /// `forward_or_spawn_with_config_and_packs` adapter boundary unchanged.
    pub fn take_captured_request_id() -> Option<Option<u64>> {
        CAPTURED_REQUEST_ID.with(|c| c.borrow_mut().take())
    }

    pub(super) fn intercept(
        frame: &DaemonRequestFrame,
        packs: Option<&[String]>,
    ) -> Option<Option<Result<String, McpError>>> {
        let was_armed = ARMED.with(|a| a.replace(false));
        if !was_armed {
            return None;
        }
        CAPTURED.with(|c| *c.borrow_mut() = Some(packs.map(<[String]>::to_vec)));
        CAPTURED_REQUEST_ID.with(|c| *c.borrow_mut() = Some(frame.request_id));
        Some(Some(Ok(serde_json::json!({
            "results": [{"ok": true, "tool": "stats", "result": {}}],
            "summary": {"total": 1, "succeeded": 1, "failed": 0},
        })
        .to_string())))
    }
}

#[cfg(test)]
async fn forward_or_spawn_with_exe(
    frame: &DaemonRequestFrame,
    exe: &std::path::Path,
) -> Option<Result<String, McpError>> {
    let spawn = || spawn_daemon_with_exe(exe);
    forward_or_spawn_with(frame, &spawn).await
}

async fn forward_or_spawn_with<F>(
    frame: &DaemonRequestFrame,
    spawn: &F,
) -> Option<Result<String, McpError>>
where
    F: Fn() -> std::io::Result<std::process::Child> + Sync,
{
    forward_or_spawn_with_policy(frame, spawn, false).await
}

#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq)]
enum SupervisorDiscoveryPoint {
    UnmanagedRetry,
    BeforeRecovery,
    RecoveryAdmission,
    SupervisorWait,
}

#[cfg(test)]
type SupervisorDiscoveryHook = (SupervisorDiscoveryPoint, Box<dyn FnOnce() + Send>);

#[cfg(test)]
static SUPERVISOR_DISCOVERY_HOOK: std::sync::Mutex<Option<SupervisorDiscoveryHook>> =
    std::sync::Mutex::new(None);

#[cfg(test)]
fn supervisor_discovery_hook(point: SupervisorDiscoveryPoint) {
    let action = {
        let mut hook = SUPERVISOR_DISCOVERY_HOOK.lock().expect("discovery hook");
        if hook.as_ref().is_some_and(|(at, _)| *at == point) {
            hook.take().map(|(_, action)| action)
        } else {
            None
        }
    };
    if let Some(action) = action {
        action();
    }
}

async fn forward_or_spawn_with_policy<F>(
    frame: &DaemonRequestFrame,
    spawn: &F,
    replay_read_only: bool,
) -> Option<Result<String, McpError>>
where
    F: Fn() -> std::io::Result<std::process::Child> + Sync,
{
    if env_truthy("KHIVE_NO_DAEMON") {
        return None;
    }

    let request_started = tokio::time::Instant::now();
    let mut replay = ReadReplayBudget::new(
        replay_read_only && crate::request_policy::read_replay_safe(&frame.ops),
    );
    let mut first = try_forward_with_read_replay(frame, &mut replay, None).await;
    let mut initial_supervisor_marker: Option<SupervisorMarker> = None;
    let mut degraded_bootstrap = false;
    if matches!(first, ForwardOutcome::NoSocket) {
        if let Some(marker) = read_supervisor_marker() {
            let budget = initial_supervisor_marker.get_or_insert(marker).clone();
            let waited =
                match wait_for_supervisor(frame, &mut replay, request_started, budget).await {
                    Ok(outcome) => outcome,
                    Err(error) => return Some(Err(error)),
                };
            degraded_bootstrap |= waited.degraded_bootstrap;
            first = waited.outcome;
        } else if recorded_daemon_is_alive() {
            // Unmanaged daemon handover retains its existing reconnect grace.
            let deadline = bounded_retry_deadline();
            while matches!(first, ForwardOutcome::NoSocket)
                && tokio::time::Instant::now() < deadline
                && !khive_storage::request_read_is_cancelled()
            {
                sleep_until_retry(deadline).await;
                #[cfg(test)]
                supervisor_discovery_hook(SupervisorDiscoveryPoint::UnmanagedRetry);
                if let Some(marker) = read_supervisor_marker() {
                    let budget = initial_supervisor_marker.get_or_insert(marker).clone();
                    let waited = match wait_for_supervisor(
                        frame,
                        &mut replay,
                        request_started,
                        budget,
                    )
                    .await
                    {
                        Ok(outcome) => outcome,
                        Err(error) => return Some(Err(error)),
                    };
                    degraded_bootstrap |= waited.degraded_bootstrap;
                    first = waited.outcome;
                    break;
                }
                if tokio::time::Instant::now() >= deadline
                    || khive_storage::request_read_is_cancelled()
                {
                    break;
                }
                first = try_forward_with_read_replay(frame, &mut replay, Some(deadline)).await;
            }
        }
    }
    // A claim may arrive at the end of unmanaged grace or without a PID file.
    // Never restart a supervisor budget already consumed by this request.
    if matches!(first, ForwardOutcome::NoSocket) && !degraded_bootstrap {
        #[cfg(test)]
        supervisor_discovery_hook(SupervisorDiscoveryPoint::BeforeRecovery);
        if let Some(marker) = read_supervisor_marker() {
            let budget = initial_supervisor_marker.get_or_insert(marker).clone();
            let waited =
                match wait_for_supervisor(frame, &mut replay, request_started, budget).await {
                    Ok(outcome) => outcome,
                    Err(error) => return Some(Err(error)),
                };
            degraded_bootstrap |= waited.degraded_bootstrap;
            first = waited.outcome;
        }
    }
    let marker_guard = loop {
        if matches!(first, ForwardOutcome::NoSocket)
            && (khive_storage::request_read_is_cancelled()
                || khive_storage::capture_request_read_context()
                    .deadline()
                    .is_some_and(|deadline| tokio::time::Instant::now() >= deadline.async_at()))
        {
            return Some(Err(daemon_reconnect_expired_before_dispatch_error()));
        }
        match first {
            ForwardOutcome::Response(resp) => {
                return map_response(*resp, &frame.config_id, &frame.namespace)
            }
            ForwardOutcome::NoSocket => {
                // No claim yet, or its bounded startup grace expired.
            }
            ForwardOutcome::Unreachable {
                kind,
                os_error_code,
            } => return Some(Err(daemon_unreachable_error(frame, kind, os_error_code))),
            ForwardOutcome::ParseFailure | ForwardOutcome::ResponseLost => {
                let config_id = opaque_config_id(&frame.config_id);
                tracing::warn!(
                    config_id = %config_id,
                    namespace = %frame.namespace,
                    retry_suppressed = true,
                    "daemon connection lost after the request was fully written — \
                     not retrying or falling back locally to avoid duplicate dispatch"
                );
                return Some(Err(ambiguous_forward_error()));
            }
            ForwardOutcome::ProtocolMismatch {
                daemon_protocol_version,
            } => {
                let config_id = opaque_config_id(&frame.config_id);
                tracing::warn!(
                    config_id = %config_id,
                    namespace = %frame.namespace,
                    retry_suppressed = true,
                    "daemon protocol mismatch discovered after the request was fully \
                     written — not retrying or falling back locally to avoid duplicate dispatch"
                );
                // #714: the daemon is fine (it just rejected us) — this bridge
                // process itself is the stale one. Trigger self-heal alongside
                // the hard error below, never in place of it.
                trigger_bridge_self_heal();
                return Some(Err(protocol_mismatch_error(
                    protocol_mismatch_message(daemon_protocol_version),
                    None,
                )));
            }
            ForwardOutcome::RequestTooLarge { bytes } => {
                return Some(Err(request_too_large_error(bytes)));
            }
        }

        // This is after the last unlocked marker read and inside recovery
        // admission. A launcher that publishes first holds the same lock.
        #[cfg(test)]
        supervisor_discovery_hook(SupervisorDiscoveryPoint::RecoveryAdmission);
        let guard = match acquire_supervisor_marker_lock().await {
            Ok(guard) => guard,
            Err(error) => {
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::Interrupted
                ) {
                    return Some(Err(supervisor_marker_lock_wait_error()));
                }
                tracing::warn!(error = %error, "supervisor marker lock unavailable; suppressing lifecycle recovery");
                return Some(Err(daemon_mcp_error(
                    "supervisor marker ownership could not be established; retry the request",
                    Some(serde_json::json!({"reason": "supervisor_marker_lock_unavailable"})),
                )));
            }
        };
        if khive_storage::request_read_is_cancelled()
            || khive_storage::capture_request_read_context()
                .deadline()
                .is_some_and(|deadline| tokio::time::Instant::now() >= deadline.async_at())
        {
            return Some(Err(daemon_reconnect_expired_before_dispatch_error()));
        }
        if let Some(marker) = read_supervisor_marker() {
            if !degraded_bootstrap {
                drop(guard);
                let budget = initial_supervisor_marker.get_or_insert(marker).clone();
                let waited =
                    match wait_for_supervisor(frame, &mut replay, request_started, budget).await {
                        Ok(outcome) => outcome,
                        Err(error) => return Some(Err(error)),
                    };
                degraded_bootstrap |= waited.degraded_bootstrap;
                first = waited.outcome;
                continue;
            }
        }
        // The first supervisor's bounded grace has elapsed, or the locked
        // read found no declaration. Rewrites never restart that first bound.
        // Keep the lock through this client's spawn and readiness wait, never
        // through a supervisor wait (whose launcher must reacquire it).
        break guard;
    };

    // NoSocket: nothing has been written yet. Establish a live daemon — either
    // this is the first-ever spawn or a stale one needs replacing — under the
    // single recovery lock, using only `probe_only` frames for the identity
    // check. `kill_and_respawn` is a no-op kill when there is nothing stale to
    // remove, so first-spawn and recovery share this one path.
    //
    // #898: `spawned_child` holds the live handle from `RecoveryOutcome::Spawned`
    // (if THIS call actually spawned one) purely so the two "about to give up
    // and fall back locally" points below can check whether that specific
    // attempt already exited — never polled eagerly, never used to cut the
    // connect-retry window or the #667 boot-quiescence wait short.
    let mut spawned_child: Option<std::process::Child> = None;
    // Only a client that actually spawns keeps the marker lock through its
    // ADR-049 readiness wait. A skipped/uncertain recovery did not acquire a
    // child to protect and must let a waiting launcher publish immediately.
    let mut marker_guard = Some(marker_guard);
    let recovery = kill_and_respawn(&frame.config_id, &frame.namespace, spawn).await;
    match recovery {
        Err(RecoveryError::RequestExpired) => {
            return Some(Err(daemon_reconnect_expired_before_dispatch_error()));
        }
        Err(RecoveryError::Spawn(e)) => {
            // #898: `Command::spawn` itself failed to start the child at all —
            // an unambiguous, already-fully-diagnosed respawn failure. Loud in
            // both strict and non-strict mode; never a silent local fallback.
            return Some(Err(respawn_failed_error(RespawnFailure::SpawnError {
                os_error_code: e.raw_os_error(),
            })));
        }
        Err(RecoveryError::IncumbentStillAlive { pid }) => {
            return Some(Err(incumbent_still_alive_error(pid)));
        }
        Err(RecoveryError::PidFileDirectoryUntrusted(message)) => {
            return Some(Err(untrusted_pid_file_directory_error(message)));
        }
        Ok(RecoveryOutcome::Skipped) => {
            // A concurrent client already has a live matching daemon ready.
            drop(marker_guard.take());
        }
        Ok(RecoveryOutcome::Spawned(child)) => {
            // Give the kernel a moment to release the socket path and let the
            // spawned daemon process start.
            spawned_child = Some(child);
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        Ok(RecoveryOutcome::Uncertain) => {
            drop(marker_guard.take());
            // Could not positively confirm the daemon's state within the
            // deadline (#838) — behave like `Skipped` (never
            // kill on an unconfirmed state) and let the forward loop below
            // discover the real state: it will either reach a daemon a peer
            // is spawning, or hit the readiness deadline and fall through to
            // `wait_for_boot_quiescence_then_reprobe`.
            tracing::debug!(
                "daemon recovery state uncertain; forwarding without a fresh kill+spawn"
            );
        }
    }

    // Send the real frame now that a daemon is confirmed ready
    // (or believed ready via Skipped). The connect attempt inside
    // `try_forward_before` doubles as the readiness check — a `NoSocket`
    // outcome here just means "not listening yet" (nothing written), so keep
    // retrying. Only the explicit read policy may replay a lost response;
    // every other post-write outcome is terminal and returned immediately.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut boot_fence_confirmed = false;
    loop {
        if khive_storage::request_read_is_cancelled()
            || khive_storage::capture_request_read_context()
                .deadline()
                .is_some_and(|deadline| tokio::time::Instant::now() >= deadline.async_at())
        {
            return Some(Err(daemon_reconnect_expired_before_dispatch_error()));
        }
        if tokio::time::Instant::now() >= deadline {
            if boot_fence_confirmed {
                return Some(Err(daemon_reconnect_expired_before_dispatch_error()));
            }
            // #667: a bare timeout here does not mean "no daemon" — it may
            // mean "daemon is still inside cold-boot schema init". Wait for
            // that boot (if any) to quiesce and re-probe before deciding.
            match wait_for_boot_quiescence_then_reprobe(frame).await {
                BootFenceOutcome::DaemonReady => {
                    // This 500 ms identity probe already confirmed readiness.
                    // Release the launcher's marker lock before dispatch;
                    // repeating the shorter 100 ms probe can otherwise loop
                    // forever after the five-second retry bound.
                    drop(marker_guard.take());
                    boot_fence_confirmed = true;
                }
                BootFenceOutcome::SafeLocalFallback => {
                    // #898: only now — after the full connect-retry window AND
                    // the #667 boot-quiescence wait, exactly as before — check
                    // whether the respawn THIS call made has already exited.
                    // A confirmed exit is unambiguous (this process spawned
                    // that exact child) and is never treated as the
                    // legitimate ADR-049 no-daemon case.
                    if let Some(child) = spawned_child.as_mut() {
                        if let Ok(Some(status)) = child.try_wait() {
                            return Some(Err(respawn_failed_error(
                                RespawnFailure::ExitedBeforeBind {
                                    exit_code: status.code(),
                                },
                            )));
                        }
                    }
                    return fallback_or_reject(
                        FallbackReason::NoSocket,
                        &frame.config_id,
                        None,
                        &frame.namespace,
                    );
                }
                BootFenceOutcome::HardError(err) => return Some(Err(err)),
            }
        }
        if khive_storage::request_read_is_cancelled()
            || khive_storage::capture_request_read_context()
                .deadline()
                .is_some_and(|deadline| tokio::time::Instant::now() >= deadline.async_at())
        {
            return Some(Err(daemon_reconnect_expired_before_dispatch_error()));
        }
        if spawned_child.is_some() && marker_guard.is_some() {
            // The launcher may be waiting for this marker lock. Release it as
            // soon as our daemon answers an identity probe, before the real
            // request is dispatched (which may run for much longer).
            if matches!(
                probe_daemon_identity(&frame.config_id, &frame.namespace, 100).await,
                ProbeOutcome::Alive
            ) {
                drop(marker_guard.take());
            } else {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                continue;
            }
        }
        match try_forward_with_read_replay(frame, &mut replay, None).await {
            ForwardOutcome::Response(resp) => {
                return map_response(*resp, &frame.config_id, &frame.namespace)
            }
            ForwardOutcome::RequestTooLarge { bytes } => {
                return Some(Err(request_too_large_error(bytes)));
            }
            ForwardOutcome::ParseFailure | ForwardOutcome::ResponseLost => {
                let config_id = opaque_config_id(&frame.config_id);
                tracing::warn!(
                    config_id = %config_id,
                    namespace = %frame.namespace,
                    retry_suppressed = true,
                    "freshly-established daemon connection lost after the request \
                     was fully written — not retrying or falling back locally"
                );
                return Some(Err(ambiguous_forward_error()));
            }
            ForwardOutcome::ProtocolMismatch {
                daemon_protocol_version,
            } => {
                let config_id = opaque_config_id(&frame.config_id);
                tracing::warn!(
                    config_id = %config_id,
                    namespace = %frame.namespace,
                    "daemon protocol mismatch discovered on the post-recovery retry \
                     — not retrying again or falling back locally"
                );
                // #714: same self-heal trigger as the first-attempt arm above —
                // either arm can observe the mismatch depending on whether this
                // was the first probe or a retry after a kill/respawn.
                trigger_bridge_self_heal();
                return Some(Err(protocol_mismatch_error(
                    protocol_mismatch_message(daemon_protocol_version),
                    None,
                )));
            }
            ForwardOutcome::Unreachable {
                kind,
                os_error_code,
            } => return Some(Err(daemon_unreachable_error(frame, kind, os_error_code))),
            ForwardOutcome::NoSocket => {
                if boot_fence_confirmed {
                    return Some(Err(daemon_reconnect_expired_before_dispatch_error()));
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
    }
}

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

#[cfg(test)]
#[path = "daemon_tests.rs"]
mod tests;
