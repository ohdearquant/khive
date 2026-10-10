//! Handle-relative checkpoint publication. A writable checkpoint directory is
//! not a trusted source of staging paths: never follow a planted link while
//! opening the lock or a temporary segment.

use std::{fs::File, io, path::Path};

pub(crate) struct CheckpointDirectory {
    #[cfg(any(unix, windows))]
    dir: File,
}

/// Read pack-owned checkpoint sidecars through one pinned directory handle.
/// A caller may reuse this across a committed delta chain without rewalking
/// the segment path for each immutable chunk.
pub struct AuxiliarySidecarReader {
    directory: CheckpointDirectory,
}

/// Keep cleanup on the directory inode opened before HEAD is removed. The
/// checkpoint path can be renamed while cleanup is in progress.
pub struct AuxiliarySidecarCleaner {
    directory: CheckpointDirectory,
}

impl AuxiliarySidecarCleaner {
    pub fn open(path: &Path) -> io::Result<Self> {
        Ok(Self {
            directory: CheckpointDirectory::open_for_cleanup(path)?,
        })
    }

    pub fn remove_and_sync(&self, name: &str) -> io::Result<()> {
        self.directory.remove(name)?;
        self.directory.sync()
    }

    /// Return at most `max_entries` UTF-8 names, counting every non-dot entry
    /// against the scan budget even when its name is not UTF-8.
    pub fn scan_names_bounded(&self, max_entries: usize) -> io::Result<(Vec<String>, bool)> {
        self.directory.scan_names_bounded(max_entries)
    }

    pub fn remove_many_and_sync(&self, names: &[String]) -> io::Result<()> {
        let mut first_error = None;
        for name in names {
            if let Err(error) = self.directory.remove(name) {
                first_error.get_or_insert(error);
            }
        }
        self.directory.sync()?;
        first_error.map_or(Ok(()), Err)
    }
}

impl AuxiliarySidecarReader {
    pub fn open(path: &Path) -> io::Result<Self> {
        Ok(Self {
            directory: CheckpointDirectory::open(path)?,
        })
    }

    /// Missing entries return `None`; links, non-files, and files larger than
    /// the caller's format-derived cap fail closed before allocation.
    pub fn read_bounded(&self, name: &str, max_bytes: usize) -> io::Result<Option<Vec<u8>>> {
        use io::Read as _;

        let Some(file) = self.directory.open_read(name)? else {
            return Ok(None);
        };
        let size = file.metadata()?.len();
        if size > u64::try_from(max_bytes).unwrap_or(u64::MAX) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "checkpoint sidecar exceeds format size bound",
            ));
        }
        let mut bytes = Vec::with_capacity(usize::try_from(size).unwrap_or(max_bytes));
        file.take(
            u64::try_from(max_bytes)
                .unwrap_or(u64::MAX)
                .saturating_add(1),
        )
        .read_to_end(&mut bytes)?;
        if bytes.len() > max_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "checkpoint sidecar grew past format size bound",
            ));
        }
        Ok(Some(bytes))
    }

    /// Read at most the first `max_bytes` of a sidecar, whatever its size.
    /// Missing entries return `None`; links and non-files fail closed.
    pub fn read_prefix(&self, name: &str, max_bytes: usize) -> io::Result<Option<Vec<u8>>> {
        use io::Read as _;

        let Some(file) = self.directory.open_read(name)? else {
            return Ok(None);
        };
        let mut bytes = Vec::new();
        file.take(u64::try_from(max_bytes).unwrap_or(u64::MAX))
            .read_to_end(&mut bytes)?;
        Ok(Some(bytes))
    }
}

