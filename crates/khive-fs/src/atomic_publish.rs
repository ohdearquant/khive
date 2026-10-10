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
    /// Refuse every existing entry via exclusive creation, without inspecting or removing it.
    RefuseExisting,
}

/// Staging policy and creation permissions, filtered by the process umask.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AtomicPublishOptions {
    pub stale: StaleTmp,
    pub mode: u32,
}

impl From<StaleTmp> for AtomicPublishOptions {
    fn from(stale: StaleTmp) -> Self {
        Self { stale, mode: 0o644 }
    }
}

// Fault overrides exist only in this crate's unit-test build. Release builds
// always call File::sync_all, with no public hook or process-global switch.
#[derive(Default)]
struct Syncs {
    #[cfg(test)]
    file: Option<fn(&File) -> io::Result<()>>,
    #[cfg(test)]
    directory: Option<fn(&File) -> io::Result<()>>,
}

impl Syncs {
    fn file(&self, file: &File) -> io::Result<()> {
        #[cfg(test)]
        if let Some(sync) = self.file {
            return sync(file);
        }
        file.sync_all()
    }

    fn directory(&self, dir: &File) -> io::Result<()> {
        #[cfg(test)]
        if let Some(sync) = self.directory {
            return sync(dir);
        }
        dir.sync_all()
    }
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

fn validate_name(name: &OsStr) -> io::Result<()> {
    c_name(name)?;
    if name == OsStr::new(".") || name == OsStr::new("..") {
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
/// Creation is exclusive, no-follow and close-on-exec. Passing a [`StaleTmp`]
/// retains mode 0644; [`AtomicPublishOptions`] selects a mode filtered by umask.
/// The callback borrows that exact file, which is synced after it succeeds.
/// This does not sync the directory or remove a partial staging file on failure.
pub fn stage_atomic_at(
    dir: &File,
    tmp_name: impl AsRef<OsStr>,
    options: impl Into<AtomicPublishOptions>,
    writer: impl FnOnce(&mut File) -> io::Result<()>,
) -> io::Result<()> {
    stage_atomic_at_detailed(dir, tmp_name, options, writer)
        .map_err(AtomicPublishError::into_source)
}

/// [`stage_atomic_at`] with phase information for caller-specific diagnostics.
pub fn stage_atomic_at_detailed(
    dir: &File,
    tmp_name: impl AsRef<OsStr>,
    options: impl Into<AtomicPublishOptions>,
    writer: impl FnOnce(&mut File) -> io::Result<()>,
) -> Result<(), AtomicPublishError> {
    let tmp_name = tmp_name.as_ref();
    validate_name(tmp_name)
        .map_err(|e| AtomicPublishError::new(AtomicPublishPhase::ValidateNames, e))?;
    stage_validated(dir, tmp_name, options.into(), writer, &Syncs::default())
}

fn stage_validated(
    dir: &File,
    tmp_name: &OsStr,
    options: AtomicPublishOptions,
    writer: impl FnOnce(&mut File) -> io::Result<()>,
    syncs: &Syncs,
) -> Result<(), AtomicPublishError> {
    use AtomicPublishPhase as Phase;

    let name = tmp_name;
    match options.stale {
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
        StaleTmp::RefuseExisting => {}
    }
    let mut file = open_file_at(
        dir.as_fd(),
        tmp_name,
        OpenFileOptions {
            read_write: false,
            create: Create::Exclusive,
            nonblock: false,
            mode: options.mode,
        },
    )
    .map_err(|e| AtomicPublishError::new(Phase::CreateTmp, e))?;
    writer(&mut file).map_err(|e| AtomicPublishError::new(Phase::WriteTmp, e))?;
    syncs
        .file(&file)
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
    tmp_name: impl AsRef<OsStr>,
    final_name: impl AsRef<OsStr>,
    options: impl Into<AtomicPublishOptions>,
    writer: impl FnOnce(&mut File) -> io::Result<()>,
) -> io::Result<()> {
    publish_atomic_at_detailed(dir, tmp_name, final_name, options, writer)
        .map_err(AtomicPublishError::into_source)
}

/// [`publish_atomic_at`] with phase information and the original I/O error.
pub fn publish_atomic_at_detailed(
    dir: &File,
    tmp_name: impl AsRef<OsStr>,
    final_name: impl AsRef<OsStr>,
    options: impl Into<AtomicPublishOptions>,
    writer: impl FnOnce(&mut File) -> io::Result<()>,
) -> Result<(), AtomicPublishError> {
    publish_with_syncs(
        dir,
        tmp_name.as_ref(),
        final_name.as_ref(),
        options.into(),
        writer,
        &Syncs::default(),
    )
}

fn publish_with_syncs(
    dir: &File,
    tmp_name: &OsStr,
    final_name: &OsStr,
    options: AtomicPublishOptions,
    writer: impl FnOnce(&mut File) -> io::Result<()>,
    syncs: &Syncs,
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
    stage_validated(dir, tmp_name, options, writer, syncs)?;
    rename_at(dir, tmp_name, dir, final_name)
        .map_err(|e| AtomicPublishError::new(Phase::Rename, e))?;
    syncs
        .directory(dir)
        .map_err(|e| AtomicPublishError::new(Phase::SyncDirectory, e))
}

#[cfg(test)]
mod sync_tests {
    use super::*;
    use std::io::Write;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            loop {
                let path = std::env::temp_dir().join(format!(
                    "khive-sync-{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
                match std::fs::create_dir(&path) {
                    Ok(()) => return Self(path),
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(e) => panic!("create scratch: {e}"),
                }
            }
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn refuse_sync(_: &File) -> io::Result<()> {
        Err(io::Error::from_raw_os_error(libc::EIO))
    }

    #[test]
    fn actual_sync_boundaries_preserve_phase_errno_and_rename_order() {
        for fail_file in [true, false] {
            let scratch = Scratch::new();
            let dir = File::open(&scratch.0).unwrap();
            std::fs::write(scratch.0.join("final"), b"old").unwrap();
            let syncs = if fail_file {
                Syncs {
                    file: Some(refuse_sync),
                    directory: None,
                }
            } else {
                Syncs {
                    file: None,
                    directory: Some(refuse_sync),
                }
            };
            let error = publish_with_syncs(
                &dir,
                OsStr::new("tmp"),
                OsStr::new("final"),
                StaleTmp::RefuseExisting.into(),
                |file| file.write_all(b"new"),
                &syncs,
            )
            .unwrap_err();
            assert_eq!(error.io_error().raw_os_error(), Some(libc::EIO));
            assert_eq!(
                error.phase(),
                if fail_file {
                    AtomicPublishPhase::SyncTmp
                } else {
                    AtomicPublishPhase::SyncDirectory
                }
            );
            assert_eq!(
                std::fs::read(scratch.0.join("final")).unwrap(),
                if fail_file { b"old" } else { b"new" }
            );
            if fail_file {
                assert_eq!(std::fs::read(scratch.0.join("tmp")).unwrap(), b"new");
            } else {
                assert!(!scratch.0.join("tmp").exists());
            }
        }
    }
}
