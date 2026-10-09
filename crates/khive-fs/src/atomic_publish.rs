//! File staging and atomic replacement within an already held Unix directory.
//!
//! Callers own directory validation and serialization with other writers. Staging is
//! separate from publication so a multi-file commit can preserve its own rename and
//! directory-sync boundaries. Errors do not clean up staging files or undo renames.

use std::ffi::OsStr;
use std::fs::File;
use std::io;
use std::os::fd::AsFd;

use crate::fd_relative::{
    c_name, open_file_at, rename_at, stat_at, unlink_at, Create, OpenFileOptions,
};

/// How an existing staging entry is handled before exclusive creation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StaleTmp {
    /// Refuse nonregular entries, including symlinks; replace stale regular files.
    Refuse,
    /// Unlink a stale file or symlink entry without following it; directories refuse.
    Unlink,
}

/// The operation that failed, without replacing its original I/O error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AtomicPublishPhase {
    ValidateNames,
    InspectTmp,
    RefuseTmp,
    RemoveTmp,
    CreateTmp,
    WriteTmp,
    SyncTmp,
    Rename,
    SyncDirectory,
}

/// A publication failure with its phase and original I/O source, including errno.
#[derive(Debug)]
pub struct AtomicPublishError {
    phase: AtomicPublishPhase,
    source: io::Error,
}

impl AtomicPublishError {
    fn new(phase: AtomicPublishPhase, source: io::Error) -> Self {
        Self { phase, source }
    }

    pub fn phase(&self) -> AtomicPublishPhase {
        self.phase
    }

    pub fn io_error(&self) -> &io::Error {
        &self.source
    }

    /// Recover the original error without wrapping it or losing its OS error code.
    pub fn into_source(self) -> io::Error {
        self.source
    }
}

impl std::fmt::Display for AtomicPublishError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.phase, self.source)
    }
}

impl std::error::Error for AtomicPublishError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

fn validate_name(name: &str) -> io::Result<()> {
    c_name(OsStr::new(name))?;
    if matches!(name, "." | "..") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "dot components are not publication names",
        ));
    }
    Ok(())
}

/// Create, write, file-sync and close one staging file without renaming it.
///
/// The name must be one nonempty component other than `.` or `..`, with no NUL.
/// Creation is exclusive, no-follow and close-on-exec, with mode 0644 filtered by
/// umask. The callback borrows that exact file, which is synced after it succeeds.
/// This does not sync the directory or remove a partial staging file on failure.
pub fn stage_atomic_at(
    dir: &File,
    tmp_name: &str,
    stale: StaleTmp,
    writer: impl FnOnce(&mut File) -> io::Result<()>,
) -> io::Result<()> {
    stage_atomic_at_detailed(dir, tmp_name, stale, writer).map_err(AtomicPublishError::into_source)
}

/// [`stage_atomic_at`] with phase information for caller-specific diagnostics.
pub fn stage_atomic_at_detailed(
    dir: &File,
    tmp_name: &str,
    stale: StaleTmp,
    writer: impl FnOnce(&mut File) -> io::Result<()>,
) -> Result<(), AtomicPublishError> {
    validate_name(tmp_name)
        .map_err(|e| AtomicPublishError::new(AtomicPublishPhase::ValidateNames, e))?;
    stage_validated(dir, tmp_name, stale, writer)
}

fn stage_validated(
    dir: &File,
    tmp_name: &str,
    stale: StaleTmp,
    writer: impl FnOnce(&mut File) -> io::Result<()>,
) -> Result<(), AtomicPublishError> {
    use AtomicPublishPhase as Phase;

    let name = OsStr::new(tmp_name);
    match stale {
        StaleTmp::Refuse => match stat_at(dir, name) {
            Ok(stat) => {
                if stat.st_mode & libc::S_IFMT != libc::S_IFREG {
                    return Err(AtomicPublishError::new(
                        Phase::RefuseTmp,
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            "staging entry is not a regular file",
                        ),
                    ));
                }
                // A name swapped after inspection is only unlinked, never followed.
                unlink_at(dir, name).map_err(|e| AtomicPublishError::new(Phase::RemoveTmp, e))?;
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(AtomicPublishError::new(Phase::InspectTmp, e)),
        },
        StaleTmp::Unlink => match unlink_at(dir, name) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(AtomicPublishError::new(Phase::RemoveTmp, e)),
        },
    }
    let mut file = open_file_at(
        dir.as_fd(),
        tmp_name,
        OpenFileOptions {
            read_write: false,
            create: Create::Exclusive,
            nonblock: false,
            mode: 0o644,
        },
    )
    .map_err(|e| AtomicPublishError::new(Phase::CreateTmp, e))?;
    writer(&mut file).map_err(|e| AtomicPublishError::new(Phase::WriteTmp, e))?;
    file.sync_all()
        .map_err(|e| AtomicPublishError::new(Phase::SyncTmp, e))
}

/// Stage a file, rename it over the destination, then sync the held directory.
///
/// Both names are validated before any filesystem effect and must differ. Names
/// use [`stage_atomic_at`]'s policy. Rename uses the kernel's ordinary replacement
/// semantics. The staging handle is closed before rename; directory sync follows
/// rename. A directory-sync error can therefore leave the new destination installed.
/// No failure performs automatic cleanup or promises rollback. Callers must serialize
/// writers sharing these names, and use staging separately for multi-file commits.
pub fn publish_atomic_at(
    dir: &File,
    tmp_name: &str,
    final_name: &str,
    stale: StaleTmp,
    writer: impl FnOnce(&mut File) -> io::Result<()>,
) -> io::Result<()> {
    publish_atomic_at_detailed(dir, tmp_name, final_name, stale, writer)
        .map_err(AtomicPublishError::into_source)
}

/// [`publish_atomic_at`] with phase information and the original I/O error.
pub fn publish_atomic_at_detailed(
    dir: &File,
    tmp_name: &str,
    final_name: &str,
    stale: StaleTmp,
    writer: impl FnOnce(&mut File) -> io::Result<()>,
) -> Result<(), AtomicPublishError> {
    use AtomicPublishPhase as Phase;

    validate_name(tmp_name).map_err(|e| AtomicPublishError::new(Phase::ValidateNames, e))?;
    validate_name(final_name).map_err(|e| AtomicPublishError::new(Phase::ValidateNames, e))?;
    if tmp_name == final_name {
        return Err(AtomicPublishError::new(
            Phase::ValidateNames,
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "staging and final names must differ",
            ),
        ));
    }
    stage_validated(dir, tmp_name, stale, writer)?;
    rename_at(dir, OsStr::new(tmp_name), dir, OsStr::new(final_name))
        .map_err(|e| AtomicPublishError::new(Phase::Rename, e))?;
    dir.sync_all()
        .map_err(|e| AtomicPublishError::new(Phase::SyncDirectory, e))
}