impl CheckpointDirectory {
    /// Pin the non-symlink/non-reparse directory resolved from the configured
    /// path at its first final-component open. The parent chain is trusted on
    /// both platforms. Unix's descriptor walk follows only ownership/mode-
    /// qualified ancestor symlinks; Windows rejects ancestor links by path
    /// before its canonical component walk and rejects reparse-point handles.
    /// An ordinary directory replacement before the first open is out of
    /// scope because a path input supplies no earlier identity to authenticate.
    /// Unix binds the walked handle to an independent `O_NOFOLLOW` reopen by
    /// device/inode; Windows binds the walked and retained handles by volume/
    /// file ID and final path. A final symlink or reparse point is refused, and
    /// subsequent checkpoint operations use the retained directory handle.
    pub(crate) fn open(path: &Path) -> io::Result<Self> {
        #[cfg(unix)]
        {
            let dir = crate::external_ids::open_dir_with_trusted_symlinks(path)
                .map_err(io::Error::other)?;
            crate::external_ids::verify_original_dir_identity(path, &dir)
                .map_err(io::Error::other)?;
            Ok(Self { dir })
        }
        #[cfg(windows)]
        {
            let dir = crate::external_ids::windows::open_checkpoint_directory(path)?;
            Ok(Self { dir })
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = path;
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "secure checkpoint publication requires handle-relative filesystem operations",
            ))
        }
    }

    fn open_for_cleanup(path: &Path) -> io::Result<Self> {
        #[cfg(windows)]
        {
            Ok(Self {
                dir: crate::external_ids::windows::open_checkpoint_directory_for_listing(path)?,
            })
        }
        #[cfg(not(windows))]
        {
            Self::open(path)
        }
    }

    fn scan_names_bounded(&self, max_entries: usize) -> io::Result<(Vec<String>, bool)> {
        #[cfg(unix)]
        {
            use nix::dir::Dir;

            let mut directory = Dir::from_fd(self.dir.try_clone()?.into())
                .map_err(|error| io::Error::from_raw_os_error(error as i32))?;
            let mut names = Vec::new();
            let mut visited = 0;
            for entry in directory.iter() {
                let entry = entry.map_err(|error| io::Error::from_raw_os_error(error as i32))?;
                let name = entry.file_name().to_bytes();
                if name == b"." || name == b".." {
                    continue;
                }
                if visited == max_entries {
                    return Ok((names, true));
                }
                visited += 1;
                if let Ok(name) = std::str::from_utf8(name) {
                    names.push(name.to_owned());
                }
            }
            Ok((names, false))
        }
        #[cfg(windows)]
        {
            crate::external_ids::windows::list_checkpoint_names_bounded(&self.dir, max_entries)
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = max_entries;
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "secure checkpoint directory scan unsupported",
            ))
        }
    }

    pub(crate) fn open_lock(&self) -> io::Result<File> {
        #[cfg(unix)]
        {
            use std::os::fd::{AsRawFd as _, FromRawFd as _};
            // SAFETY: the directory descriptor and static NUL-terminated name
            // live through this call; a successful descriptor is uniquely owned.
            let fd = unsafe {
                libc::openat(
                    self.dir.as_raw_fd(),
                    c".checkpoint.lock".as_ptr(),
                    libc::O_RDWR
                        | libc::O_CREAT
                        | libc::O_NOFOLLOW
                        | libc::O_NONBLOCK
                        | libc::O_CLOEXEC,
                    0o644 as libc::c_uint,
                )
            };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: `fd` was freshly returned by openat and has one owner.
            let file = unsafe { File::from_raw_fd(fd) };
            if !file.metadata()?.is_file() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "checkpoint lock is not a regular file",
                ));
            }
            Ok(file)
        }
        #[cfg(windows)]
        {
            crate::external_ids::windows::open_checkpoint_lock(&self.dir)
        }
        #[cfg(not(any(unix, windows)))]
        {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "secure checkpoint lock unsupported",
            ))
        }
    }

    fn open_read(&self, name: &str) -> io::Result<Option<File>> {
        component_name(name)?;
        #[cfg(unix)]
        {
            use std::{
                ffi::CString,
                os::fd::{AsRawFd as _, FromRawFd as _},
            };
            let name =
                CString::new(name).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
            // SAFETY: the pinned directory descriptor and validated name live
            // through this call. O_NOFOLLOW rejects a planted final symlink.
            let fd = unsafe {
                libc::openat(
                    self.dir.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                let error = io::Error::last_os_error();
                return if error.kind() == io::ErrorKind::NotFound {
                    Ok(None)
                } else {
                    Err(error)
                };
            }
            // SAFETY: openat returned a new descriptor with one owner.
            let file = unsafe { File::from_raw_fd(fd) };
            if !file.metadata()?.is_file() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "checkpoint sidecar is not a regular file",
                ));
            }
            Ok(Some(file))
        }
        #[cfg(windows)]
        {
            crate::external_ids::windows::open_checkpoint_read_file(&self.dir, name)
        }
        #[cfg(not(any(unix, windows)))]
        {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "secure checkpoint sidecar reads unsupported",
            ))
        }
    }

    pub(crate) fn stage(&self, name: &str, bytes: &[u8]) -> io::Result<()> {
        #[cfg(unix)]
        {
            use khive_fs::atomic_publish::{stage_atomic_at_detailed, StaleTmp};
            use std::io::Write as _;

            component_name(name)?;
            stage_atomic_at_detailed(&self.dir, name, StaleTmp::Refuse, |file| {
                file.write_all(bytes)
            })
            .map_err(checkpoint_publication_error)
        }
        #[cfg(windows)]
        {
            component_name(name)?;
            crate::external_ids::windows::stage_checkpoint_file(&self.dir, name, bytes)
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = (name, bytes);
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "secure checkpoint staging unsupported",
            ))
        }
    }

    pub(crate) fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        component_name(from)?;
        component_name(to)?;
        #[cfg(unix)]
        {
            khive_fs::fd_relative::rename_at(
                &self.dir,
                std::ffi::OsStr::new(from),
                &self.dir,
                std::ffi::OsStr::new(to),
            )
        }
        #[cfg(windows)]
        {
            crate::external_ids::windows::rename_checkpoint_file(&self.dir, from, to)
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = (from, to);
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "secure checkpoint rename unsupported",
            ))
        }
    }

    pub(crate) fn sync(&self) -> io::Result<()> {
        #[cfg(any(unix, windows))]
        {
            self.dir.sync_all()
        }
        #[cfg(not(any(unix, windows)))]
        {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "secure checkpoint sync unsupported",
            ))
        }
    }

    pub(crate) fn remove(&self, name: &str) -> io::Result<()> {
        component_name(name)?;
        #[cfg(unix)]
        {
            use std::{ffi::CString, os::fd::AsRawFd as _};
            let name =
                CString::new(name).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
            // SAFETY: the name and pinned directory descriptor are live. Unlinking
            // a planted link removes only that directory entry.
            if unsafe { libc::unlinkat(self.dir.as_raw_fd(), name.as_ptr(), 0) } != 0 {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::NotFound {
                    return Err(error);
                }
            }
            Ok(())
        }
        #[cfg(windows)]
        {
            crate::external_ids::windows::remove_checkpoint_file(&self.dir, name)
        }
        #[cfg(not(any(unix, windows)))]
        {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "secure checkpoint removal unsupported",
            ))
        }
    }
}

