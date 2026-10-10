//! Daemon forwarding entrypoints, boot fencing and recovery policy.

#[cfg(test)]
use super::spawn_daemon_with_exe;
use super::{
    acquire_supervisor_marker_lock, ambiguous_forward_error, bounded_retry_deadline,
    daemon_mcp_error, daemon_reconnect_expired_before_dispatch_error, daemon_unreachable_error,
    env_truthy, fallback_or_reject, incumbent_still_alive_error, kill_and_respawn, map_response,
    opaque_config_id, probe_daemon_identity, protocol_mismatch_error, protocol_mismatch_message,
    read_supervisor_marker, recorded_daemon_is_alive, request_too_large_error,
    respawn_failed_error, sleep_until_retry, spawn_daemon, spawn_daemon_with_exe_and_config,
    supervisor_marker_lock_wait_error, trigger_bridge_self_heal, try_forward_with_read_replay,
    untrusted_pid_file_directory_error, wait_for_supervisor, DaemonRequestFrame, FallbackReason,
    ForwardOutcome, McpError, ProbeOutcome, ReadReplayBudget, RecoveryError, RecoveryOutcome,
    RespawnFailure, SupervisorMarker,
};

/// Bounded probe timeout used by [`wait_for_boot_quiescence_then_reprobe`],
/// matching the 500ms bound already used by the identity probe inside
/// [`kill_and_respawn`].
pub(super) const BOOT_FENCE_PROBE_TIMEOUT_MS: u64 = 500;

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
pub(super) async fn forward_or_spawn_with_exe(
    frame: &DaemonRequestFrame,
    exe: &std::path::Path,
) -> Option<Result<String, McpError>> {
    let spawn = || spawn_daemon_with_exe(exe);
    forward_or_spawn_with(frame, &spawn).await
}

pub(super) async fn forward_or_spawn_with<F>(
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
pub(super) enum SupervisorDiscoveryPoint {
    UnmanagedRetry,
    BeforeRecovery,
    RecoveryAdmission,
    SupervisorWait,
}

#[cfg(test)]
pub(super) type SupervisorDiscoveryHook = (SupervisorDiscoveryPoint, Box<dyn FnOnce() + Send>);

#[cfg(test)]
pub(super) static SUPERVISOR_DISCOVERY_HOOK: std::sync::Mutex<Option<SupervisorDiscoveryHook>> =
    std::sync::Mutex::new(None);

#[cfg(test)]
pub(super) fn supervisor_discovery_hook(point: SupervisorDiscoveryPoint) {
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

pub(super) async fn forward_or_spawn_with_policy<F>(
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
                tracing::warn!(target: "khive_mcp::daemon",
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
                tracing::warn!(target: "khive_mcp::daemon",
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
                tracing::warn!(target: "khive_mcp::daemon", error = %error, "supervisor marker lock unavailable; suppressing lifecycle recovery");
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
            tracing::debug!(target: "khive_mcp::daemon",
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
                tracing::warn!(target: "khive_mcp::daemon",
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
                tracing::warn!(target: "khive_mcp::daemon",
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
