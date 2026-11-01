//! Native-handle admission for the code-map-only SQLite VFS (ADR-085 A12).

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use thiserror::Error;

use crate::file_identity::DatabaseFileIdentity;

#[cfg(unix)]
mod unix;
#[cfg(unix)]
use unix as os;
mod callbacks;
mod ledger;
use ledger::GuardedLedger;
#[cfg(test)]
mod quarantine_budget_tests;
#[cfg(test)]
mod tests;
mod transition;
mod vfs;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows as os;

const QUARANTINE_CAP: usize = 32;
const MAP_QUARANTINE_CAP: usize = 8;

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
    #[error(
        "code-map VFS map opened-handle quarantine budget is full (limit 8); restart the process before another open of this map"
    )]
    MapQuarantineFull,
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
        let mut ledger = lock_ledger();
        ledger.guarded.release(self.identity, self.role);
        if self.role == Role::Main {
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
        ledger.guarded.retire_unlinked();
    }
}

#[derive(Default)]
struct ActiveMain {
    rollback: usize,
    transition: usize,
}

/// A quarantine fault domain is a pinned directory's native identity and exact
/// native leaf spelling. It is independent of mode, expected main identity and
/// protected-path contract. There is no canonicalization, case folding or alias
/// union: hardlinks, case-equivalent leaves and replaced parents may split budgets,
/// but different path slots are never merged. The global cap bounds those splits.
/// Every occupied budget pins its parent so a recycled identity cannot merge slots.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct QuarantineDomain {
    parent: os::Identity,
    leaf: OsString,
}

struct QuarantineBudget {
    _parent: Arc<os::PinnedParent>,
    retained: usize,
    reserved: usize,
}

#[derive(Default)]
struct ProcessLedger {
    guarded: GuardedLedger,
    quarantined: Vec<File>,
    quarantine_by_domain: HashMap<QuarantineDomain, QuarantineBudget>,
    quarantine_reserved: usize,
    active_main: HashMap<os::Identity, ActiveMain>,
}

/// Hold capacity before a native open can produce an unsafe-to-close handle.
/// The borrowed ledger remains locked; rollback never acquires that mutex again.
struct QuarantineReservation<'a> {
    ledger: &'a mut ProcessLedger,
    domain: QuarantineDomain,
    active: bool,
}

impl<'a> QuarantineReservation<'a> {
    fn new(
        ledger: &'a mut ProcessLedger,
        domain: &QuarantineDomain,
        parent: &Arc<os::PinnedParent>,
    ) -> Result<Self, GuardError> {
        check_map_quarantine(ledger, domain)?;
        // Allocate all retention bookkeeping before there can be a suspect File.
        ledger.quarantined.reserve(1);
        let domain = domain.clone();
        let budget = ledger
            .quarantine_by_domain
            .entry(domain.clone())
            .or_insert_with(|| QuarantineBudget {
                _parent: Arc::clone(parent),
                retained: 0,
                reserved: 0,
            });
        budget.reserved += 1;
        ledger.quarantine_reserved += 1;
        Ok(Self {
            ledger,
            domain,
            active: true,
        })
    }

    fn retain(&mut self, file: File) {
        // Never reject or deduplicate after opening: every suspect stays live.
        self.ledger.quarantined.push(file);
        self.ledger
            .quarantine_by_domain
            .get_mut(&self.domain)
            .expect("reserved quarantine domain remains present")
            .retained += 1;
        self.release();
    }

    fn release(&mut self) {
        if !self.active {
            return;
        }
        self.active = false;
        self.ledger.quarantine_reserved -= 1;
        let empty = {
            let budget = self
                .ledger
                .quarantine_by_domain
                .get_mut(&self.domain)
                .expect("reserved quarantine domain remains present");
            budget.reserved -= 1;
            budget.retained == 0 && budget.reserved == 0
        };
        if empty {
            self.ledger.quarantine_by_domain.remove(&self.domain);
        }
    }
}

impl Drop for QuarantineReservation<'_> {
    fn drop(&mut self) {
        self.release();
    }
}

fn ledger() -> &'static Mutex<ProcessLedger> {
    static LEDGER: OnceLock<Mutex<ProcessLedger>> = OnceLock::new();
    LEDGER.get_or_init(|| Mutex::new(ProcessLedger::default()))
}

fn lock_ledger() -> std::sync::MutexGuard<'static, ProcessLedger> {
    let mut ledger = ledger()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    ledger.guarded.retire_unlinked();
    ledger
}

fn check_quarantine(ledger: &ProcessLedger) -> Result<(), GuardError> {
    if ledger.quarantined.len() + ledger.quarantine_reserved >= QUARANTINE_CAP {
        Err(GuardError::QuarantineFull)
    } else {
        Ok(())
    }
}

