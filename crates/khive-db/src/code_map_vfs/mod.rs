//! Native-handle admission for the code-map-only SQLite VFS (ADR-085 A12).

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use thiserror::Error;

#[cfg(unix)]
mod unix;
#[cfg(unix)]
use unix as os;
mod callbacks;
#[cfg(test)]
mod tests;
mod transition;
mod vfs;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows as os;

const QUARANTINE_CAP: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    Rollback,
    QuiescentWalTransition,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Role {
    Main,
    Journal,
    TransitionWal,
    Shm,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OpenAccess {
    ReadOnly,
    ReadWrite,
    Create,
    CreateNew,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ProductionKind {
    Main,
    Events,
}

#[derive(Debug, Clone)]
pub(crate) struct ProductionBase {
    pub(crate) path: PathBuf,
    pub(crate) kind: ProductionKind,
}

#[derive(Debug, Error)]
pub(crate) enum GuardError {
    #[error("code-map VFS path {path:?} is not an absolute, plain filesystem path")]
    InvalidPath { path: PathBuf },
    #[error("code-map VFS cannot prove {path:?}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("code-map VFS refuses {path:?}: {reason}")]
    Unsafe { path: PathBuf, reason: &'static str },
    #[error("code-map VFS refuses {path:?}: protected production identity")]
    ProtectedAlias { path: PathBuf },
    #[error("code-map VFS protected production paths changed during admission")]
    ProtectedChanged,
    #[error("code-map VFS refuses {role:?} in {mode:?} mode")]
    RoleNotAllowed { role: Role, mode: Mode },
    #[error(
        "code-map VFS opened-handle quarantine is full; restart the process before another code-map open"
    )]
    QuarantineFull,
    #[error("code-map VFS registration capacity is full; restart the process before another code-map open")]
    RegistrationFull,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Observed {
    identity: os::Identity,
    links: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProtectedSample {
    path: PathBuf,
    kind: ProductionKind,
    suffix: &'static str,
    observed: Option<Observed>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AdmissionSnapshot {
    main: Option<os::Identity>,
    journal: Option<os::Identity>,
    wal: Option<os::Identity>,
    shm: Option<os::Identity>,
}

#[derive(Debug)]
pub(crate) struct GuardedFile {
    file: Option<File>,
    identity: os::Identity,
    role: Role,
    mode: Mode,
}

impl GuardedFile {
    fn file(&self) -> &File {
        self.file.as_ref().expect("guarded handle remains open")
    }

    fn take_file(&mut self) -> File {
        self.file.take().expect("guarded handle is closed once")
    }
}

impl Drop for GuardedFile {
    fn drop(&mut self) {
        if let Some(file) = self.file.take() {
            os::close_unlocked(file, self.identity);
        }
        if self.role == Role::Main {
            let mut ledger = lock_ledger();
            if let Some(active) = ledger.active_main.get_mut(&self.identity) {
                let count = match self.mode {
                    Mode::Rollback => &mut active.rollback,
                    Mode::QuiescentWalTransition => &mut active.transition,
                };
                *count = count.saturating_sub(1);
                if active.rollback == 0 && active.transition == 0 {
                    ledger.active_main.remove(&self.identity);
                }
            }
        }
    }
}

#[derive(Default)]
struct ActiveMain {
    rollback: usize,
    transition: usize,
}

#[derive(Default)]
struct ProcessLedger {
    guarded: Vec<(os::Identity, Role)>,
    quarantined: Vec<File>,
    active_main: HashMap<os::Identity, ActiveMain>,
}

fn ledger() -> &'static Mutex<ProcessLedger> {
    static LEDGER: OnceLock<Mutex<ProcessLedger>> = OnceLock::new();
    LEDGER.get_or_init(|| Mutex::new(ProcessLedger::default()))
}

fn lock_ledger() -> std::sync::MutexGuard<'static, ProcessLedger> {
    ledger()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn check_quarantine(ledger: &ProcessLedger) -> Result<(), GuardError> {
    if ledger.quarantined.len() >= QUARANTINE_CAP {
        Err(GuardError::QuarantineFull)
    } else {
        Ok(())
    }
}

#[cfg(any(test, feature = "test-support"))]
pub(crate) fn quarantine_occupancy() -> usize {
    lock_ledger().quarantined.len()
}

#[cfg(test)]
thread_local! {
    static BEFORE_OS_OPEN: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn set_before_os_open(hook: impl FnOnce() + 'static) {
    BEFORE_OS_OPEN.with(|cell| *cell.borrow_mut() = Some(Box::new(hook)));
}

fn before_os_open() {
    #[cfg(test)]
    BEFORE_OS_OPEN.with(|cell| {
        if let Some(hook) = cell.borrow_mut().take() {
            hook();
        }
    });
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

fn child_name(main: &OsString, suffix: &str) -> OsString {
    let mut name = main.clone();
    name.push(suffix);
    name
}

fn ensure_absolute_plain(path: &Path) -> Result<(), GuardError> {
    if !path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                std::path::Component::CurDir | std::path::Component::ParentDir
            )
        })
    {
        return Err(GuardError::InvalidPath {
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

fn io_at(path: &Path, source: std::io::Error) -> GuardError {
    GuardError::Io {
        path: path.to_path_buf(),
        source,
    }
}

pub(crate) struct CodeMapHandleGuard {
    target: PathBuf,
    main_leaf: OsString,
    parent: os::PinnedParent,
    mode: Mode,
    protected: Vec<ProductionBase>,
    last_protected: Mutex<Vec<ProtectedSample>>,
    opened_wal: Mutex<Vec<os::Identity>>,
    // 0 = closed, 1 = authorized for one DELETE PRAGMA, 2 = consumed.
    mode_switch_phase: AtomicU8,
    exclusive_main_handles: AtomicUsize,
    // The VFS can only answer SQLITE_CANTOPEN; the reason it discards is kept
    // here until the caller that sees the error takes it.
    last_refusal: Mutex<Option<String>>,
}

pub(crate) fn register_rollback(
    target: PathBuf,
    protected: Vec<ProductionBase>,
) -> Result<String, GuardError> {
    let guard = Arc::new(CodeMapHandleGuard::new(target, Mode::Rollback, protected)?);
    vfs::register(guard)
}

pub(crate) use transition::prepare_rollback_target;

/// SQLite's message for `SQLITE_CANTOPEN`, which a migration keeps when it
/// renders a failed statement's error as text.
const CANTOPEN_MESSAGE: &str = "unable to open database file";

/// Run `operation` on a connection of the code-map VFS `vfs_name` and name the
/// guard refusal behind a `SQLITE_CANTOPEN` it fails with. A refusal recorded
/// before the operation started is discarded first, so the reason appended is
/// one the guard recorded while the operation ran. Other connections of the
/// same VFS record into, and take from, the same slot; the pool admits one
/// writer, so during a connection open or a core-schema migration only a
/// concurrent reader open can interleave, and it can supply or consume that
/// reason.
pub(crate) fn naming_refusal<T>(
    vfs_name: &str,
    operation: impl FnOnce() -> Result<T, crate::error::SqliteError>,
) -> Result<T, crate::error::SqliteError> {
    let _earlier = vfs::take_refusal(vfs_name);
    operation().map_err(|error| with_refusal(error, vfs_name))
}

/// Append the guard refusal last recorded for `vfs_name` to an error SQLite
/// reported for it, keeping the error's variant and code. Only an open that
/// SQLite reports as `SQLITE_CANTOPEN`, directly or inside a failed migration,
/// takes the refusal; any other error is returned unchanged and leaves it.
fn with_refusal(error: crate::error::SqliteError, vfs_name: &str) -> crate::error::SqliteError {
    use crate::error::SqliteError;
    match error {
        SqliteError::Rusqlite(rusqlite::Error::SqliteFailure(code, message))
            if code.code == rusqlite::ErrorCode::CannotOpen =>
        {
            let message = match (message, vfs::take_refusal(vfs_name)) {
                (Some(message), Some(reason)) => Some(format!("{message}; {reason}")),
                (None, Some(reason)) => Some(reason),
                (message, None) => message,
            };
            SqliteError::Rusqlite(rusqlite::Error::SqliteFailure(code, message))
        }
        // A migration renders its failure as text; only an open failure can
        // have a guard refusal behind it.
        SqliteError::Migration { version, error } if error.contains(CANTOPEN_MESSAGE) => {
            match vfs::take_refusal(vfs_name) {
                Some(reason) => SqliteError::Migration {
                    version,
                    error: format!("{error}; {reason}"),
                },
                None => SqliteError::Migration { version, error },
            }
        }
        other => other,
    }
}

impl CodeMapHandleGuard {
    fn record_refusal(&self, reason: String) {
        *self
            .last_refusal
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(reason);
    }

    fn take_refusal(&self) -> Option<String> {
        self.last_refusal
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }

    pub(crate) fn new(
        target: PathBuf,
        mode: Mode,
        protected: Vec<ProductionBase>,
    ) -> Result<Self, GuardError> {
        let ledger = lock_ledger();
        check_quarantine(&ledger)?;
        ensure_absolute_plain(&target)?;
        for base in &protected {
            ensure_absolute_plain(&base.path)?;
        }
        let parent_path = target.parent().ok_or_else(|| GuardError::InvalidPath {
            path: target.clone(),
        })?;
        let main_leaf = target
            .file_name()
            .ok_or_else(|| GuardError::InvalidPath {
                path: target.clone(),
            })?
            .to_os_string();
        let parent =
            os::PinnedParent::open(parent_path).map_err(|error| io_at(parent_path, error))?;
        let guard = Self {
            target,
            main_leaf,
            parent,
            mode,
            protected,
            last_protected: Mutex::new(Vec::new()),
            opened_wal: Mutex::new(Vec::new()),
            mode_switch_phase: AtomicU8::new(0),
            exclusive_main_handles: AtomicUsize::new(0),
            last_refusal: Mutex::new(None),
        };
        guard.preflight_locked(&ledger)?;
        Ok(guard)
    }

    pub(crate) fn preflight(&self) -> Result<AdmissionSnapshot, GuardError> {
        let ledger = lock_ledger();
        check_quarantine(&ledger)?;
        let result = self.preflight_locked(&ledger);
        if result.is_err() {
            self.refresh_after_refusal();
        }
        result
    }

    pub(crate) fn verify_transition_companions(
        &self,
        initial: &AdmissionSnapshot,
    ) -> Result<(), GuardError> {
        if self.mode != Mode::QuiescentWalTransition {
            return Err(GuardError::RoleNotAllowed {
                role: Role::TransitionWal,
                mode: self.mode,
            });
        }
        let current = self.preflight()?;
        let wal_is_attested = self.wal_matches_attestation(initial, &current);
        if current.main != initial.main || !wal_is_attested || current.shm != initial.shm {
            return Err(GuardError::ProtectedChanged);
        }
        Ok(())
    }

    pub(crate) fn authorize_mode_switch(&self) -> Result<(), GuardError> {
        if self.mode != Mode::QuiescentWalTransition {
            return Err(GuardError::RoleNotAllowed {
                role: Role::TransitionWal,
                mode: self.mode,
            });
        }
        self.verify_exclusive_lock()?;
        self.mode_switch_phase
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| ())
            .map_err(|_| GuardError::Unsafe {
                path: self.target.clone(),
                reason: "WAL transition mode switch was already authorized or attempted",
            })
    }

    fn consume_mode_switch_authorization(&self) -> bool {
        self.mode == Mode::QuiescentWalTransition
            && self
                .mode_switch_phase
                .compare_exchange(1, 2, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
    }

    pub(crate) fn verify_exclusive_lock(&self) -> Result<(), GuardError> {
        if self.mode != Mode::QuiescentWalTransition
            || self.exclusive_main_handles.load(Ordering::Acquire) != 1
        {
            return Err(GuardError::Unsafe {
                path: self.target.clone(),
                reason: "WAL transition has not proved an exclusive native main-file lock",
            });
        }
        Ok(())
    }

    pub(crate) fn delete_attested_transition_companions(
        &self,
        initial: &AdmissionSnapshot,
    ) -> Result<(), GuardError> {
        if self.mode != Mode::QuiescentWalTransition
            || self.mode_switch_phase.load(Ordering::Acquire) != 2
        {
            return Err(GuardError::RoleNotAllowed {
                role: Role::TransitionWal,
                mode: self.mode,
            });
        }
        for role in [Role::TransitionWal, Role::Shm] {
            let current = self.preflight()?;
            if let Some(now) = self.snapshot_role(&current, role) {
                let attested = if role == Role::TransitionWal {
                    self.wal_matches_attestation(initial, &current)
                } else {
                    self.snapshot_role(initial, role) == Some(now)
                };
                if !attested {
                    return Err(GuardError::ProtectedChanged);
                }
                self.delete(role, true)?;
            }
        }
        Ok(())
    }

    fn wal_matches_attestation(
        &self,
        initial: &AdmissionSnapshot,
        current: &AdmissionSnapshot,
    ) -> bool {
        current.wal == initial.wal
            || (initial.wal.is_none()
                && current.wal.is_some_and(|identity| {
                    self.opened_wal
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .contains(&identity)
                }))
    }

    pub(crate) fn open(&self, role: Role, access: OpenAccess) -> Result<GuardedFile, GuardError> {
        let mut ledger = lock_ledger();
        check_quarantine(&ledger)?;
        let result = self.open_locked(&mut ledger, role, access);
        if result.is_err() {
            self.refresh_after_refusal();
        }
        result
    }

    pub(crate) fn access(&self, role: Role) -> Result<bool, GuardError> {
        let ledger = lock_ledger();
        check_quarantine(&ledger)?;
        let result = (|| {
            let snapshot = self.preflight_locked(&ledger)?;
            Ok(self.snapshot_role(&snapshot, role).is_some())
        })();
        if result.is_err() {
            self.refresh_after_refusal();
        }
        result
    }

    pub(crate) fn delete(&self, role: Role, sync_parent: bool) -> Result<(), GuardError> {
        if !matches!(role, Role::Journal | Role::TransitionWal | Role::Shm)
            || (role != Role::Journal && self.mode != Mode::QuiescentWalTransition)
        {
            return Err(GuardError::RoleNotAllowed {
                role,
                mode: self.mode,
            });
        }
        if matches!(role, Role::TransitionWal | Role::Shm)
            && self.mode_switch_phase.load(Ordering::Acquire) != 2
        {
            return Err(GuardError::Unsafe {
                path: self.path(role),
                reason: "WAL companions must remain named before the DELETE mode switch",
            });
        }
        #[cfg(windows)]
        {
            let mut ledger = lock_ledger();
            check_quarantine(&ledger)?;
            let result = self.delete_windows_locked(&mut ledger, role, sync_parent);
            if result.is_err() {
                self.refresh_after_refusal();
            }
            result
        }
        #[cfg(unix)]
        {
            let ledger = lock_ledger();
            check_quarantine(&ledger)?;
            let result = (|| {
                self.preflight_locked(&ledger)?;
                let name = self.leaf(role);
                let path = self.path(role);
                self.parent
                    .delete_child(&name, sync_parent)
                    .map_err(|error| io_at(&path, error))
            })();
            if result.is_err() {
                self.refresh_after_refusal();
            }
            result
        }
    }

    #[cfg(windows)]
    fn delete_windows_locked(
        &self,
        ledger: &mut ProcessLedger,
        role: Role,
        sync_parent: bool,
    ) -> Result<(), GuardError> {
        let snapshot = self.preflight_locked(ledger)?;
        let path = self.path(role);
        let Some(expected) = self.snapshot_role(&snapshot, role) else {
            return Ok(());
        };
        let protected_before = self.sample_protected()?;
        before_os_open();
        let Some(file) = self
            .parent
            .open_delete_child(&self.leaf(role))
            .map_err(|error| io_at(&path, error))?
        else {
            return Ok(());
        };
        let observed = match os::observe_file(&file) {
            Ok(observed) => observed,
            Err(error) => {
                ledger.quarantined.push(file);
                return Err(io_at(&path, error));
            }
        };
        let protected_after = self.sample_protected();
        let matches_before = Self::matches_protected(observed.identity, &protected_before);
        let matches_after = protected_after
            .as_ref()
            .is_ok_and(|samples| Self::matches_protected(observed.identity, samples));
        if matches_before || matches_after {
            ledger.quarantined.push(file);
            return Err(GuardError::ProtectedAlias { path });
        }
        let protected_after = match protected_after {
            Ok(samples) => samples,
            Err(error) => {
                ledger.quarantined.push(file);
                return Err(error);
            }
        };
        if protected_before != protected_after {
            ledger.quarantined.push(file);
            return Err(GuardError::ProtectedChanged);
        }
        if observed.identity != expected {
            return Err(GuardError::Unsafe {
                path,
                reason: "sidecar changed between metadata proof and delete handle open",
            });
        }
        // Check links again on the exact handle immediately before the
        // disposition; the no-share-delete handle also blocks a rename.
        let current = os::observe_file(&file).map_err(|error| io_at(&path, error))?;
        if current.identity != expected || current.links != 1 {
            return Err(GuardError::Unsafe {
                path,
                reason: "sidecar identity or link count changed before deletion",
            });
        }
        self.parent
            .delete_opened_child(file, sync_parent)
            .map_err(|error| io_at(&path, error))
    }

    fn preflight_locked(&self, ledger: &ProcessLedger) -> Result<AdmissionSnapshot, GuardError> {
        self.revalidate_parent()?;
        let first = self.sample_protected()?;
        if first.iter().any(|sample| {
            sample
                .observed
                .is_some_and(|item| ledger.guarded.iter().any(|(id, _)| *id == item.identity))
        }) {
            return Err(GuardError::ProtectedChanged);
        }
        let main = self.stat_target(Role::Main, &first)?;
        let journal = self.stat_target(Role::Journal, &first)?;
        let wal = self.stat_target(Role::TransitionWal, &first)?;
        let shm = self.stat_target(Role::Shm, &first)?;
        if self.mode == Mode::Rollback && (wal.is_some() || shm.is_some()) {
            return Err(GuardError::Unsafe {
                path: self.target.clone(),
                reason: "steady rollback target has a WAL or SHM sidecar",
            });
        }
        let second = self.sample_protected()?;
        if first != second {
            return Err(GuardError::ProtectedChanged);
        }
        *self
            .last_protected
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = second;
        Ok(AdmissionSnapshot {
            main: main.map(|item| item.identity),
            journal: journal.map(|item| item.identity),
            wal: wal.map(|item| item.identity),
            shm: shm.map(|item| item.identity),
        })
    }

    fn open_locked(
        &self,
        ledger: &mut ProcessLedger,
        role: Role,
        access: OpenAccess,
    ) -> Result<GuardedFile, GuardError> {
        if role == Role::Shm
            || (role == Role::TransitionWal && self.mode != Mode::QuiescentWalTransition)
        {
            return Err(GuardError::RoleNotAllowed {
                role,
                mode: self.mode,
            });
        }
        self.preflight_locked(ledger)?;
        let protected_before = self.sample_protected()?;
        let name = self.leaf(role);
        let path = self.path(role);
        before_os_open();
        let file = self
            .parent
            .open_child(&name, access)
            .map_err(|error| io_at(&path, error))?;
        let observed = match os::observe_file(&file) {
            Ok(observed) => observed,
            Err(error) => {
                ledger.quarantined.push(file);
                return Err(io_at(&path, error));
            }
        };
        let protected_after = self.sample_protected();
        let matches_before = Self::matches_protected(observed.identity, &protected_before);
        let matches_after = protected_after
            .as_ref()
            .is_ok_and(|samples| Self::matches_protected(observed.identity, samples));
        if matches_before || matches_after {
            ledger.quarantined.push(file);
            return Err(GuardError::ProtectedAlias { path });
        }
        let protected_after = match protected_after {
            Ok(samples) => samples,
            Err(error) => {
                ledger.quarantined.push(file);
                return Err(error);
            }
        };
        if protected_before != protected_after {
            // A post-open sample drift is an uncertain stat-to-open interval.
            // Keep the live handle so a same-process production lock survives.
            ledger.quarantined.push(file);
            return Err(GuardError::ProtectedChanged);
        }
        if ledger
            .guarded
            .iter()
            .any(|(identity, opened_role)| *identity == observed.identity && *opened_role != role)
        {
            os::close_unlocked(file, observed.identity);
            return Err(GuardError::Unsafe {
                path,
                reason: "opened code-map member aliases another guarded file role",
            });
        }
        if role != Role::Main && observed.links > 1 {
            os::close_unlocked(file, observed.identity);
            return Err(GuardError::Unsafe {
                path,
                reason: "multiply linked writable code-map sidecar",
            });
        }
        if role == Role::Main {
            let header = match os::sqlite_header_mode(&file) {
                Ok(header) => header,
                Err(error) => {
                    os::close_unlocked(file, observed.identity);
                    return Err(io_at(&path, error));
                }
            };
            match (self.mode, header) {
                (Mode::Rollback, Some((1, 1))) | (_, None) => {}
                (Mode::QuiescentWalTransition, Some((1, 1) | (2, 2))) => {}
                _ => {
                    os::close_unlocked(file, observed.identity);
                    return Err(GuardError::Unsafe {
                        path,
                        reason: "SQLite header mode does not match guarded open mode",
                    });
                }
            }
        }
        if role == Role::Main {
            let active = ledger.active_main.entry(observed.identity).or_default();
            match self.mode {
                Mode::Rollback if active.transition > 0 => {
                    os::close_unlocked(file, observed.identity);
                    return Err(GuardError::Unsafe {
                        path,
                        reason: "rollback open overlaps a quiescent WAL transition",
                    });
                }
                Mode::QuiescentWalTransition if active.rollback > 0 || active.transition > 0 => {
                    os::close_unlocked(file, observed.identity);
                    return Err(GuardError::Unsafe {
                        path,
                        reason: "WAL transition requires no open code-map connection",
                    });
                }
                Mode::Rollback => active.rollback += 1,
                Mode::QuiescentWalTransition => active.transition += 1,
            }
        }
        ledger.guarded.push((observed.identity, role));
        if role == Role::TransitionWal {
            self.opened_wal
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(observed.identity);
        }
        Ok(GuardedFile {
            file: Some(file),
            identity: observed.identity,
            role,
            mode: self.mode,
        })
    }

    fn sample_protected(&self) -> Result<Vec<ProtectedSample>, GuardError> {
        let mut samples = Vec::with_capacity(self.protected.len() * 4);
        for base in &self.protected {
            let parent = base
                .path
                .parent()
                .expect("validated absolute production path");
            // A configured production parent may have a platform alias above it.
            // Resolve that root once, then inspect its leaves without following links.
            let pinned = match parent.canonicalize() {
                Ok(physical_parent) => Some(
                    os::PinnedParent::open(&physical_parent)
                        .map_err(|error| io_at(parent, error))?,
                ),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => return Err(io_at(parent, error)),
            };
            for suffix in ["", "-journal", "-wal", "-shm"] {
                let path = with_suffix(&base.path, suffix);
                let observed = match &pinned {
                    Some(parent) => parent
                        .stat_child(path.file_name().expect("validated production leaf"))
                        .map_err(|error| io_at(&path, error))?,
                    None => None,
                };
                samples.push(ProtectedSample {
                    path,
                    kind: base.kind,
                    suffix,
                    observed,
                });
            }
        }
        Ok(samples)
    }

    fn stat_target(
        &self,
        role: Role,
        protected: &[ProtectedSample],
    ) -> Result<Option<Observed>, GuardError> {
        let path = self.path(role);
        let observed = self
            .parent
            .stat_child(&self.leaf(role))
            .map_err(|error| io_at(&path, error))?;
        if let Some(item) = observed {
            if Self::matches_protected(item.identity, protected) {
                return Err(GuardError::ProtectedAlias { path });
            }
            if role != Role::Main && item.links > 1 {
                return Err(GuardError::Unsafe {
                    path,
                    reason: "multiply linked writable code-map sidecar",
                });
            }
        }
        Ok(observed)
    }

    fn matches_protected(identity: os::Identity, protected: &[ProtectedSample]) -> bool {
        protected.iter().any(|sample| {
            sample
                .observed
                .is_some_and(|item| item.identity == identity)
        })
    }

    fn revalidate_parent(&self) -> Result<(), GuardError> {
        let parent_path = self.target.parent().expect("validated absolute target");
        let current =
            os::PinnedParent::open(parent_path).map_err(|error| io_at(parent_path, error))?;
        if current.identity() != self.parent.identity() {
            return Err(GuardError::Unsafe {
                path: parent_path.to_path_buf(),
                reason: "target parent changed since its handle was pinned",
            });
        }
        Ok(())
    }

    fn path(&self, role: Role) -> PathBuf {
        with_suffix(
            &self.target,
            match role {
                Role::Main => "",
                Role::Journal => "-journal",
                Role::TransitionWal => "-wal",
                Role::Shm => "-shm",
            },
        )
    }

    fn leaf(&self, role: Role) -> OsString {
        child_name(
            &self.main_leaf,
            match role {
                Role::Main => "",
                Role::Journal => "-journal",
                Role::TransitionWal => "-wal",
                Role::Shm => "-shm",
            },
        )
    }

    fn snapshot_role(&self, snapshot: &AdmissionSnapshot, role: Role) -> Option<os::Identity> {
        match role {
            Role::Main => snapshot.main,
            Role::Journal => snapshot.journal,
            Role::TransitionWal => snapshot.wal,
            Role::Shm => snapshot.shm,
        }
    }

    fn refresh_after_refusal(&self) {
        if let Ok(samples) = self.sample_protected() {
            *self
                .last_protected
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = samples;
        }
    }
}