/// Publish a pack-owned sidecar through the same pinned-directory staging
/// boundary as the v2 segments. Callers serialize this with `.checkpoint.lock`.
pub fn write_auxiliary_sidecar_atomic(path: &Path, name: &str, bytes: &[u8]) -> io::Result<()> {
    component_name(name)?;
    let checkpoint = CheckpointDirectory::open(path)?;
    let staged = format!("{name}.tmp");
    #[cfg(unix)]
    {
        use khive_fs::atomic_publish::{publish_atomic_at_detailed, StaleTmp};
        use std::io::Write as _;

        publish_atomic_at_detailed(&checkpoint.dir, &staged, name, StaleTmp::Refuse, |file| {
            file.write_all(bytes)
        })
        .map_err(checkpoint_publication_error)
    }
    #[cfg(not(unix))]
    {
        checkpoint.stage(&staged, bytes)?;
        checkpoint.rename(&staged, name)?;
        checkpoint.sync()
    }
}

/// Remove an obsolete pack-owned sidecar after a full segment publication.
/// Callers serialize this with `.checkpoint.lock`.
pub fn remove_auxiliary_sidecar(path: &Path, name: &str) -> io::Result<()> {
    let checkpoint = CheckpointDirectory::open(path)?;
    checkpoint.remove(name)?;
    checkpoint.sync()
}

/// Reclaim orphan sidecars with one pinned directory and one directory sync.
/// Callers first remove their publication HEAD so interrupted cleanup cannot
/// leave a committed record pointing at a deleted chunk.
pub fn remove_auxiliary_sidecars(path: &Path, names: &[String]) -> io::Result<()> {
    let checkpoint = CheckpointDirectory::open(path)?;
    let mut first_error = None;
    for name in names {
        if let Err(error) = checkpoint.remove(name) {
            first_error.get_or_insert(error);
        }
    }
    checkpoint.sync()?;
    first_error.map_or(Ok(()), Err)
}

#[cfg(unix)]
fn checkpoint_publication_error(error: khive_fs::atomic_publish::AtomicPublishError) -> io::Error {
    if error.phase() == khive_fs::atomic_publish::AtomicPublishPhase::RefuseTmp {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "checkpoint staging entry is not a regular file",
        )
    } else {
        error.into_source()
    }
}

fn component_name(name: &str) -> io::Result<&str> {
    if matches!(name, "." | "..") || name.contains('\\') {
        return Err(io::Error::from(io::ErrorKind::InvalidInput));
    }
    #[cfg(unix)]
    khive_fs::fd_relative::c_name(std::ffi::OsStr::new(name))
        .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    #[cfg(not(unix))]
    if name.is_empty() || name.contains('/') || name.contains('\0') {
        return Err(io::Error::from(io::ErrorKind::InvalidInput));
    }
    Ok(name)
}
