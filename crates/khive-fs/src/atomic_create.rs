//! Create complete files without replacing a competing destination.
//!
//! Callers own parent-directory policy and serialize the staging namespace. Each
//! attempt owns only its exclusively created sibling; final-name competitors are
//! permitted. A crash can leave a stage, never this attempt's partial final file.

use std::ffi::OsString;
use std::fs::File;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

const MAX_ATTEMPTS: usize = 16;
static NEXT_TEMPORARY: AtomicU64 = AtomicU64::new(0);

/// An original I/O failure and whether this attempt confirmed publication.
#[derive(Debug)]
pub struct CreateOnlyError {
    source: io::Error,
    published: bool,
}

impl CreateOnlyError {
    fn new(source: io::Error, published: bool) -> Self {
        Self { source, published }
    }

    pub fn io_error(&self) -> &io::Error {
        &self.source
    }

    pub fn into_source(self) -> io::Error {
        self.source
    }

    /// True only after the final link succeeded. False is not an absence proof:
    /// a competitor or an unconfirmed filesystem operation may leave an entry.
    pub fn published(&self) -> bool {
        self.published
    }
}

impl std::fmt::Display for CreateOnlyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.published {
            write!(
                f,
                "complete file published before a later failure: {}",
                self.source
            )
        } else {
            write!(f, "create-only publication not confirmed: {}", self.source)
        }
    }
}

impl std::error::Error for CreateOnlyError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

/// Write and file-sync an exclusive sibling, then atomically link a new final name.
///
/// `true` confirms creation; `false` preserves an existing final entry of any type.
/// The writer runs at most once and never for an observed existing destination.
/// No rename/copy fallback is used if the filesystem does not support hard links.
/// Existing parent directories and normal new-file permissions are retained.
///
/// Errors never remove the destination. A post-link cleanup or Unix directory-sync
/// error reports `published() == true`; the complete destination remains. Cleanup
/// after an earlier primary failure is best effort and can leave an owned stage.
/// Unix uses a held directory and verifies retained file identity before link and
/// cleanup. Other platforms require the same trusted staging namespace, retain the
/// exclusive file through sync, and make no portable directory-fsync guarantee.
pub fn create_only(
    path: &Path,
    writer: impl FnOnce(&mut File) -> io::Result<()>,
) -> Result<bool, CreateOnlyError> {
    create_with(path, writer, &Faults::default(), || {
        OsString::from(format!(
            ".create-only-{}-{}.tmp",
            std::process::id(),
            NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed),
        ))
    })
}

fn create_with(
    path: &Path,
    writer: impl FnOnce(&mut File) -> io::Result<()>,
    faults: &Faults,
    names: impl FnMut() -> OsString,
) -> Result<bool, CreateOnlyError> {
    let name = path.file_name().ok_or_else(|| {
        CreateOnlyError::new(
            io::Error::new(io::ErrorKind::InvalidInput, "destination must name a file"),
            false,
        )
    })?;
    let parent = path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    platform::create(parent, name, writer, faults, names)
}

// Private per-call fault seams exist only in unit tests, never as global hooks.
#[derive(Default)]
struct Faults {
    #[cfg(test)]
    before_sync: Option<fn(&File) -> io::Result<()>>,
    #[cfg(test)]
    remove: Option<fn() -> io::Result<()>>,
    #[cfg(all(test, unix))]
    directory: Option<fn(&File) -> io::Result<()>>,
}

impl Faults {
    fn before_sync(&self, _file: &File) -> io::Result<()> {
        #[cfg(test)]
        if let Some(before_sync) = self.before_sync {
            return before_sync(_file);
        }
        Ok(())
    }

    fn before_remove(&self) -> io::Result<()> {
        #[cfg(test)]
        if let Some(remove) = self.remove {
            return remove();
        }
        Ok(())
    }

    #[cfg(unix)]
    fn sync_directory(&self, directory: &File) -> io::Result<()> {
        #[cfg(test)]
        if let Some(sync) = self.directory {
            return sync(directory);
        }
        directory.sync_all()
    }
}

