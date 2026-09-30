//! Quiescent, guarded conversion of an existing code map from WAL to DELETE.
//!
//! This runs before the ordinary code-map pool or any schema access. In
//! particular, a generic SQLite connection must never be used to inspect the
//! old WAL header or to perform the conversion.

use std::fmt::Display;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rusqlite::{Connection, ErrorCode, OpenFlags};
use thiserror::Error;

use super::{
    callbacks, os, vfs, AdmissionSnapshot, CodeMapHandleGuard, GuardError, Mode, OpenAccess,
    ProductionBase, Role,
};

#[cfg(test)]
thread_local! {
    static AFTER_DELETE_PRAGMA: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn set_after_delete_pragma(hook: impl FnOnce() + 'static) {
    AFTER_DELETE_PRAGMA.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
}

fn after_delete_pragma() {
    #[cfg(test)]
    AFTER_DELETE_PRAGMA.with(|slot| {
        if let Some(hook) = slot.borrow_mut().take() {
            hook();
        }
    });
}

#[derive(Debug, Error)]
pub(crate) enum TransitionError {
    #[error("code-map WAL transition incomplete at {stage}: {reason}")]
    Incomplete {
        stage: &'static str,
        reason: String,
        /// True when another SQLite client held a lock or checkpoint pin.
        busy: bool,
    },
    #[error("code-map WAL transition PARTIAL at {stage}: {reason}")]
    Partial { stage: &'static str, reason: String },
}

impl TransitionError {
    pub(crate) fn is_busy(&self) -> bool {
        matches!(self, Self::Incomplete { busy: true, .. })
    }
}

fn incomplete(stage: &'static str, error: impl Display) -> TransitionError {
    TransitionError::Incomplete {
        stage,
        reason: error.to_string(),
        busy: false,
    }
}

fn incomplete_busy(stage: &'static str, error: impl Display) -> TransitionError {
    TransitionError::Incomplete {
        stage,
        reason: format!("BUSY: {error}"),
        busy: true,
    }
}

fn partial(stage: &'static str, error: impl Display) -> TransitionError {
    TransitionError::Partial {
        stage,
        reason: error.to_string(),
    }
}

fn sqlite_is_busy(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(sqlite, _)
            if matches!(sqlite.code, ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked)
    )
}

fn incomplete_sql(stage: &'static str, error: rusqlite::Error) -> TransitionError {
    if sqlite_is_busy(&error) {
        incomplete_busy(stage, error)
    } else {
        incomplete(stage, error)
    }
}

fn require_no_shm_calls(
    baseline: usize,
    stage: &'static str,
    after_mode_switch: bool,
) -> Result<(), TransitionError> {
    if callbacks::shm_violation_count() == baseline {
        return Ok(());
    }
    let reason = "the guarded transition invoked a forbidden xShm callback";
    Err(if after_mode_switch {
        partial(stage, reason)
    } else {
        incomplete(stage, reason)
    })
}

fn no_wal_or_shm(snapshot: &AdmissionSnapshot) -> bool {
    snapshot.wal.is_none() && snapshot.shm.is_none()
}

/// A live guarded rollback pool may reopen the same DELETE database. Prove
/// that state with a second rollback guard instead of weakening the
/// quiescence rule for a real WAL-to-DELETE transition. Every observation and
/// the header read still use native handles from the protected parent.
fn prove_live_rollback_header(
    target: &Path,
    protected: &[ProductionBase],
    initial: &AdmissionSnapshot,
) -> bool {
    if !no_wal_or_shm(initial) {
        return false;
    }
    let Ok(guard) =
        CodeMapHandleGuard::new(target.to_path_buf(), Mode::Rollback, protected.to_vec())
    else {
        return false;
    };
    let Ok(opened) = guard.open(Role::Main, OpenAccess::ReadOnly) else {
        return false;
    };
    let header_is_delete = matches!(os::sqlite_header_mode(opened.file()), Ok(Some((1, 1))));
    drop(opened);
    header_is_delete && guard.preflight().is_ok_and(|after| &after == initial)
}

/// Prepare an existing target for the steady-state, guarded rollback pool.
///
/// A new/empty map or an already-DELETE map has no transition to perform;
/// the caller must still construct its ordinary pool through the rollback
/// VFS. Once the DELETE PRAGMA is attempted, every error is `Partial`, since
/// SQLite may already have checkpointed the WAL or changed the main header.
pub(crate) fn prepare_rollback_target(
    target: PathBuf,
    protected: Vec<ProductionBase>,
) -> Result<(), TransitionError> {
    let transition_guard = Arc::new(
        CodeMapHandleGuard::new(
            target.clone(),
            Mode::QuiescentWalTransition,
            protected.clone(),
        )
        .map_err(|error| incomplete("admission", error))?,
    );
    let initial = transition_guard
        .preflight()
        .map_err(|error| incomplete("sidecar admission", error))?;
    if initial.main.is_none() {
        return if no_wal_or_shm(&initial) && initial.journal.is_none() {
            Ok(())
        } else {
            Err(incomplete(
                "header detection",
                "sidecars exist without a code-map main database",
            ))
        };
    }

    // This is an attested native handle, not a path read or a SQLite open.
    let header = {
        let opened = match transition_guard.open(Role::Main, OpenAccess::ReadOnly) {
            Ok(opened) => opened,
            Err(error) => {
                if matches!(
                    &error,
                    GuardError::Unsafe {
                        reason: "WAL transition requires no open code-map connection",
                        ..
                    }
                ) && prove_live_rollback_header(&target, &protected, &initial)
                {
                    return Ok(());
                }
                return Err(incomplete("header open", error));
            }
        };
        os::sqlite_header_mode(opened.file()).map_err(|error| incomplete("header read", error))?
    };
    match header {
        None | Some((1, 1)) => {
            return if no_wal_or_shm(&initial) {
                Ok(())
            } else {
                Err(incomplete(
                    "rollback admission",
                    "rollback header has a WAL or SHM sidecar",
                ))
            };
        }
        Some((2, 2)) => {}
        _ => {
            return Err(incomplete(
                "header detection",
                "unsupported SQLite header mode",
            ))
        }
    }
    if initial.journal.is_some() {
        return Err(incomplete(
            "WAL admission",
            "WAL target also has a rollback journal",
        ));
    }

    // register() does not replace SQLite's default VFS. The old WAL target is
    // opened only through this transition-specific guarded VFS.
    let transition_vfs = vfs::register(Arc::clone(&transition_guard))
        .map_err(|error| incomplete("VFS registration", error))?;
    let shm_before = callbacks::shm_violation_count();
    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let conn = Connection::open_with_flags_and_vfs(&target, flags, transition_vfs.as_str())
        .map_err(|error| incomplete_sql("guarded WAL open", error))?;
    conn.busy_timeout(Duration::ZERO)
        .map_err(|error| incomplete_sql("zero busy timeout", error))?;

    // This must be the first SQL statement. BEGIN EXCLUSIVE forces SQLite to
    // acquire the actual lock promptly; the VFS callback evidence below
    // checks ownership rather than trusting the PRAGMA's returned word.
    let locking_mode: String = conn
        .query_row("PRAGMA locking_mode=EXCLUSIVE", [], |row| row.get(0))
        .map_err(|error| incomplete_sql("exclusive mode", error))?;
    if !locking_mode.eq_ignore_ascii_case("exclusive") {
        return Err(incomplete(
            "exclusive mode",
            "SQLite did not accept EXCLUSIVE locking mode",
        ));
    }
    conn.execute_batch("BEGIN EXCLUSIVE; COMMIT")
        .map_err(|error| incomplete_sql("exclusive acquisition", error))?;
    transition_guard
        .verify_exclusive_lock()
        .map_err(|error| incomplete("exclusive proof", error))?;
    require_no_shm_calls(shm_before, "exclusive acquisition", false)?;

    let journal_mode: String = conn
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .map_err(|error| incomplete_sql("WAL mode confirmation", error))?;
    if !journal_mode.eq_ignore_ascii_case("wal") {
        return Err(incomplete(
            "WAL mode confirmation",
            "the target changed journal mode before the guarded transition",
        ));
    }
    conn.pragma_update(None, "synchronous", "FULL")
        .map_err(|error| incomplete_sql("durable checkpoint setup", error))?;
    let (busy, log_frames, checkpointed_frames): (i64, i64, i64) = conn
        .query_row("PRAGMA wal_checkpoint(FULL)", [], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .map_err(|error| incomplete_sql("FULL checkpoint", error))?;
    if busy != 0 {
        return Err(incomplete_busy(
            "FULL checkpoint",
            "another client still holds WAL frames",
        ));
    }
    let all_frames_durable = (log_frames >= 0 && checkpointed_frames == log_frames)
        || (log_frames == -1 && checkpointed_frames == -1 && initial.wal.is_none());
    if !all_frames_durable {
        return Err(incomplete(
            "FULL checkpoint",
            format!(
                "checkpoint did not cover every frame (log={log_frames}, checkpointed={checkpointed_frames})"
            ),
        ));
    }

    // The first comparison protects every old name; a WAL created by this
    // exact VFS may be adopted, but an unguarded new SHM is always refused.
    transition_guard
        .verify_transition_companions(&initial)
        .map_err(|error| incomplete("pre-switch sidecar recheck", error))?;
    let pre_switch = transition_guard
        .preflight()
        .map_err(|error| incomplete("pre-switch attestation", error))?;
    transition_guard
        .verify_exclusive_lock()
        .map_err(|error| incomplete("pre-switch exclusive proof", error))?;
    require_no_shm_calls(shm_before, "pre-switch attestation", false)?;
    transition_guard
        .authorize_mode_switch()
        .map_err(|error| incomplete("mode-switch authorization", error))?;

    // The one-shot xFileControl gate permits this DELETE setter only now.
    // Even an error from this statement is PARTIAL: the disk may have moved.
    let switched: String = conn
        .pragma_update_and_check(None, "journal_mode", "DELETE", |row| row.get(0))
        .map_err(|error| partial("DELETE mode switch", error))?;
    if !switched.eq_ignore_ascii_case("delete") {
        return Err(partial(
            "DELETE mode switch",
            format!("SQLite reported journal_mode={switched:?}"),
        ));
    }
    after_delete_pragma();
    transition_guard
        .verify_exclusive_lock()
        .map_err(|error| partial("post-switch exclusive proof", error))?;
    transition_guard
        .delete_attested_transition_companions(&pre_switch)
        .map_err(|error| partial("attested sidecar cleanup", error))?;
    let after_cleanup = transition_guard
        .preflight()
        .map_err(|error| partial("post-cleanup attestation", error))?;
    if !no_wal_or_shm(&after_cleanup) {
        return Err(partial(
            "post-cleanup attestation",
            "WAL or SHM sidecar remains after DELETE switch",
        ));
    }
    require_no_shm_calls(shm_before, "DELETE transition", true)?;
    conn.close()
        .map_err(|(_, error)| partial("transition connection close", error))?;

    // A new, steady-state guard proves both the post-switch header and
    // sidecar absence before the caller may construct a pool or migrate.
    let rollback_guard = Arc::new(
        CodeMapHandleGuard::new(target.clone(), Mode::Rollback, protected)
            .map_err(|error| partial("rollback admission", error))?,
    );
    let rollback_vfs = vfs::register(Arc::clone(&rollback_guard))
        .map_err(|error| partial("rollback VFS registration", error))?;
    let reopened = Connection::open_with_flags_and_vfs(&target, flags, rollback_vfs.as_str())
        .map_err(|error| partial("guarded rollback reopen", error))?;
    let final_mode: String = reopened
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .map_err(|error| partial("rollback mode proof", error))?;
    if !final_mode.eq_ignore_ascii_case("delete") {
        return Err(partial(
            "rollback mode proof",
            format!("reopened target reports journal_mode={final_mode:?}"),
        ));
    }
    let final_snapshot = rollback_guard
        .preflight()
        .map_err(|error| partial("rollback sidecar proof", error))?;
    if !no_wal_or_shm(&final_snapshot) {
        return Err(partial(
            "rollback sidecar proof",
            "guarded rollback reopen created WAL or SHM",
        ));
    }
    require_no_shm_calls(shm_before, "guarded rollback reopen", true)?;
    reopened
        .close()
        .map_err(|(_, error)| partial("rollback proof close", error))?;
    Ok(())
}
