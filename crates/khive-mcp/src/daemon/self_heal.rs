//! Bridge self-heal state, re-exec scheduling and post-flush transport.

#[cfg(unix)]
use khive_runtime::daemon::PROTOCOL_VERSION;

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
pub(super) struct PendingSelfHeal {
    pub(super) action: MismatchRecovery,
    pub(super) executable: Option<std::path::PathBuf>,
}

pub(super) static PENDING_SELF_HEAL: std::sync::Mutex<Option<PendingSelfHeal>> =
    std::sync::Mutex::new(None);

pub(super) fn arm_executable_self_heal(executable: std::path::PathBuf) {
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
pub(super) fn resumed_generation_from_args(args: impl Iterator<Item = String>) -> Option<u32> {
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
pub(super) enum MismatchRecovery {
    /// First generation (no `--resumed-generation` marker) — schedule an
    /// in-place re-exec of the freshest on-disk binary.
    ReexecScheduled,
    /// Already a resumed generation and it hit `ProtocolMismatch` again — the
    /// on-disk binary is itself stale, or a second rebuild race — take the
    /// fallback instead of exec'ing a second time.
    DrainAndExit,
}

pub(super) fn decide_mismatch_recovery(resumed_generation: Option<u32>) -> MismatchRecovery {
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
pub(super) fn trigger_bridge_self_heal() {
    match decide_mismatch_recovery(resumed_generation()) {
        MismatchRecovery::ReexecScheduled => schedule_reexec_on_mismatch(),
        MismatchRecovery::DrainAndExit => {
            tracing::warn!(
                target: "khive_mcp::daemon",
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
        target: "khive_mcp::daemon",
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
            tracing::error!(target: "khive_mcp::daemon", error = %e, "bridge self-heal re-exec failed: could not resolve current_exe");
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
        target: "khive_mcp::daemon",
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
pub(super) fn clear_pending_self_heal() {
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