#[cfg(unix)]
mod platform {
    use std::ffi::{OsStr, OsString};
    use std::fs::File;
    use std::io;
    use std::os::fd::AsRawFd;
    use std::path::Path;

    use crate::atomic_publish::{
        stage_atomic_at_detailed, AtomicPublishOptions, AtomicPublishPhase, StaleTmp,
    };
    use crate::fd_relative::{c_name, unlink_at, FileIdentity};

    use super::{CreateOnlyError, Faults, MAX_ATTEMPTS};

    struct OwnedStage {
        file: File,
        identity: FileIdentity,
    }

    impl OwnedStage {
        fn verify(&self, directory: &File, name: &OsStr) -> io::Result<()> {
            let held = FileIdentity::of(&self.file)?;
            let named = FileIdentity::at(directory, name)?;
            if held.dev != self.identity.dev
                || held.ino != self.identity.ino
                || named.dev != self.identity.dev
                || named.ino != self.identity.ino
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "owned staging entry changed",
                ));
            }
            Ok(())
        }

        fn remove(&self, directory: &File, name: &OsStr, faults: &Faults) -> io::Result<()> {
            self.verify(directory, name)?;
            faults.before_remove()?;
            unlink_at(directory, name)
        }
    }

    pub(super) fn create(
        parent: &Path,
        final_name: &OsStr,
        writer: impl FnOnce(&mut File) -> io::Result<()>,
        faults: &Faults,
        mut names: impl FnMut() -> OsString,
    ) -> Result<bool, CreateOnlyError> {
        c_name(final_name).map_err(|error| CreateOnlyError::new(error, false))?;
        match std::fs::symlink_metadata(parent.join(final_name)) {
            Ok(_) => return Ok(false),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(CreateOnlyError::new(error, false)),
        }
        let directory = File::open(parent).map_err(|error| CreateOnlyError::new(error, false))?;
        match FileIdentity::at(&directory, final_name) {
            Ok(_) => return Ok(false),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(CreateOnlyError::new(error, false)),
        }
        let mut writer = Some(writer);
        for _ in 0..MAX_ATTEMPTS {
            let name = names();
            if name.as_os_str() == final_name {
                return Err(CreateOnlyError::new(
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "stage and destination must differ",
                    ),
                    false,
                ));
            }
            let mut entered = false;
            let mut owned = None;
            let staged = stage_atomic_at_detailed(
                &directory,
                &name,
                AtomicPublishOptions {
                    stale: StaleTmp::RefuseExisting,
                    mode: 0o666,
                },
                |file| {
                    entered = true;
                    let held = file.try_clone()?;
                    let identity = FileIdentity::of(&held)?;
                    owned = Some(OwnedStage {
                        file: held,
                        identity,
                    });
                    let write = writer
                        .take()
                        .ok_or_else(|| io::Error::other("writer already used"))?;
                    write(file)?;
                    faults.before_sync(file)
                },
            );
            if let Err(error) = staged {
                if !entered
                    && error.phase() == AtomicPublishPhase::CreateTmp
                    && error.io_error().kind() == io::ErrorKind::AlreadyExists
                {
                    continue;
                }
                if let Some(owned) = owned {
                    let _ = owned.remove(&directory, &name, faults);
                }
                return Err(CreateOnlyError::new(error.into_source(), false));
            }
            let owned = owned.ok_or_else(|| {
                CreateOnlyError::new(
                    io::Error::other("staging ownership was not captured"),
                    false,
                )
            })?;
            owned
                .verify(&directory, &name)
                .map_err(|error| CreateOnlyError::new(error, false))?;
            match link_new(&directory, &name, final_name) {
                Ok(()) => {
                    owned
                        .remove(&directory, &name, faults)
                        .map_err(|error| CreateOnlyError::new(error, true))?;
                    faults
                        .sync_directory(&directory)
                        .map_err(|error| CreateOnlyError::new(error, true))?;
                    return Ok(true);
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    owned
                        .remove(&directory, &name, faults)
                        .map_err(|error| CreateOnlyError::new(error, false))?;
                    return Ok(false);
                }
                Err(error) => {
                    let _ = owned.remove(&directory, &name, faults);
                    return Err(CreateOnlyError::new(error, false));
                }
            }
        }
        Err(CreateOnlyError::new(
            io::Error::new(
                io::ErrorKind::AlreadyExists,
                "exclusive staging retry limit reached",
            ),
            false,
        ))
    }

    fn link_new(directory: &File, from: &OsStr, to: &OsStr) -> io::Result<()> {
        let from = c_name(from)?;
        let to = c_name(to)?;
        // SAFETY: the held directory and validated component strings remain live.
        // Flags zero link the source entry and never replace an existing target.
        let result = unsafe {
            libc::linkat(
                directory.as_raw_fd(),
                from.as_ptr(),
                directory.as_raw_fd(),
                to.as_ptr(),
                0,
            )
        };
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(not(unix))]
mod platform {
    use std::ffi::{OsStr, OsString};
    use std::fs::{self, File, OpenOptions};
    use std::io;
    use std::path::Path;

