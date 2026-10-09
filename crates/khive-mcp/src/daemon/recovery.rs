//! Serialized daemon recovery and caller-visible lifecycle errors.

use khive_runtime::daemon::{acquire_recovery_lock, pid_path, DaemonRequestFrame};
use rmcp::ErrorData as McpError;

use super::fallback::{is_daemon_strict_mode, opaque_config_id, STRICT_FALLBACK_MARKER};
use super::forward::{daemon_mcp_error, SupervisorMarker};
#[cfg(test)]
use super::test_harness::RECOVERY_RACE_BARRIER;
use super::{
    kill_stale_daemon_inner, probe_daemon_identity, remove_daemon_paths_if_still_stale,
    ProbeOutcome, RecoveryError, BOOT_FENCE_PROBE_TIMEOUT_MS, INCUMBENT_EXIT_TIMEOUT_SECS,
};

/// Launch seam for daemon recovery.
///
/// Production closures return a real [`std::process::Child`]. The test harness
/// supplies an in-process handle around the real [`khive_runtime::daemon::run_daemon`] server,
/// allowing parallel recovery to assert server convergence without forking the
/// test binary with synthetic CLI arguments (#539/#544).
pub(super) trait DaemonLauncher: Sync {
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
pub(super) enum RecoveryOutcome<H = std::process::Child> {
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
pub(super) const DEAD_CONFIRM_ROUNDS: u32 = 4;

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
pub(super) async fn quiesce_then_probe_identity(
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
                target: "khive_mcp::daemon",
                BOOT_QUIESCENCE_LOCK_TIMEOUT_MS,
                "boot/recovery lock still contended past its bounded wait; \
                 could not confirm quiescence this round"
            );
            return ProbeOutcome::LockContended;
        }
        Ok(Err(e)) => {
            tracing::warn!(target: "khive_mcp::daemon", error = %e, "failed to probe boot/recovery lock state");
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
pub(super) async fn confirm_genuinely_dead(config_id: &str, namespace: &str) -> ProbeOutcome {
    confirm_genuinely_dead_with_round_observer(config_id, namespace, |_, _| {}).await
}

pub(super) async fn confirm_genuinely_dead_with_round_observer(
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
pub(super) const RECOVERER_LOCK_TIMEOUT_MS: u64 = 16_000;

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
pub(super) async fn kill_and_respawn<F>(
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
pub(super) async fn kill_and_respawn_with_exit_timeout<F>(
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

pub(super) async fn kill_and_respawn_with_launcher<L>(
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
                target: "khive_mcp::daemon",
                RECOVERER_LOCK_TIMEOUT_MS,
                "recoverer lock still contended past its deadline; a peer recoverer \
                 is likely still mid dead-confirmation/kill/spawn — skipping without \
                 a positive confirmation rather than risking a double-spawn"
            );
            return Ok(RecoveryOutcome::Uncertain);
        }
        Ok(Ok(Err(e))) => {
            tracing::warn!(target: "khive_mcp::daemon", error = %e, "failed to acquire recoverer lock");
            return Ok(RecoveryOutcome::Uncertain);
        }
        Ok(Err(_)) | Err(_) => {
            tracing::warn!(target: "khive_mcp::daemon", "recoverer lock acquisition task failed or exceeded its deadline");
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
                target: "khive_mcp::daemon",
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
pub(super) fn ambiguous_forward_error() -> McpError {
    daemon_mcp_error(
        "daemon response lost after request was sent; not retrying or locally \
         dispatching to avoid duplicate execution",
        None,
    )
}

pub(super) fn daemon_unreachable_error(
    frame: &DaemonRequestFrame,
    kind: std::io::ErrorKind,
    os_error_code: Option<i32>,
) -> McpError {
    let config_id = opaque_config_id(&frame.config_id);
    tracing::error!(
        target: "khive_mcp::daemon",
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
pub(super) enum RespawnFailure {
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
pub(super) fn respawn_failed_error(failure: RespawnFailure) -> McpError {
    match failure {
        RespawnFailure::SpawnError { os_error_code } => tracing::error!(
            target: "khive_mcp::daemon",
            reason = "respawn_failed",
            failure_category = "spawn_error",
            ?os_error_code,
            "daemon respawn attempt confirmed failed"
        ),
        RespawnFailure::ExitedBeforeBind { exit_code } => tracing::error!(
            target: "khive_mcp::daemon",
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

pub(super) fn incumbent_still_alive_error(pid: u32) -> McpError {
    tracing::error!(
        target: "khive_mcp::daemon",
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

pub(super) fn untrusted_pid_file_directory_error(message: String) -> McpError {
    tracing::error!(
        target: "khive_mcp::daemon",
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
pub(super) fn supervised_daemon_error(marker: &SupervisorMarker) -> McpError {
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
pub(super) fn supervisor_marker_lock_wait_error() -> McpError {
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

pub(super) fn daemon_reconnect_expired_before_dispatch_error() -> McpError {
    daemon_mcp_error(
        "daemon reconnect deadline expired or request cancelled before dispatch; no lifecycle recovery attempted",
        Some(serde_json::json!({"reason": "daemon_reconnect_expired"})),
    )
}