fn check_map_quarantine(
    ledger: &ProcessLedger,
    domain: &QuarantineDomain,
) -> Result<(), GuardError> {
    if ledger
        .quarantine_by_domain
        .get(domain)
        .is_some_and(|budget| budget.retained + budget.reserved >= MAP_QUARANTINE_CAP)
    {
        return Err(GuardError::MapQuarantineFull);
    }
    check_quarantine(ledger)
}

#[cfg(test)]
fn quarantine_reserved_count() -> usize {
    ledger()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .quarantine_reserved
}

pub(crate) fn quarantine_occupancy() -> usize {
    // Diagnostics must not retire ledger entries as the admission lock does.
    ledger()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .quarantined
        .len()
}

pub(crate) fn diagnostics() -> crate::diagnostics::CodeMapVfsDiagnostics {
    crate::diagnostics::CodeMapVfsDiagnostics {
        quarantine_occupancy: quarantine_occupancy(),
        quarantine_cap: QUARANTINE_CAP,
        registrations: vfs::registration_count(),
        registration_cap: vfs::REGISTRATION_CAP,
    }
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
    parent: Arc<os::PinnedParent>,
    quarantine_domain: QuarantineDomain,
    mode: Mode,
    expected_main_identity: Option<DatabaseFileIdentity>,
    protected: Vec<ProductionBase>,
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

pub(crate) fn register_read_only_rollback(
    target: PathBuf,
    expected_identity: DatabaseFileIdentity,
    protected: Vec<ProductionBase>,
) -> Result<String, GuardError> {
    let guard = Arc::new(CodeMapHandleGuard::new_with_expected_identity(
        target,
        Mode::Rollback,
        Some(expected_identity),
        protected,
    )?);
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
    #[cfg(test)]
    fn quarantine_domain_counts(&self) -> (usize, usize) {
        let ledger = ledger()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        ledger
            .quarantine_by_domain
            .get(&self.quarantine_domain)
            .map_or((0, 0), |budget| (budget.retained, budget.reserved))
    }

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
        Self::new_with_expected_identity(target, mode, None, protected)
    }

    fn new_with_expected_identity(
        target: PathBuf,
        mode: Mode,
        expected_main_identity: Option<DatabaseFileIdentity>,
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
        // A target's parent directory is never created here: directory
        // creation follows symlinks, which no-follow traversal must not do.
        let parent = os::PinnedParent::open(parent_path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                GuardError::Unsafe {
                    path: parent_path.to_path_buf(),
                    reason: "the parent directory of a code-map database must already exist",
                }
            } else {
                io_at(parent_path, error)
            }
        })?;
        let parent = Arc::new(parent);
        let quarantine_domain = QuarantineDomain {
            parent: parent.identity(),
            leaf: main_leaf.clone(),
        };
        let guard = Self {
            target,
            main_leaf,
            parent,
            quarantine_domain,
            mode,
            expected_main_identity,
            protected,
            opened_wal: Mutex::new(Vec::new()),
            mode_switch_phase: AtomicU8::new(0),
            exclusive_main_handles: AtomicUsize::new(0),
            last_refusal: Mutex::new(None),
        };
        check_map_quarantine(&ledger, &guard.quarantine_domain)?;
        guard.preflight_locked(&ledger)?;
        Ok(guard)
    }

    pub(crate) fn preflight(&self) -> Result<AdmissionSnapshot, GuardError> {
        let ledger = lock_ledger();
        check_map_quarantine(&ledger, &self.quarantine_domain)?;
        self.preflight_locked(&ledger)
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
        check_map_quarantine(&ledger, &self.quarantine_domain)?;
        self.open_locked(&mut ledger, role, access)
    }

    pub(crate) fn access(&self, role: Role) -> Result<bool, GuardError> {
        let ledger = lock_ledger();
        check_map_quarantine(&ledger, &self.quarantine_domain)?;
        let snapshot = self.preflight_locked(&ledger)?;
        Ok(self.snapshot_role(&snapshot, role).is_some())
    }

    pub(crate) fn delete(&self, role: Role, sync_parent: bool) -> Result<(), GuardError> {
        if !matches!(role, Role::Journal | Role::TransitionWal | Role::Shm)
            || (role != Role::Journal && self.mode != Mode::QuiescentWalTransition)
            || self.expected_main_identity.is_some()
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
            check_map_quarantine(&ledger, &self.quarantine_domain)?;
            self.delete_windows_locked(&mut ledger, role, sync_parent)
        }
        #[cfg(unix)]
        {
            let ledger = lock_ledger();
            check_map_quarantine(&ledger, &self.quarantine_domain)?;
            self.preflight_locked(&ledger)?;
            let name = self.leaf(role);
            let path = self.path(role);
            self.parent
                .delete_child(&name, sync_parent)
                .map_err(|error| io_at(&path, error))
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
        let mut reservation =
            QuarantineReservation::new(ledger, &self.quarantine_domain, &self.parent)?;
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
                reservation.retain(file);
                return Err(io_at(&path, error));
            }
        };
        let protected_after = self.sample_protected();
        let matches_before = Self::matches_protected(observed.identity, &protected_before);
        let matches_after = protected_after
            .as_ref()
            .is_ok_and(|samples| Self::matches_protected(observed.identity, samples));
        if matches_before || matches_after {
            reservation.retain(file);
            return Err(GuardError::ProtectedAlias { path });
        }
        let protected_after = match protected_after {
            Ok(samples) => samples,
            Err(error) => {
                reservation.retain(file);
                return Err(error);
            }
        };
        if protected_before != protected_after {
            reservation.retain(file);
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
                .is_some_and(|item| ledger.guarded.contains_identity(item.identity))
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
        if self.expected_main_identity.is_some() && access != OpenAccess::ReadOnly {
            return Err(GuardError::Unsafe {
                path: self.path(role),
                reason: "registered code maps require read-only file handles",
            });
        }
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
        let mut reservation =
            QuarantineReservation::new(ledger, &self.quarantine_domain, &self.parent)?;
        before_os_open();
        let file = self
            .parent
            .open_child(&name, access)
            .map_err(|error| io_at(&path, error))?;
        let observed = match os::observe_file(&file) {
            Ok(observed) => observed,
            Err(error) => {
                reservation.retain(file);
                return Err(io_at(&path, error));
            }
        };
        let protected_after = self.sample_protected();
        let matches_before = Self::matches_protected(observed.identity, &protected_before);
        let matches_after = protected_after
            .as_ref()
            .is_ok_and(|samples| Self::matches_protected(observed.identity, samples));
        if matches_before || matches_after {
            reservation.retain(file);
            return Err(GuardError::ProtectedAlias { path });
        }
        let protected_after = match protected_after {
            Ok(samples) => samples,
            Err(error) => {
                reservation.retain(file);
                return Err(error);
            }
        };
        if protected_before != protected_after {
            // A post-open sample drift is an uncertain stat-to-open interval.
            // Keep the live handle so a same-process production lock survives.
            reservation.retain(file);
            return Err(GuardError::ProtectedChanged);
        }
        let ledger = &mut *reservation.ledger;
        if role == Role::Main {
            if let Some(expected) = self.expected_main_identity {
                let actual = match crate::file_identity::database_file_identity_from_file(&file) {
                    Ok(actual) => actual,
                    Err(error) => {
                        os::close_unlocked(file, observed.identity);
                        return Err(io_at(&path, error));
                    }
                };
                if actual != expected {
                    os::close_unlocked(file, observed.identity);
                    return Err(GuardError::Unsafe {
                        path,
                        reason: "main file identity changed since registration",
                    });
                }
                // A second link names the registered file from outside the map root.
                if observed.links != 1 {
                    os::close_unlocked(file, observed.identity);
                    return Err(GuardError::Unsafe {
                        path,
                        reason: "registered map main file does not have exactly one link",
                    });
                }
            }
        }
        if ledger.guarded.has_other_role(observed.identity, role) {
            os::close_unlocked(file, observed.identity);
            return Err(GuardError::Unsafe {
                path,
                reason: "opened code-map member aliases another guarded file role",
            });
        }
        if role != Role::Main && observed.links > 1 && self.expected_main_identity.is_none() {
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
        if let Some(active) = ledger
            .active_main
            .get(&observed.identity)
            .filter(|_| role == Role::Main)
        {
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
                _ => {}
            }
        }
        if let Err(error) = ledger.guarded.record(&file, observed.identity, role) {
            os::close_unlocked(file, observed.identity);
            return Err(io_at(&path, error));
        }
        if role == Role::Main {
            let active = ledger.active_main.entry(observed.identity).or_default();
            match self.mode {
                Mode::Rollback => active.rollback += 1,
                Mode::QuiescentWalTransition => active.transition += 1,
            }
        }
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

    /// Stat every configured production member and companion afresh. No sample
    /// outlives the admission that took it, so every open, access and delete
    /// sees the current names, including after an earlier refusal.
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
            if role != Role::Main && item.links > 1 && self.expected_main_identity.is_none() {
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
}

/// Open code-map main handles on the file at `path`. Counting one file keeps
/// the number independent of other code-map handles open in the process. The
/// identity is read through the parent directory, never by opening the file:
/// closing a second descriptor would release this process's locks on it.
#[cfg(any(test, feature = "test-support"))]
pub(crate) fn active_main_handle_count_for(path: &Path) -> usize {
    let parent = path
        .parent()
        .and_then(|parent| parent.canonicalize().ok())
        .expect("code-map file has a resolvable parent directory");
    let observed = os::PinnedParent::open(&parent)
        .and_then(|parent| parent.stat_child(path.file_name().expect("code-map path names a file")))
        .expect("code-map file identity is readable")
        .expect("code-map file exists");
    lock_ledger()
        .active_main
        .get(&observed.identity)
        .map_or(0, |active| active.rollback + active.transition)
}