    use super::{CreateOnlyError, Faults, MAX_ATTEMPTS};

    fn verify_owned(path: &Path, file: &File) -> io::Result<()> {
        // The portable path relies on the documented trusted staging namespace;
        // these regular-file checks do not claim Unix dev/inode equivalence.
        if !fs::symlink_metadata(path)?.is_file() || !file.metadata()?.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "staging entry changed",
            ));
        }
        Ok(())
    }

    fn remove_owned(path: &Path, file: &File, faults: &Faults) -> io::Result<()> {
        verify_owned(path, file)?;
        faults.before_remove()?;
        fs::remove_file(path)
    }

    pub(super) fn create(
        parent: &Path,
        final_name: &OsStr,
        writer: impl FnOnce(&mut File) -> io::Result<()>,
        faults: &Faults,
        mut names: impl FnMut() -> OsString,
    ) -> Result<bool, CreateOnlyError> {
        let destination = parent.join(final_name);
        match fs::symlink_metadata(&destination) {
            Ok(_) => return Ok(false),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(CreateOnlyError::new(error, false)),
        }
        let mut writer = Some(writer);
        for _ in 0..MAX_ATTEMPTS {
            let name = names();
            if name.as_os_str() == final_name {
                return Err(CreateOnlyError::new(
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "stage and destination must differ",
                    ),
                    false,
                ));
            }
            let stage = parent.join(name);
            let mut file = match OpenOptions::new().write(true).create_new(true).open(&stage) {
                Ok(file) => file,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(CreateOnlyError::new(error, false)),
            };
            let written = writer
                .take()
                .ok_or_else(|| io::Error::other("writer already used"))
                .and_then(|write| write(&mut file))
                .and_then(|()| faults.before_sync(&file))
                .and_then(|()| file.sync_all());
            if let Err(error) = written {
                let _ = remove_owned(&stage, &file, faults);
                return Err(CreateOnlyError::new(error, false));
            }
            verify_owned(&stage, &file).map_err(|error| CreateOnlyError::new(error, false))?;
            match fs::hard_link(&stage, &destination) {
                Ok(()) => {
                    remove_owned(&stage, &file, faults)
                        .map_err(|error| CreateOnlyError::new(error, true))?;
                    return Ok(true);
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    remove_owned(&stage, &file, faults)
                        .map_err(|error| CreateOnlyError::new(error, false))?;
                    return Ok(false);
                }
                Err(error) => {
                    let _ = remove_owned(&stage, &file, faults);
                    return Err(CreateOnlyError::new(error, false));
                }
            }
        }
        Err(CreateOnlyError::new(
            io::Error::new(
                io::ErrorKind::AlreadyExists,
                "exclusive staging retry limit reached",
            ),
            false,
        ))
    }
}

#[cfg(test)]
#[path = "atomic_create_tests.rs"]
mod tests;
