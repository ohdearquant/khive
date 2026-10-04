use std::sync::Arc;
use std::time::Instant;

use super::{checkpoint_once_core, CheckpointConfig, CheckpointCoreOutcome, TruncateState};
use crate::pool::ConnectionPool;

#[cfg(test)]
mod panic_tests;

/// A checkpoint cycle that panicked on its blocking thread.
#[derive(Debug)]
pub(super) struct PanickedCycle {
    /// The panic, as reported by the blocking-task join.
    pub(super) join_error: tokio::task::JoinError,
    /// The escalation state the cycle was handed, with the cycle counted as a
    /// TRUNCATE attempt against the cooldown.
    pub(super) truncate_state: TruncateState,
}

/// Run one synchronous checkpoint cycle off this task's Tokio worker thread.
///
/// The cycle issues the PASSIVE observation and, when armed, the TRUNCATE
/// escalation. TRUNCATE lowers the connection's busy timeout and then waits on
/// SQLite for up to `config.truncate_busy_timeout` while a reader pins the
/// WAL, which would hold a worker thread for that whole wait if it ran inline.
/// `conn` and `truncate_state` are moved into the blocking closure and handed
/// back to the caller whenever the cycle returns, so the checkpoint task can
/// restore its dedicated connection and escalation state on every
/// non-panicking path. A panic inside the cycle surfaces as a
/// [`PanickedCycle`]: the connection moved into the task is gone with it, and
/// the caller reopens it instead of taking the checkpoint task down.
///
/// The escalation state does not go down with the closure. A copy taken before
/// the hand-over comes back in the error, so the failure streak and the
/// full-scan reservation keep the values they had when the cycle started. The
/// cycle may have been about to attempt TRUNCATE, so the copy's `last_attempt`
/// is set to the time of the panic: the cooldown stays armed, and a panic that
/// repeats cannot turn sustained WAL pressure into an attempt on every tick.
pub(super) async fn run_checkpoint_core_off_worker(
    pool: Arc<ConnectionPool>,
    conn: rusqlite::Connection,
    config: CheckpointConfig,
    mut truncate_state: TruncateState,
) -> Result<
    (
        rusqlite::Connection,
        TruncateState,
        Result<CheckpointCoreOutcome, rusqlite::Error>,
    ),
    Box<PanickedCycle>,
> {
    let kept = kept_across_a_panic(&truncate_state);
    tokio::task::spawn_blocking(move || {
        let outcome = checkpoint_once_core(&pool, &conn, &config, &mut truncate_state);
        (conn, truncate_state, outcome)
    })
    .await
    .map_err(|join_error| {
        let mut truncate_state = kept;
        truncate_state.last_attempt = Some(Instant::now());
        Box::new(PanickedCycle {
            join_error,
            truncate_state,
        })
    })
}

/// Copy of the escalation state without its cached sidecar report. The report
/// is a diagnostic reuse buffer rather than escalation state, and leaving it
/// out keeps the copy taken before every cycle free of allocation.
fn kept_across_a_panic(state: &TruncateState) -> TruncateState {
    TruncateState {
        last_attempt: state.last_attempt,
        consecutive_failures: state.consecutive_failures,
        #[cfg(unix)]
        legacy_walpin_fallback_interval: state.legacy_walpin_fallback_interval,
        #[cfg(unix)]
        walpin_full_scan_interval: state.walpin_full_scan_interval,
        #[cfg(unix)]
        walpin_full_scan_last_attempt: state.walpin_full_scan_last_attempt,
        #[cfg(unix)]
        walpin_cached_attribution: None,
        #[cfg(unix)]
        sidecar_attribution_attempted_this_tick: false,
    }
}

/// Test seam: makes the checkpoint cycle panic for one database once a
/// TRUNCATE attempt has been decided, so a test can drive the panicked-cycle
/// path of [`run_checkpoint_core_off_worker`].
#[cfg(test)]
pub(super) mod cycle_panic_seam {
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    static HOOK: Mutex<Option<PathBuf>> = Mutex::new(None);

    pub(crate) fn install(db_path: PathBuf) {
        let replaced = HOOK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .replace(db_path);
        assert!(replaced.is_none(), "cycle panic hook already installed");
    }

    pub(crate) fn uninstall() {
        *HOOK.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    }

    /// Called by `maybe_truncate` right after it stamps the attempt time. The
    /// hook is consumed by the first cycle that reaches this point for the
    /// installed database, so later ticks run normally.
    pub(crate) fn after_attempt_decided(db_path: Option<&Path>) {
        let Some(db_path) = db_path else {
            return;
        };
        let armed = {
            let mut guard = HOOK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            match guard.as_ref() {
                Some(path) if path.as_path() == db_path => guard.take(),
                _ => None,
            }
        };
        if armed.is_some() {
            panic!("injected checkpoint cycle panic");
        }
    }
}
